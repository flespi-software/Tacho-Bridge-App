#[cfg_attr(mobile, tauri::mobile_entry_point)]

// ───── Modules ─────
mod app_connect;        // Application connection to the MQTT broker.
mod config;             // Configuration handling.
mod logger;             // Logging functionality.
mod mqtt;               // MQTT communication.
mod smart_card;         // PCSC module for smart card operations.
mod apdu_sniffer;       // Passive sniffer for plaintext EF data in proxied APDUs.
mod global_app_handle;  // Global access to app state and emitters.
mod com_port;           // Card rack over the COM (serial) port.

// ───── External Crates ─────
use tauri::{async_runtime, Listener, Manager, WindowEvent}; // Tauri application framework and async runtime.

pub fn run() {
    // start builder to run tauri applicationrustup target add aarch64-pc-windows-msvc
    tauri::Builder::default()
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

                    // Initialize logging. This function configures the logging system using the `fern` crate.
                    // need to debug later. Add checking for the init result
                    //
                    logger::setup_logging();

                    // Initialize configuration. This function reads the configuration file and initializes the configuration structure.
                    // The configuration file is located in the `assets` directory and is named `config.yaml`.
                    match config::init_config() {
                        Ok(_) => log::info!("Config initialized successfully."),
                        Err(e) => log::error!("Failed to initialize config: {}", e),
                    }

                    println!("Received event with payload: {:?}", event.payload());
                    // Load server configuration from cache to frontend using event
                    match config::emit_global_config_server(&front_app_handle) {
                        Ok(_) => println!("Global config server emitted successfully."),
                        Err(e) => println!("Failed to emit global config server: {:?}", e),
                    }

                    // Re-emit the last known rack state: the rack monitor runs
                    // independently and may have already reported the rack before
                    // the frontend subscribed, so a fresh load needs it replayed.
                    global_app_handle::emit_current_rack_state();

                    // The smart-card monitor is a blocking PC/SC loop
                    // (get_status_change parks its thread until a card event),
                    // so it runs on the blocking pool, not on an async worker.
                    // Duplicate spawns from repeated `frontend-loaded` events
                    // are ignored inside sc_monitor itself.
                    async_runtime::spawn_blocking(|| {
                        smart_card::sc_monitor();
                    });

                    async_runtime::spawn(async {
                        // Start Main MQTT App client connection
                        app_connect::app_connection().await;
                    });

                    // Spawn a background task for the card rack on the COM port.
                    // Runs concurrently with the PCSC reader monitor above — a
                    // plugged-in reader and a connected rack work in parallel.
                    async_runtime::spawn(async {
                        com_port::rack_connection().await;
                    });
                });

                // Handle the application close event to log this.
                window.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { .. } = event {
                        log::info!("-== Application is closed by user ==-\n");
                    }
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            config::update_card,           // update list of cards from the frontend
            config::update_server,         // update server config from the frontend
            config::remove_card,            // remove card from config
            smart_card::manual_sync_cards, // manual sync cards from the frontend
            app_connect::app_connection,     // App connection to the MQTT broker
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
