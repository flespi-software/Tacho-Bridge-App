//! Device discovery: finding supported COM devices (currently the Lisle rack)
//! on the USB-serial bus and opening their ports.
//!
//! Matching is by the device's own descriptor strings where the OS exposes them
//! (macOS/Linux), by the parent USB node on Windows (where the COM node only
//! carries the driver's strings), and by chip identity + serial scheme as a last
//! resort. vid/pid are logged but never used as the primary match.
//!
//! What identifies a device type lives in a [`DeviceProfile`]; the scan itself
//! is profile-driven, so a future device type is a new profile entry plus its
//! own server-instruction handler — the scan and the transport need no changes.
//! Every matching device is returned: several racks on separate USB ports are
//! all served, each through its own connection (see `rack_connection`).

use std::time::Duration;

use serialport::SerialPortType;

use super::transport::SERIAL_REPLY_TIMEOUT;

use crate::global_app_handle::{emit_notification_event, NotificationPayload, RackState};

/// Declarative identity of one supported COM-device type: how to recognise it
/// on the bus and how its serial link is configured. All Lisle-rack matching
/// knowledge lives in [`LISLE_RACK`]; the wire protocol itself stays on the
/// server either way (the client is a dumb pipe — see `transport`).
#[derive(Debug)]
pub(super) struct DeviceProfile {
    /// USB product string the device advertises via its own descriptors.
    /// Combined with `brand_marker`, this identifies a supported device. We
    /// deliberately do NOT match on USB vid/pid — those belong to the
    /// underlying USB-serial chip (shared across many unrelated adapters and
    /// liable to change between hardware revisions). vid/pid are only read and
    /// logged, to collect data for now.
    product: &'static str,
    /// Substring (case-insensitive) the manufacturer string must contain for
    /// the device to be treated as supported.
    brand_marker: &'static str,
    /// Chip identity for platforms where the OS reports the *driver's* strings
    /// instead of the device's USB descriptors (Windows: manufacturer="FTDI",
    /// product="USB Serial Port (COMx)"), so the string match can never succeed
    /// there. Together with `serial_scheme` this identifies the device as a
    /// last resort.
    fallback_vid: u16,
    fallback_pid: u16,
    /// Canonical manufacturer substituted when the OS hides the device's own
    /// strings, so the client_id and UI match the platforms where the same
    /// hardware reports them via its descriptors.
    fallback_manufacturer: &'static str,
    /// Validates a reported serial against the vendor's scheme and normalizes
    /// it to the descriptor form (e.g. "SC1799A" -> "SC1799").
    serial_scheme: fn(&str) -> Option<String>,
    /// Serial line speed (8N1 framing is common to all supported devices).
    baud: u32,
}

/// The Lisle Design "Smart Card Rack" — the only supported device type so far.
pub(super) static LISLE_RACK: DeviceProfile = DeviceProfile {
    product: "Smart Card Rack",
    brand_marker: "lisle",
    fallback_vid: 0x0403,
    fallback_pid: 0x6015,
    fallback_manufacturer: "Lisle Design Ltd",
    serial_scheme: lisle_serial,
    baud: 115_200,
};

/// Every device type the discovery scan looks for.
static PROFILES: &[&DeviceProfile] = &[&LISLE_RACK];

/// How often the monitor scans the bus for devices appearing/disappearing.
pub(super) const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// MQTT client_id construction for a device connection.
///
/// The id must match the server's `^[0-9A-Z]{16}$` (exactly 16 uppercase
/// alphanumerics). Layout is `BRAND` + filler zeros + `SERIAL`: the brand comes
/// from the device's own manufacturer string (no hard-coded value), the serial
/// is kept flush at the end, and the gap between them is padded with zeros.
const RACK_ID_LEN: usize = 16; // server contract: exactly 16 chars
const RACK_ID_BRAND_LEN: usize = 5; // chars of the brand kept in the id
const RACK_ID_PAD: char = '0'; // filler placed between brand and serial

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

/// Builds the device's MQTT client_id as `<brand>` + zero filler + sanitised
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
#[derive(Clone, Debug)]
pub(super) struct RackInfo {
    pub(super) port_name: String,
    pub(super) serial: Option<String>,
    pub(super) manufacturer: Option<String>,
    pub(super) product: Option<String>,
    pub(super) vid: u16,
    pub(super) pid: u16,
    /// Which discovery pass matched this device — diagnostic only.
    matched_by: &'static str,
    /// The device type this hardware matched.
    profile: &'static DeviceProfile,
}

/// `matched_by` is deliberately excluded from equality: the pass that finds a
/// device can vary between poll ticks (e.g. a transient USB enumeration
/// failure downgrades the Windows match to chip identity) and must not read as
/// a device swap — that would tear down and rebuild a healthy connection.
/// `profile` is compared by address: profiles are statics.
impl PartialEq for RackInfo {
    fn eq(&self, other: &Self) -> bool {
        self.port_name == other.port_name
            && self.serial == other.serial
            && self.manufacturer == other.manufacturer
            && self.product == other.product
            && self.vid == other.vid
            && self.pid == other.pid
            && std::ptr::eq(self.profile, other.profile)
    }
}

impl Eq for RackInfo {}

impl RackInfo {
    /// True if the device manufacturer string marks it as a supported device
    /// (contains the profile's brand marker, case-insensitive).
    pub(super) fn is_supported(&self) -> bool {
        self.manufacturer
            .as_deref()
            .map(|m| m.to_ascii_lowercase().contains(self.profile.brand_marker))
            .unwrap_or(false)
    }

    /// The MQTT client_id the server uses to address this device. The brand
    /// prefix is derived from the device's own manufacturer string. Also the
    /// key of every per-rack structure in the app (connections, card sessions,
    /// UI state).
    pub(super) fn client_id(&self) -> String {
        build_client_id(
            self.manufacturer.as_deref().unwrap_or(""),
            self.serial.as_deref(),
        )
    }

    /// Build the frontend payload for this rack. The card list is empty for now;
    /// it will be filled once the server reports the cards in the rack's slots.
    pub(super) fn to_state(&self, connected: bool) -> RackState {
        RackState {
            client_id: self.client_id(),
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
            // A freshly (re)connected rack has not been enumerated yet; the
            // server's `watch` instruction flips this once discovery is done.
            scan_complete: false,
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
        // Poison-recovering like every other lock in the backend (see
        // `super::lock`; this one is inline because the mutex lives inside
        // `self`, not in a static).
        let mut last = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *last == value {
            false
        } else {
            value.clone_into(&mut last);
            true
        }
    }
}

/// Last logged port-inventory snapshot.
static PORT_INVENTORY: ChangeGuard = ChangeGuard::new();

/// Last logged discovery-match outcome (the whole matched set).
static DISCOVERY_MATCH: ChangeGuard = ChangeGuard::new();

/// Last logged set of client_id duplicates dropped by `dedupe_by_client_id`.
/// Must be change-gated like every other discovery log: on macOS every USB
/// serial device enumerates as BOTH its /dev/cu.* and /dev/tty.* node with
/// identical metadata, so duplicates are the NORMAL state there and an
/// unguarded warn would fire on every 2s poll tick.
static DISCOVERY_DUPLICATES: ChangeGuard = ChangeGuard::new();

/// Ports for which the "COM port busy" UI notification has been shown. An
/// entry is dropped when its port opens or its device leaves the bus, so a new
/// busy episode (e.g. after replug) notifies again. A plain Vec: the set is as
/// small as the number of connected devices.
static BUSY_PORT_NOTICES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// One-line description of an enumerated port for the inventory log. USB ports
/// carry the metadata discovery matches on; other transports just get their
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

/// Find every supported device on the bus. Runs all matching passes for every
/// profile, then orders the result deterministically (serial, then port name)
/// and drops client_id duplicates — so which device is "first" never depends
/// on OS enumeration order, which can change across reboots.
pub(super) fn find_racks() -> Vec<RackInfo> {
    let ports = match serialport::available_ports() {
        Ok(ports) => ports,
        Err(e) => {
            let snapshot = format!("enumeration_failed: {e}");
            if PORT_INVENTORY.changed(&snapshot) {
                log::error!("RACK | phase=discovery status=enumeration_failed err={e}");
            }
            return Vec::new();
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

    let mut racks: Vec<RackInfo> = Vec::new();
    for profile in PROFILES {
        // First pass: match by the device's own descriptor strings (macOS/Linux).
        collect_by_descriptor(profile, &ports, &mut racks);
        // Second pass (Windows): the COM node carries the driver's strings, but
        // the parent USB node still holds the device's real iProduct and clean
        // EEPROM serial — match by those.
        #[cfg(windows)]
        collect_by_usb_descriptor(profile, &ports, &mut racks);
        // Last resort: no descriptor strings anywhere — match by chip identity
        // plus the vendor's serial scheme, and substitute the canonical strings
        // so client_id/UI stay identical across platforms.
        collect_by_chip_identity(profile, &ports, &mut racks);
    }

    sort_racks(&mut racks);
    dedupe_by_client_id(&mut racks);

    // One log line per change of the matched set, covering every pass.
    let snapshot = if racks.is_empty() {
        "none".to_string()
    } else {
        racks
            .iter()
            .map(|r| {
                format!(
                    "{{{}:{} serial={}}}",
                    r.matched_by,
                    r.port_name,
                    r.serial.as_deref().unwrap_or("-")
                )
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    if DISCOVERY_MATCH.changed(&snapshot) && !racks.is_empty() {
        log::info!(
            "RACK | phase=discovery status=match count={} list=[{}]",
            racks.len(),
            snapshot
        );
    }

    // Busy notices only survive for ports still matched: a device that left
    // the bus notifies again on its next busy episode.
    super::lock(&BUSY_PORT_NOTICES).retain(|p| racks.iter().any(|r| r.port_name == *p));

    racks
}

/// Deterministic ordering: by serial, then port name.
fn sort_racks(racks: &mut [RackInfo]) {
    racks.sort_by(|a, b| {
        (a.serial.as_deref(), a.port_name.as_str())
            .cmp(&(b.serial.as_deref(), b.port_name.as_str()))
    });
}

/// Two entries that build the same client_id are one MQTT identity — only the
/// first (in the deterministic order) can be served, the rest are dropped.
/// This is the NORMAL state on macOS, where every USB serial device enumerates
/// as both its /dev/cu.* and /dev/tty.* node with identical metadata (the sort
/// keeps /dev/cu.*, the callout node, first). Logged once per change of the
/// dropped set, at info — it is expected housekeeping, not a fault.
fn dedupe_by_client_id(racks: &mut Vec<RackInfo>) {
    let mut seen = std::collections::HashSet::new();
    let mut dropped: Vec<String> = Vec::new();
    racks.retain(|r| {
        let id = r.client_id();
        if seen.insert(id.clone()) {
            true
        } else {
            dropped.push(format!("{}:{}", id, r.port_name));
            false
        }
    });
    let snapshot = if dropped.is_empty() {
        "none".to_string()
    } else {
        dropped.join(" ")
    };
    if DISCOVERY_DUPLICATES.changed(&snapshot) && !dropped.is_empty() {
        log::info!(
            "RACK | phase=discovery status=duplicate_client_id_dropped list=[{}]",
            snapshot
        );
    }
}

/// First pass: the device's own descriptor strings (macOS/Linux).
fn collect_by_descriptor(
    profile: &'static DeviceProfile,
    ports: &[serialport::SerialPortInfo],
    out: &mut Vec<RackInfo>,
) {
    for p in ports {
        if out.iter().any(|r| r.port_name == p.port_name) {
            continue;
        }
        if let SerialPortType::UsbPort(info) = &p.port_type {
            if info.product.as_deref() != Some(profile.product) {
                continue;
            }
            let candidate = RackInfo {
                port_name: p.port_name.clone(),
                serial: info.serial_number.clone(),
                manufacturer: info.manufacturer.clone(),
                product: info.product.clone(),
                vid: info.vid,
                pid: info.pid,
                matched_by: "descriptor",
                profile,
            };
            // The brand check lives in is_supported() only — one predicate for
            // both the discovery pass and the connect-time gate.
            if candidate.is_supported() {
                out.push(candidate);
            }
        }
    }
}

/// Windows: locate devices via the parent USB node. The COM node only shows
/// the FTDI driver's strings ("FTDI" / "USB Serial Port (COMx)"), but Windows
/// caches the device's own iProduct on the USB node (`BusReportedDeviceDesc`,
/// surfaced by nusb as `product_string`) along with the clean EEPROM serial —
/// the same values macOS/Linux read from the descriptors directly, so the
/// resulting client_id matches across platforms. The device is never opened.
#[cfg(windows)]
fn collect_by_usb_descriptor(
    profile: &'static DeviceProfile,
    ports: &[serialport::SerialPortInfo],
    out: &mut Vec<RackInfo>,
) {
    use nusb::MaybeFuture;

    let devices = match nusb::list_devices().wait() {
        Ok(devices) => devices,
        Err(e) => {
            log::warn!("RACK | phase=discovery status=usb_enum_failed err={e}");
            return;
        }
    };

    for dev in devices {
        if dev.product_string() != Some(profile.product) {
            continue;
        }
        let Some(dev_serial) = dev.serial_number() else {
            continue;
        };
        // Link the USB device to its COM port: same chip identity, and the
        // port serial is the device serial plus an optional channel letter
        // appended by the driver ("SC1799" -> "SC1799A").
        for p in ports {
            if out.iter().any(|r| r.port_name == p.port_name) {
                continue;
            }
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
            out.push(RackInfo {
                port_name: p.port_name.clone(),
                serial: Some(dev_serial.to_string()),
                // iManufacturer is not reachable on Windows without opening
                // the device; the product string is the vendor's own EEPROM
                // value, so the canonical manufacturer is substituted.
                manufacturer: Some(profile.fallback_manufacturer.to_string()),
                product: Some(profile.product.to_string()),
                vid: info.vid,
                pid: info.pid,
                matched_by: "usb",
                profile,
            });
            break; // this USB device is linked to its port — next device
        }
    }
}

/// Last resort: no descriptor strings anywhere — match by the profile's chip
/// identity plus its serial scheme, substituting the canonical strings so the
/// client_id and UI stay identical across platforms.
fn collect_by_chip_identity(
    profile: &'static DeviceProfile,
    ports: &[serialport::SerialPortInfo],
    out: &mut Vec<RackInfo>,
) {
    for p in ports {
        if out.iter().any(|r| r.port_name == p.port_name) {
            continue;
        }
        let SerialPortType::UsbPort(info) = &p.port_type else {
            continue;
        };
        if info.vid != profile.fallback_vid || info.pid != profile.fallback_pid {
            continue;
        }
        let Some(serial) = info
            .serial_number
            .as_deref()
            .and_then(profile.serial_scheme)
        else {
            continue;
        };
        out.push(RackInfo {
            port_name: p.port_name.clone(),
            serial: Some(serial),
            manufacturer: Some(profile.fallback_manufacturer.to_string()),
            product: Some(profile.product.to_string()),
            vid: info.vid,
            pid: info.pid,
            matched_by: "chip",
            profile,
        });
    }
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

/// Try to open the device's serial port (8N1). Logs success/failure.
pub(super) fn open_rack(rack: &RackInfo) -> Option<Box<dyn serialport::SerialPort>> {
    match serialport::new(&rack.port_name, rack.profile.baud)
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
                rack.profile.baud
            );
            // The busy episode of this port (if any) is over.
            super::lock(&BUSY_PORT_NOTICES).retain(|p| p != &rack.port_name);
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

/// Emits the "COM port busy" UI notification once per busy episode of a port.
fn notify_port_busy(port_name: &str) {
    {
        let mut notified = super::lock(&BUSY_PORT_NOTICES);
        if notified.iter().any(|p| p == port_name) {
            return;
        }
        notified.push(port_name.to_string());
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

#[cfg(test)]
mod tests {
    use super::*;

    const MFR: &str = "Lisle Design Ltd"; // sample manufacturer string

    /// Test RackInfo with the fields the discovery invariants care about.
    fn info(port: &str, serial: Option<&str>) -> RackInfo {
        RackInfo {
            port_name: port.into(),
            serial: serial.map(str::to_string),
            manufacturer: Some(MFR.into()),
            product: None,
            vid: 0,
            pid: 0,
            matched_by: "descriptor",
            profile: &LISLE_RACK,
        }
    }

    #[test]
    fn lisle_profile_is_set() {
        assert!(!LISLE_RACK.product.is_empty());
        assert!(!LISLE_RACK.brand_marker.is_empty());
        assert!(!LISLE_RACK.fallback_manufacturer.is_empty());
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
            LISLE_RACK.fallback_manufacturer,
            lisle_serial("SC1799A").as_deref(),
        );
        let macos_id = build_client_id("Lisle Design Ltd", Some("SC1799"));
        assert_eq!(windows_id, macos_id);
        assert_eq!(windows_id, "LISLE00000SC1799");
        assert!(matches_server_contract(&windows_id));
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
        let base = info("/dev/x", Some("SC1"));
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

    #[test]
    fn rack_info_equality_detects_swap() {
        let a = info("/dev/x", Some("SC1"));
        let b = RackInfo {
            serial: Some("SC2".into()),
            ..a.clone()
        };
        assert_ne!(a, b); // different serial => treated as a different device
        assert_eq!(a, a.clone());
    }

    #[test]
    fn rack_info_equality_ignores_the_discovery_pass() {
        // Which pass matched can vary between ticks (transient nusb failure
        // falls back to chip identity) — must not read as a device swap.
        let a = info("/dev/x", Some("SC1"));
        let b = RackInfo {
            matched_by: "chip",
            ..a.clone()
        };
        assert_eq!(a, b);
    }

    #[test]
    fn racks_sorted_by_serial_not_enumeration_order() {
        // The client's field case: COM4/SC5465 enumerated before COM3/SC4953.
        let mut racks = vec![info("COM4", Some("SC5465")), info("COM3", Some("SC4953"))];
        sort_racks(&mut racks);
        assert_eq!(racks[0].serial.as_deref(), Some("SC4953"));
        // Same result whatever the initial order.
        let mut racks = vec![info("COM3", Some("SC4953")), info("COM4", Some("SC5465"))];
        sort_racks(&mut racks);
        assert_eq!(racks[0].serial.as_deref(), Some("SC4953"));
    }

    #[test]
    fn distinct_serials_are_all_kept() {
        let mut racks = vec![info("COM3", Some("SC4953")), info("COM4", Some("SC5465"))];
        dedupe_by_client_id(&mut racks);
        assert_eq!(racks.len(), 2);
    }

    #[test]
    fn duplicate_client_ids_keep_only_the_first() {
        // No serial at all → both devices build "LISLE00000000000": one MQTT
        // identity, so only the first (deterministic order) survives.
        let mut racks = vec![info("COM4", None), info("COM3", None)];
        sort_racks(&mut racks);
        dedupe_by_client_id(&mut racks);
        assert_eq!(racks.len(), 1);
        assert_eq!(racks[0].port_name, "COM3");
    }
}
