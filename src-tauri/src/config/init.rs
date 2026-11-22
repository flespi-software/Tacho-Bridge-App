//! Loading config.yaml at startup, and rescuing a damaged one.
//!
//! `init_config` creates a default file on a first run, stamps the app
//! version, backs up a file that no longer parses before salvaging what it
//! can, and fills the cache. Until it has succeeded the cache is empty and
//! nothing connects: `is_initialized` gates the retries and the access
//! warning shown to the user.

// ───── Std Lib ─────
use std::fs;
use std::io;

// ───── Local Modules ─────
use super::cache::load_config_to_cache;
use super::store::{config_write_guard, get_config_path, save_config};
use super::types::{
    cards_from_mapping, generate_default_config, generate_ident, ConfigurationFile, ServerConfig,
};

/// Rescues what it can from a config that failed to parse as a whole (a YAML
/// that is still a mapping but has a mistyped field somewhere): the device
/// ident, the server block and the cards. Everything else falls back to the
/// defaults. Above all the ident is kept — a new one makes the device a
/// different identity on the platform.
fn salvage_config(contents: &str) -> ConfigurationFile {
    let mut config = generate_default_config();
    let Ok(serde_yaml::Value::Mapping(root)) = serde_yaml::from_str::<serde_yaml::Value>(contents)
    else {
        return config;
    };
    if let Some(ident) = root.get("ident").and_then(|v| v.as_str()) {
        if !ident.is_empty() {
            config.ident = Some(ident.to_string());
        }
    }
    if let Some(server) = root.get("server") {
        match salvage_server(server) {
            Some(server) => config.server = Some(server),
            // No error text: a mistyped credential would be quoted in it.
            None => log::error!("config: server section not recoverable (no host)"),
        }
    }
    if let Some(serde_yaml::Value::Mapping(cards)) = root.get("cards").cloned() {
        config.cards = cards_from_mapping(cards);
    }
    for (key, slot) in [
        ("beta_updates", &mut config.beta_updates),
        ("auto_install_updates", &mut config.auto_install_updates),
    ] {
        *slot = root.get(key).and_then(|v| v.as_bool());
    }
    config
}

/// The server block of a damaged config, field by field: one mistyped field
/// (`auth_enabled: yes`) must not cost the host and the credentials. `None`
/// without a usable host.
fn salvage_server(server: &serde_yaml::Value) -> Option<ServerConfig> {
    // The text a scalar was meant to be (see `lenient_opt_string`).
    let text = |key: &str| match server.get(key)? {
        serde_yaml::Value::String(text) => Some(text.clone()),
        serde_yaml::Value::Number(number) if number.is_u64() => Some(number.to_string()),
        _ => None,
    };
    let host = text("host").filter(|host| !host.is_empty())?;
    Some(ServerConfig {
        host,
        auth_enabled: server.get("auth_enabled").and_then(|v| v.as_bool()),
        username: text("username"),
        password: text("password"),
    })
}

/// Set once `init_config` has succeeded. Until then the cache is empty (no
/// host, no ident, no cards): nothing connects, and the startup retry in
/// lib.rs (`spawn_config_init_retry`) keeps trying to load the file.
static CONFIG_INITIALIZED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Whether the config file has been loaded into the cache (see
/// `CONFIG_INITIALIZED`).
pub fn is_initialized() -> bool {
    CONFIG_INITIALIZED.load(std::sync::atomic::Ordering::SeqCst)
}

/// Tells the user the config directory is not accessible. Reuses the
/// frontend's `access` notification (the one the logger raises for an
/// unwritable `~/Documents/tba`), which shows its own fixed text.
pub fn emit_access_warning() {
    let path = get_config_path()
        .map(|p| format!("{:?}", p))
        .unwrap_or_else(|_| "~/Documents/tba/config.yaml".to_string());
    crate::global_app_handle::emit_notification_event(
        "global-notification",
        crate::global_app_handle::NotificationPayload {
            notification_type: "access".to_string(),
            message: format!("The configuration {} is not accessible yet; retrying", path),
        },
    );
}

/// Re-emits the config access warning to a freshly loaded webview while the
/// config is still not loaded. Skipped when the logger's own `access` warning
/// is being replayed already: the frontend shows the same text for both, and
/// two identical persistent toasts would only be noise.
pub fn replay_access_warning() {
    if !is_initialized() && !crate::logger::access_warning_active() {
        emit_access_warning();
    }
}

/// Initializes the configuration file.
/// This function creates a default configuration file if it does not exist, and loads it into the cache.
///
/// Safe to call repeatedly: besides startup it is retried in the background
/// after a failed startup (lib.rs `spawn_config_init_retry`) and by a settings
/// save while the config is still missing (`update_server`).
pub fn init_config() -> io::Result<()> {
    // Startup also participates in the write serialization: a card task or a
    // settings save may be persisting while the startup retry re-inits.
    let _guard = config_write_guard();

    let config_path = get_config_path()?;
    let config: ConfigurationFile;
    // Whether the file on disk still matches what we would write. A config that
    // parsed cleanly and needs no field change is left byte-for-byte alone:
    // rewriting it round-trips through serde_yaml, which drops comments and
    // reorders keys in a file the user may hand-edit. Only a real change
    // earns a write.
    let mut needs_save = true;
    // Whether a failed save may be ignored: true when only the version stamp changed.
    let mut save_optional = false;

    // Only a definite "not found" means a first run. `Path::exists()` is also
    // false on any metadata error (permissions, a cloud-sync placeholder not
    // hydrated yet at login), and treating that as "missing" would write a
    // default config over the real one with no backup.
    let exists = match fs::metadata(&config_path) {
        Ok(_) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => {
            log::error!("Config file {:?} is not accessible: {}", config_path, e);
            return Err(e);
        }
    };

    if exists {
        // A file saved as another encoding by a hand edit goes down the corrupt
        // path too: backed up, then salvaged from its lossy decoding.
        let (contents, parsed) = match String::from_utf8(fs::read(&config_path)?) {
            Ok(text) => {
                let parsed =
                    serde_yaml::from_str::<ConfigurationFile>(&text).map_err(|e| e.to_string());
                (text, parsed)
            }
            Err(e) => (
                String::from_utf8_lossy(e.as_bytes()).into_owned(),
                Err(format!("not valid UTF-8: {e}")),
            ),
        };

        match parsed {
            Ok(mut loaded_config) => {
                let version = env!("CARGO_PKG_VERSION");
                needs_save = loaded_config.version != version;
                save_optional = needs_save;
                loaded_config.version = version.to_string();
                // A hand-prepared config may carry only `server` and `cards`;
                // without an ident the app never connects.
                if loaded_config.ident.as_deref().is_none_or(str::is_empty) {
                    loaded_config.ident = Some(generate_ident());
                    needs_save = true;
                    save_optional = false;
                }
                config = loaded_config;
            }
            Err(e) => {
                // The reset below persists a default config over this file —
                // rescue the original first. Whatever made it unparseable
                // (truncation, a cloud-sync half-write, a downgrade), it still
                // holds the user's server host and card list, and overwriting
                // it silently would destroy them beyond recovery.
                let backup = config_path.with_extension(format!(
                    "yaml.corrupt.{}",
                    chrono::Local::now().format("%Y%m%d_%H%M%S")
                ));
                // A copy keeps the original in place until the atomic save
                // below replaces it: a crash or a full disk in between must
                // not leave the app with no config (it would start over with
                // a new ident). The rename is a fallback for a full disk.
                // One copy per run: a save that keeps failing brings this path
                // back on every init retry, and the file has not changed since.
                static BACKED_UP: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                let backed_up = if BACKED_UP.load(std::sync::atomic::Ordering::SeqCst) {
                    Ok(())
                } else {
                    fs::copy(&config_path, &backup)
                        .map(|_| ())
                        .or_else(|_| fs::rename(&config_path, &backup))
                };
                BACKED_UP.store(backed_up.is_ok(), std::sync::atomic::Ordering::SeqCst);
                if let Err(backup_err) = backed_up {
                    // Never overwrite a file that has no copy: leave it for
                    // the user to repair and run without a config.
                    log::error!(
                        "Config parse failed ({}) and backup failed ({}). Leaving {:?} untouched.",
                        e,
                        backup_err,
                        config_path
                    );
                    return Err(io::Error::other(format!("config parse failed: {}", e)));
                }
                log::error!(
                    "Config parse failed ({}). Original saved as {:?}, recovering what parses.",
                    e,
                    backup
                );
                config = salvage_config(&contents);
            }
        }
    } else {
        log::debug!("Config file not found. Generating default config.");
        config = generate_default_config();
    }

    if needs_save {
        match save_config(&config_path, &config) {
            Ok(()) => log::debug!("config: saved config"),
            // Only the version stamp is new: a file that cannot be rewritten is still a valid config.
            Err(e) if save_optional => {
                log::warn!(
                    "config: version stamp not saved ({}), using the file as it is",
                    e
                )
            }
            Err(e) => return Err(io::Error::other(e)),
        }
    } else {
        log::debug!("config: unchanged on disk, left as-is");
    }

    load_config_to_cache(&config).map_err(io::Error::other)?;
    CONFIG_INITIALIZED.store(true, std::sync::atomic::Ordering::SeqCst);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salvage_keeps_the_server_block_around_a_mistyped_field() {
        let config = salvage_config(
            "ident: TBA0000000000001\nserver:\n  host: mqtt.flespi.io:8883\n  auth_enabled: yes\n  username: token\n  password: 123456\ncards: 5\n",
        );
        let server = config.server.unwrap();
        assert_eq!(server.host, "mqtt.flespi.io:8883");
        assert_eq!(server.auth_enabled, None);
        assert_eq!(server.username.as_deref(), Some("token"));
        assert_eq!(server.password.as_deref(), Some("123456"));
        assert_eq!(config.ident.as_deref(), Some("TBA0000000000001"));
        assert!(salvage_config("server:\n  auth_enabled: true\n")
            .server
            .is_none());
    }

    #[test]
    fn salvage_keeps_ident_server_and_cards() {
        let yaml = r#"
ident: TBA1234567890123
debug_log_until: "soon"
server:
  host: mqtt.flespi.io
cards:
  GOODCARD00000001:
    iccid: "1111"
"#;
        assert!(serde_yaml::from_str::<ConfigurationFile>(yaml).is_err());
        let cfg = salvage_config(yaml);
        assert_eq!(cfg.ident.as_deref(), Some("TBA1234567890123"));
        assert_eq!(
            cfg.server.map(|s| s.host).as_deref(),
            Some("mqtt.flespi.io")
        );
        assert!(cfg.cards.contains_key("GOODCARD00000001"));
        assert!(cfg.debug_log_until.is_none());
    }

    #[test]
    fn salvage_of_garbage_is_the_default() {
        let cfg = salvage_config(":::\n\t{[");
        assert!(cfg.cards.is_empty());
        assert!(cfg.ident.is_some());
    }
}
