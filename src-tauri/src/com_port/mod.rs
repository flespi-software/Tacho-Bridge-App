//! Card racks over COM (serial) ports.
//!
//! This module is intentionally a **thin wrapper**: it watches the USB-serial
//! bus for supported devices, logs connect/disconnect transitions, and bridges
//! bytes between the server and each serial port. It contains **no command/wire
//! protocol** — every command is built and interpreted by the server; the
//! client only forwards raw bytes.
//!
//! A rack is a peripheral of the application, like a PC/SC reader: it has no
//! MQTT connection of its own. Its traffic rides the application connection
//! under the `rack/<id>/...` topic prefix, where `<id>` is the device serial
//! (see `rack.rs`). Several devices are served at once: each connected rack
//! gets its own serial port, presence watch and card sessions, all keyed by
//! its rack id. Presence detection mirrors how `smart_card::sc_monitor`
//! watches for cards: a continuous monitor loop that reacts to devices
//! appearing and disappearing. There is no serial PnP notification, so we poll
//! the port list (liveness = the port is present on the bus), without speaking
//! the protocol.
//!
//! Layout:
//! - `transport` — the wire: port IO, timings, command envelope
//! - `discovery` — device profiles, finding devices on the bus, opening ports
//! - `rack` — the rack traffic of the application connection (link, requests)
//! - `cards` — per-card MQTT sessions and the per-rack presence watches
//! - `state` — the per-rack card lists shown in the UI

mod access;
mod cards;
mod discovery;
mod rack;
mod state;
mod transport;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serialport::SerialPort;
use tokio::sync::Mutex as AsyncMutex;

use crate::global_app_handle::rack_emit_event;

use cards::{stop_all_rack_cards, stop_all_rack_watches, stop_rack_cards, stop_rack_watch};
use discovery::{find_racks, open_rack, RackInfo, POLL_INTERVAL};
use rack::{announce_links, IdempotencySlot};
use transport::SharedPort;

// Re-exported for the rest of the app: these are the only entry points.
pub use cards::abort_rack_card_session;
pub use cards::connect_pending_rack_cards;
pub use cards::disconnect_rack_card;
pub use rack::{handle_app_publish, on_app_connack, on_app_offline, register_app_client};

use crate::backoff::{next_reconnect_delay, RECONNECT_DELAY_INITIAL_SECS};

/// Guards against starting more than one rack monitor. `initialize_backend` in
/// `lib.rs` runs once, so this is a backstop rather than the load-bearing guard
/// it was when initialization hung off the repeatable `frontend-loaded` event.
static MONITOR_RUNNING: AtomicBool = AtomicBool::new(false);

/// Set when the app is closing: stops the presence monitor from re-opening the
/// serial ports after `shutdown()` released them (its self-heal branch would
/// grab a port again within one poll tick).
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// One rack linked to the server: its open serial port plus the request
/// idempotency of its exchanges. The server counts request ids per rack, so
/// the slot is per rack too; it is an async mutex because it is held across
/// the serial exchange, which also serialises the exchanges of one rack (the
/// port queue would anyway).
#[derive(Clone)]
struct LinkedRack {
    port: SharedPort,
    idempotency: Arc<AsyncMutex<IdempotencySlot>>,
}

lazy_static::lazy_static! {
    /// Live racks keyed by rack id. An entry is added when a rack connects
    /// (its `link up` goes to the server) and removed when it disconnects
    /// (`link down`), so the map is exactly the set of racks the server may
    /// address. The port rides along so `connect_pending_rack_cards` can spawn
    /// card sessions outside the request path.
    static ref RACKS: std::sync::Mutex<HashMap<String, LinkedRack>> =
        std::sync::Mutex::new(HashMap::new());

    /// Serial device nodes this app currently holds open, keyed by rack id.
    /// Discovery consults it so a device that already has a live session keeps
    /// being addressed by the SAME descriptor, even when the OS later exposes
    /// another alias for it (see `dedupe_by_rack_id`).
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
pub(super) fn set_active_rack_port_for_test(rack_id: &str, port_name: Option<&str>) {
    let mut guard = lock(&RACK_ACTIVE_PORTS);
    match port_name {
        Some(port) => guard.insert(rack_id.to_string(), port.to_string()),
        None => guard.remove(rack_id),
    };
}

/// Locks a mutex, recovering from poisoning: a panic in any holder must not
/// permanently kill the rack stack. Every guarded value in this module is safe
/// to reuse after a panic — plain collections replaced in whole assignments,
/// never left half-updated. Shared by all `com_port` submodules.
fn lock<T>(m: &'static std::sync::Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The linked rack with this id, if its port is open.
fn linked_rack(rack_id: &str) -> Option<LinkedRack> {
    lock(&RACKS).get(rack_id).cloned()
}

/// Ids of every linked rack, in a stable order.
fn linked_rack_ids() -> Vec<String> {
    let mut ids: Vec<String> = lock(&RACKS).keys().cloned().collect();
    ids.sort();
    ids
}

/// Every linked rack with its serial port.
fn linked_rack_ports() -> Vec<(String, SharedPort)> {
    lock(&RACKS)
        .iter()
        .map(|(rack_id, rack)| (rack_id.clone(), rack.port.clone()))
        .collect()
}

/// True while `port` is the live serial port of this rack. A task holding a
/// port clone checks this before publishing a result: the port it ran on may
/// belong to a life of the rack that already ended with `link down`, and the
/// server must never hear from that life again (a stale reply can match a
/// fresh request id of the next life and abort its discovery).
fn rack_port_is_live(rack_id: &str, port: &SharedPort) -> bool {
    lock(&RACKS)
        .get(rack_id)
        .map(|rack| Arc::ptr_eq(&rack.port, port))
        .unwrap_or(false)
}

/// Forgets the cached replies of every rack. Called on every CONNACK of the
/// application connection: the server starts a new instance per connection,
/// and with it every rack's request id counter restarts at 1.
fn reset_all_rack_idempotency() {
    for rack in lock(&RACKS).values_mut() {
        // A request in flight keeps the old slot; the new one starts empty.
        rack.idempotency = Arc::new(AsyncMutex::new(IdempotencySlot::default()));
    }
}

/// Registers a rack whose port just opened and tells the server about it.
fn link_rack(rack_id: String, port: SharedPort) -> Vec<(String, bool)> {
    log::info!("RACK {} | [LINK] phase=start", rack_id);
    lock(&RACKS).insert(
        rack_id.clone(),
        LinkedRack {
            port,
            idempotency: Arc::new(AsyncMutex::new(IdempotencySlot::default())),
        },
    );
    vec![(rack_id, true)]
}

/// Tears down the sessions of every rack so they are rebuilt with fresh
/// config, and asks the server to walk every rack again. The card session
/// loops resolve the broker host once at start, so a server-host change must
/// go through a full restart; the ports stay open. The `link up` re-runs the
/// discovery on whichever server instance the application connection ends up
/// talking to: the current one, or the replacement's own CONNACK repeats it.
pub fn restart_rack_links(reason: &str) {
    let ids = linked_rack_ids();
    if ids.is_empty() {
        return;
    }
    log::info!(
        "RACK | [LINK] phase=restart reason={} racks={}",
        reason,
        ids.len()
    );
    for id in &ids {
        stop_rack_watch(id);
        stop_rack_cards(id);
    }
    reset_all_rack_idempotency();
    announce_links(ids.into_iter().map(|id| (id, true)).collect());
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

/// Unlinks one rack: its presence watch and card sessions go with it (without
/// the rack there is no transport to its cards), and the server gets its
/// `link down` — queued into `links`, so the caller can publish it in order
/// with whatever it announces next. Dropping the port handle from the map is
/// what closes the COM handle once the tasks release their clones.
///
/// Returns the rack's port handle when one was registered, so callers that are
/// about to REOPEN the same device can wait for it to actually close (see
/// `stop_rack_and_await_port_release`). `abort()` only marks a task for
/// cancellation, so the port stays open until every task actually yields and
/// drops its `Arc` clone.
fn unlink_rack(rack_id: &str, links: &mut Vec<(String, bool)>) -> Option<SharedPort> {
    // The device node is no longer ours; discovery may pick a different alias
    // for this rack from now on.
    lock(&RACK_ACTIVE_PORTS).remove(rack_id);
    let port = lock(&RACKS).remove(rack_id).map(|rack| {
        log::info!("RACK {} | [LINK] phase=stop", rack_id);
        rack.port
    });
    stop_rack_watch(rack_id);
    stop_rack_cards(rack_id);
    if port.is_some() {
        links.push((rack_id.to_string(), false));
    }
    port
}

/// `unlink_rack` with its `link down` published right away.
fn stop_rack(rack_id: &str) -> Option<SharedPort> {
    let mut links = Vec::new();
    let port = unlink_rack(rack_id, &mut links);
    announce_links(links);
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
/// `abort()` merely schedules cancellation, so the watch task, the card
/// sessions and an exchange in flight still hold `Arc` clones of the port for
/// a short while. Reopening in that window fails with "Device or resource
/// busy" — the app competing with itself for the port it just gave up — which
/// tore down a working rack session and left the rack dark until a later
/// retry tick.
///
/// Waits for the last clone to drop (the `Arc` strong count falling to one, our
/// own), then drops it, which closes the OS handle.
async fn stop_rack_and_await_port_release(rack_id: &str) {
    let Some(port) = stop_rack(rack_id) else {
        return;
    };

    let deadline = std::time::Instant::now() + PORT_RELEASE_TIMEOUT;
    while Arc::strong_count(&port) > 1 && std::time::Instant::now() < deadline {
        tokio::time::sleep(PORT_RELEASE_POLL).await;
    }

    if Arc::strong_count(&port) > 1 {
        log::warn!(
            "RACK {} | phase=stop status=port_still_referenced holders={} timeout_ms={}",
            rack_id,
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
    let ids = linked_rack_ids();
    let mut links = Vec::with_capacity(ids.len());
    for id in &ids {
        unlink_rack(id, &mut links);
    }
    announce_links(links);
    stop_all_rack_watches();
    stop_all_rack_cards();
}

/// Called when a rack transitions to connected. Logs readiness, emits the
/// frontend event, and links the rack to the server over the application
/// connection, wired to the open serial port.
fn on_rack_connected(rack: &RackInfo, port: Box<dyn SerialPort>) {
    // The shutdown flag may have been set between the monitor's loop-top check
    // and this call (find_racks + open_rack take hundreds of ms): linking the
    // rack now would leave an open COM handle and live tasks that nothing will
    // ever stop. Dropping `port` here closes the handle.
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

    let rack_id = rack.rack_id();
    log::info!(
        "RACK | phase=ready status=rack_connected_ready_for_work serial={} rack_id={}",
        rack.serial.as_deref().unwrap_or("?"),
        rack_id
    );

    // Tell the frontend the rack is present. The card list is empty for now —
    // the server reports the set of cards in the rack's slots once it has
    // walked them.
    rack_emit_event(rack.to_state(true));

    // A (re)connect starts from a clean slate: kill this rack's previous
    // sessions — they hold a handle to the old (stale) serial port. No need to
    // await its release here: our replacement port is already open, and the
    // old handle refers to a descriptor we are not going to reopen. The
    // server hears `down` then `up`, in this order, from one publish task.
    let mut links = Vec::with_capacity(2);
    let _ = unlink_rack(&rack_id, &mut links);

    // Remember which device node backs this rack, so a later alias for the same
    // device cannot displace it during discovery.
    lock(&RACK_ACTIVE_PORTS).insert(rack_id.clone(), rack.port_name.clone());

    // Link the rack to the server: from now on its `rack/<id>/request/...`
    // envelopes are written straight to this port.
    let shared_port: SharedPort = Arc::new(AsyncMutex::new(port));
    links.extend(link_rack(rack_id, shared_port));
    announce_links(links);
}

/// Called when a rack transitions to disconnected.
fn on_rack_disconnected(rack: &RackInfo) {
    announce_rack_disconnected(rack);

    // Tear down this rack's link, watch and card sessions.
    stop_rack(&rack.rack_id());
}

/// Same as `on_rack_disconnected`, but waits for the serial port to be released.
/// Used when the caller is about to reopen the same physical device.
async fn on_rack_disconnected_awaiting_port(rack: &RackInfo) {
    announce_rack_disconnected(rack);
    stop_rack_and_await_port_release(&rack.rack_id()).await;
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

    // The racks we currently consider connected, keyed by rack id.
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
            .map(|rack| (rack.rack_id(), rack))
            .collect();

        // Disappeared racks. A device swapped under the same rack id cannot
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
