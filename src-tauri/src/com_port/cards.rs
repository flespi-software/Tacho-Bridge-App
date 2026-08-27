//! Rack-backed per-card MQTT sessions.
//!
//! The server discovers cards by ICCID and tells us to open a session per card
//! (`connect`); each session is an MQTT connection under the card's own number,
//! funnelling its envelopes into the serial port of the rack that holds the
//! card. Also hosts the card presence watches, one per connected rack, armed by
//! the server once its discovery has walked that rack.

use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::{AsyncClient, Event, Incoming};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tauri::async_runtime::{self, JoinHandle};

use crate::config::{get_from_cache, split_host_to_parts, CacheSection};
use crate::global_app_handle::{rack_update_cards, RackCard};
use crate::smart_card::TASK_POOL;

use super::rack::{handle_serial_request, IdempotencySlot};
use super::state::{mutate_rack_rows, set_rack_card_state, update_rack_card_ui};
use super::transport::{
    execute_envelope, normalize_hex, SerialEnvelope, SharedPort, SERIAL_MS_MAX, SERIAL_MS_MIN,
    SERIAL_READ_DEADLINE, SERIAL_REPLY_TIMEOUT,
};
use super::{lock, next_reconnect_delay, RACK_TASKS, RECONNECT_DELAY_INITIAL_SECS};

/// One rack-backed card session: the running task plus the identity it was
/// spawned with. `rack_id` is the client_id of the rack whose serial port the
/// session writes to — server messages of one rack must never touch sessions
/// that belong to another.
struct RackCardTask {
    card_number: String,
    slot: u16,
    rack_id: String,
    handle: JoinHandle<()>,
}

lazy_static::lazy_static! {
    /// Rack-backed per-card MQTT tasks, keyed by ICCID (the stable card
    /// identifier — a physical card sits in exactly one slot of one rack).
    /// Keying by the config-resolved card number would leak the session if the
    /// config entry is deleted or edited while the card sits in the rack — the
    /// disconnect lookup would then miss the running task. The (rack, slot) the
    /// session was spawned for is kept so a `connect` for the same slot with a
    /// different ICCID (card swapped without an explicit `disconnect`) evicts
    /// the stale session, and so a `connect` from another rack (card moved
    /// while the old rack's `disconnect` is delayed) replaces the session
    /// instead of being skipped.
    static ref RACK_CARD_TASKS: std::sync::Mutex<HashMap<String, RackCardTask>> =
        std::sync::Mutex::new(HashMap::new());

    /// Cards currently exposed by each rack (RackState.cards), keyed by the
    /// rack's client_id.
    pub(super) static ref RACK_CARDS_UI: std::sync::Mutex<HashMap<String, Vec<RackCard>>> =
        std::sync::Mutex::new(HashMap::new());
}

/// Opens rack-backed sessions for discovered cards that currently have none.
/// Covers two situations the server will not retry on its own (it repeats a
/// `connect` only when the rack content changes):
///  * a card whose ICCID was unknown at discovery time and has since been
///    assigned a number in the UI (config change);
///  * a card whose spawn was skipped because a reader-backed session served
///    the same number (`served_by_reader`) and that reader session has since
///    been torn down (see the hook in `mqtt::shutdown_connections`).
pub async fn connect_pending_rack_cards() {
    // Live racks and their serial ports.
    let ports: Vec<(String, SharedPort)> = {
        let guard = lock(&RACK_TASKS);
        guard
            .iter()
            .map(|(rack_id, (_, port))| (rack_id.clone(), port.clone()))
            .collect()
    };
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
        )
        .await;
    }
}

/// Starts the rack-backed MQTT session of one card, deduplicating by ICCID and
/// by card number (two slots mapped to the same number in the config must not
/// open two MQTT connections with the same client_id).
///
/// `from_server` tells whether this spawn is driven by a fresh server `connect`
/// (the server just read the card's ICCID in that slot — ground truth) or by a
/// local replay of cached UI rows (`connect_pending_rack_cards`). Only the
/// former may relocate a live session to another rack/slot: a replayed row can
/// be stale, and rebinding a working session to it would funnel the card's
/// APDUs into a port where the card no longer sits.
fn spawn_rack_card(
    card_number: String,
    iccid: String,
    slot: u16,
    rack_id: String,
    serial_port: SharedPort,
    from_server: bool,
) {
    // The rack may have been torn down while the caller was awaiting between
    // its dedup check and this spawn (config-change path racing the monitor).
    // A session installed after the teardown would hold the stale serial port
    // and an MQTT client_id forever — verify the port is still the live one.
    let port_is_live = {
        let guard = lock(&RACK_TASKS);
        guard
            .get(&rack_id)
            .map(|(_, live)| Arc::ptr_eq(live, &serial_port))
            .unwrap_or(false)
    };
    if !port_is_live {
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
    // occupant's session is dead weight (its card is gone) — evict it even
    // without a `disconnect`. Scoped to the rack: slot numbers repeat across
    // racks (every rack has a slot 1).
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
    // Same ICCID: a repeat `connect` for the same place is a no-op, but the
    // same card reported from another rack or slot means it physically moved
    // while the old session was still alive — e.g. the old rack's connection
    // was offline, so its `disconnect` is delayed or lost. The newest `connect`
    // is ground truth (the server just read this ICCID in that slot): replace
    // the session so the card is served where it actually is.
    let moved = match tasks.get(&iccid) {
        Some(existing) if !existing.handle.inner().is_finished() => {
            if existing.rack_id == rack_id && existing.slot == slot {
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
    let handle = async_runtime::spawn(rack_card_mqtt_loop(
        card_number.clone(),
        iccid.clone(),
        slot,
        serial_port,
    ));
    tasks.insert(
        iccid,
        RackCardTask {
            card_number,
            slot,
            rack_id,
            handle,
        },
    );
}

/// MQTT loop of one rack-backed card connection. Mirrors the rack's own loop — the same opaque
/// envelope handling funneled into the rack's serial port — plus the one-shot **rack link
/// report** right after CONNACK (topic `rack`, `{"iccid":"...","slot":N}`) that binds this card
/// session to its slot on the server. Without the report the server treats the card as
/// reader-backed and uses the plain PC/SC envelope.
async fn rack_card_mqtt_loop(
    card_number: String,
    iccid: String,
    slot: u16,
    serial_port: SharedPort,
) {
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
                        // Session is up: show the card as served and idle.
                        set_rack_card_state(&iccid, true, false);
                        // rack link report: must be the first publish of the session
                        let report =
                            serde_json::json!({ "iccid": iccid, "slot": slot }).to_string();
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
                        // same guard as the rack's own loop: anything else (a
                        // future control topic, a retained stray) must not be
                        // written raw to the COM port.
                        if !topic.starts_with("request/") {
                            log::warn!(
                                "{} [MQTT] status=ignored reason=unknown_topic topic={}",
                                log_header,
                                topic
                            );
                            continue;
                        }
                        // Activity marking now happens inside, driven by the
                        // envelope's `finish` flag rather than by the mere
                        // arrival of a command.
                        handle_serial_request(
                            &mqtt_client,
                            &topic,
                            &publish.payload,
                            &serial_port,
                            &log_header,
                            &mut idempotency,
                            Some((&iccid, &card_number)),
                        )
                        .await;
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
    /// Card presence watch tasks, one per connected rack, keyed by the rack's
    /// client_id.
    static ref RACK_WATCH_TASKS: std::sync::Mutex<HashMap<String, JoinHandle<()>>> =
        std::sync::Mutex::new(HashMap::new());
}

/// Arms (or re-arms) one rack's card presence watch from a server `watch` instruction:
/// `{"cmd":"<hex>","interval_ms":1000,"idle_ms":...,"deadline_ms":...}`. A background task
/// executes the opaque command every interval through that rack's FIFO port queue and publishes
/// the reply back (topic `watch`, the standard response envelope) ONLY when its bytes change.
/// Re-arming resets the baseline, so the first successful exchange is always published — that
/// is how the server catches updates missed while its discovery chain was busy.
pub(super) fn start_rack_watch(
    payload: &[u8],
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
    let idle = ms("idle_ms")
        .map(Duration::from_millis)
        .unwrap_or(SERIAL_REPLY_TIMEOUT);
    let deadline = ms("deadline_ms")
        .map(Duration::from_millis)
        .unwrap_or(SERIAL_READ_DEADLINE);

    let serial_port = serial_port.clone();
    let mqtt_client = mqtt_client.clone();
    let log_header = log_header.to_string();
    log::info!(
        "{} [WATCH] status=armed interval={:?} cmd_bytes={}",
        log_header,
        interval,
        cmd_hex.len() / 2
    );

    let handle = async_runtime::spawn(async move {
        let mut last: Option<String> = None;
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
            let exchange = execute_envelope(&serial_port, envelope, &log_header, false).await;
            if !exchange.is_ok() {
                // transport errors are already logged by execute_envelope; the rack presence
                // monitor handles a truly gone device, so just keep trying
                continue;
            }
            if last.as_deref() == Some(exchange.resp_hex.as_str()) {
                continue;
            }
            log::info!(
                "{} [WATCH] status=change_detected rx_bytes={}",
                log_header,
                exchange.resp_hex.len() / 2
            );
            match mqtt_client
                .publish("watch", QoS::AtLeastOnce, false, exchange.to_payload())
                .await
            {
                // The baseline advances only once the server was actually told:
                // publish can fail immediately (bounded client channel while the
                // connection is down), and advancing it anyway would silently
                // drop this presence change — the next tick must retry it.
                Ok(()) => last = Some(exchange.resp_hex.clone()),
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

/// `disconnect` message from the server: a card left a slot of the rack whose
/// connection delivered the message, `{"iccid":"...","slot":N}`. Closes the
/// rack-backed card session (if this rack owns it) and removes the card from
/// this rack's section of the UI.
pub(super) fn handle_card_disconnect(payload: &[u8], rack_id: &str, log_header: &str) {
    let json = match serde_json::from_slice::<serde_json::Value>(payload) {
        Ok(json) => json,
        Err(e) => {
            log::warn!(
                "{} [SPAWN] status=ignored reason=bad_json err={}",
                log_header,
                e
            );
            return;
        }
    };
    let iccid = json.get("iccid").and_then(|v| v.as_str()).unwrap_or("");
    let slot = json.get("slot").and_then(|v| v.as_u64()).unwrap_or(0);
    // Same validation as the connect path: an out-of-range slot would wrap in
    // the `as u16` cast below and evict the wrong card from the UI, and an
    // empty ICCID must not silently "match" nothing.
    if iccid.is_empty() || !(1..=240).contains(&slot) {
        log::warn!(
            "{} [SPAWN] status=ignored reason=invalid_iccid_or_slot slot={}",
            log_header,
            slot
        );
        return;
    }
    log::info!(
        "{} [SPAWN] status=card_removed slot={} iccid={}",
        log_header,
        slot,
        iccid
    );

    // Drop the card from this rack's section of the UI. The row must match
    // BOTH slot and ICCID — same guard as the session path below: a late or
    // redelivered `disconnect` for a card that was already replaced in this
    // slot must not delete the new occupant's row.
    let cards = {
        let mut ui = lock(&RACK_CARDS_UI);
        ui.get_mut(rack_id).map(|list| {
            list.retain(|c| !(c.slot == slot as u16 && c.iccid.as_deref() == Some(iccid)));
            list.clone()
        })
    };
    if let Some(cards) = cards {
        rack_update_cards(rack_id, cards);
    }

    // Close the card session, if one was spawned — but only if it belongs to
    // THIS rack. With several racks a `disconnect` can arrive after the card
    // was already re-discovered in another rack (the old rack's connection
    // lagged behind), and must not kill the new session. Lookup is by ICCID
    // captured at spawn time — re-resolving the card number through the config
    // here would leak the task when the entry was deleted or edited mid-session.
    let mut tasks = lock(&RACK_CARD_TASKS);
    let owned_here = tasks
        .get(iccid)
        .map(|task| task.rack_id == rack_id)
        .unwrap_or(false);
    if owned_here {
        if let Some(task) = tasks.remove(iccid) {
            task.handle.abort();
            log::info!(
                "{} [SPAWN] card={} status=aborted reason=card_removed",
                log_header,
                task.card_number
            );
        }
    } else if tasks.contains_key(iccid) {
        log::info!(
            "{} [SPAWN] status=ignored reason=session_belongs_to_another_rack iccid={}",
            log_header,
            iccid
        );
    }
}

/// Aborts the live rack-backed session of one card number, if any. Called by
/// the reader path (`mqtt::ensure_connection`) right before it opens its own
/// MQTT connection under that client_id: the card was just physically detected
/// in a PC/SC reader, so it cannot still sit in a rack slot — the rack session
/// is stale (its server `disconnect` was lost or is still in flight), and
/// letting it live would put two MQTT connections with the same client_id on
/// the broker, which drops them both in a loop until the rack is unplugged.
///
/// The card's rack UI rows are removed as well, exactly as a server
/// `disconnect` would do. Keeping a row here would leave a slot the card
/// physically left looking occupied AND make it eligible for
/// `connect_pending_rack_cards` (no live session), which would later resurrect
/// a rack session bound to the empty slot once the reader releases the number.
///
/// Recovery of the row relies on the server, and holds for the normal
/// one-card-per-number world: the card leaving the rack changes the presence
/// watch bytes (or, if the rack link was down, the re-armed watch republishes
/// its first exchange unconditionally), so the server always learns the
/// current rack content and re-sends `connect` when the card is back in a
/// slot. The one case with no recovery signal is a misconfigured setup where
/// two physical cards resolve to the same number: detecting the second card
/// in a reader deletes the first card's row even though its rack content
/// never changed, and that row only comes back on a rack replug. Accepted —
/// the reader detection is strictly newer physical evidence, and a stale row
/// resurrecting a session onto an empty slot is the worse failure.
pub fn abort_rack_card_session(card_number: &str) {
    {
        let mut tasks = lock(&RACK_CARD_TASKS);
        let matching: Vec<String> = tasks
            .iter()
            .filter(|(_, task)| task.card_number == card_number)
            .map(|(iccid, _)| iccid.clone())
            .collect();
        for iccid in matching {
            if let Some(task) = tasks.remove(&iccid) {
                task.handle.abort();
                log::warn!(
                    "RACK | [SPAWN] card={} rack={} slot={} status=aborted reason=card_now_in_reader",
                    task.card_number,
                    task.rack_id,
                    task.slot
                );
            }
        }
    }
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
    {
        let mut tasks = lock(&RACK_CARD_TASKS);
        let matching: Vec<String> = tasks
            .iter()
            .filter(|(_, task)| task.card_number == card_number)
            .map(|(iccid, _)| iccid.clone())
            .collect();
        for iccid in &matching {
            if let Some(task) = tasks.remove(iccid) {
                task.handle.abort();
                log::info!(
                    "RACK | [SPAWN] card={} rack={} slot={} status=aborted reason=card_removed_from_config",
                    task.card_number,
                    task.rack_id,
                    task.slot
                );
            }
        }
    }

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
    {
        let mut tasks = lock(&RACK_CARD_TASKS);
        let matching: Vec<String> = tasks
            .iter()
            .filter(|(_, task)| task.rack_id == rack_id)
            .map(|(iccid, _)| iccid.clone())
            .collect();
        for iccid in matching {
            if let Some(task) = tasks.remove(&iccid) {
                task.handle.abort();
                log::info!(
                    "RACK | [SPAWN] card={} rack={} status=aborted reason=rack_gone",
                    task.card_number,
                    rack_id
                );
            }
        }
    }
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

pub(super) async fn handle_connect_spawn(
    payload: &[u8],
    rack_id: &str,
    serial_port: &SharedPort,
    log_header: &str,
) {
    let json = match serde_json::from_slice::<serde_json::Value>(payload) {
        Ok(json) => json,
        Err(e) => {
            log::warn!(
                "{} [SPAWN] status=ignored reason=bad_json err={}",
                log_header,
                e
            );
            return;
        }
    };
    let iccid = json.get("iccid").and_then(|v| v.as_str()).unwrap_or("");
    let slot = json.get("slot").and_then(|v| v.as_u64()).unwrap_or(0);
    if iccid.is_empty() || !(1..=240).contains(&slot) {
        log::warn!(
            "{} [SPAWN] status=ignored reason=invalid_iccid_or_slot slot={}",
            log_header,
            slot
        );
        return;
    }

    // one INFO line per discovered card: the rack inventory is readable straight from the trace
    let card_number = crate::config::find_card_number_by_iccid(iccid);
    match &card_number {
        Some(number) => log::info!(
            "{} [SPAWN] status=discovered slot={} iccid={} card={}",
            log_header,
            slot,
            iccid,
            number
        ),
        None => log::warn!(
            "{} [SPAWN] status=not_spawned reason=unknown_card slot={} iccid={}",
            log_header,
            slot,
            iccid
        ),
    }

    // every reported card lands in this rack's state (rack section of the UI), configured or
    // not: an unknown card is shown there with its ICCID, ready to be assigned a number
    update_rack_card_ui(rack_id, slot as u16, iccid, card_number.clone());

    let Some(card_number) = card_number else {
        return;
    };

    spawn_rack_card_checked(
        card_number,
        iccid.to_string(),
        slot as u16,
        rack_id.to_string(),
        serial_port.clone(),
        log_header,
        // a fresh server `connect`: allowed to relocate a moved card's session
        true,
    )
    .await;
}

/// Final spawn step shared by the server `connect` handler and the pending-card
/// retry: a reader-backed session for the same card number wins — never open a
/// second connection with the same client_id (the server treats that as an
/// ident collision). `from_server` — see `spawn_rack_card`.
async fn spawn_rack_card_checked(
    card_number: String,
    iccid: String,
    slot: u16,
    rack_id: String,
    port: SharedPort,
    log_header: &str,
    from_server: bool,
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

    spawn_rack_card(card_number, iccid, slot, rack_id, port, from_server);
    drop(pool);
}
