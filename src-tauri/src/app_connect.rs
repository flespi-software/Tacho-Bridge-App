//! Module for working with MQTT connections.
//!
//! This module provides functionality for creating and managing MQTT connections.

// ───── Std Lib ─────
use std::time::Duration;                 // For specifying time durations.

// ───── MQTT Client Library (rumqttc) ─────
use rumqttc::v5::{AsyncClient, Event, Incoming, MqttOptions}; // Core MQTT async client and options.

// ───── Smart Card ─────
use crate::smart_card::TASK_POOL;        // Task pool for managing MQTT connections.

// ───── Tauri ─────
use tauri::async_runtime::{self, JoinHandle}; // Async runtime and task join handles for Tauri apps.

// ───── Serialization ─────
use serde_json::Value;                   // For working with JSON data structures.

// ───── Local Modules ─────
use crate::config::get_from_cache;       // Function to get data from cache for syncing server data.
use crate::config::split_host_to_parts;  // Function to split the host into parts for MQTT connection.
use crate::config::CacheSection;         // Enum for cache sections for getting data from cache.
use crate::global_app_handle::app_emit_event; // Emits app connection status to the frontend.
use crate::smart_card::ProcessingCard;

/// Initial delay (in seconds) before the first reconnect attempt after a
/// connection failure. Subsequent failures back off exponentially up to
/// `RECONNECT_DELAY_MAX_SECS`.
const RECONNECT_DELAY_INITIAL_SECS: u64 = 10;
/// Upper bound for the reconnect backoff. Past this point we keep retrying
/// at this interval until either the server comes back or the task is killed.
const RECONNECT_DELAY_MAX_SECS: u64 = 300;

/// Returns the next reconnect delay given the current one (exponential, capped).
fn next_reconnect_delay(current: u64) -> u64 {
    current.saturating_mul(2).min(RECONNECT_DELAY_MAX_SECS)
}

/// Ensures an MQTT connection for the specified client ID.
#[tauri::command]
pub async fn app_connection() {
    log::info!("[CONN] phase=app_connection status=start");

    // Getting server data from the cache
    let full_host = get_from_cache(CacheSection::Server, "host");
    let (host, port) = match split_host_to_parts(&full_host) {
        Ok((host, port)) => {
            log::debug!("[CONN] phase=config status=host_loaded host={}:{}", host, port);
            (host, port)
        }
        Err(e) => {
            log::error!("[CONN] phase=config status=failed reason=invalid_host err={}", e);
            return;
        }
    };
    // Getting ident from the cache
    let client_id = get_from_cache(CacheSection::Ident, "ident");

    if client_id.is_empty() {
        log::warn!("[CONN] phase=config status=failed reason=empty_client_id");
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
            "[CONN] phase=app_connection status=skip_existing client_id={}",
            client_id
        );
        return;
    }

    //////////////////////////////////////////////////
    //  Create a new client ID for the MQTT connection
    //////////////////////////////////////////////////
    let mut mqtt_options = MqttOptions::new(client_id.clone(), &host, port);
    // mqtt_options.set_credentials(flespi_token, "");
    mqtt_options.set_keep_alive(Duration::from_secs(120));
    log::debug!("mqtt_options: {:?}", mqtt_options);
    log::info!(
        "[CONN] phase=connect_attempt status=initialized client_id={} host={}:{}",
        client_id,
        host,
        port
    );

    // Create a new asynchronous MQTT client and its associated event loop
    // `mqtt_options` specifies the configuration for the MQTT connection
    // `10` is the capacity of the internal channel used by the event loop for buffering operations
    let (mqtt_client, mut eventloop) = AsyncClient::new(mqtt_options, 10);
    let mqtt_clinet_cloned = mqtt_client.clone();
    let log_header: String = format!("{} |", client_id);
    let client_id_for_task = client_id.clone();

    // create async task for the mqtt client
    let handle: JoinHandle<()> = async_runtime::spawn(async move {
        let mut is_online = false;
        let mut reconnect_delay_secs: u64 = RECONNECT_DELAY_INITIAL_SECS;

        log::info!("{} [CONN] phase=eventloop status=started", log_header);

        loop {
            match eventloop.poll().await {
                Ok(notification) => {
                    if !is_online {
                        is_online = true;
                        // Successful poll → reset backoff for the next failure.
                        reconnect_delay_secs = RECONNECT_DELAY_INITIAL_SECS;
                        log::info!(
                            "{} [CONN] state=OFFLINE->ONLINE cause=eventloop_poll_ok",
                            log_header
                        );
                        app_emit_event(true);
                    }

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
                                    log::debug!("Parsed JSON payload: {:?}", json_payload);
                                    // The "hex" parameter contains the apdu instruction that needs to be transferred to the card
                                }
                                Err(e) => {
                                    log::error!("{} parsing JSON payload issue: {:?}", log_header, e);
                                }
                            }
                        }
                        Event::Incoming(Incoming::ConnAck(..)) => {
                            // The OFFLINE->ONLINE transition is already logged
                            // at info; CONNACK itself is a detail.
                            log::debug!("{} [CONN] event=CONNACK status=received", log_header)
                        }
                        _ => {} // This handles any other events that you haven't explicitly matched above
                    }
                }
                Err(e) => {
                    let transition = if is_online { "ONLINE->OFFLINE" } else { "OFFLINE" };
                    if is_online {
                        is_online = false;
                        app_emit_event(false);
                    }

                    // One line per failed poll: kind + retry delay; full error
                    // details only for genuinely unexpected failures.
                    crate::mqtt::log_connection_failure(
                        &log_header, "CONN", transition, &e, reconnect_delay_secs,
                    );

                    tokio::time::sleep(Duration::from_secs(reconnect_delay_secs)).await;
                    reconnect_delay_secs = next_reconnect_delay(reconnect_delay_secs);
                }
            }
        }
    });

    task_pool.push(ProcessingCard {
        client_id,
        reader_name: None,
        atr: None,
        mqtt_client: mqtt_clinet_cloned,
        task_handle: handle,
    });

    log::info!(
        "[CONN] phase=task_pool status=registered client_id={} pool_size={}",
        client_id_for_task,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_reconnect_delay_doubles_then_saturates() {
        assert_eq!(next_reconnect_delay(10), 20);
        assert_eq!(next_reconnect_delay(40), 80);
        assert_eq!(next_reconnect_delay(160), RECONNECT_DELAY_MAX_SECS);
        assert_eq!(next_reconnect_delay(RECONNECT_DELAY_MAX_SECS), RECONNECT_DELAY_MAX_SECS);
    }

    #[test]
    fn next_reconnect_delay_overflow_safe() {
        assert_eq!(next_reconnect_delay(u64::MAX), RECONNECT_DELAY_MAX_SECS);
    }
}
