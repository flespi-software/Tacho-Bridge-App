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
use crate::smart_card::TASK_POOL;   // Task pool for managing MQTT connections.

// Importing specific functionality from local modules
use crate::config::get_from_cache; // Function to get data from cache for syncing server data.
use crate::config::split_host_to_parts;
use crate::config::CacheSection; // Enum for cache sections for getting data from cache. // Function to split the host into parts for MQTT connection.

// Import the global_app_handle module to send events to the frontend
use crate::global_app_handle::emit_event;

/// Parses the ATR and extracts the communication protocol (T=0 or T=1).
///
/// # Arguments
/// - `atr`: A string containing the ATR in hexadecimal format.
///
/// # Returns
/// - `String`: The communication protocol ("T0", "T1", or "Unknown").
pub fn parse_atr_and_get_protocol(atr: &str) -> Protocols {
    let atr_bytes = match hex::decode(atr) {
        Ok(bytes) => bytes,
        Err(_) => {
            log::error!("Invalid ATR format: {}", atr);
            return Protocols::T0;
        }
    };

    if atr_bytes.len() < 2 {
        log::error!("ATR is too short: {:?}", atr_bytes);
        return Protocols::T0;
    }

    let mut index = 1;
    let y1 = atr_bytes[index] >> 4;
    index += 1;

    // Skip TA1, TB1, TC1 depends on Y1
    if y1 & 0x1 != 0 { index += 1; } // TA1
    if y1 & 0x2 != 0 { index += 1; } // TB1
    if y1 & 0x4 != 0 { index += 1; } // TC1

    // TD1
    let td1 = if y1 & 0x8 != 0 && index < atr_bytes.len() {
        let td1 = atr_bytes[index];
        index += 1;
        Some(td1)
    } else {
        None
    };

    // TD2 (if was TD1)
    let td2 = if let Some(td1) = td1 {
        let y2 = td1 >> 4;
        // Skip TA2, TB2, TC2
        if y2 & 0x1 != 0 { index += 1; } // TA2
        if y2 & 0x2 != 0 { index += 1; } // TB2
        if y2 & 0x4 != 0 { index += 1; } // TC2

        if y2 & 0x8 != 0 && index < atr_bytes.len() {
            Some(atr_bytes[index])
        } else {
            None
        }
    } else {
        None
    };

    // If TD2 exists — it is default protocol
    if let Some(td2) = td2 {
        let proto = td2 & 0x0F;
        return match proto {
            0x00 => Protocols::T0,
            0x01 => Protocols::T1,
            _ => Protocols::T0, // fallback
        };
    }

    // If TD2 is not presented, but TD1 it is — use it
    if let Some(td1) = td1 {
        let proto = td1 & 0x0F;
        return match proto {
            0x00 => Protocols::T0,
            0x01 => Protocols::T1,
            _ => Protocols::T0, // fallback
        };
    }

    // Default value if have no TD1 and TD2
    Protocols::T0
}

/// Ensures an MQTT connection for the specified client ID.
pub async fn ensure_connection(reader_name: &CStr, client_id: String, atr: String) {
    // Return early if the client_id is empty, as we cannot ensure a connection without a valid ID
    if client_id.is_empty() {
        log::warn!("Reader: {:?}. ClientID is empty. Cannot ensure connection.", reader_name);
        return;
    }

    let protocol = parse_atr_and_get_protocol(&atr);
    log::info!("Reader: {:?}. ATR: {}. Protocol: {:?}", reader_name, atr, protocol);

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
    mqtt_options.set_keep_alive(Duration::from_secs(120));
    // log::debug!("mqtt_options: {:?}", mqtt_options);
    log::debug!("mqtt_options: {:?}", mqtt_options);

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
    let mut card = match crate::smart_card::create_card_object(&reader_name, protocol) {
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

    let mut is_online: bool = false;    // flag to control the card connection (to the server) status
    let mut was_online = false;   // Flag to track the previous connection status
    let mut auth_process: bool = false;  // Flag to control the authentication process

    // create async task for the mqtt client
    let handle: JoinHandle<()> = async_runtime::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(notification) => {
                    if !is_online {
                        is_online = true;
                        if !was_online {
                            was_online = true;
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
                                                    log::error!(
                                                        "{} Failed to reconnect card: {:?}. Trying to recreate the card object...",
                                                        log_header,
                                                        e
                                                    );
                                                
                                                    // attempt to recreate card object
                                                    match crate::smart_card::create_card_object(&reader_name, protocol) {
                                                        Ok(new_card) => {
                                                            log::info!(
                                                                "Successfully recreated card object for reader: {}",
                                                                reader_name.to_string_lossy()
                                                            );
                                                            card = new_card; // change old card object to new one
                                                        }
                                                        Err(err) => {
                                                            log::error!(
                                                                "Failed to recreate card object: {} for the reader: {}. Giving up.",
                                                                err,
                                                                reader_name.to_string_lossy()
                                                            );
                                                        }
                                                    }
                                                }
                                            }

                                            payload_ack = process_rapdu_mqtt_hex("".to_string());

                                            auth_process = false;   // Authorization process is finished

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
                                                    // This case is needed to reset the card when authorization is not completed, otherwise the card will not respond to commands correctly.
                                                    if auth_process { 
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
                                                                log::error!(
                                                                    "{} Failed to reconnect card: {:?}. Trying to recreate the card object...",
                                                                    log_header,
                                                                    e
                                                                );
                                                            
                                                                // attempt to recreate card object
                                                                match crate::smart_card::create_card_object(&reader_name, protocol) {
                                                                    Ok(new_card) => {
                                                                        log::info!(
                                                                            "Successfully recreated card object for reader: {}",
                                                                            reader_name.to_string_lossy()
                                                                        );
                                                                        card = new_card; // change old card object to new one
                                                                    }
                                                                    Err(err) => {
                                                                        log::error!(
                                                                            "Failed to recreate card object: {} for the reader: {}. Giving up.",
                                                                            err,
                                                                            reader_name.to_string_lossy()
                                                                        );
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }

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
                                                    // // Otherwise, the logic for exchanging messages with the card.
                                                    // match crate::smart_card::send_apdu_to_card_command(&card, &hex_value) {
                                                    //     Ok(response) => {
                                                    //         rapdu_mqtt_hex = response;
                                                    //         log::debug!("{} APDU response: {:?}", client_id_cloned, rapdu_mqtt_hex);
                                                    //     }
                                                    //     Err(err) => {
                                                    //         log::error!("Failed to send APDU command to card: {}", err);
                                                    //     }
                                                    // }

                                                    match crate::smart_card::send_apdu_to_card_command(&card, &hex_value) {
                                                        Ok(response) => {
                                                            rapdu_mqtt_hex = response;
                                                            log::debug!("{} APDU response: {:?}", client_id_cloned, rapdu_mqtt_hex);
                                                        }
                                                        Err(err) => {
                                                            log::error!("Failed to send APDU command to card: {}. Trying to recreate card object...", err);
                                                            
                                                            // Try to recreate card object
                                                            match crate::smart_card::create_card_object(&reader_name, protocol) {
                                                                Ok(new_card) => {
                                                                    log::info!(
                                                                        "Successfully recreated card object for reader: {}. Retrying APDU command.",
                                                                        reader_name.to_string_lossy()
                                                                    );
                                                                    card = new_card; // Replace old card object with new one
                                                                    
                                                                    // Retry sending APDU command with new card object
                                                                    match crate::smart_card::send_apdu_to_card_command(&card, &hex_value) {
                                                                        Ok(response) => {
                                                                            rapdu_mqtt_hex = response;
                                                                            log::debug!("{} APDU response (after reconnect): {:?}", client_id_cloned, rapdu_mqtt_hex);
                                                                        }
                                                                        Err(retry_err) => {
                                                                            log::error!(
                                                                                "{} Failed to send APDU command even after card reconnection: {}",
                                                                                client_id_cloned,
                                                                                retry_err
                                                                            );
                                                                            // Set empty response or error code if needed
                                                                            rapdu_mqtt_hex = "6F00".to_string(); // Generic error response
                                                                        }
                                                                    }
                                                                }
                                                                Err(create_err) => {
                                                                    log::error!(
                                                                        "Failed to recreate card object: {} for the reader: {}",
                                                                        create_err,
                                                                        reader_name.to_string_lossy()
                                                                    );
                                                                    // Set empty response or error code
                                                                    rapdu_mqtt_hex = "6F00".to_string(); // Generic error response
                                                                }
                                                            }
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

                                                    auth_process = true;    // Authorization process is in progress
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
                        Event::Incoming(Incoming::PingResp(..)) => {
                            log::info!(
                                "{} Ping response received from the server.",
                                log_header
                            );
                            
                            // Send the global-cards-sync event to the frontend that card is connected
                            emit_event("global-cards-sync",
                                atr.clone().into(),
                                reader_name.to_string_lossy().into(),
                                "PRESENT".into(),
                                client_id_cloned.clone(),
                                Some(true),
                                Some(false)
                            );
                        }
                        _ => {} // This handles any other events that you haven't explicitly matched above
                    }
                }
                Err(e) => {
                    // Send the global-cards-sync event to the frontend that card is connected
                    emit_event("global-cards-sync",
                        atr.clone().into(),
                        reader_name.to_string_lossy().into(),
                        "PRESENT".into(),
                        client_id_cloned.clone(),
                        Some(false),
                        None
                    );

                    is_online = false;
                    was_online = false; // Reset the flag when the connection is lost

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

    // Логирование содержимого task_pool после добавления новой задачи
    log::info!("Current tasks in the pool:");
    for (id, _, _) in task_pool.iter() {
        log::info!("Client ID: {}", id);
    }

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

pub async fn remove_connections_all() {
    log::debug!("removing all conn's ");
    // Unlock task_pool mutex
    let mut task_pool = TASK_POOL.lock().await;

    // Abort all tasks in the pool
    for (_, _, handle) in task_pool.drain(..) {
        handle.abort();
    }
    log::info!("All connections to the server have been terminated.");
}

fn process_rapdu_mqtt_hex(rapdu_mqtt_hex: String) -> String {
    // Create a JSON object with the hex value
    let json_value = serde_json::json!({
        "payload": rapdu_mqtt_hex,
    });

    // Serialize the JSON object to a string and assign it to `payload_ack`
    let payload_ack = json_value.to_string();

    // Print the acknowledgment payload to the console
    log::debug!("Payload ack: {}", payload_ack);

    payload_ack
}
