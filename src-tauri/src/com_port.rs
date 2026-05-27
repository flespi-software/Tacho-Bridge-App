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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// ───── Serial ─────
use serialport::SerialPortType;

// ───── Local Modules ─────
use crate::global_app_handle::{rack_emit_event, RackState};

/// Guards against starting more than one rack monitor. The `frontend-loaded`
/// event in `lib.rs` can fire several times at startup, which would otherwise
/// spawn duplicate monitors.
static MONITOR_RUNNING: AtomicBool = AtomicBool::new(false);

/// USB string descriptors the rack advertises. We match on BOTH: the vendor
/// manufacturer string and the product string — both are set by the device
/// vendor and together identify the rack specifically. We deliberately do NOT
/// match on USB vid/pid — those belong to the underlying USB-serial chip
/// (shared across many unrelated adapters and liable to change between hardware
/// revisions). vid/pid are only read and logged, to collect data for now.
const RACK_MANUFACTURER: &str = "Lisle Design Ltd";
const RACK_PRODUCT: &str = "Smart Card Rack";

/// Serial line settings for the rack link.
const BAUD: u32 = 115_200;

/// How often the monitor scans the bus for the rack appearing/disappearing.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

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

/// Find the rack's serial port by its USB manufacturer + product strings. Both
/// must match. vid/pid are recorded for logging but never used as match criteria.
fn find_rack() -> Option<RackInfo> {
    let ports = serialport::available_ports().ok()?;

    for p in &ports {
        if let SerialPortType::UsbPort(info) = &p.port_type {
            if info.manufacturer.as_deref() == Some(RACK_MANUFACTURER)
                && info.product.as_deref() == Some(RACK_PRODUCT)
            {
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
        .timeout(Duration::from_millis(500))
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

/// Called when the rack transitions to connected. Logs readiness; this is the
/// single place to later announce readiness to the server over MQTT and emit a
/// frontend event.
fn on_rack_connected(rack: &RackInfo, _port: Box<dyn serialport::SerialPort>) {
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
    log::info!(
        "[RACK] phase=ready status=rack_connected_ready_for_work serial={}",
        rack.serial.as_deref().unwrap_or("?")
    );

    // Tell the frontend the rack is present. The card list is empty for now —
    // the server doesn't yet report the cards held in the rack's slots.
    rack_emit_event(rack.to_state(true));

    // TODO: open this rack's own MQTT connection, announce readiness to the
    // server, then forward server bytes <-> the serial `_port`. When the server
    // starts reporting cards, fill `RackState.cards` and re-emit.
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

    // TODO: tear down the rack's MQTT connection / mark it not-ready to the server.
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
            // No change (present-present same, or absent-absent).
            _ => {}
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rack_identity_strings_are_set() {
        // Sanity: discovery relies on both non-empty identity strings.
        assert!(!RACK_MANUFACTURER.is_empty());
        assert!(!RACK_PRODUCT.is_empty());
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
