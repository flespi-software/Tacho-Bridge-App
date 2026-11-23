//! Darkening the indicators when the app closes: the slots each engine has
//! seen, the measured reply time of an indicator write, and the off commands
//! written once the engines are gone.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::com_port::transport::{self, SharedPort, ShutdownCommand};
use crate::com_port::{lock, ShutdownEstimate};

use super::profile::{Class, Profile};
use super::ENGINES;

/// Addresses actually seen on this port, or explicitly signalled by the user.
/// Keep the history: an emptied or temporarily silent reader may still be lit.
/// The profile's status_length is a protocol capacity, not a physical inventory.
#[derive(Default)]
pub(super) struct ShutdownSlots {
    pub(super) addresses: BTreeSet<u16>,
    pub(super) timing: ReplyTiming,
}

/// Reply times of the indicator writes on one port, for the shutdown estimate.
#[derive(Default)]
pub(super) struct ReplyTiming {
    total_ms: f64,
    samples: usize,
}

impl ReplyTiming {
    /// Adds one measured reply time.
    pub(super) fn record(&mut self, duration: Duration) {
        self.total_ms += duration.as_secs_f64() * 1000.0;
        self.samples += 1;
    }

    /// The mean reply time, a default before anything was measured.
    fn average_ms(&self) -> f64 {
        if self.samples == 0 {
            15.0
        } else {
            self.total_ms / self.samples as f64
        }
    }
}

impl ShutdownSlots {
    /// Adds every address the status reading shows a reader at.
    pub(super) fn remember_status(&mut self, profile: &Profile, region: &[u8]) {
        for (index, &raw) in region.iter().take(profile.status_length).enumerate() {
            if profile.classify(raw) != Class::None {
                self.addresses.insert((index + 1) as u16);
            }
        }
    }
}

/// The off commands of one rack, with the port and the timings to write them
/// once its engine is gone.
pub(in crate::com_port) struct IndicatorShutdown {
    rack_id: String,
    port: SharedPort,
    commands: Vec<ShutdownCommand>,
    idle: Duration,
    reply_timeout: Duration,
}

/// Snapshot before stop_all discards the in-memory profiles and port handles.
pub(in crate::com_port) fn prepare_indicator_shutdown() -> Vec<IndicatorShutdown> {
    lock(&ENGINES)
        .iter()
        .filter_map(|(rack_id, engine)| {
            if engine.profile.led_off.is_none() {
                log::warn!(
                    "RACK {} | [LED] phase=shutdown status=skipped reason=no_off_template",
                    rack_id
                );
                return None;
            }
            let commands = engine
                .profile
                .shutdown_commands(&lock(&engine.shutdown_slots));
            if commands.is_empty() {
                return None;
            }
            Some(IndicatorShutdown {
                rack_id: rack_id.clone(),
                port: engine.port.clone(),
                commands,
                idle: engine.profile.slot_idle,
                reply_timeout: engine.profile.slot_deadline,
            })
        })
        .collect()
}

/// Controllers run in parallel; estimate the longest serial queue, not the
/// sum across controllers. Count reader indicators, including empty slots.
pub(in crate::com_port) fn shutdown_estimate() -> ShutdownEstimate {
    estimate_cleanup(lock(&ENGINES).values().filter_map(|engine| {
        engine.profile.led_off.as_ref()?;
        let slots = lock(&engine.shutdown_slots);
        let count = slots
            .addresses
            .iter()
            .filter(|slot| **slot > 0 && usize::from(**slot) <= engine.profile.status_length)
            .count();
        let framed = engine.profile.short_reply_complete(1).is_some();
        let mean = slots.timing.average_ms()
            + if framed {
                0.0
            } else {
                engine.profile.slot_idle.as_secs_f64() * 1000.0
            };
        Some((count, mean))
    }))
}

/// The estimate over racks given as (indicator count, mean write time in ms).
fn estimate_cleanup(racks: impl IntoIterator<Item = (usize, f64)>) -> ShutdownEstimate {
    let mut slots = 0;
    let mut longest_ms: f64 = 0.0;
    for (count, mean_ms) in racks {
        slots += count;
        longest_ms = longest_ms.max(count as f64 * mean_ms);
    }
    ShutdownEstimate {
        slots,
        estimated_ms: (longest_ms * 1.15 + 250.0).ceil().max(500.0) as u64,
    }
}

/// Writes the off commands of every rack, the racks in parallel.
pub(in crate::com_port) async fn turn_off_indicators(plans: Vec<IndicatorShutdown>) {
    // Separate controllers are independent. A missing controller must not
    // prevent the others being darkened, or multiply the exit timeout.
    let mut tasks = tokio::task::JoinSet::new();
    for plan in plans {
        tasks.spawn(transport::write_shutdown_commands(
            plan.port,
            plan.commands,
            plan.idle,
            plan.reply_timeout,
            Duration::from_secs(20),
            format!("RACK {} |", plan.rack_id),
        ));
    }
    while let Some(result) = tasks.join_next().await {
        if let Err(e) = result {
            log::error!("Indicator cleanup failed: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::profile::sum8;
    use super::super::test_profile::{profile, BUSY, ERROR, FREE, IDLE, NONE};
    use super::*;

    #[test]
    fn shutdown_estimate_uses_measured_latency_and_parallel_controller_queues() {
        let mut timing = ReplyTiming::default();
        assert_eq!(timing.average_ms(), 15.0);
        timing.record(Duration::from_millis(10));
        timing.record(Duration::from_millis(20));
        assert_eq!(timing.average_ms(), 15.0);
        let single = estimate_cleanup([(200, timing.average_ms())]);
        let parallel = estimate_cleanup([(200, 15.0), (200, 15.0)]);
        assert_eq!(single.slots, 200);
        assert_eq!(single.estimated_ms, 3700);
        assert_eq!(parallel.slots, 400);
        assert_eq!(parallel.estimated_ms, single.estimated_ms);
        assert_eq!(estimate_cleanup([]).estimated_ms, 500);
    }

    #[test]
    fn shutdown_darkens_every_address_in_a_large_rack_using_its_profile() {
        let mut profile = profile();
        profile.status_length = 240;
        let mut slots = ShutdownSlots::default();
        let mut region = vec![NONE; 240];
        region[..180].fill(IDLE);
        slots.remember_status(&profile, &region);
        let commands = profile.shutdown_commands(&slots);
        assert_eq!(commands.len(), 180);
        for (index, command) in commands.iter().enumerate() {
            assert_eq!(command.bytes[2], (index + 1) as u8);
            assert_eq!(command.bytes[3], 0xB7); // The made-up profile's off command.
            assert_eq!(sum8(&command.bytes), 0);
            assert!(command.complete.is_some());
        }
        // No assumed device bytes if the server has supplied no off template.
        profile.led_off = None;
        assert!(profile.shutdown_commands(&slots).is_empty());
    }

    #[test]
    fn shutdown_skips_unused_capacity_but_keeps_empty_error_and_previously_seen_slots() {
        let mut profile = profile();
        profile.status_length = 240;
        let mut slots = ShutdownSlots::default();
        assert!(profile.shutdown_commands(&slots).is_empty());
        let mut region = vec![NONE; 240];
        // Six readers in a row: a card, two empty, an error, a busy card, a card.
        region[..6].copy_from_slice(&[IDLE, FREE, FREE, ERROR, BUSY, IDLE]);
        slots.remember_status(&profile, &region);
        assert_eq!(profile.shutdown_commands(&slots).len(), 6);
        // Remember removed cards / readers, and a manual blink in an address gap.
        region[5] = NONE;
        slots.remember_status(&profile, &region);
        slots.addresses.insert(20);
        slots.addresses.insert(0); // Invalid addresses are never emitted.
        slots.addresses.insert(241);
        let addresses: Vec<_> = profile
            .shutdown_commands(&slots)
            .iter()
            .map(|command| command.bytes[2])
            .collect();
        assert_eq!(addresses, [1, 2, 3, 4, 5, 6, 20]);
    }
}
