//! Rack-backed per-card MQTT sessions.
//!
//! The server discovers cards by ICCID and publishes the set of cards of a
//! rack to serve (`rack/<id>/cards`); the sessions are reconciled with that
//! set. Each session is an MQTT connection under the card's own number,
//! funnelling its envelopes into the serial port of the rack that holds the
//! card. Also hosts the card presence watches, one per connected rack, armed by
//! the server once its discovery has walked that rack.

use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::{AsyncClient, Event, Incoming};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tauri::async_runtime::{self, JoinHandle};

use crate::config::{get_from_cache, split_host_to_parts, CacheSection};
use crate::global_app_handle::{rack_update_cards, RackCard};
use crate::smart_card::TASK_POOL;

use super::rack::{
    app_link_is, publish_reply, rack_topic, run_serial_request, take_rebind, IdempotencySlot,
};
use super::state::{mutate_rack_rows, set_rack_card_state, update_rack_card_ui};
use super::transport::{
    execute_envelope, normalize_hex, SerialEnvelope, SharedPort, SERIAL_MS_MAX, SERIAL_MS_MIN,
    SERIAL_READ_DEADLINE, SERIAL_REPLY_TIMEOUT,
};
use super::{
    linked_rack_ports, lock, next_reconnect_delay, rack_port_is_live,
    RECONNECT_DELAY_INITIAL_SECS,
};

/// One rack-backed card session: the running task plus the identity it was
/// spawned with. `rack_id` is the id of the rack whose serial port the session
/// writes to — server messages of one rack must never touch sessions that
/// belong to another.
struct RackCardTask {
    card_number: String,
    slot: u16,
    rack_id: String,
    handle: JoinHandle<()>,
    /// MQTT client of the session, set by its loop once built. Lets a repeated
    /// card set that still lists this card ask the live session to re-publish
    /// its rack link report without disturbing the session itself — the server
    /// needs that report to know the card is still served (it repaints the
    /// slot LED from it) after the application connection re-established and
    /// its discovery re-ran.
    client: Arc<OnceLock<AsyncClient>>,
}

lazy_static::lazy_static! {
    /// Rack-backed per-card MQTT tasks, keyed by ICCID (the stable card
    /// identifier — a physical card sits in exactly one slot of one rack).
    /// Keying by the config-resolved card number would leak the session if the
    /// config entry is deleted or edited while the card sits in the rack — the
    /// reconcile lookup would then miss the running task. The (rack, slot) the
    /// session was spawned for is kept so a card set naming the same slot
    /// with a different ICCID (card swapped) evicts the stale session, and so
    /// a set of another rack naming this card (card moved while the old
    /// rack's set is delayed) replaces the session instead of being skipped.
    static ref RACK_CARD_TASKS: std::sync::Mutex<HashMap<String, RackCardTask>> =
        std::sync::Mutex::new(HashMap::new());

    /// Cards currently exposed by each rack (RackState.cards), keyed by the
    /// rack id.
    pub(super) static ref RACK_CARDS_UI: std::sync::Mutex<HashMap<String, Vec<RackCard>>> =
        std::sync::Mutex::new(HashMap::new());
}

/// Live card identities survive an application MQTT reconnect. The server uses
/// this snapshot to preserve sessions while validating the rack again.
pub(super) fn inventory(rack_id: &str) -> serde_json::Value {
    let tasks = lock(&RACK_CARD_TASKS);
    let mut cards: Vec<_> = tasks.iter()
        .filter(|(_, task)| task.rack_id == rack_id && !task.handle.inner().is_finished())
        .map(|(iccid, task)| serde_json::json!({"slot": task.slot, "iccid": iccid}))
        .collect();
    cards.sort_by_key(|card| card["slot"].as_u64());
    serde_json::Value::Array(cards)
}

/// Opens rack-backed sessions for discovered cards that currently have none.
/// Covers two situations the server will not retry on its own (it repeats the
/// card set only when the rack content changes):
///  * a card whose ICCID was unknown at discovery time and has since been
///    assigned a number in the UI (config change);
///  * a card whose spawn was skipped because a reader-backed session served
///    the same number (`served_by_reader`) and that reader session has since
///    been torn down (see the hook in `mqtt::shutdown_connections`).
pub async fn connect_pending_rack_cards() {
    // Live racks and their serial ports.
    let ports: Vec<(String, SharedPort)> = linked_rack_ports();
    if ports.is_empty() {
        return; // no rack connected
    }

    // UI rows first, live-session filter second — the two locks are taken
    // sequentially, never nested (abort paths lock them in the other order).
    let rows: Vec<(String, u16, String)> = {
        let ui = lock(&RACK_CARDS_UI);
        ui.iter()
            .flat_map(|(rack_id, cards)| {
                cards.iter().filter_map(|card| {
                    card.iccid
                        .clone()
                        .map(|iccid| (rack_id.clone(), card.slot, iccid))
                })
            })
            .collect()
    };
    let pending: Vec<(String, u16, String)> = {
        let tasks = lock(&RACK_CARD_TASKS);
        rows.into_iter()
            .filter(|(_, _, iccid)| {
                // Only rows without a live session are pending; rows already
                // served skip the whole resolve/spawn round-trip here.
                tasks
                    .get(iccid)
                    .map(|task| task.handle.inner().is_finished())
                    .unwrap_or(true)
            })
            .collect()
    };

    for (rack_id, slot, iccid) in pending {
        let Some(card_number) = crate::config::find_card_number_by_iccid(&iccid) else {
            continue; // still unassigned
        };
        let Some((_, port)) = ports.iter().find(|(id, _)| *id == rack_id) else {
            continue; // that rack is gone
        };
        log::info!(
            "RACK | [SPAWN] status=pending_card_resolved rack={} slot={} iccid={} card={}",
            rack_id,
            slot,
            iccid,
            card_number
        );
        update_rack_card_ui(&rack_id, slot, &iccid, Some(card_number.clone()));
        spawn_rack_card_checked(
            card_number,
            iccid,
            slot,
            rack_id.clone(),
            port.clone(),
            "RACK |",
            // a replay of cached UI rows: must not displace a live session
            false,
            // no server discovery behind it: nothing to re-bind
            false,
        )
        .await;
    }
}

/// Starts the rack-backed MQTT session of one card, deduplicating by ICCID and
/// by card number (two slots mapped to the same number in the config must not
/// open two MQTT connections with the same client_id).
///
/// `from_server` tells whether this spawn is driven by a fresh server card set
/// (the server just read the card's ICCID in that slot — ground truth) or by a
/// local replay of cached UI rows (`connect_pending_rack_cards`). Only the
/// former may relocate a live session to another rack/slot: a replayed row can
/// be stale, and rebinding a working session to it would funnel the card's
/// APDUs into a port where the card no longer sits.
///
/// `rebind` tells whether a card already served must re-publish its rack link
/// report — true only for the card set that follows a `link up` (see
/// `rack::take_rebind`).
fn spawn_rack_card(
    card_number: String,
    iccid: String,
    slot: u16,
    rack_id: String,
    serial_port: SharedPort,
    from_server: bool,
    rebind: bool,
) {
    // The rack may have been torn down while the caller was awaiting between
    // its dedup check and this spawn (config-change path racing the monitor).
    // A session installed after the teardown would hold the stale serial port
    // and an MQTT client_id forever — verify the port is still the live one.
    if !rack_port_is_live(&rack_id, &serial_port) {
        log::warn!(
            "RACK | [SPAWN] card={} rack={} slot={} status=skipped reason=rack_gone",
            card_number,
            rack_id,
            slot
        );
        return;
    }

    let mut tasks = lock(&RACK_CARD_TASKS);
    // A different card now occupies this slot of this rack: the previous
    // occupant's session is dead weight (its card is gone) — evict it. Scoped
    // to the rack: slot numbers repeat across racks (every rack has a slot 1).
    let stale: Vec<String> = tasks
        .iter()
        .filter(|(other_iccid, task)| {
            **other_iccid != iccid && task.rack_id == rack_id && task.slot == slot
        })
        .map(|(other_iccid, _)| other_iccid.clone())
        .collect();
    for old_iccid in stale {
        if let Some(old) = tasks.remove(&old_iccid) {
            old.handle.abort();
            log::info!(
                "RACK | [SPAWN] card={} rack={} slot={} status=aborted reason=slot_reassigned",
                old.card_number,
                rack_id,
                slot
            );
        }
    }
    // Same ICCID: a repeat of the same place is a no-op, but the same card
    // reported from another rack or slot means it physically moved while the
    // old session was still alive — e.g. the application connection was down,
    // so the old rack's set did not arrive yet. The newest set is ground truth
    // (the server just read this ICCID in that slot): replace the session so
    // the card is served where it actually is.
    let moved = match tasks.get(&iccid) {
        Some(existing) if !existing.handle.inner().is_finished() => {
            if existing.rack_id == rack_id && existing.slot == slot {
                // The server re-ran discovery for a slot it already served —
                // typically the application connection re-established while
                // this card session stayed up. Such a discovery may paint the
                // slot as unserved (red LED); re-publishing the link report
                // over the live session tells the server the card is still
                // connected, which re-binds the slot and repaints it. Only
                // after a `link up`, though: every report costs a serial LED
                // frame, and a routine card set (one per discovery chain, and
                // a chain runs on every card insertion or removal) would then
                // put one frame per card of the rack on the wire, ahead of the
                // tracker exchanges. Detached: publish() can park on a full
                // request channel, and this path holds the task-map lock.
                if let Some(client) = existing.client.get().cloned().filter(|_| rebind) {
                    let card = existing.card_number.clone();
                    let report = link_report(&iccid, slot, &rack_id);
                    async_runtime::spawn(async move {
                        match client.publish("rack", QoS::AtLeastOnce, false, report).await {
                            Ok(()) => log::info!(
                                "RACKCARD {} | [MQTT] status=link_report_republished slot={}",
                                card,
                                slot
                            ),
                            Err(e) => log::warn!(
                                "RACKCARD {} | [MQTT] status=link_report_republish_failed err={:?}",
                                card,
                                e
                            ),
                        }
                    });
                }
                log::debug!(
                    "RACK | [SPAWN] card={} status=skipped reason=already_running",
                    card_number
                );
                return;
            }
            // A live session elsewhere, but this spawn is a local replay of a
            // cached UI row — that row may be the stale duplicate, so it must
            // not displace a working session (see `from_server` above).
            if !from_server {
                log::warn!(
                    "RACK | [SPAWN] card={} rack={} slot={} status=skipped reason=live_session_elsewhere",
                    card_number,
                    rack_id,
                    slot
                );
                return;
            }
            true
        }
        _ => false,
    };
    if moved {
        if let Some(old) = tasks.remove(&iccid) {
            old.handle.abort();
            log::info!(
                "RACK | [SPAWN] card={} status=aborted reason=card_moved from_rack={} from_slot={} to_rack={} to_slot={}",
                old.card_number,
                old.rack_id,
                old.slot,
                rack_id,
                slot
            );
        }
    }
    if tasks.iter().any(|(other_iccid, task)| {
        *other_iccid != iccid
            && task.card_number == card_number
            && !task.handle.inner().is_finished()
    }) {
        log::warn!(
            "RACK | [SPAWN] card={} rack={} slot={} status=skipped reason=served_by_another_slot",
            card_number,
            rack_id,
            slot
        );
        return;
    }
    log::info!(
        "RACK | [SPAWN] card={} rack={} slot={} status=starting_session",
        card_number,
        rack_id,
        slot
    );
    let client_slot = Arc::new(OnceLock::new());
    let handle = async_runtime::spawn(rack_card_mqtt_loop(
        card_number.clone(),
        iccid.clone(),
        slot,
        rack_id.clone(),
        serial_port,
        client_slot.clone(),
    ));
    tasks.insert(
        iccid,
        RackCardTask {
            card_number,
            slot,
            rack_id,
            handle,
            client: client_slot,
        },
    );
}

/// The rack link report of a card session: `{"iccid","slot","rack"}`. The
/// server binds the session to its slot from `iccid` and `slot`; `rack` (the
/// rack id) is diagnostics only, the server ignores it.
fn link_report(iccid: &str, slot: u16, rack_id: &str) -> String {
    serde_json::json!({ "iccid": iccid, "slot": slot, "rack": rack_id }).to_string()
}

/// Task aborts (card removed/moved or a rack restart) release local ownership.
struct AuthCleanup(SharedPort, u16);
impl Drop for AuthCleanup {
    fn drop(&mut self) {
        super::access::release(&self.0, self.1, false, None);
    }
}

/// MQTT loop of one rack-backed card connection: the opaque envelope handling funneled into
/// the rack's serial port, plus the one-shot **rack link report** right after CONNACK (topic
/// `rack`, see `link_report`) that binds this card session to its slot on the server. Without
/// the report the server treats the card as reader-backed and uses the plain PC/SC envelope.
async fn rack_card_mqtt_loop(
    card_number: String,
    iccid: String,
    slot: u16,
    rack_id: String,
    serial_port: SharedPort,
    // shared back-reference to this session's MQTT client, filled once built —
    // see RackCardTask.client
    client_slot: Arc<OnceLock<AsyncClient>>,
) {
    let _ownership = AuthCleanup(serial_port.clone(), slot);
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

    let mut is_online = false;
    let mut reconnect_delay_secs = RECONNECT_DELAY_INITIAL_SECS;
    let mut idempotency = IdempotencySlot::default();

    loop {
        match eventloop.poll().await {
            Ok(notification) => {
                if !is_online {
                    is_online = true;
                    reconnect_delay_secs = RECONNECT_DELAY_INITIAL_SECS;
                    log::info!(
                        "{} [MQTT] state=OFFLINE->ONLINE cause=eventloop_poll_ok",
                        log_header
                    );
                }

                match notification {
                    Event::Incoming(Incoming::ConnAck(..)) => {
                        // new MQTT session: the server-side request_id counter restarts at 1
                        idempotency.reset();
                        super::access::release(&serial_port, slot, false, None);
                        // Session is up: show the card as served and idle.
                        set_rack_card_state(&iccid, true, false);
                        // rack link report: must be the first publish of the session
                        let report = link_report(&iccid, slot, &rack_id);
                        if let Err(e) = mqtt_client
                            .publish("rack", QoS::AtLeastOnce, false, report)
                            .await
                        {
                            log::error!(
                                "{} [MQTT] status=link_report_failed err={:?}",
                                log_header,
                                e
                            );
                        } else {
                            log::info!(
                                "{} [MQTT] status=link_report_sent slot={} iccid={}",
                                log_header,
                                slot,
                                iccid
                            );
                        }
                    }
                    Event::Incoming(Incoming::Publish(publish)) => {
                        let topic = String::from_utf8_lossy(&publish.topic).into_owned();
                        log::info!(
                            "{} [MQTT] event=command topic={} bytes={} qos={:?}",
                            log_header,
                            topic,
                            publish.payload.len(),
                            publish.qos,
                        );
                        log::debug!(
                            "{} [MQTT] command_text={}",
                            log_header,
                            String::from_utf8_lossy(&publish.payload)
                        );
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
                            continue;
                        }
                        // Activity marking happens inside, driven by the
                        // envelope's `finish` flag rather than by the mere
                        // arrival of a command.
                        if let Some((resp_topic, resp_payload)) = run_serial_request(
                            &topic,
                            &publish.payload,
                            &serial_port,
                            &log_header,
                            &mut idempotency,
                            Some((&iccid, &card_number, slot)),
                        )
                        .await
                        {
                            publish_reply(&mqtt_client, resp_topic, resp_payload, &log_header)
                                .await;
                        }
                    }
                    Event::Incoming(Incoming::PingResp(..)) => {
                        // A keep-alive ping means this connection sent nothing
                        // for the whole keep-alive window: no serial exchange
                        // is in flight. Clear the busy indicator — same
                        // self-heal the reader path performs in mqtt.rs. This
                        // is the only recovery when the closing `finish:true`
                        // envelope never arrives (tracker aborted the session,
                        // message lost, or an older server that does not send
                        // the flag at all); without it the activity animation
                        // blinks forever on an idle card.
                        set_rack_card_state(&iccid, true, false);
                        log::debug!("{} [MQTT] event=ping_resp status=idle", log_header);
                    }
                    other => {
                        log::debug!("{} [MQTT] event=other detail={:?}", log_header, other);
                    }
                }
            }
            Err(e) => {
                let transition = if is_online {
                    "ONLINE->OFFLINE"
                } else {
                    "OFFLINE"
                };
                is_online = false;
                super::access::release(&serial_port, slot, false, None);
                // Connection lost: the card is present in its slot but no
                // longer served, so it must stop looking active.
                set_rack_card_state(&iccid, false, false);
                crate::mqtt::log_connection_failure(
                    &log_header,
                    "MQTT",
                    transition,
                    &e,
                    reconnect_delay_secs,
                );
                tokio::time::sleep(Duration::from_secs(reconnect_delay_secs)).await;
                reconnect_delay_secs = next_reconnect_delay(reconnect_delay_secs);
            }
        }
    }
}

lazy_static::lazy_static! {
    /// Card presence watch tasks, one per connected rack, keyed by rack id.
    static ref RACK_WATCH_TASKS: std::sync::Mutex<HashMap<String, JoinHandle<()>>> =
        std::sync::Mutex::new(HashMap::new());
}

/// Arms (or re-arms) one rack's card presence watch from a server `rack/<id>/watch` instruction:
/// `{"cmd":"<hex>","interval_ms":1000,"idle_ms":...,"deadline_ms":...}`. A background task
/// executes the opaque command every interval through that rack's FIFO port queue and publishes
/// the reply back (topic `rack/<id>/watch` of the application connection, the standard response
/// envelope) ONLY when its bytes change. Re-arming resets the baseline, so the first successful
/// exchange is always published — that is how the server catches updates missed while its
/// discovery chain was busy. The report goes to the server instance the instruction came from
/// (`generation`, see `rack::app_link_is`); once that instance is gone or the rack's link is
/// down (the port went away), nothing is published any more.
pub(super) fn start_rack_watch(
    payload: &[u8],
    generation: u64,
    rack_id: &str,
    serial_port: &SharedPort,
    mqtt_client: &AsyncClient,
    log_header: &str,
) {
    let json = match serde_json::from_slice::<serde_json::Value>(payload) {
        Ok(json) => json,
        Err(e) => {
            log::warn!(
                "{} [WATCH] status=ignored reason=bad_json err={}",
                log_header,
                e
            );
            return;
        }
    };
    let Some(cmd) = json.get("cmd").and_then(|v| v.as_str()) else {
        log::warn!("{} [WATCH] status=ignored reason=no_cmd_field", log_header);
        return;
    };
    let cmd_hex = match normalize_hex(cmd) {
        Ok(hex) => hex,
        Err(_) => {
            log::warn!("{} [WATCH] status=ignored reason=bad_cmd_hex", log_header);
            return;
        }
    };
    let ms = |key: &str| {
        json.get(key)
            .and_then(|v| v.as_u64())
            .map(|v| v.min(SERIAL_MS_MAX))
    };
    let interval = Duration::from_millis(ms("interval_ms").unwrap_or(1000).max(SERIAL_MS_MIN));
    let refresh = Duration::from_millis(ms("refresh_ms").unwrap_or(30_000).max(1000));
    let idle = ms("idle_ms")
        .map(Duration::from_millis)
        .unwrap_or(SERIAL_REPLY_TIMEOUT);
    let deadline = ms("deadline_ms")
        .map(Duration::from_millis)
        .unwrap_or(SERIAL_READ_DEADLINE);

    let serial_port = serial_port.clone();
    let mqtt_client = mqtt_client.clone();
    let log_header = log_header.to_string();
    let watch_topic = rack_topic(rack_id, "watch");
    let watched_rack = rack_id.to_string();
    log::info!(
        "{} [WATCH] status=armed interval={:?} cmd_bytes={}",
        log_header,
        interval,
        cmd_hex.len() / 2
    );

    let handle = async_runtime::spawn(async move {
        let mut last: Option<String> = None;
        let mut last_report = std::time::Instant::now();
        loop {
            tokio::time::sleep(interval).await;
            let envelope = SerialEnvelope {
                cmd_hex: cmd_hex.clone(),
                expect_hex: None,
                idle,
                deadline,
                poll: None,
                // presence polling is not part of any authentication session
                finish: None,
            };
            let (exchange, end) =
                execute_envelope(&serial_port, envelope, &log_header, false).await;
            if !exchange.is_ok() {
                // transport errors are already logged by execute_envelope; the rack presence
                // monitor handles a truly gone device, so just keep trying
                continue;
            }
            if last.as_deref() == Some(exchange.resp_hex.as_str()) && last_report.elapsed() < refresh {
                continue;
            }
            // The link went down while the exchange ran: this watch is being
            // stopped, and the server must not hear from the old port life.
            if !rack_port_is_live(&watched_rack, &serial_port) {
                log::debug!("{} [WATCH] status=report_dropped reason=link_down", log_header);
                continue;
            }
            // The application connection changed under this watch: the server
            // instance that armed it is gone, and the new one arms its own.
            if !app_link_is(generation) {
                log::debug!(
                    "{} [WATCH] status=report_dropped reason=app_connection_changed",
                    log_header
                );
                continue;
            }
            // `end` says whether the device finished the reply (silence) or a bound cut it
            // (deadline): a global status of a multi-block rack is streamed with pauses inside
            // the frame, and a cut one reaches the server as a checksum mismatch
            log::info!(
                "{} [WATCH] status=change_detected rx_bytes={} end={}",
                log_header,
                exchange.resp_hex.len() / 2,
                end
            );
            match mqtt_client
                .publish(
                    watch_topic.clone(),
                    QoS::AtLeastOnce,
                    false,
                    exchange.to_payload(),
                )
                .await
            {
                // The baseline advances only once the server was actually told:
                // publish can fail immediately (bounded client channel while the
                // connection is down), and advancing it anyway would silently
                // drop this presence change — the next tick must retry it.
                Ok(()) => {
                    last = Some(exchange.resp_hex.clone());
                    last_report = std::time::Instant::now();
                }
                Err(e) => {
                    log::error!("{} [WATCH] status=publish_failed err={:?}", log_header, e);
                }
            }
        }
    });

    if let Some(old) = lock(&RACK_WATCH_TASKS).insert(rack_id.to_string(), handle) {
        old.abort();
    }
}

/// Stops one rack's card presence watch task, if armed.
pub(super) fn stop_rack_watch(rack_id: &str) {
    if let Some(handle) = lock(&RACK_WATCH_TASKS).remove(rack_id) {
        handle.abort();
        log::info!("RACK {} | [WATCH] status=stopped", rack_id);
    }
}

/// Stops every rack's watch task. Shutdown backstop: reaps a watch whose rack
/// entry was already removed elsewhere.
pub(super) fn stop_all_rack_watches() {
    let mut guard = lock(&RACK_WATCH_TASKS);
    for (rack_id, handle) in guard.drain() {
        handle.abort();
        log::info!("RACK {} | [WATCH] status=stopped", rack_id);
    }
}

/// Removes every rack card session matching `pred` and aborts its task.
///
/// The three callers (card moved to a reader, card deleted from the config,
/// rack gone) differ only in which sessions they select and what they log, so
/// the collect-remove-abort sequence lives here once. `reason` fills the reason
/// field of the one log line emitted per aborted session; `warn` picks its
/// level, since a card reappearing in a reader is unexpected enough to warrant
/// WARN while the other two are routine.
///
/// The `RACK_CARD_TASKS` guard is dropped before returning: callers follow up
/// with UI mutations, which take the UI lock, and taking the two in this order
/// is what keeps the rack locks deadlock-free.
fn abort_rack_sessions_where(pred: impl Fn(&RackCardTask) -> bool, reason: &str, warn: bool) {
    let mut tasks = lock(&RACK_CARD_TASKS);
    let matching: Vec<String> = tasks
        .iter()
        .filter(|(_, task)| pred(task))
        .map(|(iccid, _)| iccid.clone())
        .collect();
    for iccid in matching {
        if let Some(task) = tasks.remove(&iccid) {
            task.handle.abort();
            if warn {
                log::warn!(
                    "RACK | [SPAWN] card={} rack={} slot={} status=aborted reason={}",
                    task.card_number,
                    task.rack_id,
                    task.slot,
                    reason
                );
            } else {
                log::info!(
                    "RACK | [SPAWN] card={} rack={} slot={} status=aborted reason={}",
                    task.card_number,
                    task.rack_id,
                    task.slot,
                    reason
                );
            }
        }
    }
}

/// Aborts the live rack-backed session of one card number, if any. Called by
/// the reader path (`mqtt::ensure_connection`) right before it opens its own
/// MQTT connection under that client_id: the card was just physically detected
/// in a PC/SC reader, so it cannot still sit in a rack slot — the rack session
/// is stale (the server's card set dropping it was lost or is still in
/// flight), and
/// letting it live would put two MQTT connections with the same client_id on
/// the broker, which drops them both in a loop until the rack is unplugged.
///
/// The card's rack UI rows are removed as well, exactly as a server card set
/// without the card would do. Keeping a row here would leave a slot the card
/// physically left looking occupied AND make it eligible for
/// `connect_pending_rack_cards` (no live session), which would later resurrect
/// a rack session bound to the empty slot once the reader releases the number.
///
/// Recovery of the row relies on the server, and holds for the normal
/// one-card-per-number world: the card leaving the rack changes the presence
/// watch bytes (or, if the rack link was down, the re-armed watch republishes
/// its first exchange unconditionally), so the server always learns the
/// current rack content and re-publishes the card set when the card is back
/// in a slot. The one case with no recovery signal is a misconfigured setup where
/// two physical cards resolve to the same number: detecting the second card
/// in a reader deletes the first card's row even though its rack content
/// never changed, and that row only comes back on a rack replug. Accepted —
/// the reader detection is strictly newer physical evidence, and a stale row
/// resurrecting a session onto an empty slot is the worse failure.
pub fn abort_rack_card_session(card_number: &str) {
    abort_rack_sessions_where(
        |task| task.card_number == card_number,
        "card_now_in_reader",
        true,
    );
    // Outside the tasks lock (lock-order discipline with the UI paths): drop
    // every rack row carrying this number — including rows whose session was
    // never spawned (skipped as served_by_reader) — and by the card's ICCID
    // from the config, covering a row that is not number-resolved yet.
    let iccid = crate::config::get_card_config_from_cache(card_number).map(|card| card.iccid);
    mutate_rack_rows(|rack_id, list| {
        let before = list.len();
        list.retain(|c| {
            c.card_number.as_deref() != Some(card_number) && (iccid.is_none() || c.iccid != iccid)
        });
        if list.len() == before {
            return false;
        }
        log::info!(
            "RACK {} | [SPAWN] status=row_dropped reason=card_now_in_reader card={}",
            rack_id,
            card_number
        );
        true
    });
}

/// Closes the rack-backed session of one card number, if any rack currently
/// serves it. Called when the card is removed from the config: `TASK_POOL` only
/// holds reader-backed sessions, so without this the rack session would keep
/// running with the removed card's MQTT client_id — leaking the task until the
/// rack is unplugged, and colliding with the server's ident check if the same
/// number is re-added later.
///
/// The card stays physically in its slot, so it is kept in the rack UI with its
/// number cleared: it reappears as an unknown card, ready to be assigned again
/// (which is what `connect_pending_rack_cards` then retries).
pub fn disconnect_rack_card(card_number: &str) {
    // Sessions are keyed by ICCID (stable across config edits), so the lookup
    // is by the number captured at spawn time.
    abort_rack_sessions_where(
        |task| task.card_number == card_number,
        "card_removed_from_config",
        false,
    );

    // Keep the card visible in its rack section, but unassigned — the physical
    // card did not move, only its config entry is gone. The sweep runs even
    // when no session was aborted: a rack row can carry the number without a
    // session (spawn skipped as served_by_reader), and skipping it here would
    // leave the deleted number on display forever.
    mutate_rack_rows(|_rack_id, list| {
        let mut touched = false;
        for card in list.iter_mut() {
            if card.card_number.as_deref() == Some(card_number) {
                card.card_number = None;
                card.name = None;
                touched = true;
            }
        }
        touched
    });
}

/// Aborts the card sessions of one rack and clears its UI card list. Called
/// when that rack disconnects or its MQTT/serial stack is restarted — without
/// the rack there is no transport to those cards.
pub(super) fn stop_rack_cards(rack_id: &str) {
    abort_rack_sessions_where(|task| task.rack_id == rack_id, "rack_gone", false);
    let cleared = {
        let mut ui = lock(&RACK_CARDS_UI);
        ui.remove(rack_id)
            .map(|list| !list.is_empty())
            .unwrap_or(false)
    };
    if cleared {
        rack_update_cards(rack_id, Vec::new());
    }
}

/// Aborts every rack-backed card session and clears the whole UI card list.
/// Shutdown backstop: reaps sessions whose rack entry was already removed.
pub(super) fn stop_all_rack_cards() {
    {
        let mut tasks = lock(&RACK_CARD_TASKS);
        for (_iccid, task) in tasks.drain() {
            task.handle.abort();
            log::info!(
                "RACK | [SPAWN] card={} rack={} status=aborted reason=rack_gone",
                task.card_number,
                task.rack_id
            );
        }
    }
    let rack_ids: Vec<String> = {
        let mut ui = lock(&RACK_CARDS_UI);
        let ids: Vec<String> = ui
            .iter()
            .filter(|(_, list)| !list.is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        ui.clear();
        ids
    };
    for rack_id in rack_ids {
        rack_update_cards(&rack_id, Vec::new());
    }
}

/// Parses the server's card set of a rack (`rack/<id>/cards`): a JSON array of
/// `{"slot":N,"iccid":"<16 hex>"}`. The whole set is rejected on any invalid
/// entry (a slot outside 1..=240, an empty ICCID, a slot or an ICCID listed
/// twice): reconciling the sessions against a half-understood set could close
/// a session that is fine.
fn parse_cards_set(payload: &[u8]) -> Result<Vec<(u16, String)>, String> {
    let json: serde_json::Value =
        serde_json::from_slice(payload).map_err(|e| format!("bad_json: {e}"))?;
    let items = json.as_array().ok_or_else(|| "not_an_array".to_string())?;
    let mut cards: Vec<(u16, String)> = Vec::with_capacity(items.len());
    for item in items {
        let slot = item.get("slot").and_then(|v| v.as_u64()).unwrap_or(0);
        let iccid = item.get("iccid").and_then(|v| v.as_str()).unwrap_or("");
        if iccid.is_empty() || !(1..=240).contains(&slot) {
            return Err(format!("invalid_iccid_or_slot slot={slot}"));
        }
        if cards.iter().any(|(s, _)| u64::from(*s) == slot) {
            return Err(format!("duplicate_slot slot={slot}"));
        }
        if cards.iter().any(|(_, i)| i == iccid) {
            return Err(format!("duplicate_iccid slot={slot}"));
        }
        cards.push((slot as u16, iccid.to_string()));
    }
    Ok(cards)
}

/// The set of cards of one rack the server wants served (`rack/<id>/cards`,
/// published at the end of every discovery chain). The set is the desired
/// state, not a notice: the rack's sessions are reconciled with it — a listed
/// card without a session gets one (ICCID resolved to the card number through
/// the local config; an unknown ICCID is shown in the UI, unserved), a session
/// of this rack whose (slot, ICCID) the set does not list is closed. The key is
/// the pair, not the ICCID alone: the server binds a session to its slot, so a
/// card that moved to another slot gets a new session there.
pub(super) async fn handle_cards_set(
    payload: Vec<u8>,
    rack_id: String,
    serial_port: SharedPort,
    log_header: String,
) {
    let cards = match parse_cards_set(&payload) {
        Ok(cards) => cards,
        Err(reason) => {
            log::warn!(
                "{} [CARDS] status=ignored reason={}",
                log_header,
                reason
            );
            return;
        }
    };
    // a set that follows a `link up` closes a full re-discovery: the sessions it
    // lists re-publish their link reports, every other set leaves them alone
    let rebind = take_rebind(&rack_id);
    log::info!(
        "{} [CARDS] status=set_received count={} rebind={}",
        log_header,
        cards.len(),
        rebind
    );
    let listed = |slot: u16, iccid: &str| cards.iter().any(|(s, i)| *s == slot && i == iccid);

    // Sessions of this rack the set no longer lists: the card left its slot
    // (or moved to another one, where the set lists it anew).
    {
        let mut tasks = lock(&RACK_CARD_TASKS);
        let gone: Vec<String> = tasks
            .iter()
            .filter(|(iccid, task)| task.rack_id == rack_id && !listed(task.slot, iccid))
            .map(|(iccid, _)| iccid.clone())
            .collect();
        for iccid in gone {
            if let Some(task) = tasks.remove(&iccid) {
                task.handle.abort();
                log::info!(
                    "{} [CARDS] card={} slot={} status=aborted reason=not_in_set iccid={}",
                    log_header,
                    task.card_number,
                    task.slot,
                    iccid
                );
            }
        }
    }

    // UI rows of this rack the set no longer lists, same rule. Outside the
    // tasks lock (lock-order discipline with the UI paths).
    let remaining = {
        let mut ui = lock(&RACK_CARDS_UI);
        ui.get_mut(&rack_id).and_then(|list| {
            let before = list.len();
            list.retain(|c| c.iccid.as_deref().is_some_and(|iccid| listed(c.slot, iccid)));
            (list.len() != before).then(|| list.clone())
        })
    };
    if let Some(list) = remaining {
        rack_update_cards(&rack_id, list);
    }

    // Every listed card lands in this rack's UI section, configured or not,
    // and the configured ones get a session unless one already serves them.
    for (slot, iccid) in &cards {
        // one INFO line per card: the rack inventory is readable straight from the trace
        let card_number = crate::config::find_card_number_by_iccid(iccid);
        match &card_number {
            Some(number) => log::info!(
                "{} [CARDS] status=listed slot={} iccid={} card={}",
                log_header,
                slot,
                iccid,
                number
            ),
            None => log::warn!(
                "{} [CARDS] status=not_served reason=unknown_card slot={} iccid={}",
                log_header,
                slot,
                iccid
            ),
        }
        update_rack_card_ui(&rack_id, *slot, iccid, card_number.clone());
        let Some(card_number) = card_number else {
            continue;
        };
        spawn_rack_card_checked(
            card_number,
            iccid.clone(),
            *slot,
            rack_id.clone(),
            serial_port.clone(),
            &log_header,
            // a fresh server set: allowed to relocate a moved card's session
            true,
            rebind,
        )
        .await;
    }

    // The set closes a discovery chain of the server: the UI can stop showing
    // a scan in progress. Treated as a hint, not a contract: the frontend
    // still has its own timeout in case a set never arrives.
    crate::global_app_handle::rack_mark_scan_complete(&rack_id);
}

/// Final spawn step shared by the card set handler and the pending-card retry:
/// a reader-backed session for the same card number wins — never open a second
/// connection with the same client_id (the server treats that as an ident
/// collision). `from_server` — see `spawn_rack_card`.
async fn spawn_rack_card_checked(
    card_number: String,
    iccid: String,
    slot: u16,
    rack_id: String,
    port: SharedPort,
    log_header: &str,
    from_server: bool,
    rebind: bool,
) {
    // INVARIANT: the TASK_POOL guard is held across both the served-by-reader
    // check and the RACK_CARD_TASKS insert inside `spawn_rack_card`. Releasing
    // it in between (the guard used to be a temporary dropped at the end of
    // this `if`) opens the window the reader path exploits: `ensure_connection`
    // holds the same guard while it aborts rack sessions and registers its own
    // entry, so a rack spawn that checked before that abort and inserted after
    // it would leave two live MQTT connections under one client_id — the broker
    // then drops both in a mutual-takeover loop until the card is replugged.
    let pool = TASK_POOL.lock().await;
    if pool.iter().any(|card| card.client_id == card_number) {
        log::warn!(
            "{} [SPAWN] card={} slot={} status=skipped reason=served_by_reader",
            log_header,
            card_number,
            slot
        );
        return;
    }

    spawn_rack_card(card_number, iccid, slot, rack_id, port, from_server, rebind);
    drop(pool);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic ICCIDs only: no real card data in the public repo.
    const A: &str = "0000000000000001";
    const B: &str = "0000000000000002";

    #[test]
    fn cards_set_parses_slots_and_iccids() {
        let payload = format!(r#"[{{"iccid":"{A}","slot":1}},{{"iccid":"{B}","slot":3}}]"#);
        assert_eq!(
            parse_cards_set(payload.as_bytes()).unwrap(),
            vec![(1, A.to_string()), (3, B.to_string())]
        );
    }

    #[test]
    fn cards_set_may_be_empty() {
        // an empty rack, or a discovery that found nothing: every session goes
        assert_eq!(parse_cards_set(b"[]").unwrap(), Vec::<(u16, String)>::new());
    }

    #[test]
    fn cards_set_rejects_malformed_entries_as_a_whole() {
        let bad = [
            "not json".to_string(),
            r#"{"iccid":"x","slot":1}"#.to_string(), // an object, not an array
            format!(r#"[{{"iccid":"{A}","slot":0}}]"#), // slot below the range
            format!(r#"[{{"iccid":"{A}","slot":241}}]"#), // slot above the range
            r#"[{"iccid":"","slot":1}]"#.to_string(),  // empty ICCID
            r#"[{"slot":1}]"#.to_string(),             // no ICCID
            format!(r#"[{{"iccid":"{A}"}}]"#),          // no slot
            format!(r#"[{{"iccid":"{A}","slot":1}},{{"iccid":"{B}","slot":1}}]"#), // slot twice
            format!(r#"[{{"iccid":"{A}","slot":1}},{{"iccid":"{A}","slot":2}}]"#), // ICCID twice
        ];
        for payload in bad {
            assert!(
                parse_cards_set(payload.as_bytes()).is_err(),
                "payload {payload} must be rejected"
            );
        }
    }

    #[test]
    fn link_report_carries_slot_iccid_and_rack() {
        let report: serde_json::Value =
            serde_json::from_str(&link_report(A, 7, "SC1799")).unwrap();
        assert_eq!(report["iccid"], A);
        assert_eq!(report["slot"], 7);
        assert_eq!(report["rack"], "SC1799");
    }
}
