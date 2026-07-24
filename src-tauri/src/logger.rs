use std::env;
use std::path::PathBuf;
// use std::fs;
// use std::error::Error; // Импортируем трэйт Error

use fern;
use log;
use sys_info;
use reqwest;
use serde::Deserialize;
use tauri::async_runtime;
// use tauri::Emitter;

use crate::global_app_handle::emit_notification_event;
use crate::global_app_handle::NotificationPayload;

#[derive(Deserialize, Debug)]
struct Release {
    tag_name: String,
}

/// Sets up logging for the application.
///
/// This function configures the logging system using the `fern` crate. It sets the log file path
/// based on the operating system and initializes the logging format and level.
///
/// # Platform-specific behavior
///
/// * On macOS, the log file is created in the `~/Documents/tba` directory.
/// * On Windows, the log file is created in the `%USERPROFILE%\Documents\tba` directory.
///

/// Rotate when the log grows past this size; one archived generation is kept
/// as `log.1.txt`. 50 MB is months of INFO-level logs — without the cap the
/// file grows forever.
const LOG_ROTATE_BYTES: u64 = 50 * 1024 * 1024;

/// Parses a level name from the `TBA_LOG` spec ("debug", "warn", ...).
fn parse_level(s: &str) -> Option<log::LevelFilter> {
    match s.trim().to_ascii_lowercase().as_str() {
        "off" => Some(log::LevelFilter::Off),
        "error" => Some(log::LevelFilter::Error),
        "warn" => Some(log::LevelFilter::Warn),
        "info" => Some(log::LevelFilter::Info),
        "debug" => Some(log::LevelFilter::Debug),
        "trace" => Some(log::LevelFilter::Trace),
        _ => None,
    }
}

pub fn setup_logging() {
    let mut log_path = PathBuf::new();

    // Resolve the home directory without panicking if the env var is missing —
    // fall back to the current working directory so the app still starts and
    // we just lose persistent logging.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let home_var = "HOME";
    #[cfg(target_os = "windows")]
    let home_var = "USERPROFILE";

    match env::var(home_var) {
        Ok(home) => {
            log_path.push(home);
            log_path.push("Documents");
            log_path.push("tba");
        }
        Err(e) => {
            eprintln!(
                "Failed to read {} env var ({}). Logging to current directory.",
                home_var, e
            );
            log_path.push(".");
            log_path.push("tba-logs");
        }
    }

    if let Err(e) = std::fs::create_dir_all(&log_path) {
        eprintln!("Failed to create log directory {:?}: {}", log_path, e);
        return;
    }

    log_path.push("log.txt");

    // Size-based rotation with one archived generation (log.1.txt).
    if let Ok(meta) = std::fs::metadata(&log_path) {
        if meta.len() > LOG_ROTATE_BYTES {
            let archived = log_path.with_file_name("log.1.txt");
            // Windows rename does not overwrite an existing destination.
            let _ = std::fs::remove_file(&archived);
            match std::fs::rename(&log_path, &archived) {
                Ok(()) => eprintln!("Log rotated: log.txt -> log.1.txt"),
                Err(e) => eprintln!("Failed to rotate log file: {}", e),
            }
        }
    }

    // Open the log file exactly once. Reusing the same handle eliminates the
    // race where the file could be removed between two open() calls and the
    // second one would panic via .unwrap().
    let log_file = match fern::log_file(&log_path) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("Failed to create log file: {}", e);
            log::warn!("No permission to write log file at: {:?}", log_path);

            let payload = NotificationPayload {
                notification_type: "access".to_string(),
                message: "No permission to write log file".to_string(),
            };
            emit_notification_event("global-notification", payload);

            return;
        }
    };

    let mut dispatch = fern::Dispatch::new()
        .format(|out, message, record| {
            // Compact single-line prefix: `2026-07-08 17:22:49.151 WARN  [mqtt] ...`
            // (our crate prefix is stripped from the target for readability).
            let target = record.target();
            let target = target.strip_prefix("app_lib::").unwrap_or(target);
            out.finish(format_args!(
                "{} {:<5} [{}] {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                record.level(),
                target,
                message
            ))
        })
        // Default: our code and dependencies at INFO.
        .level(log::LevelFilter::Info);

    // TBA_LOG controls verbosity without a rebuild. Applies to our modules
    // only — dependencies stay at INFO. Examples:
    //   TBA_LOG=debug                       whole app at debug
    //   TBA_LOG=smart_card=debug,mqtt=warn  per-module overrides
    let mut level_spec = String::from("info");
    if let Ok(spec) = env::var("TBA_LOG") {
        for part in spec.split(',').filter(|p| !p.trim().is_empty()) {
            match part.split_once('=') {
                Some((module, level)) => match parse_level(level) {
                    Some(level) => {
                        dispatch =
                            dispatch.level_for(format!("app_lib::{}", module.trim()), level);
                    }
                    None => eprintln!("TBA_LOG: unknown level in '{}'", part),
                },
                None => match parse_level(part) {
                    // Bare level: the whole app_lib tree (fern falls back from
                    // app_lib::mqtt to app_lib when matching module levels).
                    Some(level) => dispatch = dispatch.level_for("app_lib", level),
                    None => eprintln!("TBA_LOG: unknown level '{}'", part),
                },
            }
        }
        level_spec = spec;
    }

    dispatch = dispatch.chain(log_file);

    // In dev builds mirror the log to stdout so `npm run tauri dev` shows it
    // live in the terminal.
    #[cfg(debug_assertions)]
    {
        dispatch = dispatch.chain(std::io::stdout());
    }

    if let Err(e) = dispatch.apply() {
        eprintln!(
            "Failed to initialize logging at {:?}: {}",
            log_path, e
        );
    }

    // Log the application launch. For pre-release builds the version carries
    // the git commit (`+<hash>`), pinning the exact build; stable versions
    // are logged clean.
    log::info!(
        "-== Application is launched ==- (version: {}, log level: {})",
        env!("TBA_BUILD_VERSION"),
        level_spec
    );

    // Check for the latest version asynchronously
    async_runtime::spawn(async {
        if let Err(e) = check_latest_version().await {
            log::error!("Error checking latest version: {}", e);
        }
    });

    // Log system information
    log_system_info();
}

fn log_system_info() {
    let os_type = sys_info::os_type().unwrap_or_else(|_| "Unknown".to_string());
    let os_release = sys_info::os_release().unwrap_or_else(|_| "Unknown".to_string());
    let hostname = sys_info::hostname().unwrap_or_else(|_| "Unknown".to_string());
    let cpu_num = sys_info::cpu_num().unwrap_or_else(|_| 0);
    let cpu_speed = sys_info::cpu_speed().map_or_else(|_| "Unknown".to_string(), |speed| format!("{} MHz", speed));
    let mem_info = sys_info::mem_info().map_or_else(|_| "Unknown".to_string(), |mem| format!("total {} KB, free {} KB", mem.total, mem.free));

    log::info!(
        "OS Type: {}, OS Release: {}, Hostname: {}, Number of CPUs: {} ({}), Memory: {}",
        os_type, os_release, hostname, cpu_num, cpu_speed, mem_info
    );
}

async fn check_latest_version() -> Result<(), reqwest::Error> {
    let url = "https://api.github.com/repos/flespi-software/Tacho-Bridge-App/releases/latest";
    let client = reqwest::Client::new();
    let response = client
        .get(url)
        .header("User-Agent", "reqwest")
        .send()
        .await?;

    if response.status().is_success() {
        let release: Release = response.json().await?;
        // log::info!("Latest release info: {:?}", release);

        let latest_version = release.tag_name;
        let current_version = env!("CARGO_PKG_VERSION");

        let latest_version_num = version_to_number(&latest_version);
        let current_version_num = version_to_number(current_version);

        if current_version_num > latest_version_num {
            log::info!(
                "Version (current: {}, latest: {})",
                current_version,
                latest_version
            );
        } else if current_version_num < latest_version_num {
            log::info!(
                "Version (current: {}, latest: {}). New one is available, use the link to download: {}",
                current_version,
                latest_version,
                url
            );

            let payload = NotificationPayload {
                notification_type: "version".to_string(),
                message: format!("New version {} is available, use the link to download: {}", latest_version, url).into(),
            };
            emit_notification_event("global-notification", payload);
        } else {
            log::info!(
                "Version (current: {}, latest: {}). You are using the latest version.",
                current_version,
                latest_version
            );
        }
    } else {
        log::warn!("Version. Failed to fetch the latest release info: {}", response.status());
    }

    Ok(())
}

fn version_to_number(version: &str) -> u32 {
    version
        .trim_start_matches('v')
        .split('.')
        .filter_map(|s| s.parse::<u32>().ok())
        .fold(0, |acc, num| acc * 100 + num)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_to_number_strips_v_prefix_and_packs_components() {
        // 0.7.2 → 0*10000 + 7*100 + 2 = 702
        assert_eq!(version_to_number("0.7.2"), 702);
        assert_eq!(version_to_number("v0.7.2"), 702);
        assert_eq!(version_to_number("1.0.0"), 10000);
    }

    #[test]
    fn version_to_number_orders_versions_correctly() {
        assert!(version_to_number("v0.7.3") > version_to_number("v0.7.2"));
        assert!(version_to_number("v0.8.0") > version_to_number("v0.7.99"));
        assert!(version_to_number("v1.0.0") > version_to_number("v0.99.99"));
    }

    #[test]
    fn version_to_number_handles_garbage_components() {
        // Non-numeric chunks are skipped silently.
        assert_eq!(version_to_number("v1.x.3"), 103);
    }
}