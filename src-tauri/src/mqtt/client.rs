//! Building the MQTT connections every loop of the application shares (the
//! card sessions, the app connection, the rack card sessions), and the
//! one-line reporting of their connection failures.

use std::time::Duration;

use rumqttc::v5::{AsyncClient, ConnectionError, MqttOptions, StateError};

use crate::config::{get_from_cache, CacheSection};

use super::credentials::resolved_credentials;

/// Maps a connection error to a short stable `kind` for one-line logs, plus a
/// flag saying whether it is routine network churn. Expected errors are logged
/// as a single WARN line without the full Debug dump (which used to repeat the
/// same "Connection reset by peer" six times per blip across three lines);
/// unexpected ones keep the details at ERROR. Shared by all three MQTT loops
/// (app, per-card, rack).
pub(crate) fn classify_connection_error(e: &ConnectionError) -> (String, bool) {
    use std::io::ErrorKind as K;

    fn io_kind(io: &std::io::Error) -> (String, bool) {
        // DNS failures surface as Uncategorized with a lookup message —
        // routine when the machine is offline.
        let msg = io.to_string();
        if msg.contains("lookup address") || msg.contains("dns error") {
            return ("dns_lookup_failed".to_string(), true);
        }
        let expected = matches!(
            io.kind(),
            K::ConnectionAborted
                | K::ConnectionReset
                | K::ConnectionRefused
                | K::TimedOut
                | K::BrokenPipe
                | K::NotConnected
                | K::UnexpectedEof
        );
        (format!("io_{:?}", io.kind()), expected)
    }

    match e {
        ConnectionError::Io(io) => io_kind(io),
        ConnectionError::MqttState(StateError::Io(io)) => io_kind(io),
        ConnectionError::MqttState(StateError::Deserialization(
            rumqttc::v5::mqttbytes::Error::Io(io),
        )) => io_kind(io),
        ConnectionError::MqttState(StateError::AwaitPingResp) => {
            ("await_ping_resp".to_string(), true)
        }
        ConnectionError::MqttState(StateError::ConnectionAborted) => {
            ("connection_aborted".to_string(), true)
        }
        ConnectionError::MqttState(StateError::ServerDisconnect { reason_code, .. }) => {
            (format!("server_disconnect_{:?}", reason_code), true)
        }
        ConnectionError::Timeout(_) => ("timeout".to_string(), true),
        ConnectionError::ConnectionRefused(code) => (format!("conn_refused_{:?}", code), true),
        _ => ("unhandled".to_string(), false),
    }
}

/// Logs a connection failure as exactly one line: WARN with a short kind for
/// routine churn, ERROR with full details for anything unexpected.
pub(crate) fn log_connection_failure(
    log_header: &str,
    scope: &str,
    transition: &str,
    e: &ConnectionError,
    retry_in_secs: u64,
) {
    let (kind, expected) = classify_connection_error(e);
    if let ConnectionError::ConnectionRefused(code) = e {
        use rumqttc::v5::mqttbytes::v5::ConnectReturnCode;
        if matches!(
            code,
            ConnectReturnCode::BadUserNamePassword | ConnectReturnCode::NotAuthorized
        ) {
            report_auth_rejection(log_header, &format!("{:?}", code));
        }
    }
    if expected {
        log::warn!(
            "{} [{}] state={} kind={} retry_in={}s",
            log_header,
            scope,
            transition,
            kind,
            retry_in_secs
        );
    } else {
        log::error!(
            "{} [{}] state={} kind={} err={:?} retry_in={}s",
            log_header,
            scope,
            transition,
            kind,
            e,
            retry_in_secs
        );
    }
}

/// Keep-alive for every MQTT connection the app opens. The broker drops a
/// silent client after roughly 1.5× this, and the client notices a half-open
/// link (no PINGRESP) after about twice this — so it bounds how long VU
/// requests can vanish into a dead link after a resume or a network switch.
pub(super) const MQTT_KEEP_ALIVE_SECS: u64 = 60;

/// The standard MQTT-over-TLS port. A host configured with it gets a TLS
/// transport: a plain CONNECT sent to a TLS listener fails forever.
const MQTT_TLS_PORT: u16 = 8883;

/// Capacity of the event loop's internal request channel.
const MQTT_CHANNEL_CAPACITY: usize = 10;

/// Builds an MQTT client and its event loop with the settings every connection
/// in the app shares: broker credentials from config and the common keep-alive.
///
/// Used by all four connection loops (the per-card APDU bridge, the app-level
/// connection, and the rack and rack-card loops) so they cannot drift apart on
/// how they authenticate or how quickly a dead link is detected. Callers log
/// the attempt themselves — each has its own identifiers to report.
pub(crate) fn build_mqtt_client(
    client_id: impl Into<String>,
    host: &str,
    port: u16,
) -> (AsyncClient, rumqttc::v5::EventLoop) {
    let mut mqtt_options = MqttOptions::new(client_id.into(), host, port);
    mqtt_options.set_keep_alive(Duration::from_secs(MQTT_KEEP_ALIVE_SECS));
    if port == MQTT_TLS_PORT {
        match tls_transport() {
            Some(transport) => {
                mqtt_options.set_transport(transport);
                apply_mqtt_credentials(&mut mqtt_options);
            }
            // The credentials stay home: on the TLS port they would otherwise
            // go out in clear text.
            None => log::error!(
                "[CONN] TLS port {} configured but TLS is unavailable: connecting without TLS and without credentials",
                port
            ),
        }
    } else {
        apply_mqtt_credentials(&mut mqtt_options);
    }
    // No Debug dump of mqtt_options anywhere: it would print the credentials.
    AsyncClient::new(mqtt_options, MQTT_CHANNEL_CAPACITY)
}

/// TLS transport with the system root certificates, built once and shared by
/// every connection. `None` when it cannot be built.
///
/// rumqttc's default config panics on any unreadable system certificate and
/// uses rustls' process-default crypto provider, which is ambiguous here (two
/// providers are compiled in) — so the provider is installed explicitly and
/// the build runs under `catch_unwind`.
fn tls_transport() -> Option<rumqttc::Transport> {
    static TRANSPORT: std::sync::OnceLock<Option<rumqttc::Transport>> = std::sync::OnceLock::new();
    TRANSPORT
        .get_or_init(|| {
            use rumqttc::tokio_rustls::rustls;
            // Err only when a provider is installed already — fine either way.
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            match std::panic::catch_unwind(rumqttc::Transport::tls_with_default_config) {
                Ok(transport) => Some(transport),
                Err(_) => {
                    log::error!(
                        "[CONN] building the TLS configuration failed (system certificates)"
                    );
                    None
                }
            }
        })
        .clone()
}

/// Applies the MQTT credentials to freshly built connection options — see
/// `resolved_credentials`. Shared by all four MQTT loops (app, per-card,
/// rack, rack-card) so authentication can never be enabled for one transport
/// and forgotten for another.
pub(crate) fn apply_mqtt_credentials(mqtt_options: &mut MqttOptions) {
    match resolved_credentials() {
        Some((username, password)) => {
            // The values themselves must never reach the log file.
            log::debug!("[CONN] mqtt credentials applied (username set)");
            mqtt_options.set_credentials(username, password);
        }
        None if get_from_cache(CacheSection::Server, "auth_enabled") == "true" => {
            log::warn!("[CONN] authentication is enabled but no credentials are set: connecting anonymously");
        }
        None => {}
    }
}

/// Last time the "authentication rejected" notification was raised: every
/// MQTT loop retries on its own schedule, and each retry is refused again —
/// one toast per minute is enough for the user to act on.
static AUTH_REJECTED_NOTIFIED: std::sync::Mutex<Option<std::time::Instant>> =
    std::sync::Mutex::new(None);

/// Surfaces a CONNACK that refused the credentials: an explicit log line and
/// a UI notification naming the cause, instead of the generic offline state.
fn report_auth_rejection(log_header: &str, code: &str) {
    let enabled = get_from_cache(CacheSection::Server, "auth_enabled") == "true";
    log::error!(
        "{} [CONN] status=auth_rejected code={} auth_enabled={}",
        log_header,
        code,
        enabled
    );
    let mut last = AUTH_REJECTED_NOTIFIED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if last.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
        return;
    }
    *last = Some(std::time::Instant::now());
    let message = if enabled {
        "The server rejected the authentication: the credentials are not set or invalid. Check Settings → Server authentication."
    } else {
        "The server requires authentication. Enable it in Settings → Server authentication and enter the credentials."
    };
    crate::global_app_handle::emit_notification_event(
        "global-notification",
        crate::global_app_handle::NotificationPayload {
            notification_type: "auth_rejected".to_string(),
            message: message.to_string(),
        },
    );
}
