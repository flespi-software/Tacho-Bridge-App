//! Module for working with MQTT connections.
//!
//! This module provides functionality for creating and managing MQTT connections.

// Standard library imports
use std::ffi::CStr; // For handling C-style strings in Rust.
use std::io::ErrorKind;
use std::time::Duration; // For specifying time durations. // For categorizing I/O errors.

// MQTT client library imports
use rumqttc::v5::mqttbytes::QoS; // Quality of Service levels for MQTT.
use rumqttc::v5::ConnectionError; // For handling MQTT connection errors.
use rumqttc::v5::StateError::{self, AwaitPingResp, ServerDisconnect};
use rumqttc::v5::{AsyncClient, Event, Incoming, MqttOptions}; // Core MQTT async client and options. // Specific error for server disconnection.
// use rumqttc::{Transport, TlsConfiguration};

// use native_tls::TlsConnector;

use pcsc::Disposition;
use pcsc::Protocols;
use pcsc::ShareMode;

// Tauri application framework imports
use tauri::async_runtime::{self, JoinHandle}; // Async runtime and task join handles for Tauri apps.

// Serialization/Deserialization library imports
use serde_json::Value; // For working with JSON data structures.

/// Timeout in seconds to wait before reconnecting to the server.
///
/// This value is used to set the interval between reconnection attempts
/// to the MQTT server in case of connection loss.
const SLEEP_DURATION_SECS: u64 = 10;

// Import TASK_POOL from the smart_card module
use crate::smart_card::TASK_POOL;

// Importing specific functionality from local modules
use crate::config::get_from_cache; // Function to get data from cache for syncing server data.
use crate::config::split_host_to_parts;
use crate::config::CacheSection; // Enum for cache sections for getting data from cache. // Function to split the host into parts for MQTT connection.

// Import the global_app_handle module to send events to the frontend
use crate::global_app_handle::emit_event;

/// Ensures an MQTT connection for the specified client ID.
pub async fn ensure_connection(reader_name: &CStr, client_id: String, atr: String) {
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
    let exists = task_pool.iter().any(|(id, _, _)| *id == client_id);
    // If existing connection is found, then return, no add a new connection for this client_id
    if exists {
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
            log::error!("Error: {}", e);
            return;
        }
    };

    // Getting the flespi token from the cache
    // let flespi_token = get_from_cache(CacheSection::Server, "token");

    //////////////////////////////////////////////////
    //  Create a new client ID for the MQTT connection
    //////////////////////////////////////////////////
    let mut mqtt_options = MqttOptions::new(&client_id, &host, port);
    // mqtt_options.set_credentials(flespi_token, "");
    mqtt_options.set_keep_alive(Duration::from_secs(300));
    // log::debug!("mqtt_options: {:?}", mqtt_options);
    println!("mqtt_options: {:?}", mqtt_options);

    ////////////// TLS ////////////////
    // let connector = TlsConnector::new().unwrap();
    // let transport = Transport::tls_with_default_config();
    // mqtt_options.set_transport(transport);

    // Create a new asynchronous MQTT client and its associated event loop
    // `mqtt_options` specifies the configuration for the MQTT connection
    // `10` is the capacity of the internal channel used by the event loop for buffering operations
    let (mqtt_client, mut eventloop) = AsyncClient::new(mqtt_options, 10);

    let mqtt_clinet_cloned = mqtt_client.clone();
    let client_id_cloned = client_id.clone();
    let reader_name = reader_name.to_owned(); // clonning the reader name for the async task

    // format of the logging header
    let log_header: String = format!("{} |", client_id);

    // init card fot the following using in the loop
    let mut card = match crate::smart_card::create_card_object(&reader_name) {
        Ok(card) => {
            log::debug!(
                "Card object created successfully for the reader: {}",
                reader_name.to_string_lossy()
            );
            card
        }
        Err(err) => {
            // Log the error and return from the current function to reconnect to the card
            log::error!(
                "Failed to create card object: {} for the reader: {}",
                err,
                reader_name.to_string_lossy()
            );
            return;
        }
    };

    // flag to control the card connection (to the server) status
    let mut is_online: bool = false;

    // create async task for the mqtt client
    let handle: JoinHandle<()> = async_runtime::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(notification) => {
                    if !is_online {
                        is_online = true;

                        // Send the global-cards-sync event to the frontend that card is connected
                        emit_event("global-cards-sync",
                            atr.clone().into(),
                            reader_name.to_string_lossy().into(),
                            "PRESENT".into(),
                            client_id_cloned.clone(),
                            Some(true),
                            None
                        );
                    }

                    log::debug!("{} Notification: {:?}", log_header, notification);

                    match notification {
                        Event::Incoming(Incoming::Publish(publish)) => {
                            // Extracting the topic from the incoming data
                            let topic_str = match std::str::from_utf8(&publish.topic) {
                                Ok(str) => str,
                                Err(e) => {
                                    eprintln!(
                                        "Error converting topic from bytes to string: {:?}",
                                        e
                                    );
                                    return;
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
                                    println!("Parsed JSON payload: {:?}", json_payload);

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
                                            emit_event("global-cards-sync",
                                                atr.clone().into(),
                                                reader_name.to_string_lossy().into(),
                                                "PRESENT".into(),
                                                client_id_cloned.clone(),
                                                Some(true),
                                                Some(false)
                                            );

                                            log::info!("Authentication process is finished");
                                            // Reset the card to its original state
                                            match card.reconnect(
                                                ShareMode::Shared,
                                                Protocols::ANY,
                                                Disposition::ResetCard,
                                            ) {
                                                Ok(_) => {
                                                    println!("Card reconnected successfully.");
                                                }
                                                Err(e) => {
                                                    println!("Failed to reconnect card: {:?}", e);
                                                    log::error!(
                                                        "{} Failed to reconnect card: {:?}",
                                                        log_header,
                                                        e
                                                    );
                                                }
                                            }

                                            payload_ack = process_rapdu_mqtt_hex("".to_string());

                                            // handle the case when finish == true
                                        } else {
                                            // finish flag is false here
                                            // PROCESS AUTHORIZATION WITH APDU COMMUNICATION
                                            // The "hex" parameter contains the apdu instruction that needs to be transferred to the card
                                            if let Some(hex_value) = json_payload.get("payload").and_then(|v| v.as_str()) {
                                                // 00A4020c020002 - select icc id file
                                                // 00b0000019 - read selected file

                                                log::info!(
                                                    "{} TRACKER: Payload hex value: {}",
                                                    log_header,
                                                    hex_value
                                                );

                                                let mut rapdu_mqtt_hex = String::new(); // empty string for the response

                                                if hex_value.is_empty() {
                                                    // If the input value is empty, then pass the ATR to the server.
                                                    rapdu_mqtt_hex = atr.clone();
                                                    // finish_value = true;    // This is a crutch, temporary solution to not include the visual effect of authorization.
                                                    //                         // Because the ATR request is not always the beginning of authorization.
                                                    //                         // Sometimes it is a part of the command that can be rejected by the tracker, so this part should be ignored

                                                    // Send the global-cards-sync event to the frontend that card is connected
                                                    emit_event("global-cards-sync",
                                                        atr.clone().into(),
                                                        reader_name.to_string_lossy().into(),
                                                        "PRESENT".into(),
                                                        client_id_cloned.clone(),
                                                        Some(true),
                                                        Some(false)
                                                    );

                                                } else {
                                                    // Otherwise, the logic for exchanging messages with the map.
                                                    match crate::smart_card::send_apdu_to_card_command(&card, &hex_value) {
                                                        Ok(response) => {
                                                            rapdu_mqtt_hex = response;
                                                            println!("{} APDU response: {:?}", client_id_cloned, rapdu_mqtt_hex);
                                                        }
                                                        Err(err) => {
                                                            log::error!("Failed to send APDU command to card: {}", err);
                                                        }
                                                    }

                                                    // Send the global-cards-sync event to the frontend that card is connected
                                                    emit_event("global-cards-sync",
                                                        atr.clone().into(),
                                                        reader_name.to_string_lossy().into(),
                                                        "PRESENT".into(),
                                                        client_id_cloned.clone(),
                                                        Some(true),
                                                        Some(true)
                                                    );

                                                }

                                                payload_ack = process_rapdu_mqtt_hex(rapdu_mqtt_hex);


                                                // log::info!("finish_value: {}", finish_value);
                                            } else {
                                                log::error!(
                                                    "{} Hex value not found or is not a string",
                                                    log_header
                                                );
                                            }

                                            log::info!(
                                                "{} CARD: Payload hex value: {}",
                                                log_header,
                                                payload_ack
                                            );
                                        }

                                        // publish a message to the channel
                                        let publish_result = mqtt_client
                                            .publish(
                                                topic_ack,
                                                QoS::AtLeastOnce,
                                                false,
                                                payload_ack,
                                            )
                                            .await;
                                        match publish_result {
                                            Ok(_) => println!("Message published successfully"),
                                            Err(e) => println!("Error sending message: {:?}", e),
                                        }
                                    } else {
                                        println!("Finish parameter not found or is not a boolean");
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
                                "{} Сonnection to the server has been successfully established.",
                                log_header
                            )
                        }
                        _ => {} // This handles any other events that you haven't explicitly matched above
                    }
                }
                Err(e) => {
                    if is_online {
                        is_online = false;

                        // Send the global-cards-sync event to the frontend that card is connected
                        emit_event("global-cards-sync",
                            atr.clone().into(),
                            reader_name.to_string_lossy().into(),
                            "PRESENT".into(),
                            client_id_cloned.clone(),
                            Some(false),
                            None
                        );
                    }

                    match e {
                        ConnectionError::Io(ref io_err) => match io_err.kind() {
                            ErrorKind::ConnectionAborted => log::warn!("{} Can't establish a connection to a remote server.", log_header),
                            ErrorKind::ConnectionReset => log::warn!("{} The connection could not be established. Check the server address in the configuration.", log_header),
                            ErrorKind::TimedOut => log::warn!("{} Connection timeout. The server may be down or the network is unstable.", log_header),
                            _ => log::error!("{} An IO error occurred.", log_header),
                        },
                        ConnectionError::MqttState(ServerDisconnect { .. }) => log::warn!("{} The connection was terminated on the server side. Most likely the user has turned off the channel/device.", log_header),
                        ConnectionError::MqttState(AwaitPingResp { .. }) => {
                            log::warn!("{} Awaiting PING response from the server. The connection might be unstable.", log_header);
                            // Implement your reconnection or handling strategy here
                        },
                        ConnectionError::MqttState(StateError::Io(os_err)) => {
                            println!("An IO error occurred in MQTT state: {:?}", os_err);
                        },
                        _ => {
                            log::error!("{} Unhandled error: {:?}", log_header, e);
                            // return; // exit the loop
                        },
                    };
                    // Reconnection timeout for handled errors
                    tokio::time::sleep(Duration::from_secs(SLEEP_DURATION_SECS)).await;
                }
            }
        }
    });

    task_pool.push((client_id, mqtt_clinet_cloned, handle));
}

/// Removes specified MQTT connections.
///
/// This function iterates over a list of client IDs, finds the corresponding
/// tasks in the task pool, and cancels them. It ensures that any active connection
/// associated with the given client IDs is terminated.
pub async fn remove_connections(client_ids: Vec<String>) {
    log::debug!("removing conn {:?}", client_ids);
    // Unlock task_pool mutex
    let mut task_pool = TASK_POOL.lock().await;

    for client_id in client_ids {
        // Attempt to find a task associated with the current client ID
        if let Some(index) = task_pool.iter().position(|(id, _, _)| *id == client_id) {
            // If found, remove the task from the pool and abort it
            let (_, _, handle) = task_pool.remove(index);
            handle.abort();
            // Log the termination of the connection
            log::info!(
                "{} Connection to the server has been terminated.",
                client_id
            );
        }
    }
}

fn process_rapdu_mqtt_hex(rapdu_mqtt_hex: String) -> String {
    // Create a JSON object with the hex value
    let json_value = serde_json::json!({
        "payload": rapdu_mqtt_hex,
    });

    // Serialize the JSON object to a string and assign it to `payload_ack`
    let payload_ack = json_value.to_string();

    // Print the acknowledgment payload to the console
    println!("Payload ack: {}", payload_ack);

    payload_ack
}