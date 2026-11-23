//! Headless run (`--headless`): the backend alone, without Tauri.
//!
//! No window, no tray, no webview: the process starts the same backend the
//! window application runs (configuration, PC/SC readers, card racks, server
//! connections and server commands) and keeps it up until it is asked to
//! stop. Nothing of Tauri's GUI stack is initialized, so the process needs no
//! display and no user session and can be started by the OS at boot, or from
//! a terminal.
//!
//! Left out, because they live in the window application: the interface, the
//! credentials prompt, launch at login and self-update. Configuration comes
//! from `config.yaml` and from the server commands.
//!
//! A headless instance and a window instance must not run at the same time:
//! the rack COM port is exclusive and two MQTT clients with one id keep
//! disconnecting each other.

use tauri::async_runtime;

/// Whether the process was started with `--headless`.
pub fn requested() -> bool {
    std::env::args().any(|arg| arg == "--headless")
}

/// Runs the backend until a termination request, then releases the racks and
/// closes the server connections in the same order as Quit does.
pub fn run() {
    crate::global_app_handle::set_headless();
    crate::initialize_backend(None);
    log::info!(
        "[HEADLESS] status=started version={}",
        env!("CARGO_PKG_VERSION")
    );
    warn_about_missing_credentials();

    async_runtime::block_on(async {
        match crate::shutdown_signal().await {
            Some(signal) => log::info!("[SHUTDOWN] phase=request signal={}", signal),
            // No way to be asked to stop: keep serving the cards until the
            // process is terminated from outside.
            None => std::future::pending::<()>().await,
        }
        crate::shutdown::prepare().await;
    });
    log::info!("-== Application is closed ==-\n");
}

/// The window application asks for credentials that are switched on but not
/// saved; here nobody can answer, and the connections would sign in
/// anonymously.
fn warn_about_missing_credentials() {
    let auth = crate::mqtt::auth_state();
    if auth.enabled && !auth.active {
        log::error!(
            "[HEADLESS] status=credentials_missing hint=\"authentication is on but no \
             username/password is saved in config.yaml\""
        );
    }
}
