//! Cards in PC/SC readers.
//!
//! A monitor thread watches the readers through PC/SC (`monitor`); every
//! reader event runs a sweep (`sweep`) that registers an inserted card — reads
//! its ICCID, applies the configured T protocol and hands it to its MQTT
//! session (`mqtt::ensure_connection`) — or ends the session of a card that is
//! gone. A registered card is driven through the `ManagedCard` handle (`card`),
//! which runs the APDUs of the server against the card.
//!
//! PC/SC is a synchronous API whose calls cannot be interrupted: every call on
//! a card runs on the blocking pool, and a call that outlived its caller is
//! tracked per reader (`reader_io`) so that nothing queues behind it.
//!
//! Layout:
//! - `monitor` — the reader monitor thread (`sc_monitor`) and rescans
//! - `sweep` — one pass over the reader states: registering and evicting cards
//! - `card` — the `ManagedCard` handle: APDU exchange, reset, card checks
//! - `reader_io` — PC/SC calls still running per reader
//! - `protocol` — the T protocol: ATR parsing and the configured value

mod card;
mod monitor;
mod protocol;
mod reader_io;
mod sweep;

use std::error::Error as StdError;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lazy_static::lazy_static;
use log::{debug, error, warn};
use pcsc::{Context, ReaderState, Scope, State as PcscState, PNP_NOTIFICATION};
use rumqttc::v5::AsyncClient;
use tauri::async_runtime::{block_on, JoinHandle, Mutex};

use crate::mqtt::remove_connections_all;

use monitor::{list_reader_names, setup_reader_states};
use sweep::{forget_unserved_probes, process_reader_states};

// Re-exported for the rest of the app: these are the only entry points.
pub use card::ManagedCard;
pub use monitor::{request_rescan, sc_monitor};
pub use protocol::{protocol_from_str, protocol_to_str};
pub(crate) use reader_io::reader_io_pending;

/// How long the manual sync waits for a reader status change before it
/// sweeps the readers as they are.
const MANUAL_SYNC_TIMEOUT_SECS: u64 = 1;

/// Status word `mqtt::card_session` substitutes on the wire when `send_apdu` returns a
/// transport error (the APDU could not be run against the card at all). Never
/// used to classify a reply — a card can legitimately answer `6F00` itself,
/// and the two cases are told apart by `send_apdu`'s `Result`, not by bytes.
pub(crate) const SW_TECHNICAL_PROBLEM: &str = "6F00";

type DynError = Box<dyn StdError + Send + Sync>;
type DynResult<T> = Result<T, DynError>;

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
    pub mqtt_client: AsyncClient,    // MQTT client instance.
    pub task_handle: JoinHandle<()>, // Async task handle managing communication for this card.
}

lazy_static! {
    /// Global list of cards currently being processed (i.e., connected and active).
    pub static ref TASK_POOL: Arc<Mutex<Vec<ProcessingCard>>> =
        Arc::new(Mutex::new(Vec::new()));
}

/// Poison-tolerant lock for the plain bookkeeping maps of this module: a
/// panic in one sweep must not turn every later sweep into a panic too.
fn lock_map<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Manually syncs the cards, from the frontend or from anywhere in the
/// backend.
///
/// With `restart` (the settings dialog, `app_connect::reconnect_everything`),
/// every card session is ended and the monitor rescans, so the still-inserted
/// cards are registered again with fresh config. Without it (a card removed
/// from the config on Linux), the readers are swept once, as they are, by
/// this command itself.
///
/// The command lives here rather than in `sweep`: the handler macros that
/// `#[tauri::command]` generates must resolve at `smart_card::`, the path
/// `lib.rs` registers it under.
#[tauri::command]
pub async fn manual_sync_cards(_readername: String, restart: bool) -> Result<(), String> {
    debug!("Manual sync cards function is called. Restart: {}", restart);

    if restart {
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
        debug!("Context established successfully.");

        // A failed enumeration is reported to the caller: returning Ok here
        // told the frontend the sync succeeded while nothing had been swept,
        // so a broken PCSC stack looked like an empty reader list.
        let reader_count = list_reader_names(&ctx)
            .map_err(|e| format!("failed to list readers: {e}"))?
            .len();
        if reader_count == 0 {
            warn!("No readers found. Nothing to sync.");
            return Ok(());
        }
        debug!("Available readers found: {}", reader_count);

        let mut reader_states = vec![
            // Listen for reader insertions/removals, if supported.
            ReaderState::new(PNP_NOTIFICATION(), PcscState::UNAWARE),
        ];

        // setup readers states. Getting changes and other inits
        if let Err(e) = setup_reader_states(&ctx, &mut reader_states) {
            error!(
                "Failed to list readers while updating manual sync states: {:?}",
                e
            );
        }
        // Waiting for the status change. A timeout is the normal outcome when
        // nothing changed since the states were seeded, and it must NOT skip
        // the sweep below — treating it as fatal made the manual sync a no-op
        // in exactly its main use case (a card already sitting in the reader).
        // The monitor loop makes the same distinction.
        match ctx.get_status_change(
            Some(Duration::from_secs(MANUAL_SYNC_TIMEOUT_SECS)),
            &mut reader_states,
        ) {
            Ok(()) | Err(pcsc::Error::Timeout) => {}
            Err(e) => return Err(format!("failed to get status change: {e}")),
        }

        // An explicit sync request re-probes cards found unserved before.
        forget_unserved_probes();
        block_on(process_reader_states(&mut reader_states));
        Ok(())
    })
    .await
    .map_err(|e| format!("manual_sync_cards: blocking task failed: {e}"))?
}
