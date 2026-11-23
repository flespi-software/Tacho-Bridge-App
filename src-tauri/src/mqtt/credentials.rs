//! The MQTT credentials in effect: the pair entered for this run only (kept in
//! memory) or the one saved in the config, and the switch that enables them.

use crate::config::{get_from_cache, CacheSection};

/// Credentials entered for this run only ("Save to config" unchecked): they
/// live here and nowhere on disk, and are gone with the process. Take
/// precedence over the saved pair while set.
static SESSION_CREDENTIALS: std::sync::Mutex<Option<(String, String)>> =
    std::sync::Mutex::new(None);

fn session_credentials() -> std::sync::MutexGuard<'static, Option<(String, String)>> {
    SESSION_CREDENTIALS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Removes and returns the in-memory credentials.
pub(crate) fn take_session_credentials() -> Option<(String, String)> {
    session_credentials().take()
}

/// Replaces (or clears, with `None`) the in-memory credentials.
pub(crate) fn set_session_credentials(pair: Option<(String, String)>) {
    *session_credentials() = pair;
}

/// The username/password every new MQTT connection will use, or `None` for
/// an anonymous connect: the switch is off, or it is on but no credentials
/// have been provided yet (the startup prompt is still open).
pub(crate) fn resolved_credentials() -> Option<(String, String)> {
    if get_from_cache(CacheSection::Server, "auth_enabled") != "true" {
        return None;
    }
    known_credentials()
}

/// The pair the switch would put in effect: the one entered for this run,
/// else the saved one. Unlike `resolved_credentials` it ignores the switch,
/// so turning authentication back on can reuse a pair already in the file.
pub(crate) fn known_credentials() -> Option<(String, String)> {
    session_credentials()
        .clone()
        .or_else(crate::config::saved_credentials)
}

/// Authentication state as the webview needs it: the switch, the pair to
/// prefill (the settings dialog shows the password in effect), whether the
/// pair is saved in the config, and whether any pair is in effect at all
/// (saved or for this run). The settings report to the server takes the
/// switch, the username and the saved flag from it, never the password.
pub(crate) struct AuthState {
    pub enabled: bool,
    pub username: String,
    pub password: String,
    pub saved: bool,
    pub active: bool,
}

pub(crate) fn auth_state() -> AuthState {
    let enabled = get_from_cache(CacheSection::Server, "auth_enabled") == "true";
    let saved = crate::config::saved_credentials();
    let session = session_credentials().clone();
    let (username, password) = session
        .as_ref()
        .or(saved.as_ref())
        .cloned()
        .unwrap_or_default();
    AuthState {
        enabled,
        username,
        password,
        saved: saved.is_some(),
        active: enabled && (session.is_some() || saved.is_some()),
    }
}
