//! Rack traffic of the application connection.
//!
//! The rack has no MQTT connection of its own: it is a peripheral of the
//! application, and everything the server exchanges with it rides the app
//! connection under the `rack/<id>/` topic prefix, `<id>` being the rack id
//! (the device serial, see `discovery.rs`). This module owns that prefix:
//! the link reports (`rack/<id>/link`), the dispatch of the server's rack
//! publishes (`request/<n>`, `watch`, `cards`) and the reply path of the
//! serial exchanges. Per-card sessions live in `cards.rs`; the wire is in
//! `transport.rs`.
//!
//! Topics, TBA -> server: `rack/<id>/link` (`{"state":"up"|"down"}`),
//! `rack/<id>/response/<n>` (reply to `request/<n>`), `rack/<id>/watch`
//! (presence watch report). Server -> TBA: `rack/<id>/request/<n>` (one serial
//! exchange), `rack/<id>/watch` (arm the presence watch), `rack/<id>/cards`
//! (the set of cards to serve). No `/<sender>` tail on this path: the sender
//! is always the server.

use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::AsyncClient;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tauri::async_runtime;

use crate::mqtt::{request_id_from_topic, request_to_response_topic};

use super::cards::{handle_cards_set, start_rack_watch, stop_all_rack_watches};
use super::state::set_rack_card_state;
use super::transport::{execute_envelope, parse_envelope, SerialExchange, SharedPort};
use super::{
    linked_rack, linked_rack_ids, lock, rack_port_is_live, reset_all_rack_idempotency,
    LinkedRack,
};

/// Topic prefix of every rack publish on the application connection; the rack
/// id and a `/` follow it.
pub(super) const RACK_TOPIC_PREFIX: &str = "rack/";

/// MQTT client of the current application connection: the one every rack
/// publish of TBA goes through. Replaced together with the connection (see
/// `app_connect`), so a rack never publishes into a client whose event loop is
/// gone.
static APP_CLIENT: std::sync::Mutex<Option<AsyncClient>> = std::sync::Mutex::new(None);

/// Registers the MQTT client of the application connection as the carrier of
/// the rack traffic. Called when the app connection is (re)created.
pub fn register_app_client(client: &AsyncClient) {
    *lock(&APP_CLIENT) = Some(client.clone());
}

fn app_client() -> Option<AsyncClient> {
    lock(&APP_CLIENT).clone()
}

/// Generation of the application connection: 0 before its first CONNACK,
/// incremented on every CONNACK. The server runs a fresh instance per
/// connection, so a rack reply belongs to the generation its request came in
/// with: a reply that outlives it would reach the next server instance, which
/// never sent that request (and counts its ids from 1 again).
static APP_GENERATION: AtomicU64 = AtomicU64::new(0);

/// True between a CONNACK of the application connection and its next event
/// loop failure. While it is false the server has no instance to hear a rack
/// publish, so none is made: the CONNACK that ends the outage announces every
/// linked rack anyway, and a publish queued during the outage would only
/// reach the new instance out of order or twice.
static APP_ONLINE: AtomicBool = AtomicBool::new(false);

/// The current generation of the application connection, captured when a
/// server publish is dispatched.
pub(super) fn app_generation() -> u64 {
    APP_GENERATION.load(Ordering::SeqCst)
}

/// True while the application connection is online and still the generation
/// a publish was dispatched in: the server instance that sent the request is
/// the one that will hear the reply.
pub(super) fn app_link_is(generation: u64) -> bool {
    APP_ONLINE.load(Ordering::SeqCst) && APP_GENERATION.load(Ordering::SeqCst) == generation
}

/// Racks whose next card set follows a `link up`, i.e. a full re-discovery of
/// the server. Only such a set makes the live card sessions re-publish their
/// rack link reports: the report is what re-binds a slot on a server instance
/// that has just (re)built its card map, and it costs one serial LED frame per
/// card. Doing it for EVERY card set is what buried a 100-card rack under
/// hundreds of LED frames a minute, starving the discovery and the tracker
/// exchanges queued behind them on the same port.
static RACK_REBIND: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
    std::sync::Mutex::new(None);

/// Marks the rack as re-discovered: its next card set re-publishes the link
/// reports of the sessions it lists.
fn mark_rebind(rack_id: &str) {
    lock(&RACK_REBIND)
        .get_or_insert_with(std::collections::HashSet::new)
        .insert(rack_id.to_string());
}

/// Takes the re-discovery mark of a rack, clearing it: the card set being
/// handled is the one that follows its `link up`.
pub(super) fn take_rebind(rack_id: &str) -> bool {
    lock(&RACK_REBIND)
        .as_mut()
        .is_some_and(|marked| marked.remove(rack_id))
}

/// The application connection lost its server: rack publishes stop until the
/// next CONNACK. Called on every failed poll of the app event loop.
pub fn on_app_offline() {
    super::access::cancel_discovery();
    if APP_ONLINE.swap(false, Ordering::SeqCst) {
        log::info!("RACK | [LINK] status=app_offline");
    }
}

/// The topic of one rack publish: prefix, rack id, tail.
pub(super) fn rack_topic(rack_id: &str, tail: &str) -> String {
    format!("{RACK_TOPIC_PREFIX}{rack_id}/{tail}")
}

/// Splits a rack topic into its rack id and tail (`rack/SC1799/request/5` ->
/// `SC1799`, `request/5`). `None` for anything else, including a rack topic
/// with an empty id or tail.
pub(super) fn parse_rack_topic(topic: &str) -> Option<(&str, &str)> {
    let rest = topic.strip_prefix(RACK_TOPIC_PREFIX)?;
    let (rack_id, tail) = rest.split_once('/')?;
    if rack_id.is_empty() || tail.is_empty() {
        return None;
    }
    Some((rack_id, tail))
}

/// Publishes the link state of the given racks to the server, in order, from
/// one task: a `down` followed by an `up` of the same rack (a port reopened)
/// must reach the server in that order, and separately spawned publishes give
/// no such guarantee. Nothing is published while the application connection
/// is offline (or does not exist yet): the CONNACK that brings it online
/// announces every linked rack anyway, and a report queued before it would
/// make the server hear the same `up` twice, running its discovery twice.
pub(super) fn announce_links(links: Vec<(String, bool)>) {
    if links.is_empty() {
        return;
    }
    let client = match app_client() {
        Some(client) if APP_ONLINE.load(Ordering::SeqCst) => client,
        _ => {
            log::debug!(
                "RACK | [LINK] status=deferred reason=app_offline racks={}",
                links.len()
            );
            return;
        }
    };
    async_runtime::spawn(async move {
        for (rack_id, up) in links {
            let state = if up { "up" } else { "down" };
            let payload = if up {
                serde_json::json!({"state": state, "guarded_discovery": true,
                    "cards": super::cards::inventory(&rack_id)})
            } else {
                serde_json::json!({"state": state})
            }
            .to_string();
            match client
                .publish(rack_topic(&rack_id, "link"), QoS::AtLeastOnce, false, payload)
                .await
            {
                Ok(()) => {
                    // the server rebuilds its card map from the discovery this starts
                    if up {
                        mark_rebind(&rack_id);
                    }
                    log::info!("RACK {} | [LINK] status=published state={}", rack_id, state)
                }
                Err(e) => log::warn!(
                    "RACK {} | [LINK] status=publish_failed state={} err={:?}",
                    rack_id,
                    state,
                    e
                ),
            }
        }
    });
}

/// CONNACK of the application connection: the server runs a fresh instance
/// per connection and knows nothing about the racks, so every linked rack is
/// announced again and its discovery restarts from the server side. The
/// generation advances first, so a reply still in flight for the previous
/// instance is dropped instead of delivered to this one. The old presence
/// watches are stopped (the server re-arms them once its discovery is done; a
/// report of the old baseline would only be noise), and the reply cache is
/// dropped (the server's request id counters restart at 1).
pub fn on_app_connack(client: &AsyncClient) {
    register_app_client(client);
    APP_GENERATION.fetch_add(1, Ordering::SeqCst);
    APP_ONLINE.store(true, Ordering::SeqCst);
    stop_all_rack_watches();
    super::access::cancel_discovery();
    reset_all_rack_idempotency();
    let ids = linked_rack_ids();
    if ids.is_empty() {
        return;
    }
    log::info!("RACK | [LINK] phase=connack racks={}", ids.len());
    announce_links(ids.into_iter().map(|id| (id, true)).collect());
}

/// Routes one publish of the application connection when it belongs to a
/// rack (`rack/<id>/...`). Returns `false` for every other topic, which the
/// caller handles as before. The rack publishes are handled off the app event
/// loop: a serial exchange takes up to seconds, and the loop must keep
/// polling so the other racks and the app-level commands are not stalled
/// behind it.
pub fn handle_app_publish(client: &AsyncClient, topic: &str, payload: &[u8]) -> bool {
    if !topic.starts_with(RACK_TOPIC_PREFIX) {
        return false;
    }
    let Some((rack_id, tail)) = parse_rack_topic(topic) else {
        log::warn!(
            "RACK | [MQTT] status=ignored reason=malformed_rack_topic topic={}",
            topic
        );
        return true;
    };
    let log_header = format!("RACK {} |", rack_id);
    log::info!(
        "{} [MQTT] event=command topic={} bytes={}",
        log_header,
        topic,
        payload.len()
    );
    // Full command text only at debug: the rack protocol must not end up in
    // users' log files at INFO level.
    log::debug!(
        "{} [MQTT] command_text={}",
        log_header,
        String::from_utf8_lossy(payload)
    );
    // A rack the server addresses is one we announced with `link up`; one that
    // is not linked any more went away in between (its `link down` is on the
    // way or already delivered), and nothing may be executed or answered for
    // it — the port is gone with it.
    let Some(rack) = linked_rack(rack_id) else {
        log::warn!(
            "{} [MQTT] status=ignored reason=rack_not_linked topic={}",
            log_header,
            topic
        );
        return true;
    };

    // The server instance behind this publish: a reply is delivered to it or
    // not at all (see `app_link_is`).
    let generation = app_generation();
    if tail.starts_with("request/") {
        async_runtime::spawn(handle_rack_request(
            client.clone(),
            generation,
            rack_id.to_string(),
            tail.to_string(),
            payload.to_vec(),
            rack,
            log_header,
        ));
    } else if tail == "release" {
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(payload) {
            if let Some(slot) = json
                .get("slot")
                .and_then(|v| v.as_u64())
                .filter(|s| (1..=240).contains(s)) {
                if let Some(request_id) = json.get("request_id").and_then(|v| v.as_u64()) {
                    super::access::release(&rack.port, slot as u16, true, Some(request_id));
                }
            }
        }
    } else if tail == "watch" {
        // arm/re-arm this rack's card presence watch with the server-supplied
        // bytes; its reports go back on the same rack topic
        start_rack_watch(payload, generation, rack_id, &rack.port, client, &log_header);
    } else if tail == "cards" {
        // the set of cards of this rack to serve: reconcile the card sessions
        async_runtime::spawn(handle_cards_set(
            payload.to_vec(),
            rack_id.to_string(),
            rack.port.clone(),
            log_header,
        ));
    } else {
        // An unknown rack topic (a newer server feature, or a retained stray)
        // must not fall through to the serial path: its payload would be
        // written raw to the COM port. Log and drop.
        log::warn!(
            "{} [MQTT] status=ignored reason=unknown_topic topic={}",
            log_header,
            topic
        );
    }
    true
}

/// One `rack/<id>/request/<n>` of the server: the serial exchange on that
/// rack's port and its `rack/<id>/response/<n>`. The rack's idempotency slot
/// is held for the whole exchange, which also serialises the exchanges of one
/// rack (the server sends them one at a time anyway).
async fn handle_rack_request(
    client: AsyncClient,
    generation: u64,
    rack_id: String,
    tail: String,
    payload: Vec<u8>,
    rack: LinkedRack,
    log_header: String,
) {
    let mut slot = rack.idempotency.lock().await;
    if !app_link_is(generation) || !rack_port_is_live(&rack_id, &rack.port) {
        return;
    }
    let Some((resp_tail, resp_payload)) = run_serial_request(
        &tail,
        &payload,
        &rack.port,
        &log_header,
        &mut *slot,
        // the rack's own exchanges serve no single card
        None,
    )
    .await
    else {
        return;
    };
    drop(slot);
    // The link may have gone down while the exchange ran: the server must not
    // hear a result of a life of the rack it was told is over.
    if !rack_port_is_live(&rack_id, &rack.port) {
        log::warn!(
            "{} [MQTT] status=reply_dropped reason=link_down topic={}",
            log_header,
            tail
        );
        return;
    }
    // The application connection went down or was re-established while the
    // exchange ran: the server instance that asked is gone, and the one that
    // replaced it never sent this request.
    if !app_link_is(generation) {
        log::warn!(
            "{} [MQTT] status=reply_dropped reason=app_connection_changed topic={}",
            log_header,
            tail
        );
        return;
    }
    publish_reply(
        &client,
        rack_topic(&rack_id, &resp_tail),
        resp_payload,
        &log_header,
    )
    .await;
}

/// Publishes the reply of a serial exchange. The app is the server's only
/// feedback channel, so a failure to publish is an error, not a debug line.
pub(super) async fn publish_reply(
    client: &AsyncClient,
    topic: String,
    payload: String,
    log_header: &str,
) {
    if let Err(e) = client.publish(topic, QoS::AtLeastOnce, false, payload).await {
        log::error!(
            "{} [MQTT] status=reply_publish_failed err={:?}",
            log_header,
            e
        );
    }
}

/// Per-connection idempotency slot: the last request id answered and the reply
/// sent for it. The server re-sends a request with the same id when it does not
/// get a timely response; kept together so it can be threaded through as one
/// argument and reset as one unit on CONNACK.
#[derive(Default)]
pub(super) struct IdempotencySlot {
    pub(super) last_request_id: Option<u64>,
    pub(super) last_response_payload: Option<String>,
}

impl IdempotencySlot {
    /// Forgets the cached reply — called on a new MQTT session, where the
    /// server-side request_id counter restarts at 1.
    pub(super) fn reset(&mut self) {
        self.last_request_id = None;
        self.last_response_payload = None;
    }
}

/// Handles one `request/<n>...` publish addressed to a rack port — a rack's own
/// exchange (`rack/<id>/request/<n>`, `topic` being the tail after the rack
/// prefix) or one of a rack-backed card session (`request/<n>/<client_id>`):
/// idempotency, envelope parsing and serial execution. Returns the reply as
/// `(response topic, payload)` for the caller to publish — relative to the
/// same prefix `topic` was — or `None` when nothing is to be sent (a duplicate
/// of a request still in flight, or a publish that is no envelope at all).
/// The app is the server's only feedback channel, so a silent rack is
/// reported as an error reply, not swallowed.
pub(super) async fn run_serial_request(
    topic: &str,
    payload: &[u8],
    serial_port: &SharedPort,
    log_header: &str,
    idempotency: &mut IdempotencySlot,
    // `(iccid, card_number, slot)` of the card this connection serves, when the
    // caller is a card session. `None` for a rack's own exchange, which has
    // no single card and therefore no authentication state to track.
    card: Option<(&str, &str, u16)>,
) -> Option<(String, String)> {
    // A server-driven rack exchange counts as card activity: the auto-updater
    // must not restart the app in the middle of a rack card operation. The
    // 1 Hz presence watch does NOT go through here, so idle racks stay quiet.
    crate::mqtt::touch_card_activity();
    // Idempotency: the server re-sends a request with the same id after a timeout. If we already
    // answered this id, re-send the cached response without touching the port again; if it is
    // still in flight, drop the duplicate.
    let req_id = request_id_from_topic(topic);
    let resp_topic = request_to_response_topic(topic);
    if req_id.is_some() && req_id == idempotency.last_request_id {
        return match &idempotency.last_response_payload {
            Some(cached) => {
                log::warn!(
                    "{} [MQTT] status=duplicate_request request_id={:?} action=resend_cached",
                    log_header,
                    req_id
                );
                Some((resp_topic, cached.clone()))
            }
            None => {
                log::warn!(
                    "{} [MQTT] status=duplicate_request request_id={:?} action=ignored reason=in_flight",
                    log_header,
                    req_id
                );
                None
            }
        };
    }

    let json = match serde_json::from_slice::<serde_json::Value>(payload) {
        Ok(json) => json,
        Err(e) => {
            log::warn!(
                "{} [MQTT] status=ignored reason=bad_json err={}",
                log_header,
                e
            );
            return None;
        }
    };
    let Some(parsed) = parse_envelope(&json) else {
        log::warn!("{} [MQTT] status=ignored reason=no_serial_cmd", log_header);
        return None;
    };

    let exchange = match parsed {
        Ok(envelope) => {
            // Authentication boundaries come from the server's `finish` flag —
            // the same contract the PC/SC path uses (see `auth_process` in
            // mqtt.rs). Nothing is inferred from the traffic itself: the pause
            // while the tracker thinks after the ATR read is indistinguishable
            // from the end of a session, so guessing got it wrong either way.
            //
            // Only session exchanges carry the flag at all (the server always
            // sets it, true or false). An envelope without it is plain
            // signalling on the same serial path — e.g. a slot LED repaint —
            // executed and answered like any exchange, but never shown or
            // recorded as authentication activity.
            //
            // Every other way a session can end already clears the state at its
            // own site — connection lost, card pulled from the slot, rack
            // unplugged. The one gap — the closing `finish:true` simply never
            // arriving (tracker abort or lost message) — is covered by the
            // keep-alive PingResp reset in the card's MQTT loop (see cards.rs),
            // so no dedicated timeout is needed here.
            let discovery_slot = json
                .get("discovery_slot")
                .and_then(|v| v.as_u64())
                .filter(|s| (1..=240).contains(s))
                .map(|s| s as u16);
            let lease = if card.is_none() {
                discovery_slot.map(|slot| (slot, true))
            } else if envelope.finish == Some(false) {
                card.map(|(_, _, slot)| (slot, false))
            } else {
                None
            };
            if let Some((slot, discovery)) = lease {
                if !super::access::acquire(serial_port, slot, discovery, req_id.unwrap_or(0))
                    .await
                {
                    return Some((
                        resp_topic,
                        SerialExchange::error("card_busy").to_payload(),
                    ));
                }
            }
            if let Some((iccid, card_number, _)) = card {
                match envelope.finish {
                    Some(true) => {
                        set_rack_card_state(iccid, true, false);
                        // Same bookkeeping as the reader path (mqtt.rs): persist
                        // the auth timestamp so a card that only ever authenticates
                        // through a rack still shows "Last auth" in the UI.
                        // Detached — the config write must not park the serial
                        // bridge between `finish` and the reply.
                        crate::config::record_auth_result_detached(card_number, true);
                        log::info!("{} [MQTT] status=auth_finished", log_header);
                    }
                    // a command of the session is in flight, so the card is busy
                    Some(false) => set_rack_card_state(iccid, true, true),
                    // non-session signalling: no activity to show or record
                    None => {}
                }
            }

            // The closing message carries no command (`serial_cmd` empty) — it
            // exists only to mark the end of the session, so there is nothing to
            // put on the wire. Running it as an exchange would write a zero-byte
            // frame to the rack and come back `no_reply`.
            let finished = envelope.finish == Some(true);
            let exchange = if envelope.cmd_hex.is_empty() {
                SerialExchange::ok(String::new())
            } else {
                execute_envelope(serial_port, envelope, log_header, true).await.0
            };
            if finished {
                if let Some((_, _, slot)) = card {
                    super::access::release(serial_port, slot, false, None);
                }
            }
            exchange
        }
        Err(code) => SerialExchange::error(code),
    };

    let resp_payload = exchange.to_payload();
    // Only successful exchanges are cached for idempotency: a repeated request after an error
    // must retry the rack (the device may have recovered), a repeat after success is answered
    // from cache without touching the port again.
    if exchange.is_ok() && req_id.is_some() {
        idempotency.last_request_id = req_id;
        idempotency.last_response_payload = Some(resp_payload.clone());
    }
    Some((resp_topic, resp_payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rack_topic_is_prefix_id_tail() {
        assert_eq!(rack_topic("SC1799", "link"), "rack/SC1799/link");
        assert_eq!(rack_topic("SC1799", "response/5"), "rack/SC1799/response/5");
        assert_eq!(rack_topic("SC1799", "watch"), "rack/SC1799/watch");
    }

    #[test]
    fn parse_rack_topic_splits_id_and_tail() {
        assert_eq!(
            parse_rack_topic("rack/SC1799/request/5"),
            Some(("SC1799", "request/5"))
        );
        assert_eq!(parse_rack_topic("rack/SC1799/watch"), Some(("SC1799", "watch")));
        assert_eq!(parse_rack_topic("rack/SC1799/cards"), Some(("SC1799", "cards")));
    }

    #[test]
    fn parse_rack_topic_rejects_other_shapes() {
        // not a rack topic at all
        assert_eq!(parse_rack_topic("request/5/0"), None);
        assert_eq!(parse_rack_topic("logs/5/0"), None);
        assert_eq!(parse_rack_topic("rack"), None);
        // rack topic without a tail, or without an id
        assert_eq!(parse_rack_topic("rack/SC1799"), None);
        assert_eq!(parse_rack_topic("rack/SC1799/"), None);
        assert_eq!(parse_rack_topic("rack//link"), None);
    }

    #[tokio::test]
    async fn reply_is_bound_to_the_connection_generation() {
        // The statics are process-wide; the test only asserts transitions
        // relative to whatever state it starts from.
        let (client, _eventloop) = AsyncClient::new(
            rumqttc::v5::MqttOptions::new("TBA0000000000000", "localhost", 1883),
            1,
        );
        on_app_offline();
        let stale = app_generation();
        // offline: nothing is delivered, whatever the generation
        assert!(!app_link_is(stale));
        on_app_connack(&client);
        let live = app_generation();
        assert_eq!(live, stale + 1);
        assert!(app_link_is(live));
        // a reply dispatched under the previous instance is dropped
        assert!(!app_link_is(stale));
        // the connection dropped: the pending reply of this instance is dropped too
        on_app_offline();
        assert!(!app_link_is(live));
        // reconnected: a new instance, the old generation stays dead
        on_app_connack(&client);
        assert!(!app_link_is(live));
        assert!(app_link_is(app_generation()));
    }

    #[test]
    fn request_tail_maps_to_response_tail_under_the_same_prefix() {
        // the reply of `rack/<id>/request/<n>` is `rack/<id>/response/<n>`:
        // the tail is rewritten, the prefix is put back by the caller
        let (rack_id, tail) = parse_rack_topic("rack/SC1799/request/12").unwrap();
        assert_eq!(request_id_from_topic(tail), Some(12));
        assert_eq!(
            rack_topic(rack_id, &request_to_response_topic(tail)),
            "rack/SC1799/response/12"
        );
    }
}
