//! Local rack operations.
//!
//! A server that supports it answers a rack's `link up` with a **profile**
//! (`rack/<id>/profile`) instead of driving the rack exchange by exchange. The
//! profile is all this side ever knows about the device: frame templates with
//! two placeholders (`{a}` — the slot address byte, `{c}` — the checksum byte)
//! and byte rules that are applied blindly. It is kept in memory only — never
//! written to the config or the log — and nothing in this file names a device
//! or carries a byte of its protocol.
//!
//! With a profile the routine work of the rack runs here, without a server
//! round trip per step: the status poll, the slot table of the UI, card
//! identification, the removal check and the slot indicators. Card sessions
//! are opened and closed through the same reconcile a server card set uses
//! (`cards::handle_cards_set`), so everything downstream is unchanged. The
//! server keeps what must stay remote: the exchanges of the card sessions.
//!
//! The engine of a rack lives as long as its port and does not depend on the
//! application connection once it has the profile: a reconnect re-delivers the
//! same profile and the running engine is left alone. A server that drives the
//! rack itself (an older one, or local mode switched off) sends its own rack
//! commands instead, which stops the engine (see `rack::handle_app_publish`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::async_runtime::{self, JoinHandle};

use crate::global_app_handle::{rack_update_slots, RackSlotError, RackSlotStates, RackSlots};

use super::cards::{handle_cards_set, inventory, RACK_CARDS_UI};
use super::transport::{
    execute_envelope, normalize_hex, PollSpec, SerialEnvelope, SharedPort, SERIAL_MS_MAX,
    SERIAL_MS_MIN,
};
use super::{lock, rack_port_is_live};

/// Highest slot address a rack can have.
const SLOT_MAX: usize = 240;

/// Cards identified per status poll before the card set is applied and the
/// status is read again: the first walk of a full rack then serves its cards
/// as they are identified instead of after the last one.
const IDENTIFY_BATCH: usize = 8;

/// Pause before a failed identification of a slot is tried again, doubled per
/// failure up to the ceiling.
const IDENTIFY_RETRY_INITIAL: Duration = Duration::from_secs(10);
const IDENTIFY_RETRY_MAX: Duration = Duration::from_secs(600);

/// Ceiling of the status poll interval while its exchanges keep failing.
const POLL_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// A slot blink the user asked for to find a slot on the rack: the indicator
/// alternates every `BLINK_HALF` (2 Hz), `BLINK_HALVES` times (3 seconds).
/// One indicator write is a one-shot exchange of a few tens of milliseconds,
/// so the rhythm keeps steady even while other slots authenticate.
const BLINK_HALF: Duration = Duration::from_millis(250);
const BLINK_HALVES: u32 = 12;

/// Integrity rule every reply of the device satisfies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Check {
    None,
    /// All bytes of a frame sum to zero modulo 256.
    Sum8,
}

impl Check {
    fn passes(self, frame: &[u8]) -> bool {
        match self {
            Check::None => true,
            Check::Sum8 => !frame.is_empty() && sum8(frame) == 0,
        }
    }
}

fn sum8(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |acc, b| acc.wrapping_add(*b))
}

/// Fills a frame template for one slot: `{a}` becomes the slot address byte,
/// a trailing `{c}` the checksum byte. `None` for a template that is not hex
/// around its placeholders, or asks for a checksum the profile does not define.
fn fill(template: &str, slot: u16, check: Check) -> Option<Vec<u8>> {
    let addressed = template.replace("{a}", &format!("{:02X}", slot & 0xFF));
    match addressed.strip_suffix("{c}") {
        Some(body) => {
            let mut bytes = hex::decode(body).ok()?;
            match check {
                Check::Sum8 => bytes.push(0u8.wrapping_sub(sum8(&bytes))),
                Check::None => return None,
            }
            Some(bytes)
        }
        None => hex::decode(&addressed).ok(),
    }
}

/// A byte rule a reply must satisfy to count as the expected result.
#[derive(Debug, Default)]
struct Rule {
    /// The reply is judged on its first `len` bytes (whatever follows them is
    /// ignored); the whole reply without it.
    len: Option<usize>,
    /// Template the judged bytes start with.
    head: String,
    /// `(offset, mask, expected)`: the bytes at `offset`, each masked, equal `expected`.
    matches: Vec<(usize, Option<Vec<u8>>, Vec<u8>)>,
    /// `(offset, length)` of the bytes the rule yields.
    take: Option<(usize, usize)>,
}

/// One two-phase command of the identification sequence.
#[derive(Debug)]
struct Step {
    cmd: String,
    expect: Option<String>,
    deadline: Duration,
    ok: Rule,
}

/// The poll that collects the result of a two-phase command.
#[derive(Debug)]
struct PollTemplates {
    cmd: String,
    while_reply: String,
    interval: Duration,
    deadline: Duration,
}

/// What a status byte says about its slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    /// No reader answers at this address.
    None,
    /// A working reader without a card.
    Free,
    /// A card that can be worked with.
    Idle,
    /// A card in error: worth one more identification attempt.
    Error,
    /// A card that is busy or in a state this side must leave alone.
    Busy,
}

impl Class {
    fn has_card(self) -> bool {
        matches!(self, Class::Idle | Class::Error | Class::Busy)
    }
}

/// The parsed profile of a rack (see the module docs).
#[derive(Debug)]
struct Profile {
    check: Check,
    status_cmd: String,
    status_interval: Duration,
    status_idle: Duration,
    status_deadline: Duration,
    status_head: Vec<u8>,
    status_len: usize,
    status_offset: usize,
    status_length: usize,
    mask: u8,
    stable: u32,
    gap: usize,
    free: Vec<u8>,
    none: Vec<u8>,
    idle: Vec<u8>,
    error: Vec<u8>,
    slot_idle: Duration,
    slot_deadline: Duration,
    probe_cmd: Option<String>,
    probe_empty: Vec<String>,
    led_served: Option<String>,
    led_unserved: Option<String>,
    led_off: Option<String>,
    poll: Option<PollTemplates>,
    identify: Vec<Step>,
}

fn text<'a>(json: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    json.get(key).and_then(|v| v.as_str())
}

fn number(json: &serde_json::Value, key: &str) -> Option<u64> {
    json.get(key).and_then(|v| v.as_u64())
}

fn millis(json: &serde_json::Value, key: &str, default: u64) -> Duration {
    Duration::from_millis(number(json, key).unwrap_or(default).min(SERIAL_MS_MAX))
}

fn byte_set(json: &serde_json::Value, key: &str) -> Result<Vec<u8>, String> {
    match text(json, key) {
        Some(hex_text) => hex::decode(hex_text).map_err(|_| format!("bad_class {key}")),
        None => Ok(Vec::new()),
    }
}

fn parse_rule(json: &serde_json::Value) -> Result<Rule, String> {
    let mut rule = Rule {
        len: number(json, "len").map(|n| n as usize),
        head: text(json, "head").unwrap_or("").to_string(),
        ..Rule::default()
    };
    if let Some(items) = json.get("match").and_then(|v| v.as_array()) {
        for item in items {
            let at = number(item, "at").ok_or("bad_rule_offset")? as usize;
            let expected = text(item, "eq")
                .and_then(|h| hex::decode(h).ok())
                .filter(|bytes| !bytes.is_empty())
                .ok_or("bad_rule_value")?;
            let mask = match text(item, "mask") {
                Some(h) => Some(hex::decode(h).map_err(|_| "bad_rule_mask")?),
                None => None,
            };
            if mask.as_ref().is_some_and(|m| m.len() != 1 && m.len() != expected.len()) {
                return Err("bad_rule_mask".to_string());
            }
            rule.matches.push((at, mask, expected));
        }
    }
    if let (Some(at), Some(len)) = (number(json, "take_at"), number(json, "take_len")) {
        rule.take = Some((at as usize, len as usize));
    }
    Ok(rule)
}

impl Profile {
    /// Parses and validates a profile: every template must fill for a slot,
    /// so a malformed profile is rejected as a whole instead of failing on
    /// the wire later.
    fn parse(payload: &[u8]) -> Result<Profile, String> {
        let json: serde_json::Value =
            serde_json::from_slice(payload).map_err(|e| format!("bad_json: {e}"))?;
        if number(&json, "version") != Some(1) {
            return Err("unsupported_version".to_string());
        }
        let check = match text(&json, "check") {
            None => Check::None,
            Some("sum8") => Check::Sum8,
            Some(_) => return Err("unknown_check".to_string()),
        };
        let status = json.get("status").ok_or("no_status")?;
        let classes = status.get("classes").ok_or("no_classes")?;
        let slot = json.get("slot").ok_or("no_slot")?;
        let template = |section: &serde_json::Value, key: &str| -> Result<Option<String>, String> {
            match text(section, key) {
                None => Ok(None),
                Some(t) if fill(t, 1, check).is_some() => Ok(Some(t.to_string())),
                Some(_) => Err(format!("bad_template {key}")),
            }
        };
        let status_cmd = text(status, "cmd")
            .and_then(|cmd| normalize_hex(cmd).ok())
            .filter(|cmd| !cmd.is_empty())
            .ok_or("bad_status_cmd")?;
        let status_offset = number(status, "offset").ok_or("no_status_offset")? as usize;
        let status_length = number(status, "length").ok_or("no_status_length")? as usize;
        let status_len = number(status, "len").ok_or("no_status_len")? as usize;
        if status_length == 0
            || status_length > SLOT_MAX
            || status_offset + status_length > status_len
            || status_len > 4096
        {
            return Err("bad_status_region".to_string());
        }
        let mask = match text(status, "mask") {
            None => 0xFF,
            Some(h) => match hex::decode(h) {
                Ok(m) if m.len() == 1 => m[0],
                _ => return Err("bad_status_mask".to_string()),
            },
        };

        let probe = slot.get("probe");
        let mut probe_empty = Vec::new();
        if let Some(items) = probe.and_then(|p| p.get("empty")).and_then(|v| v.as_array()) {
            for item in items {
                let t = item.as_str().ok_or("bad_template empty")?;
                fill(t, 1, check).ok_or("bad_template empty")?;
                probe_empty.push(t.to_string());
            }
        }
        let led = slot.get("led");
        let led_template = |key: &str| match led {
            Some(led) => template(led, key),
            None => Ok(None),
        };
        let poll = match slot.get("poll") {
            None => None,
            Some(p) => Some(PollTemplates {
                cmd: template(p, "cmd")?.ok_or("no_poll_cmd")?,
                while_reply: template(p, "while")?.ok_or("no_poll_while")?,
                interval: millis(p, "interval_ms", 20).max(Duration::from_millis(SERIAL_MS_MIN)),
                deadline: millis(p, "deadline_ms", 5000),
            }),
        };
        let slot_deadline = millis(slot, "deadline_ms", 500);
        let mut identify = Vec::new();
        if let Some(items) = slot.get("identify").and_then(|v| v.as_array()) {
            for item in items {
                identify.push(Step {
                    cmd: template(item, "cmd")?.ok_or("no_step_cmd")?,
                    expect: template(item, "expect")?,
                    deadline: number(item, "deadline_ms")
                        .map(|ms| Duration::from_millis(ms.min(SERIAL_MS_MAX)))
                        .unwrap_or(slot_deadline),
                    ok: parse_rule(item.get("ok").ok_or("no_step_rule")?)?,
                });
            }
        }
        if identify.last().is_some_and(|step| step.ok.take.is_none()) {
            return Err("identify_yields_nothing".to_string());
        }

        Ok(Profile {
            check,
            status_cmd,
            status_interval: millis(status, "interval_ms", 2000)
                .max(Duration::from_millis(SERIAL_MS_MIN)),
            status_idle: millis(status, "idle_ms", 300),
            status_deadline: millis(status, "deadline_ms", 3000),
            status_head: text(status, "head")
                .map(|h| hex::decode(h).map_err(|_| "bad_status_head"))
                .transpose()?
                .unwrap_or_default(),
            status_len,
            status_offset,
            status_length,
            mask,
            stable: number(status, "stable").unwrap_or(2).clamp(1, 10) as u32,
            gap: number(status, "gap").unwrap_or(0).min(SLOT_MAX as u64) as usize,
            free: byte_set(classes, "free")?,
            none: byte_set(classes, "none")?,
            idle: byte_set(classes, "idle")?,
            error: byte_set(classes, "error")?,
            slot_idle: millis(slot, "idle_ms", 50),
            slot_deadline,
            probe_cmd: match probe {
                Some(p) => template(p, "cmd")?,
                None => None,
            },
            probe_empty,
            led_served: led_template("served")?,
            led_unserved: led_template("unserved")?,
            led_off: led_template("off")?,
            poll,
            identify,
        })
    }

    fn classify(&self, raw: u8) -> Class {
        let value = raw & self.mask;
        if self.none.contains(&value) {
            Class::None
        } else if self.free.contains(&value) {
            Class::Free
        } else if self.idle.contains(&value) {
            Class::Idle
        } else if self.error.contains(&value) {
            Class::Error
        } else {
            Class::Busy
        }
    }

    /// The per-slot status bytes of a status reply, `None` for a reply that
    /// is cut, damaged or not the status at all.
    fn status_region<'a>(&self, reply: &'a [u8]) -> Option<&'a [u8]> {
        let frame = reply.get(..self.status_len)?;
        if !self.check.passes(frame) || !frame.starts_with(&self.status_head) {
            return None;
        }
        frame.get(self.status_offset..self.status_offset + self.status_length)
    }

    /// Judges a reply by a rule. `None` — not the expected result;
    /// `Some(bytes)` — it is, with the bytes the rule yields (if any).
    fn apply(&self, rule: &Rule, slot: u16, reply: &[u8]) -> Option<Option<Vec<u8>>> {
        let frame = match rule.len {
            Some(len) => reply.get(..len)?,
            None => reply,
        };
        if !self.check.passes(frame) {
            return None;
        }
        let head = fill(&rule.head, slot, Check::None)?;
        if !frame.starts_with(&head) {
            return None;
        }
        for (at, mask, expected) in &rule.matches {
            let part = frame.get(*at..*at + expected.len())?;
            let equal = part.iter().zip(expected).enumerate().all(|(i, (got, want))| {
                let m = match mask {
                    Some(m) if m.len() == 1 => m[0],
                    Some(m) => m[i],
                    None => 0xFF,
                };
                got & m == want & m
            });
            if !equal {
                return None;
            }
        }
        match rule.take {
            Some((at, len)) => Some(Some(frame.get(at..at + len)?.to_vec())),
            None => Some(None),
        }
    }

    /// A one-shot exchange with one slot.
    /// The part of a step reply its rule judges. The device may deliver the
    /// frames of one operation in a single read: the "accepted" frame of the
    /// step, or a "still working" frame of its poll, ahead of the outcome.
    /// Those are known byte for byte from the profile and are skipped here.
    fn outcome_part<'a>(&self, step: &Step, slot: u16, reply: &'a [u8]) -> &'a [u8] {
        let known: Vec<Vec<u8>> = [
            step.expect.as_deref(),
            self.poll.as_ref().map(|poll| poll.while_reply.as_str()),
        ]
        .into_iter()
        .flatten()
        .filter_map(|template| fill(template, slot, self.check))
        .filter(|frame| !frame.is_empty())
        .collect();
        let mut rest = reply;
        while let Some(frame) = known.iter().find(|frame| rest.len() > frame.len() && rest.starts_with(frame)) {
            rest = &rest[frame.len()..];
        }
        rest
    }

    fn slot_envelope(&self, template: &str, slot: u16) -> Option<SerialEnvelope> {
        Some(SerialEnvelope {
            cmd_hex: hex::encode_upper(fill(template, slot, self.check)?),
            expect_hex: None,
            idle: self.slot_idle,
            deadline: self.slot_deadline,
            poll: None,
            // local signalling, never part of an authentication session
            finish: None,
        })
    }

    /// A two-phase command of the identification sequence.
    fn step_envelope(&self, step: &Step, slot: u16) -> Option<SerialEnvelope> {
        let mut envelope = self.slot_envelope(&step.cmd, slot)?;
        envelope.deadline = step.deadline;
        if let (Some(expect), Some(poll)) = (&step.expect, &self.poll) {
            envelope.expect_hex = Some(hex::encode_upper(fill(expect, slot, self.check)?));
            envelope.poll = Some(PollSpec {
                cmd_hex: hex::encode_upper(fill(&poll.cmd, slot, self.check)?),
                while_hex: hex::encode_upper(fill(&poll.while_reply, slot, self.check)?),
                interval: poll.interval,
                deadline: poll.deadline,
            });
        }
        Some(envelope)
    }
}

/// Indicator state of a slot, as last written to the rack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Led {
    Served,
    Unserved,
}

/// What the engine knows about one slot.
#[derive(Clone, Debug)]
struct SlotState {
    /// Class the slot held for `stable` polls in a row.
    class: Class,
    candidate: Class,
    seen: u32,
    /// Id of the identified card of the slot.
    card: Option<String>,
    /// The last identification of the card in the slot failed.
    failed: bool,
    /// Why it failed, for the slot table of the UI.
    fail_reason: Option<&'static str>,
    failures: u32,
    retry_at: Option<Instant>,
    led: Option<Led>,
    /// An authentication ran on the slot since its indicator was written.
    was_active: bool,
}

impl Default for SlotState {
    fn default() -> Self {
        SlotState {
            class: Class::None,
            candidate: Class::None,
            seen: 0,
            card: None,
            failed: false,
            fail_reason: None,
            failures: 0,
            retry_at: None,
            led: None,
            was_active: false,
        }
    }
}

/// Feeds one status reading into the slots: a class becomes the slot's own
/// once it was read `stable` polls in a row, so a one-poll flicker of a
/// served slot is never acted upon.
fn observe(slots: &mut [SlotState], profile: &Profile, region: &[u8]) {
    for (state, raw) in slots.iter_mut().zip(region) {
        let class = profile.classify(*raw);
        if class == state.candidate {
            state.seen = state.seen.saturating_add(1);
        } else {
            state.candidate = class;
            state.seen = 1;
        }
        if state.seen >= profile.stable {
            state.class = state.candidate;
        }
    }
}

/// The slot table of the UI. The end of the reader chain is the highest
/// address that holds anything; a run of `gap` silent addresses aligned to
/// `gap` below it is a hole in the addressing, not a row of faulty readers.
fn slot_table(slots: &[SlotState], gap: usize) -> RackSlots {
    let hi = slots
        .iter()
        .rposition(|s| s.class != Class::None || s.card.is_some())
        .map(|index| index + 1)
        .unwrap_or(0);
    let mut states = RackSlotStates::default();
    let mut errors = Vec::new();
    for (index, state) in slots.iter().enumerate().take(hi) {
        let slot = (index + 1) as u16;
        let in_gap = gap > 0 && {
            let start = index / gap * gap;
            slots
                .get(start..start + gap)
                .is_some_and(|run| run.iter().all(|s| s.class == Class::None && s.card.is_none()))
        };
        if in_gap {
            states.absent.push(slot);
        } else if state.class == Class::Error || (state.failed && state.class.has_card()) {
            states.error.push(slot);
            // a failed identification says more than the status byte behind it
            let reason = match (state.failed, state.fail_reason) {
                (true, Some(reason)) => reason,
                (true, None) => "identify_failed",
                (false, _) => "rack_error",
            };
            errors.push(RackSlotError { slot, reason: reason.to_string() });
        } else if state.card.is_some() {
            states.discovered.push(slot);
        } else if state.class.has_card() {
            states.card.push(slot);
        } else if state.class == Class::Free {
            states.free.push(slot);
        } else {
            states.faulty.push(slot);
        }
    }
    let occupied = states.card.len() + states.discovered.len() + states.error.len();
    let total = occupied + states.free.len() + states.faulty.len();
    RackSlots {
        hi: hi as u16,
        total: total as u16,
        occupied: occupied as u16,
        free: (total - occupied) as u16,
        states,
        errors,
    }
}

/// Releases the identification lease of a slot when dropped, also when the
/// engine is aborted in the middle of an identification.
struct IdentifyLease<'a>(&'a SharedPort, u16, u64);
impl Drop for IdentifyLease<'_> {
    fn drop(&mut self) {
        super::access::release(self.0, self.1, true, Some(self.2));
    }
}

/// Owner tokens of the identification leases of the engines.
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1 << 40);

/// Runs the identification sequence on one slot and returns the card id.
async fn identify(
    profile: &Profile,
    port: &SharedPort,
    slot: u16,
    log_header: &str,
) -> Result<String, &'static str> {
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    let mut lease = None;
    let mut id = None;
    for (step_index, step) in profile.identify.iter().enumerate() {
        // Taken anew before every step: an authentication that owns the slot
        // refuses it, and a lease dropped in between (the application
        // connection changed) is restored before the card is touched again.
        if !super::access::acquire(port, slot, true, token).await {
            return Err("card_busy");
        }
        lease.get_or_insert_with(|| IdentifyLease(port, slot, token));
        let envelope = profile.step_envelope(step, slot).ok_or("bad_template")?;
        let (exchange, _) = execute_envelope(port, envelope, log_header, false).await;
        if !exchange.is_ok() {
            return Err(exchange.err);
        }
        let reply = hex::decode(&exchange.resp_hex).map_err(|_| "bad_reply")?;
        match profile.apply(&step.ok, slot, profile.outcome_part(step, slot, &reply)) {
            Some(Some(bytes)) => id = Some(hex::encode_upper(bytes)),
            Some(None) => {}
            None => {
                // The reply is what tells a damaged or foreign card from a
                // profile that does not fit the reader, so it goes to the log.
                log::warn!(
                    "{} [LOCAL] status=identify_reply_rejected slot={} step={} rx_bytes={} reply={}",
                    log_header,
                    slot,
                    step_index + 1,
                    reply.len(),
                    exchange.resp_hex.get(..128).unwrap_or(&exchange.resp_hex)
                );
                return Err("unexpected_reply");
            }
        }
    }
    id.ok_or("no_card_id")
}

/// Asks the reader of one slot whether it is empty. Only its own answer
/// counts: a reply that does not start with one of the "empty" frames of this
/// very slot leaves the card where it is.
async fn confirmed_empty(profile: &Profile, port: &SharedPort, slot: u16, log_header: &str) -> bool {
    let Some(envelope) = profile
        .probe_cmd
        .as_deref()
        .and_then(|cmd| profile.slot_envelope(cmd, slot))
    else {
        return false;
    };
    let (exchange, _) = execute_envelope(port, envelope, log_header, false).await;
    if !exchange.is_ok() {
        return false;
    }
    let Ok(reply) = hex::decode(&exchange.resp_hex) else {
        return false;
    };
    profile
        .probe_empty
        .iter()
        .filter_map(|template| fill(template, slot, profile.check))
        .any(|empty| reply.starts_with(&empty))
}

/// Writes the indicator of one slot; `true` when the rack took the command.
async fn write_led(
    profile: &Profile,
    port: &SharedPort,
    template: Option<&str>,
    slot: u16,
    log_header: &str,
) -> bool {
    let Some(envelope) = template.and_then(|t| profile.slot_envelope(t, slot)) else {
        return false;
    };
    execute_envelope(port, envelope, log_header, false)
        .await
        .0
        .is_ok()
}

/// Hands the identified cards of the rack to the session reconcile, exactly
/// as a server card set would.
async fn apply_cards(slots: &[SlotState], rack_id: &str, port: &SharedPort, log_header: &str) {
    let cards: Vec<serde_json::Value> = slots
        .iter()
        .enumerate()
        .filter_map(|(index, state)| {
            state
                .card
                .as_ref()
                .map(|id| serde_json::json!({"slot": index + 1, "iccid": id}))
        })
        .collect();
    handle_cards_set(
        serde_json::Value::Array(cards).to_string().into_bytes(),
        rack_id.to_string(),
        port.clone(),
        log_header.to_string(),
    )
    .await;
}

/// Slots of the rack whose card session is up, by the UI rows of the rack.
fn served_slots(rack_id: &str) -> Vec<(u16, String)> {
    lock(&RACK_CARDS_UI)
        .get(rack_id)
        .map(|rows| {
            rows.iter()
                .filter(|row| row.online == Some(true))
                .filter_map(|row| row.iccid.clone().map(|id| (row.slot, id)))
                .collect()
        })
        .unwrap_or_default()
}

/// The pause before the next status poll: the profile interval, doubled per
/// consecutive failure up to `POLL_BACKOFF_MAX` (never below the interval).
fn poll_delay(interval: Duration, failures: u32) -> Duration {
    interval
        .saturating_mul(1u32 << failures.min(10))
        .min(POLL_BACKOFF_MAX.max(interval))
}

fn identify_retry_delay(failures: u32) -> Duration {
    IDENTIFY_RETRY_INITIAL
        .saturating_mul(1u32 << failures.min(10))
        .min(IDENTIFY_RETRY_MAX)
}

/// The engine of one rack: polls the status and acts on what it shows until
/// the port of the rack goes away.
// index loops: every pass awaits serial exchanges and writes the slot back
#[allow(clippy::needless_range_loop)]
async fn run(profile: Arc<Profile>, rack_id: String, port: SharedPort) {
    let log_header = format!("RACK {} |", rack_id);
    let mut slots = vec![SlotState::default(); profile.status_length];
    // Sessions that outlived the previous engine (or a server-driven period)
    // keep their cards: nothing is identified twice, no session is closed.
    if let Some(cards) = inventory(&rack_id).as_array() {
        for card in cards {
            let slot = card.get("slot").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if let (Some(state), Some(id)) = (
                slot.checked_sub(1).and_then(|index| slots.get_mut(index)),
                card.get("iccid").and_then(|v| v.as_str()),
            ) {
                state.card = Some(id.to_string());
            }
        }
    }
    let mut failures: u32 = 0;
    let mut first_pass = true;
    loop {
        tokio::time::sleep(poll_delay(profile.status_interval, failures)).await;
        if !rack_port_is_live(&rack_id, &port) {
            log::info!("{} [LOCAL] status=ended reason=link_down", log_header);
            break;
        }
        let envelope = SerialEnvelope {
            cmd_hex: profile.status_cmd.clone(),
            expect_hex: None,
            idle: profile.status_idle,
            deadline: profile.status_deadline,
            poll: None,
            finish: None,
        };
        let (exchange, end) = execute_envelope(&port, envelope, &log_header, false).await;
        if !exchange.is_ok() {
            failures = failures.saturating_add(1);
            continue;
        }
        failures = 0;
        let reply = hex::decode(&exchange.resp_hex).unwrap_or_default();
        let Some(region) = profile.status_region(&reply) else {
            // a cut or damaged reading is a missed poll, never a change
            log::debug!(
                "{} [LOCAL] status=reading_rejected rx_bytes={} end={}",
                log_header,
                reply.len(),
                end
            );
            continue;
        };
        observe(&mut slots, &profile, region);

        let mut changed = false;
        let now = Instant::now();

        // Removals: a known card whose slot reads empty leaves only once the
        // reader of that slot confirms it.
        for index in 0..slots.len() {
            let slot = (index + 1) as u16;
            if !slots[index].class.has_card() {
                // nothing to retry on a slot without a card
                slots[index].failed = false;
                slots[index].fail_reason = None;
                slots[index].failures = 0;
                slots[index].retry_at = None;
            }
            if slots[index].card.is_none() || slots[index].class.has_card() {
                continue;
            }
            if super::access::auth_active(&port, slot) {
                continue;
            }
            if confirmed_empty(&profile, &port, slot, &log_header).await {
                log::info!(
                    "{} [LOCAL] status=card_removed slot={} card_id={}",
                    log_header,
                    slot,
                    slots[index].card.as_deref().unwrap_or("-")
                );
                slots[index].card = None;
                slots[index].led = None;
                write_led(&profile, &port, profile.led_off.as_deref(), slot, &log_header).await;
                changed = true;
            }
        }

        // Identification of cards not known yet, a batch per poll.
        let mut identified = 0;
        for index in 0..slots.len() {
            if identified >= IDENTIFY_BATCH || profile.identify.is_empty() {
                break;
            }
            let slot = (index + 1) as u16;
            let state = &slots[index];
            if state.card.is_some()
                || !matches!(state.class, Class::Idle | Class::Error)
                || state.retry_at.is_some_and(|at| at > now)
                || super::access::auth_active(&port, slot)
                || blink_state(&rack_id, slot) == Some(true)
            {
                continue;
            }
            if !rack_port_is_live(&rack_id, &port) {
                break;
            }
            identified += 1;
            match identify(&profile, &port, slot, &log_header).await {
                Ok(id) => {
                    log::info!(
                        "{} [LOCAL] status=card_identified slot={} card_id={}",
                        log_header,
                        slot,
                        id
                    );
                    let state = &mut slots[index];
                    state.card = Some(id);
                    state.failed = false;
                    state.fail_reason = None;
                    state.failures = 0;
                    state.retry_at = None;
                    state.led = None;
                    changed = true;
                }
                Err(reason) => {
                    let state = &mut slots[index];
                    // a slot an authentication owns is not a failure of the card
                    if reason != "card_busy" {
                        state.failed = true;
                        state.fail_reason = Some(reason);
                        state.failures = state.failures.saturating_add(1);
                    }
                    let delay = identify_retry_delay(state.failures);
                    state.retry_at = Some(Instant::now() + delay);
                    state.led = None;
                    log::warn!(
                        "{} [LOCAL] status=identify_failed slot={} reason={} retry_secs={}",
                        log_header,
                        slot,
                        reason,
                        delay.as_secs()
                    );
                }
            }
        }

        if !rack_port_is_live(&rack_id, &port) {
            break;
        }
        if changed || first_pass {
            apply_cards(&slots, &rack_id, &port, &log_header).await;
            first_pass = false;
        }

        // Indicators: a served card, a card nobody serves (yet), and a repaint
        // after every authentication, which drops what was written before it.
        let served = served_slots(&rack_id);
        for index in 0..slots.len() {
            let slot = (index + 1) as u16;
            // A blink owns the indicator while it runs; once it has ended the
            // indicator of the slot is written anew.
            match blink_state(&rack_id, slot) {
                Some(true) => continue,
                Some(false) => {
                    lock(&BLINKS).remove(&(rack_id.clone(), slot));
                    slots[index].led = None;
                }
                None => {}
            }
            let state = &slots[index];
            let wanted = match &state.card {
                Some(id) if served.iter().any(|(s, i)| *s == slot && i == id) => Led::Served,
                Some(_) => Led::Unserved,
                None if state.failed && state.class.has_card() => Led::Unserved,
                None => continue,
            };
            if super::access::auth_active(&port, slot) {
                slots[index].was_active = true;
                continue;
            }
            if slots[index].was_active {
                slots[index].was_active = false;
                slots[index].led = None;
            }
            if slots[index].led == Some(wanted) {
                continue;
            }
            let template = match wanted {
                Led::Served => profile.led_served.as_deref(),
                Led::Unserved => profile.led_unserved.as_deref(),
            };
            if template.is_none() || write_led(&profile, &port, template, slot, &log_header).await {
                slots[index].led = Some(wanted);
            }
        }

        if rack_port_is_live(&rack_id, &port) {
            rack_update_slots(&rack_id, slot_table(&slots, profile.gap));
        }
    }
}

/// One running engine: its task, the profile text it was started with and
/// the port it works on.
struct Engine {
    arming: u64,
    profile_text: Vec<u8>,
    profile: Arc<Profile>,
    port: SharedPort,
    handle: JoinHandle<()>,
}

static NEXT_ARMING: AtomicU64 = AtomicU64::new(1);

lazy_static::lazy_static! {
    /// Engines of the racks in local mode, keyed by rack id.
    static ref ENGINES: std::sync::Mutex<HashMap<String, Engine>> =
        std::sync::Mutex::new(HashMap::new());
    /// Slot blinks by rack id and slot: `true` while one runs, `false` once it
    /// ended and the engine has not written the indicator of the slot again.
    static ref BLINKS: std::sync::Mutex<HashMap<(String, u16), bool>> =
        std::sync::Mutex::new(HashMap::new());
}

fn blink_state(rack_id: &str, slot: u16) -> Option<bool> {
    lock(&BLINKS).get(&(rack_id.to_string(), slot)).copied()
}

/// Blinks the indicator of one slot so the operator finds it on the rack.
/// Returns once the first indicator write was taken, with the length of the
/// whole blink; the rest runs in the background. Refused for a rack that is
/// not run locally, a slot an authentication owns, or a slot already blinking.
pub(super) async fn blink(rack_id: &str, slot: u16) -> Result<Duration, &'static str> {
    let (profile, port) = {
        let engines = lock(&ENGINES);
        let engine = engines
            .get(rack_id)
            .filter(|engine| !engine.handle.inner().is_finished())
            .ok_or("not_local")?;
        (engine.profile.clone(), engine.port.clone())
    };
    if slot == 0 || usize::from(slot) > profile.status_length {
        return Err("bad_slot");
    }
    // The served indicator alternates with the unserved one, or with dark
    // when the profile has no unserved indicator.
    let on = profile.led_served.clone().ok_or("no_indicator")?;
    let other = profile
        .led_unserved
        .clone()
        .or_else(|| profile.led_off.clone())
        .ok_or("no_indicator")?;
    if super::access::auth_active(&port, slot) {
        return Err("card_busy");
    }
    let key = (rack_id.to_string(), slot);
    {
        let mut blinks = lock(&BLINKS);
        if blinks.get(&key) == Some(&true) {
            return Err("already_blinking");
        }
        blinks.insert(key.clone(), true);
    }
    let log_header = format!("RACK {} |", rack_id);
    if !write_led(&profile, &port, Some(&on), slot, &log_header).await {
        lock(&BLINKS).insert(key, false);
        log::warn!("{} [LOCAL] status=blink_failed slot={} reason=led_rejected", log_header, slot);
        return Err("led_rejected");
    }
    log::info!("{} [LOCAL] status=blink_started slot={}", log_header, slot);
    let rack = rack_id.to_string();
    async_runtime::spawn(async move {
        let start = tokio::time::Instant::now();
        let mut ended = "done";
        for half in 1..BLINK_HALVES {
            tokio::time::sleep_until(start + BLINK_HALF * half).await;
            if !rack_port_is_live(&rack, &port) {
                ended = "rack_gone";
                break;
            }
            // an authentication that took the slot meanwhile goes first
            if super::access::auth_active(&port, slot) {
                ended = "card_busy";
                break;
            }
            let template = if half % 2 == 0 { &on } else { &other };
            write_led(&profile, &port, Some(template), slot, &log_header).await;
        }
        if ended == "done" {
            tokio::time::sleep_until(start + BLINK_HALF * BLINK_HALVES).await;
            // Dark until the engine writes the indicator of the slot again: a
            // slot without a card stays dark, a card gets its own colour back.
            write_led(&profile, &port, profile.led_off.as_deref(), slot, &log_header).await;
        }
        lock(&BLINKS).insert(key, false);
        log::info!("{} [LOCAL] status=blink_finished slot={} reason={}", log_header, slot, ended);
    });
    Ok(BLINK_HALF * BLINK_HALVES)
}

/// Whether the rack is run locally right now.
pub(super) fn is_active(rack_id: &str) -> bool {
    lock(&ENGINES)
        .get(rack_id)
        .is_some_and(|engine| !engine.handle.inner().is_finished())
}

/// A `rack/<id>/profile` of the server: starts the engine of the rack. The
/// same profile for a rack whose engine already runs on this port changes
/// nothing — that is what every reconnect of the application delivers.
pub(super) fn start(payload: &[u8], rack_id: &str, port: &SharedPort, log_header: &str) {
    let profile = match Profile::parse(payload) {
        Ok(profile) => profile,
        Err(reason) => {
            log::warn!("{} [LOCAL] status=profile_rejected reason={}", log_header, reason);
            return;
        }
    };
    let mut engines = lock(&ENGINES);
    if engines.get(rack_id).is_some_and(|engine| {
        engine.profile_text == payload
            && std::sync::Arc::ptr_eq(&engine.port, port)
            && !engine.handle.inner().is_finished()
    }) {
        log::debug!("{} [LOCAL] status=profile_unchanged", log_header);
        return;
    }
    // Same guard as the presence watch: a rack unlinked in between must not
    // get an engine that polls a dead port.
    if !rack_port_is_live(rack_id, port) {
        log::info!("{} [LOCAL] status=skipped reason=rack_not_linked", log_header);
        return;
    }
    log::info!(
        "{} [LOCAL] status=started slots={} identify_steps={} interval_ms={}",
        log_header,
        profile.status_length,
        profile.identify.len(),
        profile.status_interval.as_millis()
    );
    let arming = NEXT_ARMING.fetch_add(1, Ordering::Relaxed);
    let profile = Arc::new(profile);
    let task_profile = profile.clone();
    let task_rack = rack_id.to_string();
    let task_port = port.clone();
    let handle = async_runtime::spawn(async move {
        run(task_profile, task_rack.clone(), task_port).await;
        let mut engines = lock(&ENGINES);
        if engines.get(&task_rack).is_some_and(|engine| engine.arming == arming) {
            engines.remove(&task_rack);
        }
    });
    if let Some(old) = engines.insert(
        rack_id.to_string(),
        Engine {
            arming,
            profile_text: payload.to_vec(),
            profile,
            port: port.clone(),
            handle,
        },
    ) {
        old.handle.abort();
    }
}

/// Stops the engine of one rack, if it runs: the rack went away, or the
/// server drives it itself.
pub(super) fn stop(rack_id: &str, reason: &str) {
    if let Some(engine) = lock(&ENGINES).remove(rack_id) {
        engine.handle.abort();
        log::info!("RACK {} | [LOCAL] status=stopped reason={}", rack_id, reason);
    }
}

/// Stops every engine. Shutdown backstop.
pub(super) fn stop_all() {
    for (rack_id, engine) in lock(&ENGINES).drain() {
        engine.handle.abort();
        log::info!("RACK {} | [LOCAL] status=stopped reason=shutdown", rack_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A made-up device: frames are `A0 <addr> <len> <code> <data..> <checksum>`.
    const PROFILE: &str = r#"{
        "version": 1,
        "check": "sum8",
        "status": {
            "cmd": "a000007070", "interval_ms": 1000, "idle_ms": 100, "deadline_ms": 900,
            "head": "A0000C", "len": 17, "offset": 4, "length": 12,
            "mask": "0F", "stable": 2, "gap": 4,
            "classes": {"free": "01", "none": "00", "idle": "02", "error": "0308"}
        },
        "slot": {
            "idle_ms": 40, "deadline_ms": 400,
            "probe": {"cmd": "A0{a}0010{c}", "empty": ["A0{a}0001{c}"]},
            "led": {"served": "A0{a}0021{c}", "unserved": "A0{a}0022{c}", "off": "A0{a}0020{c}"},
            "poll": {"cmd": "A0{a}0010{c}", "while": "A0{a}0004{c}", "interval_ms": 20, "deadline_ms": 3000},
            "identify": [
                {"cmd": "A0{a}0030{c}", "expect": "A0{a}0006{c}", "deadline_ms": 1200,
                 "ok": {"head": "A0{a}", "match": [{"at": 3, "mask": "0F", "eq": "05"}]}},
                {"cmd": "A0{a}0240BEEF{c}", "expect": "A0{a}0006{c}",
                 "ok": {"len": 9, "head": "A0{a}04", "match": [{"at": 3, "mask": "0F", "eq": "05"}],
                        "take_at": 4, "take_len": 4}}
            ]
        }
    }"#;

    fn profile() -> Profile {
        Profile::parse(PROFILE.as_bytes()).unwrap()
    }

    /// A frame of the made-up device with its checksum appended.
    fn frame(body: &[u8]) -> Vec<u8> {
        let mut bytes = body.to_vec();
        bytes.push(0u8.wrapping_sub(sum8(&bytes)));
        bytes
    }

    #[test]
    fn template_gets_the_slot_address_and_a_zero_sum_checksum() {
        let bytes = fill("A0{a}0010{c}", 0x12, Check::Sum8).unwrap();
        assert_eq!(&bytes[..4], &[0xA0, 0x12, 0x00, 0x10]);
        assert_eq!(sum8(&bytes), 0);
        assert_eq!(fill("A0{a}", 7, Check::None), Some(vec![0xA0, 0x07]));
        // a checksum placeholder without a checksum rule, or broken hex
        assert_eq!(fill("A0{a}{c}", 7, Check::None), None);
        assert_eq!(fill("A0{a}0", 7, Check::Sum8), None);
    }

    #[test]
    fn profile_is_parsed_with_its_timings_and_rejected_when_malformed() {
        let p = profile();
        assert_eq!(p.status_cmd, "A000007070");
        assert_eq!(p.status_interval, Duration::from_millis(1000));
        assert_eq!((p.status_offset, p.status_length, p.stable, p.gap), (4, 12, 2, 4));
        assert_eq!(p.identify.len(), 2);
        assert_eq!(p.identify[0].deadline, Duration::from_millis(1200));
        assert_eq!(p.identify[1].deadline, Duration::from_millis(400));
        assert!(Profile::parse(b"{}").is_err());
        assert!(Profile::parse(PROFILE.replace("\"version\": 1", "\"version\": 2").as_bytes()).is_err());
        assert!(Profile::parse(PROFILE.replace("A0{a}0021{c}", "A0{a}002{c}").as_bytes()).is_err());
        assert!(Profile::parse(PROFILE.replace("\"length\": 12", "\"length\": 14").as_bytes()).is_err());
    }

    #[test]
    fn status_reading_is_taken_only_from_an_intact_reply() {
        let p = profile();
        let mut body = vec![0xA0, 0x00, 0x0C, 0x75];
        body.extend_from_slice(&[2, 1, 0, 0x48, 3, 1, 0, 0, 0, 0, 0, 0]);
        let reply = frame(&body);
        let region = p.status_region(&reply).unwrap();
        assert_eq!(region.len(), 12);
        assert_eq!(p.classify(region[0]), Class::Idle);
        assert_eq!(p.classify(region[1]), Class::Free);
        assert_eq!(p.classify(region[2]), Class::None);
        // the mask drops the high bits: 0x48 is the error value 0x08
        assert_eq!(p.classify(region[3]), Class::Error);
        assert_eq!(p.classify(0x07), Class::Busy);
        // bytes after the frame do not spoil it, a cut or damaged one is rejected
        let mut glued = reply.clone();
        glued.extend_from_slice(&[0xA0, 0x05, 0x00]);
        assert!(p.status_region(&glued).is_some());
        assert!(p.status_region(&reply[..16]).is_none());
        let mut damaged = reply.clone();
        damaged[6] ^= 0x01;
        assert!(p.status_region(&damaged).is_none());
    }

    #[test]
    fn rule_checks_the_slot_address_and_yields_the_card_id() {
        let p = profile();
        let rule = &p.identify[1].ok;
        let reply = frame(&[0xA0, 0x03, 0x04, 0x45, 0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(p.apply(rule, 3, &reply), Some(Some(vec![0xDE, 0xAD, 0xBE, 0xEF])));
        // the same frame is not an answer of another slot
        assert_eq!(p.apply(rule, 4, &reply), None);
        // another status, a short reply
        let busy = frame(&[0xA0, 0x03, 0x04, 0x44, 0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(p.apply(rule, 3, &busy), None);
        assert_eq!(p.apply(rule, 3, &reply[..8]), None);
        // a rule without a length judges the whole reply and yields nothing
        let accepted = frame(&[0xA0, 0x03, 0x02, 0x05, 0x11, 0x22]);
        assert_eq!(p.apply(&p.identify[0].ok, 3, &accepted), Some(None));
    }

    #[test]
    fn outcome_is_found_behind_the_accepted_and_working_frames_of_one_read() {
        let p = profile();
        let step = &p.identify[1];
        let taken = frame(&[0xA0, 0x03, 0x00, 0x06]);
        let working = frame(&[0xA0, 0x03, 0x00, 0x04]);
        let result = frame(&[0xA0, 0x03, 0x04, 0x45, 0xDE, 0xAD, 0xBE, 0xEF]);
        let id = Some(Some(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        for lead in [vec![], taken.clone(), working.clone(), [taken.clone(), working.clone()].concat()] {
            let reply = [lead, result.clone()].concat();
            assert_eq!(p.apply(&step.ok, 3, p.outcome_part(step, 3, &reply)), id);
        }
        // a lone accepted frame is the reply itself, not something to skip
        assert_eq!(p.outcome_part(step, 3, &taken), &taken[..]);
        // frames of another slot are not skipped
        let foreign = [frame(&[0xA0, 0x04, 0x00, 0x06]), result.clone()].concat();
        assert_eq!(p.apply(&step.ok, 3, p.outcome_part(step, 3, &foreign)), None);
    }

    #[test]
    fn step_envelope_carries_the_filled_frames_of_the_slot() {
        let p = profile();
        let envelope = p.step_envelope(&p.identify[0], 2).unwrap();
        assert_eq!(envelope.cmd_hex, hex::encode_upper(frame(&[0xA0, 0x02, 0x00, 0x30])));
        assert_eq!(
            envelope.expect_hex,
            Some(hex::encode_upper(frame(&[0xA0, 0x02, 0x00, 0x06])))
        );
        let poll = envelope.poll.unwrap();
        assert_eq!(poll.cmd_hex, hex::encode_upper(frame(&[0xA0, 0x02, 0x00, 0x10])));
        assert_eq!(poll.while_hex, hex::encode_upper(frame(&[0xA0, 0x02, 0x00, 0x04])));
        assert_eq!(envelope.deadline, Duration::from_millis(1200));
        assert_eq!(envelope.finish, None);
    }

    #[test]
    fn class_is_taken_after_it_held_for_the_stable_number_of_polls() {
        let p = profile();
        let mut slots = vec![SlotState::default(); 12];
        let reading = [2u8, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        observe(&mut slots, &p, &reading);
        assert_eq!(slots[0].class, Class::None);
        observe(&mut slots, &p, &reading);
        assert_eq!(slots[0].class, Class::Idle);
        assert_eq!(slots[1].class, Class::Free);
        // one poll showing the slot empty changes nothing
        observe(&mut slots, &p, &[1u8, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(slots[0].class, Class::Idle);
        observe(&mut slots, &p, &reading);
        observe(&mut slots, &p, &[1u8, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(slots[0].class, Class::Idle);
        observe(&mut slots, &p, &[1u8, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(slots[0].class, Class::Free);
    }

    #[test]
    fn slot_table_tells_gaps_faulty_readers_and_card_states_apart() {
        let mut slots = vec![SlotState::default(); 12];
        let set = |slots: &mut [SlotState], slot: usize, class: Class| {
            slots[slot - 1].class = class;
        };
        // 1..4: a card, a free reader, a faulty reader, an identified card
        set(&mut slots, 1, Class::Idle);
        set(&mut slots, 2, Class::Free);
        set(&mut slots, 4, Class::Busy);
        slots[3].card = Some("AA".to_string());
        // 5..8 silent: a hole in the addressing; 9: a card in error; 10: a failed one
        set(&mut slots, 9, Class::Error);
        set(&mut slots, 10, Class::Idle);
        slots[9].failed = true;
        let table = slot_table(&slots, 4);
        assert_eq!(table.hi, 10);
        assert_eq!(table.states.card, vec![1]);
        assert_eq!(table.states.free, vec![2]);
        assert_eq!(table.states.faulty, vec![3]);
        assert_eq!(table.states.discovered, vec![4]);
        assert_eq!(table.states.absent, vec![5, 6, 7, 8]);
        assert_eq!(table.states.error, vec![9, 10]);
        let reasons: Vec<_> = table.errors.iter().map(|e| (e.slot, e.reason.as_str())).collect();
        assert_eq!(reasons, vec![(9, "rack_error"), (10, "identify_failed")]);
        assert_eq!((table.total, table.occupied, table.free), (6, 4, 2));
        assert_eq!(slot_table(&vec![SlotState::default(); 12], 4).hi, 0);
    }

    #[test]
    fn delays_grow_with_failures_up_to_their_ceilings() {
        let interval = Duration::from_secs(2);
        assert_eq!(poll_delay(interval, 0), interval);
        assert_eq!(poll_delay(interval, 2), Duration::from_secs(8));
        assert_eq!(poll_delay(interval, 30), POLL_BACKOFF_MAX);
        assert_eq!(identify_retry_delay(0), IDENTIFY_RETRY_INITIAL);
        assert_eq!(identify_retry_delay(30), IDENTIFY_RETRY_MAX);
    }
}
