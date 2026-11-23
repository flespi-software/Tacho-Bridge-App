//! Serial transport for the card rack: the wire itself.
//!
//! Everything that touches the COM port lives here — framing timings, the
//! read/write loop, and the server-supplied command envelope. This module is
//! deliberately protocol-agnostic: it moves opaque bytes and never interprets
//! them — the client is a dumb pipe.
//!
//! Layout:
//! - `health` — per-port fault counters, released ports, unsolicited bytes
//! - `envelope` — the server's command envelope: parsing, timing bounds
//! - `wire` — one read and one write+read exchange on a locked port, line resync
//! - `operation` — one logical operation: the exchange, the poll loop, the port queue
//! - `lease` — an operation retained across server round trips
//! - `shutdown` — the final indicator commands once regular rack work stopped

mod envelope;
mod health;
mod lease;
mod operation;
mod shutdown;
mod wire;

#[cfg(test)]
mod test_port;

use std::sync::Arc;

use serialport::SerialPort;
use tokio::sync::Mutex as AsyncMutex;

// Re-exported for the rest of `com_port`, which reaches every item of the
// transport as `transport::X`.
pub(super) use envelope::{
    ms_field, normalize_hex, parse_envelope, PollSpec, SerialEnvelope, SERIAL_MS_MAX,
    SERIAL_MS_MIN, SERIAL_READ_DEADLINE, SERIAL_REPLY_TIMEOUT,
};
pub(super) use health::{
    port_failed_ops, port_key, release_port, take_unsolicited, track_port, untrack_port,
};
pub(super) use lease::SerialLease;
pub(super) use operation::{execute_envelope, execute_envelope_timed};
pub(super) use shutdown::{write_shutdown_commands, ShutdownCommand};
pub(super) use wire::ReplyComplete;

/// An open rack port, shared by every session of the rack; the async lock is
/// the per-port queue of operations.
pub(super) type SharedPort = Arc<AsyncMutex<Box<dyn SerialPort>>>;

/// `serial_err` codes of the response contract. The response envelope is
/// published for EVERY request; on success the code is an empty string. The
/// server is the only consumer — codes are part of the wire contract, do not
/// rename them.
pub(super) const SERIAL_ERR_NO_REPLY: &str = "no_reply";
pub(super) const SERIAL_ERR_WRITE_FAILED: &str = "write_failed";
pub(super) const SERIAL_ERR_BAD_HEX: &str = "bad_hex";
pub(super) const SERIAL_ERR_TRUNCATED: &str = "truncated";
pub(super) const SERIAL_ERR_BAD_JSON: &str = "bad_json";
pub(super) const SERIAL_ERR_BAD_REQUEST_ID: &str = "bad_request_id";
pub(super) const SERIAL_ERR_REQUEST_CONFLICT: &str = "request_conflict";
pub(super) const SERIAL_ERR_STALE_REQUEST: &str = "stale_request";
pub(super) const SERIAL_ERR_CARD_BUSY: &str = "card_busy";
/// Codes of the retained-operation controls (`serial_hold_ms`, `serial_continue`, `serial_release`).
pub(super) const SERIAL_ERR_BAD_CONTROL: &str = "bad_serial_control";
pub(super) const SERIAL_ERR_TRANSACTION_EXPIRED: &str = "serial_transaction_expired";
pub(super) const SERIAL_ERR_TRANSACTION_ACTIVE: &str = "serial_transaction_active";
pub(super) const SERIAL_ERR_QUEUE_TIMEOUT: &str = "serial_queue_timeout";

/// Outcome of one server command → rack exchange, mirroring the response
/// envelope: `resp_hex` carries whatever bytes came back (possibly empty, or
/// partial on truncation), `err` is `""` on success or one of the
/// `SERIAL_ERR_*` codes. Published for every request — the app is the server's
/// only feedback channel, so "rack stayed silent" must be distinguishable from
/// "message lost".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SerialExchange {
    pub(super) resp_hex: String,
    pub(super) err: &'static str,
}

impl SerialExchange {
    /// Successful exchange: the rack answered with these bytes.
    pub(super) fn ok(resp_hex: String) -> Self {
        Self { resp_hex, err: "" }
    }

    /// Failed exchange with no data to return.
    pub(super) fn error(err: &'static str) -> Self {
        Self {
            resp_hex: String::new(),
            err,
        }
    }

    /// True when the exchange fully succeeded (the only cacheable outcome).
    pub(super) fn is_ok(&self) -> bool {
        self.err.is_empty()
    }

    /// JSON payload for the response topic: always the same two fields, so the
    /// server-side parser never has to branch on the structure.
    pub(super) fn to_payload(&self) -> String {
        serde_json::json!({ "serial_resp": self.resp_hex, "serial_err": self.err }).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Response envelope (contract v2): both fields always present, one shape ──

    /// Parses a published payload and returns (serial_resp, serial_err).
    fn parse_payload(payload: &str) -> (String, String) {
        let v: serde_json::Value = serde_json::from_str(payload).expect("payload must be JSON");
        let obj = v.as_object().expect("payload must be an object");
        assert_eq!(
            obj.len(),
            2,
            "envelope must have exactly the two contract fields"
        );
        (
            obj["serial_resp"]
                .as_str()
                .expect("serial_resp must be a string")
                .to_string(),
            obj["serial_err"]
                .as_str()
                .expect("serial_err must be a string")
                .to_string(),
        )
    }

    #[test]
    fn exchange_payload_rack_replied() {
        // synthetic bytes — not a real device reply
        let p = SerialExchange::ok("A1B2C3D4".into()).to_payload();
        assert_eq!(parse_payload(&p), ("A1B2C3D4".to_string(), "".to_string()));
    }

    #[test]
    fn exchange_payload_no_reply() {
        let p = SerialExchange::error(SERIAL_ERR_NO_REPLY).to_payload();
        assert_eq!(parse_payload(&p), ("".to_string(), "no_reply".to_string()));
    }

    #[test]
    fn exchange_payload_write_failed() {
        let p = SerialExchange::error(SERIAL_ERR_WRITE_FAILED).to_payload();
        assert_eq!(
            parse_payload(&p),
            ("".to_string(), "write_failed".to_string())
        );
    }

    #[test]
    fn exchange_payload_bad_hex() {
        let p = SerialExchange::error(SERIAL_ERR_BAD_HEX).to_payload();
        assert_eq!(parse_payload(&p), ("".to_string(), "bad_hex".to_string()));
    }

    #[test]
    fn exchange_payload_truncated_keeps_partial_data() {
        // Truncation carries BOTH the partial hex and the error code.
        let p = SerialExchange {
            resp_hex: "A1B2C3".into(),
            err: SERIAL_ERR_TRUNCATED,
        }
        .to_payload();
        assert_eq!(
            parse_payload(&p),
            ("A1B2C3".to_string(), "truncated".to_string())
        );
    }

    #[test]
    fn exchange_only_success_is_cacheable() {
        // Idempotency contract: cache only fully successful exchanges.
        assert!(SerialExchange::ok("AA".into()).is_ok());
        assert!(!SerialExchange::error(SERIAL_ERR_NO_REPLY).is_ok());
        assert!(!SerialExchange {
            resp_hex: "AA".into(),
            err: SERIAL_ERR_TRUNCATED
        }
        .is_ok());
    }
}
