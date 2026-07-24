#[cfg_attr(mobile, tauri::mobile_entry_point)]

// ───── Modules ─────
mod app_connect;        // Application connection to the MQTT broker.
mod commands_settings;  // Settings reporting to the server.
mod config;             // Configuration handling.
mod logger;             // Logging functionality.
mod mqtt;               // MQTT communication.
mod smart_card;         // PCSC module for smart card operations.
mod apdu_sniffer;       // Passive sniffer for plaintext EF data in proxied APDUs.
mod global_app_handle;  // Global access to app state and emitters.
mod com_port;           // Card rack over the COM (serial) port.
mod updater;            // Self-update from GitHub releases.

// ───── External Crates ─────
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{async_runtime, Listener, Manager, WindowEvent}; // Tauri application framework and async runtime.

/// `frontend-loaded` fires on every webview (re)load — dev hot-reload, manual
/// refresh, sometimes twice at startup. The one-time backend initialization
/// (logging, config, background tasks) must only run for the first one.
static BACKEND_INITIALIZED: AtomicBool = AtomicBool::new(false);

pub fn run() {
    // start builder to run tauri applicationrustup target add aarch64-pc-windows-msvc
    tauri::Builder::default()
        // Must be the first registered plugin. A second instance cannot work
        // anyway (exclusive COM port, duplicate MQTT client_ids kicking each
        // other) — surface the existing window instead of starting one.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            log::warn!("Second instance launch blocked; focusing the existing window.");
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .setup(move |app| {
            // Obtain a lightweight reference to the app for convenient interaction
            let app_handle = app.app_handle();

            // Initialize the global application handle
            global_app_handle::set_app_handle(app_handle.clone());

            if let Some(window) = app.get_webview_window("main") {
                // getting Application version foriom the Cargo.toml file
                let version = env!("CARGO_PKG_VERSION");
                // Form new Title with the version
                let title = format!("v{}", version);
                // Set new title to the window. A failure here is cosmetic —
                // not worth panicking the whole app over (logging is not yet
                // initialized at this point, so eprintln is the best we have).
                if let Err(e) = window.set_title(&title) {
                    eprintln!("Failed to set window title: {e}");
                }

                let front_app_handle = app_handle.clone();

                // Frontend loading is late, so we execute a callback to the "frontend-loaded" event which the front sends when it is loaded
                window.listen("frontend-loaded", move |event: tauri::Event| {
                    #[cfg(target_os = "linux")]
                    {   // Temporary solution only for linux because webview does not load even after response from front.
                        // Apparently loading occurs later, not like Windows and MacOS. Fix later.
                        std::thread::sleep(std::time::Duration::from_millis(300));
                    }
                    #[cfg(target_os = "windows")] {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                    }

                    // ── One-time backend initialization ──
                    if !BACKEND_INITIALIZED.swap(true, Ordering::SeqCst) {
                        // Initialize logging (fern). Must be first so everything below logs.
                        logger::setup_logging();

                        // Read config.yaml (or create the default) and fill the runtime cache.
                        match config::init_config() {
                            Ok(_) => log::info!("Config initialized successfully."),
                            Err(e) => log::error!("Failed to initialize config: {}", e),
                        }

                        // The smart-card monitor is a blocking PC/SC loop
                        // (get_status_change parks its thread until a card event),
                        // so it runs on the blocking pool, not on an async worker.
                        async_runtime::spawn_blocking(|| {
                            smart_card::sc_monitor();
                        });

                        async_runtime::spawn(async {
                            // Start Main MQTT App client connection
                            app_connect::app_connection().await;
                        });

                        // Background task for the card rack on the COM port. Runs
                        // concurrently with the PCSC reader monitor — a plugged-in
                        // reader and a connected rack work in parallel.
                        async_runtime::spawn(async {
                            com_port::rack_connection().await;
                        });

                        // One-shot update check against the release endpoint.
                        let updater_handle = front_app_handle.clone();
                        async_runtime::spawn(async move {
                            updater::check_for_updates(updater_handle).await;
                        });
                    }

                    // ── Per-(re)load: replay current state to the fresh frontend ──
                    log::debug!("frontend-loaded: payload={:?}", event.payload());

                    match config::emit_global_config_server(&front_app_handle) {
                        Ok(_) => log::debug!("Global config server emitted successfully."),
                        Err(e) => log::error!("Failed to emit global config server: {:?}", e),
                    }

                    // Card list for the SmartCardList component.
                    config::emit_all_card_configs();

                    // Last known rack state: the rack monitor runs independently
                    // and may have reported the rack before the frontend subscribed.
                    global_app_handle::emit_current_rack_state();
                });

                // Handle the application close event: release the rack's COM
                // port before the process winds down — a lingering handle
                // keeps the port "Access is denied" for the next launch.
                window.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { .. } = event {
                        com_port::shutdown();
                        log::info!("-== Application is closed by user ==-\n");
                    }
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            config::update_card,           // update list of cards from the frontend
            config::update_server,         // update server config from the frontend
            config::update_theme,          // persist theme from the header button
            config::remove_card,            // remove card from config
            smart_card::manual_sync_cards, // manual sync cards from the frontend
            app_connect::app_connection,     // App connection to the MQTT broker
            updater::install_update,         // download + install the pending update
            updater::check_updates_now,      // forced update check from the settings dialog
            updater::get_changelog,          // bundled CHANGELOG.md for the settings dialog
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
