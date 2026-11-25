//! The in-memory copy of config.yaml that the rest of the app reads.
//!
//! Readers (card sessions, the app connection, the updater) never touch the
//! file: they read this cache, which every successful save replaces as a
//! whole (`load_config_to_cache`) while still holding the config write lock.

// ───── Std Lib ─────
use std::collections::HashMap;
use std::sync::Mutex;

// ───── External Crates ─────
use lazy_static::lazy_static;

// ───── Local Modules ─────
use super::types::{AppearanceConfig, CardConfig, ConfigurationFile, ServerConfig, SyncState};

/// In-memory copy of config.yaml, refreshed after every save.
#[derive(Default, Debug)]
pub struct CacheConfigData {
    pub cards: HashMap<String, CardConfig>,
    pub server: Option<ServerConfig>,
    pub ident: Option<String>,
    pub appearance: Option<AppearanceConfig>,
    pub beta_updates: Option<bool>,
    pub auto_install_updates: Option<bool>,
    pub debug_log_until: Option<u64>,
    pub sync: Option<SyncState>,
}

lazy_static! {
    /// Global cache of the config (see `CacheConfigData`).
    static ref CACHE: Mutex<CacheConfigData> = Mutex::new(CacheConfigData::default());
}

/// Acquires the runtime cache lock, recovering from poisoning. The cache can
/// never be left half-modified: reads don't mutate it and the only write
/// replaces the whole struct in one assignment, so recovery is always safe.
/// Panicking here instead (`.unwrap()`) would turn one panic — possibly
/// swallowed silently by a tokio task — into a cascade that kills every card
/// connection and the reader monitor until the app is restarted.
pub(super) fn cache_guard() -> std::sync::MutexGuard<'static, CacheConfigData> {
    match CACHE.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::warn!("CACHE mutex was poisoned — recovering");
            poisoned.into_inner()
        }
    }
}
/// The cache section `get_from_cache` reads a key from.
#[derive(Debug)]
pub enum CacheSection {
    Server,
    Ident,
    Appearance,
    Updates,
}

/// Returns a clone of the CardConfig for the given card number from the runtime cache,
/// or None if the card is not known yet.
pub fn get_card_config_from_cache(card_number: &str) -> Option<CardConfig> {
    let cache = cache_guard();
    cache.cards.get(card_number).cloned()
}

/// Returns the card number whose config holds the given ICCID, from the runtime cache.
/// Used by the rack path: the server discovers cards by ICCID, but card sessions connect
/// by the company card number only the local config knows.
pub fn find_card_number_by_iccid(iccid: &str) -> Option<String> {
    if iccid.is_empty() {
        // never match a configured card whose ICCID has not been filled in yet
        return None;
    }
    let cache = cache_guard();
    let mut matches: Vec<&String> = cache
        .cards
        .iter()
        .filter(|(_, card)| card.iccid == iccid)
        .map(|(number, _)| number)
        .collect();
    if matches.is_empty() {
        return None;
    }
    // `cards` is a HashMap, so `find` would return an arbitrary entry and could
    // pick a different one from run to run. With two entries sharing an ICCID
    // that means the physical card authenticates under a nondeterministic
    // client_id — bridging the VU session to the wrong flespi identity and
    // persisting sniffed EF data onto the wrong card. Sorting makes the choice
    // stable, and the warning surfaces the misconfiguration that has to be
    // fixed in the UI.
    matches.sort();
    if matches.len() > 1 {
        log::warn!(
            "Config error: ICCID {} is linked to {} card entries ({}). Using {} — remove the duplicate links.",
            iccid,
            matches.len(),
            matches
                .iter()
                .map(|number| number.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            matches[0]
        );
    }
    Some(matches[0].clone())
}

/// Retrieves a value from the cache by key.
/// This function locks the cache, retrieves the value for the given key, and returns it.
pub fn get_from_cache(section: CacheSection, key: &str) -> String {
    let cache = cache_guard();

    match section {
        CacheSection::Server => match (&cache.server, key) {
            (Some(server), "host") => server.host.clone(),
            // MQTT credentials; empty string = not configured (anonymous connect).
            (Some(server), "username") => server.username.clone().unwrap_or_default(),
            (Some(server), "password") => server.password.clone().unwrap_or_default(),
            // "true"/"false"; absent = anonymous connect
            (Some(server), "auth_enabled") => server.auth_enabled.unwrap_or(false).to_string(),
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

        // "true"/"false"; absent flags read as "false" (stable channel,
        // manual installs).
        CacheSection::Updates => match key {
            "beta_updates" => cache.beta_updates.unwrap_or(false).to_string(),
            "auto_install_updates" => cache.auto_install_updates.unwrap_or(false).to_string(),
            _ => {
                log::debug!("cache: unknown key for updates section: {}", key);
                "".to_string()
            }
        },
    }
}

/// Loads the configuration file into the cache.
/// This function reads the configuration file, parses it, and loads the cards into the global cache,
/// which is used to synchronize the launch of asynchronous tasks for MQTT connection, as well as for display on the interface.
pub(super) fn load_config_to_cache(
    config: &ConfigurationFile,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    log::debug!("load_config_to_cache");

    let mut cache = cache_guard();
    *cache = CacheConfigData {
        cards: config.cards.clone(),
        server: config.server.clone(),
        ident: config.ident.clone(),
        appearance: config.appearance.clone(),
        beta_updates: config.beta_updates,
        auto_install_updates: config.auto_install_updates,
        debug_log_until: config.debug_log_until,
        sync: config.sync.clone(),
    };

    Ok(())
}

/// The persisted extended-debug window: `Some(0)` = on until switched off,
/// `Some(ts)` = on until that unix time, `None` = off.
pub fn debug_log_until() -> Option<u64> {
    cache_guard().debug_log_until
}

/// The credentials saved in the config, when a username is set. The switch
/// (`auth_enabled`) is not checked here: the callers do that.
pub fn saved_credentials() -> Option<(String, String)> {
    let cache = cache_guard();
    let server = cache.server.as_ref()?;
    let username = server.username.clone().filter(|u| !u.is_empty())?;
    Some((username, server.password.clone().unwrap_or_default()))
}
