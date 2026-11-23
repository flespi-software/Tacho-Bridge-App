//! Writers of the non-card settings, and their report to the frontend.
//!
//! The settings dialog, the theme button, the MQTT credentials, the
//! server-pushed address (`set_server`) and the extended-debug window each
//! persist their own fields under the config write lock, leaving everything
//! else in the file as it was. `emit_global_config_server` is the one event
//! the frontend learns all of them from.

// ───── Std Lib ─────
use std::collections::HashMap;
use std::error::Error;
use std::path::Path;

// ───── External Crates ─────
use tauri::Emitter;

// ───── Local Modules ─────
use super::cache::{get_from_cache, load_config_to_cache, CacheSection};
use super::store::{config_write_guard, get_config_path, load_config, save_config};
use super::types::{
    dark_theme_from_label, is_valid_ident, split_host_to_parts, AppearanceConfig, ServerConfig,
};

/// Persists only the appearance section; host/ident are untouched.
pub(super) fn update_appearance_config(
    config_path: &Path,
    theme: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Serialize the whole read-modify-write against all other config writers.
    let _guard = config_write_guard();

    let mut config = load_config(config_path)?;
    config.appearance = Some(AppearanceConfig {
        dark_theme: dark_theme_from_label(theme),
    });
    save_config(config_path, &config)?;
    load_config_to_cache(&config)?;

    Ok(())
}

/// Saves the settings dialog: server address, app ident, theme and update
/// options, under the config write lock.
pub(super) fn update_server_config(
    config_path: &Path,
    host: &str,
    ident: &str,
    theme: &str,
    beta_updates: bool,
    auto_install_updates: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // The dialog validates too; this guards the device identity and the
    // broker address against any other caller.
    split_host_to_parts(host)?;
    if !is_valid_ident(ident) {
        return Err(format!("invalid app ident '{}'", ident).into());
    }

    // Serialize the whole read-modify-write against all other config writers.
    let _guard = config_write_guard();

    let mut config = load_config(config_path)?;

    // The server settings carry no MQTT credentials (they are saved on their
    // own, see `credentials.rs`) — carry the stored values over so saving the
    // dialog does not silently strip authentication from the config.
    let previous = config.server.take();
    config.server = Some(ServerConfig {
        host: host.to_string(),
        auth_enabled: previous.as_ref().and_then(|s| s.auth_enabled),
        username: previous.as_ref().and_then(|s| s.username.clone()),
        password: previous.and_then(|s| s.password),
    });
    config.ident = Some(ident.to_string());
    config.appearance = Some(AppearanceConfig {
        dark_theme: dark_theme_from_label(theme),
    });
    config.beta_updates = Some(beta_updates);
    config.auto_install_updates = Some(auto_install_updates);

    save_config(config_path, &config)?;
    load_config_to_cache(&config)?;

    Ok(())
}

/// Persists the extended-debug window (see `debug_log_until`). Blocking disk
/// I/O under the config write lock — call from a blocking context.
pub fn set_debug_log_until(until: Option<u64>) -> Result<(), String> {
    let _guard = config_write_guard();
    let config_path = get_config_path().map_err(|e| e.to_string())?;
    let mut config = load_config(&config_path).map_err(|e| e.to_string())?;
    if config.debug_log_until == until {
        // nothing to write: keep the hand-edited file byte-for-byte
        return Ok(());
    }
    config.debug_log_until = until;
    save_config(&config_path, &config).map_err(|e| e.to_string())?;
    load_config_to_cache(&config).map_err(|e| e.to_string())?;
    Ok(())
}

/// Persists the authentication switch and, when `save` is set, the
/// credentials; with `save` unset the saved credentials are removed from the
/// file (the caller keeps the pair in memory for this run only). `None`
/// values leave the stored field as it is. Blocking disk I/O under the config
/// write lock — call from a blocking context.
pub fn set_server_credentials(
    enabled: bool,
    username: Option<&str>,
    password: Option<&str>,
    save: bool,
) -> Result<(), String> {
    let _guard = config_write_guard();
    let config_path = get_config_path().map_err(|e| e.to_string())?;
    let mut config = load_config(&config_path).map_err(|e| e.to_string())?;
    let server = config
        .server
        .as_mut()
        .ok_or_else(|| "server address is not configured".to_string())?;
    server.auth_enabled = Some(enabled);
    if save {
        if let Some(username) = username {
            server.username = Some(username.to_string());
        }
        if let Some(password) = password {
            server.password = Some(password.to_string());
        }
    } else {
        server.username = None;
        server.password = None;
    }
    save_config(&config_path, &config).map_err(|e| e.to_string())?;
    load_config_to_cache(&config).map_err(|e| e.to_string())?;
    Ok(())
}

/// Persists a new server address (`host:port`, validated by the caller with
/// `split_host_to_parts`). Blocking disk I/O under the config write lock -
/// call from a blocking context. Reconnecting is the caller's job.
pub fn set_server_host(host: &str) -> Result<(), String> {
    let _guard = config_write_guard();
    let config_path = get_config_path().map_err(|e| e.to_string())?;
    let mut config = load_config(&config_path).map_err(|e| e.to_string())?;
    let server = config
        .server
        .as_mut()
        .ok_or_else(|| "server address is not configured".to_string())?;
    server.host = host.to_string();
    save_config(&config_path, &config).map_err(|e| e.to_string())?;
    load_config_to_cache(&config).map_err(|e| e.to_string())?;
    Ok(())
}

/// Emits the server-related part of the config (host, ident, theme, update
/// channel) to the frontend as the `global-config-server` event.
pub fn emit_global_config_server(app: &tauri::AppHandle) -> Result<(), Box<dyn Error>> {
    let host = get_from_cache(CacheSection::Server, "host");
    let ident = get_from_cache(CacheSection::Ident, "ident");
    let appearance = get_from_cache(CacheSection::Appearance, "dark_theme");

    let mut config_app_payload = HashMap::new();
    config_app_payload.insert("host", host);
    config_app_payload.insert("ident", ident);
    config_app_payload.insert("dark_theme", appearance);
    config_app_payload.insert(
        "beta_updates",
        get_from_cache(CacheSection::Updates, "beta_updates"),
    );
    config_app_payload.insert(
        "auto_install_updates",
        get_from_cache(CacheSection::Updates, "auto_install_updates"),
    );
    // Authentication state for the settings dialog and the startup prompt,
    // the password included: the dialog shows the pair in effect.
    let auth = crate::mqtt::auth_state();
    config_app_payload.insert("auth_enabled", auth.enabled.to_string());
    config_app_payload.insert("auth_username", auth.username);
    config_app_payload.insert("auth_password", auth.password);
    config_app_payload.insert("auth_saved", auth.saved.to_string());
    config_app_payload.insert("auth_active", auth.active.to_string());
    // Extended debug log state for the settings dialog.
    let debug = crate::debug_log::status_json();
    config_app_payload.insert(
        "debug_log_enabled",
        debug["enabled"].as_bool().unwrap_or(false).to_string(),
    );
    config_app_payload.insert(
        "debug_log_until",
        debug["until"].as_u64().unwrap_or(0).to_string(),
    );

    if let Err(e) = app.emit("global-config-server", config_app_payload) {
        return Err(Box::new(e));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::{sample_config, unique_tmp_dir};
    use std::fs;

    #[test]
    fn update_server_config_preserves_hand_edited_credentials() {
        // The settings dialog has no credentials UI: saving it goes through
        // update_server_config, which must carry the hand-edited values over
        // instead of stripping authentication from the config.
        let dir = unique_tmp_dir("credentials_preserved");
        let path = dir.join("config.yaml");

        let mut cfg = sample_config();
        if let Some(server) = cfg.server.as_mut() {
            server.username = Some("FlespiToken".to_string());
            server.password = Some("secret".to_string());
        }
        save_config(&path, &cfg).expect("save");

        update_server_config(
            &path,
            "new.example.com:8883",
            "TBA0000000000001",
            "Auto",
            true,
            false,
        )
        .expect("update server config");

        let loaded = load_config(&path).expect("load");
        let server = loaded.server.expect("server section");
        assert_eq!(server.host, "new.example.com:8883");
        assert_eq!(server.username.as_deref(), Some("FlespiToken"));
        assert_eq!(server.password.as_deref(), Some("secret"));

        let _ = fs::remove_dir_all(&dir);
    }
}
