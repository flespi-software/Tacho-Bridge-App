//! MQTT authentication of the broker connections: the switch and the
//! credentials, set from the settings dialog, from the startup prompt, or by
//! the server's `set_credentials` command.
//!
//! Two ways to hold a pair (see `mqtt.rs`): saved in config.yaml (`server.
//! username` / `password`, used on every launch) or for this run only, in
//! memory — "Save to config" unchecked. The switch itself (`server.
//! auth_enabled`) is always persisted; with it on and no pair anywhere the
//! webview shows the startup prompt.
//!
//! Server command, on `request/<request_id>/0` of the app connection:
//! `{"name":"set_credentials","enable":true,"username":"...","password":"...","save":true}`.
//! Reply on `credentials/<request_id>/done`:
//! `{"name":"set_credentials","enabled":bool,"saved":bool}` or `{"error":...}`.
//! The reply goes out on the connection the command came in on, BEFORE every
//! connection is rebuilt with the new pair; there is no rollback — what the
//! server pushes is what the device uses from then on.

use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::AsyncClient;
use serde_json::{json, Value};
use tauri::async_runtime;

/// Applies the switch and the credentials. `username`/`password` `None` keep
/// what is in effect (config or session); `save` decides between the config
/// file and memory. Returns the state the webview should show.
fn apply(
    enabled: bool,
    username: Option<&str>,
    password: Option<&str>,
    save: bool,
) -> Result<(), String> {
    if enabled && !save {
        // For this run only: resolve the pair now (a field left untouched in
        // the dialog keeps its current value) and keep it in memory. The
        // saved copy, if any, is removed from the file below.
        let current = crate::mqtt::resolved_credentials();
        let username = username
            .map(str::to_string)
            .or_else(|| current.as_ref().map(|(u, _)| u.clone()))
            .unwrap_or_default();
        let password = password
            .map(str::to_string)
            .or_else(|| current.map(|(_, p)| p))
            .unwrap_or_default();
        if username.is_empty() {
            return Err("username is empty".to_string());
        }
        crate::config::set_server_credentials(true, None, None, false)?;
        crate::mqtt::set_session_credentials(Some((username, password)));
    } else {
        // Saved, or the switch turned off: the file is the single source, the
        // in-memory pair (if any) is retired.
        if enabled && save {
            // a pair entered for this run and now saved: carry it over when
            // the dialog did not re-enter it
            let session = crate::mqtt::resolved_credentials();
            let username = username
                .map(str::to_string)
                .or_else(|| session.as_ref().map(|(u, _)| u.clone()));
            let password = password
                .map(str::to_string)
                .or_else(|| session.map(|(_, p)| p));
            crate::config::set_server_credentials(
                true,
                username.as_deref(),
                password.as_deref(),
                true,
            )?;
        } else {
            crate::config::set_server_credentials(false, username, password, true)?;
        }
        crate::mqtt::set_session_credentials(None);
    }
    let state = crate::mqtt::auth_state();
    log::info!(
        "[CONN] status=credentials_applied auth_enabled={} saved={} active={}",
        state.enabled,
        state.saved,
        state.active
    );
    Ok(())
}

/// Tauri command behind the settings dialog and the startup prompt. With
/// `reconnect` every MQTT connection is rebuilt here; the settings dialog
/// passes `false` and runs its own reconnect sequence after the rest of the
/// settings are saved.
#[tauri::command]
pub async fn apply_credentials(
    app: tauri::AppHandle,
    enabled: bool,
    username: Option<String>,
    password: Option<String>,
    save: bool,
    reconnect: bool,
) -> Result<(), String> {
    async_runtime::spawn_blocking(move || {
        apply(enabled, username.as_deref(), password.as_deref(), save)
    })
    .await
    .map_err(|e| format!("credentials task failed: {}", e))??;
    if let Err(e) = crate::config::emit_global_config_server(&app) {
        log::warn!("[CONN] status=config_emit_failed err={}", e);
    }
    if reconnect {
        crate::app_connect::reconnect_everything("credentials_changed").await;
    }
    Ok(())
}

/// Handles a `set_credentials` command (routed here by `commands_settings`).
pub fn handle(client: &AsyncClient, log_header: &str, request_id: u64, payload: &Value) {
    let done_topic = format!("credentials/{}/done", request_id);

    let bool_field = |key: &str, default: bool| -> Result<bool, String> {
        match payload.get(key) {
            None => Ok(default),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(format!("{} must be a boolean", key)),
        }
    };
    let str_field = |key: &str| -> Result<Option<String>, String> {
        match payload.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(format!("{} must be a string", key)),
        }
    };
    let parsed = (|| -> Result<(bool, Option<String>, Option<String>, bool), String> {
        let enable = bool_field("enable", true)?;
        let save = bool_field("save", true)?;
        let username = str_field("username")?;
        let password = str_field("password")?;
        if enable
            && username.as_deref().is_none_or(str::is_empty)
            && crate::mqtt::resolved_credentials().is_none()
        {
            return Err("username is required to enable authentication".to_string());
        }
        Ok((enable, username, password, save))
    })();

    log::info!(
        "{} [CONN] status=set_credentials_command request_id={} ok={}",
        log_header,
        request_id,
        parsed.is_ok()
    );
    let client = client.clone();
    let log_header = log_header.to_string();
    async_runtime::spawn(async move {
        let reply = match parsed {
            Err(e) => json!({"error": e}),
            Ok((enable, username, password, save)) => {
                let applied = async_runtime::spawn_blocking(move || {
                    apply(enable, username.as_deref(), password.as_deref(), save)
                })
                .await
                .unwrap_or_else(|e| Err(format!("credentials task failed: {}", e)));
                match applied {
                    Err(e) => json!({"error": e}),
                    Ok(()) => {
                        let state = crate::mqtt::auth_state();
                        json!({"name": "set_credentials", "enabled": state.enabled, "saved": state.saved})
                    }
                }
            }
        };
        let is_error = reply.get("error").is_some();
        // The reply must reach the server on THIS connection: after the
        // reconnect below the server hears from a new session only.
        if let Err(e) = client
            .publish(&done_topic, QoS::AtLeastOnce, false, reply.to_string())
            .await
        {
            log::error!(
                "{} [CONN] status=set_credentials_reply_failed err={:?}",
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
        crate::app_connect::reconnect_everything("set_credentials_command").await;
    });
}
