//! The engine of one rack: the status poll loop and the serial work it does
//! for the slots — identification, the removal check and the indicators.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::global_app_handle::rack_update_slots;

use crate::com_port::cards::{apply_cards_set, inventory, RACK_CARDS_UI};
use crate::com_port::transport::{
    execute_envelope, execute_envelope_timed, take_unsolicited, SerialEnvelope, SharedPort,
    SERIAL_ERR_CARD_BUSY,
};
use crate::com_port::{access, lock, rack_io_key, rack_port_is_live};

use super::blink::{blink_state, BLINKS};
use super::profile::{fill, Class, Profile};
use super::shutdown::ShutdownSlots;
use super::slots::{
    assign_card, clear_failure, first_walk_done, forget_card, identify_retry_delay, observe,
    on_rack_event, on_reading_change, poll_delay, slot_table, wanted_led, Led, SlotState,
};
use super::ENGINES;

/// Cards identified per status poll before the card set is applied and the
/// status is read again: the first walk of a full rack then serves its cards
/// as they are identified instead of after the last one.
const IDENTIFY_BATCH: usize = 8;

/// Failed identifications of a card that may have been swapped before its
/// session is closed. Until then the session keeps running: a card that was
/// not swapped at all must not lose it to one failed attempt.
const RECHECK_ATTEMPTS: u32 = 2;

/// Indicators of empty readers written per status poll. Every empty reader is
/// darkened once after the engine starts; a full pass over a large rack in one
/// poll would keep card sessions waiting for the port.
const LED_OFF_BATCH: usize = 16;

/// Releases the identification lease of a slot when dropped, also when the
/// engine is aborted in the middle of an identification.
struct IdentifyLease<'a>(&'a SharedPort, u16, u64);
impl Drop for IdentifyLease<'_> {
    fn drop(&mut self) {
        access::release(self.0, self.1, true, Some(self.2));
    }
}

/// Owner tokens of the identification leases of the engines.
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(access::LOCAL_TOKEN_BASE);

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
        if !access::acquire(port, slot, true, token).await {
            return Err(SERIAL_ERR_CARD_BUSY);
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
                // profile that does not fit the reader. Its bytes are device
                // protocol: only the debug log the server switches on has them.
                log::warn!(
                    "{} [LOCAL] status=identify_reply_rejected slot={} step={} rx_bytes={}",
                    log_header,
                    slot,
                    step_index + 1,
                    reply.len()
                );
                log::debug!(
                    "{} [LOCAL] identify_reply_rejected slot={} reply={}",
                    log_header,
                    slot,
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
async fn confirmed_empty(
    profile: &Profile,
    port: &SharedPort,
    slot: u16,
    log_header: &str,
) -> bool {
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
pub(super) async fn write_led(
    profile: &Profile,
    port: &SharedPort,
    template: Option<&str>,
    slot: u16,
    log_header: &str,
) -> bool {
    let Some(envelope) = template.and_then(|t| profile.slot_envelope(t, slot)) else {
        return false;
    };
    let (reply, _, wire) = execute_envelope_timed(port, envelope, log_header, false).await;
    if reply.is_ok() {
        // The normal reader includes its silence bound. Final cleanup ends at
        // the complete frame, so exclude that bound from the response estimate.
        let rtt = wire
            .saturating_sub(profile.slot_idle)
            .max(Duration::from_millis(1));
        if let Some(engine) = lock(&ENGINES)
            .values()
            .find(|engine| Arc::ptr_eq(&engine.port, port))
        {
            lock(&engine.shutdown_slots).timing.record(rtt);
        }
    }
    reply.is_ok()
}

/// Hands the identified cards of the rack to the session reconcile, exactly
/// as a server card set would.
async fn apply_cards(slots: &[SlotState], rack_id: &str, port: &SharedPort, log_header: &str) {
    // One place per card: a set listing an id twice is refused as a whole by
    // the reconcile, which would freeze every card of the rack.
    let mut seen = std::collections::HashSet::new();
    let cards: Vec<(u16, String)> = slots
        .iter()
        .enumerate()
        .filter_map(|(index, state)| {
            let id = state.card.as_ref().filter(|id| !id.is_empty())?;
            if !seen.insert(id.as_str()) {
                log::warn!(
                    "{} [LOCAL] status=duplicate_skipped slot={}",
                    log_header,
                    index + 1
                );
                return None;
            }
            Some(((index + 1) as u16, id.clone()))
        })
        .collect();
    apply_cards_set(
        cards,
        None,
        rack_id.to_string(),
        port.clone(),
        log_header.to_string(),
    )
    .await;
}

/// Slots of the rack whose card session is up and not refused by the server,
/// by the UI rows of the rack.
fn served_slots(rack_id: &str) -> Vec<(u16, String)> {
    lock(&RACK_CARDS_UI)
        .get(rack_id)
        .map(|rows| {
            rows.iter()
                .filter(|row| row.online == Some(true) && row.problem.is_none())
                .filter_map(|row| row.iccid.clone().map(|id| (row.slot, id)))
                .collect()
        })
        .unwrap_or_default()
}

/// What the status loop of an engine works with: fixed for its whole run.
struct EngineContext {
    profile: Arc<Profile>,
    rack_id: String,
    port: SharedPort,
    shutdown_slots: Arc<std::sync::Mutex<ShutdownSlots>>,
    log_header: String,
    /// Key of the rack events the transport keeps for the port (`rack_io_key`).
    events_key: usize,
}

/// What the status loop of an engine carries from one poll to the next.
struct EngineState {
    slots: Vec<SlotState>,
    /// Consecutive failed status exchanges: they stretch the poll interval.
    failures: u32,
    /// No card set was handed over yet: the first pass hands one over anyway.
    first_pass: bool,
    /// Status readings taken so far.
    polls: u32,
    /// The end of the first identification walk was reported to the UI.
    scan_reported: bool,
    /// Consecutive status replies the profile did not accept.
    rejected: u32,
}

/// The engine of one rack: polls the status and acts on what it shows until
/// the port of the rack goes away.
pub(super) async fn run(
    profile: Arc<Profile>,
    rack_id: String,
    port: SharedPort,
    shutdown_slots: Arc<std::sync::Mutex<ShutdownSlots>>,
) {
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
                lock(&shutdown_slots).addresses.insert(slot as u16);
            }
        }
    }
    // The port's identity for the rack events the transport keeps for us;
    // whatever it kept before this engine started is about a past state.
    // Taken from the rack record, not from the port: an authentication can
    // hold the port lock for a long time.
    let Some(events_key) = rack_io_key(&rack_id, &port) else {
        log::info!("{} [LOCAL] status=ended reason=link_down", log_header);
        return;
    };
    take_unsolicited(events_key);
    let ctx = EngineContext {
        profile,
        rack_id,
        port,
        shutdown_slots,
        log_header,
        events_key,
    };
    let mut state = EngineState {
        slots,
        failures: 0,
        first_pass: true,
        polls: 0,
        scan_reported: false,
        rejected: 0,
    };
    loop {
        tokio::time::sleep(poll_delay(ctx.profile.status_interval, state.failures)).await;
        if !rack_port_is_live(&ctx.rack_id, &ctx.port) {
            log::info!("{} [LOCAL] status=ended reason=link_down", ctx.log_header);
            break;
        }
        let Some(region) = state.read_status(&ctx).await else {
            continue;
        };
        state.apply_reading(&ctx, &region);
        state.apply_events(&ctx);

        let now = Instant::now();
        let removed = state.check_removals(&ctx).await;
        let identified = state.identify_batch(&ctx, now).await;

        if !rack_port_is_live(&ctx.rack_id, &ctx.port) {
            break;
        }
        state.hand_over_cards(&ctx, removed || identified).await;
        state.repaint_indicators(&ctx).await;

        if rack_port_is_live(&ctx.rack_id, &ctx.port) {
            rack_update_slots(&ctx.rack_id, slot_table(&state.slots, ctx.profile.gap));
        }
    }
}

// index loops: every pass awaits serial exchanges and writes the slot back
impl EngineState {
    /// Reads the status of the rack: its per-slot status bytes, `None` for a
    /// poll that brought none (a failed exchange or a rejected reply).
    async fn read_status(&mut self, ctx: &EngineContext) -> Option<Vec<u8>> {
        let envelope = SerialEnvelope {
            cmd_hex: ctx.profile.status_cmd.clone(),
            expect_hex: None,
            idle: ctx.profile.status_idle,
            deadline: ctx.profile.status_deadline,
            poll: None,
            finish: None,
        };
        let (exchange, end) = execute_envelope(&ctx.port, envelope, &ctx.log_header, false).await;
        if !exchange.is_ok() {
            self.failures = self.failures.saturating_add(1);
            return None;
        }
        self.failures = 0;
        let reply = hex::decode(&exchange.resp_hex).unwrap_or_default();
        let Some(region) = ctx.profile.status_region(&reply) else {
            // a cut or damaged reading is a missed poll, never a change
            log::debug!(
                "{} [LOCAL] status=reading_rejected rx_bytes={} end={}",
                ctx.log_header,
                reply.len(),
                end
            );
            // One cut reading is a missed poll; a streak means the profile does not fit the rack.
            self.rejected = self.rejected.saturating_add(1);
            if self.rejected == 3 {
                log::warn!(
                    "{} [LOCAL] status=reading_rejected_repeatedly rx_bytes={} end={}",
                    ctx.log_header,
                    reply.len(),
                    end
                );
            }
            return None;
        };
        self.rejected = 0;
        Some(region.to_vec())
    }

    /// Feeds one status reading into the slots and acts on the slots whose
    /// reading changed.
    fn apply_reading(&mut self, ctx: &EngineContext, region: &[u8]) {
        lock(&ctx.shutdown_slots).remember_status(&ctx.profile, region);
        for (index, from, to) in observe(&mut self.slots, &ctx.profile, region) {
            // a card at work flips between idle and busy all the time
            if !matches!(
                (from, to),
                (Class::Idle, Class::Busy) | (Class::Busy, Class::Idle)
            ) {
                log::info!(
                    "{} [LOCAL] status=slot_reading slot={} from={:?} to={:?} card_id={}",
                    ctx.log_header,
                    index + 1,
                    from,
                    to,
                    self.slots[index].card.as_deref().unwrap_or("-")
                );
            }
            on_reading_change(&mut self.slots[index], from, to);
        }
        self.polls = self.polls.saturating_add(1);
    }

    /// Acts on the unsolicited input the rack sent: the slots it names.
    fn apply_events(&mut self, ctx: &EngineContext) {
        for bytes in take_unsolicited(ctx.events_key) {
            let named = ctx.profile.event_slots(&bytes);
            log::info!(
                "{} [LOCAL] status=rack_event bytes={} slots={:?}",
                ctx.log_header,
                bytes.len(),
                named
            );
            let now = Instant::now();
            for slot in named {
                on_rack_event(&mut self.slots[usize::from(slot) - 1], now);
            }
        }
    }

    /// Lets go of the known cards whose readers confirm them gone; `true` when
    /// that changed the card set.
    async fn check_removals(&mut self, ctx: &EngineContext) -> bool {
        let mut changed = false;
        // Removals: a known card whose slot reads empty leaves only once the
        // reader of that slot confirms it.
        for index in 0..self.slots.len() {
            let slot = (index + 1) as u16;
            if !self.slots[index].class.has_card() {
                // nothing to retry on a slot without a card
                clear_failure(&mut self.slots[index]);
            }
            if self.slots[index].card.is_none() || self.slots[index].class.has_card() {
                continue;
            }
            if access::auth_active(&ctx.port, slot) {
                continue;
            }
            if confirmed_empty(&ctx.profile, &ctx.port, slot, &ctx.log_header).await {
                log::info!(
                    "{} [LOCAL] status=card_removed slot={} card_id={}",
                    ctx.log_header,
                    slot,
                    self.slots[index].card.as_deref().unwrap_or("-")
                );
                forget_card(&mut self.slots[index]);
                if write_led(
                    &ctx.profile,
                    &ctx.port,
                    ctx.profile.led_off.as_deref(),
                    slot,
                    &ctx.log_header,
                )
                .await
                {
                    self.slots[index].led = Some(Led::Off);
                }
                changed = true;
            }
        }
        changed
    }

    /// Identifies up to `IDENTIFY_BATCH` cards, new ones and ones up for a
    /// recheck, whose retry is due by `now`; `true` when that changed the
    /// card set.
    async fn identify_batch(&mut self, ctx: &EngineContext, now: Instant) -> bool {
        let mut changed = false;
        // Identification of cards not known yet, a batch per poll. A card that
        // may have been swapped is identified again the same way once its slot
        // reads as holding a card: the identity of a slot is never assumed, but
        // the card keeps its session until another card is found there.
        let mut identified = 0;
        for index in 0..self.slots.len() {
            if identified >= IDENTIFY_BATCH || ctx.profile.identify.is_empty() {
                break;
            }
            let slot = (index + 1) as u16;
            let state = &self.slots[index];
            let recheck = state.suspect && state.card.is_some();
            if (state.card.is_some() && !recheck)
                || !matches!(state.class, Class::Idle | Class::Error)
                || state.retry_at.is_some_and(|at| at > now)
                || access::auth_active(&ctx.port, slot)
                || blink_state(&ctx.rack_id, slot) == Some(true)
            {
                continue;
            }
            if !rack_port_is_live(&ctx.rack_id, &ctx.port) {
                break;
            }
            identified += 1;
            if recheck {
                log::info!(
                    "{} [LOCAL] status=card_recheck slot={} card_id={}",
                    ctx.log_header,
                    slot,
                    state.card.as_deref().unwrap_or("-")
                );
            }
            match identify(&ctx.profile, &ctx.port, slot, &ctx.log_header).await {
                Ok(id) if self.slots[index].card.as_deref() == Some(id.as_str()) => {
                    // the same card: it was never swapped, its session stays
                    self.slots[index].suspect = false;
                    clear_failure(&mut self.slots[index]);
                    log::info!(
                        "{} [LOCAL] status=card_confirmed slot={} card_id={}",
                        ctx.log_header,
                        slot,
                        id
                    );
                }
                Ok(id) => {
                    log::info!(
                        "{} [LOCAL] status=card_identified slot={} card_id={}",
                        ctx.log_header,
                        slot,
                        id
                    );
                    for other in assign_card(&mut self.slots, index, id) {
                        log::warn!(
                            "{} [LOCAL] status=card_moved from_slot={} to_slot={}",
                            ctx.log_header,
                            other + 1,
                            slot
                        );
                    }
                    changed = true;
                }
                Err(reason) => {
                    let state = &mut self.slots[index];
                    // a slot an authentication owns is not a failure of the card
                    if reason != SERIAL_ERR_CARD_BUSY {
                        state.failures = state.failures.saturating_add(1);
                        if recheck && state.failures >= RECHECK_ATTEMPTS {
                            // whatever is in the slot cannot be told from the
                            // card served there: its session goes
                            log::warn!(
                                "{} [LOCAL] status=card_recheck_failed slot={} card_id={}",
                                ctx.log_header,
                                slot,
                                state.card.as_deref().unwrap_or("-")
                            );
                            state.card = None;
                            state.suspect = false;
                            changed = true;
                        }
                        // a card kept for its recheck is not shown as failed
                        if state.card.is_none() {
                            state.failed = true;
                            state.fail_reason = Some(reason);
                        }
                    }
                    let delay = identify_retry_delay(state.failures);
                    state.retry_at = Some(Instant::now() + delay);
                    if state.card.is_none() {
                        state.led = None;
                    }
                    log::warn!(
                        "{} [LOCAL] status=identify_failed slot={} reason={} retry_secs={}",
                        ctx.log_header,
                        slot,
                        reason,
                        delay.as_secs()
                    );
                }
            }
        }
        changed
    }

    /// Hands the card set over when it `changed` (and on the first pass), then
    /// reports the end of the first identification walk once it is over.
    async fn hand_over_cards(&mut self, ctx: &EngineContext, changed: bool) {
        if changed || self.first_pass {
            apply_cards(&self.slots, &ctx.rack_id, &ctx.port, &ctx.log_header).await;
            self.first_pass = false;
        }
        // after the cards of this pass are handed over, so the rows are on
        // screen by the time the scan indicator goes away
        if !self.scan_reported
            && first_walk_done(
                &self.slots,
                self.polls,
                ctx.profile.stable,
                !ctx.profile.identify.is_empty(),
            )
        {
            crate::global_app_handle::rack_mark_scan_complete(&ctx.rack_id);
            self.scan_reported = true;
        }
    }

    /// Writes the indicators whose slots want another one than they show.
    async fn repaint_indicators(&mut self, ctx: &EngineContext) {
        // Indicators: a served card, a card nobody serves (yet), and a repaint
        // after every authentication, which drops what was written before it.
        let served = served_slots(&ctx.rack_id);
        let mut off_writes = 0;
        for index in 0..self.slots.len() {
            let slot = (index + 1) as u16;
            // A blink owns the indicator while it runs; once it has ended the
            // indicator of the slot is written anew.
            // Checked and removed under one lock: a blink started in between must keep its entry.
            let blinking = {
                let mut blinks = lock(&BLINKS);
                let key = (ctx.rack_id.clone(), slot);
                let running = blinks
                    .get(&key)
                    .map(|control| control.done.borrow().is_none());
                if running == Some(false) {
                    blinks.remove(&key);
                }
                running
            };
            match blinking {
                Some(true) => continue,
                Some(false) => self.slots[index].led = None,
                None => {}
            }
            let state = &self.slots[index];
            let is_served = state
                .card
                .as_ref()
                .is_some_and(|id| served.iter().any(|(s, i)| *s == slot && i == id));
            let Some(wanted) = wanted_led(state, is_served) else {
                continue;
            };
            if access::auth_active(&ctx.port, slot) {
                self.slots[index].was_active = true;
                continue;
            }
            if self.slots[index].was_active {
                self.slots[index].was_active = false;
                self.slots[index].led = None;
            }
            if self.slots[index].led == Some(wanted) {
                continue;
            }
            let template = match wanted {
                Led::Served => ctx.profile.led_served.as_deref(),
                Led::Unserved => ctx.profile.led_unserved.as_deref(),
                Led::Off => ctx.profile.led_off.as_deref(),
            };
            if wanted == Led::Off && template.is_some() {
                // Darkening an empty reader is cosmetic: a few per poll, and
                // one attempt each — a reader that does not take it is not
                // asked again on every poll, each time waiting out its deadline.
                if off_writes >= LED_OFF_BATCH {
                    continue;
                }
                off_writes += 1;
                write_led(&ctx.profile, &ctx.port, template, slot, &ctx.log_header).await;
                self.slots[index].led = Some(Led::Off);
                continue;
            }
            if template.is_none()
                || write_led(&ctx.profile, &ctx.port, template, slot, &ctx.log_header).await
            {
                self.slots[index].led = Some(wanted);
            }
        }
    }
}
