// ───── Std Lib ─────
use std::error::Error as StdError;
use std::ffi::CStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ───── Crates ─────
use log::{debug, error, info, warn};
use once_cell::sync::OnceCell;
use lazy_static::lazy_static;
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
const MAX_BUFFER_SIZE: usize = 260; // Buffer size for smart card communication.
const READERS_BUFFER_SIZE: usize = 2048;
const MANUAL_SYNC_TIMEOUT_SECS: u64 = 1;
const SW_TECHNICAL_PROBLEM: &str = "6F00";
/// Upper bound for one `get_status_change` wait in the monitor loop. A bounded
/// wait (instead of an infinite one) lets the loop notice a rescan request
/// even if the `cancel()` in `request_rescan` raced past a non-blocked monitor.
const MONITOR_POLL_TIMEOUT_SECS: u64 = 30;

type DynError = Box<dyn StdError + Send + Sync>;
type DynResult<T> = Result<T, DynError>;

/// Represents a card currently being processed (i.e., connected and active).
#[derive(Debug)]
pub struct ProcessingCard {
    pub client_id: String,              // It is Card number. Uses as client_id for mqtt connection
    pub reader_name: Option<String>,    // Name of the smart card reader (e.g., "Alcor Micro AU9540 00 00").
    pub atr: Option<String>,            // ATR of the inserted card (hex-encoded).
    #[allow(dead_code)] // to say the compiler does not warn about an unused field that is used in another file.
    pub mqtt_client: AsyncClient,       // MQTT client instance.
    pub task_handle: JoinHandle<()>,    // Async task handle managing communication for this card.
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

/// Represents errors that can occur while interacting with smart card readers.
#[derive(Debug)] // Enables use of `{:?}` for logging and debugging
pub enum SmartCardError {
    /// Error indicating that the specified reader is no longer available or not recognized.
    UnknownReader,

    /// A catch-all for other types of errors, represented as a string message.
    Other(String),
}

impl std::fmt::Display for SmartCardError {
    /// Provides a user-friendly string representation of the error.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SmartCardError::UnknownReader => write!(f, "UnknownReader"),
            SmartCardError::Other(s) => write!(f, "Other: {}", s),
        }
    }
}

impl std::error::Error for SmartCardError {
    // This enables interoperability with other error-handling APIs,
    // such as `?` operator, logging, and integration with `anyhow` or `thiserror`.
}

impl From<pcsc::Error> for SmartCardError {
    /// Converts a `pcsc::Error` into a `SmartCardError`.
    ///
    /// Attempts to classify `UnknownReader` errors specifically,
    /// all other errors are wrapped in `SmartCardError::Other`.
    fn from(err: pcsc::Error) -> Self {
        if err.to_string().contains("UnknownReader") {
            SmartCardError::UnknownReader
        } else {
            SmartCardError::Other(err.to_string())
        }
    }
}

fn setup_reader_states(
    ctx: &Context,
    readers_buf: &mut [u8],
    reader_states: &mut Vec<ReaderState>,
) -> DynResult<()> {
    // Remove dead readers.
    fn is_dead(rs: &ReaderState) -> bool {
        rs.event_state().intersects(PcscState::UNKNOWN | PcscState::IGNORE)
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

async fn process_reader_states(reader_states: &mut [ReaderState]) -> Result<(), SmartCardError> {
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
            log::warn!("Reader name is not valid UTF-8: {:?}. Skipping...", reader_name);
            continue;
        };

        let atr = hex::encode(rs.atr());
        let protocol = parse_atr_and_get_protocol(&atr);
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
                        effective_protocol = resolve_t_protocol(&card_number, protocol);
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
                iccid.into(),
                reader_name_string.into(),
                card_state_string.into(),
                card_number.clone().into(),
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

    Ok(())
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
            c.reader_name.as_deref() == Some(reader_name) &&
            c.atr.as_deref() == Some(atr)
        });

        if !exists {
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
            removed.task_handle.abort();
            log::warn!(
                "Removed stale ProcessingCard for reader {} with old ATR {}",
                removed.reader_name.as_deref().unwrap_or("unknown"),
                removed.atr.as_deref().unwrap_or("unknown"),
            );
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

/// Guards against starting more than one smart-card monitor. The
/// `frontend-loaded` event in `lib.rs` can fire several times at startup,
/// which would otherwise spawn duplicate monitors (duplicate PCSC contexts
/// and doubled APDU traffic to the same card).
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

    loop {
        log::debug!("Starting the outer loop to establish context...");
        let ctx = match Context::establish(Scope::User) {
            Ok(ctx) => {
                log::debug!("Successfully established context.");
                ctx
            }
            Err(e) => {
                log::error!(
                    "Failed to establish context: {:?}. Retrying in 5 seconds...",
                    e
                );
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };

        // Publish the context so request_rescan() can cancel a blocked
        // get_status_change from another thread.
        {
            let mut slot = match MONITOR_CTX.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            *slot = Some(ctx.clone());
        }

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
                        log::info!("Rescan requested (picked up on poll timeout). Re-establishing context...");
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
                    log::info!("Rescan requested (get_status_change cancelled). Re-establishing context...");
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
            if let Err(e) = block_on(process_reader_states(&mut reader_states)) {
                match e {
                    SmartCardError::UnknownReader => {
                        log::warn!("Detected UnknownReader. Sleeping 3s to avoid busy loop!");
                        std::thread::sleep(Duration::from_secs(3));
                    }
                    SmartCardError::Other(msg) => {
                        log::error!("SmartCard error: {}", msg);
                        // Without a small backoff this branch could spin the
                        // outer reconnect loop very quickly under persistent
                        // PCSC failures.
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }

                break;
            }

            log::debug!("Waiting for the next status change...");
        }

        log::debug!("Re-establishing context...");
    }
}

/// Renders a PCSC protocol as the config string form ("T0"/"T1").
pub fn protocol_to_str(protocol: Protocols) -> &'static str {
    if protocol == Protocols::T1 { "T1" } else { "T0" }
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
fn resolve_t_protocol(card_number: &str, atr_protocol: Protocols) -> Protocols {
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
            let value = protocol_to_str(atr_protocol);
            if mutate_card_config(card_number, |card| {
                card.t_protocol = Some(value.to_string());
                true
            }) {
                info!("Card {}: stored ATR-derived T protocol {} in config", card_number, value);
            }
            atr_protocol
        }
    }
}

/// Parses the ATR and extracts the communication protocol (T=0 or T=1).
///
/// # Arguments
/// - `atr`: A string containing the ATR in hexadecimal format.
///
/// # Returns
/// - `Protocols`: The communication protocol (T0 or T1; T0 when the ATR is absent or malformed).
pub fn parse_atr_and_get_protocol(atr: &str) -> Protocols {
    fn protocol_from_td(td: u8) -> Protocols {
        match td & 0x0F {
            0x00 => Protocols::T0,
            0x01 => Protocols::T1,
            _ => Protocols::T0,
        }
    }

    let atr_bytes = match hex::decode(atr) {
        Ok(bytes) => bytes,
        Err(_) => {
            log::error!("Invalid ATR format: {}", atr);
            return Protocols::T0;
        }
    };

    // An empty ATR is the normal "no card in the reader" case (e.g. a card
    // removal event) — nothing to parse, and not worth a warning.
    if atr_bytes.is_empty() {
        return Protocols::T0;
    }

    if atr_bytes.len() < 2 {
        log::warn!("ATR is too short: {:?}", atr_bytes);
        return Protocols::T0;
    }

    let mut index = 1;
    let y1 = atr_bytes[index] >> 4;
    index += 1;

    // Skip TA1, TB1, TC1 depends on Y1
    if y1 & 0x1 != 0 { index += 1; } // TA1
    if y1 & 0x2 != 0 { index += 1; } // TB1
    if y1 & 0x4 != 0 { index += 1; } // TC1

    // TD1
    let td1 = if y1 & 0x8 != 0 && index < atr_bytes.len() {
        let td1 = atr_bytes[index];
        index += 1;
        Some(td1)
    } else {
        None
    };

    // TD2 (if was TD1)
    let td2 = if let Some(td1) = td1 {
        let y2 = td1 >> 4;
        // Skip TA2, TB2, TC2
        if y2 & 0x1 != 0 { index += 1; } // TA2
        if y2 & 0x2 != 0 { index += 1; } // TB2
        if y2 & 0x4 != 0 { index += 1; } // TC2

        if y2 & 0x8 != 0 && index < atr_bytes.len() {
            Some(atr_bytes[index])
        } else {
            None
        }
    } else {
        None
    };

    // If TD2 exists — it is default protocol
    if let Some(td2) = td2 {
        return protocol_from_td(td2);
    }

    // If TD2 is not presented, but TD1 it is — use it
    if let Some(td1) = td1 {
        return protocol_from_td(td1);
    }

    // Default value if have no TD1 and TD2
    Protocols::T0
}

// Manual card sync function.
// This function is used to manually sync cards from anywhere in the program.
// Manually sync cards. Clicking on the button in the frontend will trigger this function
#[tauri::command]
pub async fn manual_sync_cards(
    _readername: String,
    restart: bool,
) -> Result<(), String> {
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
        ctx.get_status_change(Some(Duration::from_secs(MANUAL_SYNC_TIMEOUT_SECS)), &mut reader_states)
            .map_err(|e| format!("failed to get status change: {e}"))?;

        block_on(process_reader_states(&mut reader_states))
            .map_err(|e| format!("Processing failed: {}", e))
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
        let ctx = Context::establish(Scope::User)
            .map_err(|err| {
                log::error!("Failed to establish context: {}", err);
                Box::<dyn StdError + Send + Sync>::from(err)
            })?;

        let card = ctx.connect(reader_name, ShareMode::Shared, protocol)
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

    // pub async fn disconnect(&self) -> Result<(), Box<dyn StdError + Send + Sync>> {
    //     let mut guard = self.inner.lock().await;

    //     let dummy_card = mem::replace(
    //         &mut *guard,
    //         Context::establish(Scope::User)?
    //             .connect(&self.reader_name, ShareMode::Shared, self.protocol)?
    //     );

    //     #[cfg(target_os = "linux")]
    //     {
    //         log::debug!("Linux-specific disconnect logic started.");

    //         // force trigger status update
    //         let mut reader_states = vec![
    //             pcsc::ReaderState::new(self.reader_name.as_ref(), pcsc::State::UNAWARE)
    //         ];

    //         match Context::establish(Scope::User)?.get_status_change(Some(Duration::from_millis(1)), &mut reader_states) {
    //             Ok(_) => log::debug!("Status change triggered successfully for {}", self.reader_name.to_string_lossy()),
    //             Err(e) => log::warn!("get_status_change failed on Linux: {}", e),
    //         }

    //         return Ok(());
    //     }

    //     #[cfg(target_os = "macos")]
    //     {
    //         return dummy_card
    //             .disconnect(pcsc::Disposition::ResetCard)
    //             .map_err(|(_, err)| Box::new(err) as _);
    //     }

    //     #[cfg(target_os = "windows")]
    //     {
    //         return dummy_card
    //             .disconnect(pcsc::Disposition::ResetCard)
    //             .map_err(|(_, err)| Box::new(err) as _);
    //     }
    // }

    pub async fn apdu_transmit(&self, apdu_hex: &str) -> DynResult<String> {
        let apdu = match hex::decode(apdu_hex) {
            Ok(data) => data,
            Err(err) => {
                error!("Failed to decode APDU '{}': {}", apdu_hex, err);
                return Err(format!("Decode error: {}", err).into());
            }
        };

        let card = Arc::clone(&self.inner);

        // The send/response pair is logged by send_apdu() with the client_id
        // header; this layer only logs failures.
        let response = tauri::async_runtime::spawn_blocking(move || {
            let mut rapdu_buf = [0u8; MAX_BUFFER_SIZE];

            let locked = card.blocking_lock();

            match locked.transmit(&apdu, &mut rapdu_buf) {
                Ok(response) => Ok(hex::encode(response)),
                Err(err) => {
                    error!("APDU transmit failed: {}", err);
                    Err(format!("Transmit error: {}", err))
                }
            }
        })
        .await??;

        Ok(response)
    }

    pub async fn send_apdu(
        &self,
        apdu_hex: &str,
        client_id: &str,
    ) -> String {
        debug!("{} Sending APDU command: {}", client_id, apdu_hex);

        // First attempt
        match self.apdu_transmit(apdu_hex).await {
            Ok(response) => {
                debug!("{} APDU response: {:?}", client_id, response);
                return response;
            }
            Err(err) => {
                error!(
                    "{} Failed to send APDU: {}. Attempting to recreate card...",
                    client_id,
                    err
                );
            }
        }

        // recreate attempt
        if let Err(e) = self.recreate().await {
            error!(
                "{} Failed to recreate card after APDU failure: {}",
                client_id,
                e
            );
            return SW_TECHNICAL_PROBLEM.to_string();
        }

        // Seccond attempt
        match self.apdu_transmit(apdu_hex).await {
            Ok(response) => {
                debug!(
                    "{} APDU response (after recreate): {:?}",
                    client_id,
                    response
                );
                response
            }
            Err(retry_err) => {
                error!(
                    "{} Retry failed: could not send APDU after recreate: {}",
                    client_id,
                    retry_err
                );
                SW_TECHNICAL_PROBLEM.to_string()
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
            return Err(
                format!("SELECT EF_ICC failed with status: {}", select_result).into()
            );
        }

        // READ BINARY: 8 bytes of cardExtendedSerialNumber at offset 1 (skipping clockStop)
        let read_response = self.apdu_transmit("00B0000108").await?;

        let Some(hex_data) = read_response.strip_suffix("9000") else {
            return Err(
                format!("READ BINARY EF_ICC returned unexpected status: {}", read_response).into()
            );
        };

        let bytes = hex::decode(hex_data)
            .map_err(|e| format!("Failed to decode ICCID hex: {}", e))?;

        if bytes.len() != 8 {
            return Err(
                format!("EF_ICC READ BINARY returned {} bytes, expected 8", bytes.len()).into()
            );
        }

        let iccid = bytes.iter().map(|b| format!("{:02X}", b)).collect::<String>();

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
}
