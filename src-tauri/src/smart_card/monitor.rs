//! The reader monitor: a thread that watches the PC/SC readers for card and
//! reader events and runs a sweep (see `sweep`) on each of them.
//!
//! One pass owns one PC/SC context. A pass ends on a rescan request (a fresh
//! context re-reports every reader from UNAWARE) or on a failure, which the
//! monitor backs off from before the next pass; a panic inside a pass is
//! caught, so card detection never dies for the rest of the process.

use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use lazy_static::lazy_static;
use log::{debug, error, info, trace, warn};
use pcsc::{Context, ReaderState, Scope, State as PcscState, PNP_NOTIFICATION};
use tauri::async_runtime::block_on;

use super::sweep::{evict_vanished_readers, forget_unserved_probes, process_reader_states};
use super::DynResult;

/// Upper bound for one `get_status_change` wait in the monitor loop. A bounded
/// wait (instead of an infinite one) lets the loop notice a rescan request
/// even if the `cancel()` in `request_rescan` raced past a non-blocked monitor.
const MONITOR_POLL_TIMEOUT_SECS: u64 = 30;
/// Backoff bounds for failed monitor passes (context establish errors or
/// panics): the delay starts at the minimum and doubles per consecutive
/// failure up to the maximum, resetting after a successful pass.
const MONITOR_RETRY_MIN: Duration = Duration::from_secs(5);
const MONITOR_RETRY_MAX: Duration = Duration::from_secs(60);

lazy_static! {
    /// The PCSC context currently used by the reader monitor. Stored so that
    /// `request_rescan` can wake a blocked `get_status_change` from another
    /// thread via `Context::cancel()` (pcsc Context is Clone + Send + Sync).
    static ref MONITOR_CTX: std::sync::Mutex<Option<Context>> =
        std::sync::Mutex::new(None);
}

/// Set when someone asked the monitor for a full rescan (see `request_rescan`).
static RESCAN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Publishes (Some) or clears (None) the context slot used by `request_rescan`
/// to cancel a blocked `get_status_change` from another thread.
fn set_monitor_ctx(ctx: Option<Context>) {
    let mut slot = match MONITOR_CTX.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *slot = ctx;
}

/// Asks the reader monitor to drop its current PCSC view and rescan from
/// scratch: the context and reader states are rebuilt from UNAWARE, so every
/// still-inserted card is re-reported and re-registered through the normal
/// pipeline (`process_reader_states` → `ensure_connection`). Used after
/// `remove_connections_all()` — e.g. when the server host changes — so cards
/// reconnect with fresh config without being physically re-inserted.
pub fn request_rescan() {
    RESCAN_REQUESTED.store(true, Ordering::SeqCst);

    let guard = match MONITOR_CTX.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    match guard.as_ref() {
        Some(ctx) => match ctx.cancel() {
            Ok(()) => info!("Rescan requested: monitor get_status_change cancelled"),
            Err(e) => warn!(
                "Rescan requested, but cancel failed: {:?} (will be picked up within {}s on the poll timeout)",
                e,
                MONITOR_POLL_TIMEOUT_SECS
            ),
        },
        None => warn!("Rescan requested, but the monitor context is not available yet"),
    }
}

/// Lists the reader names with a buffer sized by PC/SC itself. A fixed buffer
/// failed the enumeration with `InsufficientBuffer` for good once enough
/// readers (or long enough names) were attached — and with it every monitor
/// pass. A reader plugged in between the length query and the listing makes
/// the queried size stale: one retry with a fresh length covers that race.
pub(super) fn list_reader_names(ctx: &Context) -> Result<Vec<CString>, pcsc::Error> {
    match ctx.list_readers_owned() {
        Err(pcsc::Error::InsufficientBuffer) => ctx.list_readers_owned(),
        other => other,
    }
}

/// Brings the watched reader states up to date before a wait: drops the
/// readers that are gone, adds the ones attached since (from UNAWARE), and
/// takes the last reported state as the one to wait on.
pub(super) fn setup_reader_states(
    ctx: &Context,
    reader_states: &mut Vec<ReaderState>,
) -> DynResult<()> {
    // Remove dead readers.
    fn is_dead(rs: &ReaderState) -> bool {
        rs.event_state()
            .intersects(PcscState::UNKNOWN | PcscState::IGNORE)
    }

    for rs in reader_states.iter() {
        if is_dead(rs) {
            debug!("Removing {:?}", rs.name());
        }
    }

    reader_states.retain(|rs| !is_dead(rs));
    // Add new readers.

    let names = list_reader_names(ctx)?;

    for name in names {
        if !reader_states.iter().any(|rs| rs.name() == name.as_c_str()) {
            debug!("Reader {:?} has been connected to the computer", name);
            reader_states.push(ReaderState::new(name, PcscState::UNAWARE));
        }
    }

    // Update the view of the state to wait on.
    for rs in reader_states.iter_mut() {
        rs.sync_current_state();
    }

    Ok(())
}

/// Guards against starting more than one smart-card monitor. `initialize_backend`
/// in `lib.rs` runs once, so this is a backstop rather than the load-bearing
/// guard it was when initialization hung off the repeatable `frontend-loaded`
/// event: a second monitor would mean duplicate PCSC contexts and doubled APDU
/// traffic to the same card.
static MONITOR_RUNNING: AtomicBool = AtomicBool::new(false);

/// Automatically sync cards.
///
/// Blocking by design: PC/SC is a synchronous API and `get_status_change` with
/// a timeout parks the calling thread until an event or the timeout. This function
/// must therefore run on a thread that is allowed to block (it is spawned via
/// `async_runtime::spawn_blocking` in lib.rs), never on a tokio async worker.
/// The async parts (task pool, card registration) are bridged via `block_on`.
pub fn sc_monitor() {
    // Guard against accidentally starting a second monitor.
    if MONITOR_RUNNING.swap(true, Ordering::SeqCst) {
        debug!("sc_monitor is already running. Skipping duplicate spawn.");
        return;
    }

    // Consecutive failed passes back off 5s -> 60s so a persistently broken
    // PCSC stack (unavailable daemon, polkit denial, panicking FFI) does not
    // flood the log; a pass that ran normally resets the delay.
    let mut retry_delay = MONITOR_RETRY_MIN;
    loop {
        // One full context lifecycle per pass. A panic anywhere inside (PCSC
        // FFI, event emission, config I/O) must not silently kill card
        // detection for the rest of the process — nothing ever respawns this
        // monitor. Catch it, log it, start a fresh pass.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(monitor_pass)) {
            Ok(Ok(PassEnd::Rescan)) => {
                // Deliberate restart (rescan requested, or the monitored
                // window closed normally): re-establish immediately.
                retry_delay = MONITOR_RETRY_MIN;
                continue;
            }
            Ok(Ok(PassEnd::Failed { was_healthy })) => {
                // A pass that watched readers normally (for days, maybe)
                // before failing starts the backoff over: only consecutive
                // failures should push it towards the maximum.
                if was_healthy {
                    retry_delay = MONITOR_RETRY_MIN;
                }
                // The pass gave up early. Fall through to the backoff sleep:
                // the fault (PCSC daemon down, reader enumeration refusing the
                // buffer) is usually persistent, and restarting immediately
                // would spin this thread at 100% CPU.
                warn!(
                    "sc_monitor pass ended early. Retrying in {} seconds...",
                    retry_delay.as_secs()
                );
            }
            Ok(Err(e)) => {
                if pcsc_service_gone(e) {
                    // No service, no readers: end what the readers served.
                    // Windows stops the service with its last reader and
                    // starts it with the next one, which must be seen soon.
                    block_on(evict_vanished_readers(&[]));
                    retry_delay = MONITOR_RETRY_MIN;
                }
                error!(
                    "Failed to establish context: {:?}. Retrying in {} seconds...",
                    e,
                    retry_delay.as_secs()
                );
                if e == pcsc::Error::SecurityViolation {
                    error!(
                        "SecurityViolation means the PC/SC daemon denied this process access. \
                         On Linux pcscd is usually guarded by polkit, which only allows clients \
                         from an active local desktop session - processes started inside a \
                         container, over SSH or as a service are rejected. Run the app from a \
                         local session, or add a polkit rule / start pcscd with --disable-polkit."
                    );
                }
            }
            Err(panic) => {
                let msg = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "non-string panic payload".to_string());
                error!(
                    "sc_monitor pass panicked: {}. Restarting in {} seconds...",
                    msg,
                    retry_delay.as_secs()
                );
                // The panic skipped the normal end-of-pass cleanup: drop the
                // published context so request_rescan() does not cancel a dead
                // handle while the monitor sleeps.
                set_monitor_ctx(None);
            }
        }
        std::thread::sleep(retry_delay);
        retry_delay = (retry_delay * 2).min(MONITOR_RETRY_MAX);
    }
}

/// The PC/SC service is not running (Windows stops it with the last reader,
/// pcscd may crash): every reader is gone with it.
fn pcsc_service_gone(e: pcsc::Error) -> bool {
    matches!(e, pcsc::Error::NoService | pcsc::Error::ServiceStopped)
}

/// Why a monitor pass ended. The caller paces the next pass from this: a
/// rescan is a deliberate restart and must not be delayed, while a failed pass
/// must back off — returning "healthy" for both is what let a persistently
/// broken PCSC stack (daemon down, `list_readers` refusing the buffer) spin the
/// monitor thread at 100% CPU, re-establishing a context per iteration.
#[derive(Debug, PartialEq, Eq)]
enum PassEnd {
    /// A rescan was requested (or the poll window closed normally): the next
    /// pass should start immediately.
    Rescan,
    /// The pass could not do its job and gave up early. Back off before
    /// retrying so a permanent fault does not spin. `was_healthy`: at least
    /// one wait completed normally before the failure, which resets the
    /// backoff.
    Failed { was_healthy: bool },
}

/// One pass of the reader monitor: establish a PCSC context, watch reader
/// state changes until the context needs re-establishing, then return.
/// Returns the PCSC error when the context could not be established; all retry
/// pacing (sleep, backoff) is the caller's job.
fn monitor_pass() -> Result<PassEnd, pcsc::Error> {
    debug!("Establishing PCSC context...");
    let ctx = Context::establish(Scope::User)?;
    debug!("Successfully established context.");

    // A fresh pass re-reports every reader and card from UNAWARE, so a rescan
    // requested while no context was alive (establish backoff, panic restart)
    // is satisfied by this pass anyway: clear the leftover flag so it does not
    // trigger a pointless context re-establish on the first poll timeout.
    RESCAN_REQUESTED.store(false, Ordering::SeqCst);

    // Publish the context so request_rescan() can cancel a blocked
    // get_status_change from another thread.
    set_monitor_ctx(Some(ctx.clone()));

    // Probes skipped as "unchanged since found unserved" are keyed by event
    // counters of the previous context's view; a fresh pass (rescan, a config
    // change linking a card, a reloaded webview) probes every card again.
    forget_unserved_probes();

    let mut reader_states: Vec<ReaderState> = vec![
        // Listen for reader insertions/removals, if supported.
        ReaderState::new(PNP_NOTIFICATION(), PcscState::UNAWARE),
    ];

    debug!("Initialized reader states.");

    // How this pass ended; only an explicit failure downgrades it.
    let mut outcome = PassEnd::Rescan;
    // Whether a `get_status_change` has completed normally in this pass.
    let mut was_healthy = false;
    // The stale-session sweep below runs once per pass, on its first
    // successful reader enumeration.
    let mut swept_vanished = false;

    loop {
        // These repeat every poll timeout (30s), so they live at trace to
        // keep debug output focused on actual card events.
        trace!("Starting the inner loop to monitor reader states...");
        if let Err(e) = setup_reader_states(&ctx, &mut reader_states) {
            error!(
                "Failed to list readers while updating monitor states: {:?}",
                e
            );
            // A failing enumeration is usually persistent (daemon down, buffer
            // too small): report it as a failed pass so the caller backs off.
            outcome = PassEnd::Failed { was_healthy };
            break; // Exit the inner loop to re-establish context
        }
        if !swept_vanished {
            swept_vanished = true;
            let present: Vec<String> = reader_states
                .iter()
                .filter(|rs| rs.name() != PNP_NOTIFICATION())
                .filter_map(|rs| rs.name().to_str().ok().map(str::to_owned))
                .collect();
            block_on(evict_vanished_readers(&present));
        }
        trace!(
            "Reader states: {:?}",
            reader_states
                .iter()
                .map(|rs| rs.name().to_string_lossy())
                .collect::<Vec<_>>()
        );

        match ctx.get_status_change(
            Some(Duration::from_secs(MONITOR_POLL_TIMEOUT_SECS)),
            &mut reader_states[..],
        ) {
            Ok(()) => was_healthy = true,
            Err(pcsc::Error::Timeout) => {
                was_healthy = true;
                // Nothing changed within the poll window — check for a
                // rescan request that missed the cancel(), keep waiting.
                if RESCAN_REQUESTED.swap(false, Ordering::SeqCst) {
                    info!(
                        "Rescan requested (picked up on poll timeout). Re-establishing context..."
                    );
                    break;
                }
                continue;
            }
            Err(pcsc::Error::Cancelled) => {
                // request_rescan() cancelled the wait. Break to the outer
                // loop: fresh context + UNAWARE reader states make PCSC
                // re-report every inserted card, and process_reader_states
                // re-registers them against the (now empty) task pool.
                RESCAN_REQUESTED.store(false, Ordering::SeqCst);
                info!("Rescan requested (get_status_change cancelled). Re-establishing context...");
                break;
            }
            Err(e) => {
                error!("get_status_change failed: {:?}", e);
                if pcsc_service_gone(e) {
                    // The readers went with the service, possibly before
                    // their removal was reported: no later pass can see them
                    // to evict while the service stays down.
                    block_on(evict_vanished_readers(&[]));
                }
                // Small backoff prevents a tight reconnect loop if the PCSC
                // service is repeatedly returning errors (e.g. daemon down).
                std::thread::sleep(Duration::from_secs(1));
                outcome = PassEnd::Failed { was_healthy };
                break;
            }
        }

        // Bridge back into the async runtime: registration locks the tokio
        // TASK_POOL mutex and spawns per-card MQTT tasks. Those futures run
        // on the async workers; this thread just waits for the result.
        block_on(process_reader_states(&mut reader_states));

        // A rescan requested while this thread was busy above found no wait
        // to cancel: honour it now, not after a whole quiet poll window.
        if RESCAN_REQUESTED.swap(false, Ordering::SeqCst) {
            info!("Rescan requested (picked up after a sweep). Re-establishing context...");
            break;
        }

        debug!("Waiting for the next status change...");
    }

    // The context is about to be dropped: clear the published clone so
    // request_rescan() cannot cancel a dead handle (a useless cancel that
    // would delay the rescan by a full poll window) — with the slot empty
    // it falls back to the RESCAN_REQUESTED flag, which the fresh pass
    // picks up on its bounded wait.
    set_monitor_ctx(None);

    debug!("Re-establishing context...");
    Ok(outcome)
}
