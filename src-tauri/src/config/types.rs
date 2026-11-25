//! The shape of config.yaml and the rules its values follow.
//!
//! The parse is deliberately lenient: a hand-edited file with one mistyped
//! card, an unknown theme or an unquoted number must not fail as a whole,
//! because a failed parse resets the file (see `init`). The validators here
//! are the single source of the card number, app ident and `host:port`
//! formats, and `generate_default_config` is what a first run writes.

// ───── Std Lib ─────
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

// ───── External Crates ─────
use serde::{Deserialize, Serialize};

/// Represents the configuration settings for the application.
#[derive(Serialize, Deserialize, Debug)]
pub struct ConfigurationFile {
    // `default` on the descriptive fields: a hand-prepared config with only
    // `server:` and `cards:` must not fail the parse (which resets the file).
    #[serde(default)]
    pub(super) name: String, // The name of the application.
    #[serde(default)]
    pub(super) version: String, // The version of the application.
    #[serde(default)]
    pub(super) description: String, // A brief description of the application.
    pub(super) appearance: Option<AppearanceConfig>, // Optional UI configuration settings.
    pub(super) ident: Option<String>,                // Optional ident for the application.
    pub(super) server: Option<ServerConfig>,         // Optional server configuration settings.
    // `default` keeps a missing or empty `cards:` key (YAML null) from failing
    // the whole config parse — a parse failure resets the file and wipes the
    // user's server host and card list.
    #[serde(default, deserialize_with = "cards_or_empty")]
    pub(super) cards: HashMap<String, CardConfig>, // Hashmap of the cards with the CardConfig structure
    // Update channel: true → the app also offers pre-release (alpha/beta/rc)
    // builds; false/absent → stable releases only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) beta_updates: Option<bool>,
    // Unattended updates: true → a background loop periodically checks the
    // selected channel and installs a found update on its own, waiting for a
    // pause in card activity so a restart never interrupts an authentication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) auto_install_updates: Option<bool>,
    // Extended debug log requested by the server (`debug_log` command): the
    // unix time it expires at, `0` = until switched off. Persisted so a crash
    // or restart inside the window resumes it (see `debug_log.rs`); absent =
    // off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) debug_log_until: Option<u64>,
    // The server copy of the card list this config was last synchronized with
    // (see `cards_sync`); absent = never synchronized.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "sync_lenient"
    )]
    pub(super) sync: Option<SyncState>,
}

/// The server copy of the card list the local one was last synchronized with.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SyncState {
    /// Revision of the server copy: a counter the server alone advances.
    pub rev: u64,
    /// Unix time the server stored that revision at; shown to the user only.
    #[serde(default)]
    pub updated: u64,
    /// `cards_sync::cards_hash` of the list at that moment: a different hash
    /// of the current list means it was changed locally since.
    pub hash: String,
}

/// A damaged `sync:` section reads as "never synchronized" instead of failing
/// the whole config parse (which resets the file).
fn sync_lenient<'de, D>(deserializer: D) -> Result<Option<SyncState>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_yaml::Value::deserialize(deserializer)?;
    Ok(serde_yaml::from_value::<Option<SyncState>>(value)
        .ok()
        .flatten())
}

/// Treats an explicitly empty `cards:` key (YAML null) as an empty map instead
/// of a type error, so a hand-emptied section can't nuke the whole config.
fn cards_or_empty<'de, D>(deserializer: D) -> Result<HashMap<String, CardConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let maybe: Option<serde_yaml::Mapping> = Option::deserialize(deserializer)?;
    Ok(maybe.map(cards_from_mapping).unwrap_or_default())
}

/// Converts the `cards:` mapping entry by entry, so one malformed card (a
/// hand-typed `expire: "2026-01-01"`, a numeric card-number key) costs only
/// that card instead of failing the whole parse — which would reset the file
/// and wipe the server host, every other card and the device ident.
pub(super) fn cards_from_mapping(mapping: serde_yaml::Mapping) -> HashMap<String, CardConfig> {
    let mut cards = HashMap::with_capacity(mapping.len());
    for (key, value) in mapping {
        let card_number = match &key {
            serde_yaml::Value::String(s) => s.clone(),
            serde_yaml::Value::Number(n) => n.to_string(),
            other => {
                log::error!("config: skipping a card with an invalid key {:?}", other);
                continue;
            }
        };
        // A key with no value (`CARD123:`) is a card typed in by hand.
        let value = if value.is_null() {
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
        } else {
            value
        };
        match serde_yaml::from_value::<CardConfig>(value.clone()) {
            Ok(card) => {
                cards.insert(card_number, card);
            }
            Err(e) => {
                // The entry is lost on the next save: keep it in the log so it
                // can be restored by hand.
                log::error!(
                    "config: skipping malformed card {} ({}): {}",
                    card_number,
                    e,
                    serde_yaml::to_string(&value).unwrap_or_default().trim()
                );
            }
        }
    }
    cards
}

/// An unknown or mistyped theme falls back to Auto instead of failing the
/// whole config parse.
fn dark_theme_lenient<'de, D>(deserializer: D) -> Result<DarkTheme, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_yaml::Value::deserialize(deserializer)?;
    Ok(dark_theme_from_label(value.as_str().unwrap_or_default()))
}

/// The theme of an `appearance:` section that names none.
fn default_dark_theme() -> DarkTheme {
    DarkTheme::Auto
}

// Server Configuration structure, part of ConfigurationFile that contains data about the server.
#[derive(Serialize, Deserialize, Clone)]
pub struct ServerConfig {
    pub host: String,
    /// MQTT authentication of every broker connection (see `mqtt::credentials`):
    /// `true` → connect with the credentials below (or the ones entered for
    /// this run only, kept in memory — then the two fields are absent here);
    /// `false`/absent → anonymous connect, whatever the fields hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_enabled: Option<bool>,
    /// Saved credentials (flespi channels accept a token as the username,
    /// password usually empty). Absent fields are not written back to the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// Hand-written so a `{:?}` of the config anywhere (there are several at
/// debug level) can never put the password into the log file.
impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("host", &self.host)
            .field("auth_enabled", &self.auth_enabled)
            // usually a flespi token: as secret as the password
            .field("username", &self.username.as_ref().map(|_| "<redacted>"))
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

// Dark Theme enum, part of AppearanceConfig that contains data about the theme.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum DarkTheme {
    Auto,
    Dark,
    Light,
}

/// A text field of a card typed in by hand without quotes (`iccid: 1122334455667788`,
/// `name: 2024`) reads as a YAML number: taken as the text it was meant to be,
/// or the whole card would be skipped as malformed and dropped by the next save.
/// Only what prints back exactly as typed is taken: a float (an all-digit
/// value with an `E` inside reads as one) has lost its original text.
fn lenient_opt_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match serde_yaml::Value::deserialize(deserializer)? {
        serde_yaml::Value::Null => Ok(None),
        serde_yaml::Value::String(text) => Ok(Some(text)),
        serde_yaml::Value::Number(number) if number.is_u64() => Ok(Some(number.to_string())),
        other => Err(serde::de::Error::custom(format!(
            "expected a string, found {:?}",
            other
        ))),
    }
}

/// `lenient_opt_string` for a field where an absent value is an empty string.
fn lenient_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    lenient_opt_string(deserializer).map(Option::unwrap_or_default)
}

/// One card entry of config.yaml, keyed by its card number. The frontend sends
/// the same structure as JSON.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CardConfig {
    // default: a card typed in by hand before its ICCID is known
    #[serde(default, deserialize_with = "lenient_string")]
    pub iccid: String, // ICCID
    pub expire: Option<u64>, // Expire date
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub name: Option<String>, // Custom card name (for ease of user identification)
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub t_protocol: Option<String>, // Card communication protocol "T0"/"T1"; auto-filled from ATR on first connection, may be overridden manually
    pub card_type: Option<u8>, // typeOfTachographCardId: 1=Driver, 2=Workshop, 3=Control, 4=Company
    pub structure_version: Option<(u8, u8)>, // cardStructureVersion (major, minor): major 0x00=Gen1, 0x01=Gen2; minor = data-element revision
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub company_name: Option<String>, // Company name (from EF_Identification, Company Card only)
    #[serde(default, deserialize_with = "lenient_opt_string")]
    pub company_address: Option<String>, // Company address (from EF_Identification, Company Card only)
    pub last_auth: Option<(u64, bool)>, // Last completed authentication: (unix_timestamp, success_flag)
}
// UI Configuration structure, part of ConfigurationFile that contains data about how UI looks like.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppearanceConfig {
    #[serde(
        default = "default_dark_theme",
        deserialize_with = "dark_theme_lenient"
    )]
    pub dark_theme: DarkTheme,
}

/// Maps the frontend's theme label to the config enum; unknown values fall back
/// to Auto, mirroring the deserialization default.
pub(super) fn dark_theme_from_label(theme: &str) -> DarkTheme {
    match theme {
        "Auto" => DarkTheme::Auto,
        "Dark" => DarkTheme::Dark,
        "Light" => DarkTheme::Light,
        _ => DarkTheme::Auto,
    }
}

/// Splits a `host:port` string (e.g. "example.com:8080") into its host and
/// port. Also the validator for every address the app accepts (settings
/// dialog, `set_server`): an empty host or port 0 would leave the device
/// unable to connect, with no remote way back.
pub fn split_host_to_parts(host: &str) -> Result<(String, u16), String> {
    let format_err = || "Host doesn't correspond to the format 'host:port'".to_string();
    let host = host.trim();
    // An IPv6 literal is bracketed: `[2001:db8::1]:8883`.
    let (name, port) = match host.strip_prefix('[') {
        Some(rest) => rest.split_once("]:").ok_or_else(format_err)?,
        None => host.split_once(':').ok_or_else(format_err)?,
    };
    let name = name.trim();
    if name.is_empty() || port.contains(':') {
        return Err(format_err());
    }
    let port = port
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
        .ok_or_else(|| "Invalid port number".to_string())?;
    Ok((name.to_string(), port))
}

/// The tachograph card number format (same rule as the frontend dialog).
pub(super) fn is_valid_card_number(number: &str) -> bool {
    number.len() == 16
        && number
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// The app identity format the server registers devices under: "TBA" + 13
/// digits (mirrors `TBA_IDENT_REGEXP` in the frontend).
pub(super) fn is_valid_ident(ident: &str) -> bool {
    ident.len() == 16 && ident.starts_with("TBA") && ident[3..].bytes().all(|b| b.is_ascii_digit())
}

/// Generates a unique ident value based on the current time in microseconds.
/// The ident value is in the format "TBA" followed by 13 digits.
pub(super) fn generate_ident() -> String {
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

// Default structure config
pub(super) fn generate_default_config() -> ConfigurationFile {
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
        beta_updates: None,
        auto_install_updates: None,
        debug_log_until: None,
        sync: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_missing_cards_key_does_not_fail_parse() {
        // An empty `cards:` key is YAML null — before `cards_or_empty` this
        // failed the whole parse and reset the config, wiping host and cards.
        let with_null_cards = "name: tba\nversion: 1.0.0\ndescription: d\nserver:\n  host: example.com:1883\ncards:\n";
        let parsed: ConfigurationFile =
            serde_yaml::from_str(with_null_cards).expect("null cards must parse");
        assert!(parsed.cards.is_empty());
        assert_eq!(parsed.server.unwrap().host, "example.com:1883");

        let without_cards = "name: tba\nversion: 1.0.0\ndescription: d\n";
        let parsed: ConfigurationFile =
            serde_yaml::from_str(without_cards).expect("missing cards must parse");
        assert!(parsed.cards.is_empty());
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
        assert!(split_host_to_parts("mqtt.flespi.io:0").is_err());
    }

    #[test]
    fn split_host_to_parts_rejects_empty_host_and_extra_colons() {
        assert!(split_host_to_parts(":1883").is_err());
        assert!(split_host_to_parts("a:b:1883").is_err());
    }

    #[test]
    fn card_number_format() {
        assert!(is_valid_card_number("ABCDEF0123456789"));
        assert!(!is_valid_card_number("abcdef0123456789"));
        assert!(!is_valid_card_number("ABCDEF012345678"));
        assert!(!is_valid_card_number("ABCDEF012345678/"));
        assert!(!is_valid_card_number("ABCDEF012345678#"));
    }

    #[test]
    fn split_host_to_parts_accepts_bracketed_ipv6() {
        let (host, port) = split_host_to_parts("[2001:db8::1]:8883").expect("split");
        assert_eq!(host, "2001:db8::1");
        assert_eq!(port, 8883);
        assert!(split_host_to_parts("[2001:db8::1]").is_err());
        assert!(split_host_to_parts("[]:8883").is_err());
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
    fn card_config_yaml_roundtrip_preserves_optional_fields() {
        let card = CardConfig {
            iccid: "1234567890123456".to_string(),
            expire: None,
            name: None,
            t_protocol: None,
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

    #[test]
    fn malformed_card_or_theme_does_not_fail_the_parse() {
        let yaml = r#"
ident: TBA1234567890123
server:
  host: mqtt.flespi.io
appearance:
  dark_theme: auto
cards:
  GOODCARD00000001:
    iccid: "1111"
    expire: 1700000000
  BADCARD000000002:
    iccid: "2222"
    expire: "2026-01-01"
  1234567890123456:
    iccid: "3333"
  EMPTYCARD0000003:
"#;
        let cfg: ConfigurationFile = serde_yaml::from_str(yaml).expect("lenient parse");
        assert_eq!(cfg.ident.as_deref(), Some("TBA1234567890123"));
        assert!(matches!(
            cfg.appearance.as_ref().map(|a| &a.dark_theme),
            Some(DarkTheme::Auto)
        ));
        assert!(cfg.cards.contains_key("GOODCARD00000001"));
        assert!(!cfg.cards.contains_key("BADCARD000000002"));
        assert_eq!(cfg.cards["1234567890123456"].iccid, "3333");
        assert!(cfg.cards.contains_key("EMPTYCARD0000003"));
    }

    #[test]
    fn unquoted_numeric_card_fields_are_read_as_text() {
        let yaml = r#"
cards:
  CARD000000000001:
    iccid: 1122334455667788
    name: 2024
  CARD000000000002:
    iccid: 0011223344556677
  CARD000000000003:
    iccid: "8899AABBCCDDEEFF"
    name: Truck 7
"#;
        let cfg: ConfigurationFile = serde_yaml::from_str(yaml).expect("lenient parse");
        assert_eq!(cfg.cards["CARD000000000001"].iccid, "1122334455667788");
        assert_eq!(cfg.cards["CARD000000000001"].name.as_deref(), Some("2024"));
        // leading zeros survive: such a scalar is a string in YAML 1.2
        assert_eq!(cfg.cards["CARD000000000002"].iccid, "0011223344556677");
        assert_eq!(cfg.cards["CARD000000000003"].iccid, "8899AABBCCDDEEFF");
        assert_eq!(
            cfg.cards["CARD000000000003"].name.as_deref(),
            Some("Truck 7")
        );
        // the frontend sends the same structure as JSON
        let card: CardConfig =
            serde_json::from_str(r#"{"iccid":"1122334455667788","name":null}"#).expect("json");
        assert_eq!(card.iccid, "1122334455667788");
        assert_eq!(card.name, None);
    }
}
