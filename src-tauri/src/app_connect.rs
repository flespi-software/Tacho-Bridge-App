//! Module for working with MQTT connections.
//!
//! This module provides functionality for creating and managing MQTT connections.

// ───── Std Lib ─────
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration; // For specifying time durations.

// ───── MQTT Client Library (rumqttc) ─────
use rumqttc::v5::mqttbytes::v5::Publish;
use rumqttc::v5::{AsyncClient, Event, Incoming}; // Core MQTT event types.
use std::panic::AssertUnwindSafe;

// ───── Smart Card ─────
use crate::smart_card::TASK_POOL; // Task pool for managing MQTT connections.

// ───── Tauri ─────
use tauri::async_runtime::{self, JoinHandle}; // Async runtime and task join handles for Tauri apps.

// ───── Serialization ─────
use serde_json::Value; // For working with JSON data structures.

// ───── Local Modules ─────
use crate::backoff::Backoff;
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

/// Whether an app-level connection is registered in the task pool (the one
/// entry without a reader) — running, or in its reconnect backoff.
pub async fn is_app_connection_registered() -> bool {
    TASK_POOL
        .lock()
        .await
        .iter()
        .any(|card| card.reader_name.is_none())
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
    let panic_header = log_header.clone();
    let event_loop = async move {
        let mut is_online = false;
        let mut backoff = Backoff::new();

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
                        // Successful poll: the backoff resets once the link holds.
                        backoff.connected();
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
                            // Server input is handled inline in this loop, and
                            // nothing restarts the loop: a panic must cost that
                            // one message, not the connection.
                            let handled = std::panic::catch_unwind(AssertUnwindSafe(|| {
                                handle_publish(&mqtt_client_for_task, &log_header, &publish)
                            }));
                            if handled.is_err() {
                                log::error!(
                                    "{} [MQTT] status=publish_handler_panicked",
                                    log_header
                                );
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
                    let reconnect_delay_secs = backoff.failed();
                    crate::mqtt::log_connection_failure(
                        &log_header,
                        "CONN",
                        transition,
                        &e,
                        reconnect_delay_secs,
                    );

                    tokio::time::sleep(Duration::from_secs(reconnect_delay_secs)).await;
                }
            }
        }
    };
    // Nothing else restarts this loop (it is not under `supervise`: a reconnect
    // replaces it through this very function), and only the publish handler
    // above has its own catch_unwind — a panic in the CONNACK/offline rack
    // hooks, the status emit, the failure logging or rumqttc's poll() would
    // end the task for good with the UI still showing "online". Caught here,
    // the panic marks the connection offline and schedules a fresh one.
    let handle: JoinHandle<()> = async_runtime::spawn(async move {
        if CatchUnwind::new(event_loop).await.is_err() {
            on_event_loop_panic(&panic_header, session_id);
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

/// Delay before a panicked app connection is rebuilt: long enough that a
/// panic repeating on every connect cannot spin, short enough that the server
/// commands and the rack traffic come back without user action.
const PANIC_RESTART_DELAY: Duration = Duration::from_secs(5);

/// Cleanup after the app event loop panicked (see the spawn in
/// `app_connection`): report the connection offline, hold the racks' reports,
/// and rebuild the connection after `PANIC_RESTART_DELAY` — unless it has been
/// replaced meanwhile, in which case the successor owns all of that.
fn on_event_loop_panic(log_header: &str, session_id: u64) {
    if !is_current_app_session(session_id) {
        log::error!(
            "{} [CONN] status=eventloop_panicked superseded=true action=none",
            log_header
        );
        return;
    }
    log::error!(
        "{} [CONN] status=eventloop_panicked action=restart delay_secs={}",
        log_header,
        PANIC_RESTART_DELAY.as_secs()
    );
    // Each step on its own guard: one of them may be what just panicked, and
    // the restart below must be scheduled regardless.
    let _ = std::panic::catch_unwind(|| app_emit_event(false));
    let _ = std::panic::catch_unwind(crate::com_port::on_app_offline);
    schedule_app_restart(session_id);
}

/// Rebuilds the app connection after `PANIC_RESTART_DELAY` if `session_id` is
/// still the one in charge. A plain fn (not part of `app_connection`'s body):
/// the restart future embeds `app_connection()`'s own future, which inside
/// that body would make its type recursive.
fn schedule_app_restart(session_id: u64) {
    async_runtime::spawn(async move {
        tokio::time::sleep(PANIC_RESTART_DELAY).await;
        if is_current_app_session(session_id) {
            // Replaces the dead pool entry like any reconnect does.
            let restart: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
                Box::pin(app_connection());
            restart.await;
        }
    });
}

/// Future adapter that turns a panic while polling the inner future into an
/// `Err` (the `catch_unwind` of `futures::FutureExt`, which is not a direct
/// dependency). After a panic the inner future is never polled again.
struct CatchUnwind<F> {
    inner: std::pin::Pin<Box<F>>,
}

impl<F: std::future::Future> CatchUnwind<F> {
    fn new(inner: F) -> Self {
        Self {
            inner: Box::pin(inner),
        }
    }
}

impl<F: std::future::Future> std::future::Future for CatchUnwind<F> {
    type Output = Result<F::Output, Box<dyn std::any::Any + Send>>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let inner = self.inner.as_mut();
        match std::panic::catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Ok(std::task::Poll::Ready(output)) => std::task::Poll::Ready(Ok(output)),
            Err(panic) => std::task::Poll::Ready(Err(panic)),
        }
    }
}

/// Routes one publish of the app connection: rack traffic, or a server command.
fn handle_publish(mqtt_client: &AsyncClient, log_header: &str, publish: &Publish) {
    let topic = match std::str::from_utf8(&publish.topic) {
        Ok(topic) => topic,
        Err(e) => {
            log::error!("{} invalid UTF-8 in publish topic: {:?}", log_header, e);
            return;
        }
    };

    // Debug log: when the server sent it (its `timestamp` user property)
    // against when it got here — the MQTT transit part of a slow exchange.
    log::debug!(
        "{} [MQTT] rx topic={} bytes={} pkid={} server_ts={}",
        log_header,
        topic,
        publish.payload.len(),
        publish.pkid,
        crate::debug_log::server_timestamp(publish)
    );

    // Rack traffic rides this connection under the `rack/<id>/` prefix; its
    // payloads are handled as raw bytes by the rack module.
    if crate::com_port::handle_app_publish(mqtt_client, topic, &publish.payload) {
        return;
    }

    // server command requests come as JSON publishes on request/<id>/0
    match serde_json::from_slice::<Value>(&publish.payload) {
        Ok(json_payload) => {
            // Never the payload itself: set_credentials carries the password,
            // and the log file is uploaded by fetch_logs.
            log::debug!(
                "{} server command name={}",
                log_header,
                json_payload.get("name").unwrap_or(&Value::Null)
            );
            let handled = crate::commands_settings::dispatch_request(
                mqtt_client,
                log_header,
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
            log::error!("{} parsing JSON payload issue: {:?}", log_header, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn catch_unwind_passes_output_and_catches_panics() {
        assert_eq!(CatchUnwind::new(async { 7 }).await.ok(), Some(7));

        // A panic after an await point (a later poll, like the event loop's)
        // is caught too.
        let panicking = CatchUnwind::new(async {
            tokio::task::yield_now().await;
            panic!("event loop panic");
        });
        assert!(panicking.await.is_err());
    }
}
