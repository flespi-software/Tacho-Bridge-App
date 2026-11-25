//! The application configuration: `~/Documents/tba/config.yaml`.
//!
//! The file holds the server address and MQTT credentials, the device ident
//! (`TBA` + 13 digits), the card list (card number → ICCID, name and the
//! metadata the backend learns), the theme and the update options. It is
//! user-editable, so reading it is lenient and a file that no longer parses is
//! backed up and salvaged rather than reset.
//!
//! Writes are atomic (temp file + rename) and every load-modify-save cycle
//! runs under one lock (`CONFIG_WRITE_LOCK`), so concurrent writers — the
//! frontend commands, the APDU sniffer, authentication results — never drop
//! each other's changes. Everything else reads the in-memory cache, which
//! each save refreshes while still holding the lock.
//!
//! The Tauri commands live here rather than in a submodule:
//! `tauri::generate_handler!` resolves the macro it generates next to each
//! command (`__cmd__<name>`) at the path lib.rs names, `config::<name>`, and a
//! plain `pub use` of the function does not carry that macro along.
//!
//! Layout:
//! - `types` — the file's structure, lenient deserializers, value validators
//! - `store` — the file path, load, atomic save, the write lock
//! - `init` — startup load, damaged-file backup and salvage, access warning
//! - `cache` — the in-memory copy and its readers
//! - `cards` — card entry writers and the card list replay to the frontend
//! - `settings` — server, credentials, theme and debug-window writers, and
//!   the `global-config-server` event

mod cache;
mod cards;
mod init;
mod settings;
mod store;
mod sync;
mod types;

// Re-exported for the rest of the app: these are the only entry points.
pub use cache::{
    debug_log_until, find_card_number_by_iccid, get_card_config_from_cache, get_from_cache,
    saved_credentials, CacheSection,
};
pub use cards::{
    emit_all_card_configs, mutate_card_config, record_auth_result_detached, CardMutation,
};
pub use init::{emit_access_warning, init_config, is_initialized, replay_access_warning};
pub use settings::{
    emit_global_config_server, set_debug_log_until, set_server_credentials, set_server_host,
};
pub use sync::{replace_synced_cards, set_sync_state, sync_state, synced_cards, SyncedCards};
pub use types::{split_host_to_parts, CardConfig, SyncState};

use cards::{persist_card, remove_card_from_config};
use settings::{update_appearance_config, update_server_config};
use store::get_config_path;

/// Public function to update the configuration with a new card.
/// This is a Tauri command: a thin async wrapper so the webview invoke never
/// runs file I/O on the main thread — the blocking core goes to the blocking pool.
/// Returns the reason on failure so the frontend can show it verbatim — a bare
/// "refused" tells the user nothing about a rejected duplicate ICCID link.
#[tauri::command]
pub async fn update_card(cardnumber: String, content: CardConfig) -> Result<(), String> {
    let updated = tauri::async_runtime::spawn_blocking(move || persist_card(&cardnumber, content))
        .await
        .unwrap_or_else(|e| {
            log::error!("update_card: blocking task failed: {:?}", e);
            Err("internal error while saving the card".to_string())
        });

    if updated.is_ok() {
        // A card just linked to an inserted physical card has no MQTT session yet
        // (its ICCID resolved to nothing when it was detected): wake the PCSC
        // monitor to re-register reader-backed cards, and retry the rack cards
        // the server reported before the ICCID was mapped to a number. Already
        // connected cards are untouched: both paths skip cards with a live session.
        crate::smart_card::request_rescan();
        crate::com_port::connect_pending_rack_cards().await;
        crate::cards_sync::on_local_change();
    }

    updated
}

/// Removes a card from the config (the Remove button of a card row) and ends
/// its sessions; the error text is shown to the user.
#[tauri::command]
pub async fn remove_card(cardnumber: String) -> Result<(), String> {
    let config_path = get_config_path().map_err(|e| {
        log::error!("Failed to get config path: {}", e);
        format!("Failed to get config path: {}", e)
    })?;

    remove_card_from_config(&config_path, &cardnumber)
        .await
        .map_err(|e| {
            log::error!("Failed to remove card from config: {}", e);
            format!("Failed to remove card from config: {}", e)
        })?;

    log::info!("Card {} removed from config", cardnumber);
    crate::cards_sync::on_local_change();

    Ok(())
}

/// Public function to update the server address in the configuration.
/// This is a Tauri command: a thin async wrapper so the webview invoke never
/// runs file I/O on the main thread — the blocking core goes to the blocking pool.
#[tauri::command]
pub async fn update_server(
    app: tauri::AppHandle,
    host: String,
    ident: String,
    theme: String,
    beta_updates: bool,
    auto_install_updates: bool,
) -> bool {
    let old_host = get_from_cache(CacheSection::Server, "host");
    let old_beta = get_from_cache(CacheSection::Updates, "beta_updates");
    let old_auto_install = get_from_cache(CacheSection::Updates, "auto_install_updates");

    let host_for_task = host.clone();
    let updated = tauri::async_runtime::spawn_blocking(move || {
        // The startup load failed and its background retry has not caught up
        // yet: the user saving the settings is the moment to try right away.
        // Without it a first run whose Documents was unavailable at launch has
        // no file for the read-modify-write below, and the save could never
        // succeed until the retry happened to run. The background retry still
        // runs its own recovery (debug window, card rescan) when it next ticks;
        // the dialog reconnects by itself after a successful save.
        if !is_initialized() {
            match init_config() {
                Ok(()) => log::info!("[CONFIG] status=recovered by=settings_save"),
                Err(e) => {
                    log::error!("Config is still not accessible: {}", e);
                    return false;
                }
            }
        }

        let config_path = match get_config_path() {
            Ok(path) => path,
            Err(e) => {
                log::error!("Failed to get config path: {}", e);
                return false;
            }
        };

        match update_server_config(
            &config_path,
            &host_for_task,
            &ident,
            &theme,
            beta_updates,
            auto_install_updates,
        ) {
            Ok(_) => {
                log::info!("The server address is updated to '{}'.", host_for_task);
                true
            }
            Err(e) => {
                log::error!("Failed to update server address: {}", e);
                false
            }
        }
    })
    .await
    .unwrap_or_else(|e| {
        log::error!("update_server: blocking task failed: {:?}", e);
        false
    });

    if updated {
        // Re-emit the server config: the frontend's cached host/ident (and the
        // header's "server configured" state) are fed by this event only —
        // without it they stay stale until an app restart.
        if let Err(e) = emit_global_config_server(&app) {
            log::error!("Failed to emit global-config-server after update: {}", e);
        }

        // The rack card session loops resolve the broker host once at start;
        // reader-backed cards migrate via manual_sync_cards, the racks need an
        // explicit restart of their sessions.
        if old_host != host {
            crate::com_port::restart_rack_links("server_host_changed");
        }

        // Channel switched → re-check against the newly selected endpoint.
        // Auto-install just enabled → check right away too, so the feature
        // acts within seconds instead of waiting for the next background tick.
        let auto_install_enabled =
            auto_install_updates && old_auto_install != auto_install_updates.to_string();
        if old_beta != beta_updates.to_string() || auto_install_enabled {
            let updater_app = app.clone();
            tauri::async_runtime::spawn(async move {
                crate::updater::check_for_updates(updater_app).await;
            });
        }
    }

    updated
}

/// Persists the theme chosen with the header button — the only theme control,
/// so it must not depend on the server dialog being saved.
#[tauri::command]
pub async fn update_theme(theme: String) -> bool {
    tauri::async_runtime::spawn_blocking(move || {
        let config_path = match get_config_path() {
            Ok(path) => path,
            Err(e) => {
                log::error!("Failed to get config path: {}", e);
                return false;
            }
        };

        match update_appearance_config(&config_path, &theme) {
            Ok(_) => true,
            Err(e) => {
                log::error!("Failed to update theme: {}", e);
                false
            }
        }
    })
    .await
    .unwrap_or_else(|e| {
        log::error!("update_theme: blocking task failed: {:?}", e);
        false
    })
}

/// Fixtures shared by the submodule tests: a scratch directory per test and a
/// config with one fully populated card.
#[cfg(test)]
mod test_support {
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::types::{AppearanceConfig, CardConfig, ConfigurationFile, DarkTheme, ServerConfig};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    pub(super) fn unique_tmp_dir(tag: &str) -> PathBuf {
        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut dir = std::env::temp_dir();
        dir.push(format!("tba-test-{}-{}-{}-{}", tag, pid, nanos, seq));
        fs::create_dir_all(&dir).expect("create tmp dir");
        dir
    }

    pub(super) fn sample_config() -> ConfigurationFile {
        let mut cards = HashMap::new();
        cards.insert(
            "ABCDEF0123456789".to_string(),
            CardConfig {
                iccid: "1122334455667788".to_string(),
                expire: Some(1_700_000_000),
                name: Some("My Company Card".to_string()),
                t_protocol: Some("T0".to_string()),
                card_type: Some(4),
                structure_version: Some((1, 0)),
                company_name: Some("Acme Logistics".to_string()),
                company_address: Some("Street 1".to_string()),
                last_auth: Some((1_700_000_500, true)),
            },
        );
        ConfigurationFile {
            name: "TBA".to_string(),
            version: "0.0.0-test".to_string(),
            description: "test".to_string(),
            appearance: Some(AppearanceConfig {
                dark_theme: DarkTheme::Auto,
            }),
            ident: Some("TBA0000000000001".to_string()),
            server: Some(ServerConfig {
                host: "mqtt.example.com:8883".to_string(),
                auth_enabled: None,
                username: None,
                password: None,
            }),
            cards,
            beta_updates: None,
            auto_install_updates: None,
            debug_log_until: None,
            sync: None,
        }
    }
}
