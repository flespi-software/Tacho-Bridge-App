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
use crate::global_app_handle::{
    emit_notification_event, rack_emit_event, rack_update_cards, NotificationPayload, RackCard,
    RackState,
};
use crate::smart_card::TASK_POOL;

/// The open serial port to the rack, shared between the monitor (which opens it)
/// and the MQTT task (which writes server commands to it and reads replies).
type SharedPort = Arc<AsyncMutex<Box<dyn SerialPort>>>;

/// Line silence that ends one serial reply, when the server does not supply `idle_ms`.
/// This is an *inter-byte* bound applied only after the reply has started — the wait for
/// the first byte is governed by `SERIAL_READ_DEADLINE` instead, because it scales with
/// the command size and the USB-serial adapter's latency.
const SERIAL_REPLY_TIMEOUT: Duration = Duration::from_millis(800);

/// Upper bound on a single serial reply. A healthy rack answers with small
/// frames; hitting this means the device is streaming garbage. Without the cap
/// a device that never stops sending would grow the buffer without bound and
/// keep the read loop (and the port lock) stuck forever.
const SERIAL_REPLY_MAX_BYTES: usize = 64 * 1024;

/// Hard deadline for the whole read phase of one command, and the budget for the
/// rack's first reply byte. The inter-byte timeout (`SERIAL_REPLY_TIMEOUT`) only fires
/// on a *silent* line — a device that keeps the line busy resets it on every byte, so
/// the loop also needs a total bound.
const SERIAL_READ_DEADLINE: Duration = Duration::from_secs(5);

/// `serial_err` codes of the v2 response contract. The response envelope is
/// published for EVERY request; on success the code is an empty string. The
/// server is the only consumer — codes are part of the wire contract, do not
/// rename them.
const SERIAL_ERR_NO_REPLY: &str = "no_reply";
const SERIAL_ERR_WRITE_FAILED: &str = "write_failed";
const SERIAL_ERR_BAD_HEX: &str = "bad_hex";
const SERIAL_ERR_TRUNCATED: &str = "truncated";

/// Outcome of one server command → rack exchange, mirroring the v2 response
/// envelope: `resp_hex` carries whatever bytes came back (possibly empty, or
/// partial on truncation), `err` is `""` on success or one of the
/// `SERIAL_ERR_*` codes. Published for every request — the app is the server's
/// only feedback channel, so "rack stayed silent" must be distinguishable from
/// "message lost".
#[derive(Debug, Clone, PartialEq, Eq)]
struct SerialExchange {
    resp_hex: String,
    err: &'static str,
}

impl SerialExchange {
    /// Successful exchange: the rack answered with these bytes.
    fn ok(resp_hex: String) -> Self {
        Self { resp_hex, err: "" }
    }

    /// Failed exchange with no data to return.
    fn error(err: &'static str) -> Self {
        Self {
            resp_hex: String::new(),
            err,
        }
    }

    /// True when the exchange fully succeeded (the only cacheable outcome).
    fn is_ok(&self) -> bool {
        self.err.is_empty()
    }

    /// JSON payload for the response topic: always the same two fields, so the
    /// server-side parser never has to branch on the structure.
    fn to_payload(&self) -> String {
        serde_json::json!({ "serial_resp": self.resp_hex, "serial_err": self.err }).to_string()
    }
}

/// Poll spec defaults when the server omits the optional timing fields.
const POLL_INTERVAL_DEFAULT: Duration = Duration::from_millis(20);
const POLL_DEADLINE_DEFAULT: Duration = Duration::from_secs(5);

/// Upper bound for every server-supplied timing field (`idle_ms`, `deadline_ms`,
/// `interval_ms`). An unclamped u64 would panic on `Instant + Duration` overflow
/// after the command bytes were already written to the device, and a huge poll
/// interval would pin a blocking-pool thread (and the port lock) beyond any
/// abort — `spawn_blocking` closures cannot be cancelled.
const SERIAL_MS_MAX: u64 = 300_000;

/// Server-scripted poll loop of one envelope: after the command is accepted, keep sending
/// `cmd` every `interval` while the device answers exactly `while_hex`; the first differing
/// reply is the operation result. Pure byte comparison - no protocol knowledge on this side.
#[derive(Debug)]
struct PollSpec {
    cmd_hex: String,
    while_hex: String,
    interval: Duration,
    deadline: Duration,
}

/// One server -> TBA serial exchange envelope: the raw command plus optional reply timings and
/// an opaque poll spec. All hex strings are normalized to uppercase at parse time so later
/// comparisons are plain string equality.
#[derive(Debug)]
struct SerialEnvelope {
    cmd_hex: String,
    /// Predicted "accepted" first reply; any other first reply is returned to the server as is.
    expect_hex: Option<String>,
    /// Line-silence interval that ends a reply already in flight.
    idle: Duration,
    /// Hard bound of the read phase of one exchange, first reply byte included.
    deadline: Duration,
    poll: Option<PollSpec>,
}

/// Validates and uppercases a hex string; the error is the wire contract code.
fn normalize_hex(s: &str) -> Result<String, &'static str> {
    hex::decode(s)
        .map(hex::encode_upper)
        .map_err(|_| SERIAL_ERR_BAD_HEX)
}

/// Parses the envelope from a request payload. `None` when there is no `serial_cmd` field at
/// all (not an envelope); `Some(Err(code))` when the envelope is malformed - the caller still
/// publishes a response with that code (the always-reply contract).
fn parse_envelope(json: &serde_json::Value) -> Option<Result<SerialEnvelope, &'static str>> {
    let cmd = json.get("serial_cmd").and_then(|v| v.as_str())?;
    Some(parse_envelope_fields(json, cmd))
}

fn parse_envelope_fields(
    json: &serde_json::Value,
    cmd: &str,
) -> Result<SerialEnvelope, &'static str> {
    let ms = |v: &serde_json::Value, key: &str| {
        v.get(key)
            .and_then(|x| x.as_u64())
            .map(|x| x.min(SERIAL_MS_MAX))
    };

    let expect_hex = match json.get("expect").and_then(|v| v.as_str()) {
        Some(s) => Some(normalize_hex(s)?),
        None => None,
    };
    let poll = match json.get("poll") {
        Some(p) => {
            // a poll spec without its command/while bytes is a malformed envelope
            let poll_cmd = p
                .get("cmd")
                .and_then(|v| v.as_str())
                .ok_or(SERIAL_ERR_BAD_HEX)?;
            let poll_while = p
                .get("while")
                .and_then(|v| v.as_str())
                .ok_or(SERIAL_ERR_BAD_HEX)?;
            Some(PollSpec {
                cmd_hex: normalize_hex(poll_cmd)?,
                while_hex: normalize_hex(poll_while)?,
                interval: ms(p, "interval_ms")
                    .map(Duration::from_millis)
                    .unwrap_or(POLL_INTERVAL_DEFAULT),
                deadline: ms(p, "deadline_ms")
                    .map(Duration::from_millis)
                    .unwrap_or(POLL_DEADLINE_DEFAULT),
            })
        }
        None => None,
    };
    Ok(SerialEnvelope {
        cmd_hex: normalize_hex(cmd)?,
        expect_hex,
        idle: ms(json, "idle_ms")
            .map(Duration::from_millis)
            .unwrap_or(SERIAL_REPLY_TIMEOUT),
        deadline: ms(json, "deadline_ms")
            .map(Duration::from_millis)
            .unwrap_or(SERIAL_READ_DEADLINE),
        poll,
    })
}

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

/// Fallback identity for platforms where the OS reports the *driver's* strings
/// instead of the device's USB descriptors (Windows: manufacturer="FTDI",
/// product="USB Serial Port (COMx)"), so the string match above can never
/// succeed. The rack is an FTDI FT-X (vid/pid below) whose EEPROM serial
/// follows the Lisle scheme `SC<digits>`; together these identify it. The
/// canonical strings are substituted so the client_id and UI match the other
/// platforms, where the same hardware reports them via its own descriptors.
const RACK_FALLBACK_VID: u16 = 0x0403;
const RACK_FALLBACK_PID: u16 = 0x6015;
const RACK_FALLBACK_MANUFACTURER: &str = "Lisle Design Ltd";

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
///   "Lisle Design Ltd", "SC1234" → "LISLE00000SC1234"
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
        build_client_id(
            self.manufacturer.as_deref().unwrap_or(""),
            self.serial.as_deref(),
        )
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

/// "Once per state change" guard for the 2s poll loop: remembers the last
/// seen value and reports whether a new one differs. Used to keep periodic
/// discovery from logging/notifying the same fact on every tick.
struct ChangeGuard(std::sync::Mutex<String>);

impl ChangeGuard {
    const fn new() -> Self {
        Self(std::sync::Mutex::new(String::new()))
    }

    /// Store `value` and report whether it differs from the previous one.
    fn changed(&self, value: &str) -> bool {
        let mut last = self.0.lock().unwrap();
        if *last == value {
            false
        } else {
            value.clone_into(&mut last);
            true
        }
    }

    /// Forget the stored value so the next `changed` reports true again.
    fn reset(&self) {
        self.0.lock().unwrap().clear();
    }
}

/// Last logged port-inventory snapshot.
static PORT_INVENTORY: ChangeGuard = ChangeGuard::new();

/// Last logged discovery-match outcome.
static DISCOVERY_MATCH: ChangeGuard = ChangeGuard::new();

/// Port for which the "COM port busy" UI notification has already been shown.
/// Reset when the port opens or the rack disappears, so a new busy episode
/// (e.g. after replug) notifies again.
static BUSY_PORT_NOTICE: ChangeGuard = ChangeGuard::new();

/// One-line description of an enumerated port for the inventory log. USB ports
/// carry the metadata `find_rack` matches on; other transports just get their
/// kind, so field reports show why a device was not considered a rack.
fn describe_port(p: &serialport::SerialPortInfo) -> String {
    match &p.port_type {
        SerialPortType::UsbPort(info) => format!(
            "{{port={} type=usb vid={:#06x} pid={:#06x} manufacturer={:?} product={:?} serial={:?}}}",
            p.port_name,
            info.vid,
            info.pid,
            info.manufacturer.as_deref().unwrap_or("-"),
            info.product.as_deref().unwrap_or("-"),
            info.serial_number.as_deref().unwrap_or("-"),
        ),
        SerialPortType::PciPort => format!("{{port={} type=pci}}", p.port_name),
        SerialPortType::BluetoothPort => format!("{{port={} type=bluetooth}}", p.port_name),
        SerialPortType::Unknown => format!("{{port={} type=unknown}}", p.port_name),
    }
}

/// Find a candidate rack: a USB device whose product matches and whose
/// manufacturer contains the brand marker (case-insensitive). vid/pid are
/// recorded for logging but never used as match criteria.
fn find_rack() -> Option<RackInfo> {
    let ports = match serialport::available_ports() {
        Ok(ports) => ports,
        Err(e) => {
            let snapshot = format!("enumeration_failed: {e}");
            if PORT_INVENTORY.changed(&snapshot) {
                log::error!("RACK | phase=discovery status=enumeration_failed err={e}");
            }
            return None;
        }
    };

    // Diagnostic for "rack not found" field reports: shows whether a COM port
    // exists at all and what strings the OS reports for it (on Windows the
    // driver, not the device, supplies manufacturer/product).
    let snapshot = ports
        .iter()
        .map(describe_port)
        .collect::<Vec<_>>()
        .join(" ");
    if PORT_INVENTORY.changed(&snapshot) {
        if ports.is_empty() {
            log::info!("RACK | phase=discovery status=inventory ports=0");
        } else {
            log::info!(
                "RACK | phase=discovery status=inventory ports={} list=[{}]",
                ports.len(),
                snapshot
            );
        }
    }

    // First pass: match by the device's own descriptor strings (macOS/Linux).
    for p in &ports {
        if let SerialPortType::UsbPort(info) = &p.port_type {
            let manufacturer_ok = info
                .manufacturer
                .as_deref()
                .map(|m| m.to_ascii_lowercase().contains(RACK_BRAND_MARKER))
                .unwrap_or(false);
            if manufacturer_ok && info.product.as_deref() == Some(RACK_PRODUCT) {
                DISCOVERY_MATCH.changed(&format!("descriptor:{}", p.port_name));
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

    // Second pass (Windows): the COM node carries the driver's strings, but
    // the parent USB node still holds the device's real iProduct and clean
    // EEPROM serial — match by those.
    #[cfg(windows)]
    if let Some(rack) = find_rack_by_usb_descriptor(&ports) {
        return Some(rack);
    }

    // Last resort: no descriptor strings anywhere — match by the rack's chip
    // identity plus the Lisle serial scheme, and substitute the canonical
    // strings so client_id/UI stay identical across platforms.
    for p in &ports {
        if let SerialPortType::UsbPort(info) = &p.port_type {
            if info.vid != RACK_FALLBACK_VID || info.pid != RACK_FALLBACK_PID {
                continue;
            }
            let Some(serial) = info.serial_number.as_deref().and_then(lisle_serial) else {
                continue;
            };
            if DISCOVERY_MATCH.changed(&format!("chip:{}:{}", p.port_name, serial)) {
                log::info!(
                    "RACK | phase=discovery status=fallback_match port={} serial={} reported_serial={:?} \
                     reported_manufacturer={:?} reason=os_reports_driver_strings",
                    p.port_name,
                    serial,
                    info.serial_number.as_deref().unwrap_or("-"),
                    info.manufacturer.as_deref().unwrap_or("-"),
                );
            }
            return Some(RackInfo {
                port_name: p.port_name.clone(),
                serial: Some(serial),
                manufacturer: Some(RACK_FALLBACK_MANUFACTURER.to_string()),
                product: Some(RACK_PRODUCT.to_string()),
                vid: info.vid,
                pid: info.pid,
            });
        }
    }
    // Reset the change guards so the next appearance of a rack is logged and,
    // if its port is still busy, re-notifies the user.
    DISCOVERY_MATCH.changed("none");
    BUSY_PORT_NOTICE.reset();
    None
}

/// Windows: locate the rack via the parent USB node. The COM node only shows
/// the FTDI driver's strings ("FTDI" / "USB Serial Port (COMx)"), but Windows
/// caches the device's own iProduct on the USB node (`BusReportedDeviceDesc`,
/// surfaced by nusb as `product_string`) along with the clean EEPROM serial —
/// the same values macOS/Linux read from the descriptors directly, so the
/// resulting client_id matches across platforms. The device is never opened.
#[cfg(windows)]
fn find_rack_by_usb_descriptor(ports: &[serialport::SerialPortInfo]) -> Option<RackInfo> {
    use nusb::MaybeFuture;

    let devices = match nusb::list_devices().wait() {
        Ok(devices) => devices,
        Err(e) => {
            log::warn!("RACK | phase=discovery status=usb_enum_failed err={e}");
            return None;
        }
    };

    for dev in devices {
        if dev.product_string() != Some(RACK_PRODUCT) {
            continue;
        }
        let Some(dev_serial) = dev.serial_number() else {
            continue;
        };
        // Link the USB device to its COM port: same chip identity, and the
        // port serial is the device serial plus an optional channel letter
        // appended by the driver ("SC1799" -> "SC1799A").
        for p in ports {
            let SerialPortType::UsbPort(info) = &p.port_type else {
                continue;
            };
            let serial_ok = info
                .serial_number
                .as_deref()
                .map(|s| port_serial_belongs_to_device(dev_serial, s))
                .unwrap_or(false);
            if info.vid != dev.vendor_id() || info.pid != dev.product_id() || !serial_ok {
                continue;
            }
            if DISCOVERY_MATCH.changed(&format!("usb:{}:{dev_serial}", p.port_name)) {
                log::info!(
                    "RACK | phase=discovery status=usb_descriptor_match port={} serial={} \
                     product={:?} vid={:#06x} pid={:#06x}",
                    p.port_name,
                    dev_serial,
                    RACK_PRODUCT,
                    info.vid,
                    info.pid,
                );
            }
            return Some(RackInfo {
                port_name: p.port_name.clone(),
                serial: Some(dev_serial.to_string()),
                // iManufacturer is not reachable on Windows without opening
                // the device; the product string is Lisle's own EEPROM value,
                // so the canonical manufacturer is substituted for branding.
                manufacturer: Some(RACK_FALLBACK_MANUFACTURER.to_string()),
                product: Some(RACK_PRODUCT.to_string()),
                vid: info.vid,
                pid: info.pid,
            });
        }
    }
    None
}

/// True if a COM port's serial belongs to the USB device with `dev_serial`:
/// either equal, or the device serial plus a single trailing channel letter
/// the FTDI driver appends per port ("SC1799" -> "SC1799A").
#[cfg_attr(not(windows), allow(dead_code))]
fn port_serial_belongs_to_device(dev_serial: &str, port_serial: &str) -> bool {
    if dev_serial.is_empty() {
        return false;
    }
    match port_serial.strip_prefix(dev_serial) {
        Some("") => true,
        Some(rest) => rest.len() == 1 && rest.as_bytes()[0].is_ascii_uppercase(),
        None => false,
    }
}

/// Checks whether `serial` follows the Lisle scheme — `SC` + digits, optionally
/// with the trailing channel letter the FTDI Windows driver appends — and
/// returns it normalized to the descriptor form the same hardware reports on
/// other platforms (channel letter stripped): "SC1799A" -> "SC1799".
fn lisle_serial(serial: &str) -> Option<String> {
    let body = serial.strip_prefix("SC")?;
    let body = body.strip_suffix('A').unwrap_or(body);
    (!body.is_empty() && body.bytes().all(|b| b.is_ascii_digit())).then(|| format!("SC{body}"))
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
                "RACK | phase=open status=ok port={} baud={} format=8N1",
                rack.port_name,
                BAUD
            );
            BUSY_PORT_NOTICE.reset();
            Some(port)
        }
        Err(e) => {
            // Access denied on Windows almost always means another process
            // holds the port exclusively — e.g. other tachograph software
            // (Teltonika, BCE) that grabs the rack's COM port on startup.
            let busy = matches!(
                e.kind,
                serialport::ErrorKind::Io(std::io::ErrorKind::PermissionDenied)
            );
            let hint = if busy {
                " hint=port_held_by_another_process(other_tachograph_software_or_second_TBA_instance?)"
            } else {
                ""
            };
            log::error!(
                "RACK | phase=open status=failed port={} err={}{}",
                rack.port_name,
                e,
                hint
            );
            if busy {
                notify_port_busy(&rack.port_name);
            }
            None
        }
    }
}

/// Emits the "COM port busy" UI notification once per busy episode.
fn notify_port_busy(port_name: &str) {
    if !BUSY_PORT_NOTICE.changed(port_name) {
        return;
    }
    emit_notification_event(
        "global-notification",
        NotificationPayload {
            notification_type: "port_busy".to_string(),
            message: format!(
                "Card rack detected on {port_name}, but the port is busy — another application \
                 is using it. Close or uninstall the application that occupies the port \
                 (e.g. other tachograph software), then reconnect the rack."
            ),
        },
    );
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
                        log::debug!("{} [MQTT] event=CONNACK status=received", log_header);
                    }
                    Event::Incoming(Incoming::Publish(publish)) => {
                        let topic = String::from_utf8_lossy(&publish.topic).into_owned();

                        log::info!(
                            "{} [MQTT] event=command topic={} bytes={} qos={:?}",
                            log_header,
                            topic,
                            publish.payload.len(),
                            publish.qos,
                        );
                        // Full command text only at debug: the rack protocol
                        // must not end up in users' log files at INFO level.
                        log::debug!(
                            "{} [MQTT] command_text={}",
                            log_header,
                            String::from_utf8_lossy(&publish.payload)
                        );

                        if topic == "connect" {
                            // spawn instruction: the server discovered a card in a rack slot
                            handle_connect_spawn(&publish.payload, &serial_port, &log_header).await;
                        } else if topic == "watch" {
                            // arm/re-arm the card presence watch with the server-supplied bytes
                            start_rack_watch(
                                &publish.payload,
                                &serial_port,
                                &mqtt_client,
                                &log_header,
                            );
                        } else if topic == "disconnect" {
                            // a card left its slot: close the session, drop it from the UI
                            handle_card_disconnect(&publish.payload, &log_header);
                        } else {
                            handle_serial_request(
                                &mqtt_client,
                                &topic,
                                &publish.payload,
                                &serial_port,
                                &log_header,
                                &mut last_request_id,
                                &mut last_response_payload,
                            )
                            .await;
                        }
                    }
                    other => {
                        // Full broker exchange is visible at debug (TBA_LOG=com_port=debug).
                        log::debug!("{} [MQTT] event=other detail={:?}", log_header, other);
                    }
                }
            }
            Err(e) => {
                let transition = if is_online {
                    "ONLINE->OFFLINE"
                } else {
                    "OFFLINE"
                };
                is_online = false;

                // One line per failed poll: kind + retry delay; full error
                // details only for genuinely unexpected failures.
                crate::mqtt::log_connection_failure(
                    &log_header,
                    "MQTT",
                    transition,
                    &e,
                    reconnect_delay_secs,
                );

                tokio::time::sleep(Duration::from_secs(reconnect_delay_secs)).await;
                reconnect_delay_secs = next_reconnect_delay(reconnect_delay_secs);
            }
        }
    }
}

/// Handles one `request/...` publish on a rack-linked MQTT connection (the rack's own or a
/// rack-backed card's): idempotency, envelope parsing, serial execution, and the always-reply
/// response publish — the app is the server's only feedback channel, so a silent rack must be
/// reported, not swallowed.
async fn handle_serial_request(
    mqtt_client: &AsyncClient,
    topic: &str,
    payload: &[u8],
    serial_port: &SharedPort,
    log_header: &str,
    last_request_id: &mut Option<u64>,
    last_response_payload: &mut Option<String>,
) {
    // Idempotency: the server re-sends a request with the same id after a timeout. If we already
    // answered this id, re-send the cached response without touching the port again; if it is
    // still in flight, drop the duplicate.
    let req_id = request_id_from_topic(topic);
    if req_id.is_some() && req_id == *last_request_id {
        match last_response_payload {
            Some(cached) => {
                log::warn!(
                    "{} [MQTT] status=duplicate_request request_id={:?} action=resend_cached",
                    log_header,
                    req_id
                );
                let resp_topic = request_to_response_topic(topic);
                if let Err(e) = mqtt_client
                    .publish(resp_topic, QoS::AtLeastOnce, false, cached.clone())
                    .await
                {
                    log::error!("{} [MQTT] status=cached_reply_publish_failed err={:?}", log_header, e);
                }
            }
            None => log::warn!(
                "{} [MQTT] status=duplicate_request request_id={:?} action=ignored reason=in_flight",
                log_header,
                req_id
            ),
        }
        return;
    }

    let json = match serde_json::from_slice::<serde_json::Value>(payload) {
        Ok(json) => json,
        Err(e) => {
            log::warn!(
                "{} [MQTT] status=ignored reason=bad_json err={}",
                log_header,
                e
            );
            return;
        }
    };
    let Some(parsed) = parse_envelope(&json) else {
        log::warn!("{} [MQTT] status=ignored reason=no_serial_cmd", log_header);
        return;
    };

    let exchange = match parsed {
        Ok(envelope) => execute_envelope(serial_port, envelope, log_header, true).await,
        Err(code) => SerialExchange::error(code),
    };

    let resp_topic = request_to_response_topic(topic);
    let resp_payload = exchange.to_payload();
    // Only successful exchanges are cached for idempotency: a repeated request after an error
    // must retry the rack (the device may have recovered), a repeat after success is answered
    // from cache without touching the port again.
    if exchange.is_ok() && req_id.is_some() {
        *last_request_id = req_id;
        *last_response_payload = Some(resp_payload.clone());
    }
    if let Err(e) = mqtt_client
        .publish(resp_topic, QoS::AtLeastOnce, false, resp_payload)
        .await
    {
        log::error!(
            "{} [MQTT] status=reply_publish_failed err={:?}",
            log_header,
            e
        );
    }
}

/// `connect` message from the server: a card discovered in a rack slot,
/// `{"iccid":"...","slot":N}`. The ICCID is resolved to the company card number through the
/// local config (only TBA knows that mapping) and a rack-backed per-card MQTT session is
/// spawned for it. An unknown ICCID is shown in the rack section as an unknown card with its
/// ICCID — no session until the user assigns the card number.
async fn handle_connect_spawn(payload: &[u8], serial_port: &SharedPort, log_header: &str) {
    let json = match serde_json::from_slice::<serde_json::Value>(payload) {
        Ok(json) => json,
        Err(e) => {
            log::warn!(
                "{} [SPAWN] status=ignored reason=bad_json err={}",
                log_header,
                e
            );
            return;
        }
    };
    let iccid = json.get("iccid").and_then(|v| v.as_str()).unwrap_or("");
    let slot = json.get("slot").and_then(|v| v.as_u64()).unwrap_or(0);
    if iccid.is_empty() || !(1..=240).contains(&slot) {
        log::warn!(
            "{} [SPAWN] status=ignored reason=invalid_iccid_or_slot slot={}",
            log_header,
            slot
        );
        return;
    }

    // one INFO line per discovered card: the rack inventory is readable straight from the trace
    let card_number = crate::config::find_card_number_by_iccid(iccid);
    match &card_number {
        Some(number) => log::info!(
            "{} [SPAWN] status=discovered slot={} iccid={} card={}",
            log_header,
            slot,
            iccid,
            number
        ),
        None => log::warn!(
            "{} [SPAWN] status=not_spawned reason=unknown_card slot={} iccid={}",
            log_header,
            slot,
            iccid
        ),
    }

    // every reported card lands in the rack state (rack section of the UI), configured or
    // not: an unknown card is shown there with its ICCID, ready to be assigned a number
    update_rack_card_ui(slot as u16, iccid, card_number.clone());

    let Some(card_number) = card_number else {
        return;
    };

    spawn_rack_card_checked(
        card_number,
        iccid.to_string(),
        slot as u16,
        serial_port.clone(),
        log_header,
    )
    .await;
}

/// Final spawn step shared by the server `connect` handler and the pending-card
/// retry: a reader-backed session for the same card number wins — never open a
/// second connection with the same client_id (the server treats that as an
/// ident collision).
async fn spawn_rack_card_checked(
    card_number: String,
    iccid: String,
    slot: u16,
    port: SharedPort,
    log_header: &str,
) {
    if TASK_POOL
        .lock()
        .await
        .iter()
        .any(|card| card.client_id == card_number)
    {
        log::warn!(
            "{} [SPAWN] card={} slot={} status=skipped reason=served_by_reader",
            log_header,
            card_number,
            slot
        );
        return;
    }

    spawn_rack_card(card_number, iccid, slot, port);
}

/// Puts one discovered rack card into the UI card list (keyed by slot) and re-emits the
/// rack state. Cards without a local config entry are shown too — with no card number.
fn update_rack_card_ui(slot: u16, iccid: &str, card_number: Option<String>) {
    let name = card_number
        .as_deref()
        .and_then(|number| crate::config::get_card_config_from_cache(number).and_then(|c| c.name));
    let cards = {
        let mut ui = match RACK_CARDS_UI.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        ui.retain(|c| c.slot != slot);
        ui.push(RackCard {
            slot,
            iccid: Some(iccid.to_string()),
            card_number,
            name,
        });
        ui.sort_by_key(|c| c.slot);
        ui.clone()
    };
    rack_update_cards(cards);
}

lazy_static::lazy_static! {
    /// Rack-backed per-card MQTT tasks, keyed by ICCID (the rack's stable card
    /// identifier) with the spawn-time card number kept alongside. Keying by the
    /// config-resolved card number would leak the session if the config entry is
    /// deleted or edited while the card sits in the rack — the disconnect lookup
    /// would then miss the running task. All aborted when the rack disconnects —
    /// without the rack there is no transport to those cards.
    static ref RACK_CARD_TASKS: std::sync::Mutex<std::collections::HashMap<String, (String, JoinHandle<()>)>> =
        std::sync::Mutex::new(std::collections::HashMap::new());

    /// Cards currently exposed by the rack, as shown in the UI (RackState.cards).
    static ref RACK_CARDS_UI: std::sync::Mutex<Vec<RackCard>> = std::sync::Mutex::new(Vec::new());
}

/// Opens rack-backed sessions for discovered cards that became resolvable after
/// a config change: the server's `connect` for a card with an unknown ICCID is
/// skipped at discovery time, and the server does not repeat it until the rack
/// content changes — so assigning the number in the UI must retry it locally.
pub async fn connect_pending_rack_cards() {
    let port = {
        let guard = match RACK_MQTT_TASK.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.as_ref().map(|(_, port)| port.clone())
    };
    let Some(port) = port else {
        return; // no rack connected
    };

    let pending: Vec<(u16, String)> = {
        let ui = match RACK_CARDS_UI.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        ui.iter()
            .filter(|card| card.card_number.is_none())
            .filter_map(|card| card.iccid.clone().map(|iccid| (card.slot, iccid)))
            .collect()
    };

    for (slot, iccid) in pending {
        let Some(card_number) = crate::config::find_card_number_by_iccid(&iccid) else {
            continue; // still unassigned
        };
        log::info!(
            "RACK | [SPAWN] status=pending_card_resolved slot={} iccid={} card={}",
            slot,
            iccid,
            card_number
        );
        update_rack_card_ui(slot, &iccid, Some(card_number.clone()));
        spawn_rack_card_checked(card_number, iccid, slot, port.clone(), "RACK |").await;
    }
}

/// Starts the rack-backed MQTT session of one card, deduplicating by ICCID and
/// by card number (two slots mapped to the same number in the config must not
/// open two MQTT connections with the same client_id).
fn spawn_rack_card(card_number: String, iccid: String, slot: u16, serial_port: SharedPort) {
    let mut tasks = match RACK_CARD_TASKS.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some((_, handle)) = tasks.get(&iccid) {
        if !handle.inner().is_finished() {
            log::debug!(
                "RACK | [SPAWN] card={} status=skipped reason=already_running",
                card_number
            );
            return;
        }
    }
    if tasks.iter().any(|(other_iccid, (number, handle))| {
        *other_iccid != iccid && *number == card_number && !handle.inner().is_finished()
    }) {
        log::warn!(
            "RACK | [SPAWN] card={} slot={} status=skipped reason=served_by_another_slot",
            card_number,
            slot
        );
        return;
    }
    log::info!(
        "RACK | [SPAWN] card={} slot={} status=starting_session",
        card_number,
        slot
    );
    let handle = async_runtime::spawn(rack_card_mqtt_loop(
        card_number.clone(),
        iccid.clone(),
        slot,
        serial_port,
    ));
    tasks.insert(iccid, (card_number, handle));
}

/// MQTT loop of one rack-backed card connection. Mirrors the rack's own loop — the same opaque
/// envelope handling funneled into the shared serial port — plus the one-shot **rack link
/// report** right after CONNACK (topic `rack`, `{"iccid":"...","slot":N}`) that binds this card
/// session to its slot on the server. Without the report the server treats the card as
/// reader-backed and uses the plain PC/SC envelope.
async fn rack_card_mqtt_loop(
    card_number: String,
    iccid: String,
    slot: u16,
    serial_port: SharedPort,
) {
    let log_header = format!("RACKCARD {} |", card_number);

    // same waiting policy as the rack loop: the server may not be configured yet
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

    let mut mqtt_options = MqttOptions::new(&card_number, &host, port);
    mqtt_options.set_keep_alive(Duration::from_secs(120));
    log::info!(
        "{} [MQTT] phase=connect_attempt status=initialized host={}:{} slot={}",
        log_header,
        host,
        port,
        slot
    );

    let (mqtt_client, mut eventloop) = AsyncClient::new(mqtt_options, 10);

    let mut is_online = false;
    let mut reconnect_delay_secs = RECONNECT_DELAY_INITIAL_SECS;
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
                        // new MQTT session: the server-side request_id counter restarts at 1
                        last_request_id = None;
                        last_response_payload = None;
                        // rack link report: must be the first publish of the session
                        let report =
                            serde_json::json!({ "iccid": iccid, "slot": slot }).to_string();
                        if let Err(e) = mqtt_client
                            .publish("rack", QoS::AtLeastOnce, false, report)
                            .await
                        {
                            log::error!(
                                "{} [MQTT] status=link_report_failed err={:?}",
                                log_header,
                                e
                            );
                        } else {
                            log::info!(
                                "{} [MQTT] status=link_report_sent slot={} iccid={}",
                                log_header,
                                slot,
                                iccid
                            );
                        }
                    }
                    Event::Incoming(Incoming::Publish(publish)) => {
                        let topic = String::from_utf8_lossy(&publish.topic).into_owned();
                        log::info!(
                            "{} [MQTT] event=command topic={} bytes={} qos={:?}",
                            log_header,
                            topic,
                            publish.payload.len(),
                            publish.qos,
                        );
                        log::debug!(
                            "{} [MQTT] command_text={}",
                            log_header,
                            String::from_utf8_lossy(&publish.payload)
                        );
                        handle_serial_request(
                            &mqtt_client,
                            &topic,
                            &publish.payload,
                            &serial_port,
                            &log_header,
                            &mut last_request_id,
                            &mut last_response_payload,
                        )
                        .await;
                    }
                    other => {
                        log::debug!("{} [MQTT] event=other detail={:?}", log_header, other);
                    }
                }
            }
            Err(e) => {
                let transition = if is_online {
                    "ONLINE->OFFLINE"
                } else {
                    "OFFLINE"
                };
                is_online = false;
                crate::mqtt::log_connection_failure(
                    &log_header,
                    "MQTT",
                    transition,
                    &e,
                    reconnect_delay_secs,
                );
                tokio::time::sleep(Duration::from_secs(reconnect_delay_secs)).await;
                reconnect_delay_secs = next_reconnect_delay(reconnect_delay_secs);
            }
        }
    }
}

lazy_static::lazy_static! {
    /// The card presence watch task of the current rack connection, if armed.
    static ref RACK_WATCH_TASK: std::sync::Mutex<Option<JoinHandle<()>>> =
        std::sync::Mutex::new(None);
}

/// Arms (or re-arms) the card presence watch from a server `watch` instruction:
/// `{"cmd":"<hex>","interval_ms":1000,"idle_ms":...,"deadline_ms":...}`. A background task
/// executes the opaque command every interval through the same FIFO port queue and publishes
/// the reply back (topic `watch`, the standard response envelope) ONLY when its bytes change.
/// Re-arming resets the baseline, so the first successful exchange is always published — that
/// is how the server catches updates missed while its discovery chain was busy.
fn start_rack_watch(
    payload: &[u8],
    serial_port: &SharedPort,
    mqtt_client: &AsyncClient,
    log_header: &str,
) {
    let json = match serde_json::from_slice::<serde_json::Value>(payload) {
        Ok(json) => json,
        Err(e) => {
            log::warn!(
                "{} [WATCH] status=ignored reason=bad_json err={}",
                log_header,
                e
            );
            return;
        }
    };
    let Some(cmd) = json.get("cmd").and_then(|v| v.as_str()) else {
        log::warn!("{} [WATCH] status=ignored reason=no_cmd_field", log_header);
        return;
    };
    let cmd_hex = match normalize_hex(cmd) {
        Ok(hex) => hex,
        Err(_) => {
            log::warn!("{} [WATCH] status=ignored reason=bad_cmd_hex", log_header);
            return;
        }
    };
    let ms = |key: &str| {
        json.get(key)
            .and_then(|v| v.as_u64())
            .map(|v| v.min(SERIAL_MS_MAX))
    };
    let interval = Duration::from_millis(ms("interval_ms").unwrap_or(1000));
    let idle = ms("idle_ms")
        .map(Duration::from_millis)
        .unwrap_or(SERIAL_REPLY_TIMEOUT);
    let deadline = ms("deadline_ms")
        .map(Duration::from_millis)
        .unwrap_or(SERIAL_READ_DEADLINE);

    let serial_port = serial_port.clone();
    let mqtt_client = mqtt_client.clone();
    let log_header = log_header.to_string();
    log::info!(
        "{} [WATCH] status=armed interval={:?} cmd_bytes={}",
        log_header,
        interval,
        cmd_hex.len() / 2
    );

    let handle = async_runtime::spawn(async move {
        let mut last: Option<String> = None;
        loop {
            tokio::time::sleep(interval).await;
            let envelope = SerialEnvelope {
                cmd_hex: cmd_hex.clone(),
                expect_hex: None,
                idle,
                deadline,
                poll: None,
            };
            let exchange = execute_envelope(&serial_port, envelope, &log_header, false).await;
            if !exchange.is_ok() {
                // transport errors are already logged by execute_envelope; the rack presence
                // monitor handles a truly gone device, so just keep trying
                continue;
            }
            if last.as_deref() == Some(exchange.resp_hex.as_str()) {
                continue;
            }
            last = Some(exchange.resp_hex.clone());
            log::info!(
                "{} [WATCH] status=change_detected rx_bytes={}",
                log_header,
                exchange.resp_hex.len() / 2
            );
            if let Err(e) = mqtt_client
                .publish("watch", QoS::AtLeastOnce, false, exchange.to_payload())
                .await
            {
                log::error!("{} [WATCH] status=publish_failed err={:?}", log_header, e);
                // let the next change (or re-arm) retry; keep the baseline as published intent
            }
        }
    });

    let mut guard = match RACK_WATCH_TASK.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(old) = guard.replace(handle) {
        old.abort();
    }
}

/// Stops the card presence watch task, if armed.
fn stop_rack_watch() {
    let mut guard = match RACK_WATCH_TASK.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(handle) = guard.take() {
        handle.abort();
        log::info!("RACK | [WATCH] status=stopped");
    }
}

/// `disconnect` message from the server: a card left its rack slot,
/// `{"iccid":"...","slot":N}`. Closes the rack-backed card session (if one was spawned) and
/// removes the card from the rack section of the UI.
fn handle_card_disconnect(payload: &[u8], log_header: &str) {
    let json = match serde_json::from_slice::<serde_json::Value>(payload) {
        Ok(json) => json,
        Err(e) => {
            log::warn!(
                "{} [SPAWN] status=ignored reason=bad_json err={}",
                log_header,
                e
            );
            return;
        }
    };
    let iccid = json.get("iccid").and_then(|v| v.as_str()).unwrap_or("");
    let slot = json.get("slot").and_then(|v| v.as_u64()).unwrap_or(0);
    log::info!(
        "{} [SPAWN] status=card_removed slot={} iccid={}",
        log_header,
        slot,
        iccid
    );

    // drop the card from the rack section of the UI
    let cards = {
        let mut ui = match RACK_CARDS_UI.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        ui.retain(|c| c.slot != slot as u16);
        ui.clone()
    };
    rack_update_cards(cards);

    // Close the card session, if one was spawned. Lookup is by ICCID captured at
    // spawn time — re-resolving the card number through the config here would
    // leak the task when the entry was deleted or edited mid-session.
    let mut tasks = match RACK_CARD_TASKS.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some((card_number, handle)) = tasks.remove(iccid) {
        handle.abort();
        log::info!(
            "{} [SPAWN] card={} status=aborted reason=card_removed",
            log_header,
            card_number
        );
    }
}

/// Aborts all rack-backed card sessions and clears the UI card list. Called when the rack
/// disconnects or its MQTT/serial stack is restarted — without the rack there is no transport.
fn stop_rack_cards() {
    let mut tasks = match RACK_CARD_TASKS.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    for (_iccid, (card_number, handle)) in tasks.drain() {
        handle.abort();
        log::info!(
            "RACK | [SPAWN] card={} status=aborted reason=rack_gone",
            card_number
        );
    }
    let mut ui = match RACK_CARDS_UI.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if !ui.is_empty() {
        ui.clear();
        rack_update_cards(Vec::new());
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

/// Bytes sitting in the driver's input buffer right now, taken without waiting.
/// `bytes_to_read` is the only non-blocking look at that buffer; reading exactly that
/// many bytes cannot block whatever the port timeout currently is. Any error is treated
/// as "nothing pending" — the caller's read loop has its own bounds either way.
fn drain_buffered(port: &mut Box<dyn SerialPort>, log_header: &str) -> Vec<u8> {
    let pending = match port.bytes_to_read() {
        Ok(0) => return Vec::new(),
        Ok(n) => (n as usize).min(SERIAL_REPLY_MAX_BYTES),
        Err(e) => {
            log::warn!("{} [SERIAL] bytes_to_read failed: {}", log_header, e);
            return Vec::new();
        }
    };
    let mut buf = vec![0u8; pending];
    match port.read(&mut buf) {
        Ok(n) => {
            buf.truncate(n);
            buf
        }
        Err(e) => {
            log::warn!("{} [SERIAL] drain read failed: {}", log_header, e);
            Vec::new()
        }
    }
}

/// Reads one reply off the port. Two timings are involved and they are NOT the same order
/// of magnitude:
///
///   * time to the FIRST byte (`first_wait`) — after a write, the rack has to receive the
///     whole command before it starts answering, so this scales with the command size (a
///     210-byte frame is ~18 ms of wire time at 115200 on its own) and carries the
///     USB-serial adapter's latency on top.
///   * gap BETWEEN bytes of a reply already in flight — that is `idle`, the line silence
///     that marks the end of the reply. Tens of milliseconds.
///
/// Using `idle` for both is what made every large command come back `no_reply` while short
/// ones went through: the reply was on its way, we just stopped listening. So the port
/// timeout starts at `first_wait` and drops to `idle` as soon as the first bytes land — a
/// non-empty `carry` (bytes the rack already pushed) means the reply has started, so the
/// silence bound applies from the very first read.
///
/// Two hard bounds protect against a misbehaving device that streams bytes continuously
/// (each read would then succeed before the timeout and the loop would never exit): a cap
/// on the reply size and `deadline` on the whole read phase. Returns the bytes and whether
/// the size cap truncated them.
fn read_reply(
    port: &mut Box<dyn SerialPort>,
    carry: Vec<u8>,
    first_wait: Duration,
    idle: Duration,
    deadline: Duration,
    log_header: &str,
) -> (Vec<u8>, bool) {
    let mut reply = carry;
    let mut first_byte_pending = reply.is_empty();
    let initial_timeout = if first_byte_pending { first_wait } else { idle };
    if let Err(e) = port.set_timeout(initial_timeout) {
        log::warn!(
            "{} [SERIAL] set_timeout({:?}) failed: {}",
            log_header,
            initial_timeout,
            e
        );
    }

    let mut buf = [0u8; 512];
    let mut truncated = false;
    let read_started = std::time::Instant::now();
    // the total bound must never undercut the first-byte budget it contains
    let total = if deadline > first_wait {
        deadline
    } else {
        first_wait
    };
    let read_deadline = read_started + total;
    loop {
        if reply.len() >= SERIAL_REPLY_MAX_BYTES {
            log::warn!(
                "{} [SERIAL] reply exceeded {} bytes — truncating, device is misbehaving",
                log_header,
                SERIAL_REPLY_MAX_BYTES
            );
            truncated = true;
            break;
        }
        if std::time::Instant::now() >= read_deadline {
            log::warn!(
                "{} [SERIAL] read deadline {:?} reached — returning {} bytes read so far",
                log_header,
                total,
                reply.len()
            );
            break;
        }
        match port.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if first_byte_pending {
                    // reply started: from here on the read is bounded by line silence.
                    // The elapsed time is logged because a first byte arriving later
                    // than `idle` is exactly the condition that used to be reported as
                    // `no_reply` — worth seeing in a log when tuning the server timings.
                    first_byte_pending = false;
                    let ttfb = read_started.elapsed();
                    if ttfb > idle {
                        log::debug!(
                            "{} [SERIAL] first byte after {:?} (over idle {:?})",
                            log_header,
                            ttfb,
                            idle
                        );
                    }
                    if let Err(e) = port.set_timeout(idle) {
                        log::warn!(
                            "{} [SERIAL] set_timeout({:?}) failed: {}",
                            log_header,
                            idle,
                            e
                        );
                    }
                }
                reply.extend_from_slice(&buf[..n]);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(e) => {
                log::warn!("{} [SERIAL] read error: {}", log_header, e);
                break;
            }
        }
    }
    (reply, truncated)
}

/// Listens for up to `interval` for a frame the rack pushes without being asked, and reads
/// it whole once it starts. This replaces the blind sleep that used to sit between status
/// polls: the rack does not wait to be polled for a card result, it sends the frame as soon
/// as the card is done (verified on live hardware — "accepted"+result in a single read at
/// `polls=0`), and it hands that result out exactly once, going back to "idle" afterwards.
/// A result landing in an unwatched gap was therefore lost for good. Empty return = the
/// interval elapsed in silence, time to send the next status poll.
fn wait_for_push(
    port: &mut Box<dyn SerialPort>,
    interval: Duration,
    idle: Duration,
    deadline: Duration,
    log_header: &str,
) -> Vec<u8> {
    let (bytes, _truncated) = read_reply(port, Vec::new(), interval, idle, deadline, log_header);
    if !bytes.is_empty() {
        log::debug!(
            "{} [SERIAL] rx pushed bytes={} hex={}",
            log_header,
            bytes.len(),
            hex::encode_upper(&bytes)
        );
    }
    bytes
}

/// One write+read exchange on an already-locked port. `deadline` bounds the whole read phase,
/// including the wait for the rack's first reply byte; `idle` is the line silence that ends a
/// reply once it has started. `purge_stale` tells whether pending bytes belong to a previous
/// operation (dropped) or to this one (carried into the reply) — see the body. Payload hex only
/// at debug — the rack protocol must not end up in users' log files at INFO level.
fn exchange_once(
    port: &mut Box<dyn SerialPort>,
    cmd_hex: &str,
    idle: Duration,
    deadline: Duration,
    purge_stale: bool,
    log_header: &str,
) -> SerialExchange {
    // envelope hex is pre-normalized; this guard covers direct callers only
    let bytes = match hex::decode(cmd_hex) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("{} [SERIAL] bad hex in serial_cmd: {}", log_header, e);
            return SerialExchange::error(SERIAL_ERR_BAD_HEX);
        }
    };

    // Bytes already buffered when this exchange starts. Their meaning depends on where in
    // the operation we are, and getting that wrong costs the operation its result:
    //   * first exchange of an operation (`purge_stale`) — the port lock was just taken, so
    //     anything pending is left over from an earlier, finished operation. Dropped, but
    //     logged: silent purges are how a lost result stays invisible.
    //   * any later exchange (a status poll of the same operation) — the rack pushes the
    //     card result on its own as soon as the card is done, without waiting to be polled.
    //     Those bytes ARE this operation's result; they are carried into the reply and the
    //     server's multi-frame parser picks the outcome frame out of the concatenation.
    // Blind-clearing here (every write, unconditionally) is what ate the result of every
    // card operation slow enough to finish between two read windows — EXTERNAL AUTHENTICATE
    // above all — after which the slot reports plain "card in reader, idle".
    let carry = drain_buffered(port, log_header);
    let carry = if purge_stale {
        if !carry.is_empty() {
            log::debug!(
                "{} [SERIAL] dropped {} stale bytes before tx hex={}",
                log_header,
                carry.len(),
                hex::encode_upper(&carry)
            );
        }
        Vec::new()
    } else {
        if !carry.is_empty() {
            log::debug!(
                "{} [SERIAL] carrying {} pushed bytes into this exchange hex={}",
                log_header,
                carry.len(),
                hex::encode_upper(&carry)
            );
        }
        carry
    };

    log::debug!(
        "{} [SERIAL] tx bytes={} hex={}",
        log_header,
        bytes.len(),
        cmd_hex
    );

    if let Err(e) = port.write_all(&bytes) {
        log::error!("{} [SERIAL] write failed: {}", log_header, e);
        return SerialExchange::error(SERIAL_ERR_WRITE_FAILED);
    }
    let _ = port.flush();

    let (reply, truncated) = read_reply(port, carry, deadline, idle, deadline, log_header);
    let exchange = if truncated {
        // Partial data + error code: the server sees what came through AND
        // knows the exchange is unusable.
        SerialExchange {
            resp_hex: hex::encode_upper(&reply),
            err: SERIAL_ERR_TRUNCATED,
        }
    } else if reply.is_empty() {
        SerialExchange::error(SERIAL_ERR_NO_REPLY)
    } else {
        SerialExchange::ok(hex::encode_upper(&reply))
    };
    log::debug!(
        "{} [SERIAL] rx bytes={} err={} hex={}",
        log_header,
        exchange.resp_hex.len() / 2,
        exchange.err,
        exchange.resp_hex
    );
    exchange
}

/// The blocking core of one logical operation on an already-locked port: the command exchange
/// plus the optional server-scripted poll loop. Returns the outcome and how it was obtained
/// (`polls` status requests sent, `pushes` frames the rack sent on its own).
///
/// The wire model this implements, as verified on live hardware: the rack answers the command
/// with "accepted", then sends the card's result **by itself** as soon as the card is done, and
/// hands that result out exactly once — afterwards the slot reports plain "idle". Status polls
/// are the fallback for a result that was not caught, not the primary channel. Hence the two
/// rules below: never stop listening between reads, and never drop bytes that arrived while we
/// were not reading.
fn run_envelope(
    port: &mut Box<dyn SerialPort>,
    env: &SerialEnvelope,
    log_header: &str,
) -> (SerialExchange, u32, u32) {
    let mut polls: u32 = 0;
    let mut pushes: u32 = 0;

    // first exchange of the operation: the port lock was just taken, so anything still
    // buffered belongs to an operation that is already over — purge it
    let first = exchange_once(port, &env.cmd_hex, env.idle, env.deadline, true, log_header);
    let outcome = 'op: {
        if !first.is_ok() {
            break 'op first;
        }
        let Some(poll) = &env.poll else {
            break 'op first; // one-shot exchange: the first reply is the result
        };
        // the poll loop is entered only when the device answered exactly the predicted
        // "accepted" bytes; anything else (a NAK, an instant result) goes back as is
        match &env.expect_hex {
            Some(expect) if first.resp_hex == *expect => {}
            _ => break 'op first,
        }
        let poll_deadline = std::time::Instant::now() + poll.deadline;
        loop {
            // listen through the poll interval instead of sleeping through it: the rack
            // pushes the card result on its own and only once, so an unwatched gap loses
            // it. A pushed frame that is exactly the predicted "busy" bytes is just a
            // late poll reply — same rule as below, keep waiting for the real outcome.
            let pushed = wait_for_push(port, poll.interval, env.idle, env.deadline, log_header);
            if !pushed.is_empty() {
                pushes += 1;
                let pushed_hex = hex::encode_upper(&pushed);
                if pushed_hex != poll.while_hex {
                    break 'op SerialExchange::ok(pushed_hex);
                }
            }
            polls += 1;
            // not the first exchange: pending bytes are this operation's pushed result,
            // they get carried into the poll reply rather than dropped
            let reply = exchange_once(
                port,
                &poll.cmd_hex,
                env.idle,
                env.deadline,
                false,
                log_header,
            );
            if !reply.is_ok() || reply.resp_hex != poll.while_hex {
                // the first differing reply is the operation result (or a transport error)
                break 'op reply;
            }
            if std::time::Instant::now() >= poll_deadline {
                // still "busy" at the deadline: hand the last reply to the server — it can
                // decode the device state and report a readable failure
                log::warn!(
                    "{} [SERIAL] poll deadline {:?} reached after {} polls — returning the last reply",
                    log_header,
                    poll.deadline,
                    polls
                );
                break 'op reply;
            }
        }
    };
    (outcome, polls, pushes)
}

/// Executes a whole envelope on the shared port: the command exchange plus the optional
/// server-scripted poll loop. The port lock is held for the entire logical operation — the rack
/// is Master/Slave (one request on the wire at a time), so concurrent card sessions of one rack
/// interleave at operation granularity; tokio's Mutex queues the waiters FIFO-fair, which is the
/// per-port queue. Blocking serial I/O runs on a blocking thread so the async runtime isn't stalled.
/// `log_summary=false` silences the per-operation INFO line — the 1 Hz watch loop would flood
/// the log otherwise; its caller logs only actual changes.
async fn execute_envelope(
    port: &SharedPort,
    env: SerialEnvelope,
    log_header: &str,
    log_summary: bool,
) -> SerialExchange {
    let port = port.clone();
    let log_header_blocking = log_header.to_string();

    let result = tokio::task::spawn_blocking(move || {
        let mut guard = port.blocking_lock();
        run_envelope(&mut guard, &env, &log_header_blocking)
    })
    .await
    // A join error means the blocking closure panicked before producing a
    // result — no reply was obtained, report it as such.
    .unwrap_or_else(|e| {
        log::error!("{} [SERIAL] exchange task failed: {}", log_header, e);
        (SerialExchange::error(SERIAL_ERR_NO_REPLY), 0, 0)
    });

    let (exchange, polls, pushes) = result;
    // one INFO summary per logical operation; per-exchange details are at debug
    if exchange.is_ok() {
        if log_summary {
            log::info!(
                "{} [SERIAL] op done polls={} pushes={} rx bytes={}",
                log_header,
                polls,
                pushes,
                exchange.resp_hex.len() / 2
            );
        }
    } else {
        log::warn!(
            "{} [SERIAL] op failed err={} polls={} pushes={} partial_bytes={}",
            log_header,
            exchange.err,
            polls,
            pushes,
            exchange.resp_hex.len() / 2
        );
    }
    exchange
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
                    "RACK | phase=presence status=mqtt_task_dead port={} action=restart",
                    rack.port_name
                );
                // The dead loop's siblings (watch task, card sessions) may still
                // hold the old exclusive serial handle — kill them BEFORE reopening,
                // otherwise open_rack fails on every tick forever. Running this on
                // each retry tick also reaps a watch task that raced past a
                // previous stop_rack_mqtt.
                stop_rack_mqtt();
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
    fn port_serial_linking_allows_optional_channel_letter() {
        // Exact match (single-port chip without suffix) and driver-suffixed.
        assert!(port_serial_belongs_to_device("SC1799", "SC1799"));
        assert!(port_serial_belongs_to_device("SC1799", "SC1799A"));
        assert!(port_serial_belongs_to_device("SC1799", "SC1799B"));
        // Different device, longer suffixes, or empty serials do not link.
        assert!(!port_serial_belongs_to_device("SC1799", "SC1798A"));
        assert!(!port_serial_belongs_to_device("SC1799", "SC1799AB"));
        assert!(!port_serial_belongs_to_device("SC1799", "SC17991"));
        assert!(!port_serial_belongs_to_device("", "A"));
    }

    #[test]
    fn lisle_serial_accepts_scheme_and_strips_channel_letter() {
        // Windows FTDI driver appends the channel letter to the EEPROM serial.
        assert_eq!(lisle_serial("SC1799A"), Some("SC1799".to_string()));
        // Descriptor form (macOS/Linux) passes through unchanged.
        assert_eq!(lisle_serial("SC1799"), Some("SC1799".to_string()));
        // FTDI factory-default serials and other schemes are rejected.
        assert_eq!(lisle_serial("A5XK3RJT"), None);
        assert_eq!(lisle_serial("SC"), None);
        assert_eq!(lisle_serial("SCA"), None);
        assert_eq!(lisle_serial("SC17X9"), None);
        assert_eq!(lisle_serial(""), None);
    }

    #[test]
    fn fallback_identity_builds_same_client_id_as_descriptor_match() {
        // The same physical rack: macOS reports the descriptors, Windows the
        // driver strings + suffixed serial. Both must yield one client_id.
        let windows_id = build_client_id(
            RACK_FALLBACK_MANUFACTURER,
            lisle_serial("SC1799A").as_deref(),
        );
        let macos_id = build_client_id("Lisle Design Ltd", Some("SC1799"));
        assert_eq!(windows_id, macos_id);
        assert_eq!(windows_id, "LISLE00000SC1799");
        assert!(matches_server_contract(&windows_id));
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
        assert_eq!(
            request_id_from_topic("request/42/RACK0000000000AB"),
            Some(42)
        );
        assert_eq!(request_id_from_topic("request/0"), Some(0));
        // Non-numeric id or wrong prefix → None.
        assert_eq!(request_id_from_topic("request/abc/X"), None);
        assert_eq!(request_id_from_topic("status/1/X"), None);
    }

    // The server contract: client_id must match ^[0-9A-Z]{16}$.
    fn matches_server_contract(id: &str) -> bool {
        id.len() == 16
            && id
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    }

    #[test]
    fn brand_prefix_from_manufacturer() {
        assert_eq!(brand_prefix("Lisle Design Ltd"), "LISLE");
        assert_eq!(brand_prefix("lisle design ltd"), "LISLE"); // case-insensitive
        assert_eq!(brand_prefix("Acme Co"), "ACME"); // shorter first word
        assert_eq!(brand_prefix(""), ""); // empty
    }

    #[test]
    fn client_id_from_serial() {
        // Brand (from manufacturer) + zero filler + serial; serial flush at end.
        assert_eq!(build_client_id(MFR, Some("SC1234")), "LISLE00000SC1234");
    }

    #[test]
    fn client_id_serial_is_flush_at_end() {
        // Whatever the serial length, it must end the id (filler in the middle).
        assert!(build_client_id(MFR, Some("SC1234")).ends_with("SC1234"));
        assert!(build_client_id(MFR, Some("AB")).ends_with("AB"));
    }

    #[test]
    fn client_id_always_matches_server_contract() {
        for serial in [
            Some("SC1234"),
            Some("sc1234"),                  // lowercase gets uppercased
            Some("SC-12/34"),                // punctuation stripped
            Some(""),                        // empty serial
            None,                            // no serial at all
            Some("VERYLONGSERIALNUMBER123"), // longer than 16 → truncated
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
        assert!(RackInfo {
            manufacturer: Some("LISLE DESIGN".into()),
            ..base.clone()
        }
        .is_supported());
        assert!(!RackInfo {
            manufacturer: Some("Acme Co".into()),
            ..base.clone()
        }
        .is_supported());
        assert!(!RackInfo {
            manufacturer: None,
            ..base.clone()
        }
        .is_supported());
    }

    // ── Response envelope (contract v2): both fields always present, one shape ──

    /// Parses a published payload and returns (serial_resp, serial_err).
    fn parse_payload(payload: &str) -> (String, String) {
        let v: serde_json::Value = serde_json::from_str(payload).expect("payload must be JSON");
        let obj = v.as_object().expect("payload must be an object");
        assert_eq!(
            obj.len(),
            2,
            "envelope must have exactly the two contract fields"
        );
        (
            obj["serial_resp"]
                .as_str()
                .expect("serial_resp must be a string")
                .to_string(),
            obj["serial_err"]
                .as_str()
                .expect("serial_err must be a string")
                .to_string(),
        )
    }

    #[test]
    fn exchange_payload_rack_replied() {
        // synthetic bytes — not a real device reply
        let p = SerialExchange::ok("A1B2C3D4".into()).to_payload();
        assert_eq!(parse_payload(&p), ("A1B2C3D4".to_string(), "".to_string()));
    }

    #[test]
    fn exchange_payload_no_reply() {
        let p = SerialExchange::error(SERIAL_ERR_NO_REPLY).to_payload();
        assert_eq!(parse_payload(&p), ("".to_string(), "no_reply".to_string()));
    }

    #[test]
    fn exchange_payload_write_failed() {
        let p = SerialExchange::error(SERIAL_ERR_WRITE_FAILED).to_payload();
        assert_eq!(
            parse_payload(&p),
            ("".to_string(), "write_failed".to_string())
        );
    }

    #[test]
    fn exchange_payload_bad_hex() {
        let p = SerialExchange::error(SERIAL_ERR_BAD_HEX).to_payload();
        assert_eq!(parse_payload(&p), ("".to_string(), "bad_hex".to_string()));
    }

    #[test]
    fn exchange_payload_truncated_keeps_partial_data() {
        // Truncation carries BOTH the partial hex and the error code.
        let p = SerialExchange {
            resp_hex: "A1B2C3".into(),
            err: SERIAL_ERR_TRUNCATED,
        }
        .to_payload();
        assert_eq!(
            parse_payload(&p),
            ("A1B2C3".to_string(), "truncated".to_string())
        );
    }

    // ── Envelope parsing (poll primitive contract) ──
    // Only synthetic placeholder bytes here: the device wire protocol is owned
    // by the server and must never appear in this repo, not even in tests.

    #[test]
    fn envelope_without_serial_cmd_is_not_an_envelope() {
        let json: serde_json::Value = serde_json::from_str(r#"{"connect":{}}"#).unwrap();
        assert!(parse_envelope(&json).is_none());
    }

    #[test]
    fn envelope_bad_hex_reports_contract_code() {
        let json: serde_json::Value = serde_json::from_str(r#"{"serial_cmd":"ZZ"}"#).unwrap();
        assert_eq!(
            parse_envelope(&json).unwrap().unwrap_err(),
            SERIAL_ERR_BAD_HEX
        );
        // bad hex inside the poll spec is just as malformed
        let json: serde_json::Value =
            serde_json::from_str(r#"{"serial_cmd":"AB","poll":{"cmd":"AB","while":"XX"}}"#)
                .unwrap();
        assert_eq!(
            parse_envelope(&json).unwrap().unwrap_err(),
            SERIAL_ERR_BAD_HEX
        );
    }

    #[test]
    fn envelope_poll_without_bytes_is_malformed() {
        let json: serde_json::Value =
            serde_json::from_str(r#"{"serial_cmd":"AB","poll":{"interval_ms":20}}"#).unwrap();
        assert_eq!(
            parse_envelope(&json).unwrap().unwrap_err(),
            SERIAL_ERR_BAD_HEX
        );
    }

    #[test]
    fn exchange_only_success_is_cacheable() {
        // Idempotency contract: cache only fully successful exchanges.
        assert!(SerialExchange::ok("AA".into()).is_ok());
        assert!(!SerialExchange::error(SERIAL_ERR_NO_REPLY).is_ok());
        assert!(!SerialExchange {
            resp_hex: "AA".into(),
            err: SERIAL_ERR_TRUNCATED
        }
        .is_ok());
    }

    // ── Poll primitive against a scripted port ──
    // Placeholder bytes again — only the SHAPE of the exchange is TBA's business (a command,
    // the predicted "accepted" echo, an outcome frame, the predicted "busy" poll reply, and a
    // status that is neither). The timing shape is taken from a real failing card session
    // captured on the channel: the device answered "accepted" within milliseconds and sent the
    // card result on its own roughly one idle-window later, i.e. exactly into the gap between
    // two reads. Absolute values are scaled up here so a loaded CI host cannot turn the
    // scheduling into a coin flip.

    const CMD: &str = "C0DE01";
    const ACCEPTED: &str = "AC01";
    const RESULT: &str = "1234ABCD";
    const POLL_CMD: &str = "5701";
    const BUSY: &str = "B501";
    const IDLE_STATUS: &str = "1D01";

    const T_IDLE: Duration = Duration::from_millis(100);
    const T_DEADLINE: Duration = Duration::from_millis(1000);
    const T_INTERVAL: Duration = Duration::from_millis(100);

    struct PortState {
        /// written hex -> replies to schedule, each at its own delay from that write
        rules: Vec<(String, Vec<(Duration, String)>)>,
        scheduled: Vec<(std::time::Instant, Vec<u8>)>,
        inbox: std::collections::VecDeque<u8>,
        timeout: Duration,
        /// Every frame written to the port, in order — shared so a test can still read it
        /// after the port has been boxed into a `dyn SerialPort`.
        writes: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl PortState {
        /// Moves every reply whose time has come into the readable buffer.
        fn pump(&mut self) {
            let now = std::time::Instant::now();
            let mut still = Vec::new();
            for (at, bytes) in std::mem::take(&mut self.scheduled) {
                if at <= now {
                    self.inbox.extend(bytes);
                } else {
                    still.push((at, bytes));
                }
            }
            self.scheduled = still;
        }

        fn next_due(&self) -> Option<std::time::Instant> {
            self.scheduled.iter().map(|(at, _)| *at).min()
        }
    }

    /// Stand-in for the rack's serial port. Replies are delivered on the real clock, so the
    /// production timing rules (first-byte budget, line-silence bound, poll interval) run
    /// exactly as they do on hardware. State lives behind a mutex because `bytes_to_read`
    /// takes `&self` yet has to advance the schedule.
    struct ScriptedPort {
        state: std::sync::Mutex<PortState>,
        writes: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl ScriptedPort {
        fn new(rules: &[(&str, &[(u64, &str)])]) -> Self {
            let writes = Arc::new(std::sync::Mutex::new(Vec::new()));
            Self {
                writes: writes.clone(),
                state: std::sync::Mutex::new(PortState {
                    rules: rules
                        .iter()
                        .map(|(cmd, replies)| {
                            (
                                cmd.to_string(),
                                replies
                                    .iter()
                                    .map(|(ms, hex)| (Duration::from_millis(*ms), hex.to_string()))
                                    .collect(),
                            )
                        })
                        .collect(),
                    scheduled: Vec::new(),
                    inbox: std::collections::VecDeque::new(),
                    timeout: Duration::from_millis(0),
                    writes,
                }),
            }
        }

        /// Bytes already sitting in the buffer when the exchange starts — a leftover of an
        /// operation that is already over, or a result pushed while nobody was reading.
        fn with_pending(self, hex: &str) -> Self {
            self.state
                .lock()
                .unwrap()
                .inbox
                .extend(hex::decode(hex).unwrap());
            self
        }
    }

    impl std::io::Read for ScriptedPort {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let deadline = std::time::Instant::now() + self.state.lock().unwrap().timeout;
            loop {
                let wake = {
                    let mut st = self.state.lock().unwrap();
                    st.pump();
                    if !st.inbox.is_empty() {
                        let n = buf.len().min(st.inbox.len());
                        for slot in buf.iter_mut().take(n) {
                            *slot = st.inbox.pop_front().unwrap();
                        }
                        return Ok(n);
                    }
                    st.next_due().map(|d| d.min(deadline)).unwrap_or(deadline)
                };
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "scripted",
                    ));
                }
                if wake > now {
                    std::thread::sleep(wake - now);
                }
            }
        }
    }

    impl std::io::Write for ScriptedPort {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut st = self.state.lock().unwrap();
            let written = hex::encode_upper(buf);
            let now = std::time::Instant::now();
            let replies: Vec<(Duration, String)> = st
                .rules
                .iter()
                .find(|(cmd, _)| *cmd == written)
                .map(|(_, r)| r.clone())
                .unwrap_or_default();
            for (delay, hex) in replies {
                st.scheduled.push((now + delay, hex::decode(&hex).unwrap()));
            }
            st.writes.lock().unwrap().push(written);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl SerialPort for ScriptedPort {
        fn name(&self) -> Option<String> {
            Some("scripted".into())
        }
        fn baud_rate(&self) -> serialport::Result<u32> {
            Ok(115_200)
        }
        fn data_bits(&self) -> serialport::Result<serialport::DataBits> {
            Ok(serialport::DataBits::Eight)
        }
        fn flow_control(&self) -> serialport::Result<serialport::FlowControl> {
            Ok(serialport::FlowControl::None)
        }
        fn parity(&self) -> serialport::Result<serialport::Parity> {
            Ok(serialport::Parity::None)
        }
        fn stop_bits(&self) -> serialport::Result<serialport::StopBits> {
            Ok(serialport::StopBits::One)
        }
        fn timeout(&self) -> Duration {
            self.state.lock().unwrap().timeout
        }
        fn set_baud_rate(&mut self, _: u32) -> serialport::Result<()> {
            Ok(())
        }
        fn set_data_bits(&mut self, _: serialport::DataBits) -> serialport::Result<()> {
            Ok(())
        }
        fn set_flow_control(&mut self, _: serialport::FlowControl) -> serialport::Result<()> {
            Ok(())
        }
        fn set_parity(&mut self, _: serialport::Parity) -> serialport::Result<()> {
            Ok(())
        }
        fn set_stop_bits(&mut self, _: serialport::StopBits) -> serialport::Result<()> {
            Ok(())
        }
        fn set_timeout(&mut self, timeout: Duration) -> serialport::Result<()> {
            self.state.lock().unwrap().timeout = timeout;
            Ok(())
        }
        fn write_request_to_send(&mut self, _: bool) -> serialport::Result<()> {
            Ok(())
        }
        fn write_data_terminal_ready(&mut self, _: bool) -> serialport::Result<()> {
            Ok(())
        }
        fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn bytes_to_read(&self) -> serialport::Result<u32> {
            let mut st = self.state.lock().unwrap();
            st.pump();
            Ok(st.inbox.len() as u32)
        }
        fn bytes_to_write(&self) -> serialport::Result<u32> {
            Ok(0)
        }
        fn clear(&self, _: serialport::ClearBuffer) -> serialport::Result<()> {
            self.state.lock().unwrap().inbox.clear();
            Ok(())
        }
        fn try_clone(&self) -> serialport::Result<Box<dyn SerialPort>> {
            Err(serialport::Error::new(
                serialport::ErrorKind::Unknown,
                "not cloneable",
            ))
        }
        fn set_break(&self) -> serialport::Result<()> {
            Ok(())
        }
        fn clear_break(&self) -> serialport::Result<()> {
            Ok(())
        }
    }

    fn two_phase_envelope() -> SerialEnvelope {
        SerialEnvelope {
            cmd_hex: CMD.into(),
            expect_hex: Some(ACCEPTED.into()),
            idle: T_IDLE,
            deadline: T_DEADLINE,
            poll: Some(PollSpec {
                cmd_hex: POLL_CMD.into(),
                while_hex: BUSY.into(),
                interval: T_INTERVAL,
                deadline: Duration::from_millis(2000),
            }),
        }
    }

    #[test]
    fn result_pushed_after_the_accepted_window_is_not_lost() {
        // THE REGRESSION. Timing of a real failed authentication: "accepted" comes back at
        // once, the card takes just over one idle window, and the device sends the result on
        // its own — into the gap between two reads. That gap used to be a blind sleep followed
        // by a buffer purge, so the result was destroyed and the next status request found the
        // slot back at rest, which surfaced to the server as a plain "idle" status and failed
        // the whole authentication one command before the end.
        let port = ScriptedPort::new(&[
            (CMD, &[(10, ACCEPTED), (150, RESULT)]),
            // the device hands a result out exactly once and rests afterwards
            (POLL_CMD, &[(10, IDLE_STATUS)]),
        ]);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let (outcome, _polls, _pushes) = run_envelope(&mut port, &two_phase_envelope(), "TEST |");

        // The invariant is what the bug broke: the result reaches the server. Whether it was
        // caught by the listening window or carried into a poll reply is a scheduling detail
        // and deliberately not asserted — both are correct, neither loses the bytes.
        assert!(
            outcome.resp_hex.contains(RESULT),
            "result went missing, got {:?}",
            outcome.resp_hex
        );
        assert_ne!(
            outcome.resp_hex, IDLE_STATUS,
            "the lost-result symptom is back"
        );
        assert!(outcome.is_ok());
    }

    #[test]
    fn fast_result_glued_with_accepted_needs_no_poll() {
        // The healthy case that always worked: the card is quick enough that the result lands
        // inside the read window of the "accepted" frame. The buffer then differs from the
        // predicted bytes, so it goes straight back to the server (which owns the framing).
        let port = ScriptedPort::new(&[(CMD, &[(10, ACCEPTED), (20, RESULT)])]);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let (outcome, polls, pushes) = run_envelope(&mut port, &two_phase_envelope(), "TEST |");

        assert_eq!(outcome.resp_hex, format!("{}{}", ACCEPTED, RESULT));
        assert_eq!((polls, pushes), (0, 0));
    }

    #[test]
    fn pushed_busy_frame_is_not_mistaken_for_the_outcome() {
        // Not every unprompted frame is a result: a poll reply that outlived its read window
        // arrives the same way. It matches the "keep polling" bytes the server predicted, so
        // it must be consumed and ignored, not returned as the operation's outcome.
        let port = ScriptedPort::new(&[
            (CMD, &[(10, ACCEPTED), (150, BUSY)]),
            (POLL_CMD, &[(10, RESULT)]),
        ]);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let (outcome, polls, pushes) = run_envelope(&mut port, &two_phase_envelope(), "TEST |");

        assert_eq!(outcome.resp_hex, RESULT);
        assert_eq!((polls, pushes), (1, 1));
    }

    #[test]
    fn transport_failure_of_the_first_exchange_skips_the_poll_loop() {
        // A silent device must not be polled: the server needs the transport error, not a
        // status decoded from nothing.
        let port = ScriptedPort::new(&[]);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let mut env = two_phase_envelope();
        env.deadline = Duration::from_millis(50); // no point waiting a full budget in a test
        let (outcome, polls, pushes) = run_envelope(&mut port, &env, "TEST |");

        assert_eq!(outcome.err, SERIAL_ERR_NO_REPLY);
        assert_eq!((polls, pushes), (0, 0));
    }

    #[test]
    fn poll_exchange_carries_bytes_that_were_already_waiting() {
        // Inside an operation, whatever is buffered belongs to that operation — the device
        // pushed it while we were between reads. It is prepended to the reply; the server's
        // parser is the one that walks a buffer of several frames.
        let port = ScriptedPort::new(&[(POLL_CMD, &[(10, BUSY)])]).with_pending(RESULT);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let ex = exchange_once(&mut port, POLL_CMD, T_IDLE, T_DEADLINE, false, "TEST |");

        assert_eq!(ex.resp_hex, format!("{}{}", RESULT, BUSY));
    }

    #[test]
    fn first_exchange_of_an_operation_drops_what_was_left_over() {
        // At the start of an operation the port lock was just taken, so anything buffered is
        // the tail of an operation that is already over. Keeping it would prepend a foreign
        // frame to this operation's reply.
        let port = ScriptedPort::new(&[(CMD, &[(10, ACCEPTED)])]).with_pending(RESULT);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let ex = exchange_once(&mut port, CMD, T_IDLE, T_DEADLINE, true, "TEST |");

        assert_eq!(ex.resp_hex, ACCEPTED);
    }

    #[test]
    fn one_shot_exchange_returns_the_first_reply() {
        // No poll spec (global status, LED, firmware): the first reply is the whole answer.
        let port = ScriptedPort::new(&[(CMD, &[(10, RESULT)])]);
        let writes = port.writes.clone();
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let env = SerialEnvelope {
            cmd_hex: CMD.into(),
            expect_hex: None,
            idle: T_IDLE,
            deadline: T_DEADLINE,
            poll: None,
        };
        let (outcome, polls, pushes) = run_envelope(&mut port, &env, "TEST |");

        assert_eq!(outcome.resp_hex, RESULT);
        assert_eq!((polls, pushes), (0, 0));
        assert_eq!(
            *writes.lock().unwrap(),
            vec![CMD.to_string()],
            "a one-shot exchange writes the command and nothing else"
        );
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
