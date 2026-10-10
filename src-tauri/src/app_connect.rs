//! Module for working with MQTT connections.
//!
//! This module provides functionality for creating and managing MQTT connections.

// ───── Std Lib ─────
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration; // For specifying time durations.

// ───── MQTT Client Library (rumqttc) ─────
use rumqttc::v5::{Event, Incoming}; // Core MQTT event types.

// ───── Smart Card ─────
use crate::smart_card::TASK_POOL; // Task pool for managing MQTT connections.

// ───── Tauri ─────
use tauri::async_runtime::{self, JoinHandle}; // Async runtime and task join handles for Tauri apps.

// ───── Serialization ─────
use serde_json::Value; // For working with JSON data structures.

// ───── Local Modules ─────
use crate::backoff::{next_reconnect_delay, RECONNECT_DELAY_INITIAL_SECS};
use crate::config::get_from_cache; // Function to get data from cache for syncing server data.
use crate::config::split_host_to_parts; // Function to split the host into parts for MQTT connection.
use crate::config::CacheSection; // Enum for cache sections for getting data from cache.
use crate::global_app_handle::app_emit_event; // Emits app connection status to the frontend.
use crate::smart_card::ProcessingCard;

/// Session id of the app connection currently in charge. A replaced loop can
/// still be alive for a moment (its shutdown is detached and may race its own
/// CONNECT): only the current one may register its client with the racks,
/// report the status to the UI, or announce the racks on CONNACK.
static CURRENT_APP_SESSION: AtomicU64 = AtomicU64::new(0);

fn is_current_app_session(session_id: u64) -> bool {
    CURRENT_APP_SESSION.load(Ordering::SeqCst) == session_id
}

/// Rebuilds every MQTT connection with the current settings — what the
/// settings dialog does after a save, available to the backend for changes
/// that arrive from the server (credentials). Each step on its own: a PC/SC
/// failure must not keep the app connection from moving.
pub async fn reconnect_everything(reason: &str) {
    log::info!("[CONN] phase=reconnect_all reason={}", reason);
    if let Err(e) = crate::smart_card::manual_sync_cards(String::new(), true).await {
        log::warn!(
            "[CONN] phase=reconnect_all step=cards status=failed err={}",
            e
        );
    }
    app_connection().await;
    crate::com_port::restart_rack_links(reason);
}

/// (Re)establishes the application-level MQTT connection with the current
/// settings, replacing the running one.
#[tauri::command]
pub async fn app_connection() {
    log::info!("[CONN] phase=app_connection status=start");

    // Getting server data from the cache
    let full_host = get_from_cache(CacheSection::Server, "host");
    let (host, port) = match split_host_to_parts(&full_host) {
        Ok((host, port)) => {
            log::debug!(
                "[CONN] phase=config status=host_loaded host={}:{}",
                host,
                port
            );
            (host, port)
        }
        Err(e) => {
            log::error!(
                "[CONN] phase=config status=failed reason=invalid_host err={}",
                e
            );
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

    // Take over before the predecessor is told to close, so its teardown
    // already sees itself superseded.
    let session_id = crate::smart_card::next_session_id();
    CURRENT_APP_SESSION.store(session_id, Ordering::SeqCst);

    // The app-level connection is the only pool entry without a reader. If one
    // already exists (possibly stuck deep in reconnect backoff, or registered
    // under a previous ident), replace it: close the old task and start fresh,
    // so a manual reconnect acts immediately instead of being silently skipped.
    if let Some(index) = task_pool.iter().position(|card| card.reader_name.is_none()) {
        let old = task_pool.remove(index);
        log::info!(
            "[CONN] phase=app_connection status=replacing_existing old_client_id={} client_id={}",
            old.client_id,
            client_id
        );
        // Close gracefully (clean MQTT DISCONNECT); detached so the new
        // connection is not delayed behind the old one's shutdown.
        async_runtime::spawn(crate::mqtt::shutdown_connections(
            vec![old],
            crate::mqtt::SHUTDOWN_REASON_APP_CONNECTION_REPLACED,
        ));
    }

    //////////////////////////////////////////////////
    //  Create a new client ID for the MQTT connection
    //////////////////////////////////////////////////
    log::info!(
        "[CONN] phase=connect_attempt status=initialized client_id={} host={}:{}",
        client_id,
        host,
        port
    );
    let (mqtt_client, mut eventloop) =
        crate::mqtt::build_mqtt_client(client_id.clone(), &host, port);
    // The racks publish through the app connection (`rack/<id>/...`): hand
    // them the new client right away, so a rack linking before the first
    // CONNACK queues its report into the right connection.
    crate::com_port::register_app_client(&mqtt_client);
    let mqtt_clinet_cloned = mqtt_client.clone();
    let mqtt_client_for_task = mqtt_client.clone();
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
                    if !is_current_app_session(session_id) {
                        // Replaced: the successor owns the broker session, the
                        // racks' client and the UI status, so nothing here is
                        // acted on. Keep polling until the detached shutdown's
                        // DISCONNECT is flushed — unless this loop has just
                        // won a CONNECT, which would take the session over
                        // from its successor: drop it right away then.
                        match notification {
                            Event::Outgoing(rumqttc::Outgoing::Disconnect)
                            | Event::Incoming(Incoming::ConnAck(..)) => {
                                log::info!(
                                    "{} [CONN] phase=shutdown status=superseded",
                                    log_header
                                );
                                break;
                            }
                            _ => continue,
                        }
                    }
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

                    // The full notification embeds publish payloads (rack
                    // envelopes included): trace only, never in the debug log.
                    log::trace!("App {} Notification: {:?}", log_header, notification);

                    match notification {
                        Event::Incoming(Incoming::Publish(publish)) => {
                            let topic = match std::str::from_utf8(&publish.topic) {
                                Ok(topic) => topic,
                                Err(e) => {
                                    log::error!(
                                        "{} invalid UTF-8 in publish topic: {:?}",
                                        log_header,
                                        e
                                    );
                                    continue;
                                }
                            };

                            // Debug log: when the server sent it (its
                            // `timestamp` user property) against when it got
                            // here — the MQTT transit part of a slow exchange.
                            log::debug!(
                                "{} [MQTT] rx topic={} bytes={} pkid={} server_ts={}",
                                log_header,
                                topic,
                                publish.payload.len(),
                                publish.pkid,
                                crate::debug_log::server_timestamp(&publish)
                            );

                            // Rack traffic rides this connection under the
                            // `rack/<id>/` prefix; its payloads are handled as
                            // raw bytes by the rack module.
                            if crate::com_port::handle_app_publish(
                                &mqtt_client_for_task,
                                topic,
                                &publish.payload,
                            ) {
                                continue;
                            }

                            // server command requests come as JSON publishes on request/<id>/0
                            match serde_json::from_slice::<Value>(&publish.payload) {
                                Ok(json_payload) => {
                                    // Never the payload itself: set_credentials carries
                                    // the password, and the log file is uploaded by
                                    // fetch_logs.
                                    log::debug!(
                                        "{} server command name={}",
                                        log_header,
                                        json_payload.get("name").unwrap_or(&Value::Null)
                                    );
                                    let handled = crate::commands_settings::dispatch_request(
                                        &mqtt_client_for_task,
                                        &log_header,
                                        topic,
                                        &json_payload,
                                    );
                                    if !handled {
                                        log::warn!(
                                            "{} unsupported publish topic={} name={} bytes={}",
                                            log_header,
                                            topic,
                                            json_payload.get("name").unwrap_or(&Value::Null),
                                            publish.payload.len()
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
                            // The OFFLINE->ONLINE transition is already logged
                            // at info; CONNACK itself is a detail.
                            log::debug!("{} [CONN] event=CONNACK status=received", log_header);
                            // One-shot settings report per established connection.
                            // Spawned, never awaited here: publish() parks when the
                            // client's bounded request channel is full (e.g. log-upload
                            // chunks buffered during a network blip), and that channel
                            // is drained only by the poll() this loop must return to —
                            // awaiting would deadlock the connection permanently.
                            let client = mqtt_client_for_task.clone();
                            let header = log_header.clone();
                            async_runtime::spawn(async move {
                                crate::commands_settings::publish_settings_report(&client, &header)
                                    .await;
                            });
                            // The server runs a fresh instance per connection:
                            // every linked rack is announced again (`link up`)
                            // so its discovery restarts. Spawned inside, for
                            // the same reason as the settings report.
                            crate::com_port::on_app_connack(&mqtt_client_for_task);
                        }
                        Event::Outgoing(rumqttc::Outgoing::Disconnect) => {
                            // graceful teardown: the DISCONNECT packet is already flushed to the
                            // socket, so exit instead of letting the loop treat the closing
                            // connection as a network failure and reconnect
                            log::info!(
                                "{} [CONN] phase=shutdown status=disconnect_sent",
                                log_header
                            );
                            if is_current_app_session(session_id) {
                                // Closed without a successor (yet): the racks
                                // must hold their reports, and the UI must
                                // not show a closed link as online.
                                crate::com_port::on_app_offline();
                                if is_online {
                                    app_emit_event(false);
                                }
                            }
                            break;
                        }
                        _ => {} // This handles any other events that you haven't explicitly matched above
                    }
                }
                Err(e) => {
                    if !is_current_app_session(session_id) {
                        // A replaced connection has nothing to reconnect for.
                        log::info!(
                            "{} [CONN] phase=shutdown status=superseded err={}",
                            log_header,
                            e
                        );
                        break;
                    }
                    let transition = if is_online {
                        "ONLINE->OFFLINE"
                    } else {
                        "OFFLINE"
                    };
                    // The cached status too: a manual reconnect replaces an
                    // online connection without an offline report, and this
                    // fresh loop has never been online — the UI would keep
                    // showing the old "online" through every failed retry.
                    if is_online || crate::global_app_handle::get_app_connection_status() {
                        is_online = false;
                        app_emit_event(false);
                    }
                    // The racks publish through this connection: hold their
                    // reports until the next CONNACK, which announces them all.
                    crate::com_port::on_app_offline();

                    // One line per failed poll: kind + retry delay; full error
                    // details only for genuinely unexpected failures.
                    crate::mqtt::log_connection_failure(
                        &log_header,
                        "CONN",
                        transition,
                        &e,
                        reconnect_delay_secs,
                    );

                    tokio::time::sleep(Duration::from_secs(reconnect_delay_secs)).await;
                    reconnect_delay_secs = next_reconnect_delay(reconnect_delay_secs);
                }
            }
        }
    });

    task_pool.push(ProcessingCard {
        client_id,
        session_id,
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
