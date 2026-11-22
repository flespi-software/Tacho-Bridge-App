//! The wire under one port lock: reading one reply, one write+read exchange,
//! and bringing the line back to quiet after a failure. Every function here
//! blocks; callers run it on a blocking thread with the port already locked.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use serialport::SerialPort;

use crate::debug_log::frame_digest;

use super::envelope::SERIAL_MS_MIN;
use super::health::{
    late_reply_possible, must_stop, note_io_fault, note_io_ok, port_key, record_unsolicited,
};
use super::{
    SerialExchange, SERIAL_ERR_BAD_HEX, SERIAL_ERR_NO_REPLY, SERIAL_ERR_TRANSACTION_EXPIRED,
    SERIAL_ERR_TRUNCATED, SERIAL_ERR_WRITE_FAILED,
};

/// Upper bound on a single serial reply. A healthy rack answers with small
/// frames; hitting this means the device is streaming garbage. Without the cap
/// a device that never stops sending would grow the buffer without bound and
/// keep the read loop (and the port lock) stuck forever.
const SERIAL_REPLY_MAX_BYTES: usize = 64 * 1024;

/// Why one serial read ended. Log-only (the server never sees these), but the
/// distinction is what tells a reply cut by the inter-byte silence from one cut
/// by the total deadline, and either from a rack that never answered at all.
pub(super) const READ_END_SILENCE: &str = "silence";
pub(super) const READ_END_DEADLINE: &str = "deadline";
const READ_END_CAP: &str = "cap";
pub(super) const READ_END_SHUTDOWN: &str = "shutdown";
pub(super) const READ_END_FRAME: &str = "frame";
pub(super) const READ_END_EOF: &str = "eof";
pub(super) const READ_END_ERROR: &str = "error";

/// Bounds of line resync after a failed operation or stale bytes before TX: the rack answers a
/// command it did not answer in time LATER, on its own, and those bytes would
/// otherwise open the next read window and make two good frames decode as one
/// broken one on the server. `QUIET` is the silence that ends the resync,
/// `BUDGET` caps the whole of it, `SLICE` is how often the line is sampled.
const SERIAL_RESYNC_QUIET: Duration = Duration::from_millis(300);
const SERIAL_RESYNC_BUDGET: Duration = Duration::from_millis(1500);
const SERIAL_RESYNC_SLICE: Duration = Duration::from_millis(20);

/// One blocking `port.read` never waits longer than this slice, whatever the
/// server-supplied timings say: the read loop re-checks its deadlines and the
/// app shutdown flag between slices. This is what keeps the uncancellable
/// `spawn_blocking` serial closures from pinning the port (and a blocking-pool
/// thread) for up to `SERIAL_MS_MAX` after the app started closing.
pub(super) const SERIAL_READ_SLICE: Duration = Duration::from_millis(500);

/// Bound of one command write: frames are small, a healthy port drains them at once.
pub(super) const SERIAL_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// Bytes of one read and why it ended.
pub(super) struct Reply {
    bytes: Vec<u8>,
    /// The size cap cut the reply.
    truncated: bool,
    /// One of the `READ_END_*` reasons.
    pub(super) end: &'static str,
}

impl Reply {
    pub(super) fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn hex(&self) -> String {
        hex::encode_upper(&self.bytes)
    }

    /// The exchange the server gets: a reply the device finished is a success, one cut by the
    /// size cap or by a bound is truncated (partial data plus the error code, so the server sees
    /// what came through AND knows the exchange is unusable), no bytes at all is no reply.
    pub(super) fn into_exchange(self) -> SerialExchange {
        if self.truncated
            || (!self.is_empty() && !matches!(self.end, READ_END_SILENCE | READ_END_FRAME))
        {
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
/// `None` when the port itself failed (a dead handle), as opposed to an empty
/// buffer.
pub(super) fn drain_buffered(port: &mut Box<dyn SerialPort>, log_header: &str) -> Option<Vec<u8>> {
    let pending = match port.bytes_to_read() {
        Ok(0) => return Some(Vec::new()),
        Ok(n) => (n as usize).min(SERIAL_REPLY_MAX_BYTES),
        Err(e) => {
            log::warn!("{} [SERIAL] bytes_to_read failed: {}", log_header, e);
            note_io_fault(&**port);
            return None;
        }
    };
    let mut buf = vec![0u8; pending];
    match port.read(&mut buf) {
        Ok(n) => {
            buf.truncate(n);
            if n > 0 {
                note_io_ok(&**port);
            }
            Some(buf)
        }
        Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => Some(Vec::new()),
        Err(e) => {
            log::warn!("{} [SERIAL] drain read failed: {}", log_header, e);
            note_io_fault(&**port);
            None
        }
    }
}

/// Reads one reply off the port. Two timings are involved and they are NOT the same order
/// of magnitude:
///
///   * time to the FIRST byte (`first_wait`) — after a write, the rack has to receive the
///     whole command before it starts answering, so this scales with the command size (a
///     frame of a few hundred bytes takes tens of milliseconds on the wire) and carries the
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
    read_reply_inner(
        port,
        carry,
        first_wait,
        idle,
        deadline,
        log_header,
        ReadMode::Normal,
    )
}

/// Tells from the bytes read so far whether a reply is complete; supplied by
/// the caller, the transport itself never interprets the bytes.
pub(in crate::com_port) type ReplyComplete = Box<dyn Fn(&[u8]) -> bool + Send>;

/// Which traffic a read serves: regular operations, or the final cleanup that
/// still reads after the port was released (optionally ending a reply as soon
/// as it is complete).
pub(super) enum ReadMode<'a> {
    Normal,
    Shutdown(Option<&'a ReplyComplete>),
}

/// Only the final indicator cleanup may read after normal traffic was stopped.
pub(super) fn read_reply_inner(
    port: &mut Box<dyn SerialPort>,
    carry: Vec<u8>,
    first_wait: Duration,
    idle: Duration,
    deadline: Duration,
    log_header: &str,
    mode: ReadMode<'_>,
) -> Reply {
    let mut reply = Reply {
        bytes: carry,
        truncated: false,
        end: READ_END_SILENCE,
    };
    let mut first_byte_pending = reply.is_empty();
    let framed_read = matches!(mode, ReadMode::Shutdown(Some(_)));

    let mut buf = [0u8; 512];
    let read_started = Instant::now();
    // the total bound must never undercut the first-byte budget it contains
    let total = deadline.max(first_wait);
    let read_deadline = read_started + total;
    // The silence bound that ends the reply: first-byte budget while nothing
    // has arrived yet, line-idle from the last received byte afterwards.
    let mut silence_deadline = read_started + if first_byte_pending { first_wait } else { idle };
    loop {
        if let ReadMode::Shutdown(Some(complete)) = &mode {
            if complete(&reply.bytes) {
                reply.end = READ_END_FRAME;
                break;
            }
        }
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
        // App closing or rack unlinked: stop waiting (see `must_stop`).
        if matches!(mode, ReadMode::Normal) && must_stop(&**port) {
            log::info!(
                "{} [SERIAL] read stopped reason=port_released bytes={}",
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
        // A sub-millisecond timeout rounds down to zero in the serial driver.
        let slice = wait.min(SERIAL_READ_SLICE).max(Duration::from_millis(1));
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
                // A hung-up handle reads EOF instead of failing: count it, or
                // the dead-handle reopen never triggers.
                note_io_fault(&**port);
                reply.end = READ_END_EOF;
                break;
            }
            Ok(n) => {
                note_io_ok(&**port);
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
                silence_deadline = if framed_read {
                    read_deadline
                } else {
                    Instant::now() + idle
                };
            }
            // A timed-out slice is not the end of the reply by itself — the
            // loop re-evaluates the silence bound and keeps listening.
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => {
                log::warn!("{} [SERIAL] read error: {}", log_header, e);
                note_io_fault(&**port);
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
pub(super) fn resync_line(port: &mut Box<dyn SerialPort>, log_header: &str) -> bool {
    let started = Instant::now();
    let mut last_byte = started;
    let mut dropped = 0usize;
    let mut quiet = false;
    loop {
        if started.elapsed() >= SERIAL_RESYNC_BUDGET || must_stop(&**port) {
            break;
        }
        // A port that fails outright will not go quiet by waiting: give up
        // at once instead of repeating the failing call for the whole budget.
        let Some(late) = drain_buffered(port, log_header) else {
            break;
        };
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

/// Listens for up to `interval` for bytes the device sends without being asked, and reads
/// them whole once they start. This replaces the blind sleep that used to sit between polls:
/// bytes that arrive while nothing reads the line must never be lost, since they may be the
/// result of the running operation. An empty reply means the interval elapsed in silence,
/// time to send the next poll.
pub(super) fn wait_for_push(
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
///   * any later exchange (a poll of the same operation) — bytes that arrived since the
///     previous read belong to the running operation, not to an earlier one.
///     They are carried into the reply and the
///     server's multi-frame parser picks the outcome frame out of the concatenation.
///
/// Blind-clearing on every write is what ate the result of every card operation slow
/// enough to finish between two read windows, after which the slot reports plain idle.
fn settle_line(
    port: &mut Box<dyn SerialPort>,
    purge_stale: bool,
    log_header: &str,
) -> Result<Vec<u8>, (SerialExchange, &'static str)> {
    let Some(pending) = drain_buffered(port, log_header) else {
        // the port failed: writing to it would fail the same way
        return Err((
            SerialExchange::error(SERIAL_ERR_WRITE_FAILED),
            READ_END_ERROR,
        ));
    };
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
    if late_reply_possible(&**port) {
        log::debug!(
            "{} [SERIAL] stale bytes taken for a late reply, not a rack event",
            log_header
        );
    } else {
        record_unsolicited(port_key(&**port), &pending);
    }
    if !resync_line(port, log_header) {
        log::warn!("{} [SERIAL] tx skipped reason=line_not_quiet", log_header);
        return Err((
            SerialExchange::error(SERIAL_ERR_NO_REPLY),
            READ_END_DEADLINE,
        ));
    }
    Ok(Vec::new())
}

/// One write+read exchange on an already-locked port. `deadline` bounds the whole read phase,
/// including the wait for the rack's first reply byte; `idle` is the line silence that ends a
/// reply once it has started. `purge_stale` tells whether pending bytes belong to a previous
/// operation (dropped) or to this one (carried into the reply) — see `settle_line`. Returns the
/// exchange and why its read ended. Payload hex only at debug — the rack protocol must not end
/// up in users' log files at INFO level.
pub(super) fn exchange_once(
    port: &mut Box<dyn SerialPort>,
    cmd_hex: &str,
    idle: Duration,
    deadline: Duration,
    purge_stale: bool,
    log_header: &str,
) -> (SerialExchange, &'static str) {
    // Queued work can acquire the port after cleanup has darkened the rack.
    // It must not write another command (in particular an LED-on command).
    if must_stop(&**port) {
        return (
            SerialExchange::error(SERIAL_ERR_NO_REPLY),
            READ_END_SHUTDOWN,
        );
    }
    // A spent lease budget leaves no time to read the reply: the device would
    // act on a command whose answer is thrown away.
    if deadline < Duration::from_millis(SERIAL_MS_MIN) {
        return (
            SerialExchange::error(SERIAL_ERR_TRANSACTION_EXPIRED),
            READ_END_DEADLINE,
        );
    }
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
    // The port keeps the timeout of the last read slice, possibly a fraction
    // of a millisecond: a momentarily full TX buffer would fail the write.
    let _ = port.set_timeout(SERIAL_WRITE_TIMEOUT);
    if let Err(e) = port.write_all(&bytes) {
        log::error!("{} [SERIAL] write failed: {}", log_header, e);
        // A slow drain is not a dead handle.
        if e.kind() != std::io::ErrorKind::TimedOut {
            note_io_fault(&**port);
        }
        return (
            SerialExchange::error(SERIAL_ERR_WRITE_FAILED),
            READ_END_ERROR,
        );
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

#[cfg(test)]
mod tests {
    use super::super::test_port::{
        ScriptedPort, ACCEPTED, BUSY, CMD, POLL_CMD, RESULT, T_DEADLINE, T_IDLE,
    };
    use super::*;

    #[test]
    fn reply_classification_follows_the_response_contract() {
        let finished = Reply {
            bytes: vec![0x12, 0x34],
            truncated: false,
            end: READ_END_SILENCE,
        };
        assert_eq!(finished.into_exchange(), SerialExchange::ok("1234".into()));
        let framed = Reply {
            bytes: vec![0x12, 0x34],
            truncated: false,
            end: READ_END_FRAME,
        };
        assert_eq!(framed.into_exchange(), SerialExchange::ok("1234".into()));
        // cut by a bound: the partial bytes travel with the error code
        let cut = Reply {
            bytes: vec![0x12],
            truncated: false,
            end: READ_END_DEADLINE,
        };
        assert_eq!(
            cut.into_exchange(),
            SerialExchange {
                resp_hex: "12".into(),
                err: SERIAL_ERR_TRUNCATED
            }
        );
        let capped = Reply {
            bytes: vec![0x12],
            truncated: true,
            end: READ_END_CAP,
        };
        assert_eq!(capped.into_exchange().err, SERIAL_ERR_TRUNCATED);
        let silent = Reply {
            bytes: Vec::new(),
            truncated: false,
            end: READ_END_DEADLINE,
        };
        assert_eq!(
            silent.into_exchange(),
            SerialExchange::error(SERIAL_ERR_NO_REPLY)
        );
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
            port.state
                .lock()
                .unwrap()
                .scheduled
                .push((started + Duration::from_millis(ms), vec![0xAB]));
        }
        let writes = port.writes.clone();
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let (ex, end) = exchange_once(&mut port, CMD, T_IDLE, T_DEADLINE, true, "TEST |");
        assert_eq!(ex.err, SERIAL_ERR_NO_REPLY);
        assert_eq!(end, READ_END_DEADLINE);
        assert!(writes.lock().unwrap().is_empty());
    }
}
