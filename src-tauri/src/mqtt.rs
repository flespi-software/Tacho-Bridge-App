//! Module for working with MQTT connections.
//!
//! This module provides functionality for creating and managing MQTT connections.

// ───── Std Lib ─────
use std::ffi::CStr; // For handling C-style strings in Rust.
use std::sync::atomic::{AtomicU64, Ordering}; // Card-activity timestamp for the auto-updater.
use std::time::{Duration, Instant}; // For specifying time durations and shutdown deadlines.

// ───── MQTT Client Library (rumqttc) ─────
use rumqttc::v5::mqttbytes::QoS; // Quality of Service levels for MQTT.
use rumqttc::v5::ConnectionError; // For handling MQTT connection errors.
use rumqttc::v5::StateError; // MQTT protocol state errors.
use rumqttc::v5::{AsyncClient, Event, Incoming, MqttOptions}; // Core MQTT async client and options.
use rumqttc::Outgoing; // Outgoing event markers (graceful DISCONNECT detection).

// ───── Tauri ─────
use tauri::async_runtime::{self, JoinHandle}; // Async runtime and task join handles for Tauri apps.

// ───── Serde (Serialization / Deserialization) ─────
use serde_json::Value; // For working with JSON data structures.

// ───── Local Modules ─────
use crate::config::get_from_cache; // Function to get data from cache for syncing server data.
use crate::config::split_host_to_parts; // Function to split the host into parts for MQTT connection.
use crate::config::CacheSection; // Enum for cache sections for getting data from cache.
use crate::global_app_handle::card_emit_event; // Sends events to the frontend via global app handle.
use crate::smart_card::ProcessingCard;
use crate::smart_card::{ManagedCard, SW_TECHNICAL_PROBLEM, TASK_POOL}; // Managed card object, global task pool, and the "card unreachable" status word.

/// Initial delay (in seconds) before the first reconnect attempt after a
/// connection failure. Subsequent failures back off exponentially up to
/// `RECONNECT_DELAY_MAX_SECS`.
const RECONNECT_DELAY_INITIAL_SECS: u64 = 10;
/// Upper bound for the reconnect backoff. Past this point we keep retrying
/// at this interval until either the server comes back or the task is killed.
const RECONNECT_DELAY_MAX_SECS: u64 = 300;
const GLOBAL_CARDS_SYNC_EVENT: &str = "global-cards-sync";
const CARD_PRESENT_STATE: &str = "PRESENT";

/// Shutdown reasons that `shutdown_connections` branches on (most reasons are
/// free-text log context only). Shared constants tie the producing call sites
/// to the consuming comparisons — a bare literal drifting out of sync would
/// silently re-enable the behavior the branch suppresses.
///
/// "A successor entry for the same client_id is being registered": skips both
/// the rack-side retry (it could only report served_by_reader) and, for the
/// app connection, the offline emit (a late `false` would race the successor's
/// `true`).
pub(crate) const SHUTDOWN_REASON_STALE_ENTRY_REPLACED: &str = "stale_entry_replaced";
pub(crate) const SHUTDOWN_REASON_APP_CONNECTION_REPLACED: &str = "app_connection_replaced";

/// How long a closing MQTT task gets to flush its DISCONNECT packet and exit
/// before being force-aborted.
const SHUTDOWN_FLUSH_TIMEOUT_MS: u64 = 2000;
/// Poll step while waiting for closing MQTT tasks to finish.
const SHUTDOWN_POLL_INTERVAL_MS: u64 = 50;

/// Returns the next reconnect delay given the current one (exponential, capped).
fn next_reconnect_delay(current: u64) -> u64 {
    current.saturating_mul(2).min(RECONNECT_DELAY_MAX_SECS)
}

/// Unix seconds of the most recent card-facing exchange: a server request on a
/// reader-backed card connection, or a rack serial envelope. A timestamp (not
/// a session counter) on purpose — a counter would leak on an aborted task and
/// block auto-updates forever, while an APDU flow going quiet is a robust
/// "no authentication in progress" signal.
static LAST_CARD_ACTIVITY_SECS: AtomicU64 = AtomicU64::new(0);

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Records "a card exchange is happening right now". Called on every server
/// request that reaches a card (reader APDU bridge and rack serial bridge).
pub fn touch_card_activity() {
    LAST_CARD_ACTIVITY_SECS.store(unix_now_secs(), Ordering::Relaxed);
}

/// Seconds since the last card exchange; a large value when none happened yet.
pub fn seconds_since_card_activity() -> u64 {
    unix_now_secs().saturating_sub(LAST_CARD_ACTIVITY_SECS.load(Ordering::Relaxed))
}

/// Maps a connection error to a short stable `kind` for one-line logs, plus a
/// flag saying whether it is routine network churn. Expected errors are logged
/// as a single WARN line without the full Debug dump (which used to repeat the
/// same "Connection reset by peer" six times per blip across three lines);
/// unexpected ones keep the details at ERROR. Shared by all three MQTT loops
/// (app, per-card, rack).
pub(crate) fn classify_connection_error(e: &ConnectionError) -> (String, bool) {
    use std::io::ErrorKind as K;

    fn io_kind(io: &std::io::Error) -> (String, bool) {
        // DNS failures surface as Uncategorized with a lookup message —
        // routine when the machine is offline.
        let msg = io.to_string();
        if msg.contains("lookup address") || msg.contains("dns error") {
            return ("dns_lookup_failed".to_string(), true);
        }
        let expected = matches!(
            io.kind(),
            K::ConnectionAborted
                | K::ConnectionReset
                | K::ConnectionRefused
                | K::TimedOut
                | K::BrokenPipe
                | K::NotConnected
                | K::UnexpectedEof
        );
        (format!("io_{:?}", io.kind()), expected)
    }

    match e {
        ConnectionError::Io(io) => io_kind(io),
        ConnectionError::MqttState(StateError::Io(io)) => io_kind(io),
        ConnectionError::MqttState(StateError::Deserialization(
            rumqttc::v5::mqttbytes::Error::Io(io),
        )) => io_kind(io),
        ConnectionError::MqttState(StateError::AwaitPingResp) => {
            ("await_ping_resp".to_string(), true)
        }
        ConnectionError::MqttState(StateError::ConnectionAborted) => {
            ("connection_aborted".to_string(), true)
        }
        ConnectionError::MqttState(StateError::ServerDisconnect { reason_code, .. }) => {
            (format!("server_disconnect_{:?}", reason_code), true)
        }
        ConnectionError::Timeout(_) => ("timeout".to_string(), true),
        ConnectionError::ConnectionRefused(code) => (format!("conn_refused_{:?}", code), true),
        _ => ("unhandled".to_string(), false),
    }
}

/// Logs a connection failure as exactly one line: WARN with a short kind for
/// routine churn, ERROR with full details for anything unexpected.
pub(crate) fn log_connection_failure(
    log_header: &str,
    scope: &str,
    transition: &str,
    e: &ConnectionError,
    retry_in_secs: u64,
) {
    let (kind, expected) = classify_connection_error(e);
    if expected {
        log::warn!(
            "{} [{}] state={} kind={} retry_in={}s",
            log_header,
            scope,
            transition,
            kind,
            retry_in_secs
        );
    } else {
        log::error!(
            "{} [{}] state={} kind={} err={:?} retry_in={}s",
            log_header,
            scope,
            transition,
            kind,
            e,
            retry_in_secs
        );
    }
}

/// Applies the optional MQTT credentials from the config's `server` section to
/// freshly built connection options. flespi authenticates a channel connection
/// with a username (typically a flespi token; password usually empty). No UI
/// for these yet — they are hand-edited in config.yaml; an absent or empty
/// username keeps today's anonymous connect. Shared by all four MQTT loops
/// (app, per-card, rack, rack-card) so authentication can never be enabled for
/// one transport and forgotten for another.
pub(crate) fn apply_mqtt_credentials(mqtt_options: &mut MqttOptions) {
    let username = get_from_cache(CacheSection::Server, "username");
    if username.is_empty() {
        return;
    }
    let password = get_from_cache(CacheSection::Server, "password");
    // The values themselves must never reach the log file.
    log::debug!("[CONN] mqtt credentials applied (username set)");
    mqtt_options.set_credentials(username, password);
}

/// Rewrites a `request/...` topic to its matching `response/...` topic.
/// Replaces only the leading segment so substrings deeper in the topic
/// (in client_id, parcel id, etc.) cannot be accidentally mangled.
/// Shared with the rack path in `com_port.rs` — one wire contract, one parser.
pub(crate) fn request_to_response_topic(topic: &str) -> String {
    if let Some(rest) = topic.strip_prefix("request/") {
        format!("response/{}", rest)
    } else {
        topic.to_string()
    }
}

/// Extracts the request id (the first segment after `request/`) from a topic of
/// the form `request/<id>/<sender>`. Used for idempotent handling of repeated
/// requests: the server re-sends the same id when it does not get a timely reply.
pub(crate) fn request_id_from_topic(topic: &str) -> Option<u64> {
    topic
        .strip_prefix("request/")
        .and_then(|rest| rest.split('/').next())
        .and_then(|id| id.parse::<u64>().ok())
}

fn emit_card_sync_event(
    iccid: &str,
    reader_name: &CStr,
    client_id: &str,
    is_online: Option<bool>,
    auth_in_progress: Option<bool>,
) {
    card_emit_event(
        GLOBAL_CARDS_SYNC_EVENT,
        iccid.to_owned(),
        reader_name.to_string_lossy().into(),
        CARD_PRESENT_STATE.into(),
        client_id.to_owned(),
        is_online,
        auth_in_progress,
    );
}

async fn publish_ack(
    mqtt_client: &AsyncClient,
    topic_ack: String,
    payload_ack: String,
    log_header: &str,
) {
    if let Err(e) = mqtt_client
        .publish(topic_ack, QoS::AtLeastOnce, false, payload_ack)
        .await
    {
        log::error!("{} MQTT publish failed: {:?}", log_header, e);
    } else {
        log::debug!("{} MQTT response published successfully", log_header);
    }
}

// /// Ensures an MQTT connection for the specified client ID.
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

    // Return early if the client_id is empty, as we cannot ensure a connection without a valid ID
    if client_id.is_empty() {
        log::warn!(
            "Reader: {:?}. ClientID is empty. Cannot ensure connection.",
            reader_name
        );
        return;
    }

    // Unlock task_pool mutex
    let mut task_pool = TASK_POOL.lock().await;

    // Only one connection per client_id. An existing entry blocks a new one
    // unless it is unusable: a finished task (returned early or panicked) will
    // never serve the card again, and an entry bound to a DIFFERENT reader
    // means the card was physically moved — its old session is dead weight and
    // would otherwise keep the card offline until a replug (the PCSC sweep may
    // process the new reader before the old reader's removal).
    if let Some(index) = task_pool
        .iter()
        .position(|card| card.client_id == client_id)
    {
        let finished = task_pool[index].task_handle.inner().is_finished();
        let same_reader =
            task_pool[index].reader_name.as_deref() == Some(reader_name.to_string_lossy().as_ref());
        if !finished && same_reader {
            log::info!(
                "[CONN] phase=ensure_connection status=skip_existing reader={} client_id={}",
                reader_name.to_string_lossy(),
                client_id
            );
            return;
        }
        // INVARIANT: no `.await` may sit between this removal and the push of
        // the successor entry at the end of this function. The guard is held
        // throughout, so concurrent pool readers (e.g. the rack side's
        // served_by_reader check) can never observe the client_id as absent —
        // an await inserted here would open exactly the client_id-collision
        // window this replacement logic exists to prevent.
        let old = task_pool.remove(index);
        crate::apdu_sniffer::forget(&old.client_id);
        log::warn!(
            "[CONN] phase=ensure_connection status=replacing_stale reader={} client_id={} reason={}",
            reader_name.to_string_lossy(),
            client_id,
            if finished { "task_finished" } else { "reader_changed" }
        );
        // Close the old session gracefully; detached so the fresh registration
        // below is not delayed behind its shutdown flush.
        async_runtime::spawn(shutdown_connections(
            vec![old],
            SHUTDOWN_REASON_STALE_ENTRY_REPLACED,
        ));
    }

    // Getting server data from the cache
    let full_host = get_from_cache(CacheSection::Server, "host");
    let (host, port) = match split_host_to_parts(&full_host) {
        Ok((host, port)) => {
            // log::debug!("Server data from cache: {:?}:{}", host, port);
            (host, port)
        }
        Err(e) => {
            log::error!(
                "[CONN] phase=ensure_connection status=failed reason=invalid_host reader={} client_id={} err={}",
                reader_name.to_string_lossy(),
                client_id,
                e
            );
            return;
        }
    };

    // A live rack-backed session for the same card number would collide with
    // the connection opened below: the flespi broker allows one session per
    // client_id and would drop the two connections in a loop. The card was
    // just physically detected in this reader, so it cannot still sit in a
    // rack slot — the rack session is stale (its server `disconnect` was lost
    // or is still in flight). Abort it before opening ours. Deliberately AFTER
    // every early return above (empty client_id, existing session, invalid
    // host): killing the rack session and then not opening the reader one
    // would leave the card served by neither transport.
    crate::com_port::abort_rack_card_session(&client_id);

    //////////////////////////////////////////////////
    //  Create a new client ID for the MQTT connection
    //////////////////////////////////////////////////
    let mut mqtt_options = MqttOptions::new(&client_id, &host, port);
    apply_mqtt_credentials(&mut mqtt_options);
    mqtt_options.set_keep_alive(Duration::from_secs(120));
    log::info!(
        "[CONN] phase=connect_attempt status=initialized reader={} client_id={} host={}:{}",
        reader_name.to_string_lossy(),
        client_id,
        host,
        port
    );

    // Create a new asynchronous MQTT client and its associated event loop
    // `mqtt_options` specifies the configuration for the MQTT connection
    // `10` is the capacity of the internal channel used by the event loop for buffering operations
    let (mqtt_client, mut eventloop) = AsyncClient::new(mqtt_options, 10);

    let mqtt_clinet_cloned = mqtt_client.clone();
    let client_id_cloned = client_id.clone();

    let reader_name = reader_name.to_owned(); // clonning the reader name for the async task
    let reader_name_str = reader_name.to_string_lossy().into_owned(); // for using outside async_runtime task

    let atr_clone = atr.clone(); // Using ATR inside async_runtime

    // format of the logging header
    let log_header: String = format!("{} |", client_id);

    let mut is_online: bool = false; // flag to control the card connection (to the server) status
    let mut was_online = false; // Flag to track the previous connection status
    let mut auth_process: bool = false; // Flag to control the authentication process
    let mut reconnect_delay_secs: u64 = RECONNECT_DELAY_INITIAL_SECS; // exponential backoff state

    // Idempotency state for the current MQTT connection. The server re-sends a command with the
    // same request_id when it does not get a timely reply. We remember the last id we answered and
    // its response so a repeated request re-sends the cached reply instead of re-running the card:
    // replaying a stateful authentication APDU can corrupt the card state, and the duplicate reply
    // would be rejected by the server anyway. Reset on every CONNACK, because a new MQTT session
    // restarts the server-side request_id counter at 1.
    let mut last_request_id: Option<u64> = None;
    let mut last_response_payload: Option<String> = None;
    // Generation of the physical card state. Bumped on every reset of the card
    // (`reconnect`, protocol switch), which wipes the security state a cached
    // reply was produced against. The cache records the generation it belongs
    // to, so a reply captured before a reset can never be replayed after one:
    // it would answer the server with data describing a card state that no
    // longer exists. Resets happen without the MQTT session dropping, so the
    // CONNACK reset below is not enough on its own.
    let mut card_generation: u64 = 0;
    let mut cached_generation: u64 = 0;

    // Identity of the pool entry this task belongs to (see
    // ProcessingCard::session_id): the failure path below must only ever drop
    // ITS OWN entry, never a successor registered under the same client_id.
    let session_id = crate::smart_card::next_session_id();

    // create async task for the mqtt client
    let handle: JoinHandle<()> = async_runtime::spawn(async move {
        let iccid: String = match managed_card.get_iccid().await {
            Ok(iccid) => {
                log::info!("{} [CONN] phase=init status=iccid_resolved", log_header);
                iccid
            }
            Err(e) => {
                log::error!(
                    "{} [CONN] phase=init status=aborted reason=iccid_resolve_failed err={}",
                    log_header,
                    e
                );
                emit_card_sync_event("", &reader_name, &client_id_cloned, Some(false), None);
                // Drop our own pool entry before giving up. The task is about to
                // end, so the entry would sit there dead — `ensure_connection`
                // only reclaims it when the SAME card is detected again, which
                // for a reader-backed card needs a re-plug or a rescan. Until
                // then the stale entry makes the card look served and blocks a
                // rack-backed session for the same number from starting.
                //
                // Matched by session_id, not client_id: the card may have moved
                // to another reader while this task was still starting, in which
                // case the pool already holds the successor's entry under the
                // same client_id — and removing THAT one would orphan a live
                // session (unstoppable by remove_connections, invisible to the
                // served_by_reader guard).
                //
                // Ordering is safe: the spawning `ensure_connection` holds the
                // TASK_POOL lock across the spawn and releases it only after
                // pushing our entry, so this lock cannot be taken before the
                // entry exists.
                let mut pool = TASK_POOL.lock().await;
                if let Some(index) = pool.iter().position(|card| card.session_id == session_id) {
                    pool.remove(index);
                    log::info!(
                        "{} [CONN] phase=init status=pool_entry_dropped reason=iccid_resolve_failed",
                        log_header
                    );
                }
                drop(pool);
                // The dropped entry may have been the reason a rack-backed
                // session for this number was skipped (served_by_reader) —
                // retry the rack side now that the number is free again.
                crate::com_port::connect_pending_rack_cards().await;
                return;
            }
        };

        log::info!(
            "{} [CONN] phase=eventloop status=started reader={} iccid={}",
            log_header,
            reader_name.to_string_lossy(),
            iccid
        );

        loop {
            match eventloop.poll().await {
                Ok(notification) => {
                    if !is_online {
                        is_online = true;
                        // Successful poll → reset the backoff so the next failure
                        // starts from the initial delay again.
                        reconnect_delay_secs = RECONNECT_DELAY_INITIAL_SECS;
                        if !was_online {
                            was_online = true;
                            // Send the global-cards-sync event to the frontend that card is connected
                            emit_card_sync_event(
                                &iccid,
                                &reader_name,
                                &client_id_cloned,
                                Some(true),
                                None,
                            );

                            log::info!(
                                "{} [CONN] state=OFFLINE->ONLINE cause=eventloop_poll_ok",
                                log_header
                            );
                        }
                    }

                    log::debug!("{} Notification: {:?}", log_header, notification);

                    match notification {
                        Event::Incoming(Incoming::Publish(publish)) => {
                            // Every publish on a card connection is a VU-driven
                            // exchange: mark card activity so the auto-updater
                            // never restarts the app mid-authentication.
                            touch_card_activity();
                            // Extracting the topic from the incoming data
                            let topic_str = match std::str::from_utf8(&publish.topic) {
                                Ok(str) => str,
                                Err(e) => {
                                    log::error!(
                                        "{} Failed to decode incoming topic as UTF-8: {:?}",
                                        log_header,
                                        e
                                    );
                                    continue;
                                }
                            };

                            // Convert &str to String for further use
                            let topic = topic_str.to_string();
                            // The contents of response and request are the same.
                            // Card number and parcel ID. So we just change the leading
                            // "request/" segment to "response/" — prefix-only swap to
                            // avoid mangling substrings inside the rest of the topic.
                            let topic_ack = request_to_response_topic(&topic);

                            // Idempotency: the server re-sends a request with the same id after a
                            // command timeout. If we already answered this id, re-send the cached
                            // response without touching the card; if it is still being processed, drop it.
                            let req_id = request_id_from_topic(&topic);
                            if req_id.is_some() && req_id == last_request_id {
                                match &last_response_payload {
                                    // Only replay a reply captured against the card state that is
                                    // still in place. After a reset the cached bytes describe a
                                    // card that no longer exists, so the duplicate must go to the
                                    // card again rather than be answered from a stale snapshot.
                                    Some(cached) if cached_generation == card_generation => {
                                        log::warn!(
                                            "{} duplicate request_id={:?}: re-sending cached response, skipping card exchange",
                                            log_header,
                                            req_id
                                        );
                                        publish_ack(
                                            &mqtt_client,
                                            topic_ack.clone(),
                                            cached.clone(),
                                            &log_header,
                                        )
                                        .await;
                                        continue;
                                    }
                                    Some(_) => {
                                        log::warn!(
                                            "{} duplicate request_id={:?} predates a card reset: \
                                             re-running it on the card instead of replaying the cache",
                                            log_header,
                                            req_id
                                        );
                                        last_request_id = None;
                                        last_response_payload = None;
                                    }
                                    None => {
                                        log::warn!(
                                            "{} duplicate request_id={:?} still in flight: ignoring",
                                            log_header,
                                            req_id
                                        );
                                        continue;
                                    }
                                }
                            }

                            // serializable data to interpret it as json
                            match serde_json::from_slice::<Value>(&publish.payload) {
                                Ok(json_payload) => {
                                    log::debug!("Parsed JSON payload: {:?}", json_payload);

                                    let mut payload_ack = String::new();
                                    // Set when the card exchange itself failed (send_apdu
                                    // returned a transport error and SW_TECHNICAL_PROBLEM was
                                    // substituted on the wire). Such a reply must never be
                                    // cached for idempotency — see the caching block below.
                                    // A genuine 6F00 answered BY the card is an Ok and is
                                    // cached like any other reply.
                                    let mut exchange_failed = false;

                                    // Server-requested T protocol for this session. Applied only on session
                                    // start (empty payload) below: switching is a physical card reset, so
                                    // honoring it mid-session would destroy the authentication state.
                                    let requested_protocol = json_payload
                                        .get("protocol")
                                        .and_then(|v| v.as_str())
                                        .and_then(|requested| {
                                            let parsed = crate::smart_card::protocol_from_str(requested);
                                            if parsed.is_none() {
                                                log::warn!(
                                                    "{} server requested unknown T protocol '{}': ignoring",
                                                    log_header,
                                                    requested
                                                );
                                            }
                                            parsed
                                        });

                                    // Check for the presence of the "finish" parameter
                                    if let Some(finish_value) =
                                        json_payload.get("finish").and_then(|v| v.as_bool())
                                    {
                                        log::debug!(
                                            "{} Finish parameter: {}",
                                            log_header,
                                            finish_value
                                        );

                                        // Processing the "finish" parameter depending on its value
                                        if finish_value {
                                            // Send the global-cards-sync event to the frontend that card is connected
                                            emit_card_sync_event(
                                                &iccid,
                                                &reader_name,
                                                &client_id_cloned,
                                                Some(true),
                                                Some(false),
                                            );

                                            log::info!(
                                                "{} Authentication process is finished",
                                                log_header
                                            );

                                            // Persist successful auth timestamp (detached: the
                                            // config write must not park the bridge on disk I/O
                                            // between `finish` and our reply).
                                            crate::config::record_auth_result_detached(
                                                &client_id_cloned,
                                                true,
                                            );

                                            // Reset the card to its original state
                                            managed_card.reconnect().await;
                                            // The card lost its security state: nothing captured
                                            // before this point may be replayed from cache.
                                            card_generation = card_generation.wrapping_add(1);

                                            payload_ack = process_rapdu_mqtt_hex("".to_string());

                                            auth_process = false; // Authorization process is finished

                                        // handle the case when finish == true
                                        } else {
                                            // finish flag is false here
                                            // PROCESS AUTHORIZATION WITH APDU COMMUNICATION
                                            // The "hex" parameter contains the apdu instruction that needs to be transferred to the card
                                            if let Some(hex_value) =
                                                json_payload.get("payload").and_then(|v| v.as_str())
                                            {
                                                log::debug!(
                                                    "{} TRACKER: Payload hex value: {}",
                                                    log_header,
                                                    hex_value
                                                );

                                                let is_session_start = hex_value.is_empty();
                                                if is_session_start {
                                                    // A fresh VU session starts with no file
                                                    // selected — stale sniffer state from the
                                                    // previous session must not leak into it.
                                                    crate::apdu_sniffer::forget(&client_id_cloned);
                                                }
                                                let rapdu_mqtt_hex = if is_session_start {
                                                    // This case is needed to reset the card when authorization is not completed, otherwise the card will not respond to commands correctly.
                                                    if auth_process {
                                                        log::warn!(
                                                            "{} Empty payload received while auth in progress. Reconnecting card.",
                                                            log_header
                                                        );
                                                        // Persist failed auth timestamp (aborted
                                                        // mid-flow; detached, same reason as above).
                                                        crate::config::record_auth_result_detached(
                                                            &client_id_cloned,
                                                            false,
                                                        );
                                                        // Reset the card to its original state
                                                        managed_card.reconnect().await;
                                                        card_generation =
                                                            card_generation.wrapping_add(1);
                                                    }

                                                    // Session start is the only safe point to honor the requested
                                                    // T protocol: the card has no session state to lose yet.
                                                    // Session-scoped by design - not persisted to the config,
                                                    // the server owns the value per tracker.
                                                    if let Some(protocol) = requested_protocol {
                                                        managed_card
                                                            .switch_protocol(protocol)
                                                            .await;
                                                        // A protocol switch physically resets the
                                                        // card, same as a reconnect.
                                                        card_generation =
                                                            card_generation.wrapping_add(1);
                                                    }

                                                    // If the input value is empty, then pass the ATR to the server.
                                                    log::info!(
                                                        "{} Authentication process started",
                                                        log_header
                                                    );

                                                    // Send the global-cards-sync event to the frontend that card is connected
                                                    emit_card_sync_event(
                                                        &iccid,
                                                        &reader_name,
                                                        &client_id_cloned,
                                                        Some(true),
                                                        Some(false),
                                                    );

                                                    atr_clone.clone()
                                                } else {
                                                    // a differing T protocol mid-session is ignored: switching resets
                                                    // the card and would destroy the authentication state
                                                    if let Some(protocol) = requested_protocol {
                                                        if crate::smart_card::protocol_to_str(
                                                            protocol,
                                                        ) != managed_card.protocol_str()
                                                        {
                                                            log::warn!(
                                                                "{} server requested T protocol {} mid-session while card is on {}: ignoring",
                                                                log_header,
                                                                crate::smart_card::protocol_to_str(protocol),
                                                                managed_card.protocol_str()
                                                            );
                                                        }
                                                    }

                                                    // // Otherwise, the logic for exchanging messages with the card.
                                                    if !auth_process {
                                                        log::info!(
                                                            "{} Authentication APDU exchange started",
                                                            log_header
                                                        );
                                                    }

                                                    let rapdu = match managed_card
                                                        .send_apdu(hex_value, &client_id_cloned)
                                                        .await
                                                    {
                                                        Ok(rapdu) => {
                                                            // Passive sniffer: extract plaintext EF
                                                            // data from SM'd responses. Only a real
                                                            // card reply is worth parsing — feeding
                                                            // it a command that never ran would
                                                            // poison the SELECT-EF tracking.
                                                            crate::apdu_sniffer::sniff(
                                                                &client_id_cloned,
                                                                hex_value,
                                                                &rapdu,
                                                            );
                                                            rapdu
                                                        }
                                                        Err(_) => {
                                                            // Transport failure (already logged by
                                                            // send_apdu): substitute the technical-
                                                            // problem status on the wire and keep
                                                            // this reply out of the idempotency
                                                            // cache so the server's retry reaches
                                                            // the card again.
                                                            exchange_failed = true;
                                                            SW_TECHNICAL_PROBLEM.to_string()
                                                        }
                                                    };

                                                    // Send the global-cards-sync event to the frontend that card is connected
                                                    emit_card_sync_event(
                                                        &iccid,
                                                        &reader_name,
                                                        &client_id_cloned,
                                                        Some(true),
                                                        Some(true),
                                                    );

                                                    auth_process = true; // Authorization process is in progress
                                                    rapdu
                                                };

                                                payload_ack = if is_session_start {
                                                    // the session-start (ATR) reply also reports the actual T protocol -
                                                    // the server seeds its per-tracker value from it and it signals that
                                                    // this application understands the 'protocol' request field
                                                    process_session_start_mqtt_hex(
                                                        rapdu_mqtt_hex,
                                                        managed_card.protocol_str(),
                                                    )
                                                } else {
                                                    process_rapdu_mqtt_hex(rapdu_mqtt_hex)
                                                };

                                                // log::info!("finish_value: {}", finish_value);
                                            } else {
                                                log::error!(
                                                    "{} Hex value not found or is not a string",
                                                    log_header
                                                );
                                            }

                                            log::debug!(
                                                "{} CARD: Payload hex value: {}",
                                                log_header,
                                                payload_ack
                                            );
                                        }

                                        // Cache this reply so a re-sent request with the same id is
                                        // answered from cache without re-running the card exchange.
                                        // Only real card replies are cached:
                                        //  * a malformed request leaves payload_ack empty, and
                                        //    pinning that against the id would answer the server's
                                        //    corrected retry from cache without touching the card;
                                        //  * a failed exchange (SW_TECHNICAL_PROBLEM, the card was
                                        //    never reached) must be retried on the card when the
                                        //    server re-sends — caching it would pin the failure to
                                        //    the id and deny the retry that the recreate path just
                                        //    made possible. Same rule the rack path applies via
                                        //    SerialExchange::is_ok().
                                        if req_id.is_some()
                                            && !payload_ack.is_empty()
                                            && !exchange_failed
                                        {
                                            last_request_id = req_id;
                                            last_response_payload = Some(payload_ack.clone());
                                            // Pin the reply to the card state it was produced
                                            // against; a later reset invalidates it.
                                            cached_generation = card_generation;
                                        }

                                        // publish a message to the channel
                                        publish_ack(
                                            &mqtt_client,
                                            topic_ack,
                                            payload_ack,
                                            &log_header,
                                        )
                                        .await;
                                    } else {
                                        log::error!(
                                            "{} Finish parameter not found or is not a boolean",
                                            log_header
                                        );
                                    }
                                }
                                Err(e) => {
                                    log::error!(
                                        "{} parsing JSON payload issue: {:?}",
                                        log_header,
                                        e
                                    );
                                }
                            }
                        }
                        Event::Incoming(Incoming::ConnAck(..)) => {
                            // New MQTT session: the server restarts its request_id counter at 1, so
                            // drop the idempotency slot to avoid mistaking a fresh request for a duplicate.
                            last_request_id = None;
                            last_response_payload = None;
                            // The OFFLINE->ONLINE transition is already logged
                            // at info; CONNACK itself is a detail.
                            log::debug!("{} [CONN] event=CONNACK status=received", log_header)
                        }
                        Event::Incoming(Incoming::PingResp(..)) => {
                            log::debug!("{} Ping response received from the server.", log_header);

                            // Send the global-cards-sync event to the frontend that card is connected
                            emit_card_sync_event(
                                &iccid,
                                &reader_name,
                                &client_id_cloned,
                                Some(true),
                                Some(false),
                            );
                        }
                        Event::Outgoing(Outgoing::Disconnect) => {
                            // graceful teardown: the DISCONNECT packet is already flushed to the
                            // socket, so exit instead of letting the loop treat the closing
                            // connection as a network failure and reconnect
                            log::info!(
                                "{} [CONN] phase=shutdown status=disconnect_sent",
                                log_header
                            );
                            break;
                        }
                        _ => {} // This handles any other events that you haven't explicitly matched above
                    }
                }
                Err(e) => {
                    // Send the global-cards-sync event to the frontend that card is connected
                    emit_card_sync_event(
                        &iccid,
                        &reader_name,
                        &client_id_cloned,
                        Some(false),
                        None,
                    );

                    let transition = if is_online {
                        "ONLINE->OFFLINE"
                    } else {
                        "OFFLINE"
                    };
                    is_online = false;
                    was_online = false; // Reset the flag when the connection is lost

                    // One line per failed poll: kind + retry delay; full error
                    // details only for genuinely unexpected failures.
                    log_connection_failure(
                        &log_header,
                        "CONN",
                        transition,
                        &e,
                        reconnect_delay_secs,
                    );

                    tokio::time::sleep(Duration::from_secs(reconnect_delay_secs)).await;
                    reconnect_delay_secs = next_reconnect_delay(reconnect_delay_secs);
                }
            }
        }
    });

    task_pool.push(ProcessingCard {
        client_id,
        session_id,
        reader_name: Some(reader_name_str),
        atr: Some(atr),
        mqtt_client: mqtt_clinet_cloned,
        task_handle: handle,
    });

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

/// Gracefully terminates MQTT tasks whose entries are already removed from
/// TASK_POOL: queues a clean DISCONNECT for each one (the server then logs a
/// normal close instead of an internal error), waits for the event loops to
/// flush the packet and exit on their own, and force-aborts the stragglers.
pub async fn shutdown_connections(cards: Vec<ProcessingCard>, reason: &str) {
    // A reader-backed session that closes here may have been the reason a
    // rack-backed session for the same card number was skipped
    // (`served_by_reader`) — remembered so the rack side can be retried once
    // the teardown is done (see the trailing call below). The retry is skipped
    // on the replacement path: a successor entry for the same client_id is
    // (or is being) registered, so the retry could only ever report
    // served_by_reader again.
    let had_reader_backed = reason != SHUTDOWN_REASON_STALE_ENTRY_REPLACED
        && cards.iter().any(|card| card.reader_name.is_some());

    // Phase 1: queue DISCONNECTs; a task whose request queue is unreachable
    // (already dead or clogged) has nothing to flush and is aborted right away.
    let mut waiting: Vec<ProcessingCard> = Vec::with_capacity(cards.len());
    for card in cards {
        // Single choke point every removal path goes through: drop the card's
        // sniffer state here so no path (e.g. physical card extraction) leaks
        // an entry in the sniffer's global map. No-op when absent.
        crate::apdu_sniffer::forget(&card.client_id);
        // The app-level connection (the one entry with no reader) never emits
        // its own offline transition when force-aborted mid-backoff — tell the
        // frontend explicitly so the UI cannot keep showing a dead connection
        // as online. The replacement path skips this: its successor connection
        // starts immediately and a late `false` would race the new `true`.
        if card.reader_name.is_none() && reason != SHUTDOWN_REASON_APP_CONNECTION_REPLACED {
            crate::global_app_handle::app_emit_event(false);
        }
        match card.mqtt_client.try_disconnect() {
            Ok(()) => {
                log::info!(
                    "{} | [CONN] phase=shutdown status=disconnect_queued reason={}",
                    card.client_id,
                    reason
                );
                waiting.push(card);
            }
            Err(e) => {
                log::warn!(
                    "{} | [CONN] phase=shutdown status=force_abort reason={} err={:?}",
                    card.client_id,
                    reason,
                    e
                );
                card.task_handle.abort();
            }
        }
    }

    // Phase 2: one shared deadline for all tasks - the event loop exits itself
    // after flushing the DISCONNECT (see the Outgoing::Disconnect break arm).
    let deadline = Instant::now() + Duration::from_millis(SHUTDOWN_FLUSH_TIMEOUT_MS);
    while !waiting.is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(SHUTDOWN_POLL_INTERVAL_MS)).await;
        waiting.retain(|card| {
            if card.task_handle.inner().is_finished() {
                log::info!(
                    "{} | [CONN] phase=shutdown status=closed_cleanly reason={}",
                    card.client_id,
                    reason
                );
                false
            } else {
                true
            }
        });
    }

    // A task still alive here is offline or wedged (e.g. sitting in reconnect
    // backoff, unable to process the queued DISCONNECT) - kill it like before.
    for card in waiting {
        log::warn!(
            "{} | [CONN] phase=shutdown status=force_abort reason=flush_timeout_{}ms",
            card.client_id,
            SHUTDOWN_FLUSH_TIMEOUT_MS
        );
        card.task_handle.abort();
    }

    // The card numbers just released may belong to cards physically sitting in
    // a rack whose session spawn was skipped while a reader served the number.
    // Retry the rack side; a no-op when no rack is connected or nothing is
    // pending (and during app shutdown the rack ports are already gone).
    if had_reader_backed {
        crate::com_port::connect_pending_rack_cards().await;
    }
}

/// Terminates connections for the specified client IDs (card numbers).
pub async fn remove_connections(client_ids: Vec<String>) {
    log::debug!("Removing connections for client_ids: {:?}", client_ids);

    // Collect the matching cards under the pool lock, then close them outside
    // of it so the disconnect flush cannot block new card registrations.
    let mut removed: Vec<ProcessingCard> = Vec::new();
    {
        let mut task_pool = TASK_POOL.lock().await;
        for client_id in client_ids {
            // Find the index of the card with the matching client_id
            if let Some(index) = task_pool
                .iter()
                .position(|card| card.client_id == client_id)
            {
                let card = task_pool.remove(index);

                // Drop any APDU sniffer state we accumulated for this client so the
                // global HashMap does not grow without bound across reconnects.
                crate::apdu_sniffer::forget(&card.client_id);

                log::debug!(
                    "TASK_POOL: Connection scheduled for shutdown for client_id: {}, reader: {}, atr: {}",
                    card.client_id,
                    card.reader_name.as_deref().unwrap_or("unknown"),
                    card.atr.as_deref().unwrap_or("unknown"),
                );
                removed.push(card);
            } else {
                log::warn!(
                    "TASK_POOL: No active connection found for requested client_id: {}",
                    client_id
                );
                // Also clear any stale sniffer state, just in case (no-op if absent).
                crate::apdu_sniffer::forget(&client_id);
            }
        }
    }

    shutdown_connections(removed, "removed_by_request").await;
}

/// Terminates all active card-related MQTT connections and clears the task pool.
pub async fn remove_connections_all() {
    log::info!("Removing all card connections...");

    // Drain the pool under the lock, then close the connections outside of it.
    let cards: Vec<ProcessingCard> = {
        let mut task_pool = TASK_POOL.lock().await;
        task_pool.drain(..).collect()
    };

    // Clear all APDU sniffer state in one shot.
    crate::apdu_sniffer::forget_all();

    shutdown_connections(cards, "remove_all").await;

    log::info!("All card connections have been terminated and the task pool has been cleared.");
}

fn process_rapdu_mqtt_hex(rapdu_mqtt_hex: String) -> String {
    // Create a JSON object with the hex value
    serde_json::json!({
        "payload": rapdu_mqtt_hex,
    })
    // Serialize the JSON object to a string and assign it to `payload_ack`
    .to_string()
}

// Session-start (ATR) reply: the payload plus the actual T protocol the card is opened with.
// Old servers ignore the extra field, new ones seed their per-tracker protocol value from it.
fn process_session_start_mqtt_hex(atr_hex: String, protocol: &str) -> String {
    serde_json::json!({
        "payload": atr_hex,
        "protocol": protocol,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_session_start_mqtt_hex_reports_payload_and_protocol() {
        let json = process_session_start_mqtt_hex("3BDB96FF".to_string(), "T1");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(
            parsed.get("payload").and_then(|v| v.as_str()),
            Some("3BDB96FF")
        );
        assert_eq!(parsed.get("protocol").and_then(|v| v.as_str()), Some("T1"));
    }

    #[test]
    fn process_rapdu_mqtt_hex_has_no_protocol_field() {
        // mid-session replies must stay in the old shape - only session start reports the protocol
        let parsed: serde_json::Value =
            serde_json::from_str(&process_rapdu_mqtt_hex("9000".to_string())).expect("valid json");
        assert!(parsed.get("protocol").is_none());
    }

    #[test]
    fn request_to_response_topic_swaps_prefix_only() {
        assert_eq!(
            request_to_response_topic("request/1/ABCDEF0123456789"),
            "response/1/ABCDEF0123456789"
        );
    }

    #[test]
    fn request_to_response_topic_does_not_touch_inner_substrings() {
        // The literal substring "request" appears inside the payload-id segment;
        // the prefix-only swap must leave it intact.
        assert_eq!(
            request_to_response_topic("request/1/clientrequest-99"),
            "response/1/clientrequest-99"
        );
    }

    #[test]
    fn request_to_response_topic_returns_unchanged_when_no_prefix() {
        assert_eq!(
            request_to_response_topic("status/1/ABCDEF"),
            "status/1/ABCDEF"
        );
    }

    #[test]
    fn request_id_from_topic_parses_first_segment_only() {
        assert_eq!(
            request_id_from_topic("request/354/0000000067664100"),
            Some(354)
        );
        assert_eq!(request_id_from_topic("request/1/ABCDEF"), Some(1));
    }

    #[test]
    fn request_id_from_topic_rejects_non_numeric_or_wrong_prefix() {
        assert_eq!(request_id_from_topic("request/abc/ABCDEF"), None);
        assert_eq!(request_id_from_topic("response/1/ABCDEF"), None);
        assert_eq!(request_id_from_topic("request/"), None);
    }

    #[test]
    fn next_reconnect_delay_doubles_then_saturates() {
        assert_eq!(next_reconnect_delay(10), 20);
        assert_eq!(next_reconnect_delay(20), 40);
        assert_eq!(next_reconnect_delay(150), RECONNECT_DELAY_MAX_SECS);
        assert_eq!(
            next_reconnect_delay(RECONNECT_DELAY_MAX_SECS),
            RECONNECT_DELAY_MAX_SECS
        );
    }

    #[test]
    fn next_reconnect_delay_overflow_safe() {
        // saturating_mul prevents overflow; should still cap at max.
        assert_eq!(next_reconnect_delay(u64::MAX), RECONNECT_DELAY_MAX_SECS);
    }

    #[test]
    fn process_rapdu_mqtt_hex_emits_payload_field() {
        let json = process_rapdu_mqtt_hex("DEADBEEF".to_string());
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(
            parsed.get("payload").and_then(|v| v.as_str()),
            Some("DEADBEEF")
        );
    }
}
