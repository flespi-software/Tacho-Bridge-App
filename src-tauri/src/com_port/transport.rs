//! Serial transport for the card rack: the wire itself.
//!
//! Everything that touches the COM port lives here — framing timings, the
//! read/write loop, and the server-supplied command envelope. This module is
//! deliberately protocol-agnostic: it moves opaque bytes and never interprets
//! them (see `docs/hyper-card-protocol.md` §11a — the client is a dumb pipe).

use std::io::{Read, Write};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serialport::SerialPort;
use tokio::sync::Mutex as AsyncMutex;

use super::SHUTTING_DOWN;

pub(super) type SharedPort = Arc<AsyncMutex<Box<dyn SerialPort>>>;

/// Line silence that ends one serial reply, when the server does not supply `idle_ms`.
/// This is an *inter-byte* bound applied only after the reply has started — the wait for
/// the first byte is governed by `SERIAL_READ_DEADLINE` instead, because it scales with
/// the command size and the USB-serial adapter's latency.
pub(super) const SERIAL_REPLY_TIMEOUT: Duration = Duration::from_millis(800);

/// Upper bound on a single serial reply. A healthy rack answers with small
/// frames; hitting this means the device is streaming garbage. Without the cap
/// a device that never stops sending would grow the buffer without bound and
/// keep the read loop (and the port lock) stuck forever.
const SERIAL_REPLY_MAX_BYTES: usize = 64 * 1024;

/// Hard deadline for the whole read phase of one command, and the budget for the
/// rack's first reply byte. The inter-byte timeout (`SERIAL_REPLY_TIMEOUT`) only fires
/// on a *silent* line — a device that keeps the line busy resets it on every byte, so
/// the loop also needs a total bound.
pub(super) const SERIAL_READ_DEADLINE: Duration = Duration::from_secs(5);

/// `serial_err` codes of the v2 response contract. The response envelope is
/// published for EVERY request; on success the code is an empty string. The
/// server is the only consumer — codes are part of the wire contract, do not
/// rename them.
const SERIAL_ERR_NO_REPLY: &str = "no_reply";
const SERIAL_ERR_WRITE_FAILED: &str = "write_failed";
const SERIAL_ERR_BAD_HEX: &str = "bad_hex";
const SERIAL_ERR_TRUNCATED: &str = "truncated";

/// Outcome of one server command → rack exchange, mirroring the v2 response
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

/// Poll spec defaults when the server omits the optional timing fields.
const POLL_INTERVAL_DEFAULT: Duration = Duration::from_millis(20);
const POLL_DEADLINE_DEFAULT: Duration = Duration::from_secs(5);

/// Upper bound for every server-supplied timing field (`idle_ms`, `deadline_ms`,
/// `interval_ms`). An unclamped u64 would panic on `Instant + Duration` overflow
/// after the command bytes were already written to the device, and a huge poll
/// interval would pin a blocking-pool thread (and the port lock) beyond any
/// abort — `spawn_blocking` closures cannot be cancelled.
pub(super) const SERIAL_MS_MAX: u64 = 300_000;

/// Lower bound for server-supplied *interval* fields (`interval_ms` of the
/// watch and of a poll spec). Without a floor, `interval_ms: 0` turns the
/// watch/poll loop into a busy loop that hammers the wire and monopolises the
/// port lock, starving every card session behind it.
pub(super) const SERIAL_MS_MIN: u64 = 20;

/// One blocking `port.read` never waits longer than this slice, whatever the
/// server-supplied timings say: the read loop re-checks its deadlines and the
/// app shutdown flag between slices. This is what keeps the uncancellable
/// `spawn_blocking` serial closures from pinning the port (and a blocking-pool
/// thread) for up to `SERIAL_MS_MAX` after the app started closing.
const SERIAL_READ_SLICE: Duration = Duration::from_millis(500);

/// Server-scripted poll loop of one envelope: after the command is accepted, keep sending
/// `cmd` every `interval` while the device answers exactly `while_hex`; the first differing
/// reply is the operation result. Pure byte comparison - no protocol knowledge on this side.
#[derive(Debug)]
pub(super) struct PollSpec {
    pub(super) cmd_hex: String,
    pub(super) while_hex: String,
    pub(super) interval: Duration,
    pub(super) deadline: Duration,
}

/// One server -> TBA serial exchange envelope: the raw command plus optional reply timings and
/// an opaque poll spec. All hex strings are normalized to uppercase at parse time so later
/// comparisons are plain string equality.
#[derive(Debug)]
pub(super) struct SerialEnvelope {
    pub(super) cmd_hex: String,
    /// Predicted "accepted" first reply; any other first reply is returned to the server as is.
    pub(super) expect_hex: Option<String>,
    /// Line-silence interval that ends a reply already in flight.
    pub(super) idle: Duration,
    /// Hard bound of the read phase of one exchange, first reply byte included.
    pub(super) deadline: Duration,
    pub(super) poll: Option<PollSpec>,
    /// End-of-authentication marker, same semantics as the `finish` flag of the
    /// PC/SC path: `false` on every command of an ongoing session, `true` on the
    /// closing message the server sends (with an empty `serial_cmd`) once the
    /// tracker reports the authentication finished. Session envelopes always
    /// carry it; `None` marks non-session signalling on the same serial path
    /// (e.g. a slot LED repaint) — see `handle_serial_request`.
    pub(super) finish: Option<bool>,
}

/// Validates and uppercases a hex string; the error is the wire contract code.
pub(super) fn normalize_hex(s: &str) -> Result<String, &'static str> {
    hex::decode(s)
        .map(hex::encode_upper)
        .map_err(|_| SERIAL_ERR_BAD_HEX)
}

/// Parses the envelope from a request payload. `None` when there is no `serial_cmd` field at
/// all (not an envelope); `Some(Err(code))` when the envelope is malformed - the caller still
/// publishes a response with that code (the always-reply contract).
pub(super) fn parse_envelope(
    json: &serde_json::Value,
) -> Option<Result<SerialEnvelope, &'static str>> {
    let cmd = json.get("serial_cmd").and_then(|v| v.as_str())?;
    Some(parse_envelope_fields(json, cmd))
}

fn parse_envelope_fields(
    json: &serde_json::Value,
    cmd: &str,
) -> Result<SerialEnvelope, &'static str> {
    let ms = |v: &serde_json::Value, key: &str| {
        v.get(key)
            .and_then(|x| x.as_u64())
            .map(|x| x.min(SERIAL_MS_MAX))
    };

    let expect_hex = match json.get("expect").and_then(|v| v.as_str()) {
        Some(s) => Some(normalize_hex(s)?),
        None => None,
    };
    let poll = match json.get("poll") {
        Some(p) => {
            // a poll spec without its command/while bytes is a malformed envelope
            let poll_cmd = p
                .get("cmd")
                .and_then(|v| v.as_str())
                .ok_or(SERIAL_ERR_BAD_HEX)?;
            let poll_while = p
                .get("while")
                .and_then(|v| v.as_str())
                .ok_or(SERIAL_ERR_BAD_HEX)?;
            Some(PollSpec {
                cmd_hex: normalize_hex(poll_cmd)?,
                while_hex: normalize_hex(poll_while)?,
                interval: ms(p, "interval_ms")
                    .map(|v| Duration::from_millis(v.max(SERIAL_MS_MIN)))
                    .unwrap_or(POLL_INTERVAL_DEFAULT),
                deadline: ms(p, "deadline_ms")
                    .map(Duration::from_millis)
                    .unwrap_or(POLL_DEADLINE_DEFAULT),
            })
        }
        None => None,
    };
    Ok(SerialEnvelope {
        cmd_hex: normalize_hex(cmd)?,
        expect_hex,
        idle: ms(json, "idle_ms")
            .map(Duration::from_millis)
            .unwrap_or(SERIAL_REPLY_TIMEOUT),
        deadline: ms(json, "deadline_ms")
            .map(Duration::from_millis)
            .unwrap_or(SERIAL_READ_DEADLINE),
        poll,
        finish: json.get("finish").and_then(|v| v.as_bool()),
    })
}

/// Initial / capped reconnect backoff for the rack's MQTT connection — same
/// policy as the app and per-card connections.
fn drain_buffered(port: &mut Box<dyn SerialPort>, log_header: &str) -> Vec<u8> {
    let pending = match port.bytes_to_read() {
        Ok(0) => return Vec::new(),
        Ok(n) => (n as usize).min(SERIAL_REPLY_MAX_BYTES),
        Err(e) => {
            log::warn!("{} [SERIAL] bytes_to_read failed: {}", log_header, e);
            return Vec::new();
        }
    };
    let mut buf = vec![0u8; pending];
    match port.read(&mut buf) {
        Ok(n) => {
            buf.truncate(n);
            buf
        }
        Err(e) => {
            log::warn!("{} [SERIAL] drain read failed: {}", log_header, e);
            Vec::new()
        }
    }
}

/// Reads one reply off the port. Two timings are involved and they are NOT the same order
/// of magnitude:
///
///   * time to the FIRST byte (`first_wait`) — after a write, the rack has to receive the
///     whole command before it starts answering, so this scales with the command size (a
///     210-byte frame is ~18 ms of wire time at 115200 on its own) and carries the
///     USB-serial adapter's latency on top.
///   * gap BETWEEN bytes of a reply already in flight — that is `idle`, the line silence
///     that marks the end of the reply. Tens of milliseconds.
///
/// Using `idle` for both is what made every large command come back `no_reply` while short
/// ones went through: the reply was on its way, we just stopped listening. So the port
/// timeout starts at `first_wait` and drops to `idle` as soon as the first bytes land — a
/// non-empty `carry` (bytes the rack already pushed) means the reply has started, so the
/// silence bound applies from the very first read.
///
/// Two hard bounds protect against a misbehaving device that streams bytes continuously
/// (each read would then succeed before the timeout and the loop would never exit): a cap
/// on the reply size and `deadline` on the whole read phase. Returns the bytes and whether
/// the size cap truncated them.
fn read_reply(
    port: &mut Box<dyn SerialPort>,
    carry: Vec<u8>,
    first_wait: Duration,
    idle: Duration,
    deadline: Duration,
    log_header: &str,
) -> (Vec<u8>, bool) {
    let mut reply = carry;
    let mut first_byte_pending = reply.is_empty();

    let mut buf = [0u8; 512];
    let mut truncated = false;
    let read_started = std::time::Instant::now();
    // the total bound must never undercut the first-byte budget it contains
    let total = if deadline > first_wait {
        deadline
    } else {
        first_wait
    };
    let read_deadline = read_started + total;
    // The silence bound that ends the reply: first-byte budget while nothing
    // has arrived yet, line-idle from the last received byte afterwards.
    let mut silence_deadline = read_started + if first_byte_pending { first_wait } else { idle };
    loop {
        if reply.len() >= SERIAL_REPLY_MAX_BYTES {
            log::warn!(
                "{} [SERIAL] reply exceeded {} bytes — truncating, device is misbehaving",
                log_header,
                SERIAL_REPLY_MAX_BYTES
            );
            truncated = true;
            break;
        }
        let now = std::time::Instant::now();
        if now >= read_deadline {
            log::warn!(
                "{} [SERIAL] read deadline {:?} reached — returning {} bytes read so far",
                log_header,
                total,
                reply.len()
            );
            break;
        }
        // App is closing: stop waiting so the blocking closure releases the
        // port lock promptly — `spawn_blocking` cannot be aborted from outside.
        if SHUTTING_DOWN.load(Ordering::SeqCst) {
            log::info!(
                "{} [SERIAL] read stopped reason=app_shutdown bytes={}",
                log_header,
                reply.len()
            );
            break;
        }
        // Wait in short slices so the deadline/shutdown checks above run even
        // while the server-supplied budgets are minutes long.
        let wait = silence_deadline
            .min(read_deadline)
            .saturating_duration_since(now);
        if wait.is_zero() {
            break; // line went silent: the reply (or its absence) is complete
        }
        let slice = wait.min(SERIAL_READ_SLICE);
        if let Err(e) = port.set_timeout(slice) {
            log::warn!(
                "{} [SERIAL] set_timeout({:?}) failed: {}",
                log_header,
                slice,
                e
            );
        }
        match port.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if first_byte_pending {
                    // reply started: from here on the read is bounded by line silence.
                    // The elapsed time is logged because a first byte arriving later
                    // than `idle` is exactly the condition that used to be reported as
                    // `no_reply` — worth seeing in a log when tuning the server timings.
                    first_byte_pending = false;
                    let ttfb = read_started.elapsed();
                    if ttfb > idle {
                        log::debug!(
                            "{} [SERIAL] first byte after {:?} (over idle {:?})",
                            log_header,
                            ttfb,
                            idle
                        );
                    }
                }
                reply.extend_from_slice(&buf[..n]);
                silence_deadline = std::time::Instant::now() + idle;
            }
            // A timed-out slice is not the end of the reply by itself — the
            // loop re-evaluates the silence bound and keeps listening.
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => {
                log::warn!("{} [SERIAL] read error: {}", log_header, e);
                break;
            }
        }
    }
    (reply, truncated)
}

/// Listens for up to `interval` for a frame the rack pushes without being asked, and reads
/// it whole once it starts. This replaces the blind sleep that used to sit between status
/// polls: the rack does not wait to be polled for a card result, it sends the frame as soon
/// as the card is done (verified on live hardware — "accepted"+result in a single read at
/// `polls=0`), and it hands that result out exactly once, going back to "idle" afterwards.
/// A result landing in an unwatched gap was therefore lost for good. Empty return = the
/// interval elapsed in silence, time to send the next status poll. The truncation flag
/// rides along so a capped frame is reported as a transport failure, never as a result.
fn wait_for_push(
    port: &mut Box<dyn SerialPort>,
    interval: Duration,
    idle: Duration,
    deadline: Duration,
    log_header: &str,
) -> (Vec<u8>, bool) {
    let (bytes, truncated) = read_reply(port, Vec::new(), interval, idle, deadline, log_header);
    if !bytes.is_empty() {
        log::debug!(
            "{} [SERIAL] rx pushed bytes={} truncated={} hex={}",
            log_header,
            bytes.len(),
            truncated,
            hex::encode_upper(&bytes)
        );
    }
    (bytes, truncated)
}

/// One write+read exchange on an already-locked port. `deadline` bounds the whole read phase,
/// including the wait for the rack's first reply byte; `idle` is the line silence that ends a
/// reply once it has started. `purge_stale` tells whether pending bytes belong to a previous
/// operation (dropped) or to this one (carried into the reply) — see the body. Payload hex only
/// at debug — the rack protocol must not end up in users' log files at INFO level.
fn exchange_once(
    port: &mut Box<dyn SerialPort>,
    cmd_hex: &str,
    idle: Duration,
    deadline: Duration,
    purge_stale: bool,
    log_header: &str,
) -> SerialExchange {
    // envelope hex is pre-normalized; this guard covers direct callers only
    let bytes = match hex::decode(cmd_hex) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("{} [SERIAL] bad hex in serial_cmd: {}", log_header, e);
            return SerialExchange::error(SERIAL_ERR_BAD_HEX);
        }
    };

    // Bytes already buffered when this exchange starts. Their meaning depends on where in
    // the operation we are, and getting that wrong costs the operation its result:
    //   * first exchange of an operation (`purge_stale`) — the port lock was just taken, so
    //     anything pending is left over from an earlier, finished operation. Dropped, but
    //     logged: silent purges are how a lost result stays invisible.
    //   * any later exchange (a status poll of the same operation) — the rack pushes the
    //     card result on its own as soon as the card is done, without waiting to be polled.
    //     Those bytes ARE this operation's result; they are carried into the reply and the
    //     server's multi-frame parser picks the outcome frame out of the concatenation.
    // Blind-clearing here (every write, unconditionally) is what ate the result of every
    // card operation slow enough to finish between two read windows — EXTERNAL AUTHENTICATE
    // above all — after which the slot reports plain "card in reader, idle".
    let carry = drain_buffered(port, log_header);
    let carry = if purge_stale {
        if !carry.is_empty() {
            log::debug!(
                "{} [SERIAL] dropped {} stale bytes before tx hex={}",
                log_header,
                carry.len(),
                hex::encode_upper(&carry)
            );
        }
        Vec::new()
    } else {
        if !carry.is_empty() {
            log::debug!(
                "{} [SERIAL] carrying {} pushed bytes into this exchange hex={}",
                log_header,
                carry.len(),
                hex::encode_upper(&carry)
            );
        }
        carry
    };

    log::debug!(
        "{} [SERIAL] tx bytes={} hex={}",
        log_header,
        bytes.len(),
        cmd_hex
    );

    if let Err(e) = port.write_all(&bytes) {
        log::error!("{} [SERIAL] write failed: {}", log_header, e);
        return SerialExchange::error(SERIAL_ERR_WRITE_FAILED);
    }
    let _ = port.flush();

    let (reply, truncated) = read_reply(port, carry, deadline, idle, deadline, log_header);
    let exchange = if truncated {
        // Partial data + error code: the server sees what came through AND
        // knows the exchange is unusable.
        SerialExchange {
            resp_hex: hex::encode_upper(&reply),
            err: SERIAL_ERR_TRUNCATED,
        }
    } else if reply.is_empty() {
        SerialExchange::error(SERIAL_ERR_NO_REPLY)
    } else {
        SerialExchange::ok(hex::encode_upper(&reply))
    };
    log::debug!(
        "{} [SERIAL] rx bytes={} err={} hex={}",
        log_header,
        exchange.resp_hex.len() / 2,
        exchange.err,
        exchange.resp_hex
    );
    exchange
}

/// The blocking core of one logical operation on an already-locked port: the command exchange
/// plus the optional server-scripted poll loop. Returns the outcome and how it was obtained
/// (`polls` status requests sent, `pushes` frames the rack sent on its own).
///
/// The wire model this implements, as verified on live hardware: the rack answers the command
/// with "accepted", then sends the card's result **by itself** as soon as the card is done, and
/// hands that result out exactly once — afterwards the slot reports plain "idle". Status polls
/// are the fallback for a result that was not caught, not the primary channel. Hence the two
/// rules below: never stop listening between reads, and never drop bytes that arrived while we
/// were not reading.
fn run_envelope(
    port: &mut Box<dyn SerialPort>,
    env: &SerialEnvelope,
    log_header: &str,
) -> (SerialExchange, u32, u32) {
    let mut polls: u32 = 0;
    let mut pushes: u32 = 0;

    // first exchange of the operation: the port lock was just taken, so anything still
    // buffered belongs to an operation that is already over — purge it
    let first = exchange_once(port, &env.cmd_hex, env.idle, env.deadline, true, log_header);
    let outcome = 'op: {
        if !first.is_ok() {
            break 'op first;
        }
        let Some(poll) = &env.poll else {
            break 'op first; // one-shot exchange: the first reply is the result
        };
        // the poll loop is entered only when the device answered exactly the predicted
        // "accepted" bytes; anything else (a NAK, an instant result) goes back as is
        match &env.expect_hex {
            Some(expect) if first.resp_hex == *expect => {}
            _ => break 'op first,
        }
        let poll_deadline = std::time::Instant::now() + poll.deadline;
        loop {
            // App is closing: abandon the operation so the port lock is released.
            if SHUTTING_DOWN.load(Ordering::SeqCst) {
                break 'op SerialExchange::error(SERIAL_ERR_NO_REPLY);
            }
            // listen through the poll interval instead of sleeping through it: the rack
            // pushes the card result on its own and only once, so an unwatched gap loses
            // it. A pushed frame that is exactly the predicted "busy" bytes is just a
            // late poll reply — same rule as below, keep waiting for the real outcome.
            let (pushed, pushed_truncated) =
                wait_for_push(port, poll.interval, env.idle, env.deadline, log_header);
            if !pushed.is_empty() {
                pushes += 1;
                let pushed_hex = hex::encode_upper(&pushed);
                if pushed_truncated {
                    // capped frame: partial data + error code, same contract as
                    // exchange_once — the server must not parse it as a result
                    break 'op SerialExchange {
                        resp_hex: pushed_hex,
                        err: SERIAL_ERR_TRUNCATED,
                    };
                }
                if pushed_hex != poll.while_hex {
                    break 'op SerialExchange::ok(pushed_hex);
                }
            }
            polls += 1;
            // not the first exchange: pending bytes are this operation's pushed result,
            // they get carried into the poll reply rather than dropped
            let reply = exchange_once(
                port,
                &poll.cmd_hex,
                env.idle,
                env.deadline,
                false,
                log_header,
            );
            if !reply.is_ok() || reply.resp_hex != poll.while_hex {
                // the first differing reply is the operation result (or a transport error)
                break 'op reply;
            }
            if std::time::Instant::now() >= poll_deadline {
                // still "busy" at the deadline: hand the last reply to the server — it can
                // decode the device state and report a readable failure
                log::warn!(
                    "{} [SERIAL] poll deadline {:?} reached after {} polls — returning the last reply",
                    log_header,
                    poll.deadline,
                    polls
                );
                break 'op reply;
            }
        }
    };
    (outcome, polls, pushes)
}

/// Executes a whole envelope on the shared port: the command exchange plus the optional
/// server-scripted poll loop. The port lock is held for the entire logical operation — the rack
/// is Master/Slave (one request on the wire at a time), so concurrent card sessions of one rack
/// interleave at operation granularity; tokio's Mutex queues the waiters FIFO-fair, which is the
/// per-port queue. Blocking serial I/O runs on a blocking thread so the async runtime isn't stalled.
/// `log_summary=false` silences the per-operation INFO line — the 1 Hz watch loop would flood
/// the log otherwise; its caller logs only actual changes.
pub(super) async fn execute_envelope(
    port: &SharedPort,
    env: SerialEnvelope,
    log_header: &str,
    log_summary: bool,
) -> SerialExchange {
    let port = port.clone();
    let log_header_blocking = log_header.to_string();

    let result = tokio::task::spawn_blocking(move || {
        let mut guard = port.blocking_lock();
        run_envelope(&mut guard, &env, &log_header_blocking)
    })
    .await
    // A join error means the blocking closure panicked before producing a
    // result — no reply was obtained, report it as such.
    .unwrap_or_else(|e| {
        log::error!("{} [SERIAL] exchange task failed: {}", log_header, e);
        (SerialExchange::error(SERIAL_ERR_NO_REPLY), 0, 0)
    });

    let (exchange, polls, pushes) = result;
    // one INFO summary per logical operation; per-exchange details are at debug
    if exchange.is_ok() {
        if log_summary {
            log::info!(
                "{} [SERIAL] op done polls={} pushes={} rx bytes={}",
                log_header,
                polls,
                pushes,
                exchange.resp_hex.len() / 2
            );
        }
    } else {
        log::warn!(
            "{} [SERIAL] op failed err={} polls={} pushes={} partial_bytes={}",
            log_header,
            exchange.err,
            polls,
            pushes,
            exchange.resp_hex.len() / 2
        );
    }
    exchange
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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

    // ── Envelope parsing (poll primitive contract) ──
    // Only synthetic placeholder bytes here: the device wire protocol is owned
    // by the server and must never appear in this repo, not even in tests.

    #[test]
    fn envelope_without_serial_cmd_is_not_an_envelope() {
        let json: serde_json::Value = serde_json::from_str(r#"{"connect":{}}"#).unwrap();
        assert!(parse_envelope(&json).is_none());
    }

    #[test]
    fn envelope_reads_finish_flag_from_a_real_server_payload() {
        // Verbatim envelope captured from the server during an authentication.
        let json: serde_json::Value = serde_json::from_str(
            r#"{"deadline_ms":500,"expect":"5501006644","finish":false,"idle_ms":50,"poll":{"cmd":"55010000AA","deadline_ms":5000,"interval_ms":20,"while":"55010004A6"},"serial_cmd":"550100604A"}"#,
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
        assert_eq!(env.cmd_hex, "550100604A");
        assert_eq!(env.expect_hex.as_deref(), Some("5501006644"));
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
        let json: serde_json::Value =
            serde_json::from_str(r#"{"serial_cmd":"550100604A"}"#).unwrap();
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

    // ── Poll primitive against a scripted port ──
    // Placeholder bytes again — only the SHAPE of the exchange is TBA's business (a command,
    // the predicted "accepted" echo, an outcome frame, the predicted "busy" poll reply, and a
    // status that is neither). The timing shape is taken from a real failing card session
    // captured on the channel: the device answered "accepted" within milliseconds and sent the
    // card result on its own roughly one idle-window later, i.e. exactly into the gap between
    // two reads. Absolute values are scaled up here so a loaded CI host cannot turn the
    // scheduling into a coin flip.

    const CMD: &str = "C0DE01";
    const ACCEPTED: &str = "AC01";
    const RESULT: &str = "1234ABCD";
    const POLL_CMD: &str = "5701";
    const BUSY: &str = "B501";
    const IDLE_STATUS: &str = "1D01";

    const T_IDLE: Duration = Duration::from_millis(100);
    const T_DEADLINE: Duration = Duration::from_millis(1000);
    const T_INTERVAL: Duration = Duration::from_millis(100);

    struct PortState {
        /// written hex -> replies to schedule, each at its own delay from that write
        rules: Vec<(String, Vec<(Duration, String)>)>,
        scheduled: Vec<(std::time::Instant, Vec<u8>)>,
        inbox: std::collections::VecDeque<u8>,
        timeout: Duration,
        /// Every frame written to the port, in order — shared so a test can still read it
        /// after the port has been boxed into a `dyn SerialPort`.
        writes: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl PortState {
        /// Moves every reply whose time has come into the readable buffer.
        fn pump(&mut self) {
            let now = std::time::Instant::now();
            let mut still = Vec::new();
            for (at, bytes) in std::mem::take(&mut self.scheduled) {
                if at <= now {
                    self.inbox.extend(bytes);
                } else {
                    still.push((at, bytes));
                }
            }
            self.scheduled = still;
        }

        fn next_due(&self) -> Option<std::time::Instant> {
            self.scheduled.iter().map(|(at, _)| *at).min()
        }
    }

    /// Stand-in for the rack's serial port. Replies are delivered on the real clock, so the
    /// production timing rules (first-byte budget, line-silence bound, poll interval) run
    /// exactly as they do on hardware. State lives behind a mutex because `bytes_to_read`
    /// takes `&self` yet has to advance the schedule.
    struct ScriptedPort {
        state: std::sync::Mutex<PortState>,
        writes: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl ScriptedPort {
        fn new(rules: &[(&str, &[(u64, &str)])]) -> Self {
            let writes = Arc::new(std::sync::Mutex::new(Vec::new()));
            Self {
                writes: writes.clone(),
                state: std::sync::Mutex::new(PortState {
                    rules: rules
                        .iter()
                        .map(|(cmd, replies)| {
                            (
                                cmd.to_string(),
                                replies
                                    .iter()
                                    .map(|(ms, hex)| (Duration::from_millis(*ms), hex.to_string()))
                                    .collect(),
                            )
                        })
                        .collect(),
                    scheduled: Vec::new(),
                    inbox: std::collections::VecDeque::new(),
                    timeout: Duration::from_millis(0),
                    writes,
                }),
            }
        }

        /// Bytes already sitting in the buffer when the exchange starts — a leftover of an
        /// operation that is already over, or a result pushed while nobody was reading.
        fn with_pending(self, hex: &str) -> Self {
            self.state
                .lock()
                .unwrap()
                .inbox
                .extend(hex::decode(hex).unwrap());
            self
        }
    }

    impl std::io::Read for ScriptedPort {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let deadline = std::time::Instant::now() + self.state.lock().unwrap().timeout;
            loop {
                let wake = {
                    let mut st = self.state.lock().unwrap();
                    st.pump();
                    if !st.inbox.is_empty() {
                        let n = buf.len().min(st.inbox.len());
                        for slot in buf.iter_mut().take(n) {
                            *slot = st.inbox.pop_front().unwrap();
                        }
                        return Ok(n);
                    }
                    st.next_due().map(|d| d.min(deadline)).unwrap_or(deadline)
                };
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "scripted",
                    ));
                }
                if wake > now {
                    std::thread::sleep(wake - now);
                }
            }
        }
    }

    impl std::io::Write for ScriptedPort {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut st = self.state.lock().unwrap();
            let written = hex::encode_upper(buf);
            let now = std::time::Instant::now();
            let replies: Vec<(Duration, String)> = st
                .rules
                .iter()
                .find(|(cmd, _)| *cmd == written)
                .map(|(_, r)| r.clone())
                .unwrap_or_default();
            for (delay, hex) in replies {
                st.scheduled.push((now + delay, hex::decode(&hex).unwrap()));
            }
            st.writes.lock().unwrap().push(written);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl SerialPort for ScriptedPort {
        fn name(&self) -> Option<String> {
            Some("scripted".into())
        }
        fn baud_rate(&self) -> serialport::Result<u32> {
            Ok(115_200)
        }
        fn data_bits(&self) -> serialport::Result<serialport::DataBits> {
            Ok(serialport::DataBits::Eight)
        }
        fn flow_control(&self) -> serialport::Result<serialport::FlowControl> {
            Ok(serialport::FlowControl::None)
        }
        fn parity(&self) -> serialport::Result<serialport::Parity> {
            Ok(serialport::Parity::None)
        }
        fn stop_bits(&self) -> serialport::Result<serialport::StopBits> {
            Ok(serialport::StopBits::One)
        }
        fn timeout(&self) -> Duration {
            self.state.lock().unwrap().timeout
        }
        fn set_baud_rate(&mut self, _: u32) -> serialport::Result<()> {
            Ok(())
        }
        fn set_data_bits(&mut self, _: serialport::DataBits) -> serialport::Result<()> {
            Ok(())
        }
        fn set_flow_control(&mut self, _: serialport::FlowControl) -> serialport::Result<()> {
            Ok(())
        }
        fn set_parity(&mut self, _: serialport::Parity) -> serialport::Result<()> {
            Ok(())
        }
        fn set_stop_bits(&mut self, _: serialport::StopBits) -> serialport::Result<()> {
            Ok(())
        }
        fn set_timeout(&mut self, timeout: Duration) -> serialport::Result<()> {
            self.state.lock().unwrap().timeout = timeout;
            Ok(())
        }
        fn write_request_to_send(&mut self, _: bool) -> serialport::Result<()> {
            Ok(())
        }
        fn write_data_terminal_ready(&mut self, _: bool) -> serialport::Result<()> {
            Ok(())
        }
        fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn bytes_to_read(&self) -> serialport::Result<u32> {
            let mut st = self.state.lock().unwrap();
            st.pump();
            Ok(st.inbox.len() as u32)
        }
        fn bytes_to_write(&self) -> serialport::Result<u32> {
            Ok(0)
        }
        fn clear(&self, _: serialport::ClearBuffer) -> serialport::Result<()> {
            self.state.lock().unwrap().inbox.clear();
            Ok(())
        }
        fn try_clone(&self) -> serialport::Result<Box<dyn SerialPort>> {
            Err(serialport::Error::new(
                serialport::ErrorKind::Unknown,
                "not cloneable",
            ))
        }
        fn set_break(&self) -> serialport::Result<()> {
            Ok(())
        }
        fn clear_break(&self) -> serialport::Result<()> {
            Ok(())
        }
    }

    fn two_phase_envelope() -> SerialEnvelope {
        SerialEnvelope {
            cmd_hex: CMD.into(),
            expect_hex: Some(ACCEPTED.into()),
            idle: T_IDLE,
            deadline: T_DEADLINE,
            finish: None,
            poll: Some(PollSpec {
                cmd_hex: POLL_CMD.into(),
                while_hex: BUSY.into(),
                interval: T_INTERVAL,
                deadline: Duration::from_millis(2000),
            }),
        }
    }

    #[test]
    fn result_pushed_after_the_accepted_window_is_not_lost() {
        // THE REGRESSION. Timing of a real failed authentication: "accepted" comes back at
        // once, the card takes just over one idle window, and the device sends the result on
        // its own — into the gap between two reads. That gap used to be a blind sleep followed
        // by a buffer purge, so the result was destroyed and the next status request found the
        // slot back at rest, which surfaced to the server as a plain "idle" status and failed
        // the whole authentication one command before the end.
        let port = ScriptedPort::new(&[
            (CMD, &[(10, ACCEPTED), (150, RESULT)]),
            // the device hands a result out exactly once and rests afterwards
            (POLL_CMD, &[(10, IDLE_STATUS)]),
        ]);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let (outcome, _polls, _pushes) = run_envelope(&mut port, &two_phase_envelope(), "TEST |");

        // The invariant is what the bug broke: the result reaches the server. Whether it was
        // caught by the listening window or carried into a poll reply is a scheduling detail
        // and deliberately not asserted — both are correct, neither loses the bytes.
        assert!(
            outcome.resp_hex.contains(RESULT),
            "result went missing, got {:?}",
            outcome.resp_hex
        );
        assert_ne!(
            outcome.resp_hex, IDLE_STATUS,
            "the lost-result symptom is back"
        );
        assert!(outcome.is_ok());
    }

    #[test]
    fn fast_result_glued_with_accepted_needs_no_poll() {
        // The healthy case that always worked: the card is quick enough that the result lands
        // inside the read window of the "accepted" frame. The buffer then differs from the
        // predicted bytes, so it goes straight back to the server (which owns the framing).
        let port = ScriptedPort::new(&[(CMD, &[(10, ACCEPTED), (20, RESULT)])]);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let (outcome, polls, pushes) = run_envelope(&mut port, &two_phase_envelope(), "TEST |");

        assert_eq!(outcome.resp_hex, format!("{}{}", ACCEPTED, RESULT));
        assert_eq!((polls, pushes), (0, 0));
    }

    #[test]
    fn pushed_busy_frame_is_not_mistaken_for_the_outcome() {
        // Not every unprompted frame is a result: a poll reply that outlived its read window
        // arrives the same way. It matches the "keep polling" bytes the server predicted, so
        // it must be consumed and ignored, not returned as the operation's outcome.
        let port = ScriptedPort::new(&[
            (CMD, &[(10, ACCEPTED), (150, BUSY)]),
            (POLL_CMD, &[(10, RESULT)]),
        ]);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let (outcome, polls, pushes) = run_envelope(&mut port, &two_phase_envelope(), "TEST |");

        assert_eq!(outcome.resp_hex, RESULT);
        assert_eq!((polls, pushes), (1, 1));
    }

    #[test]
    fn transport_failure_of_the_first_exchange_skips_the_poll_loop() {
        // A silent device must not be polled: the server needs the transport error, not a
        // status decoded from nothing.
        let port = ScriptedPort::new(&[]);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let mut env = two_phase_envelope();
        env.deadline = Duration::from_millis(50); // no point waiting a full budget in a test
        let (outcome, polls, pushes) = run_envelope(&mut port, &env, "TEST |");

        assert_eq!(outcome.err, SERIAL_ERR_NO_REPLY);
        assert_eq!((polls, pushes), (0, 0));
    }

    #[test]
    fn poll_exchange_carries_bytes_that_were_already_waiting() {
        // Inside an operation, whatever is buffered belongs to that operation — the device
        // pushed it while we were between reads. It is prepended to the reply; the server's
        // parser is the one that walks a buffer of several frames.
        let port = ScriptedPort::new(&[(POLL_CMD, &[(10, BUSY)])]).with_pending(RESULT);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let ex = exchange_once(&mut port, POLL_CMD, T_IDLE, T_DEADLINE, false, "TEST |");

        assert_eq!(ex.resp_hex, format!("{}{}", RESULT, BUSY));
    }

    #[test]
    fn first_exchange_of_an_operation_drops_what_was_left_over() {
        // At the start of an operation the port lock was just taken, so anything buffered is
        // the tail of an operation that is already over. Keeping it would prepend a foreign
        // frame to this operation's reply.
        let port = ScriptedPort::new(&[(CMD, &[(10, ACCEPTED)])]).with_pending(RESULT);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let ex = exchange_once(&mut port, CMD, T_IDLE, T_DEADLINE, true, "TEST |");

        assert_eq!(ex.resp_hex, ACCEPTED);
    }

    #[test]
    fn one_shot_exchange_returns_the_first_reply() {
        // No poll spec (global status, LED, firmware): the first reply is the whole answer.
        let port = ScriptedPort::new(&[(CMD, &[(10, RESULT)])]);
        let writes = port.writes.clone();
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let env = SerialEnvelope {
            cmd_hex: CMD.into(),
            expect_hex: None,
            idle: T_IDLE,
            deadline: T_DEADLINE,
            poll: None,
            finish: None,
        };
        let (outcome, polls, pushes) = run_envelope(&mut port, &env, "TEST |");

        assert_eq!(outcome.resp_hex, RESULT);
        assert_eq!((polls, pushes), (0, 0));
        assert_eq!(
            *writes.lock().unwrap(),
            vec![CMD.to_string()],
            "a one-shot exchange writes the command and nothing else"
        );
    }
}
