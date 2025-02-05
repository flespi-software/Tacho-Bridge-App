use std::error::Error;
use std::error::Error as StdError;
use std::ffi::CStr;
use std::sync::Arc;

use pcsc::*; // Importing pcsc module for smart card reader operations.

use tauri::async_runtime::JoinHandle; // Async runtime join handles for managing async tasks in Tauri.
use tauri::async_runtime::Mutex;
// use tauri::Manager; // Tauri application manager for app lifecycle and window management. // There is a Mutex implementation for the standard from the std lib, but it blocks the current thread and is not integrated with the Tauri async framework we are using, so we will use what is intended: Tauri mutex.

use hex::{decode, encode}; // Hexadecimal encoding and decoding utilities.

// Importing specific functionality from local modules
use crate::config::get_from_cache; // Function to get data from cache for syncing cards.
use crate::config::CacheSection;
use crate::global_app_handle::emit_event;
// Enum for cache sections for getting data from cache.
use crate::mqtt::{ensure_connection, remove_connections, remove_connections_all}; // MQTT module functions for managing connections with the readers.

// import set for async task_pool under mutex
use lazy_static::lazy_static; // Importing the lazy_static macro
use rumqttc::v5::AsyncClient;

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

/// Represents the state of a tachograph card.
///
/// This structure holds information about a tachograph card currently being
/// interacted with through a smart card reader.
///
/// # Fields
///
/// * `atr` - A string representing the Answer To Reset (ATR) of the card. The ATR is a sequence
///   of bytes returned by the card upon reset, identifying the card's communication parameters.
/// * `reader_name` - The name of the smart card reader through which the card is being accessed.
/// * `card_state` - A string describing the current state of the card (e.g., "Inserted", "Removed").
/// * `card_number` - The identification number of the tachograph card.
#[derive(Clone, serde::Serialize)]
pub struct TachoState {
    pub atr: String,
    pub reader_name: String,
    pub card_state: String,
    pub card_number: String,
    pub online: Option<bool>,
    pub authentication: Option<bool>,
}

fn setup_reader_states(
    ctx: &Context,
    readers_buf: &mut [u8],
    reader_states: &mut Vec<ReaderState>,
) -> Result<(), Box<dyn Error>> {
    // Remove dead readers.
    fn is_dead(rs: &ReaderState) -> bool {
        rs.event_state().intersects(State::UNKNOWN | State::IGNORE)
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
            reader_states.push(ReaderState::new(name, State::UNAWARE));
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
        Ok(status) => status,
        Err(e) => {
            log::error!("Failed to get reader status change: {:?}", e);
        }
    }

    for rs in reader_states {
        if rs.name() != PNP_NOTIFICATION() {
            // convert ATR to hex string value
            let atr = hex::encode(rs.atr());
            // Checking if card number is in the cache
            let card_number = get_from_cache(CacheSection::Cards, &atr);
            let card_number_clone = card_number.clone();

            // convert reader name to string
            let reader_name_string: &str = rs.name().to_str().unwrap(); // convert reader name(&CStr) to string
            /*
                This is a CRUTCH!!! Need to find a better way to convert card_state to string
                The meaning of the card_state is in the pcsc module with the their own state enum.
                The card_state is a bit mask and it is not clear how to convert it to a human readable string properly
            */
            let card_state_string = format!("{:?}", rs.event_state());

            // If the card state has not 'CHANGED' state, then we skip the processing of this card
            // Due to the specifics of the library, the map can be initialized in several stages,
            // But we only need the final result with the value changed
            if !card_state_string.contains("CHANGED") {
                continue;
            }

            //  Trace status of the reader & card
            log::info!(
                "{:?} {:?} {:?}, {:?}",
                rs.name(),
                rs.event_state(),
                atr,
                card_number
            );

            // launches async task with a card and mqtt connection.
            ensure_connection(rs.name(), card_number.clone(), atr.clone()).await;

            // find cards that have been ejected and return as a vector
            let readers_list = reader_cards_pool_update(
                reader_cards_pool,
                reader_name_string,
                &card_state_string,
                &card_number,
            );
            // check the inserted cards and their connections. If the card is removed, it deletes the task in which the mqtt connection is running.
            remove_connections(readers_list).await;

            // send an event to the frontend to update the state of the card
            emit_event("global-cards-sync", atr.into(), reader_name_string.into(), card_state_string.into(), card_number_clone.into(), None, None);
        };
    }

    Ok(())
}

// Automatically sync cards
pub async fn sc_monitor() -> ! {
    loop {
        let ctx = match Context::establish(Scope::User) {
            Ok(ctx) => ctx,
            Err(e) => {
                log::error!(
                    "Failed to establish context: {:?}. Try to reinit in a 5 seconds.",
                    e
                );
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        let mut readers_buf = [0; 2048];
        let mut reader_states = vec![
            // Listen for reader insertions/removals, if supported.
            ReaderState::new(PNP_NOTIFICATION(), State::UNAWARE),
        ];

        // Vector that stores the connected states of the reader + card (so that it would be possible to understand that the card has been removed)
        let mut reader_cards_pool = Vec::new();

        loop {
            if let Err(e) = setup_reader_states(&ctx, &mut readers_buf, &mut reader_states) {
                log::error!("Failed to setup_reader_states: {:?}", e);
                break; // Exit the inner loop to re-establish context
            }
            if let Err(e) =
                process_reader_states(&ctx, &mut reader_states, &mut reader_cards_pool).await
            {
                log::error!("Failed to process reader states: {:?}", e);
                break; // Exit the inner loop to re-establish context
            }
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
    } else if !reader_name.is_empty() && card_number.is_empty() {
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
            "Reader name is empty or both reader name and card number are empty. No action taken."
        );
    }

    println!("Final state of reader_cards_pool: {:?}", reader_cards_pool);

    company_card_numbers
}

pub fn send_apdu_to_card_command(card: &Card, apdu_hex: &str) -> Result<String, Box<dyn Error>> {
    // Convert HEX string to bytes
    let apdu =
        decode(apdu_hex).map_err(|err| format!("Failed to decode tracker's APDU HEX: {}", err))?;

    println!("Sending APDU: {:?}", apdu);
    let mut rapdu_buf = [0; MAX_BUFFER_SIZE];
    let rapdu = card.transmit(&apdu, &mut rapdu_buf).map_err(|err| {
        log::error!("Failed to transmit APDU command to card: {}", err);
        format!("Failed to transmit APDU command to card: {}", err)
    })?;

    // Decoding response from binary array to HEX string
    let rapdu_hex = encode(rapdu);
    log::debug!("APDU response: {:?}", rapdu_hex);

    Ok(rapdu_hex)
}

pub fn create_card_object(reader_name: &CStr) -> Result<Card, Box<dyn StdError>> {
    // Establish a PC/SC context.
    let ctx = Context::establish(Scope::User).expect("Failed to establish context");

    // Directly use the reader name to connect to the card.
    ctx.connect(reader_name, ShareMode::Shared, Protocols::ANY)
        .map_err(|err| {
            log::error!("Failed to connect to card: {}", err);
            Box::new(err) as Box<dyn StdError>
        })
}

// Manual card sync function. ////////////
// This function is used to manually sync cards from anywhere in the program.
// Manually sync cards. Clicking on the button in the frontend will trigger this function
#[tauri::command]
pub async fn manual_sync_cards(restart: bool) -> () {
    log::debug!("Manual sync cards function is called. Restart {}", restart);
    let ctx = Context::establish(Scope::User).expect("failed to establish context");

    let mut readers_buf = [0; 2048];
    let mut reader_states = vec![
        // Listen for reader insertions/removals, if supported.
        ReaderState::new(PNP_NOTIFICATION(), State::UNAWARE),
    ];

    // setup readers states. Getting changes and other inits
    if let Err(e) = setup_reader_states(&ctx, &mut readers_buf, &mut reader_states) {
        log::error!("Failed to setup reader states: {:?}", e);
    }
    // waiting fot the status change
    ctx.get_status_change(None, &mut reader_states)
        .expect("failed to get status change");

    if restart {
        // remove all connections
        remove_connections_all().await;
    }

    for rs in reader_states {
        if rs.name() != PNP_NOTIFICATION() {
            // convert ATR to hex string value
            let atr = hex::encode(rs.atr());
            // Checking if card number is in the cache
            let card_number = get_from_cache(CacheSection::Cards, &atr);
            /*
                This is a CRUTCH!!! Need to find a better way to convert card_state to string
                The meaning of the card_state is in the pcsc module with the their own state enum.
                The card_state is a bit mask and it is not clear how to convert it to a human readable string properly
            */
            let card_state_string = format!("{:?}", rs.event_state());
            // If the card state has not 'CHANGED' state, then we skip the processing of this card
            // Due to the specifics of the library, the map can be initialized in several stages,
            // But we only need the final result with the value changed
            if !card_state_string.contains("CHANGED") {
                continue;
            }

            //  Trace status of the reader & card
            log::info!(
                "{:?} {:?} {:?}, {:?}",
                rs.name(),
                rs.event_state(),
                atr,
                card_number
            );

            // launches async task with a card and mqtt connection.
            ensure_connection(rs.name(), card_number.clone(), atr.clone()).await;

            // convert reader name to string
            let reader_name_string: &str = rs.name().to_str().unwrap(); // convert reader name(&CStr) to string
            let card_number_clone = card_number.clone();

            // send an event to the frontend to update the state of the card
            emit_event("global-cards-sync", atr.into(), reader_name_string.into(), card_state_string.into(), card_number_clone.into(), None, None);
        };
    }
}
