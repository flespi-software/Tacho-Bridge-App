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
    fn release(&mut self, slot: u16, discovery: bool, token: Option<u64>) {
        if self.leases.get(&slot).is_some_and(|(owner, _, current)| {
            *owner == discovery && token.is_none_or(|token| token == *current)
        }) {
            self.leases.remove(&slot);
        }
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
    loop {
        if slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .acquire(slot, discovery, Instant::now(), token)
        {
            return true;
        }
        // Discovery defers an active card; a new tracker waits for the short
        // discovery transaction to finish without taking the serial port lock.
        if discovery || started.elapsed() >= Duration::from_secs(65) {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
pub(super) fn release(port: &SharedPort, slot: u16, discovery: bool, token: Option<u64>) {
    slots(port)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .release(slot, discovery, token);
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
