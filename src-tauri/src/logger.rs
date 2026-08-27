use std::env;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::global_app_handle::emit_notification_event;
use crate::global_app_handle::NotificationPayload;

/// Rotate when the log grows to this size. Rotation happens at runtime (the
/// app is a server-style solution and can run for months without a restart):
/// `log.txt` -> `log.1.txt`, the displaced `log.1.txt` -> zipped generation
/// in the `archive/` subdirectory.
const LOG_ROTATE_BYTES: u64 = 50 * 1024 * 1024;

/// How many zipped archive generations to keep in `archive/`; the oldest are
/// pruned on every rotation so a long-running instance cannot fill the disk.
const LOG_ARCHIVE_KEEP: usize = 10;

/// Minimal pause between rotation retries after a failed rotation, so a
/// persistent failure (e.g. a locked file) does not retry on every log line.
const ROTATE_RETRY_PAUSE: Duration = Duration::from_secs(60);

/// Timestamp format of a log line prefix; the fern formatter and the
/// fetch_logs period filter (logs_upload) must agree on it.
pub const LOG_LINE_TS_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.3f";

/// Length of the formatted `LOG_LINE_TS_FORMAT` prefix in a log line.
pub const LOG_LINE_TS_LEN: usize = 23;

/// Distinguishes detached-generation temp files created within one second.
static DETACH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Builds a unique temp path a displaced log generation is detached under
/// before being zipped in the background.
fn detach_temp_path(dir: &Path) -> PathBuf {
    let seq = DETACH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    dir.join(format!(
        "log.1.archiving.{}.{}.txt",
        chrono::Local::now().format("%Y%m%d_%H%M%S"),
        seq
    ))
}

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

/// Resolves the directory holding the application log files.
fn log_dir() -> PathBuf {
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

    log_path
}

/// Returns the current and archived log file paths (log.txt, log.1.txt).
pub fn log_file_paths() -> (PathBuf, PathBuf) {
    let dir = log_dir();
    (dir.join("log.txt"), dir.join("log.1.txt"))
}

/// Log sink with runtime size-based rotation.
///
/// Counts the bytes it writes; once `log.txt` reaches the limit the next write
/// rotates the chain: the previous `log.1.txt` is zipped into `archive/` (the
/// oldest archives are pruned to `keep`), `log.txt` is renamed to `log.1.txt`
/// and a fresh `log.txt` is opened. Rotation failures are reported to stderr
/// and retried later — the logging path itself must never fail the app.
struct RotatingLogWriter {
    dir: PathBuf,
    file: Option<File>,
    written: u64,
    limit: u64,
    keep: usize,
    failed_rotate_at: Option<Instant>,
    /// True when the last byte written was a newline. fern delivers one record
    /// as several `write` calls (message body, then the line separator), and
    /// rotating between them would split the record across two files — an
    /// unterminated tail in the archived generation and a timestamp-less head
    /// in the fresh one, which then confuses the fetch_logs period filter.
    at_line_start: bool,
}

impl RotatingLogWriter {
    fn new(dir: PathBuf, limit: u64, keep: usize) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("log.txt"))?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            dir,
            file: Some(file),
            written,
            limit,
            keep,
            failed_rotate_at: None,
            at_line_start: true,
        })
    }

    /// True when a rotation attempt is needed: the size limit is reached, or the
    /// sink is broken and a reopen is due — unless a recent attempt already
    /// failed. Only ever true at a record boundary (see `at_line_start`).
    fn rotation_due(&self) -> bool {
        self.at_line_start
            && (self.file.is_none() || self.written >= self.limit)
            && self
                .failed_rotate_at
                .is_none_or(|at| at.elapsed() >= ROTATE_RETRY_PAUSE)
    }

    fn rotate(&mut self) {
        // close our handle first: Windows cannot rename a file that is open
        drop(self.file.take());

        let current = self.dir.join("log.txt");
        let archived = self.dir.join("log.1.txt");

        // the displaced generation is detached under a unique temp name and zipped in
        // a background thread: deflating 50 MB inline would stall every logging thread
        // on the fern mutex for the whole compression
        if archived.exists() {
            let temp = detach_temp_path(&self.dir);
            match std::fs::rename(&archived, &temp) {
                Ok(()) => {
                    let dir = self.dir.clone();
                    let keep = self.keep;
                    std::thread::spawn(move || archive_detached_generations(&dir, &[temp], keep));
                }
                Err(e) => {
                    // Keep the displaced generation. Deleting it here would destroy up
                    // to 50 MB of history over a transient failure (e.g. an antivirus
                    // or the log collector briefly holding the file on Windows). The
                    // rotation below then fails against the still-present log.1.txt,
                    // pauses via failed_rotate_at and retries the detach in a minute.
                    eprintln!("Failed to detach {:?} for archiving: {}", archived, e);
                }
            }
        }

        if let Err(e) = std::fs::rename(&current, &archived) {
            eprintln!("Failed to rotate log file: {}", e);
        }

        match OpenOptions::new().create(true).append(true).open(&current) {
            Ok(file) => {
                self.written = file.metadata().map(|m| m.len()).unwrap_or(0);
                // rename failure leaves the oversized file in place: pause the retries
                self.failed_rotate_at = (self.written >= self.limit).then(Instant::now);
                self.file = Some(file);
            }
            Err(e) => {
                eprintln!("Failed to reopen log file after rotation: {}", e);
                self.failed_rotate_at = Some(Instant::now());
            }
        }
    }
}

impl Write for RotatingLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.rotation_due() {
            self.rotate();
        }
        match self.file.as_mut() {
            Some(file) => {
                let written = file.write(buf)?;
                self.written += written as u64;
                if written > 0 {
                    self.at_line_start = buf[written - 1] == b'\n';
                }
                Ok(written)
            }
            // the sink is broken (reopen failed, next attempt not due yet): swallow the
            // line instead of erroring — fern must keep serving the stdout chain in dev
            None => {
                if let Some(&last) = buf.last() {
                    self.at_line_start = last == b'\n';
                }
                Ok(buf.len())
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

/// Zips a single `log.txt` entry read from `source` into `sink` (streamed, not
/// buffered). Shared by the rotation archiver and the fetch_logs upload packer.
fn zip_log_entry<W: Write + std::io::Seek>(
    source: &mut impl std::io::Read,
    sink: W,
) -> std::io::Result<W> {
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut writer = zip::ZipWriter::new(sink);
    writer
        .start_file("log.txt", options)
        .map_err(std::io::Error::other)?;
    std::io::copy(source, &mut writer)?;
    writer.finish().map_err(std::io::Error::other)
}

/// Zips a displaced log generation into `archive/log_<timestamp>.zip` and
/// prunes the oldest archives beyond `keep`.
fn archive_log_generation(dir: &Path, archived: &Path, keep: usize) -> std::io::Result<()> {
    let archive_dir = dir.join("archive");
    std::fs::create_dir_all(&archive_dir)?;

    // several generations can be archived within one second (startup sweep):
    // pick a free destination name instead of truncating an existing archive
    let base = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let mut dest = archive_dir.join(format!("log_{}.zip", base));
    let mut suffix = 1;
    while dest.exists() {
        dest = archive_dir.join(format!("log_{}_{}.zip", base, suffix));
        suffix += 1;
    }

    // Deflate into a `.part` file and rename into place only when complete:
    // a mid-stream failure (disk full while compressing 50 MB) must not leave
    // a truncated `.zip` that prune_archives would count against the retention
    // window and a reader could not open.
    let part = archive_dir.join(format!(
        "{}.part",
        dest.file_name().unwrap_or_default().to_string_lossy()
    ));
    let zip_result = zip_log_entry(
        &mut File::open(archived)?,
        std::io::BufWriter::new(File::create(&part)?),
    )
    .and_then(|mut sink| sink.flush());
    if let Err(e) = zip_result {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, &dest)?;

    prune_archives(&archive_dir, keep);
    Ok(())
}

/// Detaches an oversized `log.1.txt` (leftover of the legacy startup-only
/// rotation, can be way past the limit) into a uniquely named temp file, and
/// picks up temp files of an archiving interrupted by a previous shutdown.
/// The rename is cheap, so the caller can zip the result in the background
/// without racing the regular rotation chain.
fn detach_oversized_generation(dir: &Path, limit: u64) -> Vec<PathBuf> {
    let mut detached: Vec<PathBuf> = Vec::new();

    // temp files left by an interrupted archiving run
    if let Ok(entries) = std::fs::read_dir(dir) {
        detached.extend(entries.flatten().map(|entry| entry.path()).filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("log.1.archiving.") && name.ends_with(".txt"))
        }));
    }

    let generation = dir.join("log.1.txt");
    if std::fs::metadata(&generation)
        .map(|m| m.len() >= limit)
        .unwrap_or(false)
    {
        let temp = detach_temp_path(dir);
        match std::fs::rename(&generation, &temp) {
            Ok(()) => detached.push(temp),
            Err(e) => eprintln!("Failed to detach oversized log generation: {}", e),
        }
    }

    detached
}

/// Zips detached generations into `archive/` and removes them; a file that
/// failed to archive is kept for the sweep of the next application start.
fn archive_detached_generations(dir: &Path, detached: &[PathBuf], keep: usize) {
    for path in detached {
        match archive_log_generation(dir, path, keep) {
            Ok(()) => {
                if let Err(e) = std::fs::remove_file(path) {
                    eprintln!("Failed to remove archived log generation {:?}: {}", path, e);
                }
            }
            Err(e) => eprintln!("Failed to archive log generation {:?}: {}", path, e),
        }
    }
}

/// Removes the oldest `log_*.zip` archives so at most `keep` remain. The
/// timestamped names sort chronologically, so a name sort is enough.
fn prune_archives(archive_dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(archive_dir) else {
        return;
    };
    let mut archives: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("log_") && name.ends_with(".zip"))
        })
        .collect();
    if archives.len() <= keep {
        return;
    }
    archives.sort();
    for path in &archives[..archives.len() - keep] {
        if let Err(e) = std::fs::remove_file(path) {
            eprintln!("Failed to prune log archive {:?}: {}", path, e);
        }
    }
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
pub fn setup_logging() {
    let dir = log_dir();

    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("Failed to create log directory {:?}: {}", dir, e);
        return;
    }

    // An oversized log.1.txt (left by the legacy startup-only rotation) is
    // detached right away and zipped in the background: waiting for the next
    // rotation to displace it would keep hundreds of MB on disk for months and
    // then stall the logging mutex while such a generation is compressed.
    let detached = detach_oversized_generation(&dir, LOG_ROTATE_BYTES);
    if !detached.is_empty() {
        let sweep_dir = dir.clone();
        std::thread::spawn(move || {
            archive_detached_generations(&sweep_dir, &detached, LOG_ARCHIVE_KEEP)
        });
    }

    // Runtime-rotating sink: rotation triggers on size while the app runs, so
    // an instance living for months keeps rotating without a restart. An
    // oversized log.txt left by a previous run rotates on the first write.
    let log_writer = match RotatingLogWriter::new(dir.clone(), LOG_ROTATE_BYTES, LOG_ARCHIVE_KEEP) {
        Ok(writer) => writer,
        Err(e) => {
            eprintln!("Failed to create log file: {}", e);
            log::warn!("No permission to write log file at: {:?}", dir);

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
                chrono::Local::now().format(LOG_LINE_TS_FORMAT),
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
                        dispatch = dispatch.level_for(format!("app_lib::{}", module.trim()), level);
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

    dispatch = dispatch.chain(fern::Output::writer(Box::new(log_writer), "\n"));

    // In dev builds mirror the log to stdout so `npm run tauri dev` shows it
    // live in the terminal.
    #[cfg(debug_assertions)]
    {
        dispatch = dispatch.chain(std::io::stdout());
    }

    if let Err(e) = dispatch.apply() {
        eprintln!("Failed to initialize logging at {:?}: {}", dir, e);
    }

    // Log the application launch
    log::info!(
        "-== Application is launched ==- (version: {}, log level: {})",
        env!("CARGO_PKG_VERSION"),
        level_spec
    );

    // NOTE: the update check lives in `updater.rs` (tauri-plugin-updater), which
    // runs at startup from lib.rs and understands pre-release versions. The
    // hand-rolled GitHub-API check that used to run here was removed: its
    // numeric comparison dropped the channel suffix, so `0.8.0-beta.8` collapsed
    // to `8` and every beta build reported itself as outdated against any
    // stable release — a false "new version available" popup on each launch.

    // Log system information
    log_system_info();
}

fn log_system_info() {
    let os_type = sys_info::os_type().unwrap_or_else(|_| "Unknown".to_string());
    let os_release = sys_info::os_release().unwrap_or_else(|_| "Unknown".to_string());
    let hostname = sys_info::hostname().unwrap_or_else(|_| "Unknown".to_string());
    let cpu_num = sys_info::cpu_num().unwrap_or(0);
    let cpu_speed = sys_info::cpu_speed()
        .map_or_else(|_| "Unknown".to_string(), |speed| format!("{} MHz", speed));
    let mem_info = sys_info::mem_info().map_or_else(
        |_| "Unknown".to_string(),
        |mem| format!("total {} KB, free {} KB", mem.total, mem.free),
    );

    log::info!(
        "OS Type: {}, OS Release: {}, Hostname: {}, Number of CPUs: {} ({}), Memory: {}",
        os_type,
        os_release,
        hostname,
        cpu_num,
        cpu_speed,
        mem_info
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a unique empty scratch directory for a rotation test.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("tba-logger-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn list_archives(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir.join("archive"))
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn rotation_moves_current_log_to_generation_at_limit() {
        let dir = scratch_dir("rotate");
        let mut writer = RotatingLogWriter::new(dir.clone(), 32, 3).unwrap();

        writer
            .write_all(b"old line big enough to cross the 32 bytes limit\n")
            .unwrap();
        writer.write_all(b"new line\n").unwrap(); // limit reached: this write rotates first
        writer.flush().unwrap();

        let archived = std::fs::read_to_string(dir.join("log.1.txt")).unwrap();
        assert!(archived.contains("old line"));
        let current = std::fs::read_to_string(dir.join("log.txt")).unwrap();
        assert_eq!(current, "new line\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn displaced_generation_is_zipped_into_archive() {
        let dir = scratch_dir("archive");
        let mut writer = RotatingLogWriter::new(dir.clone(), 8, 3).unwrap();

        writer.write_all(b"generation one\n").unwrap();
        writer.write_all(b"generation two\n").unwrap(); // rotation 1: no log.1 yet, nothing to archive
        writer.write_all(b"generation three\n").unwrap(); // rotation 2: "generation one" leaves log.1 for archive

        // the displaced generation is zipped in a background thread, and the
        // archive file becomes visible before its content is flushed: poll
        // until the zip signature lands, not just until the file appears
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let archives = list_archives(&dir);
            if archives.len() == 1 {
                let zipped = std::fs::read(&archives[0]).unwrap();
                if zipped.starts_with(b"PK\x03\x04") {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "zipped archive did not appear in time"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_generation_is_detached_and_archived_on_startup() {
        let dir = scratch_dir("detach");
        std::fs::write(dir.join("log.1.txt"), b"a big legacy generation").unwrap();

        let detached = detach_oversized_generation(&dir, 10);
        assert_eq!(detached.len(), 1);
        assert!(!dir.join("log.1.txt").exists());
        assert!(detached[0].exists());

        archive_detached_generations(&dir, &detached, 3);
        assert!(!detached[0].exists());
        assert_eq!(list_archives(&dir).len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn small_generation_is_left_in_the_rotation_chain() {
        let dir = scratch_dir("detach-small");
        std::fs::write(dir.join("log.1.txt"), b"small").unwrap();

        let detached = detach_oversized_generation(&dir, 10);
        assert!(detached.is_empty());
        assert!(dir.join("log.1.txt").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_keeps_only_newest_archives() {
        let dir = scratch_dir("prune");
        let archive_dir = dir.join("archive");
        std::fs::create_dir_all(&archive_dir).unwrap();
        for i in 0..5 {
            std::fs::write(
                archive_dir.join(format!("log_2026010{}_000000.zip", i)),
                b"x",
            )
            .unwrap();
        }

        prune_archives(&archive_dir, 2);

        let mut left: Vec<String> = list_archives(&dir)
            .into_iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec!["log_20260103_000000.zip", "log_20260104_000000.zip"]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
