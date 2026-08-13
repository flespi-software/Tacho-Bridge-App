//! Card rack over the COM (serial) port.
//!
//! This module is intentionally a **thin wrapper**: it watches the USB-serial
//! bus for the rack device, logs connect/disconnect transitions, and bridges
//! bytes between the server and the serial port. It contains **no command/wire
//! protocol** — every command is built and interpreted by the server; the
//! client only forwards raw bytes.
//!
//! Presence detection mirrors how `smart_card::sc_monitor` watches for cards:
//! a continuous monitor loop that reacts to the device appearing and
//! disappearing. There is no serial PnP notification, so we poll the port list
//! (liveness = the port is present on the bus), without speaking the protocol.
//!
//! Layout:
//! - `transport` — the wire: port IO, timings, command envelope
//! - `discovery` — finding the rack on the bus and opening its port
//! - `rack` — the rack's own MQTT connection
//! - `cards` — per-card MQTT sessions and the presence watch
//! - `state` — the card list shown in the UI

mod cards;
mod discovery;
mod rack;
mod state;
mod transport;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serialport::SerialPort;
use tauri::async_runtime::{self, JoinHandle};
use tokio::sync::Mutex as AsyncMutex;

use crate::global_app_handle::rack_emit_event;

use cards::{stop_rack_cards, stop_rack_watch};
use discovery::{find_rack, open_rack, RackInfo, POLL_INTERVAL};
use rack::rack_mqtt_loop;
use transport::SharedPort;

// Re-exported for the rest of the app: these are the only entry points.
pub use cards::connect_pending_rack_cards;
pub use cards::disconnect_rack_card;

const RECONNECT_DELAY_INITIAL_SECS: u64 = 10;
const RECONNECT_DELAY_MAX_SECS: u64 = 300;

/// Returns the next reconnect delay given the current one (exponential, capped).
fn next_reconnect_delay(current: u64) -> u64 {
    current.saturating_mul(2).min(RECONNECT_DELAY_MAX_SECS)
}

/// Guards against starting more than one rack monitor. The `frontend-loaded`
/// event in `lib.rs` can fire several times at startup, which would otherwise
/// spawn duplicate monitors.
static MONITOR_RUNNING: AtomicBool = AtomicBool::new(false);

/// Set when the app is closing: stops the presence monitor from re-opening the
/// serial port after `shutdown()` released it (its self-heal branch would grab
/// the port again within one poll tick).
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

lazy_static::lazy_static! {
    /// Handle to the rack's MQTT task, if one is running, together with the rack's
    /// serial port. Started when the rack connects and aborted when it disconnects,
    /// so there is at most one rack MQTT connection at a time; the port rides along
    /// so `connect_pending_rack_cards` can spawn card sessions outside the MQTT loop.
    static ref RACK_MQTT_TASK: std::sync::Mutex<Option<(JoinHandle<()>, SharedPort)>> =
        std::sync::Mutex::new(None);
}

fn rack_mqtt_running() -> bool {
    let mut guard = match RACK_MQTT_TASK.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    match guard.as_ref() {
        Some((handle, _)) if handle.inner().is_finished() => {
            log::warn!("RACK | [MQTT] status=stale_task_cleared reason=task_exited");
            *guard = None;
            false
        }
        Some(_) => true,
        None => false,
    }
}

/// Starts the rack's MQTT task if one is not already running.
fn start_rack_mqtt(client_id: String, port: SharedPort) {
    if rack_mqtt_running() {
        log::debug!("RACK | [MQTT] phase=start status=skipped reason=already_running");
        return;
    }
    let mut guard = match RACK_MQTT_TASK.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    log::info!("RACK | [MQTT] phase=start client_id={}", client_id);
    let handle = async_runtime::spawn(rack_mqtt_loop(client_id, port.clone()));
    *guard = Some((handle, port));
}

/// Tears down the rack's MQTT stack so the presence monitor rebuilds it on the
/// next tick with fresh config. The MQTT loops resolve the broker host once at
/// start, so a server-host change must go through a full restart; the server
/// re-issues `connect`/`watch` after the rack reconnects.
pub fn restart_rack_mqtt(reason: &str) {
    if !rack_mqtt_running() {
        return;
    }
    log::info!("RACK | [MQTT] phase=restart reason={}", reason);
    stop_rack_mqtt();
}

/// Releases the serial port and stops all rack tasks. Called when the app is
/// closing: the process may linger briefly (WebView children, blocking PC/SC
/// thread), and an undisposed COM handle makes a relaunched instance fail with
/// "Access is denied" until the old process fully dies.
pub fn shutdown() {
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
    stop_rack_mqtt();
    log::info!("RACK | phase=shutdown status=port_released");
}

/// Stops the rack's MQTT task if one is running, along with every rack-backed
/// card session — without the rack there is no transport to those cards.
fn stop_rack_mqtt() {
    {
        let mut guard = match RACK_MQTT_TASK.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some((handle, _)) = guard.take() {
            handle.abort();
            log::info!("RACK | [MQTT] phase=stop status=aborted");
        }
    }
    stop_rack_watch();
    stop_rack_cards();
}

/// Called when the rack transitions to connected. Logs readiness, emits the
/// frontend event, and starts the rack's own MQTT connection wired to the open
/// serial port.
fn on_rack_connected(rack: &RackInfo, port: Box<dyn SerialPort>) {
    // The shutdown flag may have been set between the monitor's loop-top check
    // and this call (find_rack + open_rack take hundreds of ms): starting the
    // MQTT stack now would leave an open COM handle and live tasks that
    // nothing will ever stop. Dropping `port` here closes the handle.
    if SHUTTING_DOWN.load(Ordering::SeqCst) {
        log::info!("RACK | phase=ready status=skipped reason=app_shutdown");
        return;
    }
    // vid/pid logged for data collection only — matching is by product string.
    log::info!(
        "RACK | phase=discovery status=found port={} serial={} manufacturer={} product={} vid={:#06x} pid={:#06x}",
        rack.port_name,
        rack.serial.as_deref().unwrap_or("?"),
        rack.manufacturer.as_deref().unwrap_or("?"),
        rack.product.as_deref().unwrap_or("?"),
        rack.vid,
        rack.pid,
    );
    // Only Lisle Design racks are supported. The manufacturer string must carry
    // the brand marker; otherwise we don't talk to the device at all.
    if !rack.is_supported() {
        log::warn!(
            "RACK | phase=ready status=unsupported reason=manufacturer_not_lisle manufacturer={} \
             detail=not_a_lisle_design_tachograph_rack",
            rack.manufacturer.as_deref().unwrap_or("?")
        );
        return;
    }

    let client_id = rack.client_id();
    log::info!(
        "RACK | phase=ready status=rack_connected_ready_for_work serial={} client_id={}",
        rack.serial.as_deref().unwrap_or("?"),
        client_id
    );

    // Tell the frontend the rack is present. The card list is empty for now —
    // the server doesn't yet report the cards held in the rack's slots.
    rack_emit_event(rack.to_state(true));

    // A (re)connect starts from a clean slate: kill any previous rack MQTT task and its
    // card sessions — they hold a handle to the old (stale) serial port.
    stop_rack_mqtt();

    // Open the rack's own MQTT connection wired to the serial port, and wait for
    // server commands. Each `serial_cmd` is written straight to this port.
    let shared_port: SharedPort = Arc::new(AsyncMutex::new(port));
    start_rack_mqtt(client_id, shared_port);
}

/// Called when the rack transitions to disconnected.
fn on_rack_disconnected(rack: &RackInfo) {
    log::warn!(
        "RACK | phase=presence status=disconnected port={} serial={}",
        rack.port_name,
        rack.serial.as_deref().unwrap_or("?")
    );

    // Tell the frontend the rack is gone.
    rack_emit_event(rack.to_state(false));

    // Tear down the rack's MQTT connection.
    stop_rack_mqtt();
}

/// Background monitor: continuously watches the bus for the rack appearing and
/// disappearing, reacting on each transition. Once started it loops forever; a
/// second concurrent call returns immediately (see `MONITOR_RUNNING`).
pub async fn rack_connection() {
    // Ignore duplicate spawns from repeated `frontend-loaded` events.
    if MONITOR_RUNNING.swap(true, Ordering::SeqCst) {
        log::debug!("RACK | phase=rack_connection status=already_running");
        return;
    }

    log::info!(
        "RACK | phase=rack_connection status=start poll_secs={}",
        POLL_INTERVAL.as_secs()
    );

    // The rack we currently consider connected, if any.
    let mut current: Option<RackInfo> = None;

    loop {
        if SHUTTING_DOWN.load(Ordering::SeqCst) {
            log::info!("RACK | phase=rack_connection status=stopped reason=app_shutdown");
            return;
        }
        // Device enumeration is synchronous OS work (SetupAPI/IOKit, and a
        // blocking USB scan on Windows) that can take hundreds of ms — run it
        // on the blocking pool so this 2s tick never stalls the async workers
        // driving the MQTT event loops.
        let found = tokio::task::spawn_blocking(find_rack)
            .await
            .unwrap_or_default();

        match (&current, found) {
            // Newly appeared.
            (None, Some(rack)) => {
                if let Some(port) = open_rack_blocking(&rack).await {
                    on_rack_connected(&rack, port);
                    current = Some(rack);
                }
                // If open failed, leave `current` None and retry next tick.
            }
            // Disappeared.
            (Some(prev), None) => {
                on_rack_disconnected(prev);
                current = None;
            }
            // Still present, but a different device than before (e.g. swapped
            // unit / port rename): treat as disconnect + reconnect.
            (Some(prev), Some(rack)) if *prev != rack => {
                on_rack_disconnected(prev);
                if let Some(port) = open_rack_blocking(&rack).await {
                    on_rack_connected(&rack, port);
                    current = Some(rack);
                } else {
                    current = None;
                }
            }
            // Same device still present, but its MQTT task died (e.g. a
            // panic). Self-heal: the dead task dropped the shared port
            // handle, so reopen the port and restart the task.
            (Some(_), Some(rack)) if rack.is_supported() && !rack_mqtt_running() => {
                log::warn!(
                    "RACK | phase=presence status=mqtt_task_dead port={} action=restart",
                    rack.port_name
                );
                // The dead loop's siblings (watch task, card sessions) may still
                // hold the old exclusive serial handle — kill them BEFORE reopening,
                // otherwise open_rack fails on every tick forever. Running this on
                // each retry tick also reaps a watch task that raced past a
                // previous stop_rack_mqtt.
                stop_rack_mqtt();
                if let Some(port) = open_rack_blocking(&rack).await {
                    on_rack_connected(&rack, port);
                    current = Some(rack);
                }
                // If open failed, keep `current` as is and retry next tick.
            }
            // No change (present-present same, or absent-absent).
            _ => {}
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// `open_rack` moved to the blocking pool: opening a busy COM port can block,
/// and the monitor runs on an async worker.
async fn open_rack_blocking(rack: &RackInfo) -> Option<Box<dyn SerialPort>> {
    let rack = rack.clone();
    tokio::task::spawn_blocking(move || open_rack(&rack))
        .await
        .unwrap_or_default()
}
