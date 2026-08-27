//! The rack's own MQTT connection.
//!
//! Carries the server's rack-level commands (`connect`, `watch`, `disconnect`)
//! and the plain serial envelopes addressed to the rack itself. Per-card
//! sessions live in `cards.rs`; the wire is in `transport.rs`.

use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::{AsyncClient, Event, Incoming};
use std::time::Duration;

use crate::config::{get_from_cache, split_host_to_parts, CacheSection};
use crate::mqtt::{request_id_from_topic, request_to_response_topic};

use super::cards::{handle_card_disconnect, handle_connect_spawn, start_rack_watch};
use super::state::set_rack_card_state;
use super::transport::{execute_envelope, parse_envelope, SerialExchange, SharedPort};
use super::{next_reconnect_delay, RECONNECT_DELAY_INITIAL_SECS};

pub(super) async fn rack_mqtt_loop(client_id: String, serial_port: SharedPort) {
    let log_header = format!("RACK {} |", client_id);

    // Do not exit when the server host is missing or invalid — typical on
    // first launch, when the rack is plugged in before the server is
    // configured. Exiting would leave a finished task in RACK_MQTT_TASK and
    // no rack MQTT until the device is re-plugged; poll the config instead.
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
        "{} [MQTT] phase=connect_attempt status=initialized host={}:{}",
        log_header,
        host,
        port
    );
    let (mqtt_client, mut eventloop) = crate::mqtt::build_mqtt_client(&client_id, &host, port);

    let mut is_online = false;
    let mut reconnect_delay_secs = RECONNECT_DELAY_INITIAL_SECS;

    // Idempotency state for the current MQTT connection: the server re-sends a request with the
    // same id when it does not get a timely reply. Remember the last id we answered and its reply
    // so a repeat re-sends the cached response instead of re-forwarding to the rack. Reset on every
    // CONNACK, because a new MQTT session restarts the server-side request_id counter at 1.
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
                        // New MQTT session: server restarts request_id at 1, drop the idempotency slot.
                        idempotency.reset();
                        log::debug!("{} [MQTT] event=CONNACK status=received", log_header);
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
                        // Full command text only at debug: the rack protocol
                        // must not end up in users' log files at INFO level.
                        log::debug!(
                            "{} [MQTT] command_text={}",
                            log_header,
                            String::from_utf8_lossy(&publish.payload)
                        );

                        if topic == "connect" {
                            // spawn instruction: the server discovered a card in a rack slot
                            handle_connect_spawn(
                                &publish.payload,
                                &client_id,
                                &serial_port,
                                &log_header,
                            )
                            .await;
                        } else if topic == "watch" {
                            // arm/re-arm this rack's card presence watch with the
                            // server-supplied bytes
                            start_rack_watch(
                                &publish.payload,
                                &client_id,
                                &serial_port,
                                &mqtt_client,
                                &log_header,
                            );
                            // The server arms the presence watch once its discovery
                            // chain has walked the rack, so this doubles as the
                            // "enumeration finished" signal the UI needs to stop
                            // showing a scan in progress. Treated as a hint, not a
                            // contract: the frontend still has its own timeout in
                            // case a server version never sends `watch`.
                            crate::global_app_handle::rack_mark_scan_complete(&client_id);
                        } else if topic == "disconnect" {
                            // a card left its slot: close the session (if this rack
                            // owns it), drop it from this rack's UI section
                            handle_card_disconnect(&publish.payload, &client_id, &log_header);
                        } else if topic.starts_with("request/") {
                            handle_serial_request(
                                &mqtt_client,
                                &topic,
                                &publish.payload,
                                &serial_port,
                                &log_header,
                                &mut idempotency,
                                // the rack's own connection serves no single card
                                None,
                            )
                            .await;
                        } else {
                            // An unknown control topic (a newer server feature, or
                            // a retained stray) must not fall through to the serial
                            // path: its payload would be written raw to the COM
                            // port, and the reply published back to the control
                            // topic itself (request_to_response_topic returns
                            // non-`request/` topics unchanged). Log and drop.
                            log::warn!(
                                "{} [MQTT] status=ignored reason=unknown_topic topic={}",
                                log_header,
                                topic
                            );
                        }
                    }
                    other => {
                        // Full broker exchange is visible at debug (TBA_LOG=com_port=debug).
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

                // One line per failed poll: kind + retry delay; full error
                // details only for genuinely unexpected failures.
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

/// Handles one `request/...` publish on a rack-linked MQTT connection (the rack's own or a
/// rack-backed card's): idempotency, envelope parsing, serial execution, and the always-reply
/// response publish — the app is the server's only feedback channel, so a silent rack must be
/// reported, not swallowed.
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

pub(super) async fn handle_serial_request(
    mqtt_client: &AsyncClient,
    topic: &str,
    payload: &[u8],
    serial_port: &SharedPort,
    log_header: &str,
    idempotency: &mut IdempotencySlot,
    // `(iccid, card_number)` of the card this connection serves, when the
    // caller is a card session. `None` for the rack's own connection, which
    // has no single card and therefore no authentication state to track.
    card: Option<(&str, &str)>,
) {
    // A server-driven rack exchange counts as card activity: the auto-updater
    // must not restart the app in the middle of a rack card operation. The
    // 1 Hz presence watch does NOT go through here, so idle racks stay quiet.
    crate::mqtt::touch_card_activity();
    // Idempotency: the server re-sends a request with the same id after a timeout. If we already
    // answered this id, re-send the cached response without touching the port again; if it is
    // still in flight, drop the duplicate.
    let req_id = request_id_from_topic(topic);
    if req_id.is_some() && req_id == idempotency.last_request_id {
        match &idempotency.last_response_payload {
            Some(cached) => {
                log::warn!(
                    "{} [MQTT] status=duplicate_request request_id={:?} action=resend_cached",
                    log_header,
                    req_id
                );
                let resp_topic = request_to_response_topic(topic);
                if let Err(e) = mqtt_client
                    .publish(resp_topic, QoS::AtLeastOnce, false, cached.clone())
                    .await
                {
                    log::error!("{} [MQTT] status=cached_reply_publish_failed err={:?}", log_header, e);
                }
            }
            None => log::warn!(
                "{} [MQTT] status=duplicate_request request_id={:?} action=ignored reason=in_flight",
                log_header,
                req_id
            ),
        }
        return;
    }

    let json = match serde_json::from_slice::<serde_json::Value>(payload) {
        Ok(json) => json,
        Err(e) => {
            log::warn!(
                "{} [MQTT] status=ignored reason=bad_json err={}",
                log_header,
                e
            );
            return;
        }
    };
    let Some(parsed) = parse_envelope(&json) else {
        log::warn!("{} [MQTT] status=ignored reason=no_serial_cmd", log_header);
        return;
    };

    let exchange = match parsed {
        Ok(envelope) => {
            // Authentication boundaries come from the server's `finish` flag —
            // the same contract the PC/SC path uses (see `auth_process` in
            // mqtt.rs). Nothing is inferred from the traffic itself: the pause
            // while the tracker thinks after the ATR read is indistinguishable
            // from the end of a session, so guessing got it wrong either way.
            //
            // Every other way a session can end already clears the state at its
            // own site — connection lost, card pulled from the slot, rack
            // unplugged. The one gap — the closing `finish:true` simply never
            // arriving (tracker abort, lost message, or an older server that
            // does not send the flag) — is covered by the keep-alive PingResp
            // reset in the card's MQTT loop (see cards.rs), so no dedicated
            // timeout is needed here.
            if let Some((iccid, card_number)) = card {
                if envelope.finish == Some(true) {
                    set_rack_card_state(iccid, true, false);
                    // Same bookkeeping as the reader path (mqtt.rs): persist
                    // the auth timestamp so a card that only ever authenticates
                    // through a rack still shows "Last auth" in the UI.
                    // Detached — the config write must not park the serial
                    // bridge between `finish` and the reply.
                    crate::config::record_auth_result_detached(card_number, true);
                    log::info!("{} [MQTT] status=auth_finished", log_header);
                } else {
                    // `finish: false`, or a server that omits the flag: either
                    // way a command is in flight, so the card is busy.
                    set_rack_card_state(iccid, true, true);
                }
            }

            // The closing message carries no command (`serial_cmd` empty) — it
            // exists only to mark the end of the session, so there is nothing to
            // put on the wire. Running it as an exchange would write a zero-byte
            // frame to the rack and come back `no_reply`.
            if envelope.cmd_hex.is_empty() {
                SerialExchange::ok(String::new())
            } else {
                execute_envelope(serial_port, envelope, log_header, true).await
            }
        }
        Err(code) => SerialExchange::error(code),
    };

    let resp_topic = request_to_response_topic(topic);
    let resp_payload = exchange.to_payload();
    // Only successful exchanges are cached for idempotency: a repeated request after an error
    // must retry the rack (the device may have recovered), a repeat after success is answered
    // from cache without touching the port again.
    if exchange.is_ok() && req_id.is_some() {
        idempotency.last_request_id = req_id;
        idempotency.last_response_payload = Some(resp_payload.clone());
    }
    if let Err(e) = mqtt_client
        .publish(resp_topic, QoS::AtLeastOnce, false, resp_payload)
        .await
    {
        log::error!(
            "{} [MQTT] status=reply_publish_failed err={:?}",
            log_header,
            e
        );
    }
}
