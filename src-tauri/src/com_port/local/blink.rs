//! Slot blinks the user asks for to find a slot on the rack. A blink owns the
//! indicator of its slot while it runs; the engine writes it anew afterwards.

use std::collections::HashMap;
use std::time::Duration;
use tauri::async_runtime;
use tauri::Emitter;

use crate::com_port::transport::SERIAL_ERR_CARD_BUSY;
use crate::com_port::{access, lock, rack_port_is_live};

use super::engine::write_led;
use super::ENGINES;

/// A slot blink the user asked for to find a slot on the rack: the indicator
/// alternates every `BLINK_HALF` (2 Hz), `BLINK_HALVES` times (6 seconds).
/// One indicator write is a one-shot exchange of a few tens of milliseconds,
/// so the rhythm keeps steady even while other slots authenticate.
const BLINK_HALF: Duration = Duration::from_millis(250);
const BLINK_HALVES: u32 = 24;

lazy_static::lazy_static! {
    /// Slot blinks by rack id and slot: an entry stays after its blink ended
    /// until the engine has written the indicator of the slot again.
    pub(super) static ref BLINKS: std::sync::Mutex<HashMap<(String, u16), BlinkControl>> =
        std::sync::Mutex::new(HashMap::new());
}

/// Control of one blink: `cancel` ends it, `done` carries the reason it
/// ended (`None` while it runs).
#[derive(Clone)]
pub(super) struct BlinkControl {
    cancel: tokio::sync::watch::Sender<bool>,
    pub(super) done: tokio::sync::watch::Receiver<Option<&'static str>>,
}

/// `Some(true)` while a blink runs on the slot, `Some(false)` once it ended
/// and the engine has not written the indicator again, `None` without one.
pub(super) fn blink_state(rack_id: &str, slot: u16) -> Option<bool> {
    lock(&BLINKS)
        .get(&(rack_id.to_string(), slot))
        .map(|control| control.done.borrow().is_none())
}

/// Ends a blink: tells the frontend why, and whoever waits for it.
fn finish_blink(
    done: &tokio::sync::watch::Sender<Option<&'static str>>,
    rack_id: &str,
    slot: u16,
    reason: &'static str,
) {
    if let Some(app) = crate::global_app_handle::get_app_handle() {
        let _ = app.emit(
            "rack-slot-blink-ended",
            serde_json::json!({
                "rack_id": rack_id, "slot": slot, "reason": reason,
            }),
        );
    }
    let _ = done.send(Some(reason));
}

/// Starts a six-second blink, or a continuous blink with an explicit stop.
/// A zero duration tells the frontend that no automatic end is scheduled.
pub(in crate::com_port) async fn blink(
    rack_id: &str,
    slot: u16,
    until_stopped: bool,
) -> Result<Duration, &'static str> {
    let (profile, port, shutdown_slots) = {
        let engines = lock(&ENGINES);
        let engine = engines
            .get(rack_id)
            .filter(|engine| !engine.handle.inner().is_finished())
            .ok_or("not_local")?;
        (
            engine.profile.clone(),
            engine.port.clone(),
            engine.shutdown_slots.clone(),
        )
    };
    if slot == 0 || usize::from(slot) > profile.status_length {
        return Err("bad_slot");
    }
    let on = profile.led_served.clone().ok_or("no_indicator")?;
    let other = profile
        .led_unserved
        .clone()
        .or_else(|| profile.led_off.clone())
        .ok_or("no_indicator")?;
    if access::auth_active(&port, slot) {
        return Err(SERIAL_ERR_CARD_BUSY);
    }
    let key = (rack_id.to_string(), slot);
    let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
    let (done, done_rx) = tokio::sync::watch::channel(None);
    {
        let mut blinks = lock(&BLINKS);
        if blinks
            .get(&key)
            .is_some_and(|control| control.done.borrow().is_none())
        {
            return Err("already_blinking");
        }
        blinks.insert(
            key,
            BlinkControl {
                cancel,
                done: done_rx,
            },
        );
    }
    let log_header = format!("RACK {} |", rack_id);
    lock(&shutdown_slots).addresses.insert(slot);
    if !write_led(&profile, &port, Some(&on), slot, &log_header).await {
        finish_blink(&done, rack_id, slot, "led_rejected");
        return Err("led_rejected");
    }
    log::info!(
        "{} [LOCAL] status=blink_started slot={} continuous={}",
        log_header,
        slot,
        until_stopped
    );
    let rack = rack_id.to_string();
    async_runtime::spawn(async move {
        let mut ticks =
            tokio::time::interval_at(tokio::time::Instant::now() + BLINK_HALF, BLINK_HALF);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut halves = 1u32;
        let mut show_on = true;
        let ended = loop {
            if *cancelled.borrow() {
                break "stopped";
            }
            tokio::select! {
                _ = cancelled.changed() => { break "stopped"; }
                _ = ticks.tick() => {}
            }
            if !rack_port_is_live(&rack, &port) {
                break "rack_gone";
            }
            if access::auth_active(&port, slot) {
                break SERIAL_ERR_CARD_BUSY;
            }
            if !until_stopped && halves >= BLINK_HALVES {
                break "done";
            }
            show_on = !show_on;
            let template = if show_on { &on } else { &other };
            if !write_led(&profile, &port, Some(template), slot, &log_header).await {
                break "led_rejected";
            }
            halves = halves.saturating_add(1);
        };
        // Do not touch an indicator now owned by authentication or a dead rack.
        if rack_port_is_live(&rack, &port) && !access::auth_active(&port, slot) {
            write_led(
                &profile,
                &port,
                profile.led_off.as_deref(),
                slot,
                &log_header,
            )
            .await;
        }
        finish_blink(&done, &rack, slot, ended);
        log::info!(
            "{} [LOCAL] status=blink_finished slot={} reason={}",
            log_header,
            slot,
            ended
        );
    });
    Ok(if until_stopped {
        Duration::ZERO
    } else {
        BLINK_HALF * BLINK_HALVES
    })
}

/// Stops the blink of a slot and waits until it has ended; nothing to do
/// without one.
pub(in crate::com_port) async fn stop_blink(rack_id: &str, slot: u16) -> Result<(), &'static str> {
    let control = lock(&BLINKS).get(&(rack_id.to_string(), slot)).cloned();
    let Some(mut control) = control else {
        return Ok(());
    };
    let _ = control.cancel.send(true);
    while control.done.borrow().is_none() {
        if control.done.changed().await.is_err() {
            break;
        }
    }
    Ok(())
}

/// Cancels every running slot blink of a rack: each darkens its slot and ends as stopped.
pub(super) fn cancel_blinks(rack_id: &str) {
    for ((rack, _), control) in lock(&BLINKS).iter() {
        if rack == rack_id {
            let _ = control.cancel.send(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stopping_a_continuous_blink_waits_for_indicator_cleanup_and_is_repeatable() {
        let rack = "TEST-CONTINUOUS-STOP";
        let slot = 1;
        let key = (rack.to_string(), slot);
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let (done, done_rx) = tokio::sync::watch::channel(None);
        lock(&BLINKS).insert(
            key.clone(),
            BlinkControl {
                cancel,
                done: done_rx,
            },
        );
        assert_eq!(blink_state(rack, slot), Some(true));
        let stopping = tokio::spawn(stop_blink(rack, slot));
        cancelled.changed().await.unwrap();
        assert!(*cancelled.borrow());
        assert!(!stopping.is_finished());
        done.send(Some("stopped")).unwrap();
        stopping.await.unwrap().unwrap();
        assert_eq!(blink_state(rack, slot), Some(false));
        stop_blink(rack, slot).await.unwrap();
        lock(&BLINKS).remove(&key);
    }
}
