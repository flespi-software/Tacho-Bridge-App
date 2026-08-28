// ───── Std Lib ─────
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Locks [`APP_HANDLE`], recovering the inner value if an earlier panic
/// poisoned the mutex — a cascading panic here would silently kill every
/// later `emit_*` call.
fn lock_handle() -> std::sync::MutexGuard<'static, Option<AppHandle>> {
    APP_HANDLE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// initialize the global app handle
pub fn set_app_handle(handle: AppHandle) {
    *lock_handle() = Some(handle);
}

// getting the global app handle
pub fn get_app_handle() -> Option<AppHandle> {
    lock_handle().clone()
}

/// Emits one event to the frontend, logging the outcome uniformly. Every
/// `emit_*` in this module funnels through here so a missing app handle (the
/// window not built yet, or already torn down) is always a warning rather than
/// a silent drop.
fn emit_to_frontend<P: Serialize + Clone>(event_name: &str, payload: P) {
    let Some(app_handle) = get_app_handle() else {
        log::warn!("emit '{}' skipped: app handle not set", event_name);
        return;
    };
    match app_handle.emit(event_name, payload) {
        Ok(()) => log::debug!("'{}' has been sent", event_name),
        Err(e) => log::error!("emit '{}' failed: {:?}", event_name, e),
    }
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

    emit_to_frontend(event_name, payload);
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

    emit_to_frontend(event_name, payload);
}

#[derive(Clone, Serialize)]
pub struct NotificationPayload {
    pub notification_type: String,
    pub message: String,
}

// Last known app connection status. The connection task starts from setup(),
// before the webview loads, so the first OFFLINE->ONLINE emit can fire with no
// listener and be lost — the status is cached here and replayed on
// `frontend-loaded` (and pulled via the `get_app_connection_status` command).
static APP_ONLINE: AtomicBool = AtomicBool::new(false);

/// Emits the app connection status to the frontend and caches it for replay.
pub fn app_emit_event(online: bool) {
    APP_ONLINE.store(online, Ordering::SeqCst);
    emit_to_frontend("app-connection-status", online);
}

/// Re-emits the cached app connection status. Called on `frontend-loaded` so a
/// freshly loaded webview shows the real status without waiting for the next
/// OFFLINE<->ONLINE transition.
pub fn emit_current_app_connection() {
    emit_to_frontend("app-connection-status", APP_ONLINE.load(Ordering::SeqCst));
}

/// Frontend pull of the cached app connection status — covers the race where
/// the webview mounts after the replay emit has already fired.
#[tauri::command]
pub fn get_app_connection_status() -> bool {
    APP_ONLINE.load(Ordering::SeqCst)
}

pub fn emit_notification_event(event_name: &str, payload: NotificationPayload) {
    emit_to_frontend(event_name, payload);
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
    /// True once this card's rack-backed MQTT session is connected. `None` for
    /// a card the rack reports but TBA does not serve (unknown ICCID).
    pub online: Option<bool>,
    /// True while an APDU exchange is actually running on this card — drives
    /// the blinking activity icon, same as `Reader.authentication` does for a
    /// card in a plain PC/SC reader.
    pub authentication: Option<bool>,
}

/// State of one connected card rack, pushed to the frontend. Carries only
/// device identity + presence + the (eventual) card list — no wire protocol.
#[derive(Clone, Serialize)]
pub struct RackState {
    /// Stable identity of this rack: its MQTT client_id, derived from the
    /// device serial. Keys the rack list in the UI and every per-rack update.
    pub client_id: String,
    pub connected: bool,
    pub name: String,
    pub serial: Option<String>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub cards: Vec<RackCard>,
    /// True once the server has finished enumerating the rack, so the UI can
    /// stop its "scanning" indicator instead of guessing from a silence
    /// timeout. Set when the presence `watch` is armed — the server arms it
    /// after its discovery chain has walked the rack (see `start_rack_watch`).
    pub scan_complete: bool,
}

// Last known state of every rack reported this session, keyed by client_id.
// The rack monitor runs independently of the frontend, so an emit can fire
// before the UI has subscribed (the event would be lost) — the states are
// cached and re-emitted when the frontend (re)loads. BTreeMap: the emitted
// list is ordered by client_id (i.e. by device serial), so the UI order is
// stable across updates and restarts.
lazy_static! {
    static ref RACK_STATES: Mutex<std::collections::BTreeMap<String, RackState>> =
        Mutex::new(std::collections::BTreeMap::new());
}

/// Emits the full rack list to the frontend (event `rack-state`). The payload
/// is always every known rack, so the frontend replaces its list wholesale and
/// never has to merge deltas.
fn emit_rack_states(states: Vec<RackState>) {
    emit_to_frontend("rack-state", states);
}

/// Applies one mutation to the rack map under its lock and emits the full list
/// when the mutation reports a change. The single lock-mutate-snapshot-emit
/// sequence every per-rack update goes through — the snapshot is taken inside
/// the lock, so concurrent updates cannot emit lists that reorder each other's
/// changes.
fn mutate_rack_states_and_emit(
    mutate: impl FnOnce(&mut std::collections::BTreeMap<String, RackState>) -> bool,
) {
    let states = {
        let mut guard = RACK_STATES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !mutate(&mut guard) {
            return;
        }
        guard.values().cloned().collect::<Vec<_>>()
    };
    emit_rack_states(states);
}

/// Upserts one rack's state (keyed by its client_id) and emits the full rack
/// list, caching it so a freshly-loaded frontend can be brought up to date via
/// [`emit_current_rack_state`]. A disconnected rack stays in the list marked
/// `connected: false` — "was here and vanished" is useful diagnostics.
pub fn rack_emit_event(state: RackState) {
    mutate_rack_states_and_emit(|states| {
        states.insert(state.client_id.clone(), state);
        true
    });
}

/// Updates the card list of one rack and re-emits the full list. Called by the
/// rack module as rack-backed card sessions are spawned and closed; a no-op
/// when that rack has never been reported.
pub fn rack_update_cards(client_id: &str, cards: Vec<RackCard>) {
    mutate_rack_states_and_emit(|states| match states.get_mut(client_id) {
        Some(state) => {
            state.cards = cards;
            true
        }
        None => false,
    });
}

/// Marks one rack's scan as finished and re-emits the list, so the UI can end
/// that rack's "scanning" indicator on a real signal rather than a silence
/// timeout. A no-op when the rack is unknown or already marked.
pub fn rack_mark_scan_complete(client_id: &str) {
    mutate_rack_states_and_emit(|states| match states.get_mut(client_id) {
        Some(state) if !state.scan_complete => {
            state.scan_complete = true;
            log::info!("RACK {} | phase=discovery status=scan_complete", client_id);
            true
        }
        // Already complete, or unknown rack — nothing to announce.
        _ => false,
    });
}

/// Re-emits the cached rack list to the frontend, if any. Called on
/// `frontend-loaded` so the UI shows the racks immediately on (re)load, without
/// waiting for the next connect/disconnect transition.
pub fn emit_current_rack_state() {
    let states: Vec<RackState> = RACK_STATES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .cloned()
        .collect();
    if !states.is_empty() {
        emit_rack_states(states);
    }
}
