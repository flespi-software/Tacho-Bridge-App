//! Reporting TBA settings to the server.
//!
//! Right after the app connection is established, TBA publishes a one-shot
//! settings report so the server can populate the read-only device settings.
//! The payload is a JSON object keyed by setting name (currently only
//! `app_info`), so more settings can be reported later without changing the
//! topic or the format.

use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::AsyncClient;

/// Topic the settings report is published to.
const SETTINGS_TOPIC: &str = "settings";

/// Builds the settings report payload: an object keyed by setting name.
fn settings_report_payload() -> String {
    serde_json::json!({
        "app_info": {
            "version": env!("CARGO_PKG_VERSION"),
            "os": sys_info::os_type().unwrap_or_else(|_| "Unknown".to_string()),
            "os_release": sys_info::os_release().unwrap_or_else(|_| "Unknown".to_string()),
            "arch": std::env::consts::ARCH,
        }
    })
    .to_string()
}

/// Publishes the settings report on the given (app) connection. Called once
/// per established connection (on CONNACK): the server tracks the values per
/// connection, so every reconnect gets a fresh report.
pub async fn publish_settings_report(client: &AsyncClient, log_header: &str) {
    let payload = settings_report_payload();
    match client
        .publish(SETTINGS_TOPIC, QoS::AtLeastOnce, false, payload)
        .await
    {
        Ok(()) => log::info!("{} [SETTINGS] status=report_published", log_header),
        Err(e) => log::error!("{} [SETTINGS] status=report_failed err={:?}", log_header, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_report_payload_is_valid_and_complete() {
        let report: serde_json::Value =
            serde_json::from_str(&settings_report_payload()).expect("report must be valid JSON");
        let app_info = &report["app_info"];
        assert_eq!(app_info["version"], env!("CARGO_PKG_VERSION"));
        assert!(!app_info["os"].as_str().expect("os is a string").is_empty());
        assert!(!app_info["os_release"].as_str().expect("os_release is a string").is_empty());
        assert!(!app_info["arch"].as_str().expect("arch is a string").is_empty());
    }
}
