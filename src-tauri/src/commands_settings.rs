//! The server-command side of the application connection: the commands the
//! server sends to TBA, and the settings TBA reports back.
//!
//! Commands are JSON publishes on `request/<request_id>/0` with a `name`
//! field; `dispatch_request` routes each name to its handler module (which
//! owns that command's state and reply topic):
//!   * `fetch_logs`      → `logs_upload`
//!   * `debug_log`       → `debug_log`
//!   * `set_credentials` → `credentials`
//!   * `set_server`      → `server_address`
//!   * `get_settings`    → the settings report on demand (below)
//!
//! The contract of every command is written up in `communication_protocol.md`.
//!
//! Right after the app connection is established, TBA publishes a one-shot
//! settings report so the server can populate the read-only device settings.
//! The payload is a JSON object keyed by setting name (`app_info`, `debug_log`
//! — the state of the extended debug log, `server` — the address in use,
//! `authentication` — the switch, username and whether the pair is saved,
//! never the password), so more settings can be reported later without
//! changing the topic or the format. The same object answers the
//! `get_settings` command on `settings/<request_id>/done`.

use std::time::Duration;

use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::AsyncClient;
use serde_json::Value;

/// Bound of one reply / report publish (same as the other command replies,
/// see `debug_log::REPLY_TIMEOUT`). `publish().await` parks while the
/// client's bounded request channel is full, and a connection stuck offline
/// never drains it: unbounded, the task would hang for the whole outage — for
/// `reply_then_reconnect` that also means the reconnect it exists for never
/// happens.
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(60);

/// Card-link silence that counts as "no authentication in flight" — the same
/// rule the updater applies before an install restarts the app.
const RECONNECT_QUIET_SECS: u64 = 60;

/// Upper bound of the wait for that silence. Past it the reconnect goes ahead
/// anyway: the new settings are already persisted, and a link that is never
/// quiet (a busy rack) must not keep the application on the old ones forever.
const RECONNECT_QUIET_WAIT_MAX: Duration = Duration::from_secs(90);

/// Publishes under `PUBLISH_TIMEOUT`; the error text (or "timeout") on failure.
async fn publish_bounded(client: &AsyncClient, topic: &str, payload: String) -> Result<(), String> {
    let publish = client.publish(topic, QoS::AtLeastOnce, false, payload);
    match tokio::time::timeout(PUBLISH_TIMEOUT, publish).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("{:?}", e)),
        Err(_) => Err("timeout".to_string()),
    }
}

/// Waits (bounded by `max_wait`) until no card exchange happened for
/// `RECONNECT_QUIET_SECS`, so the reconnect does not cut a VU authentication
/// mid-flight. Returns how long it waited.
async fn wait_for_card_quiet(log_header: &str, max_wait: Duration) -> Duration {
    let started = std::time::Instant::now();
    let mut announced = false;
    loop {
        let quiet_for = crate::mqtt::seconds_since_card_activity();
        if quiet_for >= RECONNECT_QUIET_SECS {
            break;
        }
        let waited = started.elapsed();
        if waited >= max_wait {
            log::warn!(
                "{} [CONN] status=reconnect_wait_expired quiet_secs={} waited_secs={}",
                log_header,
                quiet_for,
                waited.as_secs()
            );
            break;
        }
        if !announced {
            announced = true;
            log::info!(
                "{} [CONN] status=reconnect_postponed reason=card_activity quiet_secs={}",
                log_header,
                quiet_for
            );
        }
        // Sleep until the link would have been quiet long enough, re-checked
        // at least every 5 s and never past the cap.
        let until_quiet = Duration::from_secs(RECONNECT_QUIET_SECS - quiet_for);
        let step = until_quiet
            .min(Duration::from_secs(5))
            .min(max_wait - waited)
            .max(Duration::from_millis(100));
        tokio::time::sleep(step).await;
    }
    started.elapsed()
}

/// Routes a JSON publish of the app connection to its command handler.
/// Returns false when the publish is not a command (wrong topic shape or an
/// unknown name) — the caller logs it as unsupported.
pub fn dispatch_request(
    client: &AsyncClient,
    log_header: &str,
    topic: &str,
    payload: &Value,
) -> bool {
    let Some(request_id) = crate::mqtt::request_id_from_topic(topic) else {
        return false;
    };
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        return false;
    };
    match name {
        "fetch_logs" => crate::logs_upload::handle(client, log_header, request_id, payload),
        "debug_log" => crate::debug_log::handle(client, log_header, request_id, payload),
        "set_credentials" => crate::credentials::handle(client, log_header, request_id, payload),
        "set_server" => crate::server_address::handle(client, log_header, request_id, payload),
        "get_settings" => handle_get_settings(client, log_header, request_id),
        _ => return false,
    }
    true
}

/// Tail shared by the commands that move the application to new connection
/// settings (`set_credentials`, `set_server`): publishes `reply` on the
/// connection the command arrived on, then — unless the reply is an error —
/// refreshes the settings dialog and rebuilds every connection. The reply goes
/// first: after the reconnect the server hears from a new session only (or,
/// for a new address, from nobody at all).
///
/// The reconnect drops every card session, so it waits (bounded) for the card
/// link to go quiet first: a set_server/set_credentials arriving during a VU
/// authentication must not cut it mid-way. The settings themselves are
/// already persisted by the caller; only the switch-over is deferred.
pub(crate) async fn reply_then_reconnect(
    client: &AsyncClient,
    log_header: &str,
    done_topic: &str,
    reply: Value,
    reason: &'static str,
) {
    if let Err(e) = publish_bounded(client, done_topic, reply.to_string()).await {
        // Not fatal: the settings are applied, and the server learns about
        // the outcome from the new session's settings report.
        log::error!(
            "{} [CONN] status=reply_failed topic={} err={}",
            log_header,
            done_topic,
            e
        );
    }
    if reply.get("error").is_some() {
        return;
    }
    if let Some(app) = crate::global_app_handle::get_app_handle() {
        if let Err(e) = crate::config::emit_global_config_server(&app) {
            log::warn!("{} [CONN] status=config_emit_failed err={}", log_header, e);
        }
    }
    wait_for_card_quiet(log_header, RECONNECT_QUIET_WAIT_MAX).await;
    crate::app_connect::reconnect_everything(reason).await;
}

/// Topic the settings report is published to.
const SETTINGS_TOPIC: &str = "settings";

/// Builds the settings report payload: an object keyed by setting name.
fn settings_report_payload() -> String {
    let auth = crate::mqtt::auth_state();
    serde_json::json!({
        "app_info": {
            "version": env!("CARGO_PKG_VERSION"),
            "os": sys_info::os_type().unwrap_or_else(|_| "Unknown".to_string()),
            "os_release": sys_info::os_release().unwrap_or_else(|_| "Unknown".to_string()),
            "arch": std::env::consts::ARCH,
        },
        "debug_log": crate::debug_log::status_json(),
        "server": {
            "host": crate::config::get_from_cache(crate::config::CacheSection::Server, "host"),
        },
        "authentication": {
            "enabled": auth.enabled,
            "username": auth.username,
            "saved": auth.saved,
        },
    })
    .to_string()
}

/// Publishes the settings report on the given (app) connection. Called once
/// per established connection (on CONNACK): the server tracks the values per
/// connection, so every reconnect gets a fresh report.
pub async fn publish_settings_report(client: &AsyncClient, log_header: &str) {
    let payload = settings_report_payload();
    match publish_bounded(client, SETTINGS_TOPIC, payload).await {
        Ok(()) => log::info!("{} [SETTINGS] status=report_published", log_header),
        Err(e) => log::error!("{} [SETTINGS] status=report_failed err={}", log_header, e),
    }
}

/// Handles a `get_settings` command: the settings report on demand, published
/// as the reply on `settings/<request_id>/done` (the server reads a setting it
/// has no value for through it).
fn handle_get_settings(client: &AsyncClient, log_header: &str, request_id: u64) {
    let topic = format!("settings/{}/done", request_id);
    let payload = settings_report_payload();
    let client = client.clone();
    let log_header = log_header.to_string();
    tauri::async_runtime::spawn(async move {
        match publish_bounded(&client, &topic, payload).await {
            Ok(()) => log::info!(
                "{} [SETTINGS] status=report_replied request_id={}",
                log_header,
                request_id
            ),
            Err(e) => log::error!(
                "{} [SETTINGS] status=reply_failed request_id={} err={}",
                log_header,
                request_id,
                e
            ),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn card_quiet_wait_is_bounded() {
        // A card exchange just happened: the wait runs into its cap and stops
        // there (the last step is clipped to the remaining time).
        let cap = Duration::from_millis(300);
        crate::mqtt::touch_card_activity();
        let waited = wait_for_card_quiet("test |", cap).await;
        assert!(waited >= cap, "returned early after {:?}", waited);
        assert!(waited < cap + Duration::from_secs(2), "waited {:?}", waited);
    }

    #[test]
    fn settings_report_payload_is_valid_and_complete() {
        let report: serde_json::Value =
            serde_json::from_str(&settings_report_payload()).expect("report must be valid JSON");
        let app_info = &report["app_info"];
        assert_eq!(app_info["version"], env!("CARGO_PKG_VERSION"));
        assert!(!app_info["os"].as_str().expect("os is a string").is_empty());
        assert!(!app_info["os_release"]
            .as_str()
            .expect("os_release is a string")
            .is_empty());
        assert!(!app_info["arch"]
            .as_str()
            .expect("arch is a string")
            .is_empty());
        let debug_log = &report["debug_log"];
        assert!(debug_log["enabled"].is_boolean());
        assert!(debug_log["until"].is_u64());
        assert!(report["server"]["host"].is_string());
        let authentication = &report["authentication"];
        assert!(authentication["enabled"].is_boolean());
        assert!(authentication["username"].is_string());
        assert!(authentication["saved"].is_boolean());
        assert!(authentication.get("password").is_none());
    }
}
