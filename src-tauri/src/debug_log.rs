//! Extended debug log: the `debug_log` server command.
//!
//! The server publishes `{"name":"debug_log","enable":true,"duration":<secs>}`
//! to `request/<request_id>/0` on the app connection. `duration` `0` keeps the
//! debug log on until an explicit `enable:false`; any other value switches it
//! off on its own after that many seconds. The reply goes to
//! `debug/<request_id>/done`: `{"name":"debug_log","enabled":bool,"until":ts}`
//! (`until` `0` = no expiry) or `{"error":...}`.
//!
//! What the debug log adds is everything the server cannot see by itself —
//! which port an exchange ran on, how long it queued for the port, when the
//! first byte came, why the read ended, slot leases, card-set reconciliation,
//! a state snapshot at switch-on. Payload bytes stay out of it: the server has
//! them already (it built every command and receives every reply), and raw
//! rack frames must not land in users' log files. Frames are identified by a
//! short digest instead (see `frame_digest`), which the server can compute
//! from its own copy to match the two logs.
//!
//! The window is persisted in config.yaml (`debug_log_until`) so a crash or
//! restart inside it resumes the debug log for the remainder.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rumqttc::v5::mqttbytes::v5::Publish;
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::AsyncClient;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tauri::async_runtime;

/// Longest window a single command can ask for; `0` still means "no expiry".
const MAX_DURATION_SECS: u64 = 7 * 24 * 3600;

/// The active window and its generation, under one lock so an expiry timer
/// can check "still my window" and switch it off atomically.
struct Window {
    /// `Some(0)` = on until switched off, `Some(ts)` = on until that unix
    /// time, `None` = off. Mirrors the persisted config value.
    until: Option<u64>,
    /// Bumped on every switch so an expiry timer of an earlier window, still
    /// sleeping, finds itself outdated instead of switching off the newer one.
    generation: u64,
}

static WINDOW: Mutex<Window> = Mutex::new(Window {
    until: None,
    generation: 0,
});

/// Serializes `persist` so the last write to run always stores the latest
/// window: two quick switches persist from two blocking tasks in any order.
static PERSIST_LOCK: Mutex<()> = Mutex::new(());

fn window() -> std::sync::MutexGuard<'static, Window> {
    WINDOW
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Current state for the settings report: `{"enabled":bool,"until":ts}`.
pub fn status_json() -> Value {
    let until = window().until;
    json!({
        "enabled": until.is_some() || crate::logger::extended_debug_is_on(),
        "until": until.unwrap_or(0),
    })
}

/// The server's send time of a publish (its `timestamp` user property, unix
/// seconds with a fraction), or `-` when the publish carries none.
pub fn server_timestamp(publish: &Publish) -> String {
    publish
        .properties
        .as_ref()
        .and_then(|p| {
            p.user_properties
                .iter()
                .find(|(key, _)| key == "timestamp")
                .map(|(_, value)| value.clone())
        })
        .unwrap_or_else(|| "-".to_string())
}

/// Short identity of a frame for the debug log: the first 8 hex digits of
/// SHA-256 over the frame bytes (`-` for an empty or malformed hex string).
/// Lets the server match a logged exchange to the bytes it sent or received
/// without the bytes themselves appearing in the log.
pub fn frame_digest(hex_str: &str) -> String {
    match hex::decode(hex_str) {
        Ok(bytes) if !bytes.is_empty() => {
            let digest = Sha256::digest(&bytes);
            hex::encode(&digest[..4])
        }
        _ => "-".to_string(),
    }
}

/// Resumes the window persisted by a previous run, or clears one that has
/// expired meanwhile. Called once at startup, after the config is loaded.
pub fn restore_from_config() {
    match crate::config::debug_log_until() {
        None => {}
        Some(0) => apply(Some(0), "restart"),
        Some(until) if until > now_unix() => apply(Some(until), "restart"),
        Some(until) => {
            log::info!(
                "[DEBUG] status=expired_while_down until={} action=cleared",
                until
            );
            persist();
        }
    }
}

/// Handles a `debug_log` command (routed here by `commands_settings`).
pub fn handle(client: &AsyncClient, log_header: &str, request_id: u64, payload: &Value) {
    let done_topic = format!("debug/{}/done", request_id);

    let enable = match payload.get("enable") {
        None => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            reply(
                client,
                log_header,
                done_topic,
                json!({"error": "enable must be a boolean"}),
            );
            return;
        }
    };
    let duration = match payload.get("duration") {
        None => 0,
        Some(v) => match v.as_u64() {
            Some(secs) if secs <= MAX_DURATION_SECS => secs,
            _ => {
                reply(
                    client,
                    log_header,
                    done_topic,
                    json!({"error": format!("duration must be 0..={} seconds", MAX_DURATION_SECS)}),
                );
                return;
            }
        },
    };

    log::info!(
        "{} [DEBUG] status=command request_id={} enable={} duration_s={}",
        log_header,
        request_id,
        enable,
        duration
    );
    set_window(
        enable,
        duration,
        &format!("server request_id={}", request_id),
    );
    reply(client, log_header, done_topic, status_json_named());
}

/// Opens (or closes) a debug window: `duration` in seconds, `0` = no expiry.
/// Shared by the server command and the settings dialog; persists in the
/// background (disk I/O under the config lock stays off the caller's task).
pub fn set_window(enable: bool, duration: u64, source: &str) {
    let until = match (enable, duration) {
        (false, _) => None,
        (true, 0) => Some(0),
        (true, secs) => Some(now_unix() + secs),
    };
    apply(until, source);
    drop(async_runtime::spawn_blocking(persist));
}

/// Tauri command behind Settings → Diagnostics: the same switch the server
/// command drives, with the duration picked from a list.
#[tauri::command]
pub async fn set_debug_log(enabled: bool, duration: u64) -> Result<(), String> {
    if duration > MAX_DURATION_SECS {
        return Err(format!(
            "duration must be 0..={} seconds",
            MAX_DURATION_SECS
        ));
    }
    set_window(enabled, duration, "settings");
    Ok(())
}

fn status_json_named() -> Value {
    let mut status = status_json();
    status["name"] = json!("debug_log");
    status
}

/// Switches the debug log to the given window and arms its expiry timer.
fn apply(until: Option<u64>, source: &str) {
    let generation = {
        let mut window = window();
        window.generation += 1;
        window.until = until;
        window.generation
    };
    switched(until, generation, source);
}

/// Side effects of a switch already recorded in `WINDOW`: the logger gate,
/// the log line, the expiry timer, and a refresh of the settings dialog
/// (a server command or an expiry must not leave its toggle stale).
fn switched(until: Option<u64>, generation: u64, source: &str) {
    match until {
        None => {
            crate::logger::set_extended_debug(false);
            log::info!("[DEBUG] status=disabled source={}", source);
        }
        Some(expiry) => {
            crate::logger::set_extended_debug(true);
            log::info!(
                "[DEBUG] status=enabled until={} source={} version={}",
                expiry,
                source,
                env!("CARGO_PKG_VERSION")
            );
            crate::com_port::log_state_snapshot();
            if expiry > 0 {
                async_runtime::spawn(async move {
                    // Wall-clock expiry checked in bounded steps: the tokio
                    // timer stops while the machine sleeps, so one long sleep
                    // would overrun the window by the whole suspend time.
                    loop {
                        let remaining = expiry.saturating_sub(now_unix());
                        if remaining == 0 {
                            break;
                        }
                        tokio::time::sleep(Duration::from_secs(remaining.min(60))).await;
                        if window().generation != generation {
                            return; // a later command replaced this window
                        }
                    }
                    let generation = {
                        let mut window = window();
                        if window.generation != generation {
                            return; // a later command replaced this window
                        }
                        window.generation += 1;
                        window.until = None;
                        window.generation
                    };
                    switched(None, generation, "expired");
                    drop(async_runtime::spawn_blocking(persist));
                });
            }
        }
    }
    if let Some(app) = crate::global_app_handle::get_app_handle() {
        if let Err(e) = crate::config::emit_global_config_server(&app) {
            log::warn!("[DEBUG] status=config_emit_failed err={}", e);
        }
    }
}

/// Persists the window in effect at the time of the write (not the one at
/// the time of the switch), so racing persists cannot resurrect an old one.
fn persist() {
    let _guard = PERSIST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let until = window().until;
    if let Err(e) = crate::config::set_debug_log_until(until) {
        log::error!("[DEBUG] status=persist_failed err={}", e);
    }
}

/// Bound of one command reply publish.
const REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

fn reply(client: &AsyncClient, log_header: &str, topic: String, value: Value) {
    let client = client.clone();
    let log_header = log_header.to_string();
    async_runtime::spawn(async move {
        // Bounded: a full request channel of a connection stuck offline would
        // otherwise park this task for as long as the outage lasts.
        let publish = client.publish(&topic, QoS::AtLeastOnce, false, value.to_string());
        let err = match tokio::time::timeout(REPLY_TIMEOUT, publish).await {
            Ok(Ok(())) => return,
            Ok(Err(e)) => format!("{:?}", e),
            Err(_) => "timeout".to_string(),
        };
        log::error!(
            "{} [DEBUG] status=reply_failed topic={} err={}",
            log_header,
            topic,
            err
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_digest_is_stable_short_and_hides_the_bytes() {
        let digest = frame_digest("C0DE01");
        assert_eq!(digest.len(), 8);
        assert_eq!(digest, frame_digest("c0de01")); // case-insensitive input
        assert_ne!(digest, frame_digest("C0DE02"));
        assert!(!digest.contains("C0DE"));
    }

    #[test]
    fn frame_digest_of_nothing_is_a_dash() {
        assert_eq!(frame_digest(""), "-");
        assert_eq!(frame_digest("not hex"), "-");
    }

    #[test]
    fn status_reports_the_window() {
        window().until = Some(42);
        let status = status_json();
        assert_eq!(status["enabled"], true);
        assert_eq!(status["until"], 42);
        window().until = None;
    }
}
