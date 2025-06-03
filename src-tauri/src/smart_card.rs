// ───── Std Lib ─────
use std::error::Error;
use std::error::Error as StdError;
use std::ffi::CStr;
use std::mem;
use std::sync::Arc;

// ───── Crates ─────
use log::{debug, error, info, warn};
use once_cell::sync::OnceCell;
use lazy_static::lazy_static;
use rumqttc::v5::AsyncClient;
use tokio::time::Duration;

use tauri::async_runtime::{JoinHandle, Mutex};

// ───── PCSC ─────
use pcsc::*;
use pcsc::{Card, Protocols, State as PcscState};

// ───── Local Modules ─────
use crate::config::{get_from_cache, CacheSection};
use crate::global_app_handle::emit_event;
use crate::mqtt::{ensure_connection, remove_connections_all};

// ───── Constants ─────
const MAX_BUFFER_SIZE: usize = 260; // Example buffer size for smart card communication.

/// Represents a card currently being processed (i.e., connected and active).
#[derive(Debug)]
pub struct ProcessingCard {
    pub client_id: String,              // it is Card number. Uses as client_id for mqtt connection
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
) -> Result<(), Box<dyn Error>> {
    // Remove dead readers.
    fn is_dead(rs: &ReaderState) -> bool {
        rs.event_state().intersects(PcscState::UNKNOWN | PcscState::IGNORE)
    }

    for rs in &*reader_states {
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
            log::info!("Reader {:?} has been connected to the computer", name);
            reader_states.push(ReaderState::new(name, PcscState::UNAWARE));
        }
    }

    // Update the view of the state to wait on.
    for rs in &mut *reader_states {
        rs.sync_current_state();
    }

    Ok(())
}

async fn process_reader_states(
    ctx: &Context,
    reader_states: &mut [ReaderState],
) -> Result<(), SmartCardError> {
    ctx.get_status_change(None, reader_states)?;

    for rs in reader_states {
        if rs.name() != PNP_NOTIFICATION() {
            if is_virtual_reader(rs.name()) {
                log::warn!("Virtual reader {:?} detected. Skipping...", rs.name());
                continue; // Skipping virtual reader processing
            }

            // convert reader name to string
            let reader_name = rs.name(); // .to_str().unwrap(); // convert reader name(&CStr) to string
            let reader_name_string = reader_name.to_str().unwrap();

            // convert ATR to hex string value
            let atr = hex::encode(rs.atr());
            let protocol = parse_atr_and_get_protocol(&atr);
            log::info!("Reader: {:?}. ATR: {}. Protocol: {:?}", reader_name, atr, protocol);        

            /*
                This is a CRUTCH!!! Need to find a better way to convert card_state to string
                The meaning of the card_state is in the pcsc module with the their own state enum.
                The card_state is a bit mask and it is not clear how to convert it to a human readable string properly
            */
            let card_state_string = format!("{:?}", rs.event_state());
            log::debug!("card_state_string {}", card_state_string);

            // If the card state has not 'CHANGED' state, then we skip the processing of this card
            // Due to the specifics of the library, the card can be initialized in several stages,
            // But we only need the final result with the value changed

            // Default card_number var
            let mut card_number: String = String::new();
            let mut iccid: String = String::new();

            // Mechanism that controls the process of adding to TASK_POOL
            let action = should_register_new_card(reader_name_string, &atr).await;

            match action {
                CardProcessingResult::Create => {
                    // The card may not be created initially
                    match ManagedCard::new(reader_name, protocol) {
                        Ok(managed_card) => {
                            match managed_card.get_iccid().await {
                                Ok(received_iccid) => {
                                    log::info!("ICCID: {}", received_iccid);

                                    iccid = received_iccid.clone();
                                    card_number = get_from_cache(CacheSection::Cards, &iccid);

                                    ensure_connection(
                                        rs.name(),
                                        card_number.clone(),
                                        atr.clone(),
                                        managed_card,
                                    ).await;
                                }
                                Err(e) => {
                                    log::error!("Failed to get ICCID: {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            log::error!(
                                "Failed to create ManagedCard for reader {}: {}",
                                reader_name_string,
                                e
                            );
                        }
                    }
                }
                CardProcessingResult::Delete => {
                    // Do nothing
                    log::debug!("CARD DELETED {}", card_state_string);
                }
                CardProcessingResult::Ignore => {
                    // Do nothing
                }
            }

            // Emit event for Create or Delete, but not Ignore
            if action != CardProcessingResult::Ignore {
                emit_event(
                    "global-cards-sync",
                    iccid.into(),
                    reader_name_string.into(),
                    card_state_string.into(),
                    card_number.clone().into(),
                    None,
                    None,
                );

                //  Trace status of the reader & card
                log::info!(
                    "{:?} {:?} {:?}, {:?}",
                    rs.name(),
                    rs.event_state(),
                    atr,
                    card_number
                );
            }
        };
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
    log::debug!("should_register_new_card");
    let mut pool = TASK_POOL.lock().await;

    // Log the current contents of the task pool
    for (i, card) in pool.iter().enumerate() {
        log::debug!(
            "Checking index {}: client_id = {}, reader_name = {:?}, atr = {:?}",
            i,
            card.client_id,
            card.reader_name,
            card.atr
        );
    }

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
        log::debug!("ATR is empty. Checking for stale entries with reader_name = '{}'", reader_name);

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

// Automatically sync cards
pub async fn sc_monitor() -> ! {
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
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        let mut readers_buf = [0; 2048];
        let mut reader_states = vec![
            // Listen for reader insertions/removals, if supported.
            ReaderState::new(PNP_NOTIFICATION(), PcscState::UNAWARE),
        ];

        log::debug!("Initialized readers buffer and reader states.");

        loop {
            log::debug!("Starting the inner loop to monitor reader states...");
            if let Err(e) = setup_reader_states(&ctx, &mut readers_buf, &mut reader_states) {
                log::error!("Failed to setup_reader_states: {:?}", e);
                log::debug!("Exiting inner loop to re-establish context...");
                break; // Exit the inner loop to re-establish context
            }
            log::debug!(
                "Successfully set up reader states: {:?}",
                reader_states
                    .iter()
                    .map(|rs| rs.name().to_string_lossy())
                    .collect::<Vec<_>>()
            );
            
            if let Err(e) =
                process_reader_states(&ctx, &mut reader_states).await
            {
                match e {
                    SmartCardError::UnknownReader => {
                        log::warn!("Detected UnknownReader. Sleeping 3s to avoid busy loop!");
                        tokio::time::sleep(Duration::from_secs(3)).await;
                    }
                    SmartCardError::Other(msg) => {
                        log::error!("SmartCard error: {}", msg);
                    }
                }

                break;
            }

            log::debug!("Waiting for the next status change...");
            tokio::task::yield_now().await;
        }

        log::debug!("Re-establishing context...");
    }
}

/// Parses the ATR and extracts the communication protocol (T=0 or T=1).
///
/// # Arguments
/// - `atr`: A string containing the ATR in hexadecimal format.
///
/// # Returns
/// - `String`: The communication protocol ("T0", "T1", or "Unknown").
pub fn parse_atr_and_get_protocol(atr: &str) -> Protocols {
    let atr_bytes = match hex::decode(atr) {
        Ok(bytes) => bytes,
        Err(_) => {
            log::error!("Invalid ATR format: {}", atr);
            return Protocols::T0;
        }
    };

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
        let proto = td2 & 0x0F;
        return match proto {
            0x00 => Protocols::T0,
            0x01 => Protocols::T1,
            _ => Protocols::T0, // fallback
        };
    }

    // If TD2 is not presented, but TD1 it is — use it
    if let Some(td1) = td1 {
        let proto = td1 & 0x0F;
        return match proto {
            0x00 => Protocols::T0,
            0x01 => Protocols::T1,
            _ => Protocols::T0, // fallback
        };
    }

    // Default value if have no TD1 and TD2
    Protocols::T0
}

// Manual card sync function. ////////////
// This function is used to manually sync cards from anywhere in the program.
// Manually sync cards. Clicking on the button in the frontend will trigger this function

#[tauri::command]
pub async fn manual_sync_cards(
    readername: String,
    restart: bool,
) -> Result<(), String> {
    log::debug!("Manual sync cards function is called. Restart: {}", restart);

    if restart {
        // remove all connections
        remove_connections_all().await;

        return Ok(());
    }

    let ctx = Context::establish(Scope::User).expect("failed to establish context");
    log::debug!("Context established successfully.");

    let mut readers_buf = [0; 2048];
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
    ctx.get_status_change(None, &mut reader_states)
        .expect("failed to get status change");

    for rs in reader_states {
        if rs.name() != PNP_NOTIFICATION() {
            if is_virtual_reader(rs.name()) {
                log::warn!("Virtual reader {:?} detected. Skipping...", rs.name());
                continue; // Skipping virtual reader processing
            }

            // convert reader name to string
            let reader_name = rs.name(); // .to_str().unwrap(); // convert reader name(&CStr) to string
            let reader_name_string = reader_name.to_str().unwrap();

            // convert ATR to hex string value
            let atr = hex::encode(rs.atr());
            let protocol = parse_atr_and_get_protocol(&atr);
            log::info!("Reader: {:?}. ATR: {}. Protocol: {:?}", reader_name, atr, protocol);        

            /*
                This is a CRUTCH!!! Need to find a better way to convert card_state to string
                The meaning of the card_state is in the pcsc module with the their own state enum.
                The card_state is a bit mask and it is not clear how to convert it to a human readable string properly
            */
            let card_state_string = format!("{:?}", rs.event_state());
            log::debug!("card_state_string {}", card_state_string);

            // If the card state has not 'CHANGED' state, then we skip the processing of this card
            // Due to the specifics of the library, the card can be initialized in several stages,
            // But we only need the final result with the value changed

            if readername == reader_name_string {
                match ManagedCard::new(reader_name, protocol) {
                    Ok(managed_card) => {
                        if let Err(e) = managed_card.disconnect().await {
                            log::error!("Failed to disconnect: {}", e);
                        } else {
                            log::info!("Card disconnected: {}", reader_name_string);
                        }
                    }
                    Err(e) => {
                        log::error!(
                            "Failed to disconnect the card for reader {}: {}",
                            reader_name_string,
                            e
                        );
                    }
                }
            }
        };
    }

    Ok(())
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
    pub fn new(reader_name: &CStr, protocol: Protocols) -> Result<Self, Box<dyn StdError + Send + Sync>> {
        info!(
            "ManagedCard::new() called. Reader: '{}', Protocol: {:?}",
            reader_name.to_string_lossy(),
            protocol
        );

        let card = Self::create_card(reader_name, protocol)?;
        info!(
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

    pub fn create_card(reader_name: &CStr, protocol: Protocols) -> Result<Card, Box<dyn StdError + Send + Sync>> {
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

    pub async fn reconnect(&self) {
        debug!(
            "Attempting to reconnect card for reader: {}",
            self.reader_name.to_string_lossy()
        );

        let mut card = self.inner.lock().await;

        match card.reconnect(ShareMode::Shared, Protocols::ANY, Disposition::ResetCard) {
            Ok(_) => {
                info!(
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

    pub async fn recreate(&self) -> Result<(), Box<dyn StdError + Send + Sync>> {
        let new_card = Self::create_card(&self.reader_name, self.protocol)?;
        let mut lock = self.inner.lock().await;
        *lock = new_card;

        info!(
            "Successfully recreated card object for reader: {}",
            self.reader_name.to_string_lossy()
        );

        Ok(())
    }

    
    pub async fn disconnect(&self) -> Result<(), Box<dyn StdError + Send + Sync>> {
        let mut guard = self.inner.lock().await;

        let dummy_card = mem::replace(
            &mut *guard,
            Context::establish(Scope::User)?
                .connect(&self.reader_name, ShareMode::Shared, self.protocol)?
        );

        dummy_card
            .disconnect(pcsc::Disposition::ResetCard)
            .map_err(|(_, err)| Box::new(err) as _)
    }

    pub async fn apdu_transmit(&self, apdu_hex: &str) -> Result<String, Box<dyn StdError + Send + Sync>> {
        use crate::smart_card::MAX_BUFFER_SIZE;

        debug!(
            "apdu_transmit() called for reader: {} with APDU HEX: {}",
            self.reader_name.to_string_lossy(),
            apdu_hex
        );

        let apdu = match hex::decode(apdu_hex) {
            Ok(data) => {
                debug!("APDU decoded successfully: {:?}", data);
                data
            }
            Err(err) => {
                error!("Failed to decode APDU '{}': {}", apdu_hex, err);
                return Err(format!("Decode error: {}", err).into());
            }
        };

        let card = Arc::clone(&self.inner);
        let apdu_cloned = apdu.clone();

        debug!(
            "Cloned card for blocking transmission. Sending to spawn_blocking..."
        );

        let response = tauri::async_runtime::spawn_blocking(move || {
            debug!("Entered spawn_blocking thread. Preparing buffer and locking card...");

            let mut rapdu_buf = [0u8; MAX_BUFFER_SIZE];

            let locked = card.blocking_lock();
            debug!("Lock acquired. Transmitting...");

            match locked.transmit(&apdu_cloned, &mut rapdu_buf) {
                Ok(response) => {
                    let encoded = hex::encode(response);
                    debug!("APDU transmit success. Encoded response: {}", encoded);
                    Ok(encoded)
                }
                Err(err) => {
                    error!("APDU transmit failed: {}", err);
                    Err(format!("Transmit error: {}", err))
                }
            }
        })
        .await??;

        debug!(
            "apdu_transmit() complete for reader: {}. Final response: {}",
            self.reader_name.to_string_lossy(),
            response
        );

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
            return "6F00".to_string();
        }

        // Seccond attempt
        match self.apdu_transmit(apdu_hex).await {
            Ok(response) => {
                info!(
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
                "6F00".to_string()
            }
        }
    }
        
    /// Returns the card ICCID using lazy caching.
    /// On first call, reads it from the card; subsequent calls return the cached value.
    pub async fn get_iccid(&self) -> Result<String, Box<dyn StdError + Send + Sync>> {
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

        // SELECT EF ICC (2FE2)
        let select_result = self.apdu_transmit("00A4020C020002").await?;

        if !select_result.ends_with("9000") {
            log::warn!("SELECT EF ICC returned unexpected status: {}", select_result);
        }
        
        // READ BINARY (10 байт)
        let read_response = self.apdu_transmit("00B0000108").await?;

        let hex_data = read_response.strip_suffix("9000").unwrap_or(&read_response);

        let bytes = hex::decode(hex_data)
            .map_err(|e| format!("Failed to decode ICCID hex: {}", e))?;

        let iccid = bytes.iter().map(|b| format!("{:02X}", b)).collect::<String>();

        log::debug!("Final ICCID: {}", iccid);

        // Save ICCID, not got earlier
        let _ = self.iccid.set(iccid.clone());

        Ok(iccid)
    }

}
