//! Asynchronous exit preparation keeps the webview alive for shutdown progress.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;
use tauri::Emitter;

static EXIT_STARTED: AtomicBool = AtomicBool::new(false);
static EXIT_READY: AtomicBool = AtomicBool::new(false);
static PREPARED: AtomicBool = AtomicBool::new(false);
static CLEANUP_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static STATE: Mutex<Option<Progress>> = Mutex::new(None);

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Indicators,
    Connections,
    Installing,
}

#[derive(Clone, serde::Serialize)]
pub struct ShutdownState {
    phase: Phase,
    slots: usize,
    estimated_ms: u64,
    elapsed_ms: u64,
}

struct Progress {
    started: Instant,
    state: ShutdownState,
}

#[tauri::command]
pub fn get_shutdown_state() -> Option<ShutdownState> {
    STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(|progress| {
            let mut state = progress.state.clone();
            state.elapsed_ms = progress.started.elapsed().as_millis() as u64;
            state
        })
}

fn emit_state() {
    if let Some(app) = crate::global_app_handle::get_app_handle() {
        if let Err(e) = app.emit("app-shutdown", get_shutdown_state()) {
            log::warn!("[SHUTDOWN] status=progress_emit_failed err={}", e);
        }
    }
}

pub fn set_phase(phase: Phase) {
    if let Some(progress) = STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
    {
        progress.state.phase = phase;
    }
    emit_state();
}

/// True from the start of `prepare` until the exit or `reset_after_failed_update`.
pub fn in_progress() -> bool {
    STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some()
}

pub fn is_prepared() -> bool {
    PREPARED.load(Ordering::SeqCst)
}

pub fn is_exiting() -> bool {
    EXIT_STARTED.load(Ordering::SeqCst)
}

pub fn exit_ready() -> bool {
    EXIT_READY.load(Ordering::SeqCst)
}

/// How long an exit waits for an update install in progress before leaving anyway.
const INSTALL_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

pub fn request_exit(app: tauri::AppHandle, code: i32) {
    if EXIT_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    // A Quit from the tray must make its progress visible too.
    crate::show_main_window(&app);
    tauri::async_runtime::spawn(async move {
        // Run as its own task: a panic in the cleanup must still let the app exit.
        if let Err(e) = tauri::async_runtime::spawn(prepare()).await {
            log::error!("[SHUTDOWN] status=prepare_failed err={}", e);
        }
        // An update being installed finishes replacing the bundle first.
        let waiting = Instant::now();
        while crate::updater::is_installing() && waiting.elapsed() < INSTALL_WAIT {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        EXIT_READY.store(true, Ordering::SeqCst);
        app.exit(code);
    });
}

/// Shared by Quit and the updater, including an overlapping Quit during install.
pub async fn prepare() {
    let _cleanup = CLEANUP_LOCK.lock().await;
    if is_prepared() {
        return;
    }
    let estimate = crate::com_port::shutdown_estimate();
    *STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Progress {
        started: Instant::now(),
        state: ShutdownState {
            phase: Phase::Indicators,
            slots: estimate.slots,
            estimated_ms: estimate.estimated_ms,
            elapsed_ms: 0,
        },
    });
    log::info!(
        "[SHUTDOWN] phase=begin slots={} estimated_ms={}",
        estimate.slots,
        estimate.estimated_ms
    );
    emit_state();
    crate::com_port::shutdown().await;
    set_phase(Phase::Connections);
    crate::mqtt::remove_connections_all().await;
    PREPARED.store(true, Ordering::SeqCst);
}

/// An unsuccessful install keeps the application running and dismisses progress.
/// A user-requested exit takes precedence over recovering the failed update.
pub async fn reset_after_failed_update() -> bool {
    let _cleanup = CLEANUP_LOCK.lock().await;
    if is_exiting() {
        return false;
    }
    PREPARED.store(false, Ordering::SeqCst);
    *STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    emit_state();
    true
}
