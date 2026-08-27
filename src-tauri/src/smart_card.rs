// ───── Std Lib ─────
use std::error::Error as StdError;
use std::ffi::CStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ───── Crates ─────
use lazy_static::lazy_static;
use log::{debug, error, info, warn};
use once_cell::sync::OnceCell;
use rumqttc::v5::AsyncClient;

use tauri::async_runtime::{block_on, JoinHandle, Mutex};

// ───── PCSC ─────
use pcsc::*;
use pcsc::{Card, Protocols, State as PcscState};

// ───── Local Modules ─────
use crate::config::{get_card_config_from_cache, get_from_cache, mutate_card_config, CacheSection};
use crate::global_app_handle::card_emit_event;
use crate::mqtt::{ensure_connection, remove_connections_all};

// ───── Constants ─────
const READERS_BUFFER_SIZE: usize = 2048;
const MANUAL_SYNC_TIMEOUT_SECS: u64 = 1;
/// Status word `mqtt.rs` substitutes on the wire when `send_apdu` returns a
/// transport error (the APDU could not be run against the card at all). Never
/// used to classify a reply — a card can legitimately answer `6F00` itself,
/// and the two cases are told apart by `send_apdu`'s `Result`, not by bytes.
pub(crate) const SW_TECHNICAL_PROBLEM: &str = "6F00";
/// Upper bound for one `get_status_change` wait in the monitor loop. A bounded
/// wait (instead of an infinite one) lets the loop notice a rescan request
/// even if the `cancel()` in `request_rescan` raced past a non-blocked monitor.
const MONITOR_POLL_TIMEOUT_SECS: u64 = 30;
/// Backoff bounds for failed monitor passes (context establish errors or
/// panics): the delay starts at the minimum and doubles per consecutive
/// failure up to the maximum, resetting after a successful pass.
const MONITOR_RETRY_MIN: Duration = Duration::from_secs(5);
const MONITOR_RETRY_MAX: Duration = Duration::from_secs(60);

type DynError = Box<dyn StdError + Send + Sync>;
type DynResult<T> = Result<T, DynError>;

/// Failure of one APDU exchange, classified for the retry decision in
/// `send_apdu` and reported to the MQTT bridge as a typed transport error —
/// distinct from any status word the card itself can return (a genuine `6F00`
/// reply is an `Ok`, not an error).
#[derive(Debug)]
pub enum ApduError {
    /// The request hex could not be decoded — nothing was ever sent, and a
    /// retry with the same bytes cannot succeed either.
    BadRequest(String),
    /// PCSC transmit failed; carries the typed error so the retry decision can
    /// tell a dead handle from a failure of an already-delivered command.
    Pcsc(pcsc::Error),
    /// The blocking transmit task died (panicked, or was cancelled before it
    /// started) without producing a result.
    TaskDied(String),
    /// The PCSC handle could not be recreated after a retriable failure.
    RecreateFailed(String),
}

impl std::fmt::Display for ApduError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApduError::BadRequest(e) => write!(f, "APDU decode error: {}", e),
            ApduError::Pcsc(e) => write!(f, "PCSC transmit error: {}", e),
            ApduError::TaskDied(e) => write!(f, "APDU transmit task died: {}", e),
            ApduError::RecreateFailed(e) => write!(f, "card handle recreate failed: {}", e),
        }
    }
}

impl StdError for ApduError {}

impl ApduError {
    /// True when the failure proves (or makes it overwhelmingly likely) that
    /// the command was NOT delivered to the card — the only situation where
    /// recreating the handle and re-sending the same APDU is safe. Replaying a
    /// command the card may already have executed would run a stateful APDU
    /// twice and corrupt the authentication state.
    fn is_retriable(&self) -> bool {
        match self {
            // Nothing was sent, but a retry with the same bytes fails the same way.
            ApduError::BadRequest(_) => false,
            ApduError::Pcsc(e) => matches!(
                e,
                pcsc::Error::InvalidHandle
                    | pcsc::Error::ResetCard
                    | pcsc::Error::RemovedCard
                    | pcsc::Error::ReaderUnavailable
                    | pcsc::Error::NoService
                    | pcsc::Error::ServiceStopped
            ),
            // A cancelled blocking task never started (spawn_blocking closures
            // cannot be interrupted once running), and the only panic sites sit
            // before the transmit call — the command did not reach the card.
            ApduError::TaskDied(_) => true,
            ApduError::RecreateFailed(_) => false,
        }
    }
}

/// Monotonic id source for `ProcessingCard::session_id`.
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// Allocates a process-unique id for one TASK_POOL entry (see
/// `ProcessingCard::session_id`).
pub fn next_session_id() -> u64 {
    NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
}

/// Represents a card currently being processed (i.e., connected and active).
#[derive(Debug)]
pub struct ProcessingCard {
    pub client_id: String, // It is Card number. Uses as client_id for mqtt connection
    /// Process-unique identity of THIS pool entry. client_id alone is not an
    /// identity: a card moving between readers replaces the entry under the
    /// same client_id, and the outgoing task must never remove the successor's
    /// entry when it cleans up after itself.
    pub session_id: u64,
    pub reader_name: Option<String>, // Name of the smart card reader (e.g., "Alcor Micro AU9540 00 00").
    pub atr: Option<String>,         // ATR of the inserted card (hex-encoded).
    #[allow(dead_code)]
    // to say the compiler does not warn about an unused field that is used in another file.
    pub mqtt_client: AsyncClient, // MQTT client instance.
    pub task_handle: JoinHandle<()>, // Async task handle managing communication for this card.
}

// ───── Statics ─────
lazy_static! {
    /// Global list of cards currently being processed (i.e., connected and active).
    pub static ref TASK_POOL: Arc<Mutex<Vec<ProcessingCard>>> =
        Arc::new(Mutex::new(Vec::new()));

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
            Ok(()) => log::info!("Rescan requested: monitor get_status_change cancelled"),
            Err(e) => log::warn!(
                "Rescan requested, but cancel failed: {:?} (will be picked up within {}s on the poll timeout)",
                e,
                MONITOR_POLL_TIMEOUT_SECS
            ),
        },
        None => log::warn!("Rescan requested, but the monitor context is not available yet"),
    }
}

fn setup_reader_states(
    ctx: &Context,
    readers_buf: &mut [u8],
    reader_states: &mut Vec<ReaderState>,
) -> DynResult<()> {
    // Remove dead readers.
    fn is_dead(rs: &ReaderState) -> bool {
        rs.event_state()
            .intersects(PcscState::UNKNOWN | PcscState::IGNORE)
    }

    for rs in reader_states.iter() {
        if is_dead(rs) {
            log::debug!("Removing {:?}", rs.name());
        }
    }

    reader_states.retain(|rs| !is_dead(rs));
    // Add new readers.

    let names = match ctx.list_readers(readers_buf) {
        Ok(names) => names,
        Err(e) => {
            log::error!("Failed to list readers: {:?}", e);
            return Err(Box::new(e)); // Return the error
        }
    };

    for name in names {
        if !reader_states.iter().any(|rs| rs.name() == name) {
            log::debug!("Reader {:?} has been connected to the computer", name);
            reader_states.push(ReaderState::new(name, PcscState::UNAWARE));
        }
    }

    // Update the view of the state to wait on.
    for rs in reader_states.iter_mut() {
        rs.sync_current_state();
    }

    Ok(())
}

lazy_static::lazy_static! {
    /// Serializes whole reader-sweep passes. Two callers run `process_reader_states`
    /// against the same physical readers: the `sc_monitor` thread and the
    /// `manual_sync_cards` command. Their per-card work is NOT protected by the
    /// TASK_POOL lock alone — `should_register_new_card` releases the pool before
    /// the `await` points that follow (get_iccid, switch_protocol), and the
    /// successor entry is only pushed at the very end of `ensure_connection`. So
    /// both sweeps can see "no entry", open Shared-mode PC/SC handles on the same
    /// card, interleave SELECT/READ APDUs, and — when the config prescribes a
    /// protocol switch — have the loser call `switch_protocol` (Disposition::ResetCard)
    /// right after the winner registered its session, wiping the card's security
    /// state in the middle of a live VU authentication.
    static ref READER_SWEEP: tokio::sync::Mutex<()> = tokio::sync::Mutex::new(());
}

// Every failure here is per-card and non-fatal: it is logged and the loop
// moves on to the next reader, so there is nothing to propagate to the caller.
async fn process_reader_states(reader_states: &mut [ReaderState]) {
    let _sweep = READER_SWEEP.lock().await;
    for rs in reader_states {
        if rs.name() == PNP_NOTIFICATION() {
            continue;
        }

        if is_virtual_reader(rs.name()) {
            log::warn!("Virtual reader {:?} detected. Skipping...", rs.name());
            continue;
        }

        let reader_name = rs.name();
        let Ok(reader_name_string) = reader_name.to_str() else {
            log::warn!(
                "Reader name is not valid UTF-8: {:?}. Skipping...",
                reader_name
            );
            continue;
        };

        let atr = hex::encode(rs.atr());
        let atr_protocol = parse_atr_and_get_protocol(&atr);
        let protocol = atr_protocol.protocol();
        let mut effective_protocol = protocol;

        let card_state_string = format!("{:?}", rs.event_state());
        log::debug!("card_state_string {}", card_state_string);

        let mut card_number = String::new();
        let mut iccid = String::new();

        let action = should_register_new_card(reader_name_string, &atr).await;

        match action {
            CardProcessingResult::Create => match ManagedCard::new(reader_name, protocol) {
                Ok(mut managed_card) => match managed_card.get_iccid().await {
                    Ok(received_iccid) => {
                        log::info!("ICCID: {}", received_iccid);

                        iccid = received_iccid;
                        card_number = get_from_cache(CacheSection::Cards, &iccid);

                        // The card number (and thus the config entry) is known only after
                        // reading the ICCID, so the card is opened with the ATR-derived
                        // protocol first and switched if the config says otherwise.
                        effective_protocol = resolve_t_protocol(&card_number, &atr_protocol);
                        if effective_protocol != protocol {
                            managed_card.switch_protocol(effective_protocol).await;
                        }

                        ensure_connection(
                            rs.name(),
                            card_number.clone(),
                            atr.clone(),
                            managed_card,
                        )
                        .await;
                    }
                    Err(e) => {
                        log::error!("Failed to get ICCID: {}", e);
                    }
                },
                Err(e) => {
                    log::error!(
                        "Failed to create ManagedCard for reader {}: {}",
                        reader_name_string,
                        e
                    );
                }
            },
            CardProcessingResult::Delete => {
                log::debug!("CARD DELETED {}", card_state_string);
            }
            CardProcessingResult::Ignore => {}
        }

        if action != CardProcessingResult::Ignore {
            card_emit_event(
                "global-cards-sync",
                iccid,
                reader_name_string.into(),
                card_state_string,
                card_number.clone(),
                None,
                None,
            );

            log::info!(
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

#[derive(Debug, PartialEq, Eq)]
pub enum CardProcessingResult {
    Create,
    Delete,
    Ignore,
}

/// Determines what action should be taken for a card with the given reader name and ATR.
/// Also removes any stale entries with the same reader name but a previously stored ATR.
pub async fn should_register_new_card(reader_name: &str, atr: &str) -> CardProcessingResult {
    let mut pool = TASK_POOL.lock().await;

    log::debug!(
        "should_register_new_card: reader='{}' atr_len={} pool_size={}",
        reader_name,
        atr.len(),
        pool.len()
    );

    // Case 1: Both reader_name and atr are provided and not found in the pool → register new card
    if !reader_name.is_empty() && !atr.is_empty() {
        let exists = pool.iter().any(|c| {
            c.reader_name.as_deref() == Some(reader_name) && c.atr.as_deref() == Some(atr)
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
                log::warn!(
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
            log::warn!(
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

/// Check if the reader is a virtual reader. This usually only applies to Windows.
fn is_virtual_reader(reader_name: &CStr) -> bool {
    // Convert the reader name to a lowercase string
    let reader_name_lower = reader_name.to_string_lossy().to_lowercase();

    // Check if the name contains keywords indicating a virtual reader
    reader_name_lower.contains("microsoft")
        || reader_name_lower.contains("virtual")
        || reader_name_lower.contains("remote")
}

/// Guards against starting more than one smart-card monitor. `initialize_backend`
/// in `lib.rs` runs once, so this is a backstop rather than the load-bearing
/// guard it was when initialization hung off the repeatable `frontend-loaded`
/// event: a second monitor would mean duplicate PCSC contexts and doubled APDU
/// traffic to the same card.
static MONITOR_RUNNING: AtomicBool = AtomicBool::new(false);

// Automatically sync cards.
//
// Blocking by design: PC/SC is a synchronous API and `get_status_change` with
// no timeout parks the calling thread until a reader/card event. This function
// must therefore run on a thread that is allowed to block (it is spawned via
// `async_runtime::spawn_blocking` in lib.rs), never on a tokio async worker.
// The async parts (task pool, card registration) are bridged via `block_on`.
pub fn sc_monitor() {
    // Ignore duplicate spawns from repeated `frontend-loaded` events.
    if MONITOR_RUNNING.swap(true, Ordering::SeqCst) {
        log::debug!("sc_monitor is already running. Skipping duplicate spawn.");
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
            Ok(Ok(())) => {
                // Healthy pass ended (rescan or transient error inside the
                // monitored window): re-establish immediately, no backoff.
                retry_delay = MONITOR_RETRY_MIN;
                continue;
            }
            Ok(Err(e)) => {
                log::error!(
                    "Failed to establish context: {:?}. Retrying in {} seconds...",
                    e,
                    retry_delay.as_secs()
                );
                if e == pcsc::Error::SecurityViolation {
                    log::error!(
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
                log::error!(
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

/// One pass of the reader monitor: establish a PCSC context, watch reader
/// state changes until the context needs re-establishing, then return.
/// Returns the PCSC error when the context could not be established; all retry
/// pacing (sleep, backoff) is the caller's job.
fn monitor_pass() -> Result<(), pcsc::Error> {
    log::debug!("Establishing PCSC context...");
    let ctx = Context::establish(Scope::User)?;
    log::debug!("Successfully established context.");

    // A fresh pass re-reports every reader and card from UNAWARE, so a rescan
    // requested while no context was alive (establish backoff, panic restart)
    // is satisfied by this pass anyway: clear the leftover flag so it does not
    // trigger a pointless context re-establish on the first poll timeout.
    RESCAN_REQUESTED.store(false, Ordering::SeqCst);

    // Publish the context so request_rescan() can cancel a blocked
    // get_status_change from another thread.
    set_monitor_ctx(Some(ctx.clone()));

    let mut readers_buf = [0; READERS_BUFFER_SIZE];
    let mut reader_states: Vec<ReaderState> = vec![
        // Listen for reader insertions/removals, if supported.
        ReaderState::new(PNP_NOTIFICATION(), PcscState::UNAWARE),
    ];

    log::debug!("Initialized readers buffer and reader states.");

    loop {
        // These repeat every poll timeout (30s), so they live at trace to
        // keep debug output focused on actual card events.
        log::trace!("Starting the inner loop to monitor reader states...");
        if let Err(e) = setup_reader_states(&ctx, &mut readers_buf, &mut reader_states) {
            log::error!("Failed to setup_reader_states: {:?}", e);
            break; // Exit the inner loop to re-establish context
        }
        log::trace!(
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
            Ok(()) => {}
            Err(pcsc::Error::Timeout) => {
                // Nothing changed within the poll window — check for a
                // rescan request that missed the cancel(), keep waiting.
                if RESCAN_REQUESTED.swap(false, Ordering::SeqCst) {
                    log::info!(
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
                log::info!(
                    "Rescan requested (get_status_change cancelled). Re-establishing context..."
                );
                break;
            }
            Err(e) => {
                log::error!("get_status_change failed: {:?}", e);
                // Small backoff prevents a tight reconnect loop if the PCSC
                // service is repeatedly returning errors (e.g. daemon down).
                std::thread::sleep(Duration::from_secs(1));
                break;
            }
        }

        // Bridge back into the async runtime: registration locks the tokio
        // TASK_POOL mutex and spawns per-card MQTT tasks. Those futures run
        // on the async workers; this thread just waits for the result.
        block_on(process_reader_states(&mut reader_states));

        log::debug!("Waiting for the next status change...");
    }

    // The context is about to be dropped: clear the published clone so
    // request_rescan() cannot cancel a dead handle (a useless cancel that
    // would delay the rescan by a full poll window) — with the slot empty
    // it falls back to the RESCAN_REQUESTED flag, which the fresh pass
    // picks up on its bounded wait.
    set_monitor_ctx(None);

    log::debug!("Re-establishing context...");
    Ok(())
}

/// Renders a PCSC protocol as the config string form ("T0"/"T1").
pub fn protocol_to_str(protocol: Protocols) -> &'static str {
    if protocol == Protocols::T1 {
        "T1"
    } else {
        "T0"
    }
}

/// Parses the config string form of a protocol; accepts "T0"/"T1" case-insensitively.
pub fn protocol_from_str(value: &str) -> Option<Protocols> {
    match value.trim() {
        v if v.eq_ignore_ascii_case("T0") => Some(Protocols::T0),
        v if v.eq_ignore_ascii_case("T1") => Some(Protocols::T1),
        _ => None,
    }
}

/// Returns the protocol to use for the card: the `t_protocol` value stored in the
/// card config wins; on the first connection of a configured card the ATR-derived
/// protocol is persisted so later connects and reconnects reuse it.
/// Blocking (config file I/O on first connection) — the reader monitor thread is
/// allowed to block, so this must not be called from an async worker.
fn resolve_t_protocol(card_number: &str, atr: &AtrProtocol) -> Protocols {
    let atr_protocol = atr.protocol();

    if card_number.is_empty() {
        // Card is not configured yet - nowhere to store the protocol.
        return atr_protocol;
    }

    match get_card_config_from_cache(card_number).and_then(|card| card.t_protocol) {
        Some(stored) => match protocol_from_str(&stored) {
            Some(protocol) => {
                if protocol != atr_protocol {
                    info!(
                        "Card {}: using stored T protocol {} (ATR suggests {})",
                        card_number,
                        protocol_to_str(protocol),
                        protocol_to_str(atr_protocol)
                    );
                }
                protocol
            }
            None => {
                warn!(
                    "Card {}: invalid t_protocol value {:?} in config (expected \"T0\" or \"T1\"), using ATR-derived {}",
                    card_number,
                    stored,
                    protocol_to_str(atr_protocol)
                );
                atr_protocol
            }
        },
        None => {
            // Only a conclusive ATR read may be persisted. Storing a fallback
            // guess would freeze it: every later connection reuses the stored
            // value without consulting the ATR again, so one truncated read
            // would pin the wrong protocol until the user edits config.yaml.
            if !atr.is_resolved() {
                warn!(
                    "Card {}: ATR did not conclusively identify the T protocol; \
                     using {} for this connection without storing it",
                    card_number,
                    protocol_to_str(atr_protocol)
                );
                return atr_protocol;
            }

            let value = protocol_to_str(atr_protocol);
            if mutate_card_config(card_number, |card| {
                card.t_protocol = Some(value.to_string());
                true
            }) {
                info!(
                    "Card {}: stored ATR-derived T protocol {} in config",
                    card_number, value
                );
            }
            atr_protocol
        }
    }
}

/// Outcome of reading the protocol out of an ATR.
///
/// The distinction matters because the ATR-derived protocol gets *persisted* on
/// a card's first connection (see `resolve_t_protocol`). A truncated ATR and a
/// genuine T=0 ATR both used to yield `Protocols::T0`, so one malformed read
/// could pin the wrong protocol into the config forever — only fixable by hand
/// editing the YAML. Keeping "I could not tell" separate from "it is T0" lets
/// the caller decline to store a guess.
#[derive(Debug, PartialEq, Eq)]
pub enum AtrProtocol {
    /// The interface bytes were present and complete: this is the card's protocol.
    Resolved(Protocols),
    /// The ATR is absent, malformed, or truncated mid-way through its interface
    /// bytes. Carries the protocol to *use* for this connection (T0, the safe
    /// default), but it must not be written to the config.
    Indeterminate(Protocols),
}

impl AtrProtocol {
    /// The protocol to open the card with, whether or not the parse was conclusive.
    pub fn protocol(&self) -> Protocols {
        match self {
            AtrProtocol::Resolved(p) | AtrProtocol::Indeterminate(p) => *p,
        }
    }

    /// True only when the ATR conclusively identified the protocol — the sole
    /// case where it is safe to persist.
    pub fn is_resolved(&self) -> bool {
        matches!(self, AtrProtocol::Resolved(_))
    }
}

/// Parses the ATR and extracts the communication protocol (T=0 or T=1).
///
/// Walks the interface bytes per ISO/IEC 7816-3: TS, T0, then the TA/TB/TC/TD
/// groups selected by each Y nibble. Every skip is bounds-checked — running past
/// the end means the ATR was cut short and nothing can be concluded from it.
///
/// # Arguments
/// - `atr`: A string containing the ATR in hexadecimal format.
///
/// # Returns
/// - `AtrProtocol`: `Resolved` when the interface bytes were complete,
///   `Indeterminate` when the ATR was absent, malformed or truncated.
pub fn parse_atr_and_get_protocol(atr: &str) -> AtrProtocol {
    fn protocol_from_td(td: u8) -> Protocols {
        match td & 0x0F {
            0x01 => Protocols::T1,
            // T=0 and every other (unsupported) protocol indication fall back
            // to T0, which is what PCSC negotiates for them anyway.
            _ => Protocols::T0,
        }
    }

    let atr_bytes = match hex::decode(atr) {
        Ok(bytes) => bytes,
        Err(_) => {
            log::error!("Invalid ATR format: {}", atr);
            return AtrProtocol::Indeterminate(Protocols::T0);
        }
    };

    // An empty ATR is the normal "no card in the reader" case (e.g. a card
    // removal event) — nothing to parse, and not worth a warning.
    if atr_bytes.is_empty() {
        return AtrProtocol::Indeterminate(Protocols::T0);
    }

    if atr_bytes.len() < 2 {
        log::warn!("ATR is too short: {:?}", atr_bytes);
        return AtrProtocol::Indeterminate(Protocols::T0);
    }

    // Advances past the TA/TB/TC bytes selected by `y`, reporting whether they
    // all actually fit in the ATR. Previously these skips were unchecked, so a
    // truncated ATR silently walked `index` past the end and the TD lookups
    // below quietly found nothing — reported as a confident T0.
    fn skip_interface_bytes(index: &mut usize, y: u8, len: usize) -> bool {
        for bit in [0x1, 0x2, 0x4] {
            if y & bit != 0 {
                *index += 1;
                if *index > len {
                    return false;
                }
            }
        }
        true
    }

    let len = atr_bytes.len();
    let mut index = 1;
    let y1 = atr_bytes[index] >> 4;
    index += 1;

    // TA1, TB1, TC1 — presence selected by Y1.
    if !skip_interface_bytes(&mut index, y1, len) {
        log::warn!("ATR truncated inside its first interface bytes: {}", atr);
        return AtrProtocol::Indeterminate(Protocols::T0);
    }

    // TD1 absent → only the default protocol (T=0) is offered, which IS
    // conclusive: an ATR without TD1 defines T=0 per ISO 7816-3.
    if y1 & 0x8 == 0 {
        return AtrProtocol::Resolved(Protocols::T0);
    }
    if index >= len {
        log::warn!("ATR claims TD1 but ends before it: {}", atr);
        return AtrProtocol::Indeterminate(Protocols::T0);
    }
    let td1 = atr_bytes[index];
    index += 1;

    // TA2, TB2, TC2 — presence selected by Y2 (the high nibble of TD1).
    let y2 = td1 >> 4;
    if !skip_interface_bytes(&mut index, y2, len) {
        log::warn!("ATR truncated inside its second interface bytes: {}", atr);
        return AtrProtocol::Indeterminate(Protocols::T0);
    }

    // TD2 absent → TD1 carries the protocol.
    if y2 & 0x8 == 0 {
        return AtrProtocol::Resolved(protocol_from_td(td1));
    }
    if index >= len {
        log::warn!("ATR claims TD2 but ends before it: {}", atr);
        return AtrProtocol::Indeterminate(Protocols::T0);
    }

    // TD2 present → it names the preferred protocol.
    AtrProtocol::Resolved(protocol_from_td(atr_bytes[index]))
}

// Manual card sync function.
// This function is used to manually sync cards from anywhere in the program.
// Manually sync cards. Clicking on the button in the frontend will trigger this function
#[tauri::command]
pub async fn manual_sync_cards(_readername: String, restart: bool) -> Result<(), String> {
    log::debug!("Manual sync cards function is called. Restart: {}", restart);

    if restart {
        // remove all connections
        remove_connections_all().await;

        // Wake the monitor for a full rescan: the still-inserted cards get
        // re-registered with fresh config (e.g. the new server host) without
        // a physical re-plug. The app-level connection is restarted separately
        // by the frontend via the app_connection command.
        request_rescan();

        return Ok(());
    }

    // The whole sweep is blocking (PCSC calls, and process_reader_states writes
    // the config on a card's first connection) — keep it off the async workers,
    // same as the sc_monitor thread does.
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let ctx = Context::establish(Scope::User)
            .map_err(|e| format!("failed to establish context: {e}"))?;
        log::debug!("Context established successfully.");

        let mut readers_buf = [0; READERS_BUFFER_SIZE];
        match ctx.list_readers(&mut readers_buf) {
            Ok(readers) => {
                if readers.count() == 0 {
                    log::warn!("No readers found. Exiting...");
                    return Ok(());
                }
                log::debug!("Available readers found");
            }
            Err(e) => {
                log::error!("Failed to list readers: {:?}", e);
                return Ok(());
            }
        }

        let mut reader_states = vec![
            // Listen for reader insertions/removals, if supported.
            ReaderState::new(PNP_NOTIFICATION(), State::UNAWARE),
        ];

        // setup readers states. Getting changes and other inits
        if let Err(e) = setup_reader_states(&ctx, &mut readers_buf, &mut reader_states) {
            log::error!("Failed to setup reader states: {:?}", e);
        }
        // waiting for the status change
        ctx.get_status_change(
            Some(Duration::from_secs(MANUAL_SYNC_TIMEOUT_SECS)),
            &mut reader_states,
        )
        .map_err(|e| format!("failed to get status change: {e}"))?;

        block_on(process_reader_states(&mut reader_states));
        Ok(())
    })
    .await
    .map_err(|e| format!("manual_sync_cards: blocking task failed: {e}"))?
}
//////////////////////////////////////////////////
/// CARD WRAPER //////////////////////////////////
/// //////////////////////////////////////////////
#[derive(Clone)]
pub struct ManagedCard {
    inner: Arc<Mutex<Card>>,
    reader_name: Arc<CStr>,
    protocol: Protocols,
    pub iccid: OnceCell<String>,
}

impl ManagedCard {
    pub fn new(reader_name: &CStr, protocol: Protocols) -> DynResult<Self> {
        debug!(
            "ManagedCard::new() called. Reader: '{}', Protocol: {:?}",
            reader_name.to_string_lossy(),
            protocol
        );

        let card = Self::create_card(reader_name, protocol)?;
        debug!(
            "Card successfully created for reader: '{}'",
            reader_name.to_string_lossy()
        );

        Ok(Self {
            inner: Arc::new(Mutex::new(card)),
            reader_name: Arc::from(reader_name.to_owned()),
            protocol,
            iccid: OnceCell::new(),
        })
    }

    fn create_card(reader_name: &CStr, protocol: Protocols) -> DynResult<Card> {
        let ctx = Context::establish(Scope::User).map_err(|err| {
            log::error!("Failed to establish context: {}", err);
            Box::<dyn StdError + Send + Sync>::from(err)
        })?;

        let card = ctx
            .connect(reader_name, ShareMode::Shared, protocol)
            .map_err(|err| {
                log::error!("Failed to connect to card: {}", err);
                Box::<dyn StdError + Send + Sync>::from(err)
            })?;

        Ok(card)
    }

    /// Switches the card to another T protocol: reconnects the underlying PCSC
    /// card with it and remembers it for future reconnects/recreates. `protocol`
    /// is a plain copied field, so clones keep the old value - valid call sites
    /// are before the ManagedCard is cloned into the task pool, and inside the
    /// card's own MQTT task, which is the sole clone touching the card afterwards.
    pub async fn switch_protocol(&mut self, protocol: Protocols) {
        if protocol == self.protocol {
            return;
        }

        info!(
            "Switching card protocol to {} for reader: {}",
            protocol_to_str(protocol),
            self.reader_name.to_string_lossy()
        );

        let reconnect_result = {
            let mut card = self.inner.lock().await;
            card.reconnect(ShareMode::Shared, protocol, Disposition::ResetCard)
        };

        match reconnect_result {
            Ok(_) => self.protocol = protocol,
            Err(e) => {
                warn!(
                    "Failed to switch card protocol to {} for reader {}: {:?}. Will try to recreate.",
                    protocol_to_str(protocol),
                    self.reader_name.to_string_lossy(),
                    e
                );
                match Self::create_card(&self.reader_name, protocol) {
                    Ok(new_card) => {
                        *self.inner.lock().await = new_card;
                        self.protocol = protocol;
                    }
                    Err(e) => error!(
                        "Failed to recreate card with protocol {} for reader {}: {}. Keeping {}.",
                        protocol_to_str(protocol),
                        self.reader_name.to_string_lossy(),
                        e,
                        protocol_to_str(self.protocol)
                    ),
                }
            }
        }
    }

    /// Current T protocol in the config string form ("T0"/"T1").
    pub fn protocol_str(&self) -> &'static str {
        protocol_to_str(self.protocol)
    }

    pub async fn reconnect(&self) {
        debug!(
            "Attempting to reconnect card for reader: {}",
            self.reader_name.to_string_lossy()
        );

        let reconnect_result = {
            let mut card = self.inner.lock().await;
            // Reconnect with the same protocol the card was opened with (stored or
            // ATR-derived) - ANY would let PCSC renegotiate and silently change it.
            card.reconnect(ShareMode::Shared, self.protocol, Disposition::ResetCard)
        };

        match reconnect_result {
            Ok(_) => {
                debug!(
                    "Card reconnected successfully for reader: {}",
                    self.reader_name.to_string_lossy()
                );
            }
            Err(e) => {
                warn!(
                    "Failed to reconnect card: {:?} for reader: {}. Will try to recreate.",
                    e,
                    self.reader_name.to_string_lossy()
                );

                if let Err(e) = self.recreate().await {
                    error!(
                        "Failed to recreate card after reconnect failure for reader {}: {}",
                        self.reader_name.to_string_lossy(),
                        e
                    );
                }
            }
        }
    }

    pub async fn recreate(&self) -> DynResult<()> {
        let new_card = Self::create_card(&self.reader_name, self.protocol)?;
        let mut lock = self.inner.lock().await;
        *lock = new_card;

        info!(
            "Successfully recreated card object for reader: {}",
            self.reader_name.to_string_lossy()
        );

        Ok(())
    }

    pub async fn apdu_transmit(&self, apdu_hex: &str) -> Result<String, ApduError> {
        let apdu = match hex::decode(apdu_hex) {
            Ok(data) => data,
            Err(err) => {
                error!("Failed to decode APDU '{}': {}", apdu_hex, err);
                return Err(ApduError::BadRequest(err.to_string()));
            }
        };

        let card = Arc::clone(&self.inner);

        // The send/response pair is logged by send_apdu() with the client_id
        // header; this layer only logs failures. The buffer is sized for
        // extended-length responses: a RAPDU past 258 bytes is legitimate
        // (chained/extended reads) and must not fail the exchange.
        let response = tauri::async_runtime::spawn_blocking(move || {
            let mut rapdu_buf = vec![0u8; pcsc::MAX_BUFFER_SIZE_EXTENDED];

            let locked = card.blocking_lock();

            match locked.transmit(&apdu, &mut rapdu_buf) {
                Ok(response) => Ok(hex::encode(response)),
                Err(err) => {
                    error!("APDU transmit failed: {}", err);
                    // Preserved as the typed pcsc error so send_apdu can tell a
                    // dead handle from an error of an already-delivered command.
                    Err(err)
                }
            }
        })
        .await;

        match response {
            Ok(Ok(rapdu)) => Ok(rapdu),
            Ok(Err(pcsc_err)) => Err(ApduError::Pcsc(pcsc_err)),
            // The blocking task itself died: keep this distinct from a pcsc
            // failure so the retry decision does not lump it into the
            // "may have reached the card" bucket (see ApduError::is_retriable).
            Err(join_err) => Err(ApduError::TaskDied(join_err.to_string())),
        }
    }

    /// Runs one APDU against the card, recreating the PCSC handle and retrying
    /// once when the failure proves the command never reached the card.
    ///
    /// `Ok` carries the card's actual reply — any status word, including a
    /// genuine `6F00`. `Err` means the exchange failed at the transport level
    /// and no card reply exists: the caller substitutes `SW_TECHNICAL_PROBLEM`
    /// on the wire and must keep that reply out of the idempotency cache so the
    /// server's retry reaches the card again (same rule the rack path applies
    /// via `SerialExchange::is_ok()`).
    pub async fn send_apdu(&self, apdu_hex: &str, client_id: &str) -> Result<String, ApduError> {
        debug!("{} Sending APDU command: {}", client_id, apdu_hex);

        // First attempt
        match self.apdu_transmit(apdu_hex).await {
            Ok(response) => {
                debug!("{} APDU response: {:?}", client_id, response);
                return Ok(response);
            }
            Err(err) => {
                // Recreate-and-retry is only safe when the failure proves the
                // command never reached the card (dead handle / reader / PCSC
                // service, or a transmit task that died before sending). For
                // anything else the card may already have executed it —
                // recreating resets the card (destroying the SM auth state)
                // and replaying a stateful APDU would run it twice.
                if !err.is_retriable() {
                    error!(
                        "{} Failed to send APDU: {}. Not retried: the command may have reached the card.",
                        client_id, err
                    );
                    return Err(err);
                }
                error!(
                    "{} Failed to send APDU: {}. Command not delivered - attempting to recreate card...",
                    client_id, err
                );
            }
        }

        // recreate attempt
        if let Err(e) = self.recreate().await {
            error!(
                "{} Failed to recreate card after APDU failure: {}",
                client_id, e
            );
            return Err(ApduError::RecreateFailed(e.to_string()));
        }

        // Second attempt
        match self.apdu_transmit(apdu_hex).await {
            Ok(response) => {
                debug!(
                    "{} APDU response (after recreate): {:?}",
                    client_id, response
                );
                Ok(response)
            }
            Err(retry_err) => {
                error!(
                    "{} Retry failed: could not send APDU after recreate: {}",
                    client_id, retry_err
                );
                Err(retry_err)
            }
        }
    }

    /// Returns the card ICCID using lazy caching.
    /// On first call, reads it from the card; subsequent calls return the cached value.
    pub async fn get_iccid(&self) -> DynResult<String> {
        if let Some(cached) = self.iccid.get() {
            log::debug!(
                "Returning cached ICCID for reader {}: {}",
                self.reader_name.to_string_lossy(),
                cached
            );
            return Ok(cached.clone());
        }

        log::debug!(
            "get_iccid() started for reader: {}",
            self.reader_name.to_string_lossy()
        );

        // SELECT EF_ICC (FID 0002) under MF
        let select_result = self.apdu_transmit("00A4020C020002").await?;

        if !select_result.ends_with("9000") {
            // Do NOT proceed to READ BINARY: on SW != 9000 the current EF is
            // unchanged, so READ would return bytes from whatever was previously
            // selected (e.g. EF_Identification from a prior auth session),
            // producing a garbage ICCID that happens to be part of cardNumber.
            return Err(format!("SELECT EF_ICC failed with status: {}", select_result).into());
        }

        // READ BINARY: 8 bytes of cardExtendedSerialNumber at offset 1 (skipping clockStop)
        let read_response = self.apdu_transmit("00B0000108").await?;

        let Some(hex_data) = read_response.strip_suffix("9000") else {
            return Err(format!(
                "READ BINARY EF_ICC returned unexpected status: {}",
                read_response
            )
            .into());
        };

        let bytes =
            hex::decode(hex_data).map_err(|e| format!("Failed to decode ICCID hex: {}", e))?;

        if bytes.len() != 8 {
            return Err(format!(
                "EF_ICC READ BINARY returned {} bytes, expected 8",
                bytes.len()
            )
            .into());
        }

        let iccid = bytes
            .iter()
            .map(|b| format!("{:02X}", b))
            .collect::<String>();

        log::debug!("Final ICCID: {}", iccid);

        // Save ICCID, not got earlier
        let _ = self.iccid.set(iccid.clone());

        Ok(iccid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_str_roundtrip() {
        assert_eq!(protocol_to_str(Protocols::T0), "T0");
        assert_eq!(protocol_to_str(Protocols::T1), "T1");
        assert_eq!(protocol_from_str("T0"), Some(Protocols::T0));
        assert_eq!(protocol_from_str("T1"), Some(Protocols::T1));
    }

    #[test]
    fn protocol_from_str_is_lenient_about_case_and_spaces() {
        assert_eq!(protocol_from_str("t0"), Some(Protocols::T0));
        assert_eq!(protocol_from_str(" t1 "), Some(Protocols::T1));
    }

    #[test]
    fn protocol_from_str_rejects_unknown_values() {
        assert_eq!(protocol_from_str(""), None);
        assert_eq!(protocol_from_str("T2"), None);
        assert_eq!(protocol_from_str("T=0"), None);
        assert_eq!(protocol_from_str("ANY"), None);
    }

    #[test]
    fn atr_without_td1_resolves_to_t0() {
        // TS=3B, T0=0x60 → Y1=0110 (TB1+TC1 present, no TD1). No TD1 means the
        // card offers only the default protocol, which is conclusive.
        assert_eq!(
            parse_atr_and_get_protocol("3B6000FF"),
            AtrProtocol::Resolved(Protocols::T0)
        );
    }

    #[test]
    fn atr_with_td1_resolves_from_td1() {
        // TS=3B, T0=0x81 → Y1=1000 (TD1 present), TD1=0x31 → protocol T=1,
        // Y2=0011 (TA2+TB2 present, no TD2) → TD1 carries the protocol.
        assert_eq!(
            parse_atr_and_get_protocol("3B8131AABB"),
            AtrProtocol::Resolved(Protocols::T1)
        );
    }

    #[test]
    fn atr_with_td2_prefers_td2() {
        // TD1=0x80 → Y2=1000 (TD2 present, no TA2/TB2/TC2), TD2=0x01 → T=1.
        // TD2 wins over TD1's own protocol nibble (0 here).
        assert_eq!(
            parse_atr_and_get_protocol("3B8180 01".replace(' ', "").as_str()),
            AtrProtocol::Resolved(Protocols::T1)
        );
    }

    #[test]
    fn truncated_atr_is_indeterminate_not_a_confident_t0() {
        // Regression: these all used to return a plain Protocols::T0, which
        // resolve_t_protocol then persisted — pinning the wrong protocol for
        // good on a card that is actually T=1.

        // Claims TD1 (Y1=1000) but ends right after T0.
        assert_eq!(
            parse_atr_and_get_protocol("3B81"),
            AtrProtocol::Indeterminate(Protocols::T0)
        );
        // Claims TA1+TB1+TC1+TD1 but only one interface byte follows.
        assert_eq!(
            parse_atr_and_get_protocol("3BFF00"),
            AtrProtocol::Indeterminate(Protocols::T0)
        );
        // TD1 claims TD2 (Y2=1000) but the ATR ends there.
        assert_eq!(
            parse_atr_and_get_protocol("3B8180"),
            AtrProtocol::Indeterminate(Protocols::T0)
        );
    }

    #[test]
    fn absent_or_malformed_atr_is_indeterminate() {
        // No card in the reader, and non-hex garbage.
        assert_eq!(
            parse_atr_and_get_protocol(""),
            AtrProtocol::Indeterminate(Protocols::T0)
        );
        assert_eq!(
            parse_atr_and_get_protocol("3B"),
            AtrProtocol::Indeterminate(Protocols::T0)
        );
        assert_eq!(
            parse_atr_and_get_protocol("nothex"),
            AtrProtocol::Indeterminate(Protocols::T0)
        );
    }

    #[test]
    fn indeterminate_still_offers_a_usable_protocol() {
        // The connection must still be attempted — T0 is the safe default.
        let parsed = parse_atr_and_get_protocol("3B81");
        assert_eq!(parsed.protocol(), Protocols::T0);
        assert!(!parsed.is_resolved(), "a truncated ATR must not be stored");

        let parsed = parse_atr_and_get_protocol("3B8131AABB");
        assert_eq!(parsed.protocol(), Protocols::T1);
        assert!(parsed.is_resolved(), "a complete ATR is safe to store");
    }
}
