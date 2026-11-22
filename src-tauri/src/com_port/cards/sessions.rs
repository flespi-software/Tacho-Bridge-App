//! The registry of rack card sessions (`RACK_CARD_TASKS`): spawning a session
//! with its deduplication rules, aborting sessions (card moved to a reader,
//! card removed from the config, rack gone) and the inventory the server and
//! the debug log read from it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tauri::async_runtime;

use crate::com_port::state::mutate_rack_rows;
use crate::com_port::transport::SharedPort;
use crate::com_port::{lock, rack_port_is_live};
use crate::global_app_handle::rack_update_cards;
use crate::smart_card::TASK_POOL;

use super::session::{rack_card_mqtt_loop, try_publish_link_report};
use super::{
    bump_card_epoch, card_epoch, RackCardTask, RACK_CARDS_UI, RACK_CARD_EPOCH, RACK_CARD_TASKS,
};

/// Live card identities survive an application MQTT reconnect. The server uses
/// this snapshot to preserve sessions while validating the rack again.
pub(in crate::com_port) fn inventory(rack_id: &str) -> serde_json::Value {
    let tasks = lock(&RACK_CARD_TASKS);
    let mut cards: Vec<_> = tasks
        .iter()
        .filter(|(_, task)| task.rack_id == rack_id && !task.handle.inner().is_finished())
        .map(|(iccid, task)| serde_json::json!({"slot": task.slot, "iccid": iccid}))
        .collect();
    cards.sort_by_key(|card| card["slot"].as_u64());
    serde_json::Value::Array(cards)
}

/// Writes every rack card session and UI row to the log — part of the state
/// snapshot the extended debug log starts with.
pub(in crate::com_port) fn log_sessions_snapshot() {
    let sessions: Vec<String> = {
        let tasks = lock(&RACK_CARD_TASKS);
        let mut rows: Vec<_> = tasks
            .iter()
            .map(|(iccid, task)| {
                format!(
                    "{}/{}:{}:{}:{}",
                    task.rack_id,
                    task.slot,
                    task.card_number,
                    iccid,
                    if task.handle.inner().is_finished() {
                        "finished"
                    } else {
                        "live"
                    }
                )
            })
            .collect();
        rows.sort();
        rows
    };
    log::info!(
        "[DEBUG] snapshot=card_sessions count={} sessions={}",
        sessions.len(),
        sessions.join(",")
    );
    let rows: Vec<String> = {
        let ui = lock(&RACK_CARDS_UI);
        let mut rows: Vec<_> = ui
            .iter()
            .flat_map(|(rack_id, list)| {
                list.iter().map(move |card| {
                    format!(
                        "{}/{}:{}:online={:?}:auth={:?}",
                        rack_id,
                        card.slot,
                        card.iccid.as_deref().unwrap_or("-"),
                        card.online,
                        card.authentication
                    )
                })
            })
            .collect();
        rows.sort();
        rows
    };
    log::info!(
        "[DEBUG] snapshot=ui_rows count={} rows={}",
        rows.len(),
        rows.join(",")
    );
}

/// What drives a rack card spawn.
#[derive(Clone, Copy)]
pub(super) enum SpawnOrigin {
    /// A fresh server card set: the server just read the card's ICCID in that
    /// slot — ground truth, so it may relocate a live session to another
    /// rack/slot. `rebind` tells whether a card already served must re-publish
    /// its rack link report — true only for the card set that follows a
    /// `link up` (see `rack::take_rebind`).
    ///
    /// Every origin carries the card-set epoch of the rack it was decided
    /// under (see `RACK_CARD_EPOCH`).
    ServerSet { rebind: bool, epoch: u64 },
    /// A local replay of cached UI rows (`connect_pending_rack_cards`). A
    /// replayed row can be stale, and rebinding a working session to it would
    /// funnel the card's APDUs into a port where the card no longer sits; with
    /// no server discovery behind it there is nothing to re-bind either.
    Replay { epoch: u64 },
}

/// Starts the rack-backed MQTT session of one card, deduplicating by ICCID and
/// by card number (two slots mapped to the same number in the config must not
/// open two MQTT connections with the same client_id).
fn spawn_rack_card(
    card_number: String,
    iccid: String,
    slot: u16,
    rack_id: String,
    serial_port: SharedPort,
    origin: SpawnOrigin,
) {
    let (from_server, rebind, epoch) = match origin {
        SpawnOrigin::ServerSet { rebind, epoch } => (true, rebind, epoch),
        SpawnOrigin::Replay { epoch } => (false, false, epoch),
    };
    // The rack may have been torn down while the caller was awaiting between
    // its dedup check and this spawn (config-change path racing the monitor).
    // A session installed after the teardown would hold the stale serial port
    // and an MQTT client_id forever — verify the port is still the live one.
    if !rack_port_is_live(&rack_id, &serial_port) {
        log::warn!(
            "RACK | [SPAWN] card={} rack={} slot={} status=skipped reason=rack_gone",
            card_number,
            rack_id,
            slot
        );
        return;
    }

    let mut tasks = lock(&RACK_CARD_TASKS);
    // Checked under the task lock, which every teardown also takes after its
    // bump: a spawn either sees the newer epoch here or lands before the
    // teardown that reaps it.
    if card_epoch(&rack_id) != epoch {
        log::info!(
            "RACK | [SPAWN] card={} rack={} slot={} status=skipped reason=stale_card_set",
            card_number,
            rack_id,
            slot
        );
        return;
    }
    // A different card now occupies this slot of this rack: the previous
    // occupant's session is dead weight (its card is gone) — evict it. Scoped
    // to the rack: slot numbers repeat across racks (every rack has a slot 1).
    let stale: Vec<String> = tasks
        .iter()
        .filter(|(other_iccid, task)| {
            **other_iccid != iccid && task.rack_id == rack_id && task.slot == slot
        })
        .map(|(other_iccid, _)| other_iccid.clone())
        .collect();
    for old_iccid in stale {
        if let Some(old) = tasks.remove(&old_iccid) {
            old.handle.abort();
            log::info!(
                "RACK | [SPAWN] card={} rack={} slot={} status=aborted reason=slot_reassigned",
                old.card_number,
                rack_id,
                slot
            );
        }
    }
    // Same ICCID: a repeat of the same place is a no-op, but the same card
    // reported from another rack or slot means it physically moved while the
    // old session was still alive — e.g. the application connection was down,
    // so the old rack's set did not arrive yet. The newest set is ground truth
    // (the server just read this ICCID in that slot): replace the session so
    // the card is served where it actually is.
    let moved = match tasks.get(&iccid) {
        Some(existing) if !existing.handle.inner().is_finished() => {
            if existing.rack_id == rack_id && existing.slot == slot {
                // The server re-ran discovery for a slot it already served —
                // typically the application connection re-established while
                // this card session stayed up. Such a discovery may paint the
                // slot as unserved; re-publishing the link report
                // over the live session tells the server the card is still
                // connected, which re-binds the slot and repaints it. Only
                // after a `link up`, though: every report costs an extra serial
                // exchange, and a routine card set (one per discovery chain, and
                // a chain runs on every card insertion or removal) would then
                // put one frame per card of the rack on the wire, ahead of the
                // tracker exchanges. Only over a live connection (see
                // `RackCardTask::online`), and with `try_publish`: this path
                // holds the task-map lock, and `publish().await` parks on a
                // full request channel.
                if let Some(client) = existing
                    .client
                    .get()
                    .filter(|_| rebind && existing.online.load(Ordering::SeqCst))
                {
                    let log_header = format!("RACKCARD {} |", existing.card_number);
                    try_publish_link_report(
                        client,
                        (&iccid, slot, &rack_id),
                        &log_header,
                        format_args!("link_report_republished slot={}", slot),
                        "link_report_republish_failed",
                    );
                } else if rebind {
                    log::debug!(
                        "RACKCARD {} | [MQTT] status=link_report_republish_skipped reason=session_offline slot={}",
                        existing.card_number,
                        slot
                    );
                }
                log::debug!(
                    "RACK | [SPAWN] card={} status=skipped reason=already_running",
                    card_number
                );
                return;
            }
            // A live session elsewhere, but this spawn is a local replay of a
            // cached UI row — that row may be the stale duplicate, so it must
            // not displace a working session (see `SpawnOrigin::Replay`).
            if !from_server {
                log::warn!(
                    "RACK | [SPAWN] card={} rack={} slot={} status=skipped reason=live_session_elsewhere",
                    card_number,
                    rack_id,
                    slot
                );
                return;
            }
            true
        }
        _ => false,
    };
    if moved {
        if let Some(old) = tasks.remove(&iccid) {
            old.handle.abort();
            log::info!(
                "RACK | [SPAWN] card={} status=aborted reason=card_moved from_rack={} from_slot={} to_rack={} to_slot={}",
                old.card_number,
                old.rack_id,
                old.slot,
                rack_id,
                slot
            );
        }
    }
    if tasks.iter().any(|(other_iccid, task)| {
        *other_iccid != iccid
            && task.card_number == card_number
            && !task.handle.inner().is_finished()
    }) {
        log::warn!(
            "RACK | [SPAWN] card={} rack={} slot={} status=skipped reason=served_by_another_slot",
            card_number,
            rack_id,
            slot
        );
        return;
    }
    log::info!(
        "RACK | [SPAWN] card={} rack={} slot={} status=starting_session",
        card_number,
        rack_id,
        slot
    );
    let client_slot = Arc::new(OnceLock::new());
    let online = Arc::new(AtomicBool::new(false));
    let handle = async_runtime::spawn(rack_card_mqtt_loop(
        card_number.clone(),
        iccid.clone(),
        slot,
        rack_id.clone(),
        serial_port,
        client_slot.clone(),
        online.clone(),
    ));
    tasks.insert(
        iccid,
        RackCardTask {
            card_number,
            slot,
            rack_id,
            handle,
            client: client_slot,
            online,
        },
    );
}

/// Removes every rack card session matching `pred` and aborts its task.
///
/// The three callers (card moved to a reader, card deleted from the config,
/// rack gone) differ only in which sessions they select and what they log, so
/// the collect-remove-abort sequence lives here once. `reason` fills the reason
/// field of the one log line emitted per aborted session; `warn` picks its
/// level, since a card reappearing in a reader is unexpected enough to warrant
/// WARN while the other two are routine.
///
/// The `RACK_CARD_TASKS` guard is dropped before returning: callers follow up
/// with UI mutations, which take the UI lock, and taking the two in this order
/// is what keeps the rack locks deadlock-free.
fn abort_rack_sessions_where(pred: impl Fn(&RackCardTask) -> bool, reason: &str, warn: bool) {
    let mut tasks = lock(&RACK_CARD_TASKS);
    let matching: Vec<String> = tasks
        .iter()
        .filter(|(_, task)| pred(task))
        .map(|(iccid, _)| iccid.clone())
        .collect();
    for iccid in matching {
        if let Some(task) = tasks.remove(&iccid) {
            task.handle.abort();
            if warn {
                log::warn!(
                    "RACK | [SPAWN] card={} rack={} slot={} status=aborted reason={}",
                    task.card_number,
                    task.rack_id,
                    task.slot,
                    reason
                );
            } else {
                log::info!(
                    "RACK | [SPAWN] card={} rack={} slot={} status=aborted reason={}",
                    task.card_number,
                    task.rack_id,
                    task.slot,
                    reason
                );
            }
        }
    }
}

/// Aborts the live rack-backed session of one card number, if any. Called by
/// the reader path (`mqtt::ensure_connection`) right before it opens its own
/// MQTT connection under that client_id: the card was just physically detected
/// in a PC/SC reader, so it cannot still sit in a rack slot — the rack session
/// is stale (the server's card set dropping it was lost or is still in
/// flight), and
/// letting it live would put two MQTT connections with the same client_id on
/// the broker, which drops them both in a loop until the rack is unplugged.
///
/// The card's rack UI rows are removed as well, exactly as a server card set
/// without the card would do. Keeping a row here would leave a slot the card
/// physically left looking occupied AND make it eligible for
/// `connect_pending_rack_cards` (no live session), which would later resurrect
/// a rack session bound to the empty slot once the reader releases the number.
///
/// Recovery of the row relies on the server, and holds for the normal
/// one-card-per-number world: the card leaving the rack changes the presence
/// watch bytes (or, if the rack link was down, the re-armed watch republishes
/// its first exchange unconditionally), so the server always learns the
/// current rack content and re-publishes the card set when the card is back
/// in a slot. The one case with no recovery signal is a misconfigured setup where
/// two physical cards resolve to the same number: detecting the second card
/// in a reader deletes the first card's row even though its rack content
/// never changed, and that row only comes back on a rack replug. Accepted —
/// the reader detection is strictly newer physical evidence, and a stale row
/// resurrecting a session onto an empty slot is the worse failure.
pub fn abort_rack_card_session(card_number: &str) {
    abort_rack_sessions_where(
        |task| task.card_number == card_number,
        "card_now_in_reader",
        true,
    );
    // Outside the tasks lock (lock-order discipline with the UI paths): drop
    // every rack row carrying this number — including rows whose session was
    // never spawned (skipped as served_by_reader) — and by the card's ICCID
    // from the config, covering a row that is not number-resolved yet.
    let iccid = crate::config::get_card_config_from_cache(card_number).map(|card| card.iccid);
    mutate_rack_rows(|rack_id, list| {
        let before = list.len();
        list.retain(|c| {
            c.card_number.as_deref() != Some(card_number) && (iccid.is_none() || c.iccid != iccid)
        });
        if list.len() == before {
            return false;
        }
        log::info!(
            "RACK {} | [SPAWN] status=row_dropped reason=card_now_in_reader card={}",
            rack_id,
            card_number
        );
        true
    });
}

/// Closes the rack-backed session of one card number, if any rack currently
/// serves it. Called when the card is removed from the config: `TASK_POOL` only
/// holds reader-backed sessions, so without this the rack session would keep
/// running with the removed card's MQTT client_id — leaking the task until the
/// rack is unplugged, and colliding with the server's ident check if the same
/// number is re-added later.
///
/// The card stays physically in its slot, so it is kept in the rack UI with its
/// number cleared: it reappears as an unknown card, ready to be assigned again
/// (which is what `connect_pending_rack_cards` then retries).
pub fn disconnect_rack_card(card_number: &str) {
    // Sessions are keyed by ICCID (stable across config edits), so the lookup
    // is by the number captured at spawn time.
    abort_rack_sessions_where(
        |task| task.card_number == card_number,
        "card_removed_from_config",
        false,
    );

    // Keep the card visible in its rack section, but unassigned — the physical
    // card did not move, only its config entry is gone. The sweep runs even
    // when no session was aborted: a rack row can carry the number without a
    // session (spawn skipped as served_by_reader), and skipping it here would
    // leave the deleted number on display forever.
    mutate_rack_rows(|_rack_id, list| {
        let mut touched = false;
        for card in list.iter_mut() {
            if card.card_number.as_deref() == Some(card_number) {
                card.card_number = None;
                card.name = None;
                // its session is gone: not served, not busy
                card.online = None;
                card.authentication = None;
                touched = true;
            }
        }
        touched
    });
}

/// Aborts the card sessions of one rack and clears its UI card list. Called
/// when that rack disconnects or its MQTT/serial stack is restarted — without
/// the rack there is no transport to those cards.
pub(in crate::com_port) fn stop_rack_cards(rack_id: &str) {
    // before the abort: a card set still running for this rack stops spawning
    bump_card_epoch(rack_id);
    abort_rack_sessions_where(|task| task.rack_id == rack_id, "rack_gone", false);
    let cleared = {
        let mut ui = lock(&RACK_CARDS_UI);
        ui.remove(rack_id)
            .map(|list| !list.is_empty())
            .unwrap_or(false)
    };
    if cleared {
        rack_update_cards(rack_id, Vec::new());
    }
}

/// Aborts every rack-backed card session and clears the whole UI card list.
/// Shutdown backstop: reaps sessions whose rack entry was already removed.
pub(in crate::com_port) fn stop_all_rack_cards() {
    for epoch in lock(&RACK_CARD_EPOCH).values_mut() {
        *epoch += 1;
    }
    {
        let mut tasks = lock(&RACK_CARD_TASKS);
        for (_iccid, task) in tasks.drain() {
            task.handle.abort();
            log::info!(
                "RACK | [SPAWN] card={} rack={} status=aborted reason=rack_gone",
                task.card_number,
                task.rack_id
            );
        }
    }
    let rack_ids: Vec<String> = {
        let mut ui = lock(&RACK_CARDS_UI);
        let ids: Vec<String> = ui
            .iter()
            .filter(|(_, list)| !list.is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        ui.clear();
        ids
    };
    for rack_id in rack_ids {
        rack_update_cards(&rack_id, Vec::new());
    }
}

/// Final spawn step shared by the card set handler and the pending-card retry:
/// a reader-backed session for the same card number wins — never open a second
/// connection with the same client_id (the server treats that as an ident
/// collision).
pub(super) async fn spawn_rack_card_checked(
    card_number: String,
    iccid: String,
    slot: u16,
    rack_id: String,
    port: SharedPort,
    log_header: &str,
    origin: SpawnOrigin,
) {
    // INVARIANT: the TASK_POOL guard is held across both the served-by-reader
    // check and the RACK_CARD_TASKS insert inside `spawn_rack_card`. Releasing
    // it in between (the guard used to be a temporary dropped at the end of
    // this `if`) opens the window the reader path exploits: `ensure_connection`
    // holds the same guard while it aborts rack sessions and registers its own
    // entry, so a rack spawn that checked before that abort and inserted after
    // it would leave two live MQTT connections under one client_id — the broker
    // then drops both in a mutual-takeover loop until the card is replugged.
    let pool = TASK_POOL.lock().await;
    if pool.iter().any(|card| card.client_id == card_number) {
        log::warn!(
            "{} [SPAWN] card={} slot={} status=skipped reason=served_by_reader",
            log_header,
            card_number,
            slot
        );
        return;
    }

    spawn_rack_card(card_number, iccid, slot, rack_id, port, origin);
    drop(pool);
}
