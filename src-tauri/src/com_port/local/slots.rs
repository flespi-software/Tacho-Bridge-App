//! What the engine knows about each slot, and the rules that update it from
//! status readings, rack events and identification results. Pure state with
//! no IO, unit tested.

use std::time::{Duration, Instant};

use crate::global_app_handle::{RackSlotError, RackSlotStates, RackSlots};

use super::profile::{Class, Profile};

/// Pause before a failed identification of a slot is tried again, doubled per
/// failure up to the ceiling. Short at first: a freshly inserted card may not
/// be ready yet and read fine a moment later.
const IDENTIFY_RETRY_INITIAL: Duration = Duration::from_secs(2);
const IDENTIFY_RETRY_MAX: Duration = Duration::from_secs(600);

/// Ceiling of the status poll interval while its exchanges keep failing.
const POLL_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Indicator state of a slot, as last written to the rack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Led {
    Served,
    Unserved,
    Off,
}

/// The indicator a slot should show, `None` to leave it as it is: a served
/// card, a card nobody serves (or that could not be identified), and dark on
/// a working reader without a card — an indicator may stay as it was after
/// its card is gone, and across restarts of the app. A card still waiting
/// for its identification keeps what it has.
pub(super) fn wanted_led(state: &SlotState, served: bool) -> Option<Led> {
    match &state.card {
        Some(_) if served => Some(Led::Served),
        Some(_) => Some(Led::Unserved),
        None if state.failed && state.class.has_card() => Some(Led::Unserved),
        None if state.class == Class::Free => Some(Led::Off),
        None => None,
    }
}

/// What the engine knows about one slot.
#[derive(Clone, Debug)]
pub(super) struct SlotState {
    /// Class the slot held for `stable` polls in a row.
    pub(super) class: Class,
    candidate: Class,
    seen: u32,
    /// Id of the identified card of the slot.
    pub(super) card: Option<String>,
    /// The last identification of the card in the slot failed.
    pub(super) failed: bool,
    /// Why it failed, for the slot table of the UI.
    pub(super) fail_reason: Option<&'static str>,
    /// The identified card may have been swapped: the slot read as empty for
    /// a moment, or the rack reported an event on it. The card is identified
    /// again as soon as the slot reads as holding one, and keeps its session
    /// unless that shows another card (or fails `RECHECK_ATTEMPTS` times).
    pub(super) suspect: bool,
    pub(super) failures: u32,
    pub(super) retry_at: Option<Instant>,
    pub(super) led: Option<Led>,
    /// An authentication ran on the slot since its indicator was written.
    pub(super) was_active: bool,
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
            suspect: false,
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
/// Returns every slot whose reading changed class in this poll, with the old
/// and the new class: also a one-poll flicker, which is what a quick card swap
/// looks like.
pub(super) fn observe(
    slots: &mut [SlotState],
    profile: &Profile,
    region: &[u8],
) -> Vec<(usize, Class, Class)> {
    let mut changes = Vec::new();
    for (index, (state, raw)) in slots.iter_mut().zip(region).enumerate() {
        let class = profile.classify(*raw);
        if class == state.candidate {
            state.seen = state.seen.saturating_add(1);
        } else {
            changes.push((index, state.candidate, class));
            state.candidate = class;
            state.seen = 1;
        }
        if state.seen >= profile.stable {
            state.class = state.candidate;
        }
    }
    changes
}

/// A change in the reading of a slot: a known card that read as gone, even
/// for one poll, may have been swapped and is identified again; a card that
/// just came into an empty slot is identified at once, whatever failed
/// before. A card that only changes its state (idle, busy, error) keeps the
/// retry schedule of its failed identifications.
pub(super) fn on_reading_change(state: &mut SlotState, from: Class, to: Class) {
    if state.card.is_some() {
        if !to.has_card() {
            state.suspect = true;
        }
    } else if to.has_card() && !from.has_card() {
        clear_failure(state);
    }
}

/// Forgets the failed identifications of a slot and their retry schedule.
pub(super) fn clear_failure(state: &mut SlotState) {
    state.failed = false;
    state.fail_reason = None;
    state.failures = 0;
    state.retry_at = None;
}

/// Unsolicited input that names a slot: its card may have changed.
/// On a slot whose card failed its identification the event brings the next
/// attempt closer, but never closer than the first pause after a failure, and
/// the failure count stays: the "event" may be the late reply of the very
/// identification that failed, and must not turn the backoff into a retry on
/// every poll.
pub(super) fn on_rack_event(state: &mut SlotState, now: Instant) {
    if state.card.is_some() {
        state.suspect = true;
    } else if let Some(at) = state.retry_at {
        state.retry_at = Some(at.min(now + IDENTIFY_RETRY_INITIAL));
    }
}

/// Records the card identified in slot `index`. A card has one place only: a
/// slot still listing the same id lost it to a move the polls did not see,
/// and is cleared so its real card is identified. Returns those slots.
pub(super) fn assign_card(slots: &mut [SlotState], index: usize, id: String) -> Vec<usize> {
    let mut moved = Vec::new();
    for (other, state) in slots.iter_mut().enumerate() {
        if other != index && state.card.as_deref() == Some(id.as_str()) {
            forget_card(state);
            moved.push(other);
        }
    }
    let state = &mut slots[index];
    forget_card(state);
    state.card = Some(id);
    moved
}

/// Drops the identity of the card of a slot, to be identified anew.
pub(super) fn forget_card(state: &mut SlotState) {
    state.card = None;
    state.suspect = false;
    state.led = None;
    clear_failure(state);
}

/// The slot table of the UI. The end of the reader chain is the highest
/// address that holds anything; a run of `gap` silent addresses aligned to
/// `gap` below it is a hole in the addressing, not a row of faulty readers.
pub(super) fn slot_table(slots: &[SlotState], gap: usize) -> RackSlots {
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
            slots.get(start..start + gap).is_some_and(|run| {
                run.iter()
                    .all(|s| s.class == Class::None && s.card.is_none())
            })
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
            errors.push(RackSlotError {
                slot,
                reason: reason.to_string(),
            });
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

/// Whether the first identification walk of the rack is over: the status was
/// read often enough for every slot to hold a settled class, and every card
/// that can be identified either was or failed once (its row then shows the
/// error). Until then the UI keeps showing the scan, so it never stops with
/// the cards the user waits for still missing.
pub(super) fn first_walk_done(
    slots: &[SlotState],
    polls: u32,
    stable: u32,
    identifies: bool,
) -> bool {
    polls >= stable.max(1)
        && (!identifies
            || slots.iter().all(|s| {
                s.card.is_some() || s.failed || !matches!(s.class, Class::Idle | Class::Error)
            }))
}

/// The pause before the next status poll: the profile interval, doubled per
/// consecutive failure up to `POLL_BACKOFF_MAX` (never below the interval).
pub(super) fn poll_delay(interval: Duration, failures: u32) -> Duration {
    crate::com_port::backoff_delay(interval, failures, POLL_BACKOFF_MAX)
}

/// 2, 4, 8 … seconds after the 1st, 2nd, 3rd … failure, up to the ceiling.
pub(super) fn identify_retry_delay(failures: u32) -> Duration {
    IDENTIFY_RETRY_INITIAL
        .saturating_mul(1u32 << failures.saturating_sub(1).min(10))
        .min(IDENTIFY_RETRY_MAX)
}

#[cfg(test)]
mod tests {
    use super::super::test_profile::{profile, FREE, IDLE, NONE};
    use super::*;

    #[test]
    fn class_is_taken_after_it_held_for_the_stable_number_of_polls() {
        let p = profile();
        let mut slots = vec![SlotState::default(); 12];
        let mut reading = [NONE; 12];
        reading[..2].copy_from_slice(&[IDLE, FREE]);
        let mut emptied = reading;
        emptied[0] = FREE;
        observe(&mut slots, &p, &reading);
        assert_eq!(slots[0].class, Class::None);
        observe(&mut slots, &p, &reading);
        assert_eq!(slots[0].class, Class::Idle);
        assert_eq!(slots[1].class, Class::Free);
        // one poll showing the slot empty changes nothing
        observe(&mut slots, &p, &emptied);
        assert_eq!(slots[0].class, Class::Idle);
        observe(&mut slots, &p, &reading);
        observe(&mut slots, &p, &emptied);
        assert_eq!(slots[0].class, Class::Idle);
        observe(&mut slots, &p, &emptied);
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
        let reasons: Vec<_> = table
            .errors
            .iter()
            .map(|e| (e.slot, e.reason.as_str()))
            .collect();
        assert_eq!(reasons, vec![(9, "rack_error"), (10, "identify_failed")]);
        assert_eq!((table.total, table.occupied, table.free), (6, 4, 2));
        assert_eq!(slot_table(&vec![SlotState::default(); 12], 4).hi, 0);
    }

    #[test]
    fn a_card_found_in_another_slot_frees_the_slot_that_still_listed_it() {
        let mut slots = vec![SlotState::default(); 6];
        slots[1].card = Some("A".to_string());
        slots[3].card = Some("B".to_string());
        // card A was moved from slot 2 to slot 1 without the polls seeing it
        assert_eq!(assign_card(&mut slots, 0, "A".to_string()), vec![1]);
        assert_eq!(slots[0].card.as_deref(), Some("A"));
        assert_eq!(slots[1].card, None);
        assert_eq!(slots[3].card.as_deref(), Some("B"));
        assert!(assign_card(&mut slots, 5, "C".to_string()).is_empty());
    }

    #[test]
    fn a_flicker_or_a_rack_event_puts_the_card_up_for_a_recheck() {
        let mut state = SlotState {
            card: Some("A".to_string()),
            ..SlotState::default()
        };
        on_reading_change(&mut state, Class::Idle, Class::Busy);
        assert!(!state.suspect);
        on_reading_change(&mut state, Class::Idle, Class::Free);
        assert!(state.suspect);
        // the card itself stays: it keeps its session until its recheck
        assert_eq!(state.card.as_deref(), Some("A"));
        let mut state = SlotState {
            card: Some("A".to_string()),
            ..SlotState::default()
        };
        on_rack_event(&mut state, Instant::now());
        assert!(state.suspect);
        // a new card in an empty slot is identified at once, past failures aside
        let failed = || SlotState {
            failed: true,
            failures: 3,
            retry_at: Some(Instant::now() + Duration::from_secs(60)),
            ..SlotState::default()
        };
        let mut state = failed();
        on_reading_change(&mut state, Class::Free, Class::Idle);
        assert!(!state.failed && state.failures == 0 && state.retry_at.is_none());
        // a failed card that only changes its state keeps its backoff
        let mut state = failed();
        on_reading_change(&mut state, Class::Idle, Class::Error);
        assert!(state.failed && state.failures == 3 && state.retry_at.is_some());
    }

    #[test]
    fn a_rack_event_never_brings_a_failed_identification_closer_than_the_first_pause() {
        let now = Instant::now();
        let mut state = SlotState {
            failed: true,
            failures: 5,
            retry_at: Some(now + Duration::from_secs(60)),
            ..SlotState::default()
        };
        on_rack_event(&mut state, now);
        assert_eq!(state.retry_at, Some(now + IDENTIFY_RETRY_INITIAL));
        assert_eq!(state.failures, 5);
        // a retry already due sooner is left alone
        let soon = now + Duration::from_millis(500);
        state.retry_at = Some(soon);
        on_rack_event(&mut state, now);
        assert_eq!(state.retry_at, Some(soon));
    }

    #[test]
    fn an_empty_working_reader_is_kept_dark() {
        let mut state = SlotState {
            class: Class::Free,
            ..SlotState::default()
        };
        assert_eq!(wanted_led(&state, false), Some(Led::Off));
        // a failed card that was pulled out: its warning goes dark too
        state.failed = true;
        assert_eq!(wanted_led(&state, false), Some(Led::Off));
        state.class = Class::Idle;
        assert_eq!(wanted_led(&state, false), Some(Led::Unserved));
        // a card waiting for its identification keeps what it has
        state.failed = false;
        assert_eq!(wanted_led(&state, false), None);
        state.card = Some("A".to_string());
        assert_eq!(wanted_led(&state, true), Some(Led::Served));
        assert_eq!(wanted_led(&state, false), Some(Led::Unserved));
        // no reader at the address: nothing to write to
        let none = SlotState::default();
        assert_eq!(wanted_led(&none, false), None);
    }

    #[test]
    fn first_walk_waits_for_settled_classes_and_every_card() {
        let mut slots = vec![SlotState::default(); 4];
        // nothing read yet: classes are not settled
        assert!(!first_walk_done(&slots, 1, 2, true));
        assert!(first_walk_done(&slots, 2, 2, true));
        slots[0].class = Class::Idle;
        slots[1].class = Class::Error;
        slots[2].class = Class::Busy;
        slots[3].class = Class::Free;
        // two cards not identified yet; a busy card is not waited for
        assert!(!first_walk_done(&slots, 3, 2, true));
        slots[0].card = Some("AA".to_string());
        assert!(!first_walk_done(&slots, 3, 2, true));
        // a failed identification ends the wait too: its row shows the error
        slots[1].failed = true;
        assert!(first_walk_done(&slots, 3, 2, true));
        // a profile without identification has nothing to wait for
        slots[1].failed = false;
        assert!(first_walk_done(&slots, 3, 2, false));
    }

    #[test]
    fn delays_grow_with_failures_up_to_their_ceilings() {
        let interval = Duration::from_secs(2);
        assert_eq!(poll_delay(interval, 0), interval);
        assert_eq!(poll_delay(interval, 2), Duration::from_secs(8));
        assert_eq!(poll_delay(interval, 30), POLL_BACKOFF_MAX);
        assert_eq!(identify_retry_delay(0), IDENTIFY_RETRY_INITIAL);
        assert_eq!(identify_retry_delay(1), Duration::from_secs(2));
        assert_eq!(identify_retry_delay(3), Duration::from_secs(8));
        assert_eq!(identify_retry_delay(30), IDENTIFY_RETRY_MAX);
    }
}
