//! The wire contract of a card session: topics, request ids, reply payloads,
//! and the server's answer to the status question.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rumqttc::v5::AsyncClient;
use tauri::async_runtime;

/// Rewrites a `request/...` topic to its matching `response/...` topic.
/// Replaces only the leading segment so substrings deeper in the topic
/// (in client_id, parcel id, etc.) cannot be accidentally mangled.
/// Shared with the rack path in `com_port/` — one wire contract, one parser.
pub(crate) fn request_to_response_topic(topic: &str) -> String {
    if let Some(rest) = topic.strip_prefix("request/") {
        format!("response/{}", rest)
    } else {
        topic.to_string()
    }
}

/// Extracts the request id (the first segment after `request/`) from a topic of
/// the form `request/<id>/<sender>`. Used for idempotent handling of repeated
/// requests: the server re-sends the same id when it does not get a timely reply.
pub(crate) fn request_id_from_topic(topic: &str) -> Option<u64> {
    topic
        .strip_prefix("request/")
        .and_then(|rest| rest.split('/').next())
        .and_then(|id| id.parse::<u64>().ok())
}

/// First pause before a card the server refused reconnects to ask again; it
/// doubles per refusal in a row up to `UNSERVED_RETRY_MAX_SECS`.
const UNSERVED_RETRY_INITIAL_SECS: u64 = 60;
const UNSERVED_RETRY_MAX_SECS: u64 = 300;

/// The pause before the reconnect that follows the `refusals`-th refusal in a row.
pub(crate) fn unserved_retry_secs(refusals: u32) -> u64 {
    UNSERVED_RETRY_INITIAL_SECS
        .saturating_mul(1 << refusals.saturating_sub(1).min(8))
        .min(UNSERVED_RETRY_MAX_SECS)
}

/// The server's answers to the status question of one card session, and the
/// reconnect that asks again after a refusal. The server binds a connection to
/// its device at connect only, so a device created meanwhile is picked up on a
/// new connection. Shared by the reader and the rack card sessions.
pub(crate) struct StatusAnswers {
    /// Refusals in a row, for the retry pause.
    refusals: u32,
    /// Bumped on every answer: a retry scheduled after one answer must not fire
    /// once a later answer (a `served` after a repeated report) settled it.
    answers: std::sync::Arc<AtomicU64>,
}

impl StatusAnswers {
    pub(crate) fn new() -> Self {
        StatusAnswers {
            refusals: 0,
            answers: std::sync::Arc::new(AtomicU64::new(0)),
        }
    }

    /// Records a `served` answer: a pending retry is dropped.
    pub(crate) fn served(&mut self) {
        self.refusals = 0;
        self.answers.fetch_add(1, Ordering::SeqCst);
    }

    /// Records a refusal and schedules the reconnect that asks again; returns
    /// the pause before it. The retry is skipped when, by then, the connection
    /// it was asked on is gone, a later answer arrived, or `still_wanted` says
    /// the session is over. `requery`, when given, is raised right before the
    /// DISCONNECT so the session loop can tell it from a teardown.
    pub(crate) fn refused(
        &mut self,
        client: &AsyncClient,
        connection: &std::sync::Arc<AtomicU64>,
        requery: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        still_wanted: impl Fn() -> bool + Send + 'static,
        log_header: &str,
    ) -> u64 {
        self.refusals = self.refusals.saturating_add(1);
        let retry_secs = unserved_retry_secs(self.refusals);
        let answer = self.answers.fetch_add(1, Ordering::SeqCst) + 1;
        let answers = self.answers.clone();
        let client = client.clone();
        let connection = connection.clone();
        let asked_on = connection.load(Ordering::SeqCst);
        let log_header = log_header.to_string();
        async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_secs(retry_secs)).await;
            if connection.load(Ordering::SeqCst) != asked_on
                || answers.load(Ordering::SeqCst) != answer
                || !still_wanted()
            {
                return;
            }
            log::info!(
                "{} [CONN] status=reconnecting reason=not_served",
                log_header
            );
            if let Some(flag) = &requery {
                flag.store(true, Ordering::SeqCst);
            }
            if let Err(e) = client.try_disconnect() {
                if let Some(flag) = &requery {
                    flag.store(false, Ordering::SeqCst);
                }
                log::warn!("{} [CONN] status=reconnect_failed err={:?}", log_header, e);
            }
        });
        retry_secs
    }
}

/// The server's answer to the status question of a card session (the rack
/// link report, or the `status` publish of a reader card), `{"served":true}` or
/// `{"served":false,"reason":"not_registered"}`: `None` for a served card,
/// else the reason. An answer that cannot be read counts as a refusal, so a
/// slot never shows as served on anything but an explicit confirmation.
pub(crate) fn card_status(payload: &[u8]) -> Option<String> {
    let json: serde_json::Value = serde_json::from_slice(payload).unwrap_or_default();
    if json.get("served").and_then(serde_json::Value::as_bool) == Some(true) {
        return None;
    }
    Some(
        json.get("reason")
            .and_then(serde_json::Value::as_str)
            .filter(|reason| !reason.is_empty())
            .unwrap_or("not_served")
            .to_string(),
    )
}

pub(super) fn process_rapdu_mqtt_hex(rapdu_mqtt_hex: String) -> String {
    // Create a JSON object with the hex value
    serde_json::json!({
        "payload": rapdu_mqtt_hex,
    })
    // Serialize the JSON object to a string and assign it to `payload_ack`
    .to_string()
}

// Session-start (ATR) reply: the payload plus the actual T protocol the card is opened with.
// Old servers ignore the extra field, new ones seed their per-tracker protocol value from it.
pub(super) fn process_session_start_mqtt_hex(atr_hex: String, protocol: &str) -> String {
    serde_json::json!({
        "payload": atr_hex,
        "protocol": protocol,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_session_start_mqtt_hex_reports_payload_and_protocol() {
        let json = process_session_start_mqtt_hex("3BDB96FF".to_string(), "T1");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(
            parsed.get("payload").and_then(|v| v.as_str()),
            Some("3BDB96FF")
        );
        assert_eq!(parsed.get("protocol").and_then(|v| v.as_str()), Some("T1"));
    }

    #[test]
    fn process_rapdu_mqtt_hex_has_no_protocol_field() {
        // mid-session replies must stay in the old shape - only session start reports the protocol
        let parsed: serde_json::Value =
            serde_json::from_str(&process_rapdu_mqtt_hex("9000".to_string())).expect("valid json");
        assert!(parsed.get("protocol").is_none());
    }

    #[test]
    fn request_to_response_topic_swaps_prefix_only() {
        assert_eq!(
            request_to_response_topic("request/1/ABCDEF0123456789"),
            "response/1/ABCDEF0123456789"
        );
    }

    #[test]
    fn request_to_response_topic_does_not_touch_inner_substrings() {
        // The literal substring "request" appears inside the payload-id segment;
        // the prefix-only swap must leave it intact.
        assert_eq!(
            request_to_response_topic("request/1/clientrequest-99"),
            "response/1/clientrequest-99"
        );
    }

    #[test]
    fn request_to_response_topic_returns_unchanged_when_no_prefix() {
        assert_eq!(
            request_to_response_topic("status/1/ABCDEF"),
            "status/1/ABCDEF"
        );
    }

    #[test]
    fn request_id_from_topic_parses_first_segment_only() {
        assert_eq!(
            request_id_from_topic("request/354/0000000067664100"),
            Some(354)
        );
        assert_eq!(request_id_from_topic("request/1/ABCDEF"), Some(1));
    }

    #[test]
    fn request_id_from_topic_rejects_non_numeric_or_wrong_prefix() {
        assert_eq!(request_id_from_topic("request/abc/ABCDEF"), None);
        assert_eq!(request_id_from_topic("response/1/ABCDEF"), None);
        assert_eq!(request_id_from_topic("request/"), None);
    }

    #[test]
    fn process_rapdu_mqtt_hex_emits_payload_field() {
        let json = process_rapdu_mqtt_hex("DEADBEEF".to_string());
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(
            parsed.get("payload").and_then(|v| v.as_str()),
            Some("DEADBEEF")
        );
    }
}
