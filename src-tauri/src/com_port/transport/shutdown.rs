//! The final indicator cleanup on app close: profile-supplied commands written
//! after all regular rack work has stopped, under one deadline for the batch.

use std::io::Write;
use std::time::{Duration, Instant};

use super::wire::{
    drain_buffered, read_reply_inner, ReadMode, ReplyComplete, READ_END_EOF, READ_END_ERROR,
    READ_END_FRAME, SERIAL_READ_SLICE, SERIAL_WRITE_TIMEOUT,
};
use super::SharedPort;

/// Final, profile-supplied indicator commands, after all regular rack work
/// has stopped. Owns the port for the whole batch; unlike normal exchanges,
/// these reads must still run with SHUTTING_DOWN / release_port set.
/// Both queueing and I/O share one deadline, including silent reader addresses.
pub(in crate::com_port) async fn write_shutdown_commands(
    port: SharedPort,
    commands: Vec<ShutdownCommand>,
    idle: Duration,
    reply_timeout: Duration,
    budget: Duration,
    log_header: String,
) {
    let started = Instant::now();
    let until = started + budget;
    let Ok(mut guard) = tokio::time::timeout(budget, port.lock_owned()).await else {
        log::warn!("{} [LED] phase=shutdown status=port_busy", log_header);
        return;
    };
    let _ = tokio::task::spawn_blocking(move || {
        let mut written = 0;
        let mut answered = 0;
        let mut framed = 0;
        let mut exchanges = 0;
        let mut exchange_us = 0;
        let mut max_exchange_us = 0;
        let mut failed = false;
        for command in &commands {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            // Discard the tail of the abandoned operation / previous LED reply.
            // No protocol bytes or reply interpretation live in this transport.
            if guard.set_timeout(SERIAL_READ_SLICE.min(left)).is_err()
                || drain_buffered(&mut guard, &log_header).is_none()
            {
                failed = true;
                break;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let exchange_started = Instant::now();
            if guard.set_timeout(SERIAL_WRITE_TIMEOUT.min(left)).is_err()
                || guard.write_all(&command.bytes).is_err()
            {
                failed = true;
                break;
            }
            written += 1;
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let wait = reply_timeout.min(left);
            let reply = read_reply_inner(
                &mut guard, Vec::new(), wait, idle.min(left), wait, &log_header,
                ReadMode::Shutdown(command.complete.as_ref()),
            );
            let took_us = exchange_started.elapsed().as_micros();
            exchanges += 1;
            exchange_us += took_us;
            max_exchange_us = max_exchange_us.max(took_us);
            if !reply.is_empty() {
                answered += 1;
            }
            if reply.end == READ_END_FRAME {
                framed += 1;
            }
            if matches!(reply.end, READ_END_ERROR | READ_END_EOF)
                || (command.complete.is_some() && !reply.is_empty() && reply.end != READ_END_FRAME)
            {
                failed = true;
                break;
            }
        }
        if failed || written != commands.len() {
            log::warn!(
                "{} [LED] phase=shutdown status=incomplete written={} total={} replies={} framed={} failed={} elapsed_ms={} avg_exchange_ms={:.2} max_exchange_ms={:.2}",
                log_header, written, commands.len(), answered, framed, failed, started.elapsed().as_millis(),
                exchange_us as f64 / exchanges.max(1) as f64 / 1000.0, max_exchange_us as f64 / 1000.0
            );
        } else {
            log::info!(
                "{} [LED] phase=shutdown status=off_sent written={} replies={} framed={} elapsed_ms={} avg_exchange_ms={:.2} max_exchange_ms={:.2}",
                log_header, written, answered, framed, started.elapsed().as_millis(),
                exchange_us as f64 / exchanges.max(1) as f64 / 1000.0, max_exchange_us as f64 / 1000.0
            );
        }
    }).await.map_err(|e| log::error!("Indicator cleanup task failed: {}", e));
}

/// One final command: its bytes and, when the caller can tell, when its reply
/// is complete (the read then ends at once instead of waiting for silence).
pub(in crate::com_port) struct ShutdownCommand {
    pub bytes: Vec<u8>,
    pub complete: Option<ReplyComplete>,
}

#[cfg(test)]
impl From<Vec<u8>> for ShutdownCommand {
    fn from(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            complete: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serialport::SerialPort;
    use tokio::sync::Mutex as AsyncMutex;

    use super::super::health::{port_key, release_port};
    use super::super::operation::execute_envelope;
    use super::super::test_port::{two_phase_envelope, ScriptedPort, ACCEPTED, CMD, POLL_CMD};
    use super::super::wire::READ_END_SHUTDOWN;
    use super::*;

    #[tokio::test]
    async fn shutdown_advances_at_the_full_reply_without_waiting_for_silence() {
        // The first reply is fragmented across an idle window. Receiving the
        // first part is insufficient; receiving its last byte is sufficient.
        for idle_ms in [50, 1500] {
            let scripted = ScriptedPort::new(&[
                (CMD, &[(5, "AC01"), (80, "020304")]),
                (POLL_CMD, &[(5, "AC02030405")]),
            ]);
            let writes = scripted.writes.clone();
            let port: SharedPort = Arc::new(AsyncMutex::new(Box::new(scripted)));
            let commands = [(CMD, "AC01020304"), (POLL_CMD, "AC02030405")]
                .into_iter()
                .map(|(cmd, expected)| {
                    let expected = hex::decode(expected).unwrap();
                    ShutdownCommand {
                        bytes: hex::decode(cmd).unwrap(),
                        complete: Some(Box::new(move |bytes| bytes.ends_with(&expected))),
                    }
                })
                .collect();
            let started = Instant::now();
            write_shutdown_commands(
                port,
                commands,
                Duration::from_millis(idle_ms),
                Duration::from_secs(2),
                Duration::from_secs(5),
                "TEST |".into(),
            )
            .await;
            assert_eq!(*writes.lock().unwrap(), [CMD, POLL_CMD]);
            assert!(started.elapsed() >= Duration::from_millis(80));
            assert!(started.elapsed() < Duration::from_secs(1));
        }
    }

    #[tokio::test]
    async fn shutdown_does_not_send_the_next_command_after_an_incomplete_reply() {
        let scripted = ScriptedPort::new(&[(CMD, &[(5, "AC01")])]);
        let writes = scripted.writes.clone();
        let port: SharedPort = Arc::new(AsyncMutex::new(Box::new(scripted)));
        let command = || ShutdownCommand {
            bytes: hex::decode(CMD).unwrap(),
            complete: Some(Box::new(|bytes| bytes.ends_with(&[0xAC, 1, 2, 3, 4]))),
        };
        write_shutdown_commands(
            port,
            vec![command(), command()],
            Duration::from_millis(2),
            Duration::from_millis(50),
            Duration::from_secs(1),
            "TEST |".into(),
        )
        .await;
        assert_eq!(*writes.lock().unwrap(), [CMD]);
    }

    #[tokio::test]
    async fn shutdown_writes_all_180_slots_on_a_released_port_even_without_replies() {
        let scripted = ScriptedPort::new(&[]);
        let writes = scripted.writes.clone();
        let boxed: Box<dyn SerialPort> = Box::new(scripted);
        let key = port_key(&*boxed);
        let port: SharedPort = Arc::new(AsyncMutex::new(boxed));
        release_port(key, &port);
        let commands: Vec<_> = (1..=180).map(|slot| vec![0xD2, 0x4B, slot, 0xB7]).collect();
        write_shutdown_commands(
            port.clone(),
            commands
                .iter()
                .cloned()
                .map(ShutdownCommand::from)
                .collect(),
            Duration::from_millis(1),
            Duration::from_millis(1),
            Duration::from_secs(3),
            "TEST |".into(),
        )
        .await;
        assert_eq!(
            *writes.lock().unwrap(),
            commands.iter().map(hex::encode_upper).collect::<Vec<_>>()
        );
        // A queued ordinary command cannot turn an indicator back on afterwards.
        let outcome = execute_envelope(&port, two_phase_envelope(), "TEST |", false).await;
        assert_eq!(outcome.1, READ_END_SHUTDOWN);
        assert_eq!(writes.lock().unwrap().len(), 180);
    }

    #[tokio::test]
    async fn shutdown_still_reads_the_off_command_reply_after_port_release() {
        let scripted = ScriptedPort::new(&[(CMD, &[(5, ACCEPTED)])]);
        let boxed: Box<dyn SerialPort> = Box::new(scripted);
        let key = port_key(&*boxed);
        let port: SharedPort = Arc::new(AsyncMutex::new(boxed));
        release_port(key, &port);
        write_shutdown_commands(
            port.clone(),
            vec![hex::decode(CMD).unwrap().into()],
            Duration::from_millis(2),
            Duration::from_millis(100),
            Duration::from_secs(1),
            "TEST |".into(),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(port.lock().await.bytes_to_read().unwrap(), 0);
    }

    #[tokio::test]
    async fn shutdown_does_not_leave_a_delayed_writer_behind_when_the_port_is_busy() {
        let scripted = ScriptedPort::new(&[]);
        let writes = scripted.writes.clone();
        let port: SharedPort = Arc::new(AsyncMutex::new(Box::new(scripted)));
        let guard = port.lock().await;
        write_shutdown_commands(
            port.clone(),
            vec![vec![0xD2, 0x4B, 1, 0xB7].into()],
            Duration::from_millis(1),
            Duration::from_millis(1),
            Duration::from_millis(10),
            "TEST |".into(),
        )
        .await;
        drop(guard);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn shutdown_has_one_deadline_for_the_whole_batch() {
        let scripted = ScriptedPort::new(&[]);
        let writes = scripted.writes.clone();
        let port: SharedPort = Arc::new(AsyncMutex::new(Box::new(scripted)));
        let started = Instant::now();
        write_shutdown_commands(
            port,
            (0..180).map(|_| vec![0xD2, 0x4B, 1, 0xB7].into()).collect(),
            Duration::from_millis(2),
            Duration::from_millis(50),
            Duration::from_millis(75),
            "TEST |".into(),
        )
        .await;
        assert!(writes.lock().unwrap().len() < 180);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
