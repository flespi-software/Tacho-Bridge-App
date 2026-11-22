//! One sweep over the reader states: every reader event is reconciled with
//! the card sessions in `TASK_POOL` — an inserted card is registered (ICCID,
//! T protocol, MQTT session), a removed or swapped one has its session ended.
//!
//! Sweeps run on the monitor thread and from `manual_sync_cards`, serialized
//! by `READER_SWEEP`. The probe bookkeeping kept here stops a card that is
//! left unserved from being reopened on every event of every reader, and
//! retries the probes that failed or hit a stuck reader.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CStr, CString};
use std::time::Duration;

use log::{debug, error, info, trace, warn};
use pcsc::{Protocols, ReaderState, State as PcscState, PNP_NOTIFICATION};

use crate::global_app_handle::card_emit_event;
use crate::mqtt::{card_io, ensure_connection};

use super::card::ManagedCard;
use super::monitor::request_rescan;
use super::protocol::{parse_atr_and_get_protocol, resolve_t_protocol, AtrProtocol};
use super::reader_io::reader_io_in_flight;
use super::{lock_map, ProcessingCard, TASK_POOL};

lazy_static::lazy_static! {
    /// Serializes whole reader-sweep passes. Two callers run `process_reader_states`
    /// against the same physical readers: the `sc_monitor` thread and the
    /// `manual_sync_cards` command. Their per-card work is NOT protected by the
    /// TASK_POOL lock alone — `reconcile_reader_event` releases the pool before
    /// the `await` points that follow (get_iccid, switch_protocol), and the
    /// successor entry is only pushed at the very end of `ensure_connection`. So
    /// both sweeps can see "no entry", open Shared-mode PC/SC handles on the same
    /// card, interleave SELECT/READ APDUs, and — when the config prescribes a
    /// protocol switch — have the loser call `switch_protocol` (Disposition::ResetCard)
    /// right after the winner registered its session, wiping the card's security
    /// state in the middle of a live VU authentication.
    static ref READER_SWEEP: tokio::sync::Mutex<()> = tokio::sync::Mutex::new(());
}

/// Readers whose card the last probe left unserved (not linked in the config,
/// unreadable ICCID, no usable server host), with the ATR and PC/SC event
/// count it was probed at. Every sweep walks ALL readers, and a sweep runs on
/// an event from ANY reader — including the INUSE flips the probe's own
/// connect/disconnect causes — so without this an unserved card was reopened
/// and re-read on every event of every reader. The entry is dropped (and the
/// card probed again) when the reader reports another ATR or event count
/// (re-insertion), the card is removed, or a new pass / manual sync starts:
/// linking a card in the UI saves the config and calls `request_rescan`.
static UNSERVED_PROBES: std::sync::Mutex<BTreeMap<String, (String, u32)>> =
    std::sync::Mutex::new(BTreeMap::new());

/// True when the card in the reader is the one a previous probe already found
/// unserved, unchanged since (same ATR, no insertion/removal in between).
fn probed_unserved(memo: Option<&(String, u32)>, atr: &str, event_count: u32) -> bool {
    memo.is_some_and(|(probed_atr, probed_count)| probed_atr == atr && *probed_count == event_count)
}

/// Drops every unserved-probe memo, so the next sweep probes those cards again.
pub(super) fn forget_unserved_probes() {
    lock_map(&UNSERVED_PROBES).clear();
}

/// Probes of a card that failed outright (the reader refused the connect, the
/// ICCID read failed) since its insertion: reader → (ATR, event count,
/// attempts). Such a failure is often transient — on Windows the smart-card
/// service holds a freshly inserted card for a moment — and PC/SC reports no
/// further event for the card once it is free, so a few delayed rescans are
/// what serves it without a replug. Kept apart from `UNSERVED_PROBES`, which
/// every pass clears: the attempts must survive the rescans they trigger.
static FAILED_PROBES: std::sync::Mutex<BTreeMap<String, (String, u32, u32)>> =
    std::sync::Mutex::new(BTreeMap::new());

/// Rescans after a failed probe of the same card, at 2 s, 6 s and 18 s: a
/// card that keeps failing (a non-tachograph card) is left alone after them.
const FAILED_PROBE_RETRIES: u32 = 3;
const FAILED_PROBE_RETRY_BASE: Duration = Duration::from_secs(2);

/// Counts a failed probe of the card and returns the pause before the rescan
/// that retries it, `None` once the retries are used up.
fn next_failed_probe_retry(reader: &str, atr: &str, event_count: u32) -> Option<Duration> {
    let mut failed = lock_map(&FAILED_PROBES);
    let attempts = match failed.get(reader) {
        Some((probed_atr, probed_count, attempts))
            if probed_atr == atr && *probed_count == event_count =>
        {
            attempts + 1
        }
        _ => 1,
    };
    failed.insert(reader.to_owned(), (atr.to_owned(), event_count, attempts));
    (attempts <= FAILED_PROBE_RETRIES).then(|| FAILED_PROBE_RETRY_BASE * 3u32.pow(attempts - 1))
}

/// Records the outcome of a probe: a card left without a live session is not
/// probed again until its reader changes (see `UNSERVED_PROBES`). A probe that
/// ran out of time is not recorded — its reader is retried once it recovers.
/// A probe that `failed` outright is retried by a few delayed rescans (see
/// `FAILED_PROBES`).
async fn note_probe_outcome(
    reader_name: &CStr,
    reader: &str,
    atr: &str,
    event_count: u32,
    failed: bool,
) {
    let served = TASK_POOL.lock().await.iter().any(|c| {
        c.reader_name.as_deref() == Some(reader)
            && c.atr.as_deref() == Some(atr)
            && !c.task_handle.inner().is_finished()
    });
    let stuck = !served && reader_io_in_flight(reader_name);
    {
        let mut memo = lock_map(&UNSERVED_PROBES);
        if served || stuck {
            memo.remove(reader);
        } else {
            memo.insert(reader.to_owned(), (atr.to_owned(), event_count));
        }
    }
    if stuck {
        rescan_when_reader_idle(reader_name);
        return;
    }
    if !failed {
        lock_map(&FAILED_PROBES).remove(reader);
        return;
    }
    if let Some(pause) = next_failed_probe_retry(reader, atr, event_count) {
        info!(
            "Reader {:?}: card probe failed, rescanning in {}s",
            reader_name,
            pause.as_secs()
        );
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(pause).await;
            request_rescan();
        });
    }
}

/// Readers with a `rescan_when_reader_idle` watcher already running.
static IDLE_RESCAN_PENDING: std::sync::Mutex<BTreeSet<CString>> =
    std::sync::Mutex::new(BTreeSet::new());

/// Poll step of the `rescan_when_reader_idle` watcher.
const IDLE_RESCAN_POLL: Duration = Duration::from_secs(1);

/// Requests a rescan once the reader's stuck PC/SC call returns. A probe
/// skipped (or timed out) because of that call is otherwise retried only on
/// the reader's next PC/SC event, which may never come: the card sits there
/// unserved although the reader recovered. One watcher per reader.
fn rescan_when_reader_idle(reader_name: &CStr) {
    let reader = reader_name.to_owned();
    if !lock_map(&IDLE_RESCAN_PENDING).insert(reader.clone()) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        while reader_io_in_flight(&reader) {
            tokio::time::sleep(IDLE_RESCAN_POLL).await;
        }
        lock_map(&IDLE_RESCAN_PENDING).remove(&reader);
        info!(
            "Reader {:?}: the stuck card operation returned, rescanning",
            reader
        );
        request_rescan();
    });
}

/// PC/SC readers that currently have a row in the UI (the last state the
/// sweep emitted for them was not a removal). Same rule as the replay map of
/// `card_emit_event` (`is_reader_gone_state`), so `evict_vanished_readers`
/// can retire the rows of readers that disappeared while no context watched
/// them — not only the ones that had a card session.
static REPORTED_READERS: std::sync::Mutex<BTreeSet<String>> =
    std::sync::Mutex::new(BTreeSet::new());

/// Tracks whether the reader keeps a row in the UI after `card_state` was
/// emitted for it (see `REPORTED_READERS`).
fn note_reported_reader(reader: &str, card_state: &str) {
    let mut reported = lock_map(&REPORTED_READERS);
    if crate::global_app_handle::is_reader_gone_state(card_state) {
        reported.remove(reader);
    } else {
        reported.insert(reader.to_owned());
    }
}

/// Check if the reader is a virtual reader. This usually only applies to Windows.
fn is_virtual_reader(reader_name: &CStr) -> bool {
    // Convert the reader name to a lowercase string
    let reader_name_lower = reader_name.to_string_lossy().to_lowercase();

    // Check if the name contains keywords indicating a virtual reader
    reader_name_lower.contains("microsoft")
        || reader_name_lower.contains("virtual")
        || reader_name_lower.contains("remote")
}

/// Reconciles every reader state with the card sessions: registers inserted
/// cards, ends the sessions of removed ones, and emits each reader's state to
/// the frontend.
///
/// Every failure here is per-card and non-fatal: it is logged and the loop
/// moves on to the next reader, so there is nothing to propagate to the caller.
pub(super) async fn process_reader_states(reader_states: &mut [ReaderState]) {
    let _sweep = READER_SWEEP.lock().await;
    for rs in reader_states {
        if rs.name() == PNP_NOTIFICATION() {
            continue;
        }

        if is_virtual_reader(rs.name()) {
            warn!("Virtual reader {:?} detected. Skipping...", rs.name());
            continue;
        }

        let reader_name = rs.name();
        let Ok(reader_name_string) = reader_name.to_str() else {
            warn!(
                "Reader name is not valid UTF-8: {:?}. Skipping...",
                reader_name
            );
            continue;
        };

        // A reader that is gone (unplugged → UNKNOWN/IGNORE) keeps the last
        // ATR in its reused ReaderState, and `setup_reader_states` drops it on
        // the next pass: report it as emptied now, or its card session would
        // be reconciled as unchanged and stay "online" for good.
        let gone = rs
            .event_state()
            .intersects(PcscState::UNKNOWN | PcscState::IGNORE | PcscState::EMPTY);
        let atr = if gone {
            String::new()
        } else {
            hex::encode(rs.atr())
        };
        let atr_protocol = parse_atr_and_get_protocol(&atr);
        let card_state_string = format!("{:?}", rs.event_state());
        debug!("card_state_string {}", card_state_string);

        let action = reconcile_reader_event(reader_name_string, &atr).await;
        let registered = match action {
            CardProcessingResult::Create => {
                let event_count = rs.event_count();
                let unchanged = probed_unserved(
                    lock_map(&UNSERVED_PROBES).get(reader_name_string),
                    &atr,
                    event_count,
                );
                if unchanged {
                    // Already probed and reported; nothing changed in this
                    // reader since, so there is nothing new to emit either.
                    trace!(
                        "Reader {:?}: card already probed and unserved, skipping",
                        reader_name
                    );
                    continue;
                }
                if reader_io_in_flight(reader_name) {
                    // A probe would queue behind (and park another thread
                    // next to) a PC/SC call that already outlived its caller,
                    // holding READER_SWEEP for up to two card_io timeouts.
                    // Not recorded as unserved: retried by the next sweep,
                    // or by a rescan once the call returns.
                    warn!(
                        "Reader {:?}: an earlier card operation is still stuck in PC/SC, not probing the card",
                        reader_name
                    );
                    rescan_when_reader_idle(reader_name);
                    None
                } else {
                    let registered = register_card(reader_name, &atr, &atr_protocol).await;
                    note_probe_outcome(
                        reader_name,
                        reader_name_string,
                        &atr,
                        event_count,
                        registered.is_none(),
                    )
                    .await;
                    registered
                }
            }
            CardProcessingResult::Delete => {
                debug!("CARD DELETED {}", card_state_string);
                lock_map(&UNSERVED_PROBES).remove(reader_name_string);
                lock_map(&FAILED_PROBES).remove(reader_name_string);
                None
            }
            CardProcessingResult::Ignore => None,
        };
        // Failed registration and removal still emit the reader state with
        // empty card identifiers, so the frontend sees every non-ignored event.
        let RegisteredCard {
            iccid,
            card_number,
            protocol: effective_protocol,
        } = registered.unwrap_or_else(|| RegisteredCard {
            iccid: String::new(),
            card_number: String::new(),
            protocol: atr_protocol.protocol(),
        });

        if action != CardProcessingResult::Ignore {
            note_reported_reader(reader_name_string, &card_state_string);
            card_emit_event(
                "global-cards-sync",
                iccid,
                reader_name_string.into(),
                card_state_string,
                card_number.clone(),
                None,
                None,
            );

            info!(
                "{:?} {:?} {:?}, {:?}, Protocol: {:?}",
                rs.name(),
                rs.event_state(),
                atr,
                card_number,
                effective_protocol
            );
        }
    }
}

/// What `register_card` found out about a card it registered.
struct RegisteredCard {
    iccid: String,
    card_number: String,
    protocol: Protocols,
}

/// Opens the card in the reader, reads its ICCID, applies its T protocol and
/// hands it to its MQTT session. `None` when the card is left unserved.
///
/// Called under READER_SWEEP: keep ICCID reads, protocol switching and pool
/// registration serialized with every other reader sweep.
async fn register_card(
    reader_name: &CStr,
    atr: &str,
    atr_protocol: &AtrProtocol,
) -> Option<RegisteredCard> {
    let mut managed_card = match ManagedCard::new(reader_name, atr_protocol.protocol()) {
        Ok(card) => card,
        Err(e) => {
            error!(
                "Failed to create ManagedCard for reader {:?}: {}",
                reader_name, e
            );
            return None;
        }
    };
    // Card I/O is bounded: this runs under READER_SWEEP, and one wedged
    // reader would otherwise stop card detection on every reader.
    let header = format!("Reader {:?} |", reader_name);
    let iccid = match card_io(managed_card.get_iccid(), &header, "get_iccid").await {
        Some(Ok(iccid)) => iccid,
        Some(Err(e)) => {
            error!("Failed to get ICCID for reader {:?}: {}", reader_name, e);
            // An unlinked or unreadable card: leave it as it is, it may be in
            // use by another application.
            managed_card.release();
            return None;
        }
        None => {
            // The read is still stuck in PC/SC holding the handle: `release`
            // cannot close it now, but marks it to be closed with LeaveCard
            // on the blocking pool once the read returns. A plain drop would
            // reset the card — possibly another application's.
            managed_card.release();
            return None;
        }
    };
    info!("ICCID: {}", iccid);
    // The same deterministic lookup the rack path uses (duplicate ICCID
    // links resolve to the same entry on every run and are warned about).
    let card_number = crate::config::find_card_number_by_iccid(&iccid).unwrap_or_default();

    // The config entry is known only after reading the ICCID, so open with
    // the ATR-derived protocol first and then apply the configured protocol.
    // Off the async worker: storing a first-seen protocol writes the config.
    let requested_protocol = {
        let (card_number, atr_protocol) = (card_number.clone(), atr_protocol.clone());
        let fallback = atr_protocol.protocol();
        tauri::async_runtime::spawn_blocking(move || {
            resolve_t_protocol(&card_number, &atr_protocol)
        })
        .await
        .unwrap_or(fallback)
    };
    card_io(
        managed_card.switch_protocol(requested_protocol),
        &header,
        "switch_protocol",
    )
    .await?;
    // A failed switch keeps the old protocol; report the actual stored value.
    let protocol = managed_card.protocol;
    ensure_connection(
        reader_name,
        card_number.clone(),
        atr.to_owned(),
        managed_card,
    )
    .await;

    Some(RegisteredCard {
        iccid,
        card_number,
        protocol,
    })
}

/// What a reader event means for the card sessions (see `reconcile_reader_event`).
#[derive(Debug, PartialEq, Eq)]
pub enum CardProcessingResult {
    Create,
    Delete,
    Ignore,
}

/// Determines what action should be taken for a card with the given reader name and ATR.
/// Removes stale entries for removed or replaced cards and schedules their
/// connections for shutdown.
pub async fn reconcile_reader_event(reader_name: &str, atr: &str) -> CardProcessingResult {
    let mut pool = TASK_POOL.lock().await;

    debug!(
        "reconcile_reader_event: reader='{}' atr_len={} pool_size={}",
        reader_name,
        atr.len(),
        pool.len()
    );

    // Case 1: Both reader_name and atr are provided and not found in the pool → register new card
    if !reader_name.is_empty() && !atr.is_empty() {
        // A finished task (returned early or panicked) serves nobody: treat its
        // entry as absent, or the card stays unserved until a replug.
        // `ensure_connection` replaces it.
        let exists = pool.iter().any(|c| {
            c.reader_name.as_deref() == Some(reader_name)
                && c.atr.as_deref() == Some(atr)
                && !c.task_handle.inner().is_finished()
        });

        if !exists {
            // A reader physically holds one card at a time, so any entry bound
            // to this reader with a DIFFERENT non-empty ATR describes a card
            // that is already gone. Evict it here, because the empty-ATR pass
            // below (Case 2, the only other eviction path) never runs for it:
            // when a card is swapped faster than the monitor polls, PC/SC
            // coalesces the removal and the insertion into a single PRESENT
            // event carrying the new ATR, so no empty-ATR event is ever
            // observed. Leaving the entry behind keeps the removed card shown
            // as online with a live MQTT session retrying a dead PC/SC handle,
            // and lets a later empty-ATR event evict whichever same-reader
            // entry `position()` happens to find first — possibly orphaning the
            // new card's session instead.
            let stale: Vec<usize> = pool
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    c.reader_name.as_deref() == Some(reader_name)
                        && c.atr.as_deref().is_some_and(|a| !a.is_empty() && a != atr)
                })
                .map(|(index, _)| index)
                .collect();
            for index in stale.into_iter().rev() {
                let removed = pool.remove(index);
                warn!(
                    "Evicting ProcessingCard for reader {} — card swapped (old ATR {}, new ATR {})",
                    removed.reader_name.as_deref().unwrap_or("unknown"),
                    removed.atr.as_deref().unwrap_or("unknown"),
                    atr,
                );
                tauri::async_runtime::spawn(crate::mqtt::shutdown_connections(
                    vec![removed],
                    "card_swapped",
                ));
            }
            return CardProcessingResult::Create;
        }
    }

    // Case 2: ATR is empty, but a card with the same reader name and filled ATR exists → remove it
    if atr.is_empty() {
        let to_remove = pool.iter().position(|c| {
            c.reader_name.as_deref() == Some(reader_name)
                && c.atr.as_ref().map(|s| !s.is_empty()).unwrap_or(false)
        });
        if let Some(index) = to_remove {
            let removed = pool.remove(index);
            warn!(
                "Removed stale ProcessingCard for reader {} with old ATR {}",
                removed.reader_name.as_deref().unwrap_or("unknown"),
                removed.atr.as_deref().unwrap_or("unknown"),
            );
            // Close the connection gracefully (clean MQTT DISCONNECT instead of a
            // dropped socket); detached so the reader monitor is not delayed.
            tauri::async_runtime::spawn(crate::mqtt::shutdown_connections(
                vec![removed],
                "card_removed",
            ));
        }

        return CardProcessingResult::Delete;
    }

    // No action needed
    CardProcessingResult::Ignore
}

/// Ends the sessions of cards whose reader is no longer listed. A fresh pass
/// starts from UNAWARE with only the readers present now, so a reader that was
/// unplugged (or re-enumerated under another name) while no context was alive
/// — a failed pass's backoff, a pcscd restart — never reports a removal: its
/// card would stay "online" with a dead handle for good, and keep the rack
/// from serving the same card number.
///
/// The same applies to readers without a session (empty, or holding an
/// unlinked card): their UI row is retired too, or it stays for good.
pub(super) async fn evict_vanished_readers(present: &[String]) {
    let _sweep = READER_SWEEP.lock().await;
    let stale: Vec<ProcessingCard> = {
        let mut pool = TASK_POOL.lock().await;
        let (stale, keep) = std::mem::take(&mut *pool).into_iter().partition(|c| {
            c.reader_name
                .as_ref()
                .is_some_and(|name| !present.contains(name))
        });
        *pool = keep;
        stale
    };
    // Every vanished reader gets exactly one UNKNOWN row update: the ones
    // with a session, and the ones the UI merely shows.
    let mut vanished: BTreeSet<String> = {
        let mut reported = lock_map(&REPORTED_READERS);
        let gone: BTreeSet<String> = reported
            .iter()
            .filter(|name| !present.contains(name))
            .cloned()
            .collect();
        reported.retain(|name| !gone.contains(name));
        gone
    };
    for card in &stale {
        let reader = card.reader_name.clone().unwrap_or_default();
        warn!(
            "Reader {} is gone: ending the session of card {}",
            reader, card.client_id
        );
        vanished.insert(reader);
    }
    for reader in vanished {
        info!(
            "Reader {} is gone: removing it from the reader list",
            reader
        );
        lock_map(&UNSERVED_PROBES).remove(&reader);
        card_emit_event(
            "global-cards-sync",
            String::new(),
            reader,
            format!("{:?}", PcscState::UNKNOWN),
            String::new(),
            None,
            None,
        );
    }
    if !stale.is_empty() {
        tauri::async_runtime::spawn(crate::mqtt::shutdown_connections(stale, "reader_gone"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_probe_is_retried_a_few_times_per_insertion() {
        let reader = "TEST READER failed-probe";
        let delays: Vec<_> = (0..4)
            .map(|_| next_failed_probe_retry(reader, "3B00", 7))
            .collect();
        assert_eq!(
            delays,
            vec![
                Some(Duration::from_secs(2)),
                Some(Duration::from_secs(6)),
                Some(Duration::from_secs(18)),
                None
            ]
        );
        // a new insertion starts over
        assert_eq!(
            next_failed_probe_retry(reader, "3B00", 8),
            Some(Duration::from_secs(2))
        );
        lock_map(&FAILED_PROBES).remove(reader);
    }

    #[test]
    fn unserved_probe_is_skipped_only_while_the_reader_is_unchanged() {
        let memo = ("3B8131AABB".to_string(), 4);
        assert!(probed_unserved(Some(&memo), "3B8131AABB", 4));
        // Re-inserted (event count moved) or another card (ATR differs).
        assert!(!probed_unserved(Some(&memo), "3B8131AABB", 5));
        assert!(!probed_unserved(Some(&memo), "3B6000FF", 4));
        // Never probed.
        assert!(!probed_unserved(None, "3B8131AABB", 4));
    }

    #[test]
    fn reported_readers_follow_the_ui_row_rule() {
        let reader = "Test Reader reported 00";
        note_reported_reader(reader, "State(EMPTY | CHANGED)");
        assert!(lock_map(&REPORTED_READERS).contains(reader));
        note_reported_reader(reader, &format!("{:?}", PcscState::UNKNOWN));
        assert!(!lock_map(&REPORTED_READERS).contains(reader));
    }
}
