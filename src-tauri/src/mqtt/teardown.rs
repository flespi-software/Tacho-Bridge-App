//! Ending card sessions: a clean DISCONNECT for each, a bounded wait for the
//! event loops and the card handles, and the rack-side retry afterwards.

use std::time::{Duration, Instant};

use crate::smart_card::{ProcessingCard, TASK_POOL};

/// Shutdown reasons that `shutdown_connections` branches on (most reasons are
/// free-text log context only). Shared constants tie the producing call sites
/// to the consuming comparisons — a bare literal drifting out of sync would
/// silently re-enable the behavior the branch suppresses.
///
/// "A successor entry for the same client_id is being registered": skips both
/// the rack-side retry (it could only report served_by_reader) and, for the
/// app connection, the offline emit (a late `false` would race the successor's
/// `true`).
pub(crate) const SHUTDOWN_REASON_STALE_ENTRY_REPLACED: &str = "stale_entry_replaced";
pub(crate) const SHUTDOWN_REASON_APP_CONNECTION_REPLACED: &str = "app_connection_replaced";

/// How long a closing MQTT task gets to flush its DISCONNECT packet and exit
/// before being force-aborted.
const SHUTDOWN_FLUSH_TIMEOUT_MS: u64 = 2000;
/// How long the shutdown waits for the card handles of ended tasks to be
/// closed on the blocking pool (a ResetCard close is a cold reset: hundreds of
/// milliseconds on a healthy reader).
const HANDLE_CLOSE_TIMEOUT_MS: u64 = 2000;
/// Poll step while waiting for closing MQTT tasks to finish.
const SHUTDOWN_POLL_INTERVAL_MS: u64 = 50;

/// Gracefully terminates MQTT tasks whose entries are already removed from
/// TASK_POOL: queues a clean DISCONNECT for each one (the server then logs a
/// normal close instead of an internal error), waits for the event loops to
/// flush the packet and exit on their own, and force-aborts the stragglers.
pub async fn shutdown_connections(cards: Vec<ProcessingCard>, reason: &str) {
    // A reader-backed session that closes here may have been the reason a
    // rack-backed session for the same card number was skipped
    // (`served_by_reader`) — remembered so the rack side can be retried once
    // the teardown is done (see the trailing call below). The retry is skipped
    // on the replacement path: a successor entry for the same client_id is
    // (or is being) registered, so the retry could only ever report
    // served_by_reader again.
    let had_reader_backed = reason != SHUTDOWN_REASON_STALE_ENTRY_REPLACED
        && cards.iter().any(|card| card.reader_name.is_some());

    // Phase 1: queue DISCONNECTs; a task whose request queue is unreachable
    // (already dead or clogged) has nothing to flush and is aborted right away.
    let mut waiting: Vec<ProcessingCard> = Vec::with_capacity(cards.len());
    // Tasks that ended or were aborted, kept for the handle-close wait below.
    let mut ended: Vec<ProcessingCard> = Vec::with_capacity(cards.len());
    for card in cards {
        // Single choke point every removal path goes through: drop the card's
        // sniffer state here so no path (e.g. physical card extraction) leaks
        // an entry in the sniffer's global map. No-op when absent.
        crate::apdu_sniffer::forget(&card.client_id);
        // Same choke point for the refusal of the server: it belongs to the session that ends here.
        crate::global_app_handle::set_card_problem(&card.client_id, None);
        // The app-level connection (the one entry with no reader) never emits
        // its own offline transition when force-aborted mid-backoff — tell the
        // frontend explicitly so the UI cannot keep showing a dead connection
        // as online. The replacement path skips this: its successor connection
        // starts immediately and a late `false` would race the new `true`.
        if card.reader_name.is_none() && reason != SHUTDOWN_REASON_APP_CONNECTION_REPLACED {
            crate::global_app_handle::app_emit_event(false);
        }
        match card.mqtt_client.try_disconnect() {
            Ok(()) => {
                log::info!(
                    "{} | [CONN] phase=shutdown status=disconnect_queued reason={}",
                    card.client_id,
                    reason
                );
                waiting.push(card);
            }
            Err(e) => {
                log::warn!(
                    "{} | [CONN] phase=shutdown status=force_abort reason={} err={:?}",
                    card.client_id,
                    reason,
                    e
                );
                card.task_handle.abort();
                ended.push(card);
            }
        }
    }

    // Phase 2: one shared deadline for all tasks - the event loop exits itself
    // after flushing the DISCONNECT (see the Outgoing::Disconnect break arm).
    let deadline = Instant::now() + Duration::from_millis(SHUTDOWN_FLUSH_TIMEOUT_MS);
    while !waiting.is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(SHUTDOWN_POLL_INTERVAL_MS)).await;
        let (closed, still_running): (Vec<_>, Vec<_>) = std::mem::take(&mut waiting)
            .into_iter()
            .partition(|card| card.task_handle.inner().is_finished());
        for card in closed {
            log::info!(
                "{} | [CONN] phase=shutdown status=closed_cleanly reason={}",
                card.client_id,
                reason
            );
            ended.push(card);
        }
        waiting = still_running;
    }

    // A task still alive here is offline or wedged (e.g. sitting in reconnect
    // backoff, unable to process the queued DISCONNECT) - kill it like before.
    for card in waiting {
        log::warn!(
            "{} | [CONN] phase=shutdown status=force_abort reason=flush_timeout_{}ms",
            card.client_id,
            SHUTDOWN_FLUSH_TIMEOUT_MS
        );
        card.task_handle.abort();
        ended.push(card);
    }

    // Phase 3: an ended reader-backed task dropped its card handle, which is
    // closed — resetting the card, as a dropped handle always did — on the
    // blocking pool rather than inline (see smart_card::CardCell). Wait for
    // those closes: callers rescan right after this returns (a probe racing
    // the reset fails its ICCID read and leaves the card unserved), or exit
    // the process, which would skip the reset. Bounded: a reader wedged in
    // PC/SC may never let go of its handle.
    let readers: Vec<String> = ended
        .iter()
        .filter_map(|card| card.reader_name.clone())
        .collect();
    if !readers.is_empty() {
        let deadline = Instant::now() + Duration::from_millis(HANDLE_CLOSE_TIMEOUT_MS);
        while ended
            .iter()
            .any(|card| card.reader_name.is_some() && !card.task_handle.inner().is_finished())
            || readers
                .iter()
                .any(|reader| crate::smart_card::reader_io_pending(reader))
        {
            if Instant::now() >= deadline {
                log::warn!(
                    "[CONN] phase=shutdown status=handle_close_timeout reason={} readers={:?}",
                    reason,
                    readers
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(SHUTDOWN_POLL_INTERVAL_MS)).await;
        }
    }

    // The card numbers just released may belong to cards physically sitting in
    // a rack whose session spawn was skipped while a reader served the number.
    // Retry the rack side; a no-op when no rack is connected or nothing is
    // pending (and during app shutdown the rack ports are already gone).
    if had_reader_backed {
        crate::com_port::connect_pending_rack_cards().await;
    }
}

/// Terminates connections for the specified client IDs (card numbers).
pub async fn remove_connections(client_ids: Vec<String>) {
    log::debug!("Removing connections for client_ids: {:?}", client_ids);

    // Collect the matching cards under the pool lock, then close them outside
    // of it so the disconnect flush cannot block new card registrations.
    let mut removed: Vec<ProcessingCard> = Vec::new();
    {
        let mut task_pool = TASK_POOL.lock().await;
        for client_id in client_ids {
            // Find the index of the card with the matching client_id
            if let Some(index) = task_pool
                .iter()
                .position(|card| card.client_id == client_id)
            {
                let card = task_pool.remove(index);

                // Drop any APDU sniffer state we accumulated for this client so the
                // global HashMap does not grow without bound across reconnects.
                crate::apdu_sniffer::forget(&card.client_id);

                log::debug!(
                    "TASK_POOL: Connection scheduled for shutdown for client_id: {}, reader: {}, atr: {}",
                    card.client_id,
                    card.reader_name.as_deref().unwrap_or("unknown"),
                    card.atr.as_deref().unwrap_or("unknown"),
                );
                removed.push(card);
            } else {
                log::warn!(
                    "TASK_POOL: No active connection found for requested client_id: {}",
                    client_id
                );
                // Also clear any stale sniffer state, just in case (no-op if absent).
                crate::apdu_sniffer::forget(&client_id);
            }
        }
    }

    shutdown_connections(removed, "removed_by_request").await;
}

/// Terminates all active card-related MQTT connections and clears the task pool.
pub async fn remove_connections_all() {
    log::info!("Removing all card connections...");

    // Drain the pool under the lock, then close the connections outside of it.
    let cards: Vec<ProcessingCard> = {
        let mut task_pool = TASK_POOL.lock().await;
        task_pool.drain(..).collect()
    };

    // Clear all APDU sniffer state in one shot.
    crate::apdu_sniffer::forget_all();

    shutdown_connections(cards, "remove_all").await;

    log::info!("All card connections have been terminated and the task pool has been cleared.");
}
