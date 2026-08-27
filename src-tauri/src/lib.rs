// ───── Modules ─────
mod apdu_sniffer; // Passive sniffer for plaintext EF data in proxied APDUs.
mod app_connect; // Application connection to the MQTT broker.
mod backoff; // Shared exponential reconnect backoff.
mod com_port; // Card rack over the COM (serial) port.
mod commands_settings; // Settings reporting to the server.
mod config; // Configuration handling.
mod global_app_handle; // Global access to app state and emitters.
mod logger; // Logging functionality.
mod logs_upload; // Log upload to the server (fetch_logs command).
mod mqtt; // MQTT communication.
mod smart_card; // PCSC module for smart card operations.
mod updater; // Self-update from GitHub releases.

// ───── External Crates ─────
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{async_runtime, Listener, Manager, RunEvent, WindowEvent}; // Tauri application framework and async runtime.

/// Set once the tray icon is successfully created. While the tray is active,
/// closing the window only hides it and the APDU bridge keeps running in the
/// background; if the tray could not be created (e.g. a Linux desktop without
/// appindicator support), the close button keeps its original quit behavior
/// so the user is never left without a way to exit.
static TRAY_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Reports whether the app is registered to start at login. Dev builds always
/// answer "no" — see `autostart_set`.
#[tauri::command]
async fn autostart_get(app: tauri::AppHandle) -> Result<bool, String> {
    if cfg!(debug_assertions) {
        return Ok(false);
    }
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

/// Registers/unregisters the app for launch at login (registry Run key on
/// Windows, LaunchAgent on macOS, autostart .desktop on Linux). Blocked in dev
/// builds: it would pin the transient target/debug binary path into the OS
/// autostart location.
#[tauri::command]
async fn autostart_set(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    if cfg!(debug_assertions) {
        return Err("autostart is not available in dev builds".to_string());
    }
    use tauri_plugin_autostart::ManagerExt;
    let autolaunch = app.autolaunch();
    let result = if enabled {
        autolaunch.enable()
    } else {
        autolaunch.disable()
    };
    match &result {
        Ok(()) => log::info!(
            "[AUTOSTART] status={}",
            if enabled { "enabled" } else { "disabled" }
        ),
        Err(e) => log::error!("[AUTOSTART] status=failed enabled={} err={}", enabled, e),
    }
    result.map_err(|e| e.to_string())
}

/// Brings the main window back from the tray / hidden state.
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Final cleanup shared by every exit path: tray Quit, macOS Cmd+Q, window
/// close without a tray, and the updater restart. Guarded so overlapping
/// paths run it only once.
fn shutdown_cleanup() {
    static CLEANUP_DONE: AtomicBool = AtomicBool::new(false);
    if CLEANUP_DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    // Release the rack's COM port before the process winds down — a lingering
    // handle keeps the port "Access is denied" for the next launch.
    com_port::shutdown();
    // Close the app/card MQTT connections with clean DISCONNECTs so the
    // server logs normal closes instead of internal errors; bounded by the
    // shutdown flush timeout, so quitting stays snappy even when offline.
    tauri::async_runtime::block_on(mqtt::remove_connections_all());
    log::info!("-== Application is closed by user ==-\n");
}

/// Starts every backend subsystem: logging, configuration, the PC/SC card
/// monitor, the app-level MQTT connection, the rack monitor and the updater.
/// Called once from `setup()`, before the webview has loaded — card bridging is
/// then live regardless of what the frontend does.
fn initialize_backend(app_handle: tauri::AppHandle) {
    // Initialize logging (fern). Must be first so everything below logs.
    logger::setup_logging();

    // Read config.yaml (or create the default) and fill the runtime cache.
    match config::init_config() {
        Ok(_) => log::info!("Config initialized successfully."),
        Err(e) => log::error!("Failed to initialize config: {}", e),
    }

    // The smart-card monitor is a blocking PC/SC loop (get_status_change parks
    // its thread until a card event), so it runs on the blocking pool, not on
    // an async worker.
    async_runtime::spawn_blocking(|| {
        smart_card::sc_monitor();
    });

    // Start the main MQTT app client connection.
    async_runtime::spawn(async {
        app_connect::app_connection().await;
    });

    // Background task for the card rack on the COM port. Runs concurrently with
    // the PCSC reader monitor — a plugged-in reader and a connected rack work
    // in parallel.
    async_runtime::spawn(async {
        com_port::rack_connection().await;
    });

    // One-shot update check against the release endpoint.
    let updater_handle = app_handle.clone();
    async_runtime::spawn(async move {
        updater::check_for_updates(updater_handle).await;
    });

    // Unattended update loop: idle while the setting is off, otherwise checks
    // hourly and installs during a pause in card activity (an install restarts
    // the app).
    async_runtime::spawn(async move {
        updater::auto_update_loop(app_handle).await;
    });
}

/// Creates the system tray icon with its Show/Quit menu. Failure is reported
/// to the caller instead of aborting setup — the app is fully usable without
/// a tray, closing the window then simply quits as before.
fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, "show", "Show Window", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let tray_menu = Menu::with_items(app, &[&show_item, &quit_item])?;

    let mut tray = TrayIconBuilder::with_id("tba-tray")
        .tooltip("Tacho Bridge Application")
        .menu(&tray_menu)
        // macOS convention is menu on left click; on Windows the left click
        // restores the window and the menu stays on right click.
        .show_menu_on_left_click(cfg!(target_os = "macos"))
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Must be the first registered plugin. A second instance cannot work
        // anyway (exclusive COM port, duplicate MQTT client_ids kicking each
        // other) — surface the existing window instead of starting one.
        .plugin(tauri_plugin_updater::Builder::new().build())
        // Launch-at-login support; the `--minimized` argument makes an
        // auto-started instance stay in the tray instead of opening a window.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            log::warn!("Second instance launch blocked; focusing the existing window.");
            show_main_window(app);
        }))
        .setup(move |app| {
            // Obtain a lightweight reference to the app for convenient interaction
            let app_handle = app.app_handle();

            // Initialize the global application handle
            global_app_handle::set_app_handle(app_handle.clone());

            // ── One-time backend initialization ──
            //
            // Runs here, not from the webview's `frontend-loaded` event: the
            // APDU bridge is the product, and it must not depend on a webview
            // that may load late, fail to load, or (with the tray active) never
            // be shown at all — an app started with `--minimized` bridges cards
            // from the tray with no window in sight. Tauri calls setup() exactly
            // once, which is also why none of the subsystems below need their
            // own duplicate-spawn latch any more.
            initialize_backend(app_handle.clone());

            // System tray: with it active, closing the window hides the app
            // to the tray and card bridging keeps running in the background.
            // Logging is not initialized yet at this point, so eprintln is
            // the best we have for a failure.
            match setup_tray(app) {
                Ok(()) => TRAY_ACTIVE.store(true, Ordering::SeqCst),
                Err(e) => eprintln!("Tray icon unavailable, window close will quit the app: {e}"),
            }

            if let Some(window) = app.get_webview_window("main") {
                // Auto-started at login: stay in the tray instead of popping
                // the window up over whatever the user is doing. Only when the
                // tray actually exists — otherwise a hidden window would be
                // unreachable.
                if std::env::args().any(|arg| arg == "--minimized")
                    && TRAY_ACTIVE.load(Ordering::SeqCst)
                {
                    let _ = window.hide();
                }

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

                // The backend is already running (started in setup()); this
                // event only replays current state into a freshly loaded
                // webview — at startup, and again after every reload or dev
                // hot-reload.
                window.listen("frontend-loaded", move |event: tauri::Event| {
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

                    // Reader state has no replay of its own: the PCSC monitor only
                    // emits on card-state CHANGES, so a reloaded webview would show
                    // "no readers" forever while a card sits inserted. A rescan from
                    // UNAWARE makes PCSC re-report every present card through the
                    // normal pipeline.
                    smart_card::request_rescan();
                });

                // With an active tray, the close button only hides the window
                // and the APDU bridge keeps working; the real shutdown runs on
                // RunEvent::Exit (tray Quit, Cmd+Q, updater restart). Without
                // a tray, the close proceeds and quits the app as before.
                let close_window = window.clone();
                window.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        if TRAY_ACTIVE.load(Ordering::SeqCst) {
                            api.prevent_close();
                            let _ = close_window.hide();
                            log::info!("Window hidden to tray; background bridging continues.");
                        }
                    }
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            config::update_card,           // update list of cards from the frontend
            config::update_server,         // update server config from the frontend
            config::update_theme,          // persist theme from the header button
            config::remove_card,           // remove card from config
            smart_card::manual_sync_cards, // manual sync cards from the frontend
            app_connect::app_connection,   // App connection to the MQTT broker
            updater::install_update,       // download + install the pending update
            updater::check_updates_now,    // forced update check from the settings dialog
            updater::get_changelog,        // bundled CHANGELOG.md for the settings dialog
            autostart_get,                 // is the app registered to start at login
            autostart_set,                 // register/unregister launch at login
        ])
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|_app_handle, event| match event {
            // Single exit point for every quit path: tray Quit, Cmd+Q on
            // macOS, window close without a tray, updater restart.
            RunEvent::Exit => shutdown_cleanup(),
            // macOS: clicking the Dock icon while the window is hidden.
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => show_main_window(_app_handle),
            _ => {}
        });
}
