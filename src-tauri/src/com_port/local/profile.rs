//! The profile of a rack: its parsed templates and byte rules, and the frames
//! and judgements filled from them for one slot. Every byte comes from the
//! server's profile (see the module docs of `local`).

use std::time::Duration;

use crate::com_port::transport::{
    normalize_hex, PollSpec, ReplyComplete, SerialEnvelope, ShutdownCommand, SERIAL_MS_MAX,
    SERIAL_MS_MIN,
};

use super::shutdown::ShutdownSlots;

/// Highest slot address a rack can have.
const SLOT_MAX: usize = crate::com_port::SLOT_MAX as usize;

/// Integrity rule the profile names for its frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Check {
    None,
    /// An 8-bit additive check byte (see `fill`).
    Sum8,
}

impl Check {
    /// Whether a frame passes the check.
    fn passes(self, frame: &[u8]) -> bool {
        match self {
            Check::None => true,
            Check::Sum8 => !frame.is_empty() && sum8(frame) == 0,
        }
    }
}

/// The wrapping byte sum behind `Check::Sum8`.
pub(super) fn sum8(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |acc, b| acc.wrapping_add(*b))
}

/// Fills a frame template for one slot: `{a}` becomes the slot address byte,
/// a trailing `{c}` the checksum byte. `None` for a template that is not hex
/// around its placeholders, or asks for a checksum the profile does not define.
pub(super) fn fill(template: &str, slot: u16, check: Check) -> Option<Vec<u8>> {
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
pub(super) struct Rule {
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
pub(super) struct Step {
    cmd: String,
    expect: Option<String>,
    deadline: Duration,
    pub(super) ok: Rule,
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
pub(super) enum Class {
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
    /// Whether the slot holds a card, in whatever state.
    pub(super) fn has_card(self) -> bool {
        matches!(self, Class::Idle | Class::Error | Class::Busy)
    }
}

/// The parsed profile of a rack (see the module docs).
#[derive(Debug)]
pub(super) struct Profile {
    pub(super) check: Check,
    pub(super) status_cmd: String,
    pub(super) status_interval: Duration,
    pub(super) status_idle: Duration,
    pub(super) status_deadline: Duration,
    status_head: Vec<u8>,
    status_len: usize,
    status_offset: usize,
    pub(super) status_length: usize,
    mask: u8,
    pub(super) stable: u32,
    pub(super) gap: usize,
    free: Vec<u8>,
    none: Vec<u8>,
    idle: Vec<u8>,
    error: Vec<u8>,
    pub(super) slot_idle: Duration,
    pub(super) slot_deadline: Duration,
    pub(super) probe_cmd: Option<String>,
    pub(super) probe_empty: Vec<String>,
    pub(super) led_served: Option<String>,
    pub(super) led_unserved: Option<String>,
    pub(super) led_off: Option<String>,
    poll: Option<PollTemplates>,
    pub(super) identify: Vec<Step>,
    /// The fixed bytes ahead of the `{a}` placeholder in the profile's own
    /// reply templates: what tells the slot named by unsolicited input.
    frame_head: Option<Vec<u8>>,
    /// The server answers the link report of every card session with a
    /// `status` publish: a slot is shown as served only once confirmed.
    pub(super) card_status: bool,
}

/// A string field of a profile section.
fn text<'a>(json: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    json.get(key).and_then(|v| v.as_str())
}

/// An unsigned number field of a profile section.
fn number(json: &serde_json::Value, key: &str) -> Option<u64> {
    json.get(key).and_then(|v| v.as_u64())
}

/// A timing field of the profile, bounded like the timings of a server
/// envelope (`transport::parse_envelope_fields`): a zero silence bound would
/// end every read on its first chunk, a zero deadline would expire every
/// exchange before its command is written.
fn millis(json: &serde_json::Value, key: &str, default: u64) -> Duration {
    let ms = number(json, key).unwrap_or(default);
    Duration::from_millis(ms.clamp(SERIAL_MS_MIN, SERIAL_MS_MAX))
}

/// The values of one status class, empty when the profile lists none.
fn byte_set(json: &serde_json::Value, key: &str) -> Result<Vec<u8>, String> {
    match text(json, key) {
        Some(hex_text) => hex::decode(hex_text).map_err(|_| format!("bad_class {key}")),
        None => Ok(Vec::new()),
    }
}

/// Parses and validates the byte rule of an identification step.
fn parse_rule(json: &serde_json::Value) -> Result<Rule, String> {
    let mut rule = Rule {
        len: number(json, "len").map(|n| n as usize),
        head: text(json, "head").unwrap_or("").to_string(),
        ..Rule::default()
    };
    // The head is matched without a check byte (see `apply`): one that does
    // not fill that way would reject every reply instead of the profile.
    if fill(&rule.head, 1, Check::None).is_none() {
        return Err("bad_rule_head".to_string());
    }
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
            if mask
                .as_ref()
                .is_some_and(|m| m.len() != 1 && m.len() != expected.len())
            {
                return Err("bad_rule_mask".to_string());
            }
            rule.matches.push((at, mask, expected));
        }
    }
    if let (Some(at), Some(len)) = (number(json, "take_at"), number(json, "take_len")) {
        // An empty take would identify every card as "".
        if len == 0 {
            return Err("bad_rule_take".to_string());
        }
        rule.take = Some((at as usize, len as usize));
    }
    Ok(rule)
}

impl Profile {
    /// Parses and validates a profile: every template must fill for a slot,
    /// so a malformed profile is rejected as a whole instead of failing on
    /// the wire later.
    pub(super) fn parse(payload: &[u8]) -> Result<Profile, String> {
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
            || status_offset
                .checked_add(status_length)
                .is_none_or(|end| end > status_len)
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
        if let Some(items) = probe
            .and_then(|p| p.get("empty"))
            .and_then(|v| v.as_array())
        {
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
                interval: millis(p, "interval_ms", 20),
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
                    deadline: match number(item, "deadline_ms") {
                        Some(_) => millis(item, "deadline_ms", 0),
                        None => slot_deadline,
                    },
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
            status_interval: millis(status, "interval_ms", 2000),
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
            frame_head: None,
            card_status: json
                .get("card_status")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            poll,
            identify,
        }
        .with_frame_head())
    }

    /// Fills `frame_head` from the first reply template of the profile that
    /// carries the slot address: the hex before its `{a}` placeholder.
    fn with_frame_head(mut self) -> Self {
        let templates = self
            .identify
            .iter()
            .filter_map(|step| step.expect.clone())
            .chain(self.poll.as_ref().map(|p| p.while_reply.clone()))
            .chain(self.probe_empty.iter().cloned());
        self.frame_head = templates
            .filter_map(|t| t.find("{a}").and_then(|at| hex::decode(&t[..at]).ok()))
            .find(|head| !head.is_empty());
        self
    }

    /// Slots named in unsolicited input: every place the `frame_head` bytes
    /// are followed by an address of this rack. A false hit
    /// inside a frame body only costs one more identification.
    pub(super) fn event_slots(&self, bytes: &[u8]) -> Vec<u16> {
        let Some(head) = self.frame_head.as_deref() else {
            return Vec::new();
        };
        let mut slots = Vec::new();
        for at in 0..bytes.len() {
            if !bytes[at..].starts_with(head) {
                continue;
            }
            if let Some(&addr) = bytes.get(at + head.len()) {
                let slot = u16::from(addr);
                if slot >= 1 && usize::from(slot) <= self.status_length && !slots.contains(&slot) {
                    slots.push(slot);
                }
            }
        }
        slots
    }

    /// The class of one status byte, after the mask of the profile.
    pub(super) fn classify(&self, raw: u8) -> Class {
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
    pub(super) fn status_region<'a>(&self, reply: &'a [u8]) -> Option<&'a [u8]> {
        let frame = reply.get(..self.status_len)?;
        if !self.check.passes(frame) || !frame.starts_with(&self.status_head) {
            return None;
        }
        frame.get(self.status_offset..self.status_offset + self.status_length)
    }

    /// Judges a reply by a rule. `None` — not the expected result;
    /// `Some(bytes)` — it is, with the bytes the rule yields (if any).
    pub(super) fn apply(&self, rule: &Rule, slot: u16, reply: &[u8]) -> Option<Option<Vec<u8>>> {
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
            let part = frame.get(*at..at.checked_add(expected.len())?)?;
            let equal = part
                .iter()
                .zip(expected)
                .enumerate()
                .all(|(i, (got, want))| {
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
            Some((at, len)) => Some(Some(frame.get(at..at.checked_add(len)?)?.to_vec())),
            None => Some(None),
        }
    }

    /// The part of a step reply its rule judges. One read may carry several
    /// frames of the operation: the step's `expect` frame, or the poll's
    /// `while` frame, ahead of the outcome.
    /// Those are known byte for byte from the profile and are skipped here.
    pub(super) fn outcome_part<'a>(&self, step: &Step, slot: u16, reply: &'a [u8]) -> &'a [u8] {
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
        while let Some(frame) = known
            .iter()
            .find(|frame| rest.len() > frame.len() && rest.starts_with(frame))
        {
            rest = &rest[frame.len()..];
        }
        rest
    }

    /// A one-shot exchange with one slot.
    pub(super) fn slot_envelope(&self, template: &str, slot: u16) -> Option<SerialEnvelope> {
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
    pub(super) fn step_envelope(&self, step: &Step, slot: u16) -> Option<SerialEnvelope> {
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

    /// Learn a short-reply boundary from the server's own exact reply templates.
    /// Different known short replies must share a prefix beyond the address;
    /// the configured integrity check must validate the entire candidate frame.
    /// Profiles without enough evidence retain their silence-based read bound.
    pub(super) fn short_reply_complete(&self, slot: u16) -> Option<ReplyComplete> {
        if self.check != Check::Sum8 {
            return None;
        }
        let head = self.frame_head.as_deref()?;
        let addressed: Vec<u8> = head.iter().copied().chain([slot as u8]).collect();
        let mut frames: Vec<_> = self
            .probe_empty
            .iter()
            .map(String::as_str)
            .chain(
                self.identify
                    .iter()
                    .filter_map(|step| step.expect.as_deref()),
            )
            .chain(self.poll.as_ref().map(|poll| poll.while_reply.as_str()))
            .filter_map(|template| fill(template, slot, self.check))
            .filter(|frame| frame.starts_with(&addressed) && self.check.passes(frame))
            .collect();
        let len = frames.iter().map(Vec::len).min()?;
        frames.retain(|frame| frame.len() == len);
        frames.sort();
        frames.dedup();
        if frames.len() < 2 {
            return None;
        }
        let prefix: Vec<u8> = frames[0]
            .iter()
            .enumerate()
            .take_while(|(index, byte)| frames.iter().all(|frame| frame[*index] == **byte))
            .map(|(_, byte)| *byte)
            .collect();
        if prefix.len() <= addressed.len() {
            return None;
        }
        Some(Box::new(move |bytes| {
            bytes
                .windows(len)
                .any(|frame| frame.starts_with(&prefix) && sum8(frame) == 0)
        }))
    }

    /// The commands that darken the indicators of the slots seen on the rack;
    /// none without an off template.
    pub(super) fn shutdown_commands(&self, slots: &ShutdownSlots) -> Vec<ShutdownCommand> {
        self.led_off.as_deref().map_or_else(Vec::new, |template| {
            slots
                .addresses
                .iter()
                .copied()
                .filter(|slot| *slot > 0 && usize::from(*slot) <= self.status_length)
                .filter_map(|slot| {
                    Some(ShutdownCommand {
                        bytes: fill(template, slot, self.check)?,
                        complete: self.short_reply_complete(slot),
                    })
                })
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_profile::{frame, profile, BUSY, FREE, IDLE, NONE, PROFILE};
    use super::*;

    #[test]
    fn shutdown_reply_boundary_requires_a_complete_addressed_intact_short_frame() {
        let profile = profile();
        let complete = profile.short_reply_complete(3).unwrap();
        let reply = frame(&[0xD2, 0x4B, 3, 0xE7, 0x5E]);
        for end in 0..reply.len() {
            assert!(!complete(&reply[..end]));
        }
        assert!(complete(&reply));
        assert!(!complete(&frame(&[0xD2, 0x4B, 4, 0xE7, 0x5E])));
        let mut damaged = reply.clone();
        *damaged.last_mut().unwrap() ^= 1;
        assert!(!complete(&damaged));
        assert!(!complete(&frame(&[0xD2, 0x4B, 3, 0xE7, 0x5E, 0xBE, 0xEF])));
        let mut with_event = frame(&[0xD2, 0x4B, 4, 0xE7, 0x1A]);
        with_event.extend(reply);
        assert!(complete(&with_event));
    }

    #[test]
    fn shutdown_reply_boundary_falls_back_without_consistent_checked_templates() {
        let mut profile = profile();
        profile.check = Check::None;
        assert!(profile.short_reply_complete(3).is_none());
        profile.check = Check::Sum8;
        profile.probe_empty = vec!["D24B{a}E61A{c}".into()];
        assert!(profile.short_reply_complete(3).is_none());
        profile.probe_empty = vec!["D24B{a}E71A{c}".into()];
        profile.identify.clear();
        profile.poll = None;
        assert!(profile.short_reply_complete(3).is_none());
    }

    #[test]
    fn template_gets_the_slot_address_and_a_zero_sum_checksum() {
        let bytes = fill("D24B{a}B1{c}", 0x12, Check::Sum8).unwrap();
        assert_eq!(&bytes[..4], &[0xD2, 0x4B, 0x12, 0xB1]);
        assert_eq!(sum8(&bytes), 0);
        assert_eq!(
            fill("D24B{a}", 7, Check::None),
            Some(vec![0xD2, 0x4B, 0x07])
        );
        // a checksum placeholder without a checksum rule, or broken hex
        assert_eq!(fill("D24B{a}{c}", 7, Check::None), None);
        assert_eq!(fill("D24B{a}0", 7, Check::Sum8), None);
    }

    #[test]
    fn profile_is_parsed_with_its_timings_and_rejected_when_malformed() {
        let p = profile();
        assert_eq!(p.status_cmd, "D24B00A1C0");
        assert_eq!(p.status_interval, Duration::from_millis(1000));
        assert_eq!(
            (p.status_offset, p.status_length, p.stable, p.gap),
            (5, 12, 2, 4)
        );
        assert_eq!(p.identify.len(), 2);
        assert_eq!(p.identify[0].deadline, Duration::from_millis(1200));
        assert_eq!(p.identify[1].deadline, Duration::from_millis(400));
        // timings below the serial floor are raised to it
        let fast = PROFILE
            .replace("\"idle_ms\": 40", "\"idle_ms\": 0")
            .replace("\"deadline_ms\": 1200", "\"deadline_ms\": 5");
        let fast = Profile::parse(fast.as_bytes()).unwrap();
        assert_eq!(fast.slot_idle, Duration::from_millis(SERIAL_MS_MIN));
        assert_eq!(
            fast.identify[0].deadline,
            Duration::from_millis(SERIAL_MS_MIN)
        );
        assert!(Profile::parse(b"{}").is_err());
        assert!(Profile::parse(
            PROFILE
                .replace("\"version\": 1", "\"version\": 2")
                .as_bytes()
        )
        .is_err());
        assert!(Profile::parse(PROFILE.replace("D24B{a}B5{c}", "D24B{a}B{c}").as_bytes()).is_err());
        assert!(Profile::parse(
            PROFILE
                .replace("\"length\": 12", "\"length\": 14")
                .as_bytes()
        )
        .is_err());
        assert!(Profile::parse(
            PROFILE
                .replace("\"head\": \"D24B{a}E8\"", "\"head\": \"D24B{a}E8{c}\"")
                .as_bytes()
        )
        .is_err());
    }

    #[test]
    fn status_reading_is_taken_only_from_an_intact_reply() {
        let p = profile();
        let mut body = vec![0xD2, 0x4B, 0x00, 0xA1, 0xC0];
        body.extend_from_slice(&[
            IDLE, FREE, NONE, 0xCE, 0x59, FREE, NONE, NONE, NONE, NONE, NONE, NONE,
        ]);
        let reply = frame(&body);
        let region = p.status_region(&reply).unwrap();
        assert_eq!(region.len(), 12);
        assert_eq!(p.classify(region[0]), Class::Idle);
        assert_eq!(p.classify(region[1]), Class::Free);
        assert_eq!(p.classify(region[2]), Class::None);
        // the mask drops the high bit: 0xCE is the error value 0x4E
        assert_eq!(p.classify(region[3]), Class::Error);
        assert_eq!(p.classify(region[4]), Class::Error);
        assert_eq!(p.classify(BUSY), Class::Busy);
        // bytes after the frame do not spoil it, a cut or damaged one is rejected
        let mut glued = reply.clone();
        glued.extend_from_slice(&[0xD2, 0x4B, 0x05]);
        assert!(p.status_region(&glued).is_some());
        assert!(p.status_region(&reply[..17]).is_none());
        let mut damaged = reply.clone();
        damaged[6] ^= 0x01;
        assert!(p.status_region(&damaged).is_none());
    }

    #[test]
    fn rule_checks_the_slot_address_and_yields_the_card_id() {
        let p = profile();
        let rule = &p.identify[1].ok;
        let reply = frame(&[0xD2, 0x4B, 0x03, 0xE8, 0xC3, 0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(
            p.apply(rule, 3, &reply),
            Some(Some(vec![0xDE, 0xAD, 0xBE, 0xEF]))
        );
        // the mask lets the bits it clears vary
        let flagged = frame(&[0xD2, 0x4B, 0x03, 0xE8, 0xC7, 0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(p.apply(rule, 3, &flagged).is_some());
        // the same frame is not an answer of another slot
        assert_eq!(p.apply(rule, 4, &reply), None);
        // another status, a short reply
        let other = frame(&[0xD2, 0x4B, 0x03, 0xE8, 0xD3, 0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(p.apply(rule, 3, &other), None);
        assert_eq!(p.apply(rule, 3, &reply[..9]), None);
        // a rule without a length judges the whole reply and yields nothing
        let accepted = frame(&[0xD2, 0x4B, 0x03, 0xE8, 0x11, 0x22]);
        assert_eq!(p.apply(&p.identify[0].ok, 3, &accepted), Some(None));
    }

    #[test]
    fn outcome_is_found_behind_the_accepted_and_working_frames_of_one_read() {
        let p = profile();
        let step = &p.identify[1];
        let taken = frame(&[0xD2, 0x4B, 0x03, 0xE7, 0x5E]);
        let working = frame(&[0xD2, 0x4B, 0x03, 0xE7, 0xA7]);
        let result = frame(&[0xD2, 0x4B, 0x03, 0xE8, 0xC3, 0xDE, 0xAD, 0xBE, 0xEF]);
        let id = Some(Some(vec![0xDE, 0xAD, 0xBE, 0xEF]));
        for lead in [
            vec![],
            taken.clone(),
            working.clone(),
            [taken.clone(), working.clone()].concat(),
        ] {
            let reply = [lead, result.clone()].concat();
            assert_eq!(p.apply(&step.ok, 3, p.outcome_part(step, 3, &reply)), id);
        }
        // a lone accepted frame is the reply itself, not something to skip
        assert_eq!(p.outcome_part(step, 3, &taken), &taken[..]);
        // frames of another slot are not skipped
        let foreign = [frame(&[0xD2, 0x4B, 0x04, 0xE7, 0x5E]), result.clone()].concat();
        assert_eq!(
            p.apply(&step.ok, 3, p.outcome_part(step, 3, &foreign)),
            None
        );
    }

    #[test]
    fn step_envelope_carries_the_filled_frames_of_the_slot() {
        let p = profile();
        let envelope = p.step_envelope(&p.identify[0], 2).unwrap();
        assert_eq!(
            envelope.cmd_hex,
            hex::encode_upper(frame(&[0xD2, 0x4B, 0x02, 0xB9]))
        );
        assert_eq!(
            envelope.expect_hex,
            Some(hex::encode_upper(frame(&[0xD2, 0x4B, 0x02, 0xE7, 0x5E])))
        );
        let poll = envelope.poll.unwrap();
        assert_eq!(
            poll.cmd_hex,
            hex::encode_upper(frame(&[0xD2, 0x4B, 0x02, 0xB1]))
        );
        assert_eq!(
            poll.while_hex,
            hex::encode_upper(frame(&[0xD2, 0x4B, 0x02, 0xE7, 0xA7]))
        );
        assert_eq!(envelope.deadline, Duration::from_millis(1200));
        assert_eq!(envelope.finish, None);
    }

    #[test]
    fn rack_events_name_their_slots_by_the_profile_frame_head() {
        let p = profile();
        assert_eq!(p.frame_head.as_deref(), Some(&[0xD2u8, 0x4B][..]));
        // two frames back to back, naming slots 3 and 5
        let bytes = [
            frame(&[0xD2, 0x4B, 0x03, 0xE7, 0x1A]),
            frame(&[0xD2, 0x4B, 0x05, 0xE8, 0xC3]),
        ]
        .concat();
        assert_eq!(p.event_slots(&bytes), vec![3, 5]);
        // an address past the rack is not a slot
        assert!(p
            .event_slots(&frame(&[0xD2, 0x4B, 0xF0, 0xE7, 0x1A]))
            .is_empty());
    }
}
