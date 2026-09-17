//! Serial transport for the card rack: the wire itself.
//!
//! Everything that touches the COM port lives here — framing timings, the
//! read/write loop, and the server-supplied command envelope. This module is
//! deliberately protocol-agnostic: it moves opaque bytes and never interprets
//! them — the client is a dumb pipe.

use std::io::{Read, Write};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serialport::SerialPort;
use tokio::sync::Mutex as AsyncMutex;

use crate::debug_log::frame_digest;

use super::SHUTTING_DOWN;

pub(super) type SharedPort = Arc<AsyncMutex<Box<dyn SerialPort>>>;

/// The port lock of a retained operation, parked between the exchanges that run under it.
type PortGuard = tokio::sync::OwnedMutexGuard<Box<dyn SerialPort>>;
type HeldPort = Arc<std::sync::Mutex<Option<PortGuard>>>;

/// Locks the cell of a retained port, recovering from poisoning: the guard inside is safe to reuse.
fn held(cell: &HeldPort) -> std::sync::MutexGuard<'_, Option<PortGuard>> {
    cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// An opaque serial operation retained across server round trips, scoped to one MQTT owner.
pub(super) struct SerialLease {
    pub(super) id: u64,
    until: Instant,
    port: HeldPort,
}

impl SerialLease {
    pub(super) fn is_live(&self) -> bool {
        Instant::now() < self.until && held(&self.port).is_some()
    }

    /// Takes the port for up to `ms` (clamped to 1..10000): the wait for the port has the same
    /// bound, and the lifetime of the operation starts once the port is acquired.
    pub(super) async fn acquire(port: &SharedPort, id: u64, ms: u64) -> Result<Self, &'static str> {
        let duration = Duration::from_millis(ms.clamp(1, 10_000));
        let queued = Instant::now();
        let guard = tokio::time::timeout(duration, port.clone().lock_owned())
            .await
            .map_err(|_| SERIAL_ERR_QUEUE_TIMEOUT)?;
        log::debug!(
            "SERIAL lease id={} status=acquired queue_ms={} hold_ms={}",
            id,
            queued.elapsed().as_millis(),
            duration.as_millis()
        );
        let until = Instant::now() + duration;
        let port = Arc::new(std::sync::Mutex::new(Some(guard)));
        tokio::spawn(expire_lease(Arc::downgrade(&port), until));
        Ok(Self { id, until, port })
    }

    /// Runs one envelope under the retained port. `first` tells whether pending bytes are
    /// leftovers of an earlier operation (purged) or belong to this one (carried into the reply).
    pub(super) async fn execute(&mut self, env: SerialEnvelope, first: bool, log: &str) -> SerialExchange {
        let Some(mut port) = held(&self.port).take() else {
            return SerialExchange::error(SERIAL_ERR_TRANSACTION_EXPIRED);
        };
        let until = self.until;
        let log = log.to_owned();
        let cell = self.port.clone();
        tokio::task::spawn_blocking(move || {
            if Instant::now() >= until {
                resync_line(&mut port, &log);
                return SerialExchange::error(SERIAL_ERR_TRANSACTION_EXPIRED);
            }
            let started = Instant::now();
            let outcome = run_envelope_inner(&mut port, &env, first, Budget::until(until), &log);
            log::info!(
                "{} [SERIAL] retained exchange polls={} pushes={} end={} err={} rx_bytes={} wire_ms={} lease_left_ms={}",
                log,
                outcome.polls,
                outcome.pushes,
                outcome.end,
                outcome.exchange.err,
                outcome.exchange.resp_hex.len() / 2,
                started.elapsed().as_millis(),
                until.saturating_duration_since(Instant::now()).as_millis()
            );
            // the port stays retained only after a successful exchange inside the lifetime
            if outcome.exchange.is_ok() && Instant::now() < until {
                *held(&cell) = Some(port);
            }
            outcome.exchange
        })
        .await
        .unwrap_or_else(|_| SerialExchange::error(SERIAL_ERR_NO_REPLY))
    }
}

impl Drop for SerialLease {
    fn drop(&mut self) {
        drop(held(&self.port).take());
    }
}

/// Frees a retained port whose lifetime ended without a release from the server, draining
/// whatever the device still sends so the next operation starts on a quiet line.
async fn expire_lease(cell: std::sync::Weak<std::sync::Mutex<Option<PortGuard>>>, until: Instant) {
    while Instant::now() < until && !SHUTTING_DOWN.load(Ordering::SeqCst) {
        let left = until.saturating_duration_since(Instant::now());
        tokio::time::sleep(Duration::from_millis(20).min(left)).await;
    }
    let Some(cell) = cell.upgrade() else {
        return;
    };
    let taken = held(&cell).take();
    if let Some(mut port) = taken {
        let _ = tokio::task::spawn_blocking(move || resync_line(&mut port, "SERIAL lease expired")).await;
    }
}

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

/// Why one serial read ended. Log-only (the server never sees these), but the
/// distinction is what tells a reply cut by the inter-byte silence from one cut
/// by the total deadline, and either from a rack that never answered at all.
const READ_END_SILENCE: &str = "silence";
const READ_END_DEADLINE: &str = "deadline";
const READ_END_CAP: &str = "cap";
const READ_END_SHUTDOWN: &str = "shutdown";
const READ_END_EOF: &str = "eof";
const READ_END_ERROR: &str = "error";

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

/// Bounds of line resync after a failed operation or stale bytes before TX: the rack answers a
/// command it did not answer in time LATER, on its own, and those bytes would
/// otherwise open the next read window and make two good frames decode as one
/// broken one on the server. `QUIET` is the silence that ends the resync,
/// `BUDGET` caps the whole of it, `SLICE` is how often the line is sampled.
const SERIAL_RESYNC_QUIET: Duration = Duration::from_millis(300);
const SERIAL_RESYNC_BUDGET: Duration = Duration::from_millis(1500);
const SERIAL_RESYNC_SLICE: Duration = Duration::from_millis(20);

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
    /// (e.g. a slot indicator repaint) — see `run_serial_request`.
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

/// A server-supplied timing field in milliseconds, capped at `SERIAL_MS_MAX`.
fn ms_field(json: &serde_json::Value, key: &str) -> Option<u64> {
    json.get(key)
        .and_then(|v| v.as_u64())
        .map(|v| v.min(SERIAL_MS_MAX))
}

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
        idle: ms_field(json, "idle_ms")
            .map(Duration::from_millis)
            .unwrap_or(SERIAL_REPLY_TIMEOUT),
        deadline: ms_field(json, "deadline_ms")
            .map(Duration::from_millis)
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

/// Bytes of one read and why it ended.
struct Reply {
    bytes: Vec<u8>,
    /// The size cap cut the reply.
    truncated: bool,
    /// One of the `READ_END_*` reasons.
    end: &'static str,
}

impl Reply {
    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn hex(&self) -> String {
        hex::encode_upper(&self.bytes)
    }

    /// The exchange the server gets: a reply the device finished is a success, one cut by the
    /// size cap or by a bound is truncated (partial data plus the error code, so the server sees
    /// what came through AND knows the exchange is unusable), no bytes at all is no reply.
    fn into_exchange(self) -> SerialExchange {
        if self.truncated || (!self.is_empty() && self.end != READ_END_SILENCE) {
            SerialExchange {
                resp_hex: self.hex(),
                err: SERIAL_ERR_TRUNCATED,
            }
        } else if self.is_empty() {
            SerialExchange::error(SERIAL_ERR_NO_REPLY)
        } else {
            SerialExchange::ok(self.hex())
        }
    }
}

/// Takes a snapshot of the bytes currently buffered by the serial driver.
/// A single drain does not imply that the device has finished sending.
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
/// on the reply size and `deadline` on the whole read phase. The reply says WHY the read
/// ended - a reply cut short by the silence bound and one cut by the total deadline look
/// identical in the bytes but mean opposite things when the server timings are tuned (a long
/// reply can arrive with pauses inside it, so both bounds have to clear those pauses).
fn read_reply(
    port: &mut Box<dyn SerialPort>,
    carry: Vec<u8>,
    first_wait: Duration,
    idle: Duration,
    deadline: Duration,
    log_header: &str,
) -> Reply {
    let mut reply = Reply {
        bytes: carry,
        truncated: false,
        end: READ_END_SILENCE,
    };
    let mut first_byte_pending = reply.is_empty();

    let mut buf = [0u8; 512];
    let read_started = Instant::now();
    // the total bound must never undercut the first-byte budget it contains
    let total = deadline.max(first_wait);
    let read_deadline = read_started + total;
    // The silence bound that ends the reply: first-byte budget while nothing
    // has arrived yet, line-idle from the last received byte afterwards.
    let mut silence_deadline = read_started + if first_byte_pending { first_wait } else { idle };
    loop {
        if reply.bytes.len() >= SERIAL_REPLY_MAX_BYTES {
            log::warn!(
                "{} [SERIAL] reply exceeded {} bytes — truncating, device is misbehaving",
                log_header,
                SERIAL_REPLY_MAX_BYTES
            );
            reply.truncated = true;
            reply.end = READ_END_CAP;
            break;
        }
        let now = Instant::now();
        if now >= read_deadline {
            log::warn!(
                "{} [SERIAL] read deadline {:?} reached — returning {} bytes read so far",
                log_header,
                total,
                reply.bytes.len()
            );
            reply.end = READ_END_DEADLINE;
            break;
        }
        // App is closing: stop waiting so the blocking closure releases the
        // port lock promptly — `spawn_blocking` cannot be aborted from outside.
        if SHUTTING_DOWN.load(Ordering::SeqCst) {
            log::info!(
                "{} [SERIAL] read stopped reason=app_shutdown bytes={}",
                log_header,
                reply.bytes.len()
            );
            reply.end = READ_END_SHUTDOWN;
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
            Ok(0) => {
                reply.end = READ_END_EOF;
                break;
            }
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
                reply.bytes.extend_from_slice(&buf[..n]);
                silence_deadline = Instant::now() + idle;
            }
            // A timed-out slice is not the end of the reply by itself — the
            // loop re-evaluates the silence bound and keeps listening.
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => {
                log::warn!("{} [SERIAL] read error: {}", log_header, e);
                reply.end = READ_END_ERROR;
                break;
            }
        }
    }
    reply
}

/// Drains the line until it goes quiet after a failure or when stale bytes are found.
/// Returns whether silence was observed within the recovery budget. The rack does
/// answer a command it missed its deadline on - just late, on its own - and those bytes
/// sit in the port buffer until the next read picks them up in front of the reply they
/// do not belong to. The server then receives two replies run together in one buffer and
/// rejects it, so the failure spreads from the operation that timed out to the ones after it.
fn resync_line(port: &mut Box<dyn SerialPort>, log_header: &str) -> bool {
    let started = Instant::now();
    let mut last_byte = started;
    let mut dropped = 0usize;
    let mut quiet = false;
    loop {
        if started.elapsed() >= SERIAL_RESYNC_BUDGET || SHUTTING_DOWN.load(Ordering::SeqCst) {
            break;
        }
        let late = drain_buffered(port, log_header);
        if late.is_empty() {
            if last_byte.elapsed() >= SERIAL_RESYNC_QUIET {
                quiet = true;
                break;
            }
            std::thread::sleep(SERIAL_RESYNC_SLICE);
            continue;
        }
        dropped += late.len();
        last_byte = Instant::now();
    }
    if dropped > 0 {
        log::warn!(
            "{} [SERIAL] resync dropped {} late bytes quiet={}",
            log_header,
            dropped,
            quiet
        );
    }
    quiet
}

/// Listens for up to `interval` for a frame the rack pushes without being asked, and reads
/// it whole once it starts. This replaces the blind sleep that used to sit between status
/// polls: the rack does not wait to be polled for a card result, it sends the frame as soon
/// as the card is done (verified on live hardware — "accepted"+result in a single read at
/// `polls=0`), and it hands that result out exactly once, going back to "idle" afterwards.
/// A result landing in an unwatched gap was therefore lost for good. An empty reply means the
/// interval elapsed in silence, time to send the next status poll.
fn wait_for_push(
    port: &mut Box<dyn SerialPort>,
    interval: Duration,
    idle: Duration,
    deadline: Duration,
    log_header: &str,
) -> Reply {
    let reply = read_reply(port, Vec::new(), interval, idle, deadline, log_header);
    if !reply.is_empty() {
        log::debug!(
            "{} [SERIAL] rx pushed bytes={} truncated={} end={} digest={}",
            log_header,
            reply.bytes.len(),
            reply.truncated,
            reply.end,
            frame_digest(&reply.hex())
        );
        log::trace!("{} [SERIAL] rx pushed hex={}", log_header, reply.hex());
    }
    reply
}

/// Decides what to do with the bytes already buffered when an exchange starts, and returns the
/// ones the reply must begin with. Getting this wrong costs the operation its result:
///   * first exchange of an operation (`purge_stale`) — the port lock was just taken, so
///     anything pending is left over from an earlier, finished operation. Dropped, but
///     logged: silent purges are how a lost result stays invisible. The previous reply can
///     still be arriving in chunks, so the line is drained until quiet; a line that does not
///     settle fails the exchange without writing the command.
///   * any later exchange (a status poll of the same operation) — the rack pushes the
///     card result on its own as soon as the card is done, without waiting to be polled.
///     Those bytes ARE this operation's result; they are carried into the reply and the
///     server's multi-frame parser picks the outcome frame out of the concatenation.
/// Blind-clearing on every write is what ate the result of every card operation slow
/// enough to finish between two read windows, after which the slot reports plain idle.
fn settle_line(
    port: &mut Box<dyn SerialPort>,
    purge_stale: bool,
    log_header: &str,
) -> Result<Vec<u8>, (SerialExchange, &'static str)> {
    let pending = drain_buffered(port, log_header);
    if !purge_stale {
        if !pending.is_empty() {
            log::debug!(
                "{} [SERIAL] carrying {} pushed bytes into this exchange digest={}",
                log_header,
                pending.len(),
                frame_digest(&hex::encode(&pending))
            );
            log::trace!(
                "{} [SERIAL] carried hex={}",
                log_header,
                hex::encode_upper(&pending)
            );
        }
        return Ok(pending);
    }
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    log::debug!(
        "{} [SERIAL] dropped {} stale bytes before tx digest={}",
        log_header,
        pending.len(),
        frame_digest(&hex::encode(&pending))
    );
    log::trace!(
        "{} [SERIAL] dropped hex={}",
        log_header,
        hex::encode_upper(&pending)
    );
    log::warn!(
        "{} [SERIAL] stale bytes before tx bytes={} action=resync",
        log_header,
        pending.len()
    );
    if !resync_line(port, log_header) {
        log::warn!("{} [SERIAL] tx skipped reason=line_not_quiet", log_header);
        return Err((SerialExchange::error(SERIAL_ERR_NO_REPLY), READ_END_DEADLINE));
    }
    Ok(Vec::new())
}

/// One write+read exchange on an already-locked port. `deadline` bounds the whole read phase,
/// including the wait for the rack's first reply byte; `idle` is the line silence that ends a
/// reply once it has started. `purge_stale` tells whether pending bytes belong to a previous
/// operation (dropped) or to this one (carried into the reply) — see `settle_line`. Returns the
/// exchange and why its read ended. Payload hex only at debug — the rack protocol must not end
/// up in users' log files at INFO level.
fn exchange_once(
    port: &mut Box<dyn SerialPort>,
    cmd_hex: &str,
    idle: Duration,
    deadline: Duration,
    purge_stale: bool,
    log_header: &str,
) -> (SerialExchange, &'static str) {
    // envelope hex is pre-normalized; this guard covers direct callers only
    let bytes = match hex::decode(cmd_hex) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("{} [SERIAL] bad hex in serial_cmd: {}", log_header, e);
            return (SerialExchange::error(SERIAL_ERR_BAD_HEX), READ_END_ERROR);
        }
    };
    let carry = match settle_line(port, purge_stale, log_header) {
        Ok(carry) => carry,
        Err(failed) => return failed,
    };

    log::debug!(
        "{} [SERIAL] tx bytes={} digest={}",
        log_header,
        bytes.len(),
        frame_digest(cmd_hex)
    );
    log::trace!("{} [SERIAL] tx hex={}", log_header, cmd_hex);
    let write_started = Instant::now();
    if let Err(e) = port.write_all(&bytes) {
        log::error!("{} [SERIAL] write failed: {}", log_header, e);
        return (SerialExchange::error(SERIAL_ERR_WRITE_FAILED), READ_END_ERROR);
    }
    let _ = port.flush();

    let reply = read_reply(port, carry, deadline, idle, deadline, log_header);
    let end = reply.end;
    let exchange = reply.into_exchange();
    log::debug!(
        "{} [SERIAL] rx bytes={} err={} end={} digest={} since_tx_ms={}",
        log_header,
        exchange.resp_hex.len() / 2,
        exchange.err,
        end,
        frame_digest(&exchange.resp_hex),
        write_started.elapsed().as_millis()
    );
    log::trace!("{} [SERIAL] rx hex={}", log_header, exchange.resp_hex);
    (exchange, end)
}

/// Time left to the retained operation an exchange runs under; `NONE` for a plain operation.
#[derive(Clone, Copy)]
struct Budget(Option<Instant>);

impl Budget {
    const NONE: Budget = Budget(None);

    fn until(until: Instant) -> Self {
        Budget(Some(until))
    }

    /// `limit` cut down to what is left of the retained operation.
    fn remaining(self, limit: Duration) -> Duration {
        self.0
            .map_or(limit, |until| limit.min(until.saturating_duration_since(Instant::now())))
    }

    /// True when nothing is left of `limit` under this budget.
    fn spent(self, limit: Duration) -> bool {
        self.remaining(limit).is_zero()
    }
}

/// Result of one logical operation and how it was obtained.
struct EnvelopeOutcome {
    exchange: SerialExchange,
    /// Status requests sent by the poll loop.
    polls: u32,
    /// Frames the device sent on its own during the poll loop.
    pushes: u32,
    /// Why the last read ended, one of the `READ_END_*` reasons.
    end: &'static str,
}

/// The blocking core of one logical operation on an already-locked port: the command exchange
/// plus the optional server-scripted poll loop.
///
/// The wire model this implements, as verified on live hardware: the rack answers the command
/// with "accepted", then sends the card's result **by itself** as soon as the card is done, and
/// hands that result out exactly once — afterwards the slot reports plain "idle". Status polls
/// are the fallback for a result that was not caught, not the primary channel. Hence the two
/// rules of the poll loop: never stop listening between reads, and never drop bytes that arrived
/// while we were not reading.
fn run_envelope(
    port: &mut Box<dyn SerialPort>,
    env: &SerialEnvelope,
    log_header: &str,
) -> EnvelopeOutcome {
    run_envelope_inner(port, env, true, Budget::NONE, log_header)
}

/// `purge_stale` is false for a continuation of a retained operation: its pending bytes are
/// carried into the reply instead of dropped. `budget` caps every wait by the operation lifetime.
fn run_envelope_inner(
    port: &mut Box<dyn SerialPort>,
    env: &SerialEnvelope,
    purge_stale: bool,
    budget: Budget,
    log_header: &str,
) -> EnvelopeOutcome {
    let (first, first_end) = exchange_once(
        port,
        &env.cmd_hex,
        env.idle,
        budget.remaining(env.deadline),
        purge_stale,
        log_header,
    );
    let mut outcome = EnvelopeOutcome {
        exchange: first,
        polls: 0,
        pushes: 0,
        end: first_end,
    };
    // the poll loop is entered only when the device answered exactly the predicted "accepted"
    // bytes; anything else (a transport error, a NAK, an instant result) goes back as is
    let accepted = outcome.exchange.is_ok()
        && env.expect_hex.as_deref() == Some(outcome.exchange.resp_hex.as_str());
    if accepted {
        if let Some(poll) = &env.poll {
            poll_for_outcome(port, env, poll, budget, &mut outcome, log_header);
        }
    }
    // A failed operation leaves the line in an unknown state: the rack may still be about
    // to answer it. Those bytes are drained here, under the same port lock, so the next
    // operation starts on a quiet line instead of reading someone else's frame first.
    if !outcome.exchange.is_ok() {
        resync_line(port, log_header);
    }
    outcome
}

/// The server-scripted poll loop after an accepted command. Listens for the result the rack
/// pushes on its own between the status polls and leaves in `outcome` the first reply that is
/// not the predicted "busy" bytes, a transport failure, or the last busy reply at the deadline
/// (the server decodes the device state from it and reports a readable failure).
fn poll_for_outcome(
    port: &mut Box<dyn SerialPort>,
    env: &SerialEnvelope,
    poll: &PollSpec,
    budget: Budget,
    outcome: &mut EnvelopeOutcome,
    log_header: &str,
) {
    let poll_deadline = Instant::now() + budget.remaining(poll.deadline);
    loop {
        if budget.spent(env.deadline) {
            outcome.exchange = SerialExchange::error(SERIAL_ERR_TRANSACTION_EXPIRED);
            return;
        }
        // App is closing: abandon the operation so the port lock is released.
        if SHUTTING_DOWN.load(Ordering::SeqCst) {
            outcome.end = READ_END_SHUTDOWN;
            outcome.exchange = SerialExchange::error(SERIAL_ERR_NO_REPLY);
            return;
        }
        // listen through the poll interval instead of sleeping through it: the rack pushes
        // the card result on its own and only once, so an unwatched gap loses it
        let pushed = wait_for_push(
            port,
            budget.remaining(poll.interval),
            env.idle,
            budget.remaining(env.deadline),
            log_header,
        );
        if !pushed.is_empty() {
            outcome.pushes += 1;
            outcome.end = pushed.end;
            let exchange = pushed.into_exchange();
            // a pushed frame that is exactly the predicted "busy" bytes is just a late poll
            // reply: consumed and ignored, the real outcome is still to come. A capped or cut
            // frame is a transport failure, never a result.
            if !exchange.is_ok() || exchange.resp_hex != poll.while_hex {
                outcome.exchange = exchange;
                return;
            }
        }
        if budget.spent(env.deadline) {
            outcome.exchange = SerialExchange::error(SERIAL_ERR_TRANSACTION_EXPIRED);
            return;
        }
        outcome.polls += 1;
        // not the first exchange: pending bytes are this operation's pushed result,
        // they get carried into the poll reply rather than dropped
        let (reply, reply_end) = exchange_once(
            port,
            &poll.cmd_hex,
            env.idle,
            budget.remaining(env.deadline),
            false,
            log_header,
        );
        outcome.end = reply_end;
        let still_busy = reply.is_ok() && reply.resp_hex == poll.while_hex;
        if still_busy && Instant::now() < poll_deadline {
            continue;
        }
        if still_busy {
            log::warn!(
                "{} [SERIAL] poll deadline {:?} reached after {} polls — returning the last reply",
                log_header,
                poll.deadline,
                outcome.polls
            );
        }
        // the first differing reply is the operation result (or a transport error)
        outcome.exchange = reply;
        return;
    }
}

/// Executes a whole envelope on the shared port: the command exchange plus the optional
/// server-scripted poll loop. The port lock is held for the entire logical operation — the rack
/// is Master/Slave (one request on the wire at a time), so concurrent card sessions of one rack
/// interleave at operation granularity; tokio's Mutex queues the waiters FIFO-fair, which is the
/// per-port queue. Blocking serial I/O runs on a blocking thread so the async runtime isn't stalled.
/// `log_summary=false` silences the per-operation INFO line — the periodic watch loop would
/// flood the log otherwise; its caller logs only actual changes. Returns the outcome and why
/// its last read ended, which is what tells a reply the device finished from one cut short by
/// a bound (a truncated reply is rejected by the server, and the byte count alone does not
/// say which bound cut it).
pub(super) async fn execute_envelope(
    port: &SharedPort,
    env: SerialEnvelope,
    log_header: &str,
    log_summary: bool,
) -> (SerialExchange, &'static str) {
    let port = port.clone();
    let log_header_blocking = log_header.to_string();
    let idle_ms = env.idle.as_millis();
    let deadline_ms = env.deadline.as_millis();

    let queued = Instant::now();
    let (outcome, queue_ms, wire_ms) = tokio::task::spawn_blocking(move || {
        // how long this operation waited behind the other sessions of the
        // same rack — the part of a slow exchange the server's timings never
        // see, and the first suspect when a deadline cuts a reply short
        let mut guard = port.blocking_lock();
        let queue_ms = queued.elapsed().as_millis();
        let started = Instant::now();
        let outcome = run_envelope(&mut guard, &env, &log_header_blocking);
        (outcome, queue_ms, started.elapsed().as_millis())
    })
    .await
    // A join error means the blocking closure panicked before producing a
    // result — no reply was obtained, report it as such.
    .unwrap_or_else(|e| {
        log::error!("{} [SERIAL] exchange task failed: {}", log_header, e);
        (
            EnvelopeOutcome {
                exchange: SerialExchange::error(SERIAL_ERR_NO_REPLY),
                polls: 0,
                pushes: 0,
                end: READ_END_ERROR,
            },
            0,
            0,
        )
    });

    let EnvelopeOutcome {
        exchange,
        polls,
        pushes,
        end,
    } = outcome;
    // one INFO summary per logical operation; per-exchange details are at debug
    if exchange.is_ok() {
        if log_summary {
            log::info!(
                "{} [SERIAL] op done polls={} pushes={} end={} rx bytes={} idle_ms={} deadline_ms={} queue_ms={} wire_ms={}",
                log_header,
                polls,
                pushes,
                end,
                exchange.resp_hex.len() / 2,
                idle_ms,
                deadline_ms,
                queue_ms,
                wire_ms
            );
        } else {
            // the silenced summary (watch loop) still leaves a debug trace
            log::debug!(
                "{} [SERIAL] op done end={} rx bytes={} queue_ms={} wire_ms={}",
                log_header,
                end,
                exchange.resp_hex.len() / 2,
                queue_ms,
                wire_ms
            );
        }
    } else {
        log::warn!(
            "{} [SERIAL] op failed err={} end={} polls={} pushes={} partial_bytes={} idle_ms={} deadline_ms={} queue_ms={} wire_ms={}",
            log_header,
            exchange.err,
            end,
            polls,
            pushes,
            exchange.resp_hex.len() / 2,
            idle_ms,
            deadline_ms,
            queue_ms,
            wire_ms
        );
    }
    (exchange, end)
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
        let json: serde_json::Value =
            serde_json::from_str(r#"{"serial_cmd":"C0DE01"}"#).unwrap();
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

    #[test]
    fn reply_classification_follows_the_response_contract() {
        let finished = Reply { bytes: vec![0x12, 0x34], truncated: false, end: READ_END_SILENCE };
        assert_eq!(finished.into_exchange(), SerialExchange::ok("1234".into()));
        // cut by a bound: the partial bytes travel with the error code
        let cut = Reply { bytes: vec![0x12], truncated: false, end: READ_END_DEADLINE };
        assert_eq!(
            cut.into_exchange(),
            SerialExchange { resp_hex: "12".into(), err: SERIAL_ERR_TRUNCATED }
        );
        let capped = Reply { bytes: vec![0x12], truncated: true, end: READ_END_CAP };
        assert_eq!(capped.into_exchange().err, SERIAL_ERR_TRUNCATED);
        let silent = Reply { bytes: Vec::new(), truncated: false, end: READ_END_DEADLINE };
        assert_eq!(silent.into_exchange(), SerialExchange::error(SERIAL_ERR_NO_REPLY));
    }

    #[test]
    fn budget_caps_every_bound_by_the_operation_lifetime() {
        assert_eq!(Budget::NONE.remaining(T_DEADLINE), T_DEADLINE);
        assert!(!Budget::NONE.spent(T_DEADLINE));
        let short = Budget::until(std::time::Instant::now() + Duration::from_millis(50));
        assert!(short.remaining(T_DEADLINE) <= Duration::from_millis(50));
        assert!(!short.spent(T_DEADLINE));
        let over = Budget::until(std::time::Instant::now());
        std::thread::sleep(Duration::from_millis(2));
        assert!(over.spent(T_DEADLINE));
        assert_eq!(over.remaining(T_DEADLINE), Duration::ZERO);
    }

    // ── Poll primitive against a scripted port ──
    // Placeholder bytes again — only the SHAPE of the exchange is TBA's business (a command,
    // the predicted "accepted" echo, an outcome frame, the predicted "busy" poll reply, and a
    // status that is neither). The timing shape covers the case this code has to get right: a
    // reply that arrives promptly, followed by a result the device pushes on its own about one
    // idle-window later, i.e. exactly into the gap between two reads. Absolute values are
    // scaled up here so a loaded CI host cannot turn the scheduling into a coin flip.

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
        let outcome = run_envelope(&mut port, &two_phase_envelope(), "TEST |").exchange;

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
        let outcome = run_envelope(&mut port, &two_phase_envelope(), "TEST |");

        assert_eq!(outcome.exchange.resp_hex, format!("{}{}", ACCEPTED, RESULT));
        assert_eq!((outcome.polls, outcome.pushes), (0, 0));
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
        let outcome = run_envelope(&mut port, &two_phase_envelope(), "TEST |");

        assert_eq!(outcome.exchange.resp_hex, RESULT);
        assert_eq!((outcome.polls, outcome.pushes), (1, 1));
    }

    #[test]
    fn transport_failure_of_the_first_exchange_skips_the_poll_loop() {
        // A silent device must not be polled: the server needs the transport error, not a
        // status decoded from nothing.
        let port = ScriptedPort::new(&[]);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let mut env = two_phase_envelope();
        env.deadline = Duration::from_millis(50); // no point waiting a full budget in a test
        let outcome = run_envelope(&mut port, &env, "TEST |");

        assert_eq!(outcome.exchange.err, SERIAL_ERR_NO_REPLY);
        assert_eq!((outcome.polls, outcome.pushes), (0, 0));
    }

    #[test]
    fn poll_exchange_carries_bytes_that_were_already_waiting() {
        // Inside an operation, whatever is buffered belongs to that operation — the device
        // pushed it while we were between reads. It is prepended to the reply; the server's
        // parser is the one that walks a buffer of several frames.
        let port = ScriptedPort::new(&[(POLL_CMD, &[(10, BUSY)])]).with_pending(RESULT);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let (ex, _end) = exchange_once(&mut port, POLL_CMD, T_IDLE, T_DEADLINE, false, "TEST |");

        assert_eq!(ex.resp_hex, format!("{}{}", RESULT, BUSY));
    }

    #[test]
    fn first_exchange_of_an_operation_drops_what_was_left_over() {
        // At the start of an operation the port lock was just taken, so anything buffered is
        // the tail of an operation that is already over. Keeping it would prepend a foreign
        // frame to this operation's reply.
        let port = ScriptedPort::new(&[(CMD, &[(10, ACCEPTED)])]).with_pending(RESULT);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let (ex, _end) = exchange_once(&mut port, CMD, T_IDLE, T_DEADLINE, true, "TEST |");

        assert_eq!(ex.resp_hex, ACCEPTED);
    }

    #[test]
    fn stale_reply_tail_is_drained_before_the_next_command() {
        let port = ScriptedPort::new(&[(CMD, &[(10, ACCEPTED)])]).with_pending("DEAD");
        // The prefix is already buffered, but the rest of the old reply
        // arrives after the new command would previously have been sent.
        port.state.lock().unwrap().scheduled.push((
            std::time::Instant::now() + Duration::from_millis(50),
            hex::decode("BEEF").unwrap(),
        ));
        let writes = port.writes.clone();
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let (ex, _) = exchange_once(&mut port, CMD, T_IDLE, T_DEADLINE, true, "TEST |");
        assert!(ex.is_ok());
        assert_eq!(ex.resp_hex, ACCEPTED);
        assert_eq!(*writes.lock().unwrap(), vec![CMD.to_string()]);
    }

    #[test]
    fn busy_stale_line_does_not_receive_a_new_command() {
        let port = ScriptedPort::new(&[(CMD, &[(10, ACCEPTED)])]).with_pending("DEAD");
        let started = std::time::Instant::now();
        for ms in (50..=2000).step_by(50) {
            port.state.lock().unwrap().scheduled.push((
                started + Duration::from_millis(ms),
                vec![0xAB],
            ));
        }
        let writes = port.writes.clone();
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let (ex, end) = exchange_once(&mut port, CMD, T_IDLE, T_DEADLINE, true, "TEST |");
        assert_eq!(ex.err, SERIAL_ERR_NO_REPLY);
        assert_eq!(end, READ_END_DEADLINE);
        assert!(writes.lock().unwrap().is_empty());
    }

    /// One-shot envelope (no poll spec) for the given command.
    fn one_shot_envelope(cmd: &str) -> SerialEnvelope {
        SerialEnvelope {
            cmd_hex: cmd.into(),
            expect_hex: None,
            idle: T_IDLE,
            deadline: T_DEADLINE,
            poll: None,
            finish: None,
        }
    }

    #[test]
    fn a_late_reply_of_a_failed_operation_does_not_leak_into_the_next_one() {
        // A rack that missed its deadline still answers - later, on its own. Those bytes sit
        // in the port buffer, and without the resync they open the read window of the NEXT
        // operation: the server then receives two replies run together in one buffer and
        // rejects it, so the failure of one exchange spreads to the ones after it.
        let port = ScriptedPort::new(&[
            (CMD, &[(250, RESULT)]), // answers long after this operation gave up
            (POLL_CMD, &[(10, BUSY)]),
        ]);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let mut env = one_shot_envelope(CMD);
        env.deadline = Duration::from_millis(50);
        let first = run_envelope(&mut port, &env, "TEST |");
        assert_eq!(first.exchange.err, SERIAL_ERR_NO_REPLY);
        assert_eq!(first.end, READ_END_DEADLINE);

        let second = run_envelope(&mut port, &one_shot_envelope(POLL_CMD), "TEST |");
        assert_eq!(
            second.exchange.resp_hex, BUSY,
            "the late frame of the failed operation leaked into the next reply"
        );
    }

    #[test]
    fn partial_deadline_is_an_error_and_drains_the_late_tail() {
        let port = ScriptedPort::new(&[
            (CMD, &[(10, "1234"), (250, "ABCD")]),
            (POLL_CMD, &[(10, BUSY)]),
        ]);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let mut env = one_shot_envelope(CMD);
        env.idle = Duration::from_millis(300);
        env.deadline = Duration::from_millis(50);
        let first = run_envelope(&mut port, &env, "TEST |");
        assert_eq!(first.exchange.err, SERIAL_ERR_TRUNCATED);
        assert_eq!(first.exchange.resp_hex, "1234");
        assert_eq!(first.end, READ_END_DEADLINE);
        let second = run_envelope(&mut port, &one_shot_envelope(POLL_CMD), "TEST |");
        assert_eq!(second.exchange.resp_hex, BUSY);
    }

    #[test]
    fn partial_pushed_result_is_not_reported_as_success() {
        let port = ScriptedPort::new(&[(CMD, &[
            (10, ACCEPTED), (60, "AB"), (90, "CD"), (120, "EF"), (150, "12"), (400, "345678"),
        ])]);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let mut env = two_phase_envelope();
        env.idle = Duration::from_millis(40);
        env.deadline = Duration::from_millis(110);
        env.poll.as_mut().unwrap().interval = Duration::from_millis(100);
        let first = run_envelope(&mut port, &env, "TEST |");
        assert_eq!(first.exchange.err, SERIAL_ERR_TRUNCATED);
        assert_eq!(first.end, READ_END_DEADLINE);
        assert_eq!(first.pushes, 1);
    }

    #[test]
    fn read_end_reason_tells_silence_from_deadline() {
        // The two bounds cut a reply the same way in the bytes and mean opposite things:
        // silence = the device finished, deadline = it was still sending (or never started).
        let port = ScriptedPort::new(&[(CMD, &[(10, RESULT)])]);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let outcome = run_envelope(&mut port, &one_shot_envelope(CMD), "TEST |");
        assert_eq!(outcome.exchange.resp_hex, RESULT);
        assert_eq!(outcome.end, READ_END_SILENCE);
    }

    #[test]
    fn one_shot_exchange_returns_the_first_reply() {
        // No poll spec: the first reply is the whole answer.
        let port = ScriptedPort::new(&[(CMD, &[(10, RESULT)])]);
        let writes = port.writes.clone();
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let env = one_shot_envelope(CMD);
        let outcome = run_envelope(&mut port, &env, "TEST |");

        assert_eq!(outcome.exchange.resp_hex, RESULT);
        assert_eq!((outcome.polls, outcome.pushes), (0, 0));
        assert_eq!(
            *writes.lock().unwrap(),
            vec![CMD.to_string()],
            "a one-shot exchange writes the command and nothing else"
        );
    }
    #[tokio::test]
    async fn retained_exchange_keeps_port_and_pending_bytes_across_requests() {
        let scripted = ScriptedPort::new(&[
            (CMD, &[(10, "DEAD"), (400, RESULT)]),
            (POLL_CMD, &[(10, IDLE_STATUS)]),
        ]);
        let writes = scripted.writes.clone();
        let port: SharedPort = Arc::new(AsyncMutex::new(Box::new(scripted)));
        let mut lease = SerialLease::acquire(&port, 7, 3000).await.unwrap();
        let first = lease.execute(one_shot_envelope(CMD), true, "TEST").await;
        assert_eq!(first.resp_hex, "DEAD");
        assert!(port.try_lock().is_err());
        tokio::time::sleep(Duration::from_millis(450)).await;
        let second = lease.execute(one_shot_envelope(POLL_CMD), false, "TEST").await;
        assert_eq!(second.resp_hex, format!("{RESULT}{IDLE_STATUS}"));
        assert!(port.try_lock().is_err());
        drop(lease);
        assert!(port.try_lock().is_ok());
        assert_eq!(*writes.lock().unwrap(), vec![CMD.to_string(), POLL_CMD.to_string()]);
    }

    #[tokio::test]
    async fn retained_exchange_expires_without_another_server_message() {
        let scripted = ScriptedPort::new(&[]);
        let writes = scripted.writes.clone();
        let port: SharedPort = Arc::new(AsyncMutex::new(Box::new(scripted)));
        let mut lease = SerialLease::acquire(&port, 7, 50).await.unwrap();
        let guard = tokio::time::timeout(Duration::from_secs(2), port.lock()).await.unwrap();
        drop(guard);
        let reply = lease.execute(one_shot_envelope(CMD), false, "TEST").await;
        assert_eq!(reply.err, SERIAL_ERR_TRANSACTION_EXPIRED);
        assert!(writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn serial_request_duplicates_and_stale_releases_do_not_touch_the_wire() {
        use super::super::rack::{run_serial_request, IdempotencySlot};
        let scripted = ScriptedPort::new(&[(CMD, &[(10, RESULT)])]);
        let writes = scripted.writes.clone();
        let port: SharedPort = Arc::new(AsyncMutex::new(Box::new(scripted)));
        let mut state = IdempotencySlot::default();
        let body = serde_json::json!({"serial_cmd": CMD, "idle_ms": 100, "serial_hold_ms": 3000}).to_string();
        let first = run_serial_request("request/7", body.as_bytes(), &port, "TEST", &mut state, None).await;
        let duplicate = run_serial_request("request/7", body.as_bytes(), &port, "TEST", &mut state, None).await;
        assert_eq!(first, duplicate);
        let conflict = run_serial_request("request/7", b"{\"serial_cmd\":\"00\"}", &port, "TEST", &mut state, None).await.unwrap();
        assert!(conflict.1.contains("request_conflict"));
        let _ = run_serial_request("request/7", b"{\"serial_release\":7}", &port, "TEST", &mut state, None).await;
        let _ = run_serial_request("request/8", body.as_bytes(), &port, "TEST", &mut state, None).await;
        let _ = run_serial_request("request/7", b"{\"serial_release\":7}", &port, "TEST", &mut state, None).await;
        assert!(port.try_lock().is_err());
        let stale = run_serial_request("request/7", body.as_bytes(), &port, "TEST", &mut state, None).await.unwrap();
        assert!(stale.1.contains("stale_request"));
        assert_eq!(*writes.lock().unwrap(), vec![CMD.to_string(), CMD.to_string()]);
        state.reset();
        assert!(port.try_lock().is_ok());
    }

    #[tokio::test]
    async fn rack_queue_keeps_release_before_next_hold_and_other_racks_independent() {
        use super::super::rack::{request_queue, run_serial_request, IdempotencySlot};
        let scripted = ScriptedPort::new(&[(CMD, &[(10, RESULT)])]);
        let writes = scripted.writes.clone();
        let port: SharedPort = Arc::new(AsyncMutex::new(Box::new(scripted)));
        let mut state = IdempotencySlot::default();
        let body = serde_json::json!({"serial_cmd": CMD, "idle_ms": 100, "serial_hold_ms": 3000}).to_string();
        let first = run_serial_request("request/7", body.as_bytes(), &port, "TEST", &mut state, None).await.unwrap();
        assert!(first.1.contains("\"serial_err\":\"\""));
        let state = Arc::new(AsyncMutex::new(state));
        let requests = request_queue();
        let (release_gate, wait_release) = tokio::sync::oneshot::channel();
        let release_port = port.clone();
        let release_state = state.clone();
        assert!(requests.send(Box::pin(async move {
            wait_release.await.unwrap();
            let mut state = release_state.lock().await;
            assert!(run_serial_request("request/7", b"{\"serial_release\":7}", &release_port, "TEST", &mut state, None).await.is_none());
        })).is_ok());
        let next_port = port.clone();
        let next_state = state.clone();
        let (done, mut result) = tokio::sync::oneshot::channel();
        assert!(requests.send(Box::pin(async move {
            let mut state = next_state.lock().await;
            let reply = run_serial_request("request/8", body.as_bytes(), &next_port, "TEST", &mut state, None).await;
            done.send(reply).unwrap();
        })).is_ok());
        let other_rack = request_queue();
        let (other_done, other_result) = tokio::sync::oneshot::channel();
        assert!(other_rack.send(Box::pin(async move { other_done.send(()).unwrap(); })).is_ok());
        tokio::time::timeout(Duration::from_secs(1), other_result).await.unwrap().unwrap();
        assert!(matches!(result.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)));
        release_gate.send(()).unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(2), result).await.unwrap().unwrap().unwrap();
        assert!(reply.1.contains("\"serial_err\":\"\""));
        assert_eq!(*writes.lock().unwrap(), vec![CMD.to_string(), CMD.to_string()]);
        state.lock().await.reset();
        assert!(port.try_lock().is_ok());
    }

    #[tokio::test]
    async fn duplicate_transport_error_does_not_repeat_the_write() {
        use super::super::rack::{run_serial_request, IdempotencySlot};
        let scripted = ScriptedPort::new(&[]);
        let writes = scripted.writes.clone();
        let port: SharedPort = Arc::new(AsyncMutex::new(Box::new(scripted)));
        let mut state = IdempotencySlot::default();
        let body = serde_json::json!({"serial_cmd": CMD, "deadline_ms": 10}).to_string();
        let first = run_serial_request("request/7", body.as_bytes(), &port, "TEST", &mut state, None).await;
        let duplicate = run_serial_request("request/7", body.as_bytes(), &port, "TEST", &mut state, None).await;
        assert_eq!(first, duplicate);
        assert!(first.unwrap().1.contains("no_reply"));
        assert_eq!(*writes.lock().unwrap(), vec![CMD.to_string()]);
    }

}
