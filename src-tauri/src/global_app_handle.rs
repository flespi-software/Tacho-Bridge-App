use lazy_static::lazy_static;
use std::sync::Mutex;
use tauri::{AppHandle, Manager};

use crate::smart_card::TachoState;

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

pub fn emit_event(event_name: &str, atr: String, reader_name: String, card_state: String, card_number: String, online: Option<bool>, authentication: Option<bool>) {
    let payload = TachoState {
        atr,
        reader_name,
        card_state,
        card_number,
        online,
        authentication
    };

    if let Some(app_handle) = get_app_handle() {
        if let Err(e) = app_handle.emit_all(event_name, payload) {
            println!("Error: {:?}", e);
        }
        println!("{} has been sent", event_name);
    } else {
        println!("App handle is not set");
    }
}