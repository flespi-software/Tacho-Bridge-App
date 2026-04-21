//! Module for working with MQTT connections.
//!
//! This module provides functionality for creating and managing MQTT connections.

// ───── Std Lib ─────
use std::ffi::CStr;                  // For handling C-style strings in Rust.
use std::io::ErrorKind;             // For categorizing I/O errors.
use std::time::Duration;            // For specifying time durations.

// ───── MQTT Client Library (rumqttc) ─────
use rumqttc::v5::mqttbytes::QoS;                    // Quality of Service levels for MQTT.
use rumqttc::v5::ConnectionError;                   // For handling MQTT connection errors.
use rumqttc::v5::StateError::{self, AwaitPingResp, ServerDisconnect}; // Specific error for server disconnection.
use rumqttc::v5::{AsyncClient, Event, Incoming, MqttOptions};        // Core MQTT async client and options.

// ───── Tauri ─────
use tauri::async_runtime::{self, JoinHandle};       // Async runtime and task join handles for Tauri apps.

// ───── Serde (Serialization / Deserialization) ─────
use serde_json::Value;                              // For working with JSON data structures.

// ───── Local Modules ─────
use crate::config::get_from_cache;                  // Function to get data from cache for syncing server data.
use crate::config::split_host_to_parts;             // Function to split the host into parts for MQTT connection.
use crate::config::CacheSection;                    // Enum for cache sections for getting data from cache.
use crate::smart_card::{ManagedCard, TASK_POOL};    // Managed card object and global task pool for MQTT handling.
use crate::global_app_handle::card_emit_event;           // Sends events to the frontend via global app handle.
use crate::smart_card::ProcessingCard;

/// Timeout in seconds to wait before reconnecting to the server.
///
/// This value is used to set the interval between reconnection attempts
/// to the MQTT server in case of connection loss.
const SLEEP_DURATION_SECS: u64 = 10;
const GLOBAL_CARDS_SYNC_EVENT: &str = "global-cards-sync";
const CARD_PRESENT_STATE: &str = "PRESENT";

fn emit_card_sync_event(
    iccid: &str,
    reader_name: &CStr,
    client_id: &str,
    is_online: Option<bool>,
    auth_in_progress: Option<bool>,
) {
    card_emit_event(
        GLOBAL_CARDS_SYNC_EVENT,
        iccid.to_owned().into(),
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
pub async fn ensure_connection(reader_name: &CStr, client_id: String, atr: String, managed_card: ManagedCard) {
    log::info!(
        "[CONN] phase=ensure_connection status=start reader={} client_id={} atr_len={}",
        reader_name.to_string_lossy(),
        client_id,
        atr.len()
    );

    // Return early if the client_id is empty, as we cannot ensure a connection without a valid ID
    if client_id.is_empty() {
        log::warn!("Reader: {:?}. ClientID is empty. Cannot ensure connection.", reader_name);
        return;
    }

    // Unlock task_pool mutex
    let mut task_pool = TASK_POOL.lock().await;

    // This part of function checks if a connection already exists for the given client ID
    // in the task pool. If not, it initiates a new connection. This is useful for maintaining
    // a list of active MQTT connections and ensuring that each client ID is only connected once.
    let exists = task_pool.iter().any(|card| card.client_id == client_id);
    // If existing connection is found, then return, no add a new connection for this client_id
    if exists {
        log::info!(
            "[CONN] phase=ensure_connection status=skip_existing reader={} client_id={}",
            reader_name.to_string_lossy(),
            client_id
        );
        return;
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

    //////////////////////////////////////////////////
    //  Create a new client ID for the MQTT connection
    //////////////////////////////////////////////////
    let mut mqtt_options = MqttOptions::new(&client_id, &host, port);
    // mqtt_options.set_credentials(flespi_token, "");
    mqtt_options.set_keep_alive(Duration::from_secs(120));
    // log::debug!("mqtt_options: {:?}", mqtt_options);
    log::debug!("mqtt_options: {:?}", mqtt_options);
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

    let mut is_online: bool = false;    // flag to control the card connection (to the server) status
    let mut was_online = false;   // Flag to track the previous connection status
    let mut auth_process: bool = false;  // Flag to control the authentication process

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
                        if !was_online {
                            was_online = true;
                            // Send the global-cards-sync event to the frontend that card is connected
                            emit_card_sync_event(&iccid, &reader_name, &client_id_cloned, Some(true), None);

                            log::info!(
                                "{} [CONN] state=OFFLINE->ONLINE cause=eventloop_poll_ok",
                                log_header
                            );
                        }
                    }

                    log::debug!("{} Notification: {:?}", log_header, notification);

                    match notification {
                        Event::Incoming(Incoming::Publish(publish)) => {
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
                            // Card number and parcel ID. So we just change the initial topic
                            let topic_ack = topic.replace("request", "response");
                            // serializable data to interpret it as json
                            match serde_json::from_slice::<Value>(&publish.payload) {
                                Ok(json_payload) => {
                                    log::debug!("Parsed JSON payload: {:?}", json_payload);

                                    let mut payload_ack = String::new();

                                    // Check for the presence of the "finish" parameter
                                    if let Some(finish_value) = json_payload.get("finish").and_then(|v| v.as_bool()) {
                                        log::debug!(
                                            "{} Finish parameter: {}",
                                            log_header,
                                            finish_value
                                        );

                                        // Processing the "finish" parameter depending on its value
                                        if finish_value {
                                            // Send the global-cards-sync event to the frontend that card is connected
                                            emit_card_sync_event(&iccid, &reader_name, &client_id_cloned, Some(true), Some(false));

                                            log::info!(
                                                "{} Authentication process is finished",
                                                log_header
                                            );

                                            // Reset the card to its original state
                                            managed_card.reconnect().await;

                                            payload_ack = process_rapdu_mqtt_hex("".to_string());

                                            auth_process = false;   // Authorization process is finished

                                            // handle the case when finish == true
                                        } else {
                                            // finish flag is false here
                                            // PROCESS AUTHORIZATION WITH APDU COMMUNICATION
                                            // The "hex" parameter contains the apdu instruction that needs to be transferred to the card
                                            if let Some(hex_value) = json_payload.get("payload").and_then(|v| v.as_str()) {
                                                log::debug!(
                                                    "{} TRACKER: Payload hex value: {}",
                                                    log_header,
                                                    hex_value
                                                );

                                                let rapdu_mqtt_hex = if hex_value.is_empty() {
                                                    // This case is needed to reset the card when authorization is not completed, otherwise the card will not respond to commands correctly.
                                                    if auth_process {
                                                        log::warn!(
                                                            "{} Empty payload received while auth in progress. Reconnecting card.",
                                                            log_header
                                                        );
                                                        // Reset the card to its original state
                                                        managed_card.reconnect().await;
                                                    }

                                                    // If the input value is empty, then pass the ATR to the server.
                                                    log::info!(
                                                        "{} Authentication process started",
                                                        log_header
                                                    );

                                                    // Send the global-cards-sync event to the frontend that card is connected
                                                    emit_card_sync_event(&iccid, &reader_name, &client_id_cloned, Some(true), Some(false));

                                                    atr_clone.clone()
                                                } else {
                                                    // // Otherwise, the logic for exchanging messages with the card.
                                                    if !auth_process {
                                                        log::info!(
                                                            "{} Authentication APDU exchange started",
                                                            log_header
                                                        );
                                                    }

                                                    let rapdu = managed_card.send_apdu(&hex_value, &client_id_cloned).await;

                                                    // Passive sniffer: extract plaintext EF data from SM'd responses
                                                    crate::apdu_sniffer::sniff(&client_id_cloned, hex_value, &rapdu);

                                                    // Send the global-cards-sync event to the frontend that card is connected
                                                    emit_card_sync_event(&iccid, &reader_name, &client_id_cloned, Some(true), Some(true));

                                                    auth_process = true;    // Authorization process is in progress
                                                    rapdu
                                                };

                                                payload_ack = process_rapdu_mqtt_hex(rapdu_mqtt_hex);

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

                                        // publish a message to the channel
                                        publish_ack(&mqtt_client, topic_ack, payload_ack, &log_header).await;
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
                            log::info!(
                                "{} [CONN] event=CONNACK status=received",
                                log_header
                            )
                        }
                        Event::Incoming(Incoming::PingResp(..)) => {
                            log::debug!(
                                "{} Ping response received from the server.",
                                log_header
                            );

                            // Send the global-cards-sync event to the frontend that card is connected
                            emit_card_sync_event(&iccid, &reader_name, &client_id_cloned, Some(true), Some(false));
                        }
                        _ => {} // This handles any other events that you haven't explicitly matched above
                    }
                }
                Err(e) => {
                    // Send the global-cards-sync event to the frontend that card is connected
                    emit_card_sync_event(&iccid, &reader_name, &client_id_cloned, Some(false), None);

                    is_online = false;
                    was_online = false; // Reset the flag when the connection is lost

                    log::warn!(
                        "{} [CONN] state=ONLINE->OFFLINE err={:?}",
                        log_header,
                        e
                    );

                    match e {
                        ConnectionError::Io(ref io_err) => match io_err.kind() {
                            ErrorKind::ConnectionAborted => log::warn!(
                                "{} [CONN] failure=io kind=connection_aborted detail=remote_connection_not_established",
                                log_header
                            ),
                            ErrorKind::ConnectionReset => log::warn!(
                                "{} [CONN] failure=io kind=connection_reset detail=check_server_address",
                                log_header
                            ),
                            ErrorKind::TimedOut => log::warn!(
                                "{} [CONN] failure=io kind=timed_out detail=server_or_network_unstable",
                                log_header
                            ),
                            _ => log::error!("{} [CONN] failure=io kind=other", log_header),
                        },
                        ConnectionError::MqttState(ServerDisconnect { .. }) => log::warn!(
                            "{} [CONN] failure=mqtt_state kind=server_disconnect detail=server_terminated_connection",
                            log_header
                        ),
                        ConnectionError::MqttState(AwaitPingResp { .. }) => {
                            log::warn!(
                                "{} [CONN] failure=mqtt_state kind=await_ping_resp detail=connection_may_be_unstable",
                                log_header
                            );
                            // Implement your reconnection or handling strategy here
                        },
                        ConnectionError::MqttState(StateError::Io(os_err)) => {
                            log::error!(
                                "{} [CONN] failure=mqtt_state kind=io err={:?}",
                                log_header,
                                os_err
                            );
                        },
                        _ => {
                            log::error!("{} [CONN] failure=unhandled err={:?}", log_header, e);
                            // return; // exit the loop
                        },
                    };
                    // Reconnection timeout for handled errors
                    log::warn!(
                        "{} [CONN] action=reconnect_scheduled delay_secs={}",
                        log_header,
                        SLEEP_DURATION_SECS
                    );
                    tokio::time::sleep(Duration::from_secs(SLEEP_DURATION_SECS)).await;
                }
            }
        }
    });

    task_pool.push(ProcessingCard {
        client_id,
        reader_name: Some(reader_name_str),
        atr: Some(atr),
        mqtt_client: mqtt_clinet_cloned,
        task_handle: handle,
    });

    log::info!("MQTT task registered in TASK_POOL. Current size: {}", task_pool.len());

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

/// Terminates connections for the specified client IDs (card numbers).
pub async fn remove_connections(client_ids: Vec<String>) {
    log::debug!("Removing connections for client_ids: {:?}", client_ids);

    // Lock the task pool
    let mut task_pool = TASK_POOL.lock().await;

    for client_id in client_ids {
        // Find the index of the card with the matching client_id
        if let Some(index) = task_pool.iter().position(|card| card.client_id == client_id) {
            let card = task_pool.remove(index);
            card.task_handle.abort();

            log::debug!(
                "TASK_POOL: Connection terminated for client_id: {}, reader: {}, atr: {}",
                card.client_id,
                card.reader_name.as_deref().unwrap_or("unknown"),
                card.atr.as_deref().unwrap_or("unknown"),
            );
        } else {
            log::warn!(
                "TASK_POOL: No active connection found for requested client_id: {}",
                client_id
            );
        }
    }
}

/// Terminates all active card-related MQTT connections and clears the task pool.
pub async fn remove_connections_all() {
    log::info!("Removing all card connections...");

    // Lock the task pool
    let mut task_pool = TASK_POOL.lock().await;

    // Abort each task and log which client is being disconnected
    for card in task_pool.drain(..) {
        log::debug!(
            "TASK_POOL: Aborting task for client_id: {}, reader: {}, atr: {}",
            card.client_id,
            card.reader_name.as_deref().unwrap_or("unknown"),
            card.atr.as_deref().unwrap_or("unknown"),
        );
        card.task_handle.abort();
    }

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