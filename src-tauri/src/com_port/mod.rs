//! Card racks over COM (serial) ports.
//!
//! This module is intentionally a **thin wrapper**: it watches the USB-serial
//! bus for supported devices, logs connect/disconnect transitions, and bridges
//! bytes between the server and each serial port. It contains **no command/wire
//! protocol** — every command is built and interpreted by the server; the
//! client only forwards raw bytes.
//!
//! Several devices are served at once: each connected rack gets its own MQTT
//! connection, serial port, presence watch and card sessions, all keyed by the
//! rack's client_id (derived from the device serial). Presence detection
//! mirrors how `smart_card::sc_monitor` watches for cards: a continuous monitor
//! loop that reacts to devices appearing and disappearing. There is no serial
//! PnP notification, so we poll the port list (liveness = the port is present
//! on the bus), without speaking the protocol.
//!
//! Layout:
//! - `transport` — the wire: port IO, timings, command envelope
//! - `discovery` — device profiles, finding devices on the bus, opening ports
//! - `rack` — a rack's own MQTT connection
//! - `cards` — per-card MQTT sessions and the per-rack presence watches
//! - `state` — the per-rack card lists shown in the UI

mod cards;
mod discovery;
mod rack;
mod state;
mod transport;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serialport::SerialPort;
use tauri::async_runtime::{self, JoinHandle};
use tokio::sync::Mutex as AsyncMutex;

use crate::global_app_handle::rack_emit_event;

use cards::{stop_all_rack_cards, stop_all_rack_watches, stop_rack_cards, stop_rack_watch};
use discovery::{find_racks, open_rack, RackInfo, POLL_INTERVAL};
use rack::rack_mqtt_loop;
use transport::SharedPort;

// Re-exported for the rest of the app: these are the only entry points.
pub use cards::abort_rack_card_session;
pub use cards::connect_pending_rack_cards;
pub use cards::disconnect_rack_card;

use crate::backoff::{next_reconnect_delay, RECONNECT_DELAY_INITIAL_SECS};

/// Guards against starting more than one rack monitor. `initialize_backend` in
/// `lib.rs` runs once, so this is a backstop rather than the load-bearing guard
/// it was when initialization hung off the repeatable `frontend-loaded` event.
static MONITOR_RUNNING: AtomicBool = AtomicBool::new(false);

/// Set when the app is closing: stops the presence monitor from re-opening the
/// serial ports after `shutdown()` released them (its self-heal branch would
/// grab a port again within one poll tick).
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

lazy_static::lazy_static! {
    /// Live rack connections keyed by the rack's client_id: each entry is the
    /// rack's MQTT task together with its open serial port. An entry is added
    /// when a rack connects and removed when it disconnects, so there is at
    /// most one MQTT connection per rack; the port rides along so
    /// `connect_pending_rack_cards` can spawn card sessions outside the MQTT
    /// loop.
    static ref RACK_TASKS: std::sync::Mutex<HashMap<String, (JoinHandle<()>, SharedPort)>> =
        std::sync::Mutex::new(HashMap::new());

    /// Serial device nodes this app currently holds open, keyed by rack
    /// client_id. Discovery consults it so a device that already has a live
    /// session keeps being addressed by the SAME descriptor, even when the OS
    /// later exposes another alias for it (see `dedupe_by_client_id`).
    static ref RACK_ACTIVE_PORTS: std::sync::Mutex<HashMap<String, String>> =
        std::sync::Mutex::new(HashMap::new());
}

/// Device nodes currently held open by this app.
pub(super) fn active_rack_ports() -> std::collections::HashSet<String> {
    lock(&RACK_ACTIVE_PORTS).values().cloned().collect()
}

/// Test-only: pin a device node as "open by this app" so discovery's stickiness
/// can be exercised without real hardware.
#[cfg(test)]
pub(super) fn set_active_rack_port_for_test(client_id: &str, port_name: Option<&str>) {
    let mut guard = lock(&RACK_ACTIVE_PORTS);
    match port_name {
        Some(port) => guard.insert(client_id.to_string(), port.to_string()),
        None => guard.remove(client_id),
    };
}

/// Locks a mutex, recovering from poisoning: a panic in any holder must not
/// permanently kill the rack stack. Every guarded value in this module is safe
/// to reuse after a panic — plain collections replaced in whole assignments,
/// never left half-updated. Shared by all `com_port` submodules.
fn lock<T>(m: &'static std::sync::Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// True while the MQTT task of this rack is running; reaps a finished entry.
fn rack_mqtt_running(client_id: &str) -> bool {
    let mut guard = lock(&RACK_TASKS);
    match guard.get(client_id) {
        Some((handle, _)) if handle.inner().is_finished() => {
            log::warn!(
                "RACK {} | [MQTT] status=stale_task_cleared reason=task_exited",
                client_id
            );
            guard.remove(client_id);
            false
        }
        Some(_) => true,
        None => false,
    }
}

/// Starts one rack's MQTT task if it is not already running.
fn start_rack_mqtt(client_id: String, port: SharedPort) {
    if rack_mqtt_running(&client_id) {
        log::debug!(
            "RACK {} | [MQTT] phase=start status=skipped reason=already_running",
            client_id
        );
        return;
    }
    let mut guard = lock(&RACK_TASKS);
    log::info!("RACK | [MQTT] phase=start client_id={}", client_id);
    let handle = async_runtime::spawn(rack_mqtt_loop(client_id.clone(), port.clone()));
    guard.insert(client_id, (handle, port));
}

/// Tears down the MQTT stack of every rack so the presence monitor rebuilds
/// them on the next tick with fresh config. The MQTT loops resolve the broker
/// host once at start, so a server-host change must go through a full restart;
/// the server re-issues `connect`/`watch` after each rack reconnects.
pub fn restart_rack_mqtt(reason: &str) {
    let ids: Vec<String> = lock(&RACK_TASKS).keys().cloned().collect();
    if ids.is_empty() {
        return;
    }
    log::info!(
        "RACK | [MQTT] phase=restart reason={} racks={}",
        reason,
        ids.len()
    );
    for id in &ids {
        stop_rack(id);
    }
}

/// Releases the serial ports and stops all rack tasks. Called when the app is
/// closing: the process may linger briefly (WebView children, blocking PC/SC
/// thread), and an undisposed COM handle makes a relaunched instance fail with
/// "Access is denied" until the old process fully dies.
pub fn shutdown() {
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
    stop_all_racks();
    log::info!("RACK | phase=shutdown status=ports_released");
}

/// Stops one rack's MQTT task, presence watch and card sessions — without the
/// rack there is no transport to its cards. Dropping the port handle from the
/// map is what closes the COM handle once the tasks release their clones.
///
/// Returns the rack's port handle when one was registered, so callers that are
/// about to REOPEN the same device can wait for it to actually close (see
/// `stop_rack_and_await_port_release`). `abort()` only marks a task for
/// cancellation, so the port stays open until every task actually yields and
/// drops its `Arc` clone.
fn stop_rack(client_id: &str) -> Option<SharedPort> {
    // The device node is no longer ours; discovery may pick a different alias
    // for this rack from now on.
    lock(&RACK_ACTIVE_PORTS).remove(client_id);
    let port = {
        let mut guard = lock(&RACK_TASKS);
        match guard.remove(client_id) {
            Some((handle, port)) => {
                handle.abort();
                log::info!("RACK {} | [MQTT] phase=stop status=aborted", client_id);
                Some(port)
            }
            None => None,
        }
    };
    stop_rack_watch(client_id);
    stop_rack_cards(client_id);
    port
}

/// How long to wait for aborted rack tasks to drop their serial-port clones
/// before reopening the device. Cancellation is cooperative, so the handles go
/// away within a scheduler tick or two; this is an upper bound, not a delay we
/// expect to spend.
const PORT_RELEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);
/// Poll step while waiting for the port handle to become unreferenced.
const PORT_RELEASE_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// Stops a rack and waits until its serial port is really closed.
///
/// `stop_rack` alone is not enough before reopening the SAME physical device:
/// `abort()` merely schedules cancellation, so the MQTT loop, the watch task
/// and the card sessions still hold `Arc` clones of the port for a short while.
/// Reopening in that window fails with "Device or resource busy" — the app
/// competing with itself for the port it just gave up — which tore down a
/// working rack session and left the rack dark until a later retry tick.
///
/// Waits for the last clone to drop (the `Arc` strong count falling to one, our
/// own), then drops it, which closes the OS handle.
async fn stop_rack_and_await_port_release(client_id: &str) {
    let Some(port) = stop_rack(client_id) else {
        return;
    };

    let deadline = std::time::Instant::now() + PORT_RELEASE_TIMEOUT;
    while Arc::strong_count(&port) > 1 && std::time::Instant::now() < deadline {
        tokio::time::sleep(PORT_RELEASE_POLL).await;
    }

    if Arc::strong_count(&port) > 1 {
        log::warn!(
            "RACK {} | phase=stop status=port_still_referenced holders={} timeout_ms={}",
            client_id,
            Arc::strong_count(&port) - 1,
            PORT_RELEASE_TIMEOUT.as_millis()
        );
    }
    // Closes the OS handle when this was the last reference.
    drop(port);
}

/// Stops every rack. The trailing all-variants are a backstop for watchers and
/// card sessions whose rack entry was already reaped (e.g. a task that died
/// and was cleared before its siblings were stopped).
fn stop_all_racks() {
    let ids: Vec<String> = lock(&RACK_TASKS).keys().cloned().collect();
    for id in &ids {
        stop_rack(id);
    }
    stop_all_rack_watches();
    stop_all_rack_cards();
}

/// Called when a rack transitions to connected. Logs readiness, emits the
/// frontend event, and starts the rack's own MQTT connection wired to the open
/// serial port.
fn on_rack_connected(rack: &RackInfo, port: Box<dyn SerialPort>) {
    // The shutdown flag may have been set between the monitor's loop-top check
    // and this call (find_racks + open_rack take hundreds of ms): starting the
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
    // Only devices carrying their profile's brand marker are supported; the
    // discovery passes guarantee this, kept as a belt-and-braces check.
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
    // the server reports the cards in the rack's slots one `connect` at a time.
    rack_emit_event(rack.to_state(true));

    // A (re)connect starts from a clean slate: kill this rack's previous MQTT
    // task and card sessions — they hold a handle to the old (stale) serial port.
    // No need to await its release here: our replacement port is already open,
    // and the old handle refers to a descriptor we are not going to reopen.
    let _ = stop_rack(&client_id);

    // Remember which device node backs this rack, so a later alias for the same
    // device cannot displace it during discovery.
    lock(&RACK_ACTIVE_PORTS).insert(client_id.clone(), rack.port_name.clone());

    // Open the rack's own MQTT connection wired to the serial port, and wait for
    // server commands. Each `serial_cmd` is written straight to this port.
    let shared_port: SharedPort = Arc::new(AsyncMutex::new(port));
    start_rack_mqtt(client_id, shared_port);
}

/// Called when a rack transitions to disconnected.
fn on_rack_disconnected(rack: &RackInfo) {
    announce_rack_disconnected(rack);

    // Tear down this rack's MQTT connection, watch and card sessions.
    stop_rack(&rack.client_id());
}

/// Same as `on_rack_disconnected`, but waits for the serial port to be released.
/// Used when the caller is about to reopen the same physical device.
async fn on_rack_disconnected_awaiting_port(rack: &RackInfo) {
    announce_rack_disconnected(rack);
    stop_rack_and_await_port_release(&rack.client_id()).await;
}

fn announce_rack_disconnected(rack: &RackInfo) {
    log::warn!(
        "RACK | phase=presence status=disconnected port={} serial={}",
        rack.port_name,
        rack.serial.as_deref().unwrap_or("?")
    );

    // Tell the frontend the rack is gone (it stays listed as disconnected).
    rack_emit_event(rack.to_state(false));
}

/// Background monitor: continuously watches the bus for racks appearing and
/// disappearing, reacting on each transition — every rack independently. Once
/// started it loops forever; a second concurrent call returns immediately (see
/// `MONITOR_RUNNING`).
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

    // The racks we currently consider connected, keyed by client_id.
    let mut current: HashMap<String, RackInfo> = HashMap::new();

    loop {
        if SHUTTING_DOWN.load(Ordering::SeqCst) {
            log::info!("RACK | phase=rack_connection status=stopped reason=app_shutdown");
            return;
        }
        // Device enumeration is synchronous OS work (SetupAPI/IOKit, and a
        // blocking USB scan on Windows) that can take hundreds of ms — run it
        // on the blocking pool so this 2s tick never stalls the async workers
        // driving the MQTT event loops.
        let found: HashMap<String, RackInfo> = tokio::task::spawn_blocking(find_racks)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|rack| (rack.client_id(), rack))
            .collect();

        // Disappeared racks. A device swapped under the same client_id cannot
        // happen (the id derives from the serial); a port rename keeps the id
        // and lands in the "changed" branch below.
        let gone: Vec<String> = current
            .keys()
            .filter(|id| !found.contains_key(*id))
            .cloned()
            .collect();
        for id in gone {
            if let Some(prev) = current.remove(&id) {
                on_rack_disconnected(&prev);
            }
        }

        for (id, rack) in &found {
            let prev = current.get(id).cloned();
            match prev {
                // Newly appeared.
                None => {
                    if let Some(port) = open_rack_blocking(rack).await {
                        on_rack_connected(rack, port);
                        current.insert(id.clone(), rack.clone());
                    }
                    // If open failed, leave it out of `current` and retry next tick.
                }
                // Still present, but with different attributes (e.g. a port
                // rename after replug): treat as disconnect + reconnect.
                Some(prev) if prev != *rack => {
                    // The old and the new descriptor can name the SAME physical
                    // device (macOS exposes a rack as both /dev/cu.usbserial-<serial>
                    // and /dev/cu.usbserial-<N>), so the reopen below competes with
                    // the handle we are releasing. Wait for it to close first,
                    // otherwise the open fails as busy and the rack — plus every
                    // card session on it — stays down until a later tick.
                    on_rack_disconnected_awaiting_port(&prev).await;
                    current.remove(id);
                    if let Some(port) = open_rack_blocking(rack).await {
                        on_rack_connected(rack, port);
                        current.insert(id.clone(), rack.clone());
                    }
                }
                // Same rack still present, but its MQTT task died (e.g. a
                // panic). Self-heal: the dead task dropped the shared port
                // handle, so reopen the port and restart the task.
                Some(_) if rack.is_supported() && !rack_mqtt_running(id) => {
                    log::warn!(
                        "RACK | phase=presence status=mqtt_task_dead port={} action=restart",
                        rack.port_name
                    );
                    // The dead loop's siblings (watch task, card sessions) may still
                    // hold the old exclusive serial handle — kill them BEFORE reopening
                    // and wait for the handle to actually close, otherwise open_rack
                    // fails as busy on every tick forever. Running this on each retry
                    // tick also reaps a watch task that raced past a previous stop.
                    stop_rack_and_await_port_release(id).await;
                    if let Some(port) = open_rack_blocking(rack).await {
                        on_rack_connected(rack, port);
                    }
                    // If open failed, keep it in `current` and retry next tick.
                }
                // No change.
                _ => {}
            }
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
