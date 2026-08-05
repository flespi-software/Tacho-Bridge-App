//! Self-update via `tauri-plugin-updater`.
//!
//! Flow: `check_for_updates` runs once at startup (and again on demand via the
//! `check_updates_now` command or a channel switch), asks the channel's
//! manifest endpoint and, when a newer signed build exists, stores it and
//! notifies the frontend. The user clicks "Install" → the frontend invokes
//! `install_update` → download, signature check, install, restart.

use tauri::AppHandle;
use tauri_plugin_updater::UpdaterExt;
use tokio::sync::Mutex;

use crate::global_app_handle::{emit_notification_event, NotificationPayload};

/// The update found by the last check, waiting for the user to accept it.
static PENDING_UPDATE: Mutex<Option<tauri_plugin_updater::Update>> = Mutex::const_new(None);

/// Log tag prefix, mirrors the `[RACK]`/`[CONN]` style used elsewhere.
const TAG: &str = "[UPDATER]";

/// Manifest for the pre-release channel: a rolling release the CI updates on
/// every build (stable or pre-release). The default endpoint in tauri.conf.json
/// (`releases/latest/download/latest.json`) is the stable channel — GitHub
/// keeps it pointed at the newest non-prerelease release.
const BETA_MANIFEST_URL: &str =
    "https://github.com/flespi-software/Tacho-Bridge-App/releases/download/updater-beta/latest.json";

/// Outcome of a manifest check, consumed by the settings dialog.
#[derive(Clone, serde::Serialize)]
pub struct CheckResult {
    /// "update_available" | "up_to_date"
    pub status: &'static str,
    /// The available version, or the running one when already up to date.
    pub version: String,
}

/// Runs one check against the channel endpoint. On an available update the
/// frontend is notified (`update` notification with the Install action) and
/// the update is parked in `PENDING_UPDATE` for `install_update`.
/// `beta_override` lets the settings dialog check the channel currently
/// selected on screen, even before it has been saved; `None` uses the
/// persisted setting.
async fn perform_check(
    app: &AppHandle,
    beta_override: Option<bool>,
) -> Result<CheckResult, String> {
    let beta = beta_override.unwrap_or_else(|| {
        crate::config::get_from_cache(crate::config::CacheSection::Updates, "beta_updates")
            == "true"
    });

    let updater = if beta {
        let url = BETA_MANIFEST_URL
            .parse()
            .map_err(|e| format!("bad beta manifest url: {e}"))?;
        app.updater_builder()
            .endpoints(vec![url])
            .map_err(|e| e.to_string())?
            .build()
            .map_err(|e| e.to_string())?
    } else {
        app.updater().map_err(|e| e.to_string())?
    };

    log::info!(
        "{TAG} phase=check status=start channel={}",
        if beta { "beta" } else { "stable" }
    );

    match updater.check().await {
        Ok(Some(update)) => {
            let version = update.version.clone();
            log::info!(
                "{TAG} phase=check status=update_available current={} available={}",
                env!("CARGO_PKG_VERSION"),
                version
            );
            let message = format!(
                "Version {version} is available. Install it now? The application will restart."
            );
            *PENDING_UPDATE.lock().await = Some(update);
            emit_notification_event(
                "global-notification",
                NotificationPayload {
                    notification_type: "update".to_string(),
                    message,
                },
            );
            Ok(CheckResult {
                status: "update_available",
                version,
            })
        }
        Ok(None) => {
            log::info!(
                "{TAG} phase=check status=up_to_date current={}",
                env!("CARGO_PKG_VERSION")
            );
            Ok(CheckResult {
                status: "up_to_date",
                version: env!("CARGO_PKG_VERSION").to_string(),
            })
        }
        Err(e) => {
            // Expected until the first release of the channel ships a manifest.
            log::warn!("{TAG} phase=check status=failed err={e}");
            Err(e.to_string())
        }
    }
}

/// Startup / background variant: outcome only matters via logs and the
/// frontend notification, errors are swallowed.
pub async fn check_for_updates(app: AppHandle) {
    let _ = perform_check(&app, None).await;
}

/// Forced check from the settings dialog. Returns the outcome so the dialog
/// can tell the user "you are up to date" explicitly. The dialog passes its
/// on-screen channel toggle so the check honors it even before Save.
#[tauri::command]
pub async fn check_updates_now(
    app: AppHandle,
    beta_updates: Option<bool>,
) -> Result<CheckResult, String> {
    log::info!("{TAG} phase=check status=manual_trigger");
    perform_check(&app, beta_updates).await
}

/// The changelog bundled into this build — the settings dialog renders it.
#[tauri::command]
pub fn get_changelog() -> &'static str {
    include_str!("../../CHANGELOG.md")
}

/// Downloads and installs the pending update, then restarts the application.
/// Invoked from the frontend when the user accepts the update notification.
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    let update = PENDING_UPDATE
        .lock()
        .await
        .take()
        .ok_or_else(|| "no pending update".to_string())?;

    log::info!(
        "{TAG} phase=install status=downloading version={}",
        update.version
    );

    let mut downloaded: usize = 0;
    let mut last_logged_pct: u64 = 0;
    update
        .download_and_install(
            move |chunk, total| {
                downloaded += chunk;
                if let Some(total) = total {
                    let pct = (downloaded as u64 * 100) / total.max(1);
                    // One log line per ~25% keeps the log readable.
                    if pct >= last_logged_pct + 25 {
                        last_logged_pct = pct;
                        log::info!("{TAG} phase=install status=downloading progress={pct}%");
                    }
                }
            },
            || log::info!("{TAG} phase=install status=downloaded"),
        )
        .await
        .map_err(|e| {
            log::error!("{TAG} phase=install status=failed err={e}");
            e.to_string()
        })?;

    log::info!("{TAG} phase=install status=installed action=restart");
    // Release the rack's serial port before the restart, same as window close.
    crate::com_port::shutdown();
    app.restart();
}
