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

use crate::logger::log_file_paths;

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

/// Length of the `YYYY-mm-dd HH:MM:SS.mmm` timestamp prefix of a log line.
const TS_PREFIX_LEN: usize = 23;

/// Timestamp format of a log line prefix (see the fern format in logger.rs).
const TS_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.3f";

/// The request currently being served; the server re-sends the command when
/// the reply is slow, so duplicates must not start a second upload.
static ACTIVE_REQUEST: Mutex<Option<u64>> = Mutex::new(None);

/// Handles a server request published on the app connection.
/// Returns true when the publish was a recognized command request.
pub fn dispatch_request(client: &AsyncClient, log_header: &str, topic: &str, payload: &Value) -> bool {
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
        let mut active = ACTIVE_REQUEST.lock().unwrap();
        if let Some(active_id) = *active {
            // server re-send of the request in flight, or a stray overlap: drop it,
            // the upload already running will produce the command result
            log::warn!(
                "{} [LOGS] status=duplicate_request request_id={} active_request_id={}",
                log_header,
                request_id,
                active_id
            );
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
        run_upload(&client, &log_header, request_id, &period).await;
        *ACTIVE_REQUEST.lock().unwrap() = None;
    });
    true
}

/// Collects, zips and publishes the log slice; reports failures to the server
/// through the `done` topic so the command fails with a readable error.
async fn run_upload(client: &AsyncClient, log_header: &str, request_id: u64, period: &str) {
    let done_topic = format!("logs/{}/done", request_id);

    let days = match period_days(period) {
        Some(days) => days,
        None => {
            log::warn!("{} [LOGS] status=bad_period period={}", log_header, period);
            publish_json(client, log_header, &done_topic, json!({"error": format!("unsupported log period '{}'", period)})).await;
            return;
        }
    };

    // file IO and zipping are blocking work, keep them off the async workers
    let period_owned = period.to_string();
    let collected = async_runtime::spawn_blocking(move || collect_zipped_logs(days, &period_owned)).await;
    let (name, data) = match collected {
        Ok(Ok(collected)) => collected,
        Ok(Err(e)) => {
            log::warn!("{} [LOGS] status=collect_failed err={}", log_header, e);
            publish_json(client, log_header, &done_topic, json!({"error": e})).await;
            return;
        }
        Err(e) => {
            log::error!("{} [LOGS] status=collect_task_failed err={}", log_header, e);
            publish_json(client, log_header, &done_topic, json!({"error": "log collection task failed"})).await;
            return;
        }
    };

    let mut chunks = 0usize;
    for chunk in data.chunks(CHUNK_SIZE) {
        let topic = format!("logs/{}/{}", request_id, chunks);
        let published = tokio::time::timeout(
            PUBLISH_TIMEOUT,
            client.publish(topic, QoS::AtLeastOnce, false, chunk.to_vec()),
        )
        .await;
        match published {
            Ok(Ok(())) => {}
            // the connection queue broke or stalled: the server will time the command
            // out, nothing else meaningful can be delivered on this connection
            Ok(Err(e)) => {
                log::error!("{} [LOGS] status=chunk_publish_failed chunk={} err={:?}", log_header, chunks, e);
                return;
            }
            Err(_) => {
                log::error!("{} [LOGS] status=chunk_publish_timeout chunk={}", log_header, chunks);
                return;
            }
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

async fn publish_json(client: &AsyncClient, log_header: &str, topic: &str, value: Value) {
    let published = tokio::time::timeout(
        PUBLISH_TIMEOUT,
        client.publish(topic.to_string(), QoS::AtLeastOnce, false, value.to_string()),
    )
    .await;
    match published {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::error!("{} [LOGS] status=done_publish_failed topic={} err={:?}", log_header, topic, e),
        Err(_) => log::error!("{} [LOGS] status=done_publish_timeout topic={}", log_header, topic),
    }
}

/// Maps the wire period value to a day count.
fn period_days(period: &str) -> Option<i64> {
    match period {
        "1d" => Some(1),
        "7d" => Some(7),
        "30d" => Some(30),
        _ => None,
    }
}

/// Reads the log slice for the last `days` days and returns the zip file
/// name and content. The current log is read first; when it does not reach
/// back to the period start, the archived generation is prepended.
fn collect_zipped_logs(days: i64, period: &str) -> Result<(String, Vec<u8>), String> {
    let cutoff = Local::now().naive_local() - Duration::days(days);
    let (current_path, archived_path) = log_file_paths();

    // the current log covers the whole period when its first entry is older than the cutoff
    let first_ts = first_entry_timestamp(&current_path).map_err(|e| format!("cannot read log file: {}", e))?;
    let archive_needed = first_ts.map_or(true, |ts| ts > cutoff);

    let mut data = Vec::new();
    if archive_needed && archived_path.exists() {
        filter_log_file(&archived_path, cutoff, &mut data).map_err(|e| format!("cannot read archived log file: {}", e))?;
    }
    if current_path.exists() {
        filter_log_file(&current_path, cutoff, &mut data).map_err(|e| format!("cannot read log file: {}", e))?;
    }
    if data.is_empty() {
        return Err(format!("no log entries for the last {} day(s)", days));
    }

    let name = format!("tba_logs_{}_{}.zip", Local::now().format("%Y%m%d_%H%M"), period);
    let zipped = zip_log(&data).map_err(|e| format!("cannot zip log data: {}", e))?;
    Ok((name, zipped))
}

/// Returns the timestamp of the first timestamped line of the file,
/// None when the file is missing, empty or holds no timestamped lines.
fn first_entry_timestamp(path: &Path) -> std::io::Result<Option<NaiveDateTime>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let reader = BufReader::new(file);
    for line in reader.lines() {
        if let Some(ts) = line_timestamp(&line?) {
            return Ok(Some(ts));
        }
    }
    Ok(None)
}

/// Appends the lines of the log file dated at or after the cutoff to `out`.
/// Lines without a timestamp prefix (continuations) follow the fate of the
/// last timestamped line before them.
fn filter_log_file(path: &Path, cutoff: NaiveDateTime, out: &mut Vec<u8>) -> std::io::Result<()> {
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    filter_log_lines(reader, cutoff, out)
}

fn filter_log_lines<R: BufRead>(reader: R, cutoff: NaiveDateTime, out: &mut Vec<u8>) -> std::io::Result<()> {
    let mut include = false;
    for line in reader.lines() {
        let line = line?;
        if let Some(ts) = line_timestamp(&line) {
            include = ts >= cutoff;
        }
        if include {
            if out.len() + line.len() + 1 > MAX_SLICE_BYTES {
                return Err(std::io::Error::other("log slice is too big"));
            }
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
    }
    Ok(())
}

/// Parses the timestamp prefix of a log line, None for continuation lines.
fn line_timestamp(line: &str) -> Option<NaiveDateTime> {
    let prefix = line.get(..TS_PREFIX_LEN)?;
    NaiveDateTime::parse_from_str(prefix, TS_FORMAT).ok()
}

/// Packs the collected log slice into a single-entry zip archive.
fn zip_log(data: &[u8]) -> zip::result::ZipResult<Vec<u8>> {
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer.start_file("log.txt", options)?;
    writer.write_all(data)?;
    Ok(writer.finish()?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn ts(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.3f").unwrap()
    }

    #[test]
    fn period_days_maps_wire_values_only() {
        assert_eq!(period_days("1d"), Some(1));
        assert_eq!(period_days("7d"), Some(7));
        assert_eq!(period_days("30d"), Some(30));
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
        filter_log_lines(Cursor::new(log), ts("2026-07-02 00:00:00.000"), &mut out).unwrap();
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
        filter_log_lines(Cursor::new(log), ts("2026-07-01 00:00:00.000"), &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("orphan"));
        assert!(out.contains("entry"));
    }

    #[test]
    fn zip_log_produces_zip_magic() {
        let zipped = zip_log(b"2026-07-02 10:00:00.000 INFO  [a] entry\n").unwrap();
        assert_eq!(&zipped[..4], b"PK\x03\x04");
    }
}
