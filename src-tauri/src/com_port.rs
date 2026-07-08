//! Card rack over the COM (serial) port — client-side wrapper.
//!
//! This module is intentionally a **thin wrapper**: it watches the USB-serial
//! bus for the rack device, logs connect/disconnect transitions, and (once the
//! rack's MQTT connection is in place) bridges bytes between the server and the
//! serial port. It contains **no command/wire protocol** — every command is
//! built and interpreted by the server; the client only forwards raw bytes.
//!
//! Presence detection mirrors how `smart_card::sc_monitor` watches for cards:
//! a continuous monitor loop that reacts to the device appearing and
//! disappearing. There is no serial PnP notification, so we poll the port list
//! (liveness = the port is present on the bus), without speaking the protocol.
//!
//! Runs as its own `async_runtime::spawn` task in `lib.rs`, concurrently with
//! the PCSC reader monitor — a plugged-in reader and a connected rack work in
//! parallel and neither blocks the other.

// ───── Std Lib ─────
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ───── Serial ─────
use serialport::{SerialPort, SerialPortType};

// ───── MQTT Client Library (rumqttc) ─────
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::{AsyncClient, Event, Incoming, MqttOptions};

// ───── Async ─────
use tauri::async_runtime::{self, JoinHandle};
use tokio::sync::Mutex as AsyncMutex;

// ───── Local Modules ─────
use crate::config::{get_from_cache, split_host_to_parts, CacheSection};
use crate::global_app_handle::{rack_emit_event, RackState};

/// The open serial port to the rack, shared between the monitor (which opens it)
/// and the MQTT task (which writes server commands to it and reads replies).
type SharedPort = Arc<AsyncMutex<Box<dyn SerialPort>>>;

/// How long to wait for the rack's serial reply after writing a command.
const SERIAL_REPLY_TIMEOUT: Duration = Duration::from_millis(800);

/// Upper bound on a single serial reply. A healthy rack answers with small
/// frames; hitting this means the device is streaming garbage. Without the cap
/// a device that never stops sending would grow the buffer without bound and
/// keep the read loop (and the port lock) stuck forever.
const SERIAL_REPLY_MAX_BYTES: usize = 64 * 1024;

/// Hard deadline for the whole read phase of one command. The per-read timeout
/// (`SERIAL_REPLY_TIMEOUT`) only fires on a *silent* line — a device that keeps
/// the line busy resets it on every byte, so the loop also needs a total bound.
const SERIAL_READ_DEADLINE: Duration = Duration::from_secs(5);

/// Initial / capped reconnect backoff for the rack's MQTT connection — same
/// policy as the app and per-card connections.
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

lazy_static::lazy_static! {
    /// Handle to the rack's MQTT task, if one is running. Started when the rack
    /// connects and aborted when it disconnects, so there is at most one rack
    /// MQTT connection at a time.
    static ref RACK_MQTT_TASK: std::sync::Mutex<Option<JoinHandle<()>>> =
        std::sync::Mutex::new(None);
}

/// USB product string the rack advertises. Combined with the brand marker in
/// the manufacturer string (see `RACK_BRAND_MARKER`), this identifies a
/// supported rack. We deliberately do NOT match on USB vid/pid — those belong to
/// the underlying USB-serial chip (shared across many unrelated adapters and
/// liable to change between hardware revisions). vid/pid are only read and
/// logged, to collect data for now.
const RACK_PRODUCT: &str = "Smart Card Rack";

/// Serial line settings for the rack link.
const BAUD: u32 = 115_200;

/// How often the monitor scans the bus for the rack appearing/disappearing.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// MQTT client_id construction for the rack connection.
///
/// The id must match the server's `^[0-9A-Z]{16}$` (exactly 16 uppercase
/// alphanumerics). Layout is `BRAND` + filler zeros + `SERIAL`: the brand comes
/// from the device's own manufacturer string (no hard-coded value), the serial
/// is kept flush at the end, and the gap between them is padded with zeros.
const RACK_ID_LEN: usize = 16; // server contract: exactly 16 chars
const RACK_ID_BRAND_LEN: usize = 5; // chars of the brand kept in the id
const RACK_ID_PAD: char = '0'; // filler placed between brand and serial

/// Substring (case-insensitive) the manufacturer must contain for the device to
/// be treated as a supported rack.
const RACK_BRAND_MARKER: &str = "lisle";

/// Extracts the brand prefix for the client_id from the manufacturer string:
/// the first whitespace-separated word, kept to `[0-9A-Z]`, uppercased, and
/// limited to `RACK_ID_BRAND_LEN` chars. e.g. "Lisle Design Ltd" -> "LISLE".
fn brand_prefix(manufacturer: &str) -> String {
    manufacturer
        .split_whitespace()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .take(RACK_ID_BRAND_LEN)
        .collect()
}

/// Builds the rack's MQTT client_id as `<brand>` + zero filler + sanitised
/// serial, uppercased, kept to `[0-9A-Z]`, exactly 16 chars. The serial sits
/// flush at the end; if brand + serial would exceed 16, the serial is truncated
/// to its trailing chars (no filler).
///
/// Examples (manufacturer, serial → id):
///   "Lisle Design Ltd", "SC1799" → "LISLE00000SC1799"
///   "Lisle Design Ltd", none/""  → "LISLE00000000000"
fn build_client_id(manufacturer: &str, serial: Option<&str>) -> String {
    let brand = brand_prefix(manufacturer);

    // Keep only [0-9A-Z] from the serial, uppercased.
    let mut serial_clean: String = serial
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect();

    // Space available for the serial after the brand.
    let serial_room = RACK_ID_LEN - brand.len();
    if serial_clean.len() > serial_room {
        // Too long: keep the trailing chars so the serial stays flush at the end.
        serial_clean = serial_clean[serial_clean.len() - serial_room..].to_string();
    }

    let mut id = String::with_capacity(RACK_ID_LEN);
    id.push_str(&brand);
    // Filler zeros between brand and serial, so the serial ends at position 16.
    for _ in 0..(serial_room - serial_clean.len()) {
        id.push(RACK_ID_PAD);
    }
    id.push_str(&serial_clean);
    id
}

/// Details of a discovered rack device, for logging and later use. `vid`/`pid`
/// are kept only for logging/data collection — they are not used for matching.
#[derive(Clone, PartialEq, Eq, Debug)]
struct RackInfo {
    port_name: String,
    serial: Option<String>,
    manufacturer: Option<String>,
    product: Option<String>,
    vid: u16,
    pid: u16,
}

impl RackInfo {
    /// True if the device manufacturer string marks it as a supported rack
    /// (contains the brand marker, case-insensitive).
    fn is_supported(&self) -> bool {
        self.manufacturer
            .as_deref()
            .map(|m| m.to_ascii_lowercase().contains(RACK_BRAND_MARKER))
            .unwrap_or(false)
    }

    /// The MQTT client_id the server uses to address this rack. The brand prefix
    /// is derived from the device's own manufacturer string.
    fn client_id(&self) -> String {
        build_client_id(self.manufacturer.as_deref().unwrap_or(""), self.serial.as_deref())
    }

    /// Build the frontend payload for this rack. The card list is empty for now;
    /// it will be filled once the server reports the cards in the rack's slots.
    fn to_state(&self, connected: bool) -> RackState {
        RackState {
            connected,
            name: self
                .product
                .clone()
                .unwrap_or_else(|| "Card Rack".to_string()),
            serial: self.serial.clone(),
            manufacturer: self.manufacturer.clone(),
            product: self.product.clone(),
            vid: Some(self.vid),
            pid: Some(self.pid),
            cards: Vec::new(),
        }
    }
}

/// Find a candidate rack: a USB device whose product matches and whose
/// manufacturer contains the brand marker (case-insensitive). vid/pid are
/// recorded for logging but never used as match criteria.
fn find_rack() -> Option<RackInfo> {
    let ports = serialport::available_ports().ok()?;

    for p in &ports {
        if let SerialPortType::UsbPort(info) = &p.port_type {
            let manufacturer_ok = info
                .manufacturer
                .as_deref()
                .map(|m| m.to_ascii_lowercase().contains(RACK_BRAND_MARKER))
                .unwrap_or(false);
            if manufacturer_ok && info.product.as_deref() == Some(RACK_PRODUCT) {
                return Some(RackInfo {
                    port_name: p.port_name.clone(),
                    serial: info.serial_number.clone(),
                    manufacturer: info.manufacturer.clone(),
                    product: info.product.clone(),
                    vid: info.vid,
                    pid: info.pid,
                });
            }
        }
    }
    None
}

/// Try to open the rack's serial port (8N1). Logs success/failure.
fn open_rack(rack: &RackInfo) -> Option<Box<dyn serialport::SerialPort>> {
    match serialport::new(&rack.port_name, BAUD)
        .data_bits(serialport::DataBits::Eight)
        .stop_bits(serialport::StopBits::One)
        .parity(serialport::Parity::None)
        .timeout(SERIAL_REPLY_TIMEOUT)
        .open()
    {
        Ok(port) => {
            log::info!(
                "[RACK] phase=open status=ok port={} baud={} format=8N1",
                rack.port_name,
                BAUD
            );
            Some(port)
        }
        Err(e) => {
            log::error!(
                "[RACK] phase=open status=failed port={} err={}",
                rack.port_name,
                e
            );
            None
        }
    }
}

/// MQTT event loop for the rack's own connection. Mirrors the app/per-card
/// connections in `mqtt.rs`: connect, poll the event loop, reconnect with
/// exponential backoff. Runs until aborted.
///
/// Server commands arrive as JSON `{"serial_cmd":"<hex>"}`. The hex is decoded
/// and the raw bytes are written straight to the serial port; the rack's reply
/// is read back and published to the response topic. The meaning of the bytes is
/// owned by the server — the client is just a transparent pipe.
async fn rack_mqtt_loop(client_id: String, serial_port: SharedPort) {
    let log_header = format!("RACK {} |", client_id);

    // Do not exit when the server host is missing or invalid — typical on
    // first launch, when the rack is plugged in before the server is
    // configured. Exiting would leave a finished task in RACK_MQTT_TASK and
    // no rack MQTT until the device is re-plugged; poll the config instead.
    let (host, port) = loop {
        let full_host = get_from_cache(CacheSection::Server, "host");
        match split_host_to_parts(&full_host) {
            Ok(hp) => break hp,
            Err(e) => {
                log::warn!(
                    "{} [MQTT] phase=config status=waiting reason=invalid_host err={} retry_secs={}",
                    log_header,
                    e,
                    RECONNECT_DELAY_INITIAL_SECS
                );
                tokio::time::sleep(Duration::from_secs(RECONNECT_DELAY_INITIAL_SECS)).await;
            }
        }
    };

    let mut mqtt_options = MqttOptions::new(&client_id, &host, port);
    mqtt_options.set_keep_alive(Duration::from_secs(120));
    log::info!(
        "{} [MQTT] phase=connect_attempt status=initialized host={}:{}",
        log_header,
        host,
        port
    );

    let (mqtt_client, mut eventloop) = AsyncClient::new(mqtt_options, 10);

    let mut is_online = false;
    let mut reconnect_delay_secs = RECONNECT_DELAY_INITIAL_SECS;

    // Idempotency state for the current MQTT connection: the server re-sends a request with the
    // same id when it does not get a timely reply. Remember the last id we answered and its reply
    // so a repeat re-sends the cached response instead of re-forwarding to the rack. Reset on every
    // CONNACK, because a new MQTT session restarts the server-side request_id counter at 1.
    let mut last_request_id: Option<u64> = None;
    let mut last_response_payload: Option<String> = None;

    loop {
        match eventloop.poll().await {
            Ok(notification) => {
                if !is_online {
                    is_online = true;
                    reconnect_delay_secs = RECONNECT_DELAY_INITIAL_SECS;
                    log::info!(
                        "{} [MQTT] state=OFFLINE->ONLINE cause=eventloop_poll_ok",
                        log_header
                    );
                }

                match notification {
                    Event::Incoming(Incoming::ConnAck(..)) => {
                        // New MQTT session: server restarts request_id at 1, drop the idempotency slot.
                        last_request_id = None;
                        last_response_payload = None;
                        log::info!("{} [MQTT] event=CONNACK status=received", log_header);
                    }
                    Event::Incoming(Incoming::Publish(publish)) => {
                        let topic = String::from_utf8_lossy(&publish.topic).into_owned();
                        let payload_text = String::from_utf8_lossy(&publish.payload);

                        log::info!(
                            "{} [MQTT] event=command topic={} bytes={} qos={:?}",
                            log_header,
                            topic,
                            publish.payload.len(),
                            publish.qos,
                        );
                        log::info!("{} [MQTT] command_text={}", log_header, payload_text);

                        // Idempotency: the server re-sends a request with the same id after a
                        // timeout. If we already answered this id, re-send the cached response
                        // without forwarding to the rack again; if it is still in flight, drop it.
                        let req_id = request_id_from_topic(&topic);
                        if req_id.is_some() && req_id == last_request_id {
                            match &last_response_payload {
                                Some(cached) => {
                                    log::warn!(
                                        "{} [MQTT] duplicate request_id={:?}: re-sending cached response, skipping serial exchange",
                                        log_header,
                                        req_id
                                    );
                                    let resp_topic = request_to_response_topic(&topic);
                                    if let Err(e) = mqtt_client
                                        .publish(resp_topic, QoS::AtLeastOnce, false, cached.clone())
                                        .await
                                    {
                                        log::error!("{} [MQTT] cached reply publish failed: {:?}", log_header, e);
                                    }
                                }
                                None => log::warn!(
                                    "{} [MQTT] duplicate request_id={:?} still in flight: ignoring",
                                    log_header,
                                    req_id
                                ),
                            }
                            continue;
                        }

                        // Extract the `serial_cmd` hex string from the JSON command.
                        let serial_hex = match serde_json::from_slice::<serde_json::Value>(
                            &publish.payload,
                        ) {
                            Ok(json) => json
                                .get("serial_cmd")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                            Err(e) => {
                                log::warn!("{} [MQTT] command ignored: bad JSON: {}", log_header, e);
                                None
                            }
                        };

                        if let Some(serial_hex) = serial_hex {
                            // Forward the bytes to the rack and publish its reply
                            // back on the matching response topic.
                            let reply =
                                forward_to_serial(&serial_port, &serial_hex, &log_header).await;
                            if let Some(reply_hex) = reply {
                                let resp_topic = request_to_response_topic(&topic);
                                let resp_payload =
                                    serde_json::json!({ "serial_resp": reply_hex }).to_string();
                                // Cache this reply so a re-sent request with the same id is answered
                                // from cache without forwarding to the rack again.
                                if req_id.is_some() {
                                    last_request_id = req_id;
                                    last_response_payload = Some(resp_payload.clone());
                                }
                                if let Err(e) = mqtt_client
                                    .publish(resp_topic, QoS::AtLeastOnce, false, resp_payload)
                                    .await
                                {
                                    log::error!("{} [MQTT] reply publish failed: {:?}", log_header, e);
                                }
                            }
                        } else {
                            log::warn!(
                                "{} [MQTT] command has no 'serial_cmd' field, nothing to forward",
                                log_header
                            );
                        }
                    }
                    other => {
                        // Trace every other incoming/outgoing event so the full
                        // exchange with the broker is visible while we wire things up.
                        log::info!("{} [MQTT] event=other detail={:?}", log_header, other);
                    }
                }
            }
            Err(e) => {
                if is_online {
                    log::warn!("{} [MQTT] state=ONLINE->OFFLINE err={:?}", log_header, e);
                    is_online = false;
                } else {
                    log::warn!("{} [MQTT] state=OFFLINE err={:?}", log_header, e);
                }
                log::warn!(
                    "{} [MQTT] action=reconnect_scheduled delay_secs={}",
                    log_header,
                    reconnect_delay_secs
                );
                tokio::time::sleep(Duration::from_secs(reconnect_delay_secs)).await;
                reconnect_delay_secs = next_reconnect_delay(reconnect_delay_secs);
            }
        }
    }
}

/// Rewrites a leading `request/` topic segment to `response/` for replies.
/// Returns the topic unchanged if it has no `request/` prefix.
fn request_to_response_topic(topic: &str) -> String {
    match topic.strip_prefix("request/") {
        Some(rest) => format!("response/{}", rest),
        None => topic.to_string(),
    }
}

/// Extracts the request id (first segment after `request/`) from a topic of the
/// form `request/<id>/<sender>`. Used for idempotent handling of repeated requests:
/// the server re-sends the same id when it does not get a timely reply.
fn request_id_from_topic(topic: &str) -> Option<u64> {
    topic
        .strip_prefix("request/")
        .and_then(|rest| rest.split('/').next())
        .and_then(|id| id.parse::<u64>().ok())
}

/// Decodes a hex command, writes the raw bytes to the rack's serial port, and
/// reads whatever the rack sends back within `SERIAL_REPLY_TIMEOUT`. Returns the
/// reply as an uppercase hex string, or `None` on a write error / no reply.
///
/// This is a transparent pipe: the bytes' meaning is decided by the server. The
/// blocking serial I/O runs on a blocking thread so the async runtime isn't stalled.
async fn forward_to_serial(port: &SharedPort, serial_hex: &str, log_header: &str) -> Option<String> {
    let bytes = match hex::decode(serial_hex) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("{} [SERIAL] bad hex in serial_cmd: {}", log_header, e);
            return None;
        }
    };

    log::info!("{} [SERIAL] tx bytes={} hex={}", log_header, bytes.len(), serial_hex);

    let port = port.clone();
    let log_header_blocking = log_header.to_string();

    // serialport is blocking; do the write+read on a blocking thread.
    let result = tokio::task::spawn_blocking(move || {
        let mut guard = port.blocking_lock();
        if let Err(e) = guard.write_all(&bytes) {
            log::error!("{} [SERIAL] write failed: {}", log_header_blocking, e);
            return None;
        }
        let _ = guard.flush();

        // Read whatever arrives until the inter-byte timeout fires. The port's
        // own read timeout bounds each read; we stop on the first timeout once
        // we have some data, or return nothing if the rack stays silent.
        //
        // Two hard bounds protect against a misbehaving device that streams
        // bytes continuously (each read would then succeed before the timeout
        // and the loop would never exit): a cap on the reply size and an
        // overall deadline for the whole read phase.
        let mut reply = Vec::new();
        let mut buf = [0u8; 512];
        let deadline = std::time::Instant::now() + SERIAL_READ_DEADLINE;
        loop {
            if reply.len() >= SERIAL_REPLY_MAX_BYTES {
                log::warn!(
                    "{} [SERIAL] reply exceeded {} bytes — truncating, device is misbehaving",
                    log_header_blocking,
                    SERIAL_REPLY_MAX_BYTES
                );
                break;
            }
            if std::time::Instant::now() >= deadline {
                log::warn!(
                    "{} [SERIAL] read deadline {:?} reached — returning {} bytes read so far",
                    log_header_blocking,
                    SERIAL_READ_DEADLINE,
                    reply.len()
                );
                break;
            }
            match guard.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => reply.extend_from_slice(&buf[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => break,
                Err(e) => {
                    log::warn!("{} [SERIAL] read error: {}", log_header_blocking, e);
                    break;
                }
            }
        }
        if reply.is_empty() {
            None
        } else {
            Some(hex::encode_upper(&reply))
        }
    })
    .await
    .ok()
    .flatten();

    match &result {
        Some(reply_hex) => log::info!("{} [SERIAL] rx hex={}", log_header, reply_hex),
        None => log::warn!("{} [SERIAL] no reply from rack", log_header),
    }
    result
}

/// True if the rack MQTT task slot holds a task that is still running.
/// A task that finished on its own (panicked or returned) does not count —
/// its stale handle is dropped so a new task can be started. Without this
/// check the slot would look "occupied" forever and the rack MQTT connection
/// could never come back without re-plugging the device.
fn rack_mqtt_running() -> bool {
    let mut guard = match RACK_MQTT_TASK.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    match guard.as_ref() {
        Some(handle) if handle.inner().is_finished() => {
            log::warn!("[RACK] [MQTT] task exited on its own — clearing stale handle");
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
        log::debug!("[RACK] [MQTT] start skipped: task already running");
        return;
    }
    let mut guard = match RACK_MQTT_TASK.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    log::info!("[RACK] [MQTT] phase=start client_id={}", client_id);
    let handle = async_runtime::spawn(rack_mqtt_loop(client_id, port));
    *guard = Some(handle);
}

/// Stops the rack's MQTT task if one is running.
fn stop_rack_mqtt() {
    let mut guard = match RACK_MQTT_TASK.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(handle) = guard.take() {
        handle.abort();
        log::info!("[RACK] [MQTT] phase=stop status=aborted");
    }
}

/// Called when the rack transitions to connected. Logs readiness, emits the
/// frontend event, and starts the rack's own MQTT connection wired to the open
/// serial port.
fn on_rack_connected(rack: &RackInfo, port: Box<dyn SerialPort>) {
    // vid/pid logged for data collection only — matching is by product string.
    log::info!(
        "[RACK] phase=discovery status=found port={} serial={} manufacturer={} product={} vid={:#06x} pid={:#06x}",
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
            "[RACK] phase=ready status=unsupported reason=manufacturer_not_lisle manufacturer={} \
             detail=not_a_lisle_design_tachograph_rack",
            rack.manufacturer.as_deref().unwrap_or("?")
        );
        return;
    }

    let client_id = rack.client_id();
    log::info!(
        "[RACK] phase=ready status=rack_connected_ready_for_work serial={} client_id={}",
        rack.serial.as_deref().unwrap_or("?"),
        client_id
    );

    // Tell the frontend the rack is present. The card list is empty for now —
    // the server doesn't yet report the cards held in the rack's slots.
    rack_emit_event(rack.to_state(true));

    // Open the rack's own MQTT connection wired to the serial port, and wait for
    // server commands. Each `serial_cmd` is written straight to this port.
    let shared_port: SharedPort = Arc::new(AsyncMutex::new(port));
    start_rack_mqtt(client_id, shared_port);
}

/// Called when the rack transitions to disconnected.
fn on_rack_disconnected(rack: &RackInfo) {
    log::warn!(
        "[RACK] phase=presence status=disconnected port={} serial={}",
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
        log::debug!("[RACK] phase=rack_connection status=already_running");
        return;
    }

    log::info!("[RACK] phase=rack_connection status=start poll_secs={}", POLL_INTERVAL.as_secs());

    // The rack we currently consider connected, if any.
    let mut current: Option<RackInfo> = None;

    loop {
        let found = find_rack();

        match (&current, found) {
            // Newly appeared.
            (None, Some(rack)) => {
                if let Some(port) = open_rack(&rack) {
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
                if let Some(port) = open_rack(&rack) {
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
                    "[RACK] phase=presence status=mqtt_task_dead port={} action=restart",
                    rack.port_name
                );
                if let Some(port) = open_rack(&rack) {
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

#[cfg(test)]
mod tests {
    use super::*;

    const MFR: &str = "Lisle Design Ltd"; // sample manufacturer string

    #[test]
    fn rack_product_string_is_set() {
        assert!(!RACK_PRODUCT.is_empty());
        assert!(!RACK_BRAND_MARKER.is_empty());
    }

    #[test]
    fn response_topic_swaps_request_prefix_only() {
        assert_eq!(request_to_response_topic("request/0"), "response/0");
        assert_eq!(request_to_response_topic("request/1/ABC"), "response/1/ABC");
        // No request/ prefix → unchanged.
        assert_eq!(request_to_response_topic("status/x"), "status/x");
    }

    #[test]
    fn request_id_from_topic_parses_first_segment_only() {
        assert_eq!(request_id_from_topic("request/42/RACK0000000000AB"), Some(42));
        assert_eq!(request_id_from_topic("request/0"), Some(0));
        // Non-numeric id or wrong prefix → None.
        assert_eq!(request_id_from_topic("request/abc/X"), None);
        assert_eq!(request_id_from_topic("status/1/X"), None);
    }

    // The server contract: client_id must match ^[0-9A-Z]{16}$.
    fn matches_server_contract(id: &str) -> bool {
        id.len() == 16 && id.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    }

    #[test]
    fn brand_prefix_from_manufacturer() {
        assert_eq!(brand_prefix("Lisle Design Ltd"), "LISLE");
        assert_eq!(brand_prefix("lisle design ltd"), "LISLE"); // case-insensitive
        assert_eq!(brand_prefix("Acme Co"), "ACME"); // shorter first word
        assert_eq!(brand_prefix(""), ""); // empty
    }

    #[test]
    fn client_id_for_sc1799() {
        // Brand (from manufacturer) + zero filler + serial; serial flush at end.
        assert_eq!(build_client_id(MFR, Some("SC1799")), "LISLE00000SC1799");
    }

    #[test]
    fn client_id_serial_is_flush_at_end() {
        // Whatever the serial length, it must end the id (filler in the middle).
        assert!(build_client_id(MFR, Some("SC1799")).ends_with("SC1799"));
        assert!(build_client_id(MFR, Some("AB")).ends_with("AB"));
    }

    #[test]
    fn client_id_always_matches_server_contract() {
        for serial in [
            Some("SC1799"),
            Some("sc1799"),                 // lowercase gets uppercased
            Some("SC-17/99"),               // punctuation stripped
            Some(""),                       // empty serial
            None,                           // no serial at all
            Some("VERYLONGSERIALNUMBER123"),// longer than 16 → truncated
        ] {
            let id = build_client_id(MFR, serial);
            assert!(
                matches_server_contract(&id),
                "id {:?} from serial {:?} violates ^[0-9A-Z]{{16}}$",
                id,
                serial
            );
        }
    }

    #[test]
    fn client_id_starts_with_brand_from_manufacturer() {
        assert!(build_client_id(MFR, Some("ANYTHING")).starts_with("LISLE"));
    }

    #[test]
    fn client_id_empty_serial_is_padded() {
        assert_eq!(build_client_id(MFR, None), "LISLE00000000000");
    }

    #[test]
    fn client_id_long_serial_keeps_trailing_chars() {
        // Serial longer than the room → keep its tail, still 16 and contract-valid.
        let id = build_client_id(MFR, Some("VERYLONGSERIAL123"));
        assert!(matches_server_contract(&id));
        assert!(id.starts_with("LISLE"));
        // 11 chars of room after "LISLE" → trailing 11 of the serial.
        assert_eq!(id, "LISLENGSERIAL123");
    }

    #[test]
    fn is_supported_checks_manufacturer_marker() {
        let base = RackInfo {
            port_name: "/dev/x".into(),
            serial: Some("SC1".into()),
            manufacturer: Some("Lisle Design Ltd".into()),
            product: None,
            vid: 0,
            pid: 0,
        };
        assert!(base.is_supported());
        assert!(RackInfo { manufacturer: Some("LISLE DESIGN".into()), ..base.clone() }.is_supported());
        assert!(!RackInfo { manufacturer: Some("Acme Co".into()), ..base.clone() }.is_supported());
        assert!(!RackInfo { manufacturer: None, ..base.clone() }.is_supported());
    }

    #[test]
    fn rack_info_equality_detects_swap() {
        let a = RackInfo {
            port_name: "/dev/x".into(),
            serial: Some("SC1".into()),
            manufacturer: None,
            product: None,
            vid: 0,
            pid: 0,
        };
        let b = RackInfo {
            serial: Some("SC2".into()),
            ..a.clone()
        };
        assert_ne!(a, b); // different serial => treated as a different device
        assert_eq!(a, a.clone());
    }
}
