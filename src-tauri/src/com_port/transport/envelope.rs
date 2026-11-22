//! The server-supplied command envelope: the raw command, its reply timings
//! and an opaque poll spec, parsed and bounded before anything reaches the
//! wire. The timing defaults and bounds of the envelope live here too.

use std::time::Duration;

use super::SERIAL_ERR_BAD_HEX;

/// Line silence that ends one serial reply, when the server does not supply `idle_ms`.
/// This is an *inter-byte* bound applied only after the reply has started — the wait for
/// the first byte is governed by `SERIAL_READ_DEADLINE` instead, because it scales with
/// the command size and the USB-serial adapter's latency.
pub(in crate::com_port) const SERIAL_REPLY_TIMEOUT: Duration = Duration::from_millis(800);

/// Hard deadline for the whole read phase of one command, and the budget for the
/// rack's first reply byte. The inter-byte timeout (`SERIAL_REPLY_TIMEOUT`) only fires
/// on a *silent* line — a device that keeps the line busy resets it on every byte, so
/// the loop also needs a total bound.
pub(in crate::com_port) const SERIAL_READ_DEADLINE: Duration = Duration::from_secs(5);

/// Poll spec defaults when the server omits the optional timing fields.
const POLL_INTERVAL_DEFAULT: Duration = Duration::from_millis(20);
const POLL_DEADLINE_DEFAULT: Duration = Duration::from_secs(5);

/// Upper bound for every server-supplied timing field (`idle_ms`, `deadline_ms`,
/// `interval_ms`). An unclamped u64 would panic on `Instant + Duration` overflow
/// after the command bytes were already written to the device, and a huge poll
/// interval would pin a blocking-pool thread (and the port lock) beyond any
/// abort — `spawn_blocking` closures cannot be cancelled.
pub(in crate::com_port) const SERIAL_MS_MAX: u64 = 300_000;

/// Lower bound for server-supplied *interval* fields (`interval_ms` of the
/// watch and of a poll spec). Without a floor, `interval_ms: 0` turns the
/// watch/poll loop into a busy loop that hammers the wire and monopolises the
/// port lock, starving every card session behind it.
pub(in crate::com_port) const SERIAL_MS_MIN: u64 = 20;

/// Server-scripted poll loop of one envelope: after the command is accepted, keep sending
/// `cmd` every `interval` while the device answers exactly `while_hex`; the first differing
/// reply is the operation result. Pure byte comparison - no protocol knowledge on this side.
#[derive(Debug)]
pub(in crate::com_port) struct PollSpec {
    pub(in crate::com_port) cmd_hex: String,
    pub(in crate::com_port) while_hex: String,
    pub(in crate::com_port) interval: Duration,
    pub(in crate::com_port) deadline: Duration,
}

/// One server -> TBA serial exchange envelope: the raw command plus optional reply timings and
/// an opaque poll spec. All hex strings are normalized to uppercase at parse time so later
/// comparisons are plain string equality.
#[derive(Debug)]
pub(in crate::com_port) struct SerialEnvelope {
    pub(in crate::com_port) cmd_hex: String,
    /// Predicted "accepted" first reply; any other first reply is returned to the server as is.
    pub(in crate::com_port) expect_hex: Option<String>,
    /// Line-silence interval that ends a reply already in flight.
    pub(in crate::com_port) idle: Duration,
    /// Hard bound of the read phase of one exchange, first reply byte included.
    pub(in crate::com_port) deadline: Duration,
    pub(in crate::com_port) poll: Option<PollSpec>,
    /// End-of-authentication marker, same semantics as the `finish` flag of the
    /// PC/SC path: `false` on every command of an ongoing session, `true` on the
    /// closing message the server sends (with an empty `serial_cmd`) once the
    /// tracker reports the authentication finished. Session envelopes always
    /// carry it; `None` marks non-session signalling on the same serial path
    /// (e.g. a slot indicator repaint) — see `run_serial_request`.
    pub(in crate::com_port) finish: Option<bool>,
}

/// Validates and uppercases a hex string; the error is the wire contract code.
pub(in crate::com_port) fn normalize_hex(s: &str) -> Result<String, &'static str> {
    hex::decode(s)
        .map(hex::encode_upper)
        .map_err(|_| SERIAL_ERR_BAD_HEX)
}

/// Parses the envelope from a request payload. `None` when there is no `serial_cmd` field at
/// all (not an envelope); `Some(Err(code))` when the envelope is malformed - the caller still
/// publishes a response with that code (the always-reply contract).
pub(in crate::com_port) fn parse_envelope(
    json: &serde_json::Value,
) -> Option<Result<SerialEnvelope, &'static str>> {
    let cmd = json.get("serial_cmd").and_then(|v| v.as_str())?;
    Some(parse_envelope_fields(json, cmd))
}

/// A server-supplied timing field in milliseconds, capped at `SERIAL_MS_MAX`.
pub(in crate::com_port) fn ms_field(json: &serde_json::Value, key: &str) -> Option<u64> {
    json.get(key)
        .and_then(|v| v.as_u64())
        .map(|v| v.min(SERIAL_MS_MAX))
}

/// The envelope fields around its command `cmd`, each validated and bounded.
fn parse_envelope_fields(
    json: &serde_json::Value,
    cmd: &str,
) -> Result<SerialEnvelope, &'static str> {
    let expect_hex = match json.get("expect").and_then(|v| v.as_str()) {
        Some(s) => Some(normalize_hex(s)?),
        None => None,
    };
    let poll = match json.get("poll") {
        Some(p) => Some(parse_poll_spec(p)?),
        None => None,
    };
    Ok(SerialEnvelope {
        cmd_hex: normalize_hex(cmd)?,
        expect_hex,
        // Floored: a zero silence bound ends the read on the first chunk, and
        // a zero deadline writes a command whose reply is never read.
        idle: ms_field(json, "idle_ms")
            .map(|ms| Duration::from_millis(ms.max(SERIAL_MS_MIN)))
            .unwrap_or(SERIAL_REPLY_TIMEOUT),
        deadline: ms_field(json, "deadline_ms")
            .map(|ms| Duration::from_millis(ms.max(SERIAL_MS_MIN)))
            .unwrap_or(SERIAL_READ_DEADLINE),
        poll,
        finish: json.get("finish").and_then(|v| v.as_bool()),
    })
}

/// A poll spec without its command or "while" bytes is a malformed envelope.
fn parse_poll_spec(p: &serde_json::Value) -> Result<PollSpec, &'static str> {
    let poll_cmd = p
        .get("cmd")
        .and_then(|v| v.as_str())
        .ok_or(SERIAL_ERR_BAD_HEX)?;
    let poll_while = p
        .get("while")
        .and_then(|v| v.as_str())
        .ok_or(SERIAL_ERR_BAD_HEX)?;
    Ok(PollSpec {
        cmd_hex: normalize_hex(poll_cmd)?,
        while_hex: normalize_hex(poll_while)?,
        interval: ms_field(p, "interval_ms")
            .map(|v| Duration::from_millis(v.max(SERIAL_MS_MIN)))
            .unwrap_or(POLL_INTERVAL_DEFAULT),
        deadline: ms_field(p, "deadline_ms")
            .map(Duration::from_millis)
            .unwrap_or(POLL_DEADLINE_DEFAULT),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Envelope parsing (poll primitive contract) ──
    // Only synthetic placeholder bytes here: the device wire protocol is owned
    // by the server and must never appear in this repo, not even in tests.

    #[test]
    fn envelope_without_serial_cmd_is_not_an_envelope() {
        let json: serde_json::Value = serde_json::from_str(r#"{"connect":{}}"#).unwrap();
        assert!(parse_envelope(&json).is_none());
    }

    #[test]
    fn envelope_reads_finish_flag_from_an_in_session_payload() {
        // The shape of an in-session envelope; the byte strings are placeholders.
        let json: serde_json::Value = serde_json::from_str(
            r#"{"deadline_ms":500,"expect":"AC01","finish":false,"idle_ms":50,"poll":{"cmd":"5701","deadline_ms":5000,"interval_ms":20,"while":"B501"},"serial_cmd":"C0DE01"}"#,
        )
        .unwrap();
        let env = parse_envelope(&json)
            .expect("is an envelope")
            .expect("parses");
        assert_eq!(
            env.finish,
            Some(false),
            "an in-session command carries finish:false"
        );
        assert_eq!(env.cmd_hex, "C0DE01");
        assert_eq!(env.expect_hex.as_deref(), Some("AC01"));
        assert_eq!(env.idle, Duration::from_millis(50));
        assert!(env.poll.is_some());
    }

    #[test]
    fn closing_envelope_carries_finish_true_and_no_command() {
        // End of the session: same shape, empty APDU, finish:true.
        let json: serde_json::Value =
            serde_json::from_str(r#"{"serial_cmd":"","finish":true}"#).unwrap();
        let env = parse_envelope(&json)
            .expect("is an envelope")
            .expect("parses");
        assert_eq!(env.finish, Some(true));
        assert!(
            env.cmd_hex.is_empty(),
            "nothing to put on the wire for the closing message"
        );
    }

    #[test]
    fn envelope_without_finish_is_backward_compatible() {
        // Older server: no flag at all — the idle-timer fallback takes over.
        let json: serde_json::Value = serde_json::from_str(r#"{"serial_cmd":"C0DE01"}"#).unwrap();
        let env = parse_envelope(&json)
            .expect("is an envelope")
            .expect("parses");
        assert_eq!(env.finish, None);
    }

    #[test]
    fn envelope_bad_hex_reports_contract_code() {
        let json: serde_json::Value = serde_json::from_str(r#"{"serial_cmd":"ZZ"}"#).unwrap();
        assert_eq!(
            parse_envelope(&json).unwrap().unwrap_err(),
            SERIAL_ERR_BAD_HEX
        );
        // bad hex inside the poll spec is just as malformed
        let json: serde_json::Value =
            serde_json::from_str(r#"{"serial_cmd":"AB","poll":{"cmd":"AB","while":"XX"}}"#)
                .unwrap();
        assert_eq!(
            parse_envelope(&json).unwrap().unwrap_err(),
            SERIAL_ERR_BAD_HEX
        );
    }

    #[test]
    fn envelope_poll_without_bytes_is_malformed() {
        let json: serde_json::Value =
            serde_json::from_str(r#"{"serial_cmd":"AB","poll":{"interval_ms":20}}"#).unwrap();
        assert_eq!(
            parse_envelope(&json).unwrap().unwrap_err(),
            SERIAL_ERR_BAD_HEX
        );
    }
}
