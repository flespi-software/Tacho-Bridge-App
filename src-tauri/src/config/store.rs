//! config.yaml on disk: where it lives, reading it, and writing it atomically.
//!
//! Every load-modify-save cycle of any writer runs under `config_write_guard`,
//! so concurrent writers can neither drop each other's changes nor share the
//! temp file half-written.

// ───── Std Lib ─────
use std::env;
use std::fs;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

// ───── Local Modules ─────
use super::types::ConfigurationFile;

/// Retrieves the configuration file path.
/// This function constructs the path to the configuration file, creating the necessary directories if they do not exist.
pub(super) fn get_config_path() -> io::Result<PathBuf> {
    let mut config_path = PathBuf::new();

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let home_dir = env::var("HOME");

    #[cfg(target_os = "windows")]
    let home_dir = env::var("USERPROFILE");

    match &home_dir {
        Ok(home) => {
            log::debug!("Home directory found: {}", home);
            config_path.push(home);
        }
        Err(e) => {
            log::error!("Failed to get home directory environment variable: {}", e);
            return Err(io::Error::other(
                "Failed to get home directory environment variable",
            ));
        }
    }

    config_path.push("Documents");
    config_path.push("tba");

    log::debug!("Config directory path resolved to: {:?}", config_path);

    if let Err(e) = fs::create_dir_all(&config_path) {
        log::error!("Failed to create config directory {:?}: {}", config_path, e);
        return Err(e);
    }

    config_path.push("config.yaml");

    log::debug!("Final config file path: {:?}", config_path);

    Ok(config_path)
}

/// Load the configuration from the file.
/// This function reads the configuration file and parses it.
pub(super) fn load_config(
    config_path: &Path,
) -> Result<ConfigurationFile, Box<dyn std::error::Error + Send + Sync>> {
    let mut config_contents = String::new();
    File::open(config_path)?.read_to_string(&mut config_contents)?;
    let config: ConfigurationFile = serde_yaml::from_str(&config_contents)?;
    Ok(config)
}

/// Saves the configuration to the file atomically.
/// Writes to a sibling temp file in the same directory, then renames it over the target.
/// This prevents config corruption (empty/partial file) if the process is killed mid-write.
pub(super) fn save_config(
    config_path: &Path,
    config: &ConfigurationFile,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let yaml = serde_yaml::to_string(config)?;

    let parent = config_path
        .parent()
        .ok_or_else(|| io::Error::other("config path has no parent directory"))?;
    let file_name = config_path
        .file_name()
        .ok_or_else(|| io::Error::other("config path has no file name"))?;

    let mut tmp_path = parent.to_path_buf();
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(".tmp");
    tmp_path.push(tmp_name);

    let written = (|| -> io::Result<()> {
        // The file may hold the broker credentials: owner-only on unix, from
        // creation on (`mode` covers a fresh file, `set_permissions` a
        // leftover one from a crash), so no window exposes a readable copy.
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut tmp_file = options.open(&tmp_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tmp_file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        tmp_file.write_all(yaml.as_bytes())?;
        tmp_file.sync_all()
    })();
    if let Err(e) = written {
        // A partial temp copy (disk full) may still hold the credentials.
        let _ = fs::remove_file(&tmp_path);
        return Err(Box::new(e));
    }

    if let Err(e) = rename_with_retry(&tmp_path, config_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(Box::new(e));
    }

    // Make the rename itself durable: without it a power loss can bring back
    // the previous file (or, on some filesystems, an empty one).
    #[cfg(unix)]
    if let Some(parent) = config_path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }

    Ok(())
}

/// `fs::rename` that rides out transient locks. On Windows, antivirus,
/// OneDrive (the config lives in Documents) or an editor briefly holding the
/// target make the replace fail with access denied / sharing violation, and
/// the change being saved would be lost.
fn rename_with_retry(from: &Path, to: &Path) -> io::Result<()> {
    const ATTEMPTS: u32 = if cfg!(windows) { 6 } else { 1 };
    let mut attempt = 1;
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            // 32 = ERROR_SHARING_VIOLATION
            Err(e)
                if attempt < ATTEMPTS
                    && (e.kind() == io::ErrorKind::PermissionDenied
                        || e.raw_os_error() == Some(32)) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(50 * u64::from(attempt)));
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Serializes every load-modify-save cycle on config.yaml. Without it,
/// concurrent writers (frontend commands, the APDU sniffer, auth-result
/// persistence) interleave: both load the same version and the last save
/// silently drops the other's changes; they also share one tmp file, which
/// breaks the atomic-rename guarantee. Held across the whole read-modify-write
/// including the cache refresh, so cache order matches file order.
static CONFIG_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Acquires the config write lock, recovering from poisoning (a panic in an
/// earlier holder must not cascade into every future config write).
pub(super) fn config_write_guard() -> std::sync::MutexGuard<'static, ()> {
    match CONFIG_WRITE_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::warn!("CONFIG_WRITE_LOCK was poisoned — recovering");
            poisoned.into_inner()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::{sample_config, unique_tmp_dir};

    #[test]
    fn save_then_load_roundtrip() {
        let dir = unique_tmp_dir("roundtrip");
        let path = dir.join("config.yaml");

        let cfg = sample_config();
        save_config(&path, &cfg).expect("save");

        let loaded = load_config(&path).expect("load");
        assert_eq!(loaded.cards.len(), 1);
        let card = loaded.cards.get("ABCDEF0123456789").expect("card present");
        assert_eq!(card.iccid, "1122334455667788");
        assert_eq!(card.card_type, Some(4));
        assert_eq!(card.last_auth, Some((1_700_000_500, true)));
        assert_eq!(
            loaded.server.as_ref().map(|s| s.host.as_str()),
            Some("mqtt.example.com:8883")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn server_credentials_roundtrip_and_stay_optional() {
        let dir = unique_tmp_dir("credentials");
        let path = dir.join("config.yaml");

        // Without credentials the file must not even mention the fields —
        // existing configs stay byte-compatible.
        save_config(&path, &sample_config()).expect("save without credentials");
        let raw = fs::read_to_string(&path).expect("read yaml");
        assert!(
            !raw.contains("username"),
            "absent username must not be written"
        );
        assert!(
            !raw.contains("password"),
            "absent password must not be written"
        );
        let loaded = load_config(&path).expect("load without credentials");
        let server = loaded.server.expect("server section");
        assert_eq!(server.username, None);
        assert_eq!(server.password, None);

        // Hand-edited credentials survive a save/load cycle.
        let mut cfg = sample_config();
        if let Some(server) = cfg.server.as_mut() {
            server.username = Some("FlespiToken".to_string());
            server.password = Some("secret".to_string());
        }
        save_config(&path, &cfg).expect("save with credentials");
        let loaded = load_config(&path).expect("load with credentials");
        let server = loaded.server.expect("server section");
        assert_eq!(server.username.as_deref(), Some("FlespiToken"));
        assert_eq!(server.password.as_deref(), Some("secret"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_is_atomic_no_tmp_leftover() {
        let dir = unique_tmp_dir("atomic");
        let path = dir.join("config.yaml");
        let tmp_path = dir.join("config.yaml.tmp");

        save_config(&path, &sample_config()).expect("save");
        assert!(path.exists(), "final config must exist");
        assert!(!tmp_path.exists(), "tmp file must be renamed away");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_overwrites_existing_file_safely() {
        let dir = unique_tmp_dir("overwrite");
        let path = dir.join("config.yaml");

        save_config(&path, &sample_config()).expect("first save");
        let first_meta = fs::metadata(&path).expect("meta1");

        let mut second = sample_config();
        second.cards.clear();
        save_config(&path, &second).expect("second save");

        let loaded = load_config(&path).expect("load second");
        assert!(loaded.cards.is_empty(), "second save must replace contents");
        assert!(first_meta.is_file());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_rejects_malformed_yaml() {
        let dir = unique_tmp_dir("malformed");
        let path = dir.join("config.yaml");
        fs::write(&path, b"this: is: not: yaml: [[[").expect("write garbage");

        let res = load_config(&path);
        assert!(res.is_err(), "load must fail on malformed yaml");

        let _ = fs::remove_dir_all(&dir);
    }
}
