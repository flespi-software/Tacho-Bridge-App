use std::env;
// use std::fs::OpenOptions;
use std::path::PathBuf;

use fern;
use log;

/// Sets up logging for the application.
///
/// This function configures the logging system using the `fern` crate. It sets the log file path
/// based on the operating system and initializes the logging format and level.
///
/// # Platform-specific behavior
///
/// * On macOS, the log file is created in the `~/Documents/tba` directory.
/// * On Windows, the log file is created in the `%USERPROFILE%\Documents\tba` directory.
pub fn setup_logging() {
    let mut log_path = PathBuf::new();

    #[cfg(target_os = "macos")]
    {
        log_path.push(env::var("HOME").unwrap());
        log_path.push("Documents");
        log_path.push("tba");
    }
    #[cfg(target_os = "linux")]
    {
        log_path.push(env::var("HOME").unwrap());
        log_path.push("Documents");
        log_path.push("tba");
    }
    #[cfg(target_os = "windows")]
    {
        log_path.push(env::var("USERPROFILE").unwrap());
        log_path.push("Documents");
        log_path.push("tba");
    }

    if let Err(e) = std::fs::create_dir_all(&log_path) {
        eprintln!("Failed to create log directory: {}", e);
        return;
    }

    log_path.push("log.txt");

    if let Err(e) = fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{}[{}][{}] {}",
                chrono::Local::now().format("[%Y-%m-%d][%H:%M:%S%.3f]"),
                record.target(),
                record.level(),
                message
            ))
        })
        .level(log::LevelFilter::Debug)  // For debugging it is needed to set up 'Debug' filter level
        .chain(fern::log_file(log_path).unwrap())
        .apply()
    {
        eprintln!("Failed to initialize logging: {}", e);
    }
}
