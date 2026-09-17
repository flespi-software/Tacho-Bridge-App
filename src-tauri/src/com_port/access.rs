//! Slot ownership across MQTT round trips. Discovery and authentication must not
//! reset or select files on the same card concurrently.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use super::transport::SharedPort;

type PortWeak = Weak<tokio::sync::Mutex<Box<dyn serialport::SerialPort>>>;
#[derive(Default)]
struct Slots {
    leases: HashMap<u16, (bool, Instant, u64)>, // true = discovery, false = authentication
}
impl Slots {
    fn acquire(&mut self, slot: u16, discovery: bool, now: Instant, token: u64) -> bool {
        if self
            .leases
            .get(&slot)
            .is_some_and(|(owner, until, _)| *until > now && *owner != discovery)
        {
            return false;
        }
        // The server abandons authentication after a 60 second gap. A small
        // margin avoids a clock-boundary race and bounds abandoned discoveries.
        self.leases
            .insert(slot, (discovery, now + Duration::from_secs(65), token));
        true
    }
    /// Returns whether a lease was actually removed.
    fn release(&mut self, slot: u16, discovery: bool, token: Option<u64>) -> bool {
        if self.leases.get(&slot).is_some_and(|(owner, _, current)| {
            *owner == discovery && token.is_none_or(|token| token == *current)
        }) {
            self.leases.remove(&slot);
            return true;
        }
        false
    }
}
type Registry = Vec<(PortWeak, Arc<Mutex<Slots>>)>;
static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
fn slots(port: &SharedPort) -> Arc<Mutex<Slots>> {
    let mut registry = REGISTRY
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.retain(|(port, _)| port.strong_count() > 0);
    let weak = Arc::downgrade(port);
    if let Some((_, slots)) = registry.iter().find(|(p, _)| p.ptr_eq(&weak)) {
        return slots.clone();
    }
    let slots = Arc::new(Mutex::new(Slots::default()));
    registry.push((weak, slots.clone()));
    slots
}
pub(super) async fn acquire(port: &SharedPort, slot: u16, discovery: bool, token: u64) -> bool {
    let slots = slots(port);
    let started = Instant::now();
    let granted = loop {
        if slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .acquire(slot, discovery, Instant::now(), token)
        {
            break true;
        }
        // Discovery defers an active card; a new tracker waits for the short
        // discovery transaction to finish without taking the serial port lock.
        if discovery || started.elapsed() >= Duration::from_secs(65) {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    // Debug log: who holds which slot is what tells a "card busy" refusal
    // from a command that went to the wrong slot.
    log::debug!(
        "SLOT {} owner={} token={} status={} waited_ms={}",
        slot,
        if discovery { "discovery" } else { "auth" },
        token,
        if granted { "acquired" } else { "refused" },
        started.elapsed().as_millis()
    );
    granted
}
pub(super) fn release(port: &SharedPort, slot: u16, discovery: bool, token: Option<u64>) {
    let released = slots(port)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .release(slot, discovery, token);
    log::debug!(
        "SLOT {} owner={} token={} status={}",
        slot,
        if discovery { "discovery" } else { "auth" },
        token.map(|t| t.to_string()).unwrap_or_else(|| "any".to_string()),
        if released { "released" } else { "release_ignored" }
    );
}

/// Every live slot lease — part of the state snapshot the extended debug log
/// starts with.
pub(super) fn log_leases_snapshot() {
    let registry = REGISTRY
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    let mut rows = Vec::new();
    for (port, slots) in registry.iter().filter(|(port, _)| port.strong_count() > 0) {
        let slots = slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (slot, (discovery, until, token)) in slots.leases.iter() {
            rows.push(format!(
                "{:p}/{}:{}:token={}:left_ms={}",
                port.as_ptr(),
                slot,
                if *discovery { "discovery" } else { "auth" },
                token,
                until.saturating_duration_since(now).as_millis()
            ));
        }
    }
    rows.sort();
    log::info!("[DEBUG] snapshot=slot_leases count={} leases={}", rows.len(), rows.join(","));
}
pub(super) fn cancel_discovery() {
    let mut registry = REGISTRY
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.retain(|(port, _)| port.strong_count() > 0);
    for (_, slots) in registry.iter() {
        slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .leases
            .retain(|_, (discovery, _, _)| !*discovery);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ownership_covers_round_trips_and_is_scoped_to_slot() {
        let mut slots = Slots::default();
        let now = Instant::now();
        assert!(slots.acquire(1, false, now, 1));
        assert!(!slots.acquire(1, true, now, 2));
        assert!(slots.acquire(2, true, now, 2));
        assert!(!slots.acquire(2, false, now, 1));
        slots.release(1, true, Some(2)); // an unrelated release cannot end authentication
        assert!(!slots.acquire(1, true, now, 2));
        slots.release(1, false, None);
        assert!(slots.acquire(1, true, now, 2));
        slots.release(1, true, Some(2));
        assert!(slots.acquire(1, false, now, 1));
        assert!(slots.acquire(2, false, now + Duration::from_secs(66), 3));
        slots.release(1, false, None);
        assert!(slots.acquire(1, true, now, 4));
        slots.release(1, true, Some(2));
        assert!(!slots.acquire(1, false, now, 5));
    }
}
