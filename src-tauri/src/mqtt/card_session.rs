//! The MQTT session of a card in a PC/SC reader: the APDU bridge between the
//! server and the card.
//!
//! `ensure_connection` registers the session in `TASK_POOL` and spawns its
//! task; `CardSession` is that task. Its state is split by what it guards:
//!
//! - the connection: online flag, reconnect backoff, the status question and
//!   the connection counter a scheduled retry checks against;
//! - the card: whether it holds session state (`auth_process`), whether the
//!   UI was told it is busy (`busy_announced`), and the generation of its
//!   physical state, bumped on every reset;
//! - the idempotency cache of the last reply (`ReplyCache`).

use std::ffi::{CStr, CString};
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pcsc::Protocols;
use rumqttc::v5::mqttbytes::v5::Publish;
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::{AsyncClient, ConnectionError, Event, EventLoop, Incoming};
use rumqttc::Outgoing;
use serde_json::Value;
use tauri::async_runtime::{self, JoinHandle};

use crate::backoff::Backoff;
use crate::config::{get_from_cache, split_host_to_parts, CacheSection};
use crate::global_app_handle::card_emit_event;
use crate::smart_card::{
    protocol_from_str, protocol_to_str, ManagedCard, ProcessingCard, SW_TECHNICAL_PROBLEM,
    TASK_POOL,
};

use super::activity::touch_card_activity;
use super::client::{build_mqtt_client, log_connection_failure, MQTT_KEEP_ALIVE_SECS};
use super::teardown::{shutdown_connections, SHUTDOWN_REASON_STALE_ENTRY_REPLACED};
use super::topics::{
    card_status, process_rapdu_mqtt_hex, process_session_start_mqtt_hex, request_id_from_topic,
    request_to_response_topic, StatusAnswers,
};

const GLOBAL_CARDS_SYNC_EVENT: &str = "global-cards-sync";
const CARD_PRESENT_STATE: &str = "PRESENT";

/// Upper bound of one card operation (an APDU exchange, a reset, a protocol
/// switch) run from the card's poll loop. A reader or driver wedged inside
/// PC/SC would otherwise stop the loop from polling: keep-alives lapse, the
/// broker drops the session, and the card stays dead until it is pulled.
pub(crate) const CARD_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs a card operation under `CARD_IO_TIMEOUT`; `None` when it expired.
pub(crate) async fn card_io<T>(
    op: impl std::future::Future<Output = T>,
    log_header: &str,
    what: &str,
) -> Option<T> {
    card_io_by(
        tokio::time::Instant::now() + CARD_IO_TIMEOUT,
        op,
        log_header,
        what,
    )
    .await
}

/// `card_io` against a deadline shared by several operations in a row.
async fn card_io_by<T>(
    deadline: tokio::time::Instant,
    op: impl std::future::Future<Output = T>,
    log_header: &str,
    what: &str,
) -> Option<T> {
    match tokio::time::timeout_at(deadline, op).await {
        Ok(value) => Some(value),
        Err(_) => {
            log::error!(
                "{} card operation {} timed out after {}s",
                log_header,
                what,
                CARD_IO_TIMEOUT.as_secs()
            );
            None
        }
    }
}

/// True when the pool no longer holds this session's entry. A busy pool lock
/// answers `false` (the caller then behaves as a live session): waiting for
/// it could deadlock against a holder awaiting this very task.
fn session_retired(session_id: u64) -> bool {
    match TASK_POOL.try_lock() {
        Ok(pool) => !pool.iter().any(|card| card.session_id == session_id),
        Err(_) => false,
    }
}

/// Ensures an MQTT connection for the card in `reader_name`: registers a new
/// session in `TASK_POOL` unless a live one already serves the card there.
pub async fn ensure_connection(
    reader_name: &CStr,
    client_id: String,
    atr: String,
    mut managed_card: ManagedCard,
) {
    log::info!(
        "[CONN] phase=ensure_connection status=start reader={} client_id={} atr_len={}",
        reader_name.to_string_lossy(),
        client_id,
        atr.len()
    );

    // No card number, no connection: the card is not linked in the config.
    if client_id.is_empty() {
        log::warn!(
            "Reader: {:?}. ClientID is empty. Cannot ensure connection.",
            reader_name
        );
        managed_card.release();
        return;
    }

    let mut task_pool = TASK_POOL.lock().await;

    // The shutdown closes every session with a card reset, which the reader
    // monitor sees as a card to serve: it must not get a new session.
    if crate::shutdown::in_progress() {
        log::info!(
            "[CONN] phase=ensure_connection status=skip_shutdown reader={} client_id={}",
            reader_name.to_string_lossy(),
            client_id
        );
        managed_card.release();
        return;
    }

    // Resolved before an existing entry is touched: replacing it and then
    // failing on the host would leave the card served by nothing. Read under
    // the pool lock: a settings save drains the pool after the new host is in
    // the cache, so a session registered after that drain uses the new host.
    let full_host = get_from_cache(CacheSection::Server, "host");
    let (host, port) = match split_host_to_parts(&full_host) {
        Ok(host_port) => host_port,
        Err(e) => {
            log::error!(
                "[CONN] phase=ensure_connection status=failed reason=invalid_host reader={} client_id={} err={}",
                reader_name.to_string_lossy(),
                client_id,
                e
            );
            managed_card.release();
            return;
        }
    };

    if !take_over_pool_entry(&mut task_pool, reader_name, &client_id) {
        // Only a probe: dropping it would reset the card under the live session.
        managed_card.release();
        return;
    }

    // A live rack-backed session for the same card number would collide with
    // the connection opened below: the flespi broker allows one session per
    // client_id and would drop the two connections in a loop. The card was
    // just physically detected in this reader, so it cannot still sit in a
    // rack slot — the rack session is stale (the server's card set dropping
    // it was lost or is still in flight). Abort it before opening ours.
    // Deliberately AFTER every early return above (empty client_id, invalid
    // host, existing session): killing the rack session and then not opening
    // the reader one would leave the card served by neither transport.
    crate::com_port::abort_rack_card_session(&client_id);

    log::info!(
        "[CONN] phase=connect_attempt status=initialized reader={} client_id={} host={}:{}",
        reader_name.to_string_lossy(),
        client_id,
        host,
        port
    );
    let (mqtt_client, eventloop) = build_mqtt_client(&client_id, &host, port);

    // Identity of the pool entry this task belongs to (see
    // ProcessingCard::session_id): the failure path of the task must only
    // ever drop ITS OWN entry, never a successor under the same client_id.
    let session_id = crate::smart_card::next_session_id();
    managed_card.bind_session(session_id);

    let session = CardSession::new(
        client_id.clone(),
        session_id,
        reader_name.to_owned(),
        atr.clone(),
        managed_card,
        mqtt_client.clone(),
    );
    let handle: JoinHandle<()> = async_runtime::spawn(session.run(eventloop));

    task_pool.push(ProcessingCard {
        client_id,
        session_id,
        reader_name: Some(reader_name.to_string_lossy().into_owned()),
        atr: Some(atr),
        mqtt_client,
        task_handle: handle,
    });
    log_task_pool(&task_pool);
}

/// Makes room in the pool for a new session of `client_id`; `false` when a
/// live session already serves the card in this very reader.
///
/// Only one connection per client_id. An existing entry blocks a new one
/// unless it is unusable: a finished task (returned early or panicked) will
/// never serve the card again, and an entry bound to a DIFFERENT reader
/// means the card was physically moved — its old session is dead weight and
/// would otherwise keep the card offline until a replug (the PCSC sweep may
/// process the new reader before the old reader's removal).
///
/// INVARIANT: synchronous on purpose. No `.await` may sit between the removal
/// here and the push of the successor entry in `ensure_connection`: the pool
/// guard is held throughout, so concurrent pool readers (e.g. the rack side's
/// served_by_reader check) never observe the client_id as absent.
fn take_over_pool_entry(
    task_pool: &mut Vec<ProcessingCard>,
    reader_name: &CStr,
    client_id: &str,
) -> bool {
    let Some(index) = task_pool
        .iter()
        .position(|card| card.client_id == client_id)
    else {
        return true;
    };
    let finished = task_pool[index].task_handle.inner().is_finished();
    let same_reader =
        task_pool[index].reader_name.as_deref() == Some(reader_name.to_string_lossy().as_ref());
    if !finished && same_reader {
        log::info!(
            "[CONN] phase=ensure_connection status=skip_existing reader={} client_id={}",
            reader_name.to_string_lossy(),
            client_id
        );
        return false;
    }
    let old = task_pool.remove(index);
    crate::apdu_sniffer::forget(&old.client_id);
    log::warn!(
        "[CONN] phase=ensure_connection status=replacing_stale reader={} client_id={} reason={}",
        reader_name.to_string_lossy(),
        client_id,
        if finished {
            "task_finished"
        } else {
            "reader_changed"
        }
    );
    // Close the old session gracefully; detached so the fresh registration
    // is not delayed behind its shutdown flush.
    async_runtime::spawn(shutdown_connections(
        vec![old],
        SHUTDOWN_REASON_STALE_ENTRY_REPLACED,
    ));
    true
}

fn log_task_pool(task_pool: &[ProcessingCard]) {
    log::info!(
        "MQTT task registered in TASK_POOL. Current size: {}",
        task_pool.len()
    );
    for (i, card) in task_pool.iter().enumerate() {
        log::debug!(
            "TASK_POOL: [{}] Client ID: {}, Reader: {}, ATR: {}",
            i,
            card.client_id,
            card.reader_name.as_deref().unwrap_or("unknown"),
            card.atr.as_deref().unwrap_or("unknown"),
        );
    }
}

/// Idempotency of the current MQTT connection. The server re-sends a command
/// with the same request_id when it does not get a timely reply. The last
/// answered id and its reply are kept so a repeated request re-sends the
/// cached reply instead of re-running the card: replaying a stateful
/// authentication APDU can corrupt the card state, and the duplicate reply
/// would be rejected by the server anyway. Cleared on every CONNACK, because
/// a new MQTT session restarts the server-side request_id counter at 1.
///
/// The reply records the card generation it was produced against (see
/// `CardSession::card_generation`): a reply captured before a reset can never
/// be replayed after one, it would describe a card state that no longer
/// exists. Resets happen without the MQTT session dropping, so the CONNACK
/// clear is not enough on its own.
#[derive(Default)]
struct ReplyCache {
    last: Option<CachedReply>,
}

struct CachedReply {
    request_id: u64,
    payload: String,
    generation: u64,
}

/// What the cache says about an incoming request.
enum CacheLookup {
    /// A new request: run it on the card.
    Miss,
    /// A repeat of the request just answered: re-send this reply.
    Replay(String),
    /// A repeat of a request answered before a card reset: run it again.
    Stale,
}

impl ReplyCache {
    fn lookup(&self, request_id: Option<u64>, generation: u64) -> CacheLookup {
        match (&self.last, request_id) {
            (Some(last), Some(id)) if last.request_id == id => {
                if last.generation == generation {
                    CacheLookup::Replay(last.payload.clone())
                } else {
                    CacheLookup::Stale
                }
            }
            _ => CacheLookup::Miss,
        }
    }

    fn store(&mut self, request_id: u64, payload: &str, generation: u64) {
        self.last = Some(CachedReply {
            request_id,
            payload: payload.to_string(),
            generation,
        });
    }

    fn clear(&mut self) {
        self.last = None;
    }
}

/// The task of one reader card session (see the module docs).
struct CardSession {
    client_id: String,
    session_id: u64,
    reader_name: CString,
    /// Resolved when the task starts (see `resolve_iccid`).
    iccid: String,
    atr: String,
    log_header: String,
    managed_card: ManagedCard,
    mqtt_client: AsyncClient,

    is_online: bool,
    backoff: Backoff,
    status_answers: StatusAnswers,
    /// Bumped on every CONNACK and connection loss: a retry scheduled on one
    /// connection must not close the next.
    connection: Arc<AtomicU64>,
    /// Set by the retry of a refused card: its DISCONNECT is a reconnect, not
    /// the teardown of the session.
    requery: Arc<AtomicBool>,

    /// The card has been driven mid-authentication and its security state is
    /// dirty, so a new session must reset it first. Only a real card exchange
    /// sets it; only an explicit session boundary (`finish`, or the reset at
    /// session start) clears it.
    auth_process: bool,
    /// Whether the frontend has already been told this session is active.
    /// Purely cosmetic, and cleared by the keep-alive self-heal, which must NOT
    /// make the card look clean to the reset gate (`auth_process`).
    busy_announced: bool,
    /// When the last request arrived: the keep-alive ping is periodic, not a
    /// sign of silence.
    last_request_at: Instant,
    /// Generation of the physical card state, bumped on every reset of the
    /// card (`reconnect`, protocol switch), which wipes the security state a
    /// cached reply was produced against (see `ReplyCache`).
    card_generation: u64,
    replies: ReplyCache,
}

/// The reply to one request and whether the card exchange behind it failed.
/// A failed exchange (`SW_TECHNICAL_PROBLEM` substituted on the wire) is
/// never cached: the server's retry must reach the card again. A genuine
/// 6F00 answered BY the card is a success and is cached like any other reply.
struct Reply {
    payload: String,
    failed: bool,
}

impl CardSession {
    fn new(
        client_id: String,
        session_id: u64,
        reader_name: CString,
        atr: String,
        managed_card: ManagedCard,
        mqtt_client: AsyncClient,
    ) -> Self {
        CardSession {
            log_header: format!("{} |", client_id),
            client_id,
            session_id,
            reader_name,
            iccid: String::new(),
            atr,
            managed_card,
            mqtt_client,
            is_online: false,
            backoff: Backoff::new(),
            status_answers: StatusAnswers::new(),
            connection: Arc::new(AtomicU64::new(0)),
            requery: Arc::new(AtomicBool::new(false)),
            auth_process: false,
            busy_announced: false,
            last_request_at: Instant::now(),
            card_generation: 0,
            replies: ReplyCache::default(),
        }
    }

    async fn run(mut self, mut eventloop: EventLoop) {
        let Some(iccid) = self.resolve_iccid().await else {
            return;
        };
        self.iccid = iccid;
        log::info!(
            "{} [CONN] phase=eventloop status=started reader={} iccid={}",
            self.log_header,
            self.reader_name.to_string_lossy(),
            self.iccid
        );

        loop {
            let flow = match eventloop.poll().await {
                Ok(event) => {
                    self.on_link_up();
                    log::trace!("{} Notification: {:?}", self.log_header, event);
                    self.on_event(event).await
                }
                Err(e) => self.on_connection_error(e).await,
            };
            if flow.is_break() {
                break;
            }
        }
    }

    fn retired(&self) -> bool {
        session_retired(self.session_id)
    }

    /// Tells the frontend the state of the card row.
    fn emit(&self, online: Option<bool>, authenticating: Option<bool>) {
        card_emit_event(
            GLOBAL_CARDS_SYNC_EVENT,
            self.iccid.clone(),
            self.reader_name.to_string_lossy().into(),
            CARD_PRESENT_STATE.into(),
            self.client_id.clone(),
            online,
            authenticating,
        );
    }

    /// Queues a card reply without waiting. Called from inside the card's poll
    /// loop: an awaited `publish` parks while the bounded request channel is
    /// full, and only that same loop's `poll()` drains it — a permanent
    /// deadlock. A reply dropped on a full channel is recovered by the
    /// server's retry (the idempotency cache answers it without the card).
    fn publish_reply(&self, topic: String, payload: String) {
        match self
            .mqtt_client
            .try_publish(topic, QoS::AtLeastOnce, false, payload)
        {
            Ok(()) => log::debug!("{} MQTT response published successfully", self.log_header),
            Err(e) => log::error!("{} MQTT publish failed: {:?}", self.log_header, e),
        }
    }

    /// The ICCID the probe cached. `None` when it cannot be read: the task
    /// then ends, after dropping its own pool entry.
    async fn resolve_iccid(&self) -> Option<String> {
        match self.managed_card.get_iccid().await {
            Ok(iccid) => {
                log::info!(
                    "{} [CONN] phase=init status=iccid_resolved",
                    self.log_header
                );
                return Some(iccid);
            }
            Err(e) => log::error!(
                "{} [CONN] phase=init status=aborted reason=iccid_resolve_failed err={}",
                self.log_header,
                e
            ),
        }
        self.emit(Some(false), None);
        // Drop our own pool entry before giving up. The task is about to end,
        // so the entry would sit there dead — `ensure_connection` only
        // reclaims it when the SAME card is detected again, which for a
        // reader-backed card needs a re-plug or a rescan. Until then the stale
        // entry makes the card look served and blocks a rack-backed session
        // for the same number from starting.
        //
        // Matched by session_id, not client_id: the card may have moved to
        // another reader while this task was still starting, in which case the
        // pool already holds the successor's entry under the same client_id —
        // and removing THAT one would orphan a live session (unstoppable by
        // remove_connections, invisible to the served_by_reader guard).
        //
        // Ordering is safe: the spawning `ensure_connection` holds the
        // TASK_POOL lock across the spawn and releases it only after pushing
        // our entry, so this lock cannot be taken before the entry exists.
        let mut pool = TASK_POOL.lock().await;
        if let Some(index) = pool
            .iter()
            .position(|card| card.session_id == self.session_id)
        {
            pool.remove(index);
            log::info!(
                "{} [CONN] phase=init status=pool_entry_dropped reason=iccid_resolve_failed",
                self.log_header
            );
        }
        drop(pool);
        // The dropped entry may have been the reason a rack-backed session for
        // this number was skipped (served_by_reader) — retry the rack side now
        // that the number is free again.
        crate::com_port::connect_pending_rack_cards().await;
        None
    }

    /// A successful poll: the first one after a connect brings the card online.
    fn on_link_up(&mut self) {
        if self.is_online {
            return;
        }
        self.is_online = true;
        // The backoff resets once the link holds.
        self.backoff.connected();
        // Not for a retired session: one retired during the reconnect backoff
        // reconnects once more to flush its DISCONNECT, and must not resurrect
        // its removed row.
        if !self.retired() {
            self.emit(Some(true), None);
        }
        log::info!(
            "{} [CONN] state=OFFLINE->ONLINE cause=eventloop_poll_ok",
            self.log_header
        );
    }

    async fn on_event(&mut self, event: Event) -> ControlFlow<()> {
        match event {
            Event::Incoming(Incoming::Publish(publish)) if &publish.topic[..] == b"status" => {
                self.on_status(&publish.payload);
            }
            Event::Incoming(Incoming::Publish(publish)) => self.on_request(&publish).await,
            Event::Incoming(Incoming::ConnAck(..)) => self.on_connack(),
            Event::Incoming(Incoming::PingResp(..)) => self.on_ping_resp(),
            Event::Outgoing(Outgoing::Disconnect) if self.requery.swap(false, Ordering::SeqCst) => {
                // The retry of a refused card: the server closes the
                // connection and the error path reconnects.
                log::debug!(
                    "{} [CONN] event=disconnect_sent reason=not_served",
                    self.log_header
                );
            }
            Event::Outgoing(Outgoing::Disconnect) => {
                // Graceful teardown: the DISCONNECT packet is already flushed
                // to the socket, so exit instead of letting the loop treat the
                // closing connection as a network failure and reconnect.
                log::info!(
                    "{} [CONN] phase=shutdown status=disconnect_sent",
                    self.log_header
                );
                return ControlFlow::Break(());
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }

    /// The answer to the status question sent on CONNACK: served, or the
    /// reason the server commands nothing here.
    fn on_status(&mut self, payload: &[u8]) {
        match card_status(payload) {
            None => {
                self.status_answers.served();
                crate::global_app_handle::set_card_problem(&self.client_id, None);
                log::info!("{} [CONN] status=served", self.log_header);
            }
            Some(reason) => {
                let session_id = self.session_id;
                let retry_secs = self.status_answers.refused(
                    &self.mqtt_client,
                    &self.connection,
                    Some(self.requery.clone()),
                    move || !session_retired(session_id),
                    &self.log_header,
                );
                log::warn!(
                    "{} [CONN] status=not_served reason={} retry_secs={}",
                    self.log_header,
                    reason,
                    retry_secs
                );
                crate::global_app_handle::set_card_problem(&self.client_id, Some(reason));
            }
        }
    }

    fn on_connack(&mut self) {
        // New MQTT session: the server restarts its request_id counter at 1,
        // so a fresh request must not be mistaken for a duplicate.
        self.replies.clear();
        self.connection.fetch_add(1, Ordering::SeqCst);
        // Ask whether the server serves this card: it accepts the connection
        // of a card without a device and only warns about it.
        if let Err(e) = self
            .mqtt_client
            .try_publish("status", QoS::AtLeastOnce, false, "{}")
        {
            log::warn!(
                "{} [CONN] status=status_question_failed err={:?}",
                self.log_header,
                e
            );
        }
        // The OFFLINE->ONLINE transition is already logged at info; CONNACK
        // itself is a detail.
        log::debug!("{} [CONN] event=CONNACK status=received", self.log_header)
    }

    /// A keep-alive ping with no request for the whole keep-alive window means
    /// no APDU is in flight and the card is idle. Clearing the busy indicator
    /// here is the only recovery when the closing `finish:true` never arrives
    /// (tracker aborted the session, message lost, older server): without it
    /// the activity animation blinks forever. Mirrors the rack path's
    /// PingResp self-heal in `com_port::cards::session`.
    ///
    /// Only the cosmetic flag is cleared, so the next real APDU re-announces
    /// the session instead of being swallowed by the once-per-session emit
    /// gate. The card's dirty-state flag (`auth_process`) is left alone: a
    /// ping is not a session boundary, and clearing it would skip the reset a
    /// resumed session needs.
    fn on_ping_resp(&mut self) {
        log::debug!(
            "{} Ping response received from the server.",
            self.log_header
        );
        let idle = self.last_request_at.elapsed() >= Duration::from_secs(MQTT_KEEP_ALIVE_SECS);
        if idle && self.busy_announced {
            log::debug!(
                "{} [CONN] event=ping_resp status=idle action=clear_busy",
                self.log_header
            );
            self.busy_announced = false;
        }
        // A ping still in flight when the session was taken out of the pool
        // must not resurrect the row the monitor just removed (same gate as
        // the error path).
        if idle && !self.retired() {
            self.emit(Some(true), Some(false));
        }
    }

    async fn on_connection_error(&mut self, e: ConnectionError) -> ControlFlow<()> {
        // Taken out of the pool already (card removed, reader gone, entry
        // replaced): the connection failure is the teardown itself. Emitting
        // here would resurrect the row the monitor just deleted, and
        // reconnecting would only compete with a successor under the same
        // client_id.
        if self.retired() {
            log::info!(
                "{} [CONN] phase=shutdown status=retired err={}",
                self.log_header,
                e
            );
            return ControlFlow::Break(());
        }
        self.emit(Some(false), None);

        let transition = if self.is_online {
            "ONLINE->OFFLINE"
        } else {
            "OFFLINE"
        };
        self.is_online = false;
        self.connection.fetch_add(1, Ordering::SeqCst);

        // One line per failed poll: kind + retry delay; full error details
        // only for genuinely unexpected failures.
        let reconnect_delay_secs = self.backoff.failed();
        log_connection_failure(
            &self.log_header,
            "CONN",
            transition,
            &e,
            reconnect_delay_secs,
        );

        tokio::time::sleep(Duration::from_secs(reconnect_delay_secs)).await;
        // Retired during the sleep: a reconnect would kick its successor.
        if self.retired() {
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }

    /// A request of the server: `{"finish": bool, "payload": "<apdu hex>"}`
    /// on `request/<id>/...`, answered on the matching `response/...` topic.
    async fn on_request(&mut self, publish: &Publish) {
        // Every publish on a card connection is a VU-driven exchange: mark
        // card activity so the auto-updater never restarts the app
        // mid-authentication.
        touch_card_activity();
        self.last_request_at = Instant::now();

        let topic = match std::str::from_utf8(&publish.topic) {
            Ok(topic) => topic,
            Err(e) => {
                log::error!(
                    "{} Failed to decode incoming topic as UTF-8: {:?}",
                    self.log_header,
                    e
                );
                return;
            }
        };
        let response_topic = request_to_response_topic(topic);

        let request_id = request_id_from_topic(topic);
        match self.replies.lookup(request_id, self.card_generation) {
            CacheLookup::Miss => {}
            CacheLookup::Replay(cached) => {
                log::warn!(
                    "{} duplicate request_id={:?}: re-sending cached response, skipping card exchange",
                    self.log_header,
                    request_id
                );
                self.publish_reply(response_topic, cached);
                return;
            }
            CacheLookup::Stale => {
                log::warn!(
                    "{} duplicate request_id={:?} predates a card reset: \
                     re-running it on the card instead of replaying the cache",
                    self.log_header,
                    request_id
                );
                self.replies.clear();
            }
        }

        let request = match serde_json::from_slice::<Value>(&publish.payload) {
            Ok(request) => request,
            Err(e) => {
                log::error!("{} parsing JSON payload issue: {:?}", self.log_header, e);
                return;
            }
        };
        log::trace!("Parsed JSON payload: {:?}", request);

        let Some(finish) = request.get("finish").and_then(Value::as_bool) else {
            log::error!(
                "{} Finish parameter not found or is not a boolean",
                self.log_header
            );
            return;
        };
        log::debug!("{} Finish parameter: {}", self.log_header, finish);

        let protocol = self.requested_protocol(&request);
        let reply = if finish {
            self.finish_session().await
        } else {
            match request.get("payload").and_then(Value::as_str) {
                Some(apdu_hex) => self.exchange(apdu_hex, protocol).await,
                None => {
                    log::error!("{} Hex value not found or is not a string", self.log_header);
                    // An empty reply is not JSON and makes the server close
                    // the connection.
                    Reply {
                        payload: process_rapdu_mqtt_hex(SW_TECHNICAL_PROBLEM.to_string()),
                        failed: true,
                    }
                }
            }
        };

        // Only real card replies are cached: a malformed request is answered
        // with an error, and pinning that against the id would answer the
        // server's corrected retry from cache without touching the card; a
        // failed exchange must be retried on the card when the server
        // re-sends. Same rule the rack path applies via SerialExchange::is_ok().
        if let Some(id) = request_id {
            if !reply.payload.is_empty() && !reply.failed {
                self.replies.store(id, &reply.payload, self.card_generation);
            }
        }
        self.publish_reply(response_topic, reply.payload);
    }

    /// The server-requested T protocol of the session, applied only at its
    /// start (see `start_session`).
    fn requested_protocol(&self, request: &Value) -> Option<Protocols> {
        let requested = request.get("protocol").and_then(Value::as_str)?;
        let parsed = protocol_from_str(requested);
        if parsed.is_none() {
            log::warn!(
                "{} server requested unknown T protocol '{}': ignoring",
                self.log_header,
                requested
            );
        }
        parsed
    }

    /// `finish: true`: the VU closed the authentication. The card is reset to
    /// its original state and checked to still be this session's card.
    async fn finish_session(&mut self) -> Reply {
        // The card is online again. Not for a session the reader sweep has
        // already retired: the card is gone and its row was removed, a late
        // finish must not bring the row back.
        if !self.retired() {
            self.emit(Some(true), Some(false));
        }
        log::info!("{} Authentication process is finished", self.log_header);

        // Persist the successful auth timestamp (detached: the config write
        // must not park the bridge on disk I/O between `finish` and our
        // reply). Only for a session that reached the card: a bare finish
        // authenticated nothing.
        if self.auth_process {
            crate::config::record_auth_result_detached(&self.client_id, true);
        }

        // One budget for the reset and the card check, as at session start:
        // the loop does not poll meanwhile, and two full timeouts in a row
        // outlast the keep-alive.
        let deadline = tokio::time::Instant::now() + CARD_IO_TIMEOUT;
        let reset = card_io_by(
            deadline,
            self.managed_card.reconnect(),
            &self.log_header,
            "reconnect",
        )
        .await;
        // The card lost its security state: nothing captured before this
        // point may be replayed from cache.
        self.card_generation = self.card_generation.wrapping_add(1);

        // The reset may have landed on a swapped card (see the session-start
        // check): a mismatch ends the session.
        let verified = reset.is_some() && self.verify_card(deadline).await;

        // Session closed cleanly and the card was just reset: it holds no
        // session state, and the UI indicator is cleared by the emit above. A
        // reset or check that did not complete leaves it dirty: the next
        // session start resets and checks it again.
        self.auth_process = !verified;
        self.busy_announced = false;
        Reply {
            payload: process_rapdu_mqtt_hex(String::new()),
            failed: false,
        }
    }

    /// `finish: false`: an empty payload starts a session (answered with the
    /// ATR), any other payload is an APDU for the card.
    async fn exchange(&mut self, apdu_hex: &str, protocol: Option<Protocols>) -> Reply {
        log::trace!(
            "{} TRACKER: Payload hex value: {}",
            self.log_header,
            apdu_hex
        );
        let reply = if apdu_hex.is_empty() {
            let (atr, failed) = self.start_session(protocol).await;
            // The session-start (ATR) reply also reports the actual T
            // protocol: the server seeds its per-tracker value from it, and it
            // signals that this application understands the `protocol` field.
            Reply {
                payload: process_session_start_mqtt_hex(atr, self.managed_card.protocol_str()),
                failed,
            }
        } else {
            let (rapdu, failed) = self.send_apdu(apdu_hex, protocol).await;
            Reply {
                payload: process_rapdu_mqtt_hex(rapdu),
                failed,
            }
        };
        log::trace!(
            "{} CARD: Payload hex value: {}",
            self.log_header,
            reply.payload
        );
        reply
    }

    /// Starts a VU session: resets a card left mid-authentication, applies the
    /// requested T protocol, and checks the card. Returns the ATR (or the
    /// technical-problem status) and whether the start failed.
    async fn start_session(&mut self, protocol: Option<Protocols>) -> (String, bool) {
        // A fresh VU session starts with no file selected — stale sniffer
        // state from the previous session must not leak into it.
        crate::apdu_sniffer::forget(&self.client_id);
        // One budget for the whole start (reset, protocol switch, card
        // check): the loop does not poll meanwhile, and several full timeouts
        // in a row outlast the keep-alive.
        let deadline = tokio::time::Instant::now() + CARD_IO_TIMEOUT;
        let mut card_reset = false;

        // A card left mid-authentication does not answer a new session
        // correctly until it is reset.
        if self.auth_process {
            card_reset = true;
            log::warn!(
                "{} Empty payload received while auth in progress. Reconnecting card.",
                self.log_header
            );
            // Persist the failed auth timestamp (aborted mid-flow; detached,
            // same reason as in `finish_session`).
            crate::config::record_auth_result_detached(&self.client_id, false);
            let reset = card_io_by(
                deadline,
                self.managed_card.reconnect(),
                &self.log_header,
                "reconnect",
            )
            .await;
            self.card_generation = self.card_generation.wrapping_add(1);
            // The card is clean again; the fresh session re-announces itself
            // on its first APDU.
            self.auth_process = reset.is_none();
            self.busy_announced = false;
        }

        // Session start is the only safe point to honor the requested T
        // protocol: the card has no session state to lose yet. Session-scoped
        // by design — not persisted to the config, the server owns the value
        // per tracker.
        if let Some(protocol) = protocol {
            card_reset |= protocol_to_str(protocol) != self.managed_card.protocol_str();
            let switched = card_io_by(
                deadline,
                self.managed_card.switch_protocol(protocol),
                &self.log_header,
                "switch_protocol",
            )
            .await;
            if switched.is_none() {
                self.auth_process = true;
            }
            // A protocol switch physically resets the card, same as a reconnect.
            self.card_generation = self.card_generation.wrapping_add(1);
        }

        // A reset lands on a swapped card without an error: the card about to
        // answer the VU must still be this session's one. A mismatch ends the
        // session; a timed-out check leaves the card dirty, so the next start
        // retries it.
        let verified = !card_reset || self.verify_card(deadline).await;
        if !verified {
            self.auth_process = true;
            return (SW_TECHNICAL_PROBLEM.to_string(), true);
        }
        log::info!("{} Authentication process started", self.log_header);
        // The card is online (not for a retired session: its row is gone).
        if !self.retired() {
            self.emit(Some(true), Some(false));
        }
        (self.atr.clone(), false)
    }

    /// Whether the card in the reader is still this session's card.
    async fn verify_card(&self, deadline: tokio::time::Instant) -> bool {
        card_io_by(
            deadline,
            self.managed_card.ensure_same_card(&self.client_id),
            &self.log_header,
            "verify_card",
        )
        .await
            == Some(true)
    }

    /// Runs one APDU of the VU on the card. Returns the response (or the
    /// technical-problem status) and whether the exchange failed.
    async fn send_apdu(&mut self, apdu_hex: &str, protocol: Option<Protocols>) -> (String, bool) {
        // A differing T protocol mid-session is ignored: switching resets the
        // card and would destroy the authentication state.
        if let Some(protocol) = protocol {
            if protocol_to_str(protocol) != self.managed_card.protocol_str() {
                log::warn!(
                    "{} server requested T protocol {} mid-session while card is on {}: ignoring",
                    self.log_header,
                    protocol_to_str(protocol),
                    self.managed_card.protocol_str()
                );
            }
        }
        if !self.auth_process {
            log::info!("{} Authentication APDU exchange started", self.log_header);
        }

        let exchange = card_io(
            self.managed_card.send_apdu(apdu_hex, &self.client_id),
            &self.log_header,
            "send_apdu",
        )
        .await;
        let rapdu = match exchange {
            Some(Ok(rapdu)) => {
                // Passive sniffer: extract plaintext EF data from SM'd
                // responses. Only a real card reply is worth parsing — feeding
                // it a command that never ran would poison the SELECT-EF
                // tracking.
                crate::apdu_sniffer::sniff(&self.client_id, apdu_hex, &rapdu);
                rapdu
            }
            failed => {
                // Transport failure (already logged by send_apdu) or a
                // timed-out exchange. The command may have reached the card (a
                // timeout, or an error that does not prove otherwise): its
                // state is unknown, so the next session start resets it.
                let undelivered = matches!(&failed, Some(Err(e)) if e.is_retriable());
                if !undelivered {
                    self.auth_process = true;
                }
                return (SW_TECHNICAL_PROBLEM.to_string(), true);
            }
        };

        // Announce "authenticating" once per session, not once per APDU: an
        // authentication is hundreds of exchanges, and each emit costs a
        // serialized IPC event plus a reactive row replacement in the UI
        // (multiplied across parallel rack cards). Skipped when the exchange
        // failed (above): the card was not reached, and claiming
        // PRESENT/online then resurrects a row the monitor may have just
        // deleted for an unplugged reader, leaving a ghost entry.
        if !self.busy_announced {
            self.emit(Some(true), Some(true));
            self.busy_announced = true;
        }
        // The card now holds session state.
        self.auth_process = true;
        (rapdu, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_cache_replays_only_the_last_id_of_the_same_card_state() {
        let mut cache = ReplyCache::default();
        assert!(matches!(cache.lookup(Some(1), 0), CacheLookup::Miss));
        cache.store(1, "reply", 0);
        assert!(matches!(cache.lookup(Some(1), 0), CacheLookup::Replay(ref r) if r == "reply"));
        // another id, or a request without one, is new
        assert!(matches!(cache.lookup(Some(2), 0), CacheLookup::Miss));
        assert!(matches!(cache.lookup(None, 0), CacheLookup::Miss));
        // the card was reset since: the reply describes a state that is gone
        assert!(matches!(cache.lookup(Some(1), 1), CacheLookup::Stale));
        cache.clear();
        assert!(matches!(cache.lookup(Some(1), 0), CacheLookup::Miss));
    }
}
