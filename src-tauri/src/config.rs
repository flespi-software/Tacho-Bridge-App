// ───── Std Lib ─────
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fs;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

// ───── External Crates ─────
use serde::{Deserialize, Serialize};
use serde_yaml;
use lazy_static::lazy_static;
use tauri::Emitter;

// ───── Local Modules ─────
use crate::global_app_handle::emit_card_config_event;
use crate::mqtt::remove_connections;
// use crate::smart_card::manual_sync_cards;

/// Represents the configuration settings for the application.
#[derive(Serialize, Deserialize, Debug)]
pub struct ConfigurationFile {
    name: String,                           // The name of the application.
    version: String,                        // The version of the application.
    description: String,                    // A brief description of the application.
    appearance: Option<AppearanceConfig>,   // Optional UI configuration settings.
    ident: Option<String>,                  // Optional ident for the application.
    server: Option<ServerConfig>,           // Optional server configuration settings.
    cards: HashMap<String, CardConfig>,     // Hashmap of the cards with the CardConfig structure
}

// Server Configuration structure, part of ConfigurationFile that contains data about the server.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ServerConfig {
    pub host: String,
}

// Dark Theme enum, part of AppearanceConfig that contains data about the theme.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum DarkTheme {
    Auto,
    Dark,
    Light,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CardConfig {
    pub iccid: String,                          // ICCID
    pub expire: Option<u64>,                    // Expire date
    pub name: Option<String>,                   // Custom card name (for ease of user identification)
    pub card_type: Option<u8>,                  // typeOfTachographCardId: 1=Driver, 2=Workshop, 3=Control, 4=Company
    pub structure_version: Option<(u8, u8)>,    // cardStructureVersion (major, minor): major 0x00=Gen1, 0x01=Gen2; minor = data-element revision
    pub company_name: Option<String>,           // Company name (from EF_Identification, Company Card only)
    pub company_address: Option<String>,        // Company address (from EF_Identification, Company Card only)
    pub last_auth: Option<(u64, bool)>,         // Last completed authentication: (unix_timestamp, success_flag)
}
// UI Configuration structure, part of ConfigurationFile that contains data about how UI looks like.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppearanceConfig {
    pub dark_theme: DarkTheme,
}

/// Retrieves the configuration file path.
/// This function constructs the path to the configuration file, creating the necessary directories if they do not exist.
pub fn get_config_path() -> io::Result<PathBuf> {
    let mut config_path = PathBuf::new();

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let home_dir = env::var("HOME");

    #[cfg(target_os = "windows")]
    let home_dir = env::var("USERPROFILE");

    match &home_dir {
        Ok(home) => {
            log::debug!("Home directory found: {}", home);
            config_path.push(home);
        }
        Err(e) => {
            log::error!("Failed to get home directory environment variable: {}", e);
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Failed to get home directory environment variable",
            ));
        }
    }

    config_path.push("Documents");
    config_path.push("tba");

    log::debug!("Config directory path resolved to: {:?}", config_path);

    if let Err(e) = fs::create_dir_all(&config_path) {
        log::error!("Failed to create config directory {:?}: {}", config_path, e);
        return Err(e);
    }

    config_path.push("config.yaml");

    log::debug!("Final config file path: {:?}", config_path);

    Ok(config_path)
}

/// Load the configuration from the file.
/// This function reads the configuration file and parses it.
fn load_config(
    config_path: &Path,
) -> Result<ConfigurationFile, Box<dyn std::error::Error + Send + Sync>> {
    let mut config_contents = String::new();
    File::open(config_path)?.read_to_string(&mut config_contents)?;
    let config: ConfigurationFile = serde_yaml::from_str(&config_contents)?;
    Ok(config)
}

/// Saves the configuration to the file atomically.
/// Writes to a sibling temp file in the same directory, then renames it over the target.
/// This prevents config corruption (empty/partial file) if the process is killed mid-write.
fn save_config(
    config_path: &Path,
    config: &ConfigurationFile,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let yaml = serde_yaml::to_string(config)?;

    let parent = config_path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "config path has no parent directory")
    })?;
    let file_name = config_path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "config path has no file name")
    })?;

    let mut tmp_path = parent.to_path_buf();
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(".tmp");
    tmp_path.push(tmp_name);

    {
        let mut tmp_file = File::create(&tmp_path)?;
        tmp_file.write_all(yaml.as_bytes())?;
        tmp_file.sync_all()?;
    }

    if let Err(e) = fs::rename(&tmp_path, config_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(Box::new(e));
    }

    Ok(())
}

/// Updates the configuration with a new card.
/// This function updates the configuration file with a new card's ATR and card number.
fn update_card_config(
    config_path: &Path,
    card_number: &str,
    content: CardConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Serialize the whole read-modify-write against all other config writers.
    let _guard = config_write_guard();

    let mut config = load_config(config_path)?;
    log::debug!("Loaded configuration: {:?}", config);

    // let mut needs_restart = false;
    let mut changed = false;

    match config.cards.get_mut(card_number) {
        Some(existing_card) => {
            if existing_card.iccid.is_empty() {
                // ICCID is being set for the first time
                log::debug!(
                    "Existing card with empty ICCID. Updating: iccid = {}, name = {:?}, expire = {:?}",
                    content.iccid,
                    content.name,
                    content.expire
                );
                existing_card.iccid = content.iccid;
                existing_card.expire = content.expire;
                existing_card.name = content.name;
                existing_card.card_type = content.card_type;
                existing_card.structure_version = content.structure_version;
                existing_card.company_name = content.company_name;
                existing_card.company_address = content.company_address;
                existing_card.last_auth = content.last_auth;
                // needs_restart = true;
                changed = true;
            } else {
                // Update optional fields only (no restart required)
                if existing_card.expire != content.expire
                    || existing_card.name != content.name
                    || existing_card.card_type != content.card_type
                    || existing_card.structure_version != content.structure_version
                    || existing_card.company_name != content.company_name
                    || existing_card.company_address != content.company_address
                    || existing_card.last_auth != content.last_auth
                {
                    log::debug!(
                        "Updating optional fields for card {}: name = {:?}, expire = {:?}, card_type = {:?}, structure_version = {:?}, company_name = {:?}, company_address = {:?}, last_auth = {:?}",
                        card_number,
                        content.name,
                        content.expire,
                        content.card_type,
                        content.structure_version,
                        content.company_name,
                        content.company_address,
                        content.last_auth
                    );
                    existing_card.expire = content.expire;
                    existing_card.name = content.name;
                    existing_card.card_type = content.card_type;
                    existing_card.structure_version = content.structure_version;
                    existing_card.company_name = content.company_name;
                    existing_card.company_address = content.company_address;
                    existing_card.last_auth = content.last_auth;
                    changed = true;
                }
            }
        }
        None => {
            // Add new card entirely
            log::debug!(
                "Adding new card: card_number = {}, iccid = {}, name = {:?}, expire = {:?}",
                card_number,
                content.iccid,
                content.name,
                content.expire
            );
            config.cards.insert(card_number.to_string(), content);
            // needs_restart = true;
            changed = true;
        }
    }

    if changed {
        // Save config to file
        save_config(config_path, &config)?;
        log::debug!("Configuration saved successfully");

        // Load into runtime cache
        load_config_to_cache(&config)?;
        log::debug!("Configuration loaded to cache successfully");

        // Emit frontend update event
        if let Some(card_config) = config.cards.get(card_number) {
            emit_card_config_event(
                "global-card-config-updated",
                card_number.to_string(),
                Some(card_config.clone())
            );
        }

        // // Restart connection if necessary
        // if needs_restart {
        //     log::info!("Restarting connection for card: {}", card_number);
        //     manual_sync_cards(card_number.to_string(), true).await;
        // }
    }

    Ok(())
}

/// Synchronous core of the full-content card update (file I/O + fsync under
/// the global config lock). Blocking by design — call it from a blocking
/// thread, never from an async task or the main thread.
pub fn persist_card(card_number: &str, content: CardConfig) -> bool {
    let config_path = match get_config_path() {
        Ok(path) => path,
        Err(e) => {
            log::error!("Failed to get config path: {}", e);
            return false;
        }
    };

    match update_card_config(&config_path, card_number, content) {
        Ok(_) => {
            log::info!("The card, {} is added to the configuration!", card_number);
            true
        }
        Err(e) => {
            log::error!("Failed to update config: {}", e);
            false
        }
    }
}

/// Applies `mutate` to one card entry under the global config write lock:
/// fresh load from disk → mutate → save → refresh cache → emit the frontend
/// event. The closure returns whether it actually changed anything; when it
/// returns false the save is skipped entirely.
///
/// This is the safe primitive for "change one field" writers (sniffer, auth
/// results): mutating fresh file state under the lock means concurrent writers
/// cannot revert each other's fields, which a cache-read + full-write would do.
/// Blocking (file I/O + fsync) — call from a blocking thread.
pub fn mutate_card_config<F>(card_number: &str, mutate: F) -> bool
where
    F: FnOnce(&mut CardConfig) -> bool,
{
    let config_path = match get_config_path() {
        Ok(path) => path,
        Err(e) => {
            log::error!("Failed to get config path: {}", e);
            return false;
        }
    };

    let _guard = config_write_guard();

    let mut config = match load_config(&config_path) {
        Ok(config) => config,
        Err(e) => {
            log::error!("mutate_card_config: failed to load config: {}", e);
            return false;
        }
    };

    let Some(card) = config.cards.get_mut(card_number) else {
        log::warn!("mutate_card_config: unknown card_number {}", card_number);
        return false;
    };

    if !mutate(card) {
        // Nothing changed against the authoritative file state — no write.
        return true;
    }
    let updated = card.clone();

    if let Err(e) = save_config(&config_path, &config) {
        log::error!("mutate_card_config: failed to save config: {}", e);
        return false;
    }
    if let Err(e) = load_config_to_cache(&config) {
        log::error!("mutate_card_config: failed to refresh cache: {}", e);
        return false;
    }

    emit_card_config_event(
        "global-card-config-updated",
        card_number.to_string(),
        Some(updated),
    );

    true
}

/// Public function to update the configuration with a new card.
/// This is a Tauri command: a thin async wrapper so the webview invoke never
/// runs file I/O on the main thread — the blocking core goes to the blocking pool.
#[tauri::command]
pub async fn update_card(cardnumber: String, content: CardConfig) -> bool {
    tauri::async_runtime::spawn_blocking(move || persist_card(&cardnumber, content))
        .await
        .unwrap_or_else(|e| {
            log::error!("update_card: blocking task failed: {:?}", e);
            false
        })
}

/// Updates the server address in the configuration.
/// This function updates the configuration file with a new server address.
pub fn update_server_config(
    config_path: &Path,
    host: &str,
    ident: &str,
    theme: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Serialize the whole read-modify-write against all other config writers.
    let _guard = config_write_guard();

    let mut config = load_config(config_path)?;

    config.server = Some(ServerConfig {
        host: host.to_string(),
    });
    config.ident = Some(ident.to_string());
    config.appearance = Some(AppearanceConfig {
        dark_theme: match theme {
            "Auto" => DarkTheme::Auto,
            "Dark" => DarkTheme::Dark,
            "Light" => DarkTheme::Light,
            _ => DarkTheme::Auto,
        },
    });

    save_config(config_path, &config)?;
    load_config_to_cache(&config)?;

    Ok(())
}

#[tauri::command]
pub async fn remove_card(
    cardnumber: String,
) -> Result<(), String> {
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

    log::info!("Card {} removed from config", &cardnumber);

    Ok(())
}

pub async fn remove_card_from_config(
    config_path: &Path,
    card_number: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = config_path.to_path_buf();
    let number = card_number.to_string();

    // The file part (load-modify-save + cache refresh) is blocking — run it on
    // the blocking pool, serialized with all other config writers by the lock.
    let removed = tauri::async_runtime::spawn_blocking(
        move || -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
            let _guard = config_write_guard();

            log::debug!("Loading configuration from {:?}", path);
            let mut config = load_config(&path)?;

            if config.cards.remove(&number).is_none() {
                return Ok(false);
            }

            save_config(&path, &config)?;
            log::debug!("Configuration saved successfully after removal");

            load_config_to_cache(&config)?;
            log::debug!("Configuration loaded to cache successfully");

            Ok(true)
        },
    )
    .await
    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
        format!("remove_card_from_config: blocking task failed: {}", e).into()
    })??;

    if removed {
        // Kill card task with the specified client_id (card number)
        remove_connections(vec![card_number.to_string()]).await;
        log::debug!("Removed connection for card {}", card_number);

        emit_card_config_event("global-card-config-updated", card_number.to_string(), None);

        #[cfg(target_os = "linux")] {
            // "Super hack" to reload card states and trigger an event to update readers.

            use tokio::time::sleep;
            use tokio::time::Duration;
            use crate::smart_card::manual_sync_cards;

            sleep(Duration::from_millis(100)).await;
            manual_sync_cards(card_number.to_string(), false).await;
        }

        Ok(())
    } else {
        log::warn!("Cardnumber {} not found in configuration", card_number);
        Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Card not found in configuration",
        )))
    }
}

/// Public function to update the server address in the configuration.
/// This is a Tauri command: a thin async wrapper so the webview invoke never
/// runs file I/O on the main thread — the blocking core goes to the blocking pool.
#[tauri::command]
pub async fn update_server(host: String, ident: String, theme: String) -> bool {
    tauri::async_runtime::spawn_blocking(move || {
        let config_path = match get_config_path() {
            Ok(path) => path,
            Err(e) => {
                log::error!("Failed to get config path: {}", e);
                return false;
            }
        };

        match update_server_config(&config_path, &host, &ident, &theme) {
            Ok(_) => {
                log::info!("The server address is updated to '{}'.", host);
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
    })
}

/*
  HashMap. ATR = Card number

  initializing a global cache (HashMap<String, String>) using Mutex.
  Mapping card keys and matching them with the real company card number,
  which can only be entered manually
*/
#[derive(Default, Debug)]
pub struct CacheConfigData {
    pub cards: HashMap<String, CardConfig>,
    pub server: Option<ServerConfig>,
    pub ident: Option<String>,
    pub appearance: Option<AppearanceConfig>,
}

lazy_static! {
    /// Global cache for card ATRs and numbers.
    /// Initializing a global cache (HashMap<String, String>) using Mutex.
    /// Mapping card keys and matching them with the real company card number,
    /// which can only be entered manually.
    static ref CACHE: Mutex<CacheConfigData> = Mutex::new(CacheConfigData::default());
}

/// Serializes every load-modify-save cycle on config.yaml. Without it,
/// concurrent writers (frontend commands, the APDU sniffer, auth-result
/// persistence) interleave: both load the same version and the last save
/// silently drops the other's changes; they also share one tmp file, which
/// breaks the atomic-rename guarantee. Held across the whole read-modify-write
/// including the cache refresh, so cache order matches file order.
static CONFIG_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Acquires the config write lock, recovering from poisoning (a panic in an
/// earlier holder must not cascade into every future config write).
fn config_write_guard() -> std::sync::MutexGuard<'static, ()> {
    match CONFIG_WRITE_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::warn!("CONFIG_WRITE_LOCK was poisoned — recovering");
            poisoned.into_inner()
        }
    }
}

/// Acquires the runtime cache lock, recovering from poisoning. The cache can
/// never be left half-modified: reads don't mutate it and the only write
/// replaces the whole struct in one assignment, so recovery is always safe.
/// Panicking here instead (`.unwrap()`) would turn one panic — possibly
/// swallowed silently by a tokio task — into a cascade that kills every card
/// connection and the reader monitor until the app is restarted.
fn cache_guard() -> std::sync::MutexGuard<'static, CacheConfigData> {
    match CACHE.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::warn!("CACHE mutex was poisoned — recovering");
            poisoned.into_inner()
        }
    }
}
#[derive(Debug)]
pub enum CacheSection {
    Cards,
    Server,
    Ident,
    Appearance
}

/// Returns a clone of the CardConfig for the given card number from the runtime cache,
/// or None if the card is not known yet.
pub fn get_card_config_from_cache(card_number: &str) -> Option<CardConfig> {
    let cache = cache_guard();
    cache.cards.get(card_number).cloned()
}

/// Records the final state of an authentication attempt in the card config:
/// `success == true` → green "success" line in UI; `false` → red "fail".
/// The processing state while auth is running is derived from Reader.authentication
/// in the frontend and is NOT stored here (it's transient, lost on restart).
pub fn record_auth_result(card_number: &str, success: bool) {
    let ts = chrono::Utc::now().timestamp() as u64;
    // Mutate fresh file state under the config lock instead of writing a full
    // cache snapshot back — a stale snapshot would revert fields a concurrent
    // writer (e.g. the sniffer) just persisted.
    if !mutate_card_config(card_number, |card| {
        card.last_auth = Some((ts, success));
        true
    }) {
        log::error!(
            "record_auth_result: failed to persist last_auth for card_number {}",
            card_number
        );
    }
}

/// Async-safe variant of `record_auth_result`.
/// Offloads the blocking config write (disk I/O + sync mutexes) to a dedicated
/// blocking thread so the caller's tokio worker is not stalled. Use this from
/// async contexts such as the MQTT eventloop tasks.
pub async fn record_auth_result_async(card_number: &str, success: bool) {
    let card_number = card_number.to_string();
    if let Err(e) = tokio::task::spawn_blocking(move || {
        record_auth_result(&card_number, success);
    })
    .await
    {
        log::error!("record_auth_result_async: spawn_blocking failed: {:?}", e);
    }
}

/// Retrieves a value from the cache by key.
/// This function locks the cache, retrieves the value for the given key, and returns it.
pub fn get_from_cache(section: CacheSection, key: &str) -> String {
    let cache = cache_guard();

    match section {
        CacheSection::Cards => {
            // Reverse lookup: ICCID → card number.
            for (card_number, config) in &cache.cards {
                if config.iccid == key {
                    return card_number.clone();
                }
            }
            log::debug!("cache: no card number for ICCID {}", key);
            "".to_string()
        }

        CacheSection::Server => match (&cache.server, key) {
            (Some(server), "host") => server.host.clone(),
            (Some(_), _) => {
                log::debug!("cache: unknown key for server section: {}", key);
                "".to_string()
            }
            (None, _) => "".to_string(),
        },

        CacheSection::Ident => cache.ident.clone().unwrap_or_default(),

        CacheSection::Appearance => match (&cache.appearance, key) {
            (Some(appearance), "dark_theme") => format!("{:?}", appearance.dark_theme),
            (Some(_), _) => {
                log::debug!("cache: unknown key for appearance section: {}", key);
                "".to_string()
            }
            (None, _) => "".to_string(),
        },
    }
}

/// Splits a host string into host and port components.
///
/// This function takes a string containing a host and port separated by a colon (e.g., "example.com:8080"),
/// and splits it into two separate strings: the host and the port. If the input string does not contain a colon,
/// it returns an error.
pub fn split_host_to_parts(host: &str) -> Result<(String, u16), String> {
    let parts: Vec<&str> = host.split(':').collect();
    if parts.len() == 2 {
        let port = parts[1]
            .parse::<u16>()
            .map_err(|_| "Invalid port number".to_string())?;
        Ok((parts[0].to_string(), port))
    } else {
        Err("Host doesn't correspond to the format 'host:port'".to_string())
    }
}

/// Loads the configuration file into the cache.
/// This function reads the configuration file, parses it, and loads the cards into the global cache,
/// which is used to synchronize the launch of asynchronous tasks for MQTT connection, as well as for display on the interface.
pub fn load_config_to_cache(
    config: &ConfigurationFile,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    log::debug!("load_config_to_cache");

    let mut cache = cache_guard();
    *cache = CacheConfigData {
        cards: config.cards.clone(),
        server: config.server.clone(),
        ident: config.ident.clone(),
        appearance: config.appearance.clone(),
    };

    // trace_cache(&*cache);

    Ok(())
}

// pub fn trace_cache(cache: &CacheConfigData) {
//     log::debug!("HashMap: Company Card Number => Card Configuration ----------");
//     for (card_number, card_config) in cache.cards.iter() {
//         log::debug!(
//             "CN: {:<16} | ICCID: {:<16} | Expire: {}",
//             card_number,
//             card_config.iccid,
//             card_config.expire.unwrap_or(0)
//         );
//     }
//     log::debug!("{}", "-".repeat(70));

//     if let Some(ident) = &cache.ident {
//         log::debug!("ident: {}", ident);
//     }

//     if let Some(server) = &cache.server {
//         log::debug!("Server Host: {}", server.host);
//     } else {
//         log::warn!("No server configuration found.");
//     }

//     if let Some(appearance) = &cache.appearance {
//         log::debug!("Appearance: {:?}", appearance);
//     } else {
//         log::warn!("No appearance configuration found.");
//     }
// }

/// Generates a unique ident value based on the current time in microseconds.
/// The ident value is in the format "TBA" followed by 13 digits.
fn generate_ident() -> String {
    // A system clock set before 1970 makes duration_since fail; fall back to
    // zero (ident "TBA0000000000000") instead of panicking at first launch —
    // the user can always set their own ident in the settings dialog.
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or_else(|e| {
            log::warn!("System clock is before UNIX epoch ({e}); using zero ident");
            0
        });
    format!("TBA{:013}", micros % 1_000_000_000_000u128)
}

/// Initializes the configuration file.
/// This function creates a default configuration file if it does not exist, and loads it into the cache.
pub fn init_config() -> io::Result<()> {
    // Startup also participates in the write serialization: a card task could
    // already be persisting by the time a repeated `frontend-loaded` re-inits.
    let _guard = config_write_guard();

    let config_path = get_config_path()?;
    let config: ConfigurationFile;

    if config_path.exists() {
        let mut contents = String::new();
        File::open(&config_path)?.read_to_string(&mut contents)?;

        match serde_yaml::from_str::<ConfigurationFile>(&contents) {
            Ok(mut loaded_config) => {
                loaded_config.version = env!("CARGO_PKG_VERSION").to_string();
                config = loaded_config;
            }
            Err(e) => {
                log::error!("Config parse failed ({}). Resetting to default config.", e);
                config = generate_default_config();
            }
        }
    } else {
        log::debug!("Config file not found. Generating default config.");
        config = generate_default_config();
    }

    save_config(&config_path, &config)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    log::debug!("config: saved config");

    load_config_to_cache(&config)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    Ok(())
}

/// Emits every known card config to the frontend. Called on each
/// `frontend-loaded` so a (re)loaded webview gets the current card list —
/// the backend initializes only once, but the frontend can reload many times.
pub fn emit_all_card_configs() {
    // Clone out under the lock, emit after releasing it.
    let cards: Vec<(String, CardConfig)> = {
        let cache = cache_guard();
        cache
            .cards
            .iter()
            .map(|(number, cfg)| (number.clone(), cfg.clone()))
            .collect()
    };

    for (card_number, card_config) in cards {
        emit_card_config_event(
            "global-card-config-updated",
            card_number,
            Some(card_config),
        );
    }
}

// Default structure config
fn generate_default_config() -> ConfigurationFile {
    ConfigurationFile {
        name: "Tacho Bridge Application".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        description: "Application for the tachograph cards authentication".to_string(),
        appearance: Some(AppearanceConfig {
            dark_theme: DarkTheme::Auto,
        }),
        ident: Some(generate_ident()),
        server: None,
        cards: HashMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_tmp_dir(tag: &str) -> PathBuf {
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

    fn sample_config() -> ConfigurationFile {
        let mut cards = HashMap::new();
        cards.insert(
            "ABCDEF0123456789".to_string(),
            CardConfig {
                iccid: "1122334455667788".to_string(),
                expire: Some(1_700_000_000),
                name: Some("My Company Card".to_string()),
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
            }),
            cards,
        }
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = unique_tmp_dir("roundtrip");
        let path = dir.join("config.yaml");

        let cfg = sample_config();
        save_config(&path, &cfg).expect("save");

        let loaded = load_config(&path).expect("load");
        assert_eq!(loaded.cards.len(), 1);
        let card = loaded.cards.get("ABCDEF0123456789").expect("card present");
        assert_eq!(card.iccid, "1122334455667788");
        assert_eq!(card.card_type, Some(4));
        assert_eq!(card.last_auth, Some((1_700_000_500, true)));
        assert_eq!(
            loaded.server.as_ref().map(|s| s.host.as_str()),
            Some("mqtt.example.com:8883")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_is_atomic_no_tmp_leftover() {
        let dir = unique_tmp_dir("atomic");
        let path = dir.join("config.yaml");
        let tmp_path = dir.join("config.yaml.tmp");

        save_config(&path, &sample_config()).expect("save");
        assert!(path.exists(), "final config must exist");
        assert!(!tmp_path.exists(), "tmp file must be renamed away");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_overwrites_existing_file_safely() {
        let dir = unique_tmp_dir("overwrite");
        let path = dir.join("config.yaml");

        save_config(&path, &sample_config()).expect("first save");
        let first_meta = fs::metadata(&path).expect("meta1");

        let mut second = sample_config();
        second.cards.clear();
        save_config(&path, &second).expect("second save");

        let loaded = load_config(&path).expect("load second");
        assert!(loaded.cards.is_empty(), "second save must replace contents");
        assert!(first_meta.is_file());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_rejects_malformed_yaml() {
        let dir = unique_tmp_dir("malformed");
        let path = dir.join("config.yaml");
        fs::write(&path, b"this: is: not: yaml: [[[").expect("write garbage");

        let res = load_config(&path);
        assert!(res.is_err(), "load must fail on malformed yaml");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_host_to_parts_valid() {
        let (host, port) = split_host_to_parts("mqtt.flespi.io:8883").expect("split");
        assert_eq!(host, "mqtt.flespi.io");
        assert_eq!(port, 8883);
    }

    #[test]
    fn split_host_to_parts_missing_port() {
        assert!(split_host_to_parts("mqtt.flespi.io").is_err());
    }

    #[test]
    fn split_host_to_parts_bad_port() {
        assert!(split_host_to_parts("mqtt.flespi.io:notaport").is_err());
    }

    #[test]
    fn generate_default_config_is_well_formed() {
        let cfg = generate_default_config();
        assert!(!cfg.name.is_empty());
        assert!(!cfg.version.is_empty());
        assert!(cfg.cards.is_empty());
        let ident = cfg.ident.expect("default ident");
        assert!(ident.starts_with("TBA"));
        assert_eq!(ident.len(), 3 + 13);
    }

    #[test]
    fn generate_ident_format() {
        let id = generate_ident();
        assert!(id.starts_with("TBA"));
        assert_eq!(id.len(), 3 + 13);
        assert!(id[3..].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn concurrent_card_updates_do_not_lose_writes() {
        // Regression for the read-modify-write race: without CONFIG_WRITE_LOCK
        // two writers load the same version and the last save drops the other's
        // card. Every thread adds unique cards; all must survive.
        let dir = unique_tmp_dir("concurrent");
        let path = dir.join("config.yaml");
        save_config(&path, &sample_config()).expect("seed config");

        const THREADS: usize = 8;
        const UPDATES_PER_THREAD: usize = 10;

        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for i in 0..UPDATES_PER_THREAD {
                        let number = format!("THREAD{:02}CARD{:04}", t, i);
                        let card = CardConfig {
                            iccid: format!("{:016}", t * 1000 + i),
                            expire: None,
                            name: None,
                            card_type: None,
                            structure_version: None,
                            company_name: None,
                            company_address: None,
                            last_auth: None,
                        };
                        update_card_config(&path, &number, card).expect("update");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("writer thread panicked");
        }

        let loaded = load_config(&path).expect("load");
        assert_eq!(
            loaded.cards.len(),
            1 + THREADS * UPDATES_PER_THREAD,
            "every concurrent update must be preserved"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn card_config_yaml_roundtrip_preserves_optional_fields() {
        let card = CardConfig {
            iccid: "1234567890123456".to_string(),
            expire: None,
            name: None,
            card_type: None,
            structure_version: None,
            company_name: None,
            company_address: None,
            last_auth: None,
        };
        let yaml = serde_yaml::to_string(&card).expect("ser");
        let back: CardConfig = serde_yaml::from_str(&yaml).expect("de");
        assert_eq!(back.iccid, "1234567890123456");
        assert!(back.expire.is_none());
        assert!(back.last_auth.is_none());
    }
}

pub fn emit_global_config_server(app: &tauri::AppHandle) -> Result<(), Box<dyn Error>> {
    // small note: the structure requires the clone trait because the configuration is passed by reference,
    // so the value cannot be fully transferred to ownership.

    // Gettting Host value from the "operation cahce" with the ServerConfig structure
    let host = get_from_cache(CacheSection::Server, "host");
    let ident = get_from_cache(CacheSection::Ident, "ident");
    let appearance = get_from_cache(CacheSection::Appearance, "dark_theme");

    let mut config_app_payload = HashMap::new();
    config_app_payload.insert("host", host);
    config_app_payload.insert("ident", ident);
    config_app_payload.insert("dark_theme", appearance);

    // Emit this data as a global event to update fornt-end fields
    if let Err(e) = app.emit("global-config-server", config_app_payload) {
        return Err(Box::new(e));
    }

    Ok(())
}
