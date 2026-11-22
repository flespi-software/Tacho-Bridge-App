//! One logical operation of a session: the command exchange plus the optional
//! server-scripted poll loop, run under the port lock for its whole length.
//! `execute_envelope` is the entry point; it queues for the port on the async
//! side and runs the blocking core on a blocking thread.

use std::time::{Duration, Instant};

use serialport::SerialPort;

use super::envelope::{PollSpec, SerialEnvelope};
use super::health::{finish_op, must_stop, note_failed_op};
use super::wire::{
    exchange_once, resync_line, wait_for_push, READ_END_DEADLINE, READ_END_ERROR, READ_END_SHUTDOWN,
};
use super::{
    SerialExchange, SharedPort, SERIAL_ERR_NO_REPLY, SERIAL_ERR_QUEUE_TIMEOUT,
    SERIAL_ERR_TRANSACTION_EXPIRED,
};

/// Longest wait for the port behind the other operations of the same rack.
/// Kept under the MQTT keep-alive of the waiting session (see `MQTT_KEEP_ALIVE_SECS` in `mqtt::client`).
const SERIAL_QUEUE_WAIT_MAX: Duration = Duration::from_secs(60);

/// Time left to the retained operation an exchange runs under; `NONE` for a plain operation.
#[derive(Clone, Copy)]
pub(super) struct Budget(Option<Instant>);

impl Budget {
    const NONE: Budget = Budget(None);

    /// The budget of a retained operation that ends at `until`.
    pub(super) fn until(until: Instant) -> Self {
        Budget(Some(until))
    }

    /// `limit` cut down to what is left of the retained operation.
    fn remaining(self, limit: Duration) -> Duration {
        self.0.map_or(limit, |until| {
            limit.min(until.saturating_duration_since(Instant::now()))
        })
    }

    /// True when nothing is left of `limit` under this budget.
    fn spent(self, limit: Duration) -> bool {
        self.remaining(limit).is_zero()
    }
}

/// Result of one logical operation and how it was obtained.
pub(super) struct EnvelopeOutcome {
    pub(super) exchange: SerialExchange,
    /// Status requests sent by the poll loop.
    pub(super) polls: u32,
    /// Frames the device sent on its own during the poll loop.
    pub(super) pushes: u32,
    /// Why the last read ended, one of the `READ_END_*` reasons.
    pub(super) end: &'static str,
}

/// The blocking core of one logical operation on an already-locked port: the command exchange
/// plus the optional server-scripted poll loop.
///
/// The contract this implements: when the first reply equals the predicted `expect` bytes, the
/// command is still running and the server-scripted poll collects its result. Bytes may arrive
/// between polls as well, and they belong to the running operation. Hence the two rules of the
/// poll loop: never stop listening between reads, and never drop bytes that arrived while we
/// were not reading.
fn run_envelope(
    port: &mut Box<dyn SerialPort>,
    env: &SerialEnvelope,
    log_header: &str,
) -> EnvelopeOutcome {
    run_envelope_inner(port, env, true, Budget::NONE, log_header)
}

/// `purge_stale` is false for a continuation of a retained operation: its pending bytes are
/// carried into the reply instead of dropped. `budget` caps every wait by the operation lifetime.
pub(super) fn run_envelope_inner(
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
    // the poll loop is entered only when the device answered exactly the predicted `expect`
    // bytes; anything else (a transport error, a refusal, an instant result) goes back as is
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
        note_failed_op(&**port);
        resync_line(port, log_header);
    }
    outcome
}

/// The server-scripted poll loop after a command that is still running. Listens for bytes the
/// device sends between the polls and leaves in `outcome` the first reply that is
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
        // App closing or rack unlinked: abandon the operation (see `must_stop`).
        if must_stop(&**port) {
            outcome.end = READ_END_SHUTDOWN;
            outcome.exchange = SerialExchange::error(SERIAL_ERR_NO_REPLY);
            return;
        }
        // listen through the poll interval instead of sleeping through it: bytes arriving
        // in an unwatched gap would be lost
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
/// server-scripted poll loop. The port lock is held for the entire logical operation — one
/// operation is on the wire at a time, so concurrent card sessions of one rack
/// interleave at operation granularity; tokio's Mutex queues the waiters FIFO-fair, which is the
/// per-port queue. Blocking serial I/O runs on a blocking thread so the async runtime isn't stalled.
/// `log_summary=false` silences the per-operation INFO line — the periodic watch loop would
/// flood the log otherwise; its caller logs only actual changes. Returns the outcome and why
/// its last read ended, which is what tells a reply the device finished from one cut short by
/// a bound (a truncated reply is rejected by the server, and the byte count alone does not
/// say which bound cut it).
pub(in crate::com_port) async fn execute_envelope(
    port: &SharedPort,
    env: SerialEnvelope,
    log_header: &str,
    log_summary: bool,
) -> (SerialExchange, &'static str) {
    let (reply, end, _) = execute_envelope_timed(port, env, log_header, log_summary).await;
    (reply, end)
}

/// Wire duration excludes the port queue, for estimating final LED cleanup.
pub(in crate::com_port) async fn execute_envelope_timed(
    port: &SharedPort,
    env: SerialEnvelope,
    log_header: &str,
    log_summary: bool,
) -> (SerialExchange, &'static str, Duration) {
    let port = port.clone();
    let log_header_blocking = log_header.to_string();
    let idle_ms = env.idle.as_millis();
    let deadline_ms = env.deadline.as_millis();

    let queued = Instant::now();
    // The port is queued for on the async side: an aborted session (card
    // moved, rack restart) then leaves the queue without ever writing its
    // command, and holds no port reference that would keep the rack from
    // being closed. A blocking-side wait could not be cancelled.
    // Bounded like `SerialLease::acquire`: a session parked here does not
    // poll its MQTT event loop, and a wait longer than the broker keep-alive
    // would cost it the connection (and the reply) anyway.
    let Ok(mut guard) = tokio::time::timeout(SERIAL_QUEUE_WAIT_MAX, port.lock_owned()).await else {
        log::warn!(
            "{} [SERIAL] op failed err={} queue_ms={}",
            log_header,
            SERIAL_ERR_QUEUE_TIMEOUT,
            queued.elapsed().as_millis()
        );
        return (
            SerialExchange::error(SERIAL_ERR_QUEUE_TIMEOUT),
            READ_END_DEADLINE,
            Duration::ZERO,
        );
    };
    // how long this operation waited behind the other sessions of the
    // same rack — the part of a slow exchange the server's timings never
    // see, and the first suspect when a deadline cuts a reply short
    let queue_ms = queued.elapsed().as_millis();
    let (outcome, queue_ms, wire_ms) = tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let outcome = run_envelope(&mut guard, &env, &log_header_blocking);
        finish_op(&**guard);
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
    (
        exchange,
        end,
        Duration::from_millis(wire_ms.min(u64::MAX as u128) as u64),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::Mutex as AsyncMutex;

    use super::super::test_port::{
        one_shot_envelope, two_phase_envelope, ScriptedPort, ACCEPTED, BUSY, CMD, IDLE_STATUS,
        POLL_CMD, RESULT, T_DEADLINE,
    };
    use super::super::wire::READ_END_SILENCE;
    use super::super::SERIAL_ERR_TRUNCATED;
    use super::*;

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

    #[test]
    fn result_pushed_after_the_accepted_window_is_not_lost() {
        // THE REGRESSION. The predicted first reply comes back at once and the result
        // arrives unasked just over one idle window later — into the gap between two reads.
        // That gap used to be a blind sleep followed by a buffer purge, so the result was
        // destroyed, the next poll got a reply without it, and the whole operation failed.
        let port = ScriptedPort::new(&[
            (CMD, &[(10, ACCEPTED), (150, RESULT)]),
            // a poll after the result no longer carries it
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
        // inside the read window of the predicted first reply. The buffer then differs from the
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
        // Timings scaled x3 from the original script: at 10 ms granularity the push was
        // swallowed into the first window whenever the test thread was scheduled late.
        let port = ScriptedPort::new(&[(
            CMD,
            &[
                (30, ACCEPTED),
                (180, "AB"),
                (270, "CD"),
                (360, "EF"),
                (450, "12"),
                (1200, "345678"),
            ],
        )]);
        let mut port: Box<dyn SerialPort> = Box::new(port);
        let mut env = two_phase_envelope();
        env.idle = Duration::from_millis(120);
        env.deadline = Duration::from_millis(330);
        env.poll.as_mut().unwrap().interval = Duration::from_millis(300);
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
    async fn duplicate_transport_error_does_not_repeat_the_write() {
        use crate::com_port::rack::{run_serial_request, IdempotencySlot};
        let scripted = ScriptedPort::new(&[]);
        let writes = scripted.writes.clone();
        let port: SharedPort = Arc::new(AsyncMutex::new(Box::new(scripted)));
        let mut state = IdempotencySlot::default();
        let body = serde_json::json!({"serial_cmd": CMD, "deadline_ms": 10}).to_string();
        let first = run_serial_request(
            "request/7",
            body.as_bytes(),
            &port,
            "TEST",
            &mut state,
            None,
        )
        .await;
        let duplicate = run_serial_request(
            "request/7",
            body.as_bytes(),
            &port,
            "TEST",
            &mut state,
            None,
        )
        .await;
        assert_eq!(first, duplicate);
        assert!(first.unwrap().1.contains("no_reply"));
        assert_eq!(*writes.lock().unwrap(), vec![CMD.to_string()]);
    }
}
