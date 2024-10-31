// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// Module imports
mod app_connect;
mod config; // Configuration handling.
mod logger; // Logging functionality.
mod mqtt; // MQTT communication.
mod smart_card; // PCSC module for smart card operations. // Application connection to the MQTT broker.

// External crate imports
use tauri::{async_runtime, Manager, WindowEvent}; // Tauri application framework and async runtime.

mod global_app_handle;

fn main() {
    // Initialize logging. This function configures the logging system using the `fern` crate.
    // need to debug later. Add checking for the init result
    //
    logger::setup_logging();
    // Log the application launch
    log::info!("-== Application is launched ==-");

    // Initialize configuration. This function reads the configuration file and initializes the configuration structure.
    // The configuration file is located in the `assets` directory and is named `config.yaml`.
    match config::init_config() {
        Ok(_) => log::info!("Config initialized successfully."),
        Err(e) => {
            log::error!("Failed to initialize config: {}", e);
        }
    }

    // start builder to run tauri applicationrustup target add aarch64-pc-windows-msvc
    tauri::Builder::default()
        .setup(|app| {
            // Obtain a lightweight reference to the app for convenient interaction
            let app_handle = app.app_handle();

            // Initialize the global application handle
            global_app_handle::set_app_handle(app.handle());

            if let Some(window) = app.get_window("main") {
                // getting Application version foriom the Cargo.toml file
                let version = env!("CARGO_PKG_VERSION");
                // Form new Title with the version
                let title = format!("v{}", version);
                // Set new title to the window
                window
                    .set_title(&title)
                    .expect("Failed to set window title");

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

                    println!("Received event with payload: {:?}", event.payload());
                    // Load server configuration from cache to frontend using event
                    match config::emit_global_config_server(&front_app_handle) {
                        Ok(_) => {
                            println!("Global config server emitted successfully.");
                        }
                        Err(e) => {
                            println!("Failed to emit global config server: {:?}", e);
                        }
                    }

                    // Run async function in the background with the Tauri runtime
                    // let app_handle_for_sc_monitor = app_handle.clone();
                    async_runtime::spawn(async {
                        /*
                            This slip is needed as a temporary solution. Fix it later!
                            The fact is that the back-end starts faster than the front, and the sent event with card data arrives at the front-end before it has time to load.
                            *** In the near future, I will add a flag for the state of readiness to receive events from the backend. ***
                        */
                        // Start monitoring smart cards. This function will run forever with the loop
                        smart_card::sc_monitor().await;
                    });
                });

                // Handle the application close event to log this.
                window.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { .. } = event {
                        log::info!("-== Application is closed by user ==-\n");
                    }
                });
            }

            async_runtime::spawn(async {
                // Start Main MQTT App client connection
                app_connect::app_connection().await;
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            config::update_card,           // update list of cards from the frontend
            config::update_server,         // update server config from the frontend
            smart_card::manual_sync_cards, // manual sync cards from the frontend
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
