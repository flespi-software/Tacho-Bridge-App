//! Card presence watches, one per connected rack: a background task that
//! polls the rack with the server's opaque command and reports the reply to
//! the server whenever it changes.

use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::AsyncClient;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tauri::async_runtime::{self, JoinHandle};

use crate::com_port::rack::{app_link_is, rack_topic};
use crate::com_port::transport::{
    execute_envelope, ms_field, normalize_hex, SerialEnvelope, SharedPort, SERIAL_MS_MIN,
    SERIAL_READ_DEADLINE, SERIAL_REPLY_TIMEOUT,
};
use crate::com_port::{lock, rack_port_is_live};

/// One armed presence watch: its task plus the arming it came from. A watch
/// that ends by itself (its rack's link went down) removes its own entry, and
/// `arming` is what keeps it from removing the entry of a newer watch that
/// re-armed the same rack in between.
struct RackWatch {
    arming: u64,
    handle: JoinHandle<()>,
}

/// Arming tokens of the presence watches (see `RackWatch::arming`).
static NEXT_WATCH_ARMING: AtomicU64 = AtomicU64::new(1);

lazy_static::lazy_static! {
    /// Card presence watch tasks, one per connected rack, keyed by rack id.
    static ref RACK_WATCH_TASKS: std::sync::Mutex<HashMap<String, RackWatch>> =
        std::sync::Mutex::new(HashMap::new());
}

/// Removes the map entry of a watch that ended by itself, but only if the
/// entry is still that watch's own.
fn forget_rack_watch(rack_id: &str, arming: u64) {
    let mut watches = lock(&RACK_WATCH_TASKS);
    if watches
        .get(rack_id)
        .is_some_and(|watch| watch.arming == arming)
    {
        watches.remove(rack_id);
    }
}

/// Arms (or re-arms) one rack's card presence watch from a server `rack/<id>/watch` instruction:
/// `{"cmd":"<hex>","interval_ms":1000,"idle_ms":...,"deadline_ms":...}`. A background task
/// executes the opaque command every interval through that rack's FIFO port queue and publishes
/// the reply back (topic `rack/<id>/watch` of the application connection, the standard response
/// envelope) ONLY when its bytes change. Re-arming resets the baseline, so the first successful
/// exchange is always published — that is how the server catches updates missed while its
/// discovery chain was busy. The report goes to the server instance the instruction came from
/// (`generation`, see `rack::app_link_is`); once that instance is gone or the rack's link is
/// down (the port went away), nothing is published any more.
pub(in crate::com_port) fn start_rack_watch(
    payload: &[u8],
    generation: u64,
    rack_id: &str,
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
    let ms = |key: &str| ms_field(&json, key);
    let interval = Duration::from_millis(ms("interval_ms").unwrap_or(1000).max(SERIAL_MS_MIN));
    let refresh = Duration::from_millis(ms("refresh_ms").unwrap_or(30_000).max(1000));
    let idle = ms("idle_ms")
        .map(|ms| Duration::from_millis(ms.max(SERIAL_MS_MIN)))
        .unwrap_or(SERIAL_REPLY_TIMEOUT);
    let deadline = ms("deadline_ms")
        .map(|ms| Duration::from_millis(ms.max(SERIAL_MS_MIN)))
        .unwrap_or(SERIAL_READ_DEADLINE);

    let armed_port = serial_port.clone();
    let serial_port = serial_port.clone();
    let mqtt_client = mqtt_client.clone();
    let log_header = log_header.to_string();
    let watch_topic = rack_topic(rack_id, "watch");
    let watched_rack = rack_id.to_string();
    log::info!(
        "{} [WATCH] status=armed interval={:?} cmd_bytes={} idle_ms={} deadline_ms={}",
        log_header,
        interval,
        cmd_hex.len() / 2,
        idle.as_millis(),
        deadline.as_millis()
    );

    let arming = NEXT_WATCH_ARMING.fetch_add(1, Ordering::Relaxed);
    let handle = async_runtime::spawn(async move {
        let mut last: Option<String> = None;
        let mut last_report = std::time::Instant::now();
        // Consecutive failed exchanges: the poll slows down while they last,
        // so a rack that stopped answering (or a port being reopened) does not
        // put a failing frame and a WARN line on the wire every interval.
        let mut failures: u32 = 0;
        loop {
            tokio::time::sleep(watch_delay(interval, failures)).await;
            // The rack's link went down (or the rack relinked on a new port):
            // this watch's port life is over, and nothing would ever stop it
            // otherwise — every branch below only `continue`s. The relinked
            // rack gets a watch of its own from the server's next `watch`.
            if !rack_port_is_live(&watched_rack, &serial_port) {
                log::info!("{} [WATCH] status=ended reason=link_down", log_header);
                break;
            }
            // The server instance that armed this watch is gone for good: the next one arms its own.
            if !app_link_is(generation) {
                log::info!(
                    "{} [WATCH] status=ended reason=app_connection_changed",
                    log_header
                );
                break;
            }
            let envelope = SerialEnvelope {
                cmd_hex: cmd_hex.clone(),
                expect_hex: None,
                idle,
                deadline,
                poll: None,
                // presence polling is not part of any authentication session
                finish: None,
            };
            let (exchange, end) =
                execute_envelope(&serial_port, envelope, &log_header, false).await;
            if !exchange.is_ok() {
                // transport errors are already logged by execute_envelope; the rack presence
                // monitor handles a truly gone device, so just keep trying (slower)
                failures = failures.saturating_add(1);
                continue;
            }
            failures = 0;
            if last.as_deref() == Some(exchange.resp_hex.as_str())
                && last_report.elapsed() < refresh
            {
                continue;
            }
            // The link went down while the exchange ran: this watch is being
            // stopped, and the server must not hear from the old port life.
            if !rack_port_is_live(&watched_rack, &serial_port) {
                log::debug!(
                    "{} [WATCH] status=report_dropped reason=link_down",
                    log_header
                );
                continue;
            }
            // The application connection changed under this watch: the server
            // instance that armed it is gone, and the new one arms its own.
            if !app_link_is(generation) {
                log::debug!(
                    "{} [WATCH] status=report_dropped reason=app_connection_changed",
                    log_header
                );
                continue;
            }
            // `end` says whether the device finished the reply (silence) or a bound cut it
            // (deadline): a long reply can arrive with pauses inside it, and a cut one is
            // rejected by the server
            log::info!(
                "{} [WATCH] status=change_detected rx_bytes={} end={}",
                log_header,
                exchange.resp_hex.len() / 2,
                end
            );
            match mqtt_client
                .publish(
                    watch_topic.clone(),
                    QoS::AtLeastOnce,
                    false,
                    exchange.to_payload(),
                )
                .await
            {
                // The baseline advances only once the server was actually told:
                // publish can fail immediately (bounded client channel while the
                // connection is down), and advancing it anyway would silently
                // drop this presence change — the next tick must retry it.
                Ok(()) => {
                    last = Some(exchange.resp_hex.clone());
                    last_report = std::time::Instant::now();
                }
                Err(e) => {
                    log::error!("{} [WATCH] status=publish_failed err={:?}", log_header, e);
                }
            }
        }
        forget_rack_watch(&watched_rack, arming);
    });

    let mut watches = lock(&RACK_WATCH_TASKS);
    // Re-checked under the watch-map lock: the rack may have been unlinked
    // between the caller's `linked_rack()` and here, and `unlink_rack` takes
    // this lock (in `stop_rack_watch`) only after removing the rack from
    // RACKS — so either its stop sees this entry, or this sees the rack gone.
    // A watch inserted after that stop would poll a dead port forever.
    if !rack_port_is_live(rack_id, &armed_port) {
        drop(watches);
        handle.abort();
        log::info!(
            "RACK {} | [WATCH] status=skipped reason=rack_not_linked",
            rack_id
        );
        return;
    }
    if let Some(old) = watches.insert(rack_id.to_string(), RackWatch { arming, handle }) {
        old.handle.abort();
    }
}

/// Ceiling of the watch poll interval while its exchanges keep failing.
const WATCH_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// The pause before the next watch exchange: the armed interval, doubled per
/// consecutive failure up to `WATCH_BACKOFF_MAX` (never below the interval).
fn watch_delay(interval: Duration, failures: u32) -> Duration {
    crate::com_port::backoff_delay(interval, failures, WATCH_BACKOFF_MAX)
}

/// Stops one rack's card presence watch task, if armed.
pub(in crate::com_port) fn stop_rack_watch(rack_id: &str) {
    if let Some(watch) = lock(&RACK_WATCH_TASKS).remove(rack_id) {
        watch.handle.abort();
        log::info!("RACK {} | [WATCH] status=stopped", rack_id);
    }
}

/// Stops every rack's watch task. Shutdown backstop: reaps a watch whose rack
/// entry was already removed elsewhere.
pub(in crate::com_port) fn stop_all_rack_watches() {
    let mut guard = lock(&RACK_WATCH_TASKS);
    for (rack_id, watch) in guard.drain() {
        watch.handle.abort();
        log::info!("RACK {} | [WATCH] status=stopped", rack_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_delay_backs_off_on_failures_and_caps() {
        let interval = Duration::from_millis(1000);
        assert_eq!(watch_delay(interval, 0), interval);
        assert_eq!(watch_delay(interval, 1), Duration::from_secs(2));
        assert_eq!(watch_delay(interval, 3), Duration::from_secs(8));
        assert_eq!(watch_delay(interval, 50), WATCH_BACKOFF_MAX);
        // a server interval above the cap is kept as is
        let slow = Duration::from_secs(60);
        assert_eq!(watch_delay(slow, 4), slow);
    }
}
