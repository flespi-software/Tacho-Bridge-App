//! Rack-backed per-card MQTT sessions.
//!
//! The server discovers cards by ICCID and tells us to open a session per card
//! (`connect`); each session is an MQTT connection under the card's own number,
//! funnelling its envelopes into the shared serial port. Also hosts the card
//! presence watch, which the server arms once discovery has walked the rack.

use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::{AsyncClient, Event, Incoming, MqttOptions};
use std::sync::Arc;
use std::time::Duration;
use tauri::async_runtime::{self, JoinHandle};

use crate::config::{get_from_cache, split_host_to_parts, CacheSection};
use crate::global_app_handle::{rack_update_cards, RackCard};
use crate::smart_card::TASK_POOL;

use super::rack::{handle_serial_request, IdempotencySlot};
use super::state::{set_rack_card_state, update_rack_card_ui};
use super::transport::{
    execute_envelope, normalize_hex, SerialEnvelope, SharedPort, SERIAL_MS_MAX, SERIAL_MS_MIN,
    SERIAL_READ_DEADLINE, SERIAL_REPLY_TIMEOUT,
};
use super::{next_reconnect_delay, RACK_MQTT_TASK, RECONNECT_DELAY_INITIAL_SECS};

type RackCardTask = (String, u16, JoinHandle<()>);

lazy_static::lazy_static! {
    /// Rack-backed per-card MQTT tasks, keyed by ICCID (the rack's stable card
    /// identifier) with the spawn-time card number and slot kept alongside.
    /// Keying by the config-resolved card number would leak the session if the
    /// config entry is deleted or edited while the card sits in the rack — the
    /// disconnect lookup would then miss the running task. The slot is kept so
    /// a `connect` for the same slot with a different ICCID (card swapped
    /// without an explicit `disconnect`) evicts the stale session instead of
    /// leaking it. All aborted when the rack disconnects — without the rack
    /// there is no transport to those cards.
    static ref RACK_CARD_TASKS: std::sync::Mutex<std::collections::HashMap<String, RackCardTask>> =
        std::sync::Mutex::new(std::collections::HashMap::new());

    /// Cards currently exposed by the rack, as shown in the UI (RackState.cards).
    pub(super) static ref RACK_CARDS_UI: std::sync::Mutex<Vec<RackCard>> = std::sync::Mutex::new(Vec::new());
}

/// Opens rack-backed sessions for discovered cards that became resolvable after
/// a config change: the server's `connect` for a card with an unknown ICCID is
/// skipped at discovery time, and the server does not repeat it until the rack
/// content changes — so assigning the number in the UI must retry it locally.
pub async fn connect_pending_rack_cards() {
    let port = {
        let guard = match RACK_MQTT_TASK.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.as_ref().map(|(_, port)| port.clone())
    };
    let Some(port) = port else {
        return; // no rack connected
    };

    let pending: Vec<(u16, String)> = {
        let ui = match RACK_CARDS_UI.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        ui.iter()
            .filter(|card| card.card_number.is_none())
            .filter_map(|card| card.iccid.clone().map(|iccid| (card.slot, iccid)))
            .collect()
    };

    for (slot, iccid) in pending {
        let Some(card_number) = crate::config::find_card_number_by_iccid(&iccid) else {
            continue; // still unassigned
        };
        log::info!(
            "RACK | [SPAWN] status=pending_card_resolved slot={} iccid={} card={}",
            slot,
            iccid,
            card_number
        );
        update_rack_card_ui(slot, &iccid, Some(card_number.clone()));
        spawn_rack_card_checked(card_number, iccid, slot, port.clone(), "RACK |").await;
    }
}

/// Starts the rack-backed MQTT session of one card, deduplicating by ICCID and
/// by card number (two slots mapped to the same number in the config must not
/// open two MQTT connections with the same client_id).
fn spawn_rack_card(card_number: String, iccid: String, slot: u16, serial_port: SharedPort) {
    // The rack may have been torn down while the caller was awaiting between
    // its dedup check and this spawn (config-change path racing the monitor).
    // A session installed after the teardown would hold the stale serial port
    // and an MQTT client_id forever — verify the port is still the live one.
    let port_is_live = {
        let guard = match RACK_MQTT_TASK.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .as_ref()
            .map(|(_, live)| Arc::ptr_eq(live, &serial_port))
            .unwrap_or(false)
    };
    if !port_is_live {
        log::warn!(
            "RACK | [SPAWN] card={} slot={} status=skipped reason=rack_gone",
            card_number,
            slot
        );
        return;
    }

    let mut tasks = match RACK_CARD_TASKS.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    // A different card now occupies this slot: the previous occupant's session
    // is dead weight (its card is gone) — evict it even without a `disconnect`.
    let stale: Vec<String> = tasks
        .iter()
        .filter(|(other_iccid, (_, other_slot, _))| **other_iccid != iccid && *other_slot == slot)
        .map(|(other_iccid, _)| other_iccid.clone())
        .collect();
    for old_iccid in stale {
        if let Some((old_number, _, handle)) = tasks.remove(&old_iccid) {
            handle.abort();
            log::info!(
                "RACK | [SPAWN] card={} slot={} status=aborted reason=slot_reassigned",
                old_number,
                slot
            );
        }
    }
    if let Some((_, _, handle)) = tasks.get(&iccid) {
        if !handle.inner().is_finished() {
            log::debug!(
                "RACK | [SPAWN] card={} status=skipped reason=already_running",
                card_number
            );
            return;
        }
    }
    if tasks.iter().any(|(other_iccid, (number, _, handle))| {
        *other_iccid != iccid && *number == card_number && !handle.inner().is_finished()
    }) {
        log::warn!(
            "RACK | [SPAWN] card={} slot={} status=skipped reason=served_by_another_slot",
            card_number,
            slot
        );
        return;
    }
    log::info!(
        "RACK | [SPAWN] card={} slot={} status=starting_session",
        card_number,
        slot
    );
    let handle = async_runtime::spawn(rack_card_mqtt_loop(
        card_number.clone(),
        iccid.clone(),
        slot,
        serial_port,
    ));
    tasks.insert(iccid, (card_number, slot, handle));
}

/// MQTT loop of one rack-backed card connection. Mirrors the rack's own loop — the same opaque
/// envelope handling funneled into the shared serial port — plus the one-shot **rack link
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

    let mut mqtt_options = MqttOptions::new(&card_number, &host, port);
    mqtt_options.set_keep_alive(Duration::from_secs(120));
    log::info!(
        "{} [MQTT] phase=connect_attempt status=initialized host={}:{} slot={}",
        log_header,
        host,
        port,
        slot
    );

    let (mqtt_client, mut eventloop) = AsyncClient::new(mqtt_options, 10);

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
                            Some(&iccid),
                        )
                        .await;
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
    /// The card presence watch task of the current rack connection, if armed.
    static ref RACK_WATCH_TASK: std::sync::Mutex<Option<JoinHandle<()>>> =
        std::sync::Mutex::new(None);
}

/// Arms (or re-arms) the card presence watch from a server `watch` instruction:
/// `{"cmd":"<hex>","interval_ms":1000,"idle_ms":...,"deadline_ms":...}`. A background task
/// executes the opaque command every interval through the same FIFO port queue and publishes
/// the reply back (topic `watch`, the standard response envelope) ONLY when its bytes change.
/// Re-arming resets the baseline, so the first successful exchange is always published — that
/// is how the server catches updates missed while its discovery chain was busy.
pub(super) fn start_rack_watch(
    payload: &[u8],
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

    let mut guard = match RACK_WATCH_TASK.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(old) = guard.replace(handle) {
        old.abort();
    }
}

/// Stops the card presence watch task, if armed.
pub(super) fn stop_rack_watch() {
    let mut guard = match RACK_WATCH_TASK.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(handle) = guard.take() {
        handle.abort();
        log::info!("RACK | [WATCH] status=stopped");
    }
}

/// `disconnect` message from the server: a card left its rack slot,
/// `{"iccid":"...","slot":N}`. Closes the rack-backed card session (if one was spawned) and
/// removes the card from the rack section of the UI.
pub(super) fn handle_card_disconnect(payload: &[u8], log_header: &str) {
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

    // drop the card from the rack section of the UI
    let cards = {
        let mut ui = match RACK_CARDS_UI.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        ui.retain(|c| c.slot != slot as u16);
        ui.clone()
    };
    rack_update_cards(cards);

    // Close the card session, if one was spawned. Lookup is by ICCID captured at
    // spawn time — re-resolving the card number through the config here would
    // leak the task when the entry was deleted or edited mid-session.
    let mut tasks = match RACK_CARD_TASKS.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some((card_number, _slot, handle)) = tasks.remove(iccid) {
        handle.abort();
        log::info!(
            "{} [SPAWN] card={} status=aborted reason=card_removed",
            log_header,
            card_number
        );
    }
}

/// Closes the rack-backed session of one card number, if the rack currently
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
    let aborted: Vec<String> = {
        let mut tasks = match RACK_CARD_TASKS.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let matching: Vec<String> = tasks
            .iter()
            .filter(|(_, (number, _, _))| number == card_number)
            .map(|(iccid, _)| iccid.clone())
            .collect();
        for iccid in &matching {
            if let Some((number, slot, handle)) = tasks.remove(iccid) {
                handle.abort();
                log::info!(
                    "RACK | [SPAWN] card={} slot={} status=aborted reason=card_removed_from_config",
                    number,
                    slot
                );
            }
        }
        matching
    };

    if aborted.is_empty() {
        return;
    }

    // Keep the card visible in the rack section, but unassigned — the physical
    // card did not move, only its config entry is gone.
    let cards = {
        let mut ui = match RACK_CARDS_UI.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        for card in ui.iter_mut() {
            if card.card_number.as_deref() == Some(card_number) {
                card.card_number = None;
                card.name = None;
            }
        }
        ui.clone()
    };
    rack_update_cards(cards);
}

/// Aborts all rack-backed card sessions and clears the UI card list. Called when the rack
/// disconnects or its MQTT/serial stack is restarted — without the rack there is no transport.
pub(super) fn stop_rack_cards() {
    let mut tasks = match RACK_CARD_TASKS.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    for (_iccid, (card_number, _slot, handle)) in tasks.drain() {
        handle.abort();
        log::info!(
            "RACK | [SPAWN] card={} status=aborted reason=rack_gone",
            card_number
        );
    }
    let mut ui = match RACK_CARDS_UI.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if !ui.is_empty() {
        ui.clear();
        rack_update_cards(Vec::new());
    }
}

pub(super) async fn handle_connect_spawn(
    payload: &[u8],
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

    // every reported card lands in the rack state (rack section of the UI), configured or
    // not: an unknown card is shown there with its ICCID, ready to be assigned a number
    update_rack_card_ui(slot as u16, iccid, card_number.clone());

    let Some(card_number) = card_number else {
        return;
    };

    spawn_rack_card_checked(
        card_number,
        iccid.to_string(),
        slot as u16,
        serial_port.clone(),
        log_header,
    )
    .await;
}

/// Final spawn step shared by the server `connect` handler and the pending-card
/// retry: a reader-backed session for the same card number wins — never open a
/// second connection with the same client_id (the server treats that as an
/// ident collision).
async fn spawn_rack_card_checked(
    card_number: String,
    iccid: String,
    slot: u16,
    port: SharedPort,
    log_header: &str,
) {
    if TASK_POOL
        .lock()
        .await
        .iter()
        .any(|card| card.client_id == card_number)
    {
        log::warn!(
            "{} [SPAWN] card={} slot={} status=skipped reason=served_by_reader",
            log_header,
            card_number,
            slot
        );
        return;
    }

    spawn_rack_card(card_number, iccid, slot, port);
}
