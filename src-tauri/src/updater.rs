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

/// The update found by the last check, waiting for the user to accept it,
/// together with the channel it was found on.
static PENDING_UPDATE: Mutex<Option<PendingUpdate>> = Mutex::const_new(None);

/// A parked update plus the bookkeeping the auto-install loop needs.
struct PendingUpdate {
    update: tauri_plugin_updater::Update,
    /// Channel this update came from. A parked pre-release must never be
    /// installed after the user switches back to stable, so a channel mismatch
    /// discards it instead of installing it.
    beta: bool,
    /// Consecutive failed install attempts, used to back off and eventually
    /// give up on a permanently broken artifact (bad signature, truncated
    /// asset) instead of re-downloading it every tick forever.
    failed_attempts: u32,
    /// Monotonic time of the last failed attempt; the next retry waits for the
    /// backoff derived from `failed_attempts`.
    last_failure: Option<std::time::Instant>,
}

/// After this many consecutive failures the parked update is dropped. A fresh
/// network check can still find it again later (hourly at most), so a genuinely
/// transient outage is not fatal — this only stops the tight retry loop.
const MAX_INSTALL_ATTEMPTS: u32 = 5;

/// Backoff before retrying a failed install: 10 min, 20, 40, 80 …
fn install_retry_backoff(failed_attempts: u32) -> std::time::Duration {
    let minutes = 10u64 << failed_attempts.min(3);
    std::time::Duration::from_secs(minutes * 60)
}

/// Set while a download+install is in flight, so only one can run at a time.
///
/// Two callers can reach `install_update` concurrently: the unattended loop and
/// the user clicking "Install" on the notification. Without this guard the
/// second one finds an empty `PENDING_UPDATE` (the first `take()`s it) and
/// reports a spurious "no pending update" error, and a failure of the first
/// would then restore an update the second already gave up on. Guarding is also
/// what makes the restore-on-failure below safe to reason about: at most one
/// task ever owns the taken update.
static INSTALL_IN_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Clears `INSTALL_IN_PROGRESS` on every exit path, including a panic or the
/// task being aborted mid-install (the auto-update loop can be torn down at
/// shutdown). A plain assignment at the end of `install_update` would leak the
/// flag on those paths and block every later install attempt until a restart.
struct InstallGuard;

impl Drop for InstallGuard {
    fn drop(&mut self) {
        INSTALL_IN_PROGRESS.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

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
            *PENDING_UPDATE.lock().await = Some(PendingUpdate {
                update,
                beta,
                failed_attempts: 0,
                last_failure: None,
            });
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
            // The channel we just asked has nothing newer than what is running,
            // so anything still parked is stale — typically a pre-release
            // parked while beta was on, which must not be installed now that
            // the check answers for stable. Leaving it would let the auto-loop
            // install a build the user opted out of.
            if let Some(stale) = PENDING_UPDATE.lock().await.take() {
                log::info!(
                    "{TAG} phase=check status=dropped_stale_pending version={} parked_channel={}",
                    stale.update.version,
                    if stale.beta { "beta" } else { "stable" }
                );
            }
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

        // Install the parked update once the card link is quiet. Skip the tick
        // while an install is already running — the user may have clicked
        // "Install" on the notification — so we never start a second download
        // of the same update alongside it.
        if INSTALL_IN_PROGRESS.load(std::sync::atomic::Ordering::SeqCst) {
            continue;
        }
        // The channel the user is on right now. A parked update from the other
        // channel is discarded rather than installed: flipping beta off must
        // not leave a pre-release queued for unattended install.
        let beta_now = crate::config::get_from_cache(
            crate::config::CacheSection::Updates,
            "beta_updates",
        ) == "true";
        let (version, prior_attempts) = {
            let mut guard = PENDING_UPDATE.lock().await;
            let Some(pending) = guard.as_ref() else {
                continue;
            };
            if pending.beta != beta_now {
                log::info!(
                    "{TAG} phase=auto_install status=dropped version={} reason=channel_changed parked_channel={} current_channel={}",
                    pending.update.version,
                    if pending.beta { "beta" } else { "stable" },
                    if beta_now { "beta" } else { "stable" }
                );
                *guard = None;
                continue;
            }
            // Respect the backoff after a failed attempt so a permanently
            // broken artifact is not re-downloaded on every tick.
            if let Some(last_failure) = pending.last_failure {
                let wait = install_retry_backoff(pending.failed_attempts.saturating_sub(1));
                if last_failure.elapsed() < wait {
                    continue;
                }
            }
            (pending.update.version.clone(), pending.failed_attempts)
        };
        let quiet_for = crate::mqtt::seconds_since_card_activity();
        if quiet_for < AUTO_INSTALL_QUIET_SECS {
            log::info!(
                "{TAG} phase=auto_install status=postponed version={} reason=card_activity quiet_secs={}",
                version,
                quiet_for
            );
            continue;
        }

        log::info!(
            "{TAG} phase=auto_install status=starting version={version} attempt={}",
            prior_attempts + 1
        );
        // Announce the install once. A retry after a failed attempt must not
        // re-promise a restart that never came — repeating the banner every
        // retry was pure noise to the user.
        if prior_attempts == 0 {
            emit_notification_event(
                "global-notification",
                NotificationPayload {
                    notification_type: "version".to_string(),
                    message: format!(
                        "Installing update {version} — the application will restart shortly."
                    ),
                },
            );
        }
        // On success this restarts the app and never returns; on failure the
        // update stays parked and the next tick retries after a backoff.
        if let Err(e) = install_update(app.clone()).await {
            log::error!("{TAG} phase=auto_install status=failed version={version} err={e}");
            let mut guard = PENDING_UPDATE.lock().await;
            if let Some(pending) = guard.as_mut() {
                // Only count the failure against the update we actually tried:
                // a concurrent check may have replaced the slot meanwhile.
                if pending.update.version == version {
                    pending.failed_attempts += 1;
                    pending.last_failure = Some(std::time::Instant::now());
                    if pending.failed_attempts >= MAX_INSTALL_ATTEMPTS {
                        log::error!(
                            "{TAG} phase=auto_install status=gave_up version={version} attempts={} — a later check may find it again",
                            pending.failed_attempts
                        );
                        *guard = None;
                    }
                }
            }
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
    // Claim the install slot before touching PENDING_UPDATE, so a concurrent
    // caller (auto-loop vs. the user's "Install" click) backs off instead of
    // racing us for the taken update.
    if INSTALL_IN_PROGRESS.swap(true, std::sync::atomic::Ordering::SeqCst) {
        log::info!("{TAG} phase=install status=skipped reason=already_in_progress");
        return Err("an update install is already in progress".to_string());
    }
    let _install_guard = InstallGuard;

    // Clone the parked update instead of taking it. `Update` is a descriptor
    // (version + download URL + signature), not the payload, so a clone is
    // cheap — and leaving the original in place means an install that never
    // reaches its error branch cannot lose it. The previous `take()` dropped
    // the update on the floor whenever this task was aborted mid-download (app
    // shutdown, auto-loop teardown): the slot stayed empty and the update could
    // only reappear after a fresh network check. The slot is cleared explicitly
    // on success, just before the restart.
    let update = {
        let guard = PENDING_UPDATE.lock().await;
        guard
            .as_ref()
            .map(|pending| pending.update.clone())
            .ok_or_else(|| "no pending update".to_string())?
    };

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
        // The update stays parked (we only cloned it), so a transient download
        // failure leaves it available: the user can retry from the notification
        // and the auto-loop picks it up on its next tick.
        return Err(e.to_string());
    }

    // Installed successfully — drop it so the restarted app does not find a
    // stale entry for the version it is now running.
    *PENDING_UPDATE.lock().await = None;

    log::info!("{TAG} phase=install status=installed action=restart");
    // Release the rack's serial port before the restart, same as window close.
    crate::com_port::shutdown();
    app.restart();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_retry_backoff_grows_then_caps() {
        // A deterministically failing artifact must not be re-downloaded every
        // 5-minute tick: the wait grows from 10 minutes and stops doubling once
        // it reaches 80, so the retries stay bounded and cheap.
        assert_eq!(install_retry_backoff(0).as_secs(), 10 * 60);
        assert_eq!(install_retry_backoff(1).as_secs(), 20 * 60);
        assert_eq!(install_retry_backoff(2).as_secs(), 40 * 60);
        assert_eq!(install_retry_backoff(3).as_secs(), 80 * 60);
        assert_eq!(install_retry_backoff(9).as_secs(), 80 * 60);
    }

    #[test]
    fn giving_up_happens_far_sooner_than_a_day_of_ticks() {
        // Before the cap existed a broken update was retried on every 5-minute
        // tick — ~288 full re-downloads a day. Bound the total retry window
        // instead of the bare attempt count, so the guarantee survives a change
        // to either constant.
        let total_wait: u64 = (0..MAX_INSTALL_ATTEMPTS)
            .map(|attempt| install_retry_backoff(attempt).as_secs())
            .sum();
        assert!(
            total_wait < 24 * 60 * 60,
            "retries must give up within a day, got {total_wait}s"
        );
    }
}
