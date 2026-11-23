//! Retained serial operations: the port lock kept across server round trips
//! for one MQTT owner, bounded by a lifetime, and freed on its own when that
//! lifetime ends or the port's rack is unlinked.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serialport::SerialPort;

use super::envelope::SerialEnvelope;
use super::health::{finish_op, must_stop};
use super::operation::{run_envelope_inner, Budget};
use super::wire::resync_line;
use super::{
    SerialExchange, SharedPort, SERIAL_ERR_NO_REPLY, SERIAL_ERR_QUEUE_TIMEOUT,
    SERIAL_ERR_TRANSACTION_EXPIRED,
};
use crate::com_port::{lock, SHUTTING_DOWN};

/// The port lock of a retained operation, parked between the exchanges that run under it.
type PortGuard = tokio::sync::OwnedMutexGuard<Box<dyn SerialPort>>;
type HeldPort = Arc<std::sync::Mutex<Option<PortGuard>>>;

/// Locks the cell of a retained port, recovering from poisoning: the guard inside is safe to reuse.
fn held(cell: &HeldPort) -> std::sync::MutexGuard<'_, Option<PortGuard>> {
    lock(cell)
}

/// An opaque serial operation retained across server round trips, scoped to one MQTT owner.
pub(in crate::com_port) struct SerialLease {
    pub(in crate::com_port) id: u64,
    until: Instant,
    port: HeldPort,
}

impl SerialLease {
    /// Whether the lease still holds the port inside its lifetime.
    pub(in crate::com_port) fn is_live(&self) -> bool {
        Instant::now() < self.until && held(&self.port).is_some()
    }

    /// Takes the port for up to `ms` (clamped to 1..10000): the wait for the port has the same
    /// bound, and the lifetime of the operation starts once the port is acquired.
    pub(in crate::com_port) async fn acquire(
        port: &SharedPort,
        id: u64,
        ms: u64,
    ) -> Result<Self, &'static str> {
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
    pub(in crate::com_port) async fn execute(
        &mut self,
        env: SerialEnvelope,
        first: bool,
        log: &str,
    ) -> SerialExchange {
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
            finish_op(&**port);
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
            // the port stays retained only after a successful exchange inside the lifetime;
            // checked under the cell lock: `expire_lease` takes the cell once, after `until`,
            // and a guard put back behind it would stay locked with nothing to free it
            if outcome.exchange.is_ok() {
                let mut slot = held(&cell);
                if Instant::now() < until {
                    *slot = Some(port);
                }
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
///
/// Also frees it early once the port's rack is unlinked (`release_port`): the parked guard
/// holds the port lock and a strong reference to the port, and the lease's owner (a rack's
/// idempotency slot) can outlive the unlink while a request still holds it. Waiting out the
/// lifetime would keep the COM handle open for up to 10 s, past the wait for its release
/// before a reopen of the same device (`stop_rack_and_await_port_release`). The resync below
/// then returns at once (`must_stop`), so nothing more is read from a released port.
async fn expire_lease(cell: std::sync::Weak<std::sync::Mutex<Option<PortGuard>>>, until: Instant) {
    while Instant::now() < until && !SHUTTING_DOWN.load(Ordering::SeqCst) {
        // the lease is gone (released or dropped): nothing left to free
        let Some(strong) = cell.upgrade() else {
            return;
        };
        // `None` while an exchange runs under the lease: that exchange checks `must_stop`
        // itself and puts the guard back only on success, so a later tick sees it.
        let released = held(&strong)
            .as_ref()
            .is_some_and(|guard| must_stop(&***guard));
        // not held across the sleep: the cell must die with its lease
        drop(strong);
        if released {
            log::debug!("SERIAL lease status=freed reason=port_released");
            break;
        }
        let left = until.saturating_duration_since(Instant::now());
        tokio::time::sleep(Duration::from_millis(20).min(left)).await;
    }
    let Some(cell) = cell.upgrade() else {
        return;
    };
    let taken = held(&cell).take();
    if let Some(mut port) = taken {
        let _ = tokio::task::spawn_blocking(move || {
            resync_line(&mut port, "SERIAL lease expired");
            finish_op(&**port);
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::Mutex as AsyncMutex;

    use super::super::health::{port_key, release_port};
    use super::super::test_port::{
        one_shot_envelope, ScriptedPort, CMD, IDLE_STATUS, POLL_CMD, RESULT,
    };
    use super::*;

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
        let second = lease
            .execute(one_shot_envelope(POLL_CMD), false, "TEST")
            .await;
        assert_eq!(second.resp_hex, format!("{RESULT}{IDLE_STATUS}"));
        assert!(port.try_lock().is_err());
        drop(lease);
        assert!(port.try_lock().is_ok());
        assert_eq!(
            *writes.lock().unwrap(),
            vec![CMD.to_string(), POLL_CMD.to_string()]
        );
    }

    #[tokio::test]
    async fn retained_port_is_freed_once_its_rack_is_unlinked() {
        let port: SharedPort = Arc::new(AsyncMutex::new(Box::new(ScriptedPort::new(&[]))));
        let key = port_key(&**port.lock().await);
        let lease = SerialLease::acquire(&port, 7, 10_000).await.unwrap();
        assert!(port.try_lock().is_err());
        // the rack is unlinked while its lease is still parked (its owner alive)
        release_port(key, &port);
        let guard = tokio::time::timeout(Duration::from_secs(2), port.lock())
            .await
            .expect("the port must be freed long before the 10 s lifetime ends");
        drop(guard);
        assert!(!lease.is_live());
    }

    #[tokio::test]
    async fn retained_exchange_expires_without_another_server_message() {
        let scripted = ScriptedPort::new(&[]);
        let writes = scripted.writes.clone();
        let port: SharedPort = Arc::new(AsyncMutex::new(Box::new(scripted)));
        let mut lease = SerialLease::acquire(&port, 7, 50).await.unwrap();
        let guard = tokio::time::timeout(Duration::from_secs(2), port.lock())
            .await
            .unwrap();
        drop(guard);
        let reply = lease.execute(one_shot_envelope(CMD), false, "TEST").await;
        assert_eq!(reply.err, SERIAL_ERR_TRANSACTION_EXPIRED);
        assert!(writes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn serial_request_duplicates_and_stale_releases_do_not_touch_the_wire() {
        use crate::com_port::rack::{run_serial_request, IdempotencySlot};
        let scripted = ScriptedPort::new(&[(CMD, &[(10, RESULT)])]);
        let writes = scripted.writes.clone();
        let port: SharedPort = Arc::new(AsyncMutex::new(Box::new(scripted)));
        let mut state = IdempotencySlot::default();
        let body = serde_json::json!({"serial_cmd": CMD, "idle_ms": 100, "serial_hold_ms": 3000})
            .to_string();
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
        let conflict = run_serial_request(
            "request/7",
            b"{\"serial_cmd\":\"00\"}",
            &port,
            "TEST",
            &mut state,
            None,
        )
        .await
        .unwrap();
        assert!(conflict.1.contains("request_conflict"));
        let _ = run_serial_request(
            "request/7",
            b"{\"serial_release\":7}",
            &port,
            "TEST",
            &mut state,
            None,
        )
        .await;
        let _ = run_serial_request(
            "request/8",
            body.as_bytes(),
            &port,
            "TEST",
            &mut state,
            None,
        )
        .await;
        let _ = run_serial_request(
            "request/7",
            b"{\"serial_release\":7}",
            &port,
            "TEST",
            &mut state,
            None,
        )
        .await;
        assert!(port.try_lock().is_err());
        let stale = run_serial_request(
            "request/7",
            body.as_bytes(),
            &port,
            "TEST",
            &mut state,
            None,
        )
        .await
        .unwrap();
        assert!(stale.1.contains("stale_request"));
        assert_eq!(
            *writes.lock().unwrap(),
            vec![CMD.to_string(), CMD.to_string()]
        );
        state.reset();
        assert!(port.try_lock().is_ok());
    }

    #[tokio::test]
    async fn rack_queue_keeps_release_before_next_hold_and_other_racks_independent() {
        use crate::com_port::rack::{request_queue, run_serial_request, IdempotencySlot};
        let scripted = ScriptedPort::new(&[(CMD, &[(10, RESULT)])]);
        let writes = scripted.writes.clone();
        let port: SharedPort = Arc::new(AsyncMutex::new(Box::new(scripted)));
        let mut state = IdempotencySlot::default();
        let body = serde_json::json!({"serial_cmd": CMD, "idle_ms": 100, "serial_hold_ms": 3000})
            .to_string();
        let first = run_serial_request(
            "request/7",
            body.as_bytes(),
            &port,
            "TEST",
            &mut state,
            None,
        )
        .await
        .unwrap();
        assert!(first.1.contains("\"serial_err\":\"\""));
        let state = Arc::new(AsyncMutex::new(state));
        let requests = request_queue();
        let (release_gate, wait_release) = tokio::sync::oneshot::channel();
        let release_port = port.clone();
        let release_state = state.clone();
        assert!(requests
            .send(Box::pin(async move {
                wait_release.await.unwrap();
                let mut state = release_state.lock().await;
                assert!(run_serial_request(
                    "request/7",
                    b"{\"serial_release\":7}",
                    &release_port,
                    "TEST",
                    &mut state,
                    None
                )
                .await
                .is_none());
            }))
            .is_ok());
        let next_port = port.clone();
        let next_state = state.clone();
        let (done, mut result) = tokio::sync::oneshot::channel();
        assert!(requests
            .send(Box::pin(async move {
                let mut state = next_state.lock().await;
                let reply = run_serial_request(
                    "request/8",
                    body.as_bytes(),
                    &next_port,
                    "TEST",
                    &mut state,
                    None,
                )
                .await;
                done.send(reply).unwrap();
            }))
            .is_ok());
        let other_rack = request_queue();
        let (other_done, other_result) = tokio::sync::oneshot::channel();
        assert!(other_rack
            .send(Box::pin(async move {
                other_done.send(()).unwrap();
            }))
            .is_ok());
        tokio::time::timeout(Duration::from_secs(1), other_result)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            result.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        release_gate.send(()).unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(2), result)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(reply.1.contains("\"serial_err\":\"\""));
        assert_eq!(
            *writes.lock().unwrap(),
            vec![CMD.to_string(), CMD.to_string()]
        );
        state.lock().await.reset();
        assert!(port.try_lock().is_ok());
    }
}
