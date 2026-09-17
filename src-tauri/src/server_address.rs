//! Server address change: the `set_server` server command.
//!
//! The server publishes `{"name":"set_server","host":"host:port"}` to
//! `request/<request_id>/0` on the app connection. The reply goes to
//! `server/<request_id>/done`: `{"name":"set_server","host":"host:port"}`
//! or `{"error":...}`. The reply is published on the connection the command
//! arrived on, BEFORE every connection is rebuilt against the new address -
//! afterwards the old server hears nothing from this application any more.
//! No rollback, as with `set_credentials`: what the server pushes is what the
//! application uses; the settings dialog is where a wrong address gets fixed.

use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::AsyncClient;
use serde_json::{json, Value};
use tauri::async_runtime;

/// Handles a `set_server` command (routed here by `commands_settings`).
pub fn handle(client: &AsyncClient, log_header: &str, request_id: u64, payload: &Value) {
    let done_topic = format!("server/{}/done", request_id);

    let parsed = match payload.get("host") {
        Some(Value::String(host)) => {
            crate::config::split_host_to_parts(host).map(|_| host.clone())
        }
        Some(_) => Err("host must be a string".to_string()),
        None => Err("host is required".to_string()),
    };

    log::info!(
        "{} [CONN] status=set_server_command request_id={} ok={}",
        log_header,
        request_id,
        parsed.is_ok()
    );
    let client = client.clone();
    let log_header = log_header.to_string();
    async_runtime::spawn(async move {
        let reply = match parsed {
            Err(e) => json!({"error": e}),
            Ok(host) => {
                let host_for_task = host.clone();
                let applied = async_runtime::spawn_blocking(move || {
                    crate::config::set_server_host(&host_for_task)
                })
                .await
                .unwrap_or_else(|e| Err(format!("server address task failed: {}", e)));
                match applied {
                    Err(e) => json!({"error": e}),
                    Ok(()) => {
                        log::info!(
                            "{} [CONN] status=server_address_applied host={}",
                            log_header,
                            host
                        );
                        json!({"name": "set_server", "host": host})
                    }
                }
            }
        };
        let is_error = reply.get("error").is_some();
        // The reply must reach the server on THIS connection: after the
        // reconnect below the application talks to the new address only.
        if let Err(e) = client
            .publish(&done_topic, QoS::AtLeastOnce, false, reply.to_string())
            .await
        {
            log::error!(
                "{} [CONN] status=set_server_reply_failed err={:?}",
                log_header,
                e
            );
        }
        if is_error {
            return;
        }
        if let Some(app) = crate::global_app_handle::get_app_handle() {
            if let Err(e) = crate::config::emit_global_config_server(&app) {
                log::warn!("{} [CONN] status=config_emit_failed err={}", log_header, e);
            }
        }
        crate::app_connect::reconnect_everything("set_server_command").await;
    });
}
