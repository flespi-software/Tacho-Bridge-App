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
    // Lock failure here means the mutex was poisoned by an earlier panic.
    // Recover the inner value instead of cascading the panic so subsequent
    // emit_* calls keep working.
    let mut guard = match APP_HANDLE.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            log::warn!("APP_HANDLE mutex was poisoned — recovering");
            poisoned.into_inner()
        }
    };
    *guard = Some(handle);
}

// getting the global app handle
pub fn get_app_handle() -> Option<AppHandle> {
    let guard = match APP_HANDLE.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            log::warn!("APP_HANDLE mutex was poisoned — recovering");
            poisoned.into_inner()
        }
    };
    guard.clone()
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

pub fn card_emit_event(
    event_name: &str,
    iccid: String,
    reader_name: String,
    card_state: String,
    card_number: String,
    online: Option<bool>,
    authentication: Option<bool>,
) {
    let payload = TachoState {
        iccid,
        reader_name,
        card_state,
        card_number,
        online,
        authentication,
    };

    if let Some(app_handle) = get_app_handle() {
        if let Err(e) = app_handle.emit(event_name, payload) {
            log::error!("emit '{}' failed: {:?}", event_name, e);
        } else {
            log::debug!("'{}' has been sent", event_name);
        }
    } else {
        log::warn!("emit '{}' skipped: app handle not set", event_name);
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
            log::error!("emit '{}' failed: {:?}", event_name, e);
        } else {
            log::debug!("'{}' has been sent", event_name);
        }
    } else {
        log::warn!("emit '{}' skipped: app handle not set", event_name);
    }
}

#[derive(Clone, Serialize)]
pub struct NotificationPayload {
    pub notification_type: String,
    pub message: String,
}

/// Emits the app connection status to the frontend.
pub fn app_emit_event(online: bool) {
    if let Some(app_handle) = get_app_handle() {
        if let Err(e) = app_handle.emit("app-connection-status", online) {
            log::error!("[CONN] failed to emit app connection status: {:?}", e);
        }
    }
}

pub fn emit_notification_event(event_name: &str, payload: NotificationPayload) {
    if let Some(app_handle) = get_app_handle() {
        if let Err(e) = app_handle.emit(event_name, payload) {
            log::error!("emit '{}' failed: {:?}", event_name, e);
        } else {
            log::debug!("'{}' has been sent", event_name);
        }
    } else {
        log::warn!("emit '{}' skipped: app handle not set", event_name);
    }
}

/// One card currently held in a rack slot, as reported by the server's rack
/// discovery. `card_number`/`name` are `None` when the card's ICCID is not in
/// the local config — the card is visible in the rack but not served.
#[derive(Clone, Serialize)]
pub struct RackCard {
    pub slot: u16,
    pub iccid: Option<String>,
    pub card_number: Option<String>,
    pub name: Option<String>,
}

/// State of the connected card rack, pushed to the frontend. Carries only
/// device identity + presence + the (eventual) card list — no wire protocol.
#[derive(Clone, Serialize)]
pub struct RackState {
    pub connected: bool,
    pub name: String,
    pub serial: Option<String>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub cards: Vec<RackCard>,
}

// Last known rack state. The rack monitor runs independently of the frontend,
// so its emit can fire before the UI has subscribed (the event would be lost).
// We cache the latest state and re-emit it when the frontend (re)loads.
lazy_static! {
    static ref LAST_RACK_STATE: Mutex<Option<RackState>> = Mutex::new(None);
}

/// Emits the card rack state to the frontend (event `rack-state`) and caches it
/// so a freshly-loaded frontend can be brought up to date via
/// [`emit_current_rack_state`].
pub fn rack_emit_event(state: RackState) {
    if let Ok(mut guard) = LAST_RACK_STATE.lock() {
        *guard = Some(state.clone());
    }
    if let Some(app_handle) = get_app_handle() {
        if let Err(e) = app_handle.emit("rack-state", state) {
            log::error!("emit 'rack-state' failed: {:?}", e);
        } else {
            log::debug!("'rack-state' has been sent");
        }
    } else {
        log::warn!("emit 'rack-state' skipped: app handle not set");
    }
}

/// Updates the card list of the cached rack state and re-emits it to the frontend.
/// Called by the rack module as rack-backed card sessions are spawned and closed;
/// a no-op when no rack state is cached yet (no rack has been reported).
pub fn rack_update_cards(cards: Vec<RackCard>) {
    let state = {
        let mut guard = match LAST_RACK_STATE.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard.as_mut() {
            Some(state) => {
                state.cards = cards;
                Some(state.clone())
            }
            None => None,
        }
    };
    if let Some(state) = state {
        if let Some(app_handle) = get_app_handle() {
            if let Err(e) = app_handle.emit("rack-state", state) {
                log::error!("emit 'rack-state' (cards update) failed: {:?}", e);
            }
        }
    }
}

/// Re-emits the cached rack state to the frontend, if any. Called on
/// `frontend-loaded` so the UI shows the rack immediately on (re)load, without
/// waiting for the next connect/disconnect transition.
pub fn emit_current_rack_state() {
    let state = LAST_RACK_STATE.lock().ok().and_then(|g| g.clone());
    if let Some(state) = state {
        if let Some(app_handle) = get_app_handle() {
            if let Err(e) = app_handle.emit("rack-state", state) {
                log::error!("re-emit 'rack-state' failed: {:?}", e);
            } else {
                log::debug!("'rack-state' re-emitted to frontend");
            }
        }
    }
}
