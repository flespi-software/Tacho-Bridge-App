//! One rack card session: its MQTT loop (the rack link report, the server's
//! answer to it, the envelopes of the card) and the serial worker that runs
//! the envelopes against the rack's port off that loop.

use rumqttc::v5::mqttbytes::v5::Publish;
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::{AsyncClient, ConnectionError, Event, Incoming};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tauri::async_runtime::{self, JoinHandle};

use crate::com_port::rack::{publish_reply, run_serial_request, IdempotencySlot};
use crate::com_port::state::{set_rack_card_problem, set_rack_card_state};
use crate::com_port::transport::SharedPort;
use crate::com_port::{Backoff, RECONNECT_DELAY_INITIAL_SECS};
use crate::config::{get_from_cache, split_host_to_parts, CacheSection};

/// The rack link report of a card session: `{"iccid","slot","rack"}`. The
/// server binds the session to its slot from `iccid` and `slot`; `rack` (the
/// rack id) is diagnostics only, the server ignores it. `local_ops` tells the
/// server the rack is run locally (see `local`), so the slot indicator is
/// written here and the server sends no indicator frames of its own. `status`
/// asks the server to answer the report on the `status` topic (see
/// `card_status`).
fn link_report(iccid: &str, slot: u16, rack_id: &str) -> String {
    serde_json::json!({
        "iccid": iccid,
        "slot": slot,
        "rack": rack_id,
        "serial_transactions": true,
        "local_ops": crate::com_port::local::is_active(rack_id),
        "status": true,
    })
    .to_string()
}

/// Shown on a card whose session is up but not confirmed by the server yet.
const PROBLEM_UNCONFIRMED: &str = "unconfirmed";

/// Task aborts (card removed/moved or a rack restart) release local ownership.
/// Only this session's own lease: the abort can land after a successor in the
/// same slot took its lease, which must survive.
struct AuthCleanup(SharedPort, u16, u64);
impl Drop for AuthCleanup {
    fn drop(&mut self) {
        crate::com_port::access::release(&self.0, self.1, false, Some(self.2));
    }
}

/// Owner tokens of the card sessions' authentication leases.
static NEXT_AUTH_SESSION: AtomicU64 = AtomicU64::new(1);

/// Work of a card session's serial worker, in MQTT arrival order.
enum CardJob {
    /// The MQTT session was (re)established or lost: the server's request ids
    /// restart and its authentication is gone.
    Reset,
    /// A serial envelope, tagged with the MQTT session it arrived on.
    Request {
        topic: String,
        payload: Vec<u8>,
        connection: u64,
    },
}

/// Aborts the task when dropped: the worker must not outlive its session.
struct AbortOnDrop(JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Runs a card session's serial requests one at a time, off its MQTT poll
/// loop. One exchange can wait far longer than the keep-alive (slot lease,
/// port queue, read deadline); a loop parked on it sends no PINGREQ, and the
/// broker drops the session together with the reply.
///
/// `in_flight` is set while a request runs.
fn spawn_serial_worker(
    card: (String, String, u16, u64),
    serial_port: SharedPort,
    mqtt_client: AsyncClient,
    connection: Arc<AtomicU64>,
    in_flight: Arc<AtomicBool>,
    log_header: String,
) -> (tokio::sync::mpsc::UnboundedSender<CardJob>, AbortOnDrop) {
    let (jobs, mut queue) = tokio::sync::mpsc::unbounded_channel::<CardJob>();
    let (iccid, card_number, slot, session) = card;
    let worker = async_runtime::spawn(async move {
        // Held by the worker, the one that takes the lease: released when it is really
        // gone, so a request it dequeued while being aborted cannot leave a lease behind.
        let _ownership = AuthCleanup(serial_port.clone(), slot, session);
        let mut idempotency = IdempotencySlot::default();
        while let Some(job) = queue.recv().await {
            let (topic, payload, arrived_on) = match job {
                CardJob::Reset => {
                    idempotency.reset();
                    crate::com_port::access::release(&serial_port, slot, false, Some(session));
                    continue;
                }
                CardJob::Request {
                    topic,
                    payload,
                    connection,
                } => (topic, payload, connection),
            };
            // Activity marking happens inside, driven by the envelope's
            // `finish` flag rather than by the mere arrival of a command.
            in_flight.store(true, Ordering::SeqCst);
            let reply = run_serial_request(
                &topic,
                &payload,
                &serial_port,
                &log_header,
                &mut idempotency,
                Some((&iccid, &card_number, slot, session)),
            )
            .await;
            in_flight.store(false, Ordering::SeqCst);
            let Some((resp_topic, resp_payload)) = reply else {
                continue;
            };
            // A reply of a lost MQTT session would go out on the next one,
            // whose request ids restart at 1, and answer the wrong request.
            if connection.load(Ordering::SeqCst) != arrived_on {
                log::warn!(
                    "{} [MQTT] status=reply_dropped reason=connection_replaced topic={}",
                    log_header,
                    resp_topic
                );
                continue;
            }
            publish_reply(&mqtt_client, resp_topic, resp_payload, &log_header).await;
        }
    });
    (jobs, AbortOnDrop(worker))
}

/// Enqueues the rack link report of a card session (`link_report`) with
/// `try_publish` and logs the outcome: `sent` completes the status of the
/// INFO line on success, `failed` the status of the WARN line on failure.
/// `card` is the (ICCID, slot, rack id) of the session. Returns whether the
/// report was enqueued.
pub(super) fn try_publish_link_report(
    client: &AsyncClient,
    card: (&str, u16, &str),
    log_header: &str,
    sent: std::fmt::Arguments<'_>,
    failed: &str,
) -> bool {
    let (iccid, slot, rack_id) = card;
    let report = link_report(iccid, slot, rack_id);
    match client.try_publish("rack", QoS::AtLeastOnce, false, report) {
        Ok(()) => {
            log::info!("{} [MQTT] status={}", log_header, sent);
            true
        }
        Err(e) => {
            log::warn!("{} [MQTT] status={} err={:?}", log_header, failed, e);
            false
        }
    }
}

/// MQTT loop of one rack-backed card connection: the opaque envelope handling funneled into
/// the rack's serial port, plus the one-shot **rack link report** right after CONNACK (topic
/// `rack`, see `link_report`) that binds this card session to its slot on the server. Without
/// the report the server treats the card as reader-backed and uses the plain PC/SC envelope.
pub(super) async fn rack_card_mqtt_loop(
    card_number: String,
    iccid: String,
    slot: u16,
    rack_id: String,
    serial_port: SharedPort,
    // shared back-reference to this session's MQTT client, filled once built —
    // see RackCardTask.client
    client_slot: Arc<OnceLock<AsyncClient>>,
    // see RackCardTask.online
    session_online: Arc<AtomicBool>,
) {
    let session = NEXT_AUTH_SESSION.fetch_add(1, Ordering::Relaxed);
    let log_header = format!("RACKCARD {} |", card_number);

    // same waiting policy as the rack loop: the server may not be configured yet
    let (host, port) = loop {
        let full_host = get_from_cache(CacheSection::Server, "host");
        match split_host_to_parts(&full_host) {
            Ok(hp) => break hp,
            Err(e) => {
                log::warn!(
                    "{} [MQTT] phase=config status=waiting reason=invalid_host err={} retry_secs={}",
                    log_header,
                    e,
                    RECONNECT_DELAY_INITIAL_SECS
                );
                tokio::time::sleep(Duration::from_secs(RECONNECT_DELAY_INITIAL_SECS)).await;
            }
        }
    };

    log::info!(
        "{} [MQTT] phase=connect_attempt status=initialized host={}:{} slot={}",
        log_header,
        host,
        port,
        slot
    );
    let (mqtt_client, mut eventloop) = crate::mqtt::build_mqtt_client(&card_number, &host, port);
    let _ = client_slot.set(mqtt_client.clone());

    // Bumped on every CONNACK and connection loss: tags requests with the MQTT
    // session they arrived on.
    let connection = Arc::new(AtomicU64::new(0));
    let in_flight = Arc::new(AtomicBool::new(false));
    let (jobs, worker) = spawn_serial_worker(
        (iccid.clone(), card_number.clone(), slot, session),
        serial_port.clone(),
        mqtt_client.clone(),
        connection.clone(),
        in_flight.clone(),
        log_header.clone(),
    );

    let mut card = CardSession {
        iccid,
        slot,
        rack_id,
        log_header,
        mqtt_client,
        connection,
        in_flight,
        session_online,
        _worker: worker,
        jobs,
        is_online: false,
        backoff: Backoff::new(),
        // The server's answers to the link report (see `card_status`).
        status_answers: crate::mqtt::StatusAnswers::new(),
        // The server confirms its card sessions and has not answered this one yet.
        awaiting_status: false,
    };

    loop {
        match eventloop.poll().await {
            Ok(notification) => {
                if !card.is_online {
                    card.is_online = true;
                    card.backoff.connected();
                    log::info!(
                        "{} [MQTT] state=OFFLINE->ONLINE cause=eventloop_poll_ok",
                        card.log_header
                    );
                }

                let keep_running = match notification {
                    Event::Incoming(Incoming::ConnAck(..)) => card.on_connack(),
                    Event::Incoming(Incoming::Publish(publish)) => card.on_publish(publish),
                    Event::Incoming(Incoming::PingResp(..)) => {
                        card.on_ping_resp();
                        true
                    }
                    other => {
                        log::debug!("{} [MQTT] event=other detail={:?}", card.log_header, other);
                        true
                    }
                };
                if !keep_running {
                    return;
                }
            }
            Err(e) => {
                if !card.on_connection_error(e).await {
                    return;
                }
            }
        }
    }
}

/// One running card session, as its MQTT event handlers see it. The handlers
/// return `false` when the session must end: the serial worker is gone (it
/// only stops if it panicked, which ends the session as a panic inline would
/// have).
struct CardSession {
    iccid: String,
    slot: u16,
    rack_id: String,
    log_header: String,
    mqtt_client: AsyncClient,
    /// Bumped on every CONNACK and connection loss: tags requests with the
    /// MQTT session they arrived on.
    connection: Arc<AtomicU64>,
    /// Set by the serial worker while a request runs.
    in_flight: Arc<AtomicBool>,
    /// See `RackCardTask::online`.
    session_online: Arc<AtomicBool>,
    /// The serial worker, aborted when the session ends. Declared before
    /// `jobs`, so it is dropped first.
    _worker: AbortOnDrop,
    /// The serial worker's queue.
    jobs: tokio::sync::mpsc::UnboundedSender<CardJob>,
    is_online: bool,
    backoff: Backoff,
    status_answers: crate::mqtt::StatusAnswers,
    awaiting_status: bool,
}

impl CardSession {
    /// Queues a job; the worker only stops if it panicked, which ends the
    /// session as a panic inline would have.
    fn enqueue(&self, job: CardJob) -> bool {
        let sent = self.jobs.send(job).is_ok();
        if !sent {
            log::error!("{} [MQTT] status=serial_worker_died", self.log_header);
        }
        sent
    }

    /// CONNACK: a new MQTT session, announced to the server by the rack link
    /// report.
    fn on_connack(&mut self) -> bool {
        // new MQTT session: the server-side request_id counter restarts at 1
        self.connection.fetch_add(1, Ordering::SeqCst);
        if !self.enqueue(CardJob::Reset) {
            return false;
        }
        // Session is up. With a server that confirms its card
        // sessions the card stays unserved until the answer to
        // the link report; set before the session shows up, so
        // the slot never flashes as served in between.
        self.awaiting_status = crate::com_port::local::confirms_cards(&self.rack_id);
        set_rack_card_problem(
            &self.iccid,
            self.awaiting_status.then_some(PROBLEM_UNCONFIRMED),
        );
        set_rack_card_state(&self.iccid, true, false);
        // Rack link report: must be the first publish of the
        // session. `try_publish`, never `publish().await`: this
        // task is the only one polling the session's event
        // loop, so awaiting room in a full request channel
        // (bounded, see `mqtt::build_mqtt_client`) would park
        // the very loop that drains it — forever, with the
        // session still counted as running but unserved.
        // `try_publish` enqueues in call order, so the report
        // still precedes every reply of this session (requests
        // reach the worker only through this loop, after this).
        if !try_publish_link_report(
            &self.mqtt_client,
            (&self.iccid, self.slot, &self.rack_id),
            &self.log_header,
            format_args!("link_report_sent slot={} iccid={}", self.slot, self.iccid),
            "link_report_deferred reason=request_channel_full",
        ) {
            // The channel is full (or closed): hand the
            // report to a detached task that waits for room
            // while this loop keeps polling and drains it.
            // Queued behind whatever filled the channel, so
            // not strictly first — but delivered, which the
            // server needs to bind the card to its slot.
            let client = self.mqtt_client.clone();
            let report = link_report(&self.iccid, self.slot, &self.rack_id);
            let log_header = self.log_header.clone();
            async_runtime::spawn(async move {
                match client
                    .publish("rack", QoS::AtLeastOnce, false, report)
                    .await
                {
                    Ok(()) => log::info!(
                        "{} [MQTT] status=link_report_sent deferred=true",
                        log_header
                    ),
                    Err(e) => log::error!(
                        "{} [MQTT] status=link_report_failed err={:?}",
                        log_header,
                        e
                    ),
                }
            });
        }
        self.session_online.store(true, Ordering::SeqCst);
        true
    }

    /// An incoming publish: the answer to the link report, or a serial
    /// envelope for the worker.
    fn on_publish(&mut self, publish: Publish) -> bool {
        let log_header = &self.log_header;
        let topic = String::from_utf8_lossy(&publish.topic).into_owned();
        log::info!(
            "{} [MQTT] event=command topic={} bytes={} qos={:?}",
            log_header,
            topic,
            publish.payload.len(),
            publish.qos,
        );
        // when the server sent it vs. when it got here: the
        // MQTT transit part of a slow exchange
        log::debug!(
            "{} [MQTT] rx pkid={} server_ts={} rack={} slot={}",
            log_header,
            publish.pkid,
            crate::debug_log::server_timestamp(&publish),
            self.rack_id,
            self.slot
        );
        // Full command text only at trace: the rack protocol
        // must not end up in users' log files, not even with
        // the extended debug log on.
        log::trace!(
            "{} [MQTT] command_text={}",
            log_header,
            String::from_utf8_lossy(&publish.payload)
        );
        // The answer to the link report: served, or the
        // reason the server will command nothing here.
        if topic == "status" {
            self.on_status(&publish.payload);
            return true;
        }
        // Only `request/...` publishes are serial envelopes —
        // same guard as the rack path: anything else (a future
        // control topic, a retained stray) must not be written
        // raw to the COM port.
        if !topic.starts_with("request/") {
            log::warn!(
                "{} [MQTT] status=ignored reason=unknown_topic topic={}",
                log_header,
                topic
            );
            return true;
        }
        self.enqueue(CardJob::Request {
            topic,
            payload: publish.payload.to_vec(),
            connection: self.connection.load(Ordering::SeqCst),
        })
    }

    /// The server's answer to the link report (topic `status`).
    fn on_status(&mut self, payload: &[u8]) {
        let log_header = &self.log_header;
        self.awaiting_status = false;
        match crate::mqtt::card_status(payload) {
            None => {
                self.status_answers.served();
                set_rack_card_problem(&self.iccid, None);
                log::info!("{} [MQTT] status=served slot={}", log_header, self.slot);
            }
            Some(reason) => {
                let retry_secs = self.status_answers.refused(
                    &self.mqtt_client,
                    &self.connection,
                    None,
                    || true,
                    log_header,
                );
                set_rack_card_problem(&self.iccid, Some(&reason));
                log::warn!(
                    "{} [MQTT] status=not_served reason={} slot={} retry_secs={}",
                    log_header,
                    reason,
                    self.slot,
                    retry_secs
                );
            }
        }
    }

    /// A keep-alive ping answer: the connection was idle for a whole
    /// keep-alive window.
    fn on_ping_resp(&self) {
        // A keep-alive ping means this connection sent nothing
        // for the whole keep-alive window. Unless a serial
        // exchange is still running in the worker, the card is
        // idle: clear the busy indicator — same self-heal the
        // reader path performs in `mqtt::card_session`. This is the only
        // recovery when the closing `finish:true` envelope
        // never arrives (tracker aborted the session, message
        // lost, or an older server that does not send the flag
        // at all); without it the activity animation blinks
        // forever on an idle card.
        if !self.in_flight.load(Ordering::SeqCst) {
            set_rack_card_state(&self.iccid, true, false);
            log::debug!("{} [MQTT] event=ping_resp status=idle", self.log_header);
        }
        // Still no answer to the link report a keep-alive later:
        // ask again, or the slot would show unserved for as long
        // as the session lives. The server takes a repeated report.
        if self.awaiting_status {
            try_publish_link_report(
                &self.mqtt_client,
                (&self.iccid, self.slot, &self.rack_id),
                &self.log_header,
                format_args!("link_report_repeated reason=no_status_answer"),
                "link_report_repeat_failed",
            );
        }
    }

    /// The connection failed: the session goes offline and reconnects after
    /// the backoff delay.
    async fn on_connection_error(&mut self, e: ConnectionError) -> bool {
        let transition = if self.is_online {
            "ONLINE->OFFLINE"
        } else {
            "OFFLINE"
        };
        self.is_online = false;
        self.session_online.store(false, Ordering::SeqCst);
        self.connection.fetch_add(1, Ordering::SeqCst);
        if !self.enqueue(CardJob::Reset) {
            return false;
        }
        // Connection lost: the card is present in its slot but no
        // longer served, so it must stop looking active.
        set_rack_card_state(&self.iccid, false, false);
        let reconnect_delay_secs = self.backoff.failed();
        crate::mqtt::log_connection_failure(
            &self.log_header,
            "MQTT",
            transition,
            &e,
            reconnect_delay_secs,
        );
        tokio::time::sleep(Duration::from_secs(reconnect_delay_secs)).await;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic ICCIDs only: no real card data in the public repo.
    const A: &str = "0000000000000001";

    #[test]
    fn link_report_carries_slot_iccid_and_rack() {
        let report: serde_json::Value = serde_json::from_str(&link_report(A, 7, "SC1799")).unwrap();
        assert_eq!(report["iccid"], A);
        assert_eq!(report["slot"], 7);
        assert_eq!(report["rack"], "SC1799");
    }
}
