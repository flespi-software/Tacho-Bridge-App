use std::error::Error;
use std::error::Error as StdError;
use std::ffi::CStr;
use std::sync::Arc;
use std::mem;

use pcsc::*; // Importing pcsc module for smart card reader operations.

use tauri::async_runtime::JoinHandle; // Async runtime join handles for managing async tasks in Tauri.
use tauri::async_runtime::Mutex;
use tokio::sync::MutexGuard;
use tokio::sync::watch;

use pcsc::State as PcscState;
use pcsc::{Card, Protocols};
// use tauri::Manager; // Tauri application manager for app lifecycle and window management. // There is a Mutex implementation for the standard from the std lib, but it blocks the current thread and is not integrated with the Tauri async framework we are using, so we will use what is intended: Tauri mutex.

use once_cell::sync::OnceCell;

use tokio::sync::watch::Sender;

use hex::{decode, encode}; // Hexadecimal encoding and decoding utilities.

// Importing specific functionality from local modules
use crate::config::get_from_cache; // Function to get data from cache for syncing cards.
use crate::config::CacheSection;
use crate::global_app_handle::emit_event;
// Enum for cache sections for getting data from cache.
use crate::mqtt::{ensure_connection, remove_connections, remove_connections_all}; // MQTT module functions for managing connections with the readers.
// use crate::mqtt::{remove_connections, remove_connections_all}; // MQTT module functions for managing connections with the readers.

use crate::app_connect; // Application connection to the MQTT broker.

// import set for async task_pool under mutex
use lazy_static::lazy_static; // Importing the lazy_static macro
use rumqttc::v5::AsyncClient;

use log::{info, debug, error, trace, warn};

const MAX_BUFFER_SIZE: usize = 260; // Example buffer size for smart card communication.
lazy_static! {
    /// Global static vector to store active MQTT client connections and their associated tasks.
    ///
    /// This vector is protected by a `Mutex` to ensure that only one task can modify it at a time,
    /// preventing data races and ensuring thread safety in an asynchronous environment.
    ///
    /// The `TASK_POOL` is an `Arc` (Atomic Reference Counted) pointer, which allows it to be shared
    /// safely among multiple tasks. Each task can clone the `Arc`, increasing the reference count,
    /// and decrement it when done, ensuring the memory is cleaned up when no longer in use.
    ///
    /// The vector stores tuples of three elements:
    /// - `String`: The client ID, a unique identifier for each MQTT client connection.
    /// - `AsyncClient`: The MQTT client instance, which handles the actual communication with the MQTT broker.
    /// - `JoinHandle<usize>`: A handle to the asynchronous task associated with this client. The task runs in the
    ///    background, handling incoming MQTT messages and other asynchronous operations.
    pub static ref TASK_POOL: Arc<Mutex<Vec<(String, AsyncClient, JoinHandle<()>)>>> = Arc::new(Mutex::new(Vec::new()));
}

// Тип для reader_cards_pool
pub type SharedReaderCardsPool = Vec<(String, String, String)>;
pub type SharedReaderCardsPoolReceiver = watch::Receiver<SharedReaderCardsPool>;


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
    reader_cards_pool: &mut Vec<(String, String, String)>,
) -> Result<(), Box<dyn Error>> {
    match ctx.get_status_change(None, reader_states) {
        Ok(_) => {}
        Err(e) => {
            log::error!("Failed to get reader status change: {:?}", e);
            return Err(Box::new(e));
        }
    }

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

            // 'PRESENT' ensures that the card is in the reader and accessible
            // if rs.event_state().contains(PcscState::PRESENT) {
            if rs.event_state().contains(PcscState::PRESENT) && !rs.event_state().contains(PcscState::INUSE) {
                if !is_card_connected(reader_cards_pool, reader_name_string) {
                    // The card may not be created initially
                    match ManagedCard::new(reader_name, protocol) {
                        Ok(managed_card) => {
                            match managed_card.get_iccid().await {
                                Ok(received_iccid) => {
                                    log::info!("ICCID: {}", received_iccid);

                                    // Save the ICCID to an external variable
                                    iccid = received_iccid.clone();

                                    // Checking if card number is in the cache
                                    card_number = get_from_cache(CacheSection::Cards, &iccid);

                                    // Only if the map and ICCID are received successfully - run the task
                                    ensure_connection(
                                        rs.name(),
                                        card_number.clone(),
                                        atr.clone(),
                                        managed_card,
                                    ).await;
                                }
                                Err(e) => {
                                    log::error!("Failed to get ICCID: {}", e);
                                    log::warn!(
                                        "Card for reader {} failed to return ICCID. Will not start connection.",
                                        reader_name_string
                                    );
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
            }

            //  Trace status of the reader & card
            log::info!(
                "{:?} {:?} {:?}, {:?}",
                rs.name(),
                rs.event_state(),
                atr,
                card_number
            );

            let cards_to_remove = reader_cards_pool_update(
                reader_cards_pool,
                reader_name_string,
                &card_state_string,
                &card_number,
            );
            remove_connections(cards_to_remove).await;

            // INUSE state is a temporary workaround, because after the map is initialized, when the ICCID is read, the context detects a change in the map's behavior
            // and sends another event that is not needed and spoils the correct sequence of sending events. Will be fixed later.
            if ! rs.event_state().contains(PcscState::INUSE) {
                // send an event to the frontend to update the state of the card
                emit_event(
                    "global-cards-sync",
                    iccid.into(),
                    reader_name_string.into(),
                    card_state_string.into(),
                    card_number.clone().into(),
                    None,
                    None,
                );
            }
        };
    }

    Ok(())
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

pub fn is_card_connected(
    reader_cards_pool: &Vec<(String, String, String)>, 
    reader_name: &str,
) -> bool {
    debug!(
        "Checking if card is connected for reader: '{}'. Pool size: {}",
        reader_name,
        reader_cards_pool.len()
    );

    for (name, state, card) in reader_cards_pool {
        debug!(
            "Checking pool entry -> Reader: '{}', State: '{}', Card: '{}'",
            name, state, card
        );
    }

    let connected = reader_cards_pool.iter().any(|(name, _, _)| name == reader_name);

    debug!(
        "Result of is_card_connected for reader '{}': {}",
        reader_name,
        connected
    );

    connected
}

// Automatically sync cards
pub async fn sc_monitor(mut pool_rx: SharedReaderCardsPoolReceiver) -> ! {
    let mut reader_cards_pool: SharedReaderCardsPool = pool_rx.borrow().clone();

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
            if pool_rx.has_changed().unwrap_or(false) {
                match pool_rx.borrow_and_update().clone() {
                    updated_pool => {
                        log::info!("Received updated reader_cards_pool via channel.");
                        reader_cards_pool = updated_pool;
                    }
                }
            }

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
                process_reader_states(&ctx, &mut reader_states, &mut reader_cards_pool).await
            {
                log::error!("Failed to process reader states: {:?}", e);
                log::debug!("Exiting inner loop to re-establish context...");
                break; // Exit the inner loop to re-establish context
            }
            log::debug!(
                "Successfully processed reader states. Current reader_cards_pool: {:?}",
                reader_cards_pool
            );

            log::debug!("Waiting for the next status change...");
            tokio::task::yield_now().await;
        }

        log::debug!("Re-establishing context...");
    }
}

pub fn reader_cards_pool_update(
    reader_cards_pool: &mut Vec<(String, String, String)>,
    reader_name: &str,
    card_state: &str,
    card_number: &str,
) -> Vec<String> {
    let mut company_card_numbers = Vec::new();

    println!(
        "Updating reader cards pool. Reader name: '{}', Card state: '{}', Card number: '{}'",
        reader_name, card_state, card_number
    );

    if !reader_name.is_empty() && !card_number.is_empty() {
        let exists = reader_cards_pool
            .iter()
            .any(|(reader, _, _)| reader == reader_name);
        if !exists {
            println!(
                "Reader '{}' does not exist in the pool. Adding new entry.",
                reader_name
            );
            reader_cards_pool.push((
                reader_name.to_string(),
                card_state.to_string(),
                card_number.to_string(),
            ));
        }
    } else if !reader_name.is_empty()
        && card_number.is_empty()
        && (card_state.contains("EMPTY") || card_state.contains("UNKNOWN"))
    {
        let entries_to_remove: Vec<_> = reader_cards_pool
            .iter()
            .enumerate()
            .filter(|(_, (reader, _, _))| reader == reader_name)
            .map(|(i, (_, _, card))| {
                company_card_numbers.push(card.clone());
                i
            })
            .collect();

        if !entries_to_remove.is_empty() {
            println!(
                "Removing {} entries for reader '{}'.",
                entries_to_remove.len(),
                reader_name
            );
            for i in entries_to_remove.into_iter().rev() {
                reader_cards_pool.remove(i);
            }
        }
    } else {
        println!(
            "Reader name is empty or state not EMPTY/UNKNOWN. No action taken."
        );
    }

    info!("Final state of reader_cards_pool: {:?}", reader_cards_pool);

    company_card_numbers
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
        log::error!("ATR is too short: {:?}", atr_bytes);
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
    pool_tx: tauri::State<'_, Sender<SharedReaderCardsPool>>,
) -> Result<(), String> {
    log::debug!("Manual sync cards function is called. Restart: {}", restart);

    if restart {
        // remove all connections
        remove_connections_all().await;

        // Send empty vector to the channel
        if let Err(e) = pool_tx.send(vec![]) {
            log::error!("Failed to clear reader_cards_pool: {}", e);
        } else {
            log::info!("Cleared reader_cards_pool via watch channel.");
        }

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
        trace!(
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

        let mut dummy_card = mem::replace(
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

        trace!(
            "apdu_transmit() called for reader: {} with APDU HEX: {}",
            self.reader_name.to_string_lossy(),
            apdu_hex
        );

        let apdu = match hex::decode(apdu_hex) {
            Ok(data) => {
                trace!("APDU decoded successfully: {:?}", data);
                data
            }
            Err(err) => {
                error!("Failed to decode APDU '{}': {}", apdu_hex, err);
                return Err(format!("Decode error: {}", err).into());
            }
        };

        let card = Arc::clone(&self.inner);
        let apdu_cloned = apdu.clone();

        trace!(
            "Cloned card for blocking transmission. Sending to spawn_blocking..."
        );

        let response = tauri::async_runtime::spawn_blocking(move || {
            debug!("Entered spawn_blocking thread. Preparing buffer and locking card...");

            let mut rapdu_buf = [0u8; MAX_BUFFER_SIZE];

            let mut locked = card.blocking_lock();
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

        trace!(
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