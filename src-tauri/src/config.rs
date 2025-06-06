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
    pub iccid: String,          // ICCID
    pub expire: Option<u64>,    // Expire date
    pub name: Option<String>,    // Custom card name (for ease of user identification)
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

/// Saves the configuration to the file.
/// This function serializes the configuration and writes it to the file.
fn save_config(
    config_path: &Path,
    config: &ConfigurationFile,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let yaml = serde_yaml::to_string(config)?;
    File::create(config_path)?.write_all(yaml.as_bytes())?;
    Ok(())
}

/// Updates the configuration with a new card.
/// This function updates the configuration file with a new card's ATR and card number.
fn update_card_config(
    config_path: &Path,
    card_number: &str,
    content: CardConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
                // needs_restart = true;
                changed = true;
            } else {
                // Update optional fields only (no restart required)
                if existing_card.expire != content.expire || existing_card.name != content.name {
                    log::debug!(
                        "Updating optional fields for card {}: name = {:?}, expire = {:?}",
                        card_number,
                        content.name,
                        content.expire
                    );
                    existing_card.expire = content.expire;
                    existing_card.name = content.name;
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

/// Public function to update the configuration with a new card.
/// This function is a Tauri command that updates the configuration file with a new card's ATR and card number.
#[tauri::command]
pub fn update_card(cardnumber: &str, content: CardConfig) -> bool {
    let config_path = match get_config_path() {
        Ok(path) => path,
        Err(e) => {
            log::error!("Failed to get config path: {}", e);
            return false;
        }
    };

    match update_card_config(&config_path, cardnumber, content) {
        Ok(_) => {
            log::info!("The card, {} is added to the configuration!", cardnumber);
            true
        }
        Err(e) => {
            log::error!("Failed to update config: {}", e);
            false
        }
    }
}

/// Updates the server address in the configuration.
/// This function updates the configuration file with a new server address.
pub fn update_server_config(
    config_path: &Path,
    host: &str,
    ident: &str,
    theme: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
    log::debug!("Loading configuration from {:?}", config_path);
    let mut config = load_config(config_path)?;
    log::debug!("Loaded configuration: {:?}", config);

    if config.cards.remove(card_number).is_some() {
        save_config(config_path, &config)?;
        log::debug!("Configuration saved successfully after removal");

        load_config_to_cache(&config)?;
        log::debug!("Configuration loaded to cache successfully");

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
/// This function is a Tauri command that updates the configuration file with a new server address.
#[tauri::command]
pub fn update_server(host: &str, ident: &str, theme: &str) -> bool {
    let config_path = match get_config_path() {
        Ok(path) => path,
        Err(e) => {
            log::error!("Failed to get config path: {}", e);
            return false;
        }
    };

    match update_server_config(&config_path, host, ident, theme) {
        Ok(_) => {
            log::info!("The server address is updated to '{}'.", host);
            true
        }
        Err(e) => {
            log::error!("Failed to update server address: {}", e);
            false
        }
    }
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
#[derive(Debug)]
pub enum CacheSection {
    Cards,
    Server,
    Ident,
    Appearance
}

/// Retrieves a value from the cache by key.
/// This function locks the cache, retrieves the value for the given key, and returns it.
pub fn get_from_cache(section: CacheSection, key: &str) -> String {
    let cache = CACHE.lock().unwrap();

    log::debug!("Accessing cache section: {:?}, key: {}", section, key);
    log::debug!("Current cache state: {:?}", *cache); // Покажет всё, если у `CacheConfigData` реализован Debug

    match section {
        CacheSection::Cards => {
            log::debug!("Looking up by ICCID: {}", key);

            for (card_number, config) in &cache.cards {
                log::debug!(
                    "Cache entry -> card_number: {}, iccid: {}, expire: {:?}",
                    card_number,
                    config.iccid,
                    config.expire
                );

                if config.iccid == key {
                    log::debug!(
                        "Match found: ICCID {} corresponds to card_number {}",
                        key,
                        card_number
                    );
                    return card_number.clone();
                }
            }

            log::debug!("No ICCID match found for: {}", key);
            "".to_string()
        }

        CacheSection::Server => {
            log::debug!("Accessing Server config");
            if let Some(server) = &cache.server {
                log::debug!("Server config: host = {}", server.host);
                match key {
                    "host" => server.host.clone(),
                    _ => {
                        log::debug!("Unknown key for server section: {}", key);
                        "".to_string()
                    }
                }
            } else {
                log::debug!("No server config found");
                "".to_string()
            }
        }

        CacheSection::Ident => {
            log::debug!("Accessing Ident config");
            if let Some(ident) = &cache.ident {
                log::debug!("Ident: {}", ident);
                ident.clone()
            } else {
                log::debug!("No ident found");
                "".to_string()
            }
        }

        CacheSection::Appearance => {
            log::debug!("Accessing Appearance config");
            if let Some(appearance) = &cache.appearance {
                log::debug!("Appearance config: {:?}", appearance);
                match key {
                    "dark_theme" => format!("{:?}", appearance.dark_theme),
                    _ => {
                        log::debug!("Unknown key for appearance section: {}", key);
                        "".to_string()
                    }
                }
            } else {
                log::debug!("No appearance config found");
                "".to_string()
            }
        }
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

    let mut cache = CACHE.lock().unwrap();
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
    let start = SystemTime::now();
    let since_the_epoch = start.duration_since(UNIX_EPOCH).expect("Time went backwards");
    let micros = since_the_epoch.as_micros();
    format!("TBA{:013}", micros % 1_000_000_000_000u128)
}

/// Initializes the configuration file.
/// This function creates a default configuration file if it does not exist, and loads it into the cache.
pub fn init_config() -> io::Result<()> {
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
            Err(_) => {
                log::warn!("Config format mismatch. Attempting migration...");
                config = migrate_old_config(&contents)
                    .unwrap_or_else(|| {
                        log::error!("Migration failed. Resetting to default config.");
                        generate_default_config()
                    });
            }
        }
    } else {
        log::debug!("Config file not found. Generating default config.");
        config = generate_default_config();
    }

    save_config(&config_path, &config)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    log::debug!("config: saved config");

    /*
        Send data of all cards in events one by one to the front.
    */
    for (card_number, card_config) in &config.cards {
        emit_card_config_event(
            "global-card-config-updated",
            card_number.clone(),
            Some(card_config.clone()),
        );
    }

    load_config_to_cache(&config)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    Ok(())
}

fn migrate_old_config(contents: &str) -> Option<ConfigurationFile> {
    #[derive(Deserialize)]
    struct OldConfig {
        name: String,
        #[allow(dead_code)] // to say the compiler does not warn about an unused field that is used in another file.
        version: String,
        description: String,
        appearance: Option<AppearanceConfig>,
        ident: Option<String>,
        server: Option<ServerConfig>,
        cards: Option<HashMap<String, String>>, // old cards format
    }

    let old_config: OldConfig = serde_yaml::from_str(contents).ok()?;

    let mut new_cards = HashMap::new();
    if let Some(old_cards ) = old_config.cards {
        for (_, card_number) in old_cards {
            let card_config = CardConfig {
                iccid: String::new(),
                expire: None,
                name: None
            };
            new_cards.insert(card_number, card_config);
        }
    }

    Some(ConfigurationFile {
        name: old_config.name,
        version: env!("CARGO_PKG_VERSION").to_string(),
        description: old_config.description,
        appearance: old_config.appearance,
        ident: old_config.ident,
        server: old_config.server,
        cards: new_cards,
    })
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
