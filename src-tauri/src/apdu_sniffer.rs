// Passive APDU sniffer.
//
// TBA is a transparent proxy between flespi (VU role) and the smart card.
// After Gen1 mutual authentication flespi wraps every APDU in Secure Messaging
// (CLA=0C), but for tachograph cards the SM wrapper uses DO'81 (plain value) +
// DO'8E (MAC) — no DO'87 (encrypted value). That means the file content flows
// through TBA in cleartext; only integrity is protected.
//
// This module observes (CAPDU, RAPDU) pairs and, when a READ BINARY response
// arrives for a previously-SELECTed EF we recognise (0520 EF_Identification,
// 0501 EF_Application_Identification), decodes and logs the parsed fields.

use std::collections::HashMap;
use std::sync::Mutex;

use lazy_static::lazy_static;

struct SniffState {
    /// FID of the most recently SELECTed EF (plain or SM-wrapped).
    last_selected_ef: Option<u16>,
}

lazy_static! {
    /// Per-client_id sniffer state. client_id == card number.
    static ref STATE: Mutex<HashMap<String, SniffState>> = Mutex::new(HashMap::new());
}

/// Drop the per-client sniffer state. Call when a card/connection is being
/// removed so the global HashMap does not grow without bound over long runs.
pub fn forget(client_id: &str) {
    if let Ok(mut state) = STATE.lock() {
        state.remove(client_id);
    }
}

/// Drop sniffer state for every client. Used when wiping the whole task pool.
pub fn forget_all() {
    if let Ok(mut state) = STATE.lock() {
        state.clear();
    }
}

/// Observe a CAPDU/RAPDU exchange. client_id is the card number.
pub fn sniff(client_id: &str, command_hex: &str, response_hex: &str) {
    let cmd = match hex::decode(command_hex) {
        Ok(v) => v,
        Err(_) => return,
    };
    let resp = match hex::decode(response_hex) {
        Ok(v) => v,
        Err(_) => return,
    };

    // Track SELECTed EF (ignore SELECT AID / SELECT MF)
    if let Some(fid) = select_ef_fid(&cmd) {
        if let Ok(mut state) = STATE.lock() {
            state
                .entry(client_id.to_string())
                .or_insert(SniffState {
                    last_selected_ef: None,
                })
                .last_selected_ef = Some(fid);
        }
        return;
    }

    // READ BINARY — check if we have a known EF context and plaintext data to parse
    if !is_read_binary(&cmd) {
        return;
    }

    let fid = {
        let Ok(state) = STATE.lock() else {
            return;
        };
        state.get(client_id).and_then(|s| s.last_selected_ef)
    };
    let Some(fid) = fid else {
        return;
    };

    let Some(data) = extract_plain_body(&resp) else {
        return;
    };

    match fid {
        0x0520 => parse_ef_identification(client_id, &data),
        0x0501 => parse_ef_application_identification(client_id, &data),
        _ => {}
    }
}

/// Returns the FID if `cmd` is a SELECT EF under current DF.
/// Handles both plain (`00 A4 02 0C 02 HI LO`) and SM-wrapped
/// (`0C A4 02 0C Lc 81 02 HI LO ... 8E 08 <MAC> 00`) forms.
fn select_ef_fid(cmd: &[u8]) -> Option<u16> {
    if cmd.len() < 7 || cmd[1] != 0xA4 || cmd[2] != 0x02 {
        return None;
    }
    // Plain: 00 A4 02 0C 02 HI LO
    if cmd[0] == 0x00 && cmd[4] == 0x02 && cmd.len() >= 7 {
        return Some(u16::from_be_bytes([cmd[5], cmd[6]]));
    }
    // SM: 0C A4 02 0C Lc 81 02 HI LO ...
    if cmd[0] == 0x0C && cmd.len() >= 9 && cmd[5] == 0x81 && cmd[6] == 0x02 {
        return Some(u16::from_be_bytes([cmd[7], cmd[8]]));
    }
    None
}

fn is_read_binary(cmd: &[u8]) -> bool {
    cmd.len() >= 2 && (cmd[0] == 0x00 || cmd[0] == 0x0C) && cmd[1] == 0xB0
}

/// Extracts plaintext body from a RAPDU.
/// - SM response: body is the value of DO'81 (plain value), expected as the
///   first data object before DO'99/DO'8E and the trailing SW.
/// - Plain response: body is everything except the trailing 2-byte SW.
///
/// Returns None if the response has no payload (e.g. only SW).
fn extract_plain_body(resp: &[u8]) -> Option<Vec<u8>> {
    if resp.len() < 2 {
        return None;
    }
    let body = &resp[..resp.len() - 2];
    if body.is_empty() {
        return None;
    }

    // SM-wrapped: starts with DO'81
    if body[0] == 0x81 {
        let (len, len_bytes) = ber_length(&body[1..])?;
        let start = 1 + len_bytes;
        if start + len <= body.len() {
            return Some(body[start..start + len].to_vec());
        }
        return None;
    }

    // SM with encrypted body (DO'87) — cannot decode without session keys
    if body[0] == 0x87 {
        return None;
    }

    // Plain response: body is the data
    Some(body.to_vec())
}

/// Parses BER-TLV length: returns (length, bytes_consumed).
fn ber_length(data: &[u8]) -> Option<(usize, usize)> {
    if data.is_empty() {
        return None;
    }
    let first = data[0];
    if first < 0x80 {
        return Some((first as usize, 1));
    }
    let num = (first & 0x7F) as usize;
    if num == 0 || num > 4 || data.len() < 1 + num {
        return None;
    }
    let mut len = 0usize;
    for i in 0..num {
        len = (len << 8) | (data[1 + i] as usize);
    }
    Some((len, 1 + num))
}

// ─────────── Field parsers ───────────

/// Parses EF_Identification (Annex 1C §2.24 CardIdentification + holder block).
/// Logs all fields and persists the subset we track (expire, company_name,
/// company_address) into the card's config if values changed.
fn parse_ef_identification(client_id: &str, data: &[u8]) {
    log::debug!(
        "EF_Identification (0520) plaintext ({} bytes): {}",
        data.len(),
        hex::encode(data)
    );

    if let Some(b) = slice(data, 0, 1) {
        log::info!("  cardIssuingMemberState: 0x{:02X} ({})", b[0], b[0]);
    }
    if let Some(b) = slice(data, 1, 16) {
        log::info!("  cardNumber: \"{}\" (raw={})", ia5(b), hex::encode(b));
    }
    if let Some(b) = slice(data, 17, 36) {
        log::info!("  cardIssuingAuthorityName: {}", name_str(b));
    }
    if let Some(b) = slice(data, 53, 4) {
        log::info!("  cardIssueDate: {}", time_real(b));
    }
    if let Some(b) = slice(data, 57, 4) {
        log::info!("  cardValidityBegin: {}", time_real(b));
    }
    if let Some(b) = slice(data, 61, 4) {
        log::info!("  cardExpiryDate: {}", time_real(b));
    }

    // Company Card holder block
    if data.len() >= 65 + 74 {
        if let Some(b) = slice(data, 65, 36) {
            log::info!("  companyName: {}", name_str(b));
        }
        if let Some(b) = slice(data, 101, 36) {
            log::info!("  companyAddress: {}", name_str(b));
        }
        if let Some(b) = slice(data, 137, 2) {
            log::info!(
                "  cardHolderPreferredLanguage: \"{}\"",
                String::from_utf8_lossy(b)
            );
        }
    }

    // Persist changes to card config.
    // Cheap pre-check against the runtime cache first: the VU re-reads these
    // EFs on every authentication, and in the common no-change case we must
    // not touch the disk at all.
    let Some(cfg) = crate::config::get_card_config_from_cache(client_id) else {
        return;
    };

    // Outer Option = field present in this response; inner value = new content.
    let new_expire = slice(data, 61, 4).map(|b| {
        let ts = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        if ts == 0 {
            None
        } else {
            Some(ts as u64)
        }
    });
    let new_company_name = slice(data, 65, 36).map(extract_name);
    let new_company_address = slice(data, 101, 36).map(extract_name);

    let would_change = new_expire.as_ref().is_some_and(|v| &cfg.expire != v)
        || new_company_name
            .as_ref()
            .is_some_and(|v| &cfg.company_name != v)
        || new_company_address
            .as_ref()
            .is_some_and(|v| &cfg.company_address != v);
    if !would_change {
        return;
    }

    log::debug!("EF_Identification → config update for {}", client_id);

    // sniff() runs on the card's async MQTT task, so the write (file I/O with
    // fsync) is offloaded to the blocking pool. mutate_card_config re-applies
    // the change against fresh file state under the global config lock, so a
    // concurrent writer cannot be reverted by a stale snapshot.
    let client_id = client_id.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let ok = crate::config::mutate_card_config(&client_id, move |card| {
            let mut changed = false;
            if let Some(v) = new_expire {
                if card.expire != v {
                    card.expire = v;
                    changed = true;
                }
            }
            if let Some(v) = new_company_name {
                if card.company_name != v {
                    card.company_name = v;
                    changed = true;
                }
            }
            if let Some(v) = new_company_address {
                if card.company_address != v {
                    card.company_address = v;
                    changed = true;
                }
            }
            changed
        });
        if !ok {
            log::error!(
                "sniffer: failed to persist EF_Identification fields for {}",
                client_id
            );
        }
    });
}

/// Parses EF_Application_Identification for Company Card.
/// Layout: typeOfTachographCardId (1) + cardStructureVersion (2) + noOfCompanyActivityRecords (2).
/// Persists card_type and structure_version into the card's config if changed.
fn parse_ef_application_identification(client_id: &str, data: &[u8]) {
    log::debug!(
        "EF_Application_Identification (0501) plaintext ({} bytes): {}",
        data.len(),
        hex::encode(data)
    );

    if !data.is_empty() {
        let t = data[0];
        let ts = match t {
            1 => "Driver Card",
            2 => "Workshop Card",
            3 => "Control Card",
            4 => "Company Card",
            _ => "Unknown",
        };
        log::info!("  typeOfTachographCardId: 0x{:02X} ({})", t, ts);
    }
    if data.len() >= 3 {
        log::info!("  cardStructureVersion: {:02X}.{:02X}", data[1], data[2]);
    }
    if data.len() >= 5 {
        let n = u16::from_be_bytes([data[3], data[4]]);
        log::info!("  noOfCompanyActivityRecords: {}", n);
    }

    // Persist changes to card config.
    // Cheap pre-check against the runtime cache first — no disk I/O in the
    // common no-change case (the VU reads this EF on every authentication).
    let Some(cfg) = crate::config::get_card_config_from_cache(client_id) else {
        return;
    };

    let new_card_type = if !data.is_empty() {
        Some(data[0])
    } else {
        None
    };
    // Gen2 cards expose EF_Application_Identification under BOTH DF_Tachograph (Gen1, ver 00.00)
    // and DF_Tachograph_G2 (Gen2, ver 01.xx). Keep only the highest version seen —
    // tuple comparison is lexicographic: (0,0) < (1,0) < (1,1) < (1,2) ...
    let new_structure_version = if data.len() >= 3 {
        Some((data[1], data[2]))
    } else {
        None
    };

    let version_is_higher = |current: Option<(u8, u8)>, candidate: (u8, u8)| match current {
        Some(current) => candidate > current,
        None => true,
    };

    let would_change = new_card_type.is_some_and(|t| cfg.card_type != Some(t))
        || new_structure_version.is_some_and(|v| version_is_higher(cfg.structure_version, v));
    if !would_change {
        return;
    }

    log::debug!(
        "EF_Application_Identification → config update for {}",
        client_id
    );

    // Same offload pattern as EF_Identification: blocking write on the blocking
    // pool, change re-applied to fresh file state under the global config lock.
    let client_id = client_id.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let ok = crate::config::mutate_card_config(&client_id, move |card| {
            let mut changed = false;
            if let Some(t) = new_card_type {
                if card.card_type != Some(t) {
                    card.card_type = Some(t);
                    changed = true;
                }
            }
            if let Some(v) = new_structure_version {
                if version_is_higher(card.structure_version, v) {
                    card.structure_version = Some(v);
                    changed = true;
                }
            }
            changed
        });
        if !ok {
            log::error!(
                "sniffer: failed to persist EF_Application_Identification fields for {}",
                client_id
            );
        }
    });
}

// ─────────── Helpers ───────────

fn slice(d: &[u8], start: usize, len: usize) -> Option<&[u8]> {
    if start + len <= d.len() {
        Some(&d[start..start + len])
    } else {
        None
    }
}

/// Trims trailing padding (0x00 / 0xFF) from tachograph fixed-length strings.
fn trim(b: &[u8]) -> &[u8] {
    let end = b
        .iter()
        .rposition(|c| *c != 0xFF && *c != 0x00 && *c != b' ')
        .map(|i| i + 1)
        .unwrap_or(0);
    &b[..end]
}

/// `Name` = codePage (1 byte) + 35 bytes string.
fn name_str(b: &[u8]) -> String {
    if b.is_empty() {
        return String::new();
    }
    let cp = b[0];
    let payload = trim(&b[1..]);
    format!("[cp={}] \"{}\"", cp, String::from_utf8_lossy(payload))
}

/// Extracts just the string value from a `Name` field (codePage byte + 35-byte string).
/// Codepage itself is dropped. Returns None if content is empty after trimming padding.
fn extract_name(b: &[u8]) -> Option<String> {
    if b.is_empty() {
        return None;
    }
    let payload = trim(&b[1..]);
    if payload.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(payload).to_string())
}

/// IA5 fixed-length string, trims padding.
fn ia5(b: &[u8]) -> String {
    String::from_utf8_lossy(trim(b)).into_owned()
}

/// 4-byte TimeReal (unix seconds, big-endian).
fn time_real(b: &[u8]) -> String {
    if b.len() < 4 {
        return "<invalid>".into();
    }
    let ts = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    if ts == 0 {
        return "<unset>".into();
    }
    match chrono::DateTime::<chrono::Utc>::from_timestamp(ts as i64, 0) {
        Some(dt) => format!("{} (ts={})", dt.format("%Y-%m-%d %H:%M:%S UTC"), ts),
        None => format!("ts={}", ts),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_ef_fid_plain_form() {
        // Plain SELECT EF for FID 0x0520: 00 A4 02 0C 02 05 20
        let cmd = [0x00, 0xA4, 0x02, 0x0C, 0x02, 0x05, 0x20];
        assert_eq!(select_ef_fid(&cmd), Some(0x0520));
    }

    #[test]
    fn select_ef_fid_sm_form() {
        // SM-wrapped SELECT EF for FID 0x0501:
        // 0C A4 02 0C Lc 81 02 05 01 ... MAC ... 00
        let cmd = [
            0x0C, 0xA4, 0x02, 0x0C, 0x09, 0x81, 0x02, 0x05, 0x01, 0x8E, 0x04, 0xAA, 0xBB, 0xCC,
            0xDD, 0x00,
        ];
        assert_eq!(select_ef_fid(&cmd), Some(0x0501));
    }

    #[test]
    fn select_ef_fid_rejects_non_select() {
        let cmd = [0x00, 0xB0, 0x00, 0x00, 0x05];
        assert_eq!(select_ef_fid(&cmd), None);
    }

    #[test]
    fn is_read_binary_recognizes_plain_and_sm() {
        assert!(is_read_binary(&[0x00, 0xB0, 0x00, 0x00, 0x05]));
        assert!(is_read_binary(&[0x0C, 0xB0, 0x00, 0x00, 0x05]));
        assert!(!is_read_binary(&[0x00, 0xA4, 0x02, 0x0C]));
    }

    #[test]
    fn extract_plain_body_strips_sw() {
        // Plain response: 12 34 56  + SW 90 00
        let resp = [0x12, 0x34, 0x56, 0x90, 0x00];
        assert_eq!(extract_plain_body(&resp), Some(vec![0x12, 0x34, 0x56]));
    }

    #[test]
    fn extract_plain_body_handles_do81() {
        // DO'81 body of length 3, then DO'8E and SW.
        let resp = [
            0x81, 0x03, 0xAA, 0xBB, 0xCC, 0x8E, 0x02, 0xFF, 0xFF, 0x90, 0x00,
        ];
        assert_eq!(extract_plain_body(&resp), Some(vec![0xAA, 0xBB, 0xCC]));
    }

    #[test]
    fn extract_plain_body_refuses_encrypted_do87() {
        let resp = [0x87, 0x02, 0xAA, 0xBB, 0x90, 0x00];
        assert_eq!(extract_plain_body(&resp), None);
    }

    #[test]
    fn extract_plain_body_handles_too_short() {
        assert_eq!(extract_plain_body(&[]), None);
        assert_eq!(extract_plain_body(&[0x90]), None);
        // Only SW, no payload
        assert_eq!(extract_plain_body(&[0x90, 0x00]), None);
    }

    #[test]
    fn ber_length_short_form() {
        assert_eq!(ber_length(&[0x05]), Some((5, 1)));
        assert_eq!(ber_length(&[0x7F]), Some((127, 1)));
    }

    #[test]
    fn ber_length_long_form() {
        // 0x82 = 2 length bytes follow
        assert_eq!(ber_length(&[0x82, 0x01, 0x23]), Some((0x0123, 3)));
    }

    #[test]
    fn ber_length_rejects_truncated_long_form() {
        assert_eq!(ber_length(&[0x82, 0x01]), None);
        assert_eq!(ber_length(&[0x85]), None); // > 4 bytes claimed
    }

    #[test]
    fn forget_removes_only_target_client() {
        forget_all();
        // Push state for two clients via SELECT
        sniff(
            "clientA",
            "00A4020C02 0520".replace(' ', "").as_str(),
            "9000",
        );
        sniff(
            "clientB",
            "00A4020C02 0501".replace(' ', "").as_str(),
            "9000",
        );
        {
            let state = STATE.lock().unwrap();
            assert!(state.contains_key("clientA"));
            assert!(state.contains_key("clientB"));
        }

        forget("clientA");
        {
            let state = STATE.lock().unwrap();
            assert!(!state.contains_key("clientA"));
            assert!(state.contains_key("clientB"));
        }

        forget_all();
        {
            let state = STATE.lock().unwrap();
            assert!(state.is_empty());
        }
    }

    #[test]
    fn trim_strips_padding() {
        assert_eq!(trim(b"ABC\x00\x00\x00"), b"ABC");
        assert_eq!(trim(b"ABC\xFF\xFF"), b"ABC");
        assert_eq!(trim(b"   ABC   "), b"   ABC");
        assert_eq!(trim(b"\xFF\xFF\xFF"), b"");
    }

    #[test]
    fn extract_name_drops_codepage_and_empty() {
        // codepage=1, content="HELLO" then padding
        let mut buf = vec![0x01];
        buf.extend_from_slice(b"HELLO");
        buf.extend_from_slice(&[0xFF; 30]);
        assert_eq!(extract_name(&buf), Some("HELLO".to_string()));

        // codepage only, no content
        let mut buf2 = vec![0x01];
        buf2.extend_from_slice(&[0xFF; 35]);
        assert_eq!(extract_name(&buf2), None);
    }
}
