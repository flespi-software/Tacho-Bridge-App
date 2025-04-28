//! Module for working with MQTT connections.
//!
//! This module provides functionality for creating and managing MQTT connections.

// Standard library imports
use std::io::ErrorKind; // For categorizing I/O errors.
use std::time::Duration; // For specifying time durations.

// MQTT client library imports
use rumqttc::v5::ConnectionError; // For handling MQTT connection errors.
use rumqttc::v5::StateError::{self, AwaitPingResp, ServerDisconnect}; // Specific error for server disconnection.
use rumqttc::v5::{AsyncClient, Event, Incoming, MqttOptions}; // Core MQTT async client and options.

// Import TASK_POOL from the smart_card module
use crate::smart_card::TASK_POOL;   // Task pool for managing MQTT connections.

// Tauri application framework imports
use tauri::async_runtime::{self, JoinHandle}; // Async runtime and task join handles for Tauri apps.

// Serialization/Deserialization library imports
use serde_json::Value; // For working with JSON data structures.

/// Timeout in seconds to wait before reconnecting to the server.
///
/// This value is used to set the interval between reconnection attempts
/// to the MQTT server in case of connection loss.
const SLEEP_DURATION_SECS: u64 = 10;

// Importing specific functionality from local modules
use crate::config::get_from_cache; // Function to get data from cache for syncing server data.
use crate::config::split_host_to_parts; // Function to split the host into parts for MQTT connection.
use crate::config::CacheSection; // Enum for cache sections for getting data from cache.

/// Ensures an MQTT connection for the specified client ID.
#[tauri::command]
pub async fn app_connection() {
    log::debug!("Ensuring APP MQTT connection...");
    // Getting server data from the cache
    let full_host = get_from_cache(CacheSection::Server, "host");
    let (host, port) = match split_host_to_parts(&full_host) {
        Ok((host, port)) => {
            log::debug!("Server data from cache: {:?}:{}", host, port);
            (host, port)
        }
        Err(e) => {
            log::error!("Error: {}", e);
            return;
        }
    };
    // Getting ident from the cache
    let client_id = get_from_cache(CacheSection::Ident, "ident");

    if client_id.is_empty() {
        log::warn!("client_id: {:?}. ClientID is empty. Cannot ensure connection.", client_id);
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

    //////////////////////////////////////////////////
    //  Create a new client ID for the MQTT connection
    //////////////////////////////////////////////////
    let mut mqtt_options = MqttOptions::new(client_id.clone(), &host, port);
    // mqtt_options.set_credentials(flespi_token, "");
    mqtt_options.set_keep_alive(Duration::from_secs(120));
    log::debug!("mqtt_options: {:?}", mqtt_options);

    // Create a new asynchronous MQTT client and its associated event loop
    // `mqtt_options` specifies the configuration for the MQTT connection
    // `10` is the capacity of the internal channel used by the event loop for buffering operations
    let (mqtt_client, mut eventloop) = AsyncClient::new(mqtt_options, 10);
    let mqtt_clinet_cloned = mqtt_client.clone();
    // let client_id_cloned = client_id.clone();

    let log_header: String = format!("{} |", client_id);

    // create async task for the mqtt client
    // create async task for the mqtt client
    let handle: JoinHandle<()> = async_runtime::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(notification) => {
                    log::debug!("App {} Notification: {:?}", log_header, notification);

                    match notification {
                        Event::Incoming(Incoming::Publish(publish)) => {
                            // Extracting the topic from the incoming data
                            // let topic_str = match std::str::from_utf8(&publish.topic) {
                            //     Ok(str) => str,
                            //     Err(e) => {
                            //         eprintln!("Error converting topic from bytes to string: {:?}", e);
                            //         return;
                            //     }
                            // };

                            // Convert &str to String for further use
                            // let topic = topic_str.to_string();
                            // The contents of response and request are the same.
                            // Card number and parcel ID. So we just change the initial topic
                            // let topic_ack = topic.replace("request", "response");

                            // serializable data to interpret it as json
                            match serde_json::from_slice::<Value>(&publish.payload) {
                                Ok(json_payload) => {
                                    println!("Parsed JSON payload: {:?}", json_payload);
                                    // The "hex" parameter contains the apdu instruction that needs to be transferred to the card
                                }
                                Err(e) => {
                                    log::error!("{} parsing JSON payload issue: {:?}", log_header, e);
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
                        ConnectionError::MqttState(StateError::Io{ .. }) => {
                            log::warn!("{} MQTT state IO error: Connection closed by peer", log_header);
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
