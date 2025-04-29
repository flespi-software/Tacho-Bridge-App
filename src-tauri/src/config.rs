use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use serde_yaml;

use tauri::Emitter;

use std::fs;

/// Represents the configuration settings for the application.
#[derive(Serialize, Deserialize, Debug)]
pub struct ConfigurationFile {
    name: String,                           // The name of the application.
    version: String,                        // The version of the application.
    description: String,                    // A brief description of the application.
    appearance: Option<AppearanceConfig>,          // Optional UI configuration settings.
    ident: Option<String>,                  // Optional ident for the application.
    server: Option<ServerConfig>,           // Optional server configuration settings.
    cards: Option<HashMap<String, String>>, // Optional mapping of card ATRs to card numbers.
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
// UI Configuration structure, part of ConfigurationFile that contains data about how UI looks like.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppearanceConfig {
    pub dark_theme: DarkTheme,
}

/// Retrieves the configuration file path.
/// This function constructs the path to the configuration file, creating the necessary directories if they do not exist.
///
/// # Returns
///
/// * `Result<PathBuf>` - The path to the configuration file or an error if the path could not be created.
pub fn get_config_path() -> io::Result<PathBuf> {
    let mut config_path = PathBuf::new();

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let home_dir = env::var("HOME");

    #[cfg(target_os = "windows")]
    let home_dir = env::var("USERPROFILE");

    match home_dir {
        Ok(home) => config_path.push(home),
        Err(e) => {
            log::error!("Failed to get home directory environment variable: {}", e);
            return Err(io::Error::new(io::ErrorKind::Other, "Failed to get home directory environment variable"));
        }
    }

    config_path.push("Documents");
    config_path.push("tba");

    if let Err(e) = fs::create_dir_all(&config_path) {
        log::error!("Failed to create directories: {}", e);
        return Err(e);
    }

    config_path.push("config.yaml");

    Ok(config_path)

}
/// Load the configuration from the file.
/// This function reads the configuration file and parses it.
///
/// # Arguments
///
/// * `config_path` - The path to the configuration file.
///
/// # Returns
///
/// * `Result<ConfigurationFile, Box<dyn std::error::Error + Send + Sync>>` - The loaded configuration or an error.
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
///
/// # Arguments
///
/// * `config_path` - The path to the configuration file.
/// * `config` - The configuration to save.
///
/// # Returns
///
/// * `Result<(), Box<dyn std::error::Error + Send + Sync>>` - Returns `Ok` if the configuration was successfully saved, otherwise returns an error.
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
///
/// # Arguments
///
/// * `config_path` - The path to the configuration file.
/// * `atr` - The ATR of the card.
/// * `cardnumber` - The card number.
///
/// # Returns
///
/// * `Result<(), Box<dyn std::error::Error + Send + Sync>>` - Returns `Ok` if the configuration was successfully updated, otherwise returns an error.
fn update_card_config(
    config_path: &Path,
    atr: &str,
    cardnumber: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    log::debug!("Loading configuration from {:?}", config_path);
    let mut config = load_config(config_path)?;
    log::debug!("Loaded configuration: {:?}", config);

    // Ensure the cards field is initialized
    if config.cards.is_none() {
        config.cards = Some(HashMap::new());
    }

    let cards = config.cards.as_mut().unwrap();

    if cards.values().any(|number| number == cardnumber) {
        log::info!("Card with cardnumber {} already exists", cardnumber);
        return Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "Card with this cardnumber already exists",
        )));
    } else {
        log::debug!("Adding new card with ATR {} and cardnumber {}", atr, cardnumber);
        config
            .cards
            .get_or_insert_with(HashMap::new)
            .insert(atr.to_string(), cardnumber.to_string());

        log::debug!("Saving updated configuration to {:?}", config_path);
        save_config(config_path, &config)?;
        log::debug!("Configuration saved successfully");

        log::debug!("Loading updated configuration to cache");
        load_config_to_cache(&config)?;
        log::debug!("Configuration loaded to cache successfully");
    }

    Ok(())
}

/// Public function to update the configuration with a new card.
/// This function is a Tauri command that updates the configuration file with a new card's ATR and card number.
///
/// # Arguments
///
/// * `atr` - The ATR of the card.
/// * `cardnumber` - The card number.
///
/// # Returns
///
/// * `bool` - Returns `true` if the configuration was successfully updated, otherwise `false`.
#[tauri::command]
pub fn update_card(atr: &str, cardnumber: &str) -> bool {
    let config_path = match get_config_path() {
        Ok(path) => path,
        Err(e) => {
            log::error!("Failed to get config path: {}", e);
            return false;
        }
    };

    match update_card_config(&config_path, atr, cardnumber) {
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
///
/// # Arguments
///
/// * `config_path` - The path to the configuration file.
/// * `server_address` - The new server address.
///
/// # Returns
///
/// * `Result<(), Box<dyn std::error::Error + Send + Sync>>` - Returns `Ok` if the configuration was successfully updated, otherwise returns an error.
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

/// Public function to update the server address in the configuration.
/// This function is a Tauri command that updates the configuration file with a new server address.
///
/// # Arguments
///
/// * `server_address` - The new server address.
///
/// # Returns
///
/// * `bool` - Returns `true` if the configuration was successfully updated, otherwise `false`.
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
#[derive(Default)]
pub struct CacheConfigData {
    pub cards: HashMap<String, String>,
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

pub enum CacheSection {
    Cards,
    Server,
    Ident,
    Appearance
}

/// Retrieves a value from the cache by key.
/// This function locks the cache, retrieves the value for the given key, and returns it.
///
/// # Arguments
///
/// * `key` - The key to search in the cache.
///
/// # Returns
///
/// * `String` - The value associated with the key, or an empty string if the key is not found.
pub fn get_from_cache(section: CacheSection, key: &str) -> String {
    let cache = CACHE.lock().unwrap();
    match section {
        CacheSection::Cards => match cache.cards.get(key) {
            Some(value) => value.clone(),
            None => "".to_string(),
        },
        CacheSection::Server => {
            if let Some(server) = &cache.server {
                match key {
                    "host" => server.host.clone(),
                    _ => "".to_string(),
                }
            } else {
                "".to_string()
            }
        }
        CacheSection::Ident => {
            if let Some(ident) = &cache.ident {
                ident.clone()
            } else {
                "".to_string()
            }
        }
        CacheSection::Appearance => {
            if let Some(appearance) = &cache.appearance {
                match key {
                    "dark_theme" => format!("{:?}", appearance.dark_theme),
                    _ => "".to_string(),
                }
            } else {
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
///
/// # Arguments
///
/// * `host` - A string slice that holds the host and port.
///
/// # Returns
///
/// * `Result<(String, String), String>` - A result containing a tuple with the host and port as separate strings,
///   or an error message if the input string does not contain a colon.
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
///
/// # Arguments
///
/// * `config` - link to the loaded configuration file object.
///
/// # Returns
///
/// * `Result<(), Box<dyn std::error::Error + Send + Sync>>` - Returns `Ok` if the configuration was successfully loaded, otherwise returns an error.
pub fn load_config_to_cache(
    config: &ConfigurationFile,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    log::debug!("load_config_to_cache");

    let mut cache = CACHE.lock().unwrap();
    *cache = CacheConfigData {
        cards: config.cards.clone().unwrap_or_default(),
        server: config.server.clone(),
        ident: config.ident.clone(),
        appearance: config.appearance.clone(),
    };

    trace_cache(&*cache);

    Ok(())
}

/// Displays the cache contents as a table.
/// This function prints cache in a table format for debugging and inspection.
pub fn trace_cache(cache: &CacheConfigData) {
    log::debug!("HashMap value correspondence table ATR: Company card number ----------");
    for (key, value) in cache.cards.iter() {
        log::debug!("{:<16}: {:<20}", value, key);
    }
    log::debug!("{}", "-".repeat(70));
    if let Some(ident) = &cache.ident {
        log::debug!("ident: {}", ident);
    }
    if let Some(server) = &cache.server {
        log::info!("Server Host: {}", server.host);
    } else {
        log::info!("No server configuration found.");
    }
    if let Some(appearance) = &cache.appearance {
        log::info!("Appearance: {:?}", appearance);
    } else {
        log::info!("No appearance configuration found.");
    }
}

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
///
/// # Returns
///
/// * `io::Result<()>` - Returns `Ok` if the configuration was successfully initialized, otherwise returns an error.
pub fn init_config() -> io::Result<()> {
    let config_path = get_config_path()?;
    if Path::new(&config_path).exists() {
        // Load existing configuration
        let mut config_contents = String::new();
        File::open(&config_path)?.read_to_string(&mut config_contents)?;

        let mut config: ConfigurationFile = serde_yaml::from_str(&config_contents)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        // Remove duplicate card numbers
        if let Some(cards) = &mut config.cards {
            let mut seen = HashMap::new();
            cards.retain(|atr, cardnumber| {
                if atr.is_empty() {
                    log::warn!("Invalid entry with empty ATR and card number {} removed", cardnumber);
                    false
                } else if seen.contains_key(cardnumber) {
                    log::warn!("Duplicate card number {} with ATR {} removed", cardnumber, atr);
                    false
                } else {
                    seen.insert(cardnumber.clone(), atr.clone());
                    true
                }
            });
        }

        // Update the version
        config.version = env!("CARGO_PKG_VERSION").to_string();

        // save updated config
        save_config(&config_path, &config)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        // load cache to the object `ConfigurationFile`
        load_config_to_cache(&config)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        return Ok(());
    }

    log::debug!("config: path not exists");

    // Filling the configuration structure with default values
    let config: ConfigurationFile = ConfigurationFile {
        name: "Tacho Bridge Application".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        description: "Application for the tachograph cards authentication".to_string(),
        appearance: Some(AppearanceConfig {
            dark_theme: DarkTheme::Auto,
        }),
        ident: Some(generate_ident()),
        server: None,
        cards: None,
    };

    // save new config
    save_config(&config_path, &config)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    log::debug!("config: default config saved");

    // load to cache
    load_config_to_cache(&config)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    Ok(())
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
