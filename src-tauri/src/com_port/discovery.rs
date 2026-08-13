//! Rack device discovery: finding the Lisle rack on the USB-serial bus and
//! opening its port.
//!
//! Matching is by the device's own descriptor strings where the OS exposes them
//! (macOS/Linux), by the parent USB node on Windows (where the COM node only
//! carries the driver's strings), and by chip identity + serial scheme as a last
//! resort. vid/pid are logged but never used as the primary match.

use std::time::Duration;

use serialport::SerialPortType;

use super::transport::SERIAL_REPLY_TIMEOUT;

use crate::global_app_handle::{emit_notification_event, NotificationPayload, RackState};

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
pub(super) const POLL_INTERVAL: Duration = Duration::from_secs(2);

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
pub(super) struct RackInfo {
    pub(super) port_name: String,
    pub(super) serial: Option<String>,
    pub(super) manufacturer: Option<String>,
    pub(super) product: Option<String>,
    pub(super) vid: u16,
    pub(super) pid: u16,
}

impl RackInfo {
    /// True if the device manufacturer string marks it as a supported rack
    /// (contains the brand marker, case-insensitive).
    pub(super) fn is_supported(&self) -> bool {
        self.manufacturer
            .as_deref()
            .map(|m| m.to_ascii_lowercase().contains(RACK_BRAND_MARKER))
            .unwrap_or(false)
    }

    /// The MQTT client_id the server uses to address this rack. The brand prefix
    /// is derived from the device's own manufacturer string.
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
        // Poison-recovering like every other lock in the backend: this guard is
        // touched from the discovery loop on the blocking pool, and a panic in
        // any holder would otherwise poison it permanently — killing rack
        // discovery for the rest of the process. The stored value is a plain
        // String replaced in one assignment, so it is never half-updated.
        let mut last = match self.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if *last == value {
            false
        } else {
            value.clone_into(&mut last);
            true
        }
    }

    /// Forget the stored value so the next `changed` reports true again.
    fn reset(&self) {
        let mut last = match self.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        last.clear();
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
pub(super) fn find_rack() -> Option<RackInfo> {
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
pub(super) fn open_rack(rack: &RackInfo) -> Option<Box<dyn serialport::SerialPort>> {
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
