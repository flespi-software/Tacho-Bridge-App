// ───── Std Lib ─────
use std::sync::Mutex;

// ───── External Crates ─────
use lazy_static::lazy_static;
use serde::Serialize;
use tauri::{AppHandle, Emitter};

// ───── Local Modules ─────
use crate::config::CardConfig;

// Global application handle used for emitting events from anywhere.
// Wrapped in a Mutex to ensure safe concurrent access.
lazy_static! {
    static ref APP_HANDLE: Mutex<Option<AppHandle>> = Mutex::new(None);
}

// initialize the global app handle
pub fn set_app_handle(handle: AppHandle) {
    let mut app_handle = APP_HANDLE.lock().unwrap();
    *app_handle = Some(handle);
}

// getting the global app handle
pub fn get_app_handle() -> Option<AppHandle> {
    let app_handle = APP_HANDLE.lock().unwrap();
    app_handle.clone()
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
    pub iccid: String,
    pub reader_name: String,
    pub card_state: String,
    pub card_number: String,
    pub online: Option<bool>,
    pub authentication: Option<bool>,
}

pub fn emit_event(event_name: &str, iccid: String, reader_name: String, card_state: String, card_number: String, online: Option<bool>, authentication: Option<bool>) {
    let payload = TachoState {
        iccid,
        reader_name,
        card_state,
        card_number,
        online,
        authentication
    };

    if let Some(app_handle) = get_app_handle() {
        if let Err(e) = app_handle.emit(event_name, payload) {
            println!("Error: {:?}", e);
        }
        println!("{} has been sent", event_name);
    } else {
        println!("App card handle is not set");
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CardConfigPayload {
    pub card_number: String,
    pub content: Option<CardConfig>,
}

pub fn emit_card_config_event(event_name: &str, card_number: String, config: Option<CardConfig>) {
    let payload = CardConfigPayload {
        card_number,
        content: config,
    };

    if let Some(app_handle) = get_app_handle() {
        if let Err(e) = app_handle.emit(event_name, payload) {
            println!("Error emitting {}: {:?}", event_name, e);
        } else {
            println!("{} has been sent", event_name);
        }
    } else {
        println!("App card handle is not set");
    }
}

#[derive(Clone, Serialize)]
pub struct NotificationPayload {
    pub notification_type: String,
    pub message: String,
}

pub fn emit_notification_event(event_name: &str, payload: NotificationPayload) {
    if let Some(app_handle) = get_app_handle() {
        if let Err(e) = app_handle.emit(event_name, payload) {
            println!("Error: {:?}", e);
        }
        println!("{} has been sent", event_name);
    } else {
        println!("App notification handle is not set");
    }
}