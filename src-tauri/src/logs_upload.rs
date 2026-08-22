//! Application log upload: the `fetch_logs` server command.
//!
//! The server publishes `{"name":"fetch_logs","period":"1d"|"7d"}` to
//! `request/<request_id>/0` on the app connection. The reply is the zipped log
//! slice for the requested period published back in binary chunks to
//! `logs/<request_id>/<seq>` (seq from 0) followed by a JSON finalizer on
//! `logs/<request_id>/done` — either `{"name":...,"size":...,"chunks":...}`
//! or `{"error":...}` when the slice could not be collected.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Mutex;

use chrono::{Duration, Local, NaiveDateTime};
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::AsyncClient;
use serde_json::{json, Value};
use tauri::async_runtime;

use crate::logger::{log_file_paths, LOG_LINE_TS_FORMAT, LOG_LINE_TS_LEN};

/// Upload chunk size. Large enough to move a multi-megabyte zip in a handful
/// of publishes, small enough to keep memory and re-send costs sane.
const CHUNK_SIZE: usize = 1024 * 1024;

/// Refuse to collect a log slice bigger than this (before zipping) — a runaway
/// debug-level log would otherwise be buffered in RAM in full.
const MAX_SLICE_BYTES: usize = 128 * 1024 * 1024;

/// Per-publish deadline. A publish into a client whose event loop died (e.g.
/// the app connection was replaced) would otherwise block forever and keep
/// ACTIVE_REQUEST occupied, silently dropping every future fetch_logs.
const PUBLISH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The request currently being served; the server re-sends the command when
/// the reply is slow, so duplicates must not start a second upload.
static ACTIVE_REQUEST: Mutex<Option<u64>> = Mutex::new(None);

/// Poison-recovering lock, matching the convention in config.rs: a panic in
/// one holder must not make every later fetch_logs dispatch panic in turn.
fn lock_active_request() -> std::sync::MutexGuard<'static, Option<u64>> {
    ACTIVE_REQUEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Clears ACTIVE_REQUEST when dropped. Held across the upload so the slot is
/// freed on EVERY exit path — normal return, panic inside run_upload, or the
/// task being aborted (the app connection can be replaced mid-upload). A plain
/// trailing statement would skip the latter two and permanently wedge
/// fetch_logs until an app restart.
struct ActiveRequestReset;

impl Drop for ActiveRequestReset {
    fn drop(&mut self) {
        *lock_active_request() = None;
    }
}

/// Handles a `fetch_logs` request published on the app connection; any other
/// publish is left to the caller (returns false). If more app-level commands
/// appear, promote the topic/name parsing to the connection layer and keep
/// only the fetch_logs handling here.
pub fn dispatch_request(
    client: &AsyncClient,
    log_header: &str,
    topic: &str,
    payload: &Value,
) -> bool {
    let Some(request_id) = crate::mqtt::request_id_from_topic(topic) else {
        return false;
    };
    if payload.get("name").and_then(Value::as_str) != Some("fetch_logs") {
        return false;
    }
    let period = payload
        .get("period")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    {
        let mut active = lock_active_request();
        if let Some(active_id) = *active {
            if active_id == request_id {
                // server re-send of the request in flight: drop it, the upload
                // already running will produce this command's result
                log::warn!(
                    "{} [LOGS] status=duplicate_request request_id={}",
                    log_header,
                    request_id
                );
                return true;
            }
            // A DIFFERENT id is a new command, and the running upload only
            // publishes under its own id — silently dropping this one would
            // leave the server waiting for a `done` that never comes, failing
            // only by timeout. Refuse it explicitly on the new id's done topic
            // instead, so the command fails fast and readably.
            log::warn!(
                "{} [LOGS] status=rejected_concurrent_request request_id={} active_request_id={}",
                log_header,
                request_id,
                active_id
            );
            let client = client.clone();
            let log_header = log_header.to_string();
            async_runtime::spawn(async move {
                publish_json(
                    &client,
                    &log_header,
                    &format!("logs/{}/done", request_id),
                    json!({"error": "another log upload is already in progress"}),
                )
                .await;
            });
            return true;
        }
        *active = Some(request_id);
    }

    log::info!(
        "{} [LOGS] status=fetch_started request_id={} period={}",
        log_header,
        request_id,
        period
    );

    let client = client.clone();
    let log_header = log_header.to_string();
    async_runtime::spawn(async move {
        let _reset = ActiveRequestReset;
        run_upload(&client, &log_header, request_id, period).await;
    });
    true
}

/// Collects, zips and publishes the log slice; reports failures to the server
/// through the `done` topic so the command fails with a readable error.
async fn run_upload(client: &AsyncClient, log_header: &str, request_id: u64, period: String) {
    let done_topic = format!("logs/{}/done", request_id);

    let days = match period_days(&period) {
        Some(days) => days,
        None => {
            log::warn!("{} [LOGS] status=bad_period period={}", log_header, period);
            publish_json(
                client,
                log_header,
                &done_topic,
                json!({"error": format!("unsupported log period '{}'", period)}),
            )
            .await;
            return;
        }
    };

    // file IO and zipping are blocking work, keep them off the async workers
    let collected = async_runtime::spawn_blocking(move || collect_zipped_logs(days, &period)).await;
    let (name, data) = match collected {
        Ok(Ok(collected)) => collected,
        Ok(Err(e)) => {
            log::warn!("{} [LOGS] status=collect_failed err={}", log_header, e);
            publish_json(client, log_header, &done_topic, json!({"error": e})).await;
            return;
        }
        Err(e) => {
            log::error!("{} [LOGS] status=collect_task_failed err={}", log_header, e);
            publish_json(
                client,
                log_header,
                &done_topic,
                json!({"error": "log collection task failed"}),
            )
            .await;
            return;
        }
    };

    let mut chunks = 0usize;
    for chunk in data.chunks(CHUNK_SIZE) {
        let topic = format!("logs/{}/{}", request_id, chunks);
        // a failed or stalled chunk ends the upload: the server will time the command
        // out, nothing else meaningful can be delivered on this connection
        if !publish_with_timeout(client, log_header, topic, chunk.to_vec()).await {
            return;
        }
        chunks += 1;
    }

    publish_json(
        client,
        log_header,
        &done_topic,
        json!({"name": name, "size": data.len(), "chunks": chunks}),
    )
    .await;
    log::info!(
        "{} [LOGS] status=fetch_finished request_id={} name={} size={} chunks={}",
        log_header,
        request_id,
        name,
        data.len(),
        chunks
    );
}

/// Publishes one payload under the per-publish deadline.
/// Returns false when the publish failed or stalled and the upload must stop.
async fn publish_with_timeout(
    client: &AsyncClient,
    log_header: &str,
    topic: String,
    payload: Vec<u8>,
) -> bool {
    let published = tokio::time::timeout(
        PUBLISH_TIMEOUT,
        client.publish(topic.clone(), QoS::AtLeastOnce, false, payload),
    )
    .await;
    match published {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            log::error!(
                "{} [LOGS] status=publish_failed topic={} err={:?}",
                log_header,
                topic,
                e
            );
            false
        }
        Err(_) => {
            log::error!(
                "{} [LOGS] status=publish_timeout topic={}",
                log_header,
                topic
            );
            false
        }
    }
}

async fn publish_json(client: &AsyncClient, log_header: &str, topic: &str, value: Value) {
    publish_with_timeout(
        client,
        log_header,
        topic.to_string(),
        value.to_string().into_bytes(),
    )
    .await;
}

/// Parses the wire period "<days>d" to a day count. The parse is generic on
/// purpose: the authoritative whitelist is the server-side command enum, so an
/// older app stays compatible when new periods are added there.
fn period_days(period: &str) -> Option<i64> {
    let days = period.strip_suffix('d')?.parse::<i64>().ok()?;
    (1..=365).contains(&days).then_some(days)
}

/// Reads the log slice for the last `days` days and returns the zip file
/// name and content. The current log is read first; when it does not reach
/// back to the period start, the archived generation is prepended.
fn collect_zipped_logs(days: i64, period: &str) -> Result<(String, Vec<u8>), String> {
    let cutoff = Local::now().naive_local() - Duration::days(days);
    let (current_path, archived_path) = log_file_paths();

    // Snapshot both generations at one instant BEFORE any reading: the logger
    // rotates concurrently (log.txt -> log.1.txt -> archive), and path-based
    // opens spread over the collection would miss a whole generation when a
    // rotation lands between them. An open handle keeps following its file
    // across the rename, so the pair below stays a consistent chain.
    let mut current_file =
        open_optional(&current_path).map_err(|e| format!("cannot read log file: {}", e))?;
    let archived_file = open_optional(&archived_path)
        .map_err(|e| format!("cannot read archived log file: {}", e))?;

    // the current log covers the whole period when its first entry is older than the cutoff
    let first_ts = match current_file.as_mut() {
        Some(file) => {
            let ts = first_entry_timestamp(BufReader::new(&mut *file))
                .map_err(|e| format!("cannot read log file: {}", e))?;
            use std::io::Seek;
            file.seek(std::io::SeekFrom::Start(0))
                .map_err(|e| format!("cannot rewind log file: {}", e))?;
            ts
        }
        None => None,
    };
    let archive_needed = first_ts.is_none_or(|ts| ts > cutoff);

    // Compress while filtering instead of collecting the whole plaintext slice
    // first: the uncompressed window can be up to MAX_SLICE_BYTES, and holding
    // it alongside the finished zip put roughly twice that in RAM at the peak.
    // Deflating as we go keeps only the zip (log text compresses ~10-20x) plus
    // one line of scratch.
    let mut writer = start_zip().map_err(|e| format!("cannot start zip: {}", e))?;
    let mut written = 0usize;

    if archive_needed {
        if let Some(file) = archived_file {
            written += filter_log_lines(BufReader::new(file), cutoff, &mut writer, written)
                .map_err(|e| format!("cannot read archived log file: {}", e))?;
        }
    }
    if let Some(file) = current_file {
        written += filter_log_lines(BufReader::new(file), cutoff, &mut writer, written)
            .map_err(|e| format!("cannot read log file: {}", e))?;
    }
    if written == 0 {
        return Err(format!("no log entries for the last {} day(s)", days));
    }

    let name = format!(
        "tba_logs_{}_{}.zip",
        Local::now().format("%Y%m%d_%H%M"),
        period
    );
    let zipped = finish_zip(writer).map_err(|e| format!("cannot zip log data: {}", e))?;
    Ok((name, zipped))
}

/// Opens a log file, mapping "not found" to None — a missing generation
/// simply contributes nothing to the slice.
fn open_optional(path: &Path) -> std::io::Result<Option<std::fs::File>> {
    match std::fs::File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Returns the timestamp of the first timestamped line of the reader,
/// None when it is empty or holds no timestamped lines.
fn first_entry_timestamp<R: BufRead>(reader: R) -> std::io::Result<Option<NaiveDateTime>> {
    for line in reader.lines() {
        if let Some(ts) = line_timestamp(&line?) {
            return Ok(Some(ts));
        }
    }
    Ok(None)
}

/// Writes the log lines at or after `cutoff` into `out`, returning how many
/// uncompressed bytes were written. `already_written` carries the running total
/// across both log generations so the size cap still bounds the whole slice —
/// the cap counts plaintext, which is what a runaway debug log actually
/// produces, not the deflated result.
fn filter_log_lines<W: Write>(
    reader: impl BufRead,
    cutoff: NaiveDateTime,
    out: &mut W,
    already_written: usize,
) -> std::io::Result<usize> {
    let mut include = false;
    let mut written = 0usize;
    for line in reader.lines() {
        let line = line?;
        if let Some(ts) = line_timestamp(&line) {
            include = ts >= cutoff;
        }
        if include {
            if already_written + written + line.len() + 1 > MAX_SLICE_BYTES {
                return Err(std::io::Error::other("log slice is too big"));
            }
            out.write_all(line.as_bytes())?;
            out.write_all(b"\n")?;
            written += line.len() + 1;
        }
    }
    Ok(written)
}

/// Parses the timestamp prefix of a log line, None for continuation lines.
fn line_timestamp(line: &str) -> Option<NaiveDateTime> {
    let prefix = line.get(..LOG_LINE_TS_LEN)?;
    NaiveDateTime::parse_from_str(prefix, LOG_LINE_TS_FORMAT).ok()
}

/// Opens an in-memory single-entry zip and positions it at the `log.txt` entry,
/// ready to be written line by line. Split from `finish_zip` so the caller can
/// stream into it instead of handing over a fully-materialised slice — see
/// `collect_zipped_logs`.
fn start_zip() -> std::io::Result<zip::ZipWriter<std::io::Cursor<Vec<u8>>>> {
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .start_file("log.txt", options)
        .map_err(std::io::Error::other)?;
    Ok(writer)
}

/// Closes the entry and returns the finished archive bytes.
fn finish_zip(writer: zip::ZipWriter<std::io::Cursor<Vec<u8>>>) -> std::io::Result<Vec<u8>> {
    Ok(writer.finish().map_err(std::io::Error::other)?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn ts(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, LOG_LINE_TS_FORMAT).unwrap()
    }

    #[test]
    fn period_days_parses_bounded_day_counts() {
        assert_eq!(period_days("1d"), Some(1));
        assert_eq!(period_days("7d"), Some(7));
        assert_eq!(period_days("30d"), Some(30));
        assert_eq!(period_days("2d"), Some(2)); // forward-compatible with new enum values
        assert_eq!(period_days("0d"), None);
        assert_eq!(period_days("366d"), None);
        assert_eq!(period_days("2w"), None);
        assert_eq!(period_days(""), None);
    }

    #[test]
    fn line_timestamp_parses_log_prefix() {
        assert_eq!(
            line_timestamp("2026-07-08 17:22:49.151 WARN  [mqtt] something"),
            Some(ts("2026-07-08 17:22:49.151"))
        );
        assert_eq!(line_timestamp("    continuation line"), None);
        assert_eq!(line_timestamp(""), None);
    }

    #[test]
    fn filter_keeps_entries_after_cutoff_with_continuations() {
        let log = "\
2026-07-01 10:00:00.000 INFO  [a] old entry
  old continuation
2026-07-02 10:00:00.000 INFO  [a] new entry
  new continuation
2026-07-03 10:00:00.000 INFO  [a] newer entry
";
        let mut out = Vec::new();
        let written =
            filter_log_lines(Cursor::new(log), ts("2026-07-02 00:00:00.000"), &mut out, 0).unwrap();
        assert_eq!(
            written,
            out.len(),
            "reported count must match what was written"
        );
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("old entry"));
        assert!(!out.contains("old continuation"));
        assert!(out.contains("new entry"));
        assert!(out.contains("new continuation"));
        assert!(out.contains("newer entry"));
    }

    #[test]
    fn filter_drops_leading_continuations_without_timestamp() {
        let log = "orphan continuation\n2026-07-02 10:00:00.000 INFO  [a] entry\n";
        let mut out = Vec::new();
        filter_log_lines(Cursor::new(log), ts("2026-07-01 00:00:00.000"), &mut out, 0).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("orphan"));
        assert!(out.contains("entry"));
    }

    #[test]
    fn streamed_zip_produces_a_readable_archive() {
        // End-to-end of the streaming path: filter straight into the zip, then
        // read the entry back out to prove the archive is well-formed and holds
        // exactly the filtered lines.
        let log = "\
2026-07-01 10:00:00.000 INFO  [a] old entry
2026-07-03 10:00:00.000 INFO  [a] kept entry
";
        let mut writer = start_zip().unwrap();
        let written = filter_log_lines(
            Cursor::new(log),
            ts("2026-07-02 00:00:00.000"),
            &mut writer,
            0,
        )
        .unwrap();
        assert!(written > 0);
        let zipped = finish_zip(writer).unwrap();

        assert_eq!(&zipped[..4], b"PK\x03\x04");

        let mut archive = zip::ZipArchive::new(Cursor::new(zipped)).expect("valid zip");
        let mut entry = archive.by_name("log.txt").expect("log.txt entry");
        let mut content = String::new();
        std::io::Read::read_to_string(&mut entry, &mut content).unwrap();
        assert!(content.contains("kept entry"));
        assert!(!content.contains("old entry"));
        assert_eq!(content.len(), written, "entry must hold every counted byte");
    }

    #[test]
    fn slice_cap_counts_across_both_generations() {
        // The cap bounds the whole slice, not each generation: a second call
        // carrying the running total must refuse to exceed MAX_SLICE_BYTES.
        let log = "2026-07-03 10:00:00.000 INFO  [a] entry\n";
        let mut out = Vec::new();
        let err = filter_log_lines(
            Cursor::new(log),
            ts("2026-07-01 00:00:00.000"),
            &mut out,
            MAX_SLICE_BYTES,
        )
        .unwrap_err();
        assert!(err.to_string().contains("too big"));
    }
}
