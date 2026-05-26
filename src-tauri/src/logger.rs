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

    let init_log_result = fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{}[{}][{}] {}",
                chrono::Local::now().format("[%Y-%m-%d][%H:%M:%S%.3f]"),
                record.target(),
                record.level(),
                message
            ))
        })
        .level(log::LevelFilter::Info)  // Change to Debug / Info if needed
        .chain(log_file)
        .apply();

    if let Err(e) = init_log_result {
        eprintln!(
            "Failed to initialize logging at {:?}: {}",
            log_path, e
        );
    }

    // Log the application launch
    log::info!("-== Application is launched ==-");

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
        log::warn!("Versioin. Failed to fetch the latest release info: {}", response.status());
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