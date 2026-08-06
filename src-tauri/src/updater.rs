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

/// How often the auto-install loop wakes up to look at its state. Cheap: no
/// network unless a check is due, so the loop reacts to the settings toggle
/// and to card activity going quiet within minutes.
const AUTO_TICK: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Minimal spacing between two network checks of the manifest endpoint.
/// Releases ship at most a few times a day — polling GitHub more often than
/// hourly buys nothing and just burns traffic.
const AUTO_CHECK_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// The card link must be this quiet before an unattended install may restart
/// the app. An active authentication is a continuous APDU flow with sub-minute
/// gaps, so one silent minute means no session is in progress.
const AUTO_INSTALL_QUIET_SECS: u64 = 60;

/// Unattended update loop, spawned once at startup and running for the whole
/// app lifetime. Does nothing while the `auto_install_updates` setting is off.
/// When on: re-checks the channel manifest at most once an hour, and installs
/// a found update (parked in `PENDING_UPDATE` by the shared check path) as
/// soon as the card link has been quiet for `AUTO_INSTALL_QUIET_SECS` — an
/// install restarts the app, and a restart mid-authentication would break the
/// VU's session. While cards stay busy the install is re-attempted every tick.
pub async fn auto_update_loop(app: AppHandle) {
    // The startup check in lib.rs has just run; start the hourly clock now.
    let mut last_network_check = std::time::Instant::now();
    loop {
        tokio::time::sleep(AUTO_TICK).await;

        let enabled = crate::config::get_from_cache(
            crate::config::CacheSection::Updates,
            "auto_install_updates",
        ) == "true";
        if !enabled {
            continue;
        }

        // Look for a new version when the hourly budget allows and nothing is
        // already waiting to be installed.
        if PENDING_UPDATE.lock().await.is_none()
            && last_network_check.elapsed() >= AUTO_CHECK_MIN_INTERVAL
        {
            last_network_check = std::time::Instant::now();
            let _ = perform_check(&app, None).await;
        }

        // Install the parked update once the card link is quiet.
        let (pending, version) = {
            let guard = PENDING_UPDATE.lock().await;
            match guard.as_ref() {
                Some(update) => (true, update.version.clone()),
                None => (false, String::new()),
            }
        };
        if !pending {
            continue;
        }
        let quiet_for = crate::mqtt::seconds_since_card_activity();
        if quiet_for < AUTO_INSTALL_QUIET_SECS {
            log::info!(
                "{TAG} phase=auto_install status=postponed version={} reason=card_activity quiet_secs={}",
                version,
                quiet_for
            );
            continue;
        }

        log::info!("{TAG} phase=auto_install status=starting version={version}");
        emit_notification_event(
            "global-notification",
            NotificationPayload {
                notification_type: "version".to_string(),
                message: format!(
                    "Installing update {version} — the application will restart shortly."
                ),
            },
        );
        // On success this restarts the app and never returns; on failure the
        // update is put back into PENDING_UPDATE and the next tick retries.
        if let Err(e) = install_update(app.clone()).await {
            log::error!("{TAG} phase=auto_install status=failed version={version} err={e}");
        }
    }
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
    if let Err(e) = update
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
    {
        log::error!("{TAG} phase=install status=failed err={e}");
        // Put the update back: the slot was take()n above, and without this a
        // transient download failure would consume it — the user could never
        // retry the install until a fresh update check found it again.
        *PENDING_UPDATE.lock().await = Some(update);
        return Err(e.to_string());
    }

    log::info!("{TAG} phase=install status=installed action=restart");
    // Release the rack's serial port before the restart, same as window close.
    crate::com_port::shutdown();
    app.restart();
}
