//! The server's card sets of a rack (`rack/<id>/cards`) and the local
//! pending-card retry: both reconcile the UI card list of a rack and hand the
//! cards to serve to the session registry.

use std::collections::HashMap;

use crate::com_port::rack::take_rebind;
use crate::com_port::state::update_rack_card_ui;
use crate::com_port::transport::SharedPort;
use crate::com_port::{linked_rack_ports, lock, rack_port_is_live};
use crate::global_app_handle::rack_update_cards;

use super::sessions::{spawn_rack_card_checked, SpawnOrigin};
use super::{bump_card_epoch, card_epoch, RACK_CARDS_UI, RACK_CARD_EPOCH, RACK_CARD_TASKS};

/// Opens rack-backed sessions for discovered cards that currently have none.
/// Covers two situations the server will not retry on its own (it repeats the
/// card set only when the rack content changes):
///  * a card whose ICCID was unknown at discovery time and has since been
///    assigned a number in the UI (config change);
///  * a card whose spawn was skipped because a reader-backed session served
///    the same number (`served_by_reader`) and that reader session has since
///    been torn down (see the hook in `mqtt::shutdown_connections`).
pub async fn connect_pending_rack_cards() {
    // Live racks and their serial ports.
    let ports: Vec<(String, SharedPort)> = linked_rack_ports();
    if ports.is_empty() {
        return; // no rack connected
    }

    // Epochs before the rows: a card set or a teardown that lands after this
    // point makes the snapshot below stale, and its spawns are dropped.
    let epochs: HashMap<String, u64> = lock(&RACK_CARD_EPOCH).clone();

    // UI rows first, live-session filter second — the two locks are taken
    // sequentially, never nested (abort paths lock them in the other order).
    let rows: Vec<(String, u16, String)> = {
        let ui = lock(&RACK_CARDS_UI);
        ui.iter()
            .flat_map(|(rack_id, cards)| {
                cards.iter().filter_map(|card| {
                    card.iccid
                        .clone()
                        .map(|iccid| (rack_id.clone(), card.slot, iccid))
                })
            })
            .collect()
    };
    let pending: Vec<(String, u16, String)> = {
        let tasks = lock(&RACK_CARD_TASKS);
        rows.into_iter()
            .filter(|(_, _, iccid)| {
                // Only rows without a live session are pending; rows already
                // served skip the whole resolve/spawn round-trip here.
                tasks
                    .get(iccid)
                    .map(|task| task.handle.inner().is_finished())
                    .unwrap_or(true)
            })
            .collect()
    };

    for (rack_id, slot, iccid) in pending {
        let Some(card_number) = crate::config::find_card_number_by_iccid(&iccid) else {
            continue; // still unassigned
        };
        let Some((_, port)) = ports.iter().find(|(id, _)| *id == rack_id) else {
            continue; // that rack is gone
        };
        let epoch = epochs.get(&rack_id).copied().unwrap_or(0);
        if card_epoch(&rack_id) != epoch {
            continue; // a newer card set of this rack owns its rows now
        }
        log::info!(
            "RACK | [SPAWN] status=pending_card_resolved rack={} slot={} iccid={} card={}",
            rack_id,
            slot,
            iccid,
            card_number
        );
        update_rack_card_ui(&rack_id, slot, &iccid, Some(card_number.clone()), || {
            card_epoch(&rack_id) == epoch
        });
        spawn_rack_card_checked(
            card_number,
            iccid,
            slot,
            rack_id.clone(),
            port.clone(),
            "RACK |",
            SpawnOrigin::Replay { epoch },
        )
        .await;
    }
}

/// Parses the server's card set of a rack (`rack/<id>/cards`): a JSON array of
/// `{"slot":N,"iccid":"<16 hex>"}`. The whole set is rejected on any invalid
/// entry (a slot outside 1..=240, an empty ICCID, a slot or an ICCID listed
/// twice): reconciling the sessions against a half-understood set could close
/// a session that is fine.
pub(in crate::com_port) fn parse_cards_set(payload: &[u8]) -> Result<Vec<(u16, String)>, String> {
    let json: serde_json::Value =
        serde_json::from_slice(payload).map_err(|e| format!("bad_json: {e}"))?;
    let items = json.as_array().ok_or_else(|| "not_an_array".to_string())?;
    let mut cards: Vec<(u16, String)> = Vec::with_capacity(items.len());
    for item in items {
        let slot = item.get("slot").and_then(|v| v.as_u64()).unwrap_or(0);
        let iccid = item.get("iccid").and_then(|v| v.as_str()).unwrap_or("");
        if iccid.is_empty() || !(1..=u64::from(crate::com_port::SLOT_MAX)).contains(&slot) {
            return Err(format!("invalid_iccid_or_slot slot={slot}"));
        }
        if cards.iter().any(|(s, _)| u64::from(*s) == slot) {
            return Err(format!("duplicate_slot slot={slot}"));
        }
        if cards.iter().any(|(_, i)| i == iccid) {
            return Err(format!("duplicate_iccid slot={slot}"));
        }
        cards.push((slot as u16, iccid.to_string()));
    }
    Ok(cards)
}

/// A card set of the server (`rack/<id>/cards`, published at the end of every
/// discovery chain): reconciles the card sessions, then tells the UI the scan
/// of the rack is over.
pub(in crate::com_port) async fn handle_cards_set(
    cards: Vec<(u16, String)>,
    epoch: u64,
    rack_id: String,
    serial_port: SharedPort,
    log_header: String,
) {
    if apply_cards_set(cards, Some(epoch), rack_id.clone(), serial_port, log_header).await {
        // The set closes a discovery chain of the server: the UI can stop showing
        // a scan in progress. Treated as a hint, not a contract: the frontend
        // still has its own timeout in case a set never arrives.
        crate::global_app_handle::rack_mark_scan_complete(&rack_id);
    }
}

/// Reconciles the card sessions of a rack with a card set; `false` when the
/// set was ignored or superseded. Says nothing about the scan of the rack: a
/// set of the local engine can come before its cards are identified.
///
/// The set is the desired state, not a notice: a listed card without a
/// session gets one (ICCID resolved to the card number through the local
/// config; an unknown ICCID is shown in the UI, unserved), a session of this
/// rack whose (slot, ICCID) the set does not list is closed. The key is the
/// pair, not the ICCID alone: a session is bound to its slot, so a card that
/// moved to another slot gets a new session there.
///
/// `epoch` is the card-set epoch the set was given on arrival (a server set);
/// `None` takes a new one here (a set of the local engine, applied in order).
pub(in crate::com_port) async fn apply_cards_set(
    cards: Vec<(u16, String)>,
    epoch: Option<u64>,
    rack_id: String,
    serial_port: SharedPort,
    log_header: String,
) -> bool {
    // A set dispatched before an unlink/relink belongs to the previous life of
    // the rack: taking an epoch would let it write that life's rows.
    if !rack_port_is_live(&rack_id, &serial_port) {
        log::info!("{} [CARDS] status=ignored reason=port_replaced", log_header);
        return false;
    }
    let epoch = epoch.unwrap_or_else(|| bump_card_epoch(&rack_id));
    // Unlinked and relinked between the check and the bump: the epoch taken belongs to the new life.
    if !rack_port_is_live(&rack_id, &serial_port) {
        log::info!("{} [CARDS] status=ignored reason=port_replaced", log_header);
        return false;
    }
    // A set that arrived later already took over.
    if card_epoch(&rack_id) != epoch {
        log::info!(
            "{} [CARDS] status=superseded reason=newer_card_set",
            log_header
        );
        return false;
    }
    // a set that follows a `link up` closes a full re-discovery: the sessions it
    // lists re-publish their link reports, every other set leaves them alone
    let rebind = take_rebind(&rack_id);
    log::info!(
        "{} [CARDS] status=set_received count={} rebind={}",
        log_header,
        cards.len(),
        rebind
    );
    log::debug!(
        "{} [CARDS] set={}",
        log_header,
        cards
            .iter()
            .map(|(slot, iccid)| format!("{}:{}", slot, iccid))
            .collect::<Vec<_>>()
            .join(",")
    );
    let listed = |slot: u16, iccid: &str| cards.iter().any(|(s, i)| *s == slot && i == iccid);

    // Sessions of this rack the set no longer lists: the card left its slot
    // (or moved to another one, where the set lists it anew).
    {
        let mut tasks = lock(&RACK_CARD_TASKS);
        let gone: Vec<String> = tasks
            .iter()
            .filter(|(iccid, task)| task.rack_id == rack_id && !listed(task.slot, iccid))
            .map(|(iccid, _)| iccid.clone())
            .collect();
        for iccid in gone {
            if let Some(task) = tasks.remove(&iccid) {
                task.handle.abort();
                log::info!(
                    "{} [CARDS] card={} slot={} status=aborted reason=not_in_set iccid={}",
                    log_header,
                    task.card_number,
                    task.slot,
                    iccid
                );
            }
        }
    }

    // UI rows of this rack the set no longer lists, same rule. Outside the
    // tasks lock (lock-order discipline with the UI paths).
    let remaining = {
        let mut ui = lock(&RACK_CARDS_UI);
        ui.get_mut(&rack_id).and_then(|list| {
            let before = list.len();
            list.retain(|c| {
                c.iccid
                    .as_deref()
                    .is_some_and(|iccid| listed(c.slot, iccid))
            });
            (list.len() != before).then(|| list.clone())
        })
    };
    if let Some(list) = remaining {
        rack_update_cards(&rack_id, list);
    }

    // Every listed card lands in this rack's UI section, configured or not,
    // and the configured ones get a session unless one already serves them.
    for (slot, iccid) in &cards {
        // A newer set (or a teardown) took over while this one was waiting:
        // its rows and sessions are the truth, ours are outdated.
        if card_epoch(&rack_id) != epoch {
            log::info!(
                "{} [CARDS] status=superseded reason=newer_card_set",
                log_header
            );
            return false;
        }
        // one INFO line per card: the rack inventory is readable straight from the trace
        let card_number = crate::config::find_card_number_by_iccid(iccid);
        match &card_number {
            Some(number) => log::info!(
                "{} [CARDS] status=listed slot={} iccid={} card={}",
                log_header,
                slot,
                iccid,
                number
            ),
            None => log::warn!(
                "{} [CARDS] status=not_served reason=unknown_card slot={} iccid={}",
                log_header,
                slot,
                iccid
            ),
        }
        update_rack_card_ui(&rack_id, *slot, iccid, card_number.clone(), || {
            card_epoch(&rack_id) == epoch
        });
        let Some(card_number) = card_number else {
            continue;
        };
        spawn_rack_card_checked(
            card_number,
            iccid.clone(),
            *slot,
            rack_id.clone(),
            serial_port.clone(),
            &log_header,
            SpawnOrigin::ServerSet { rebind, epoch },
        )
        .await;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic ICCIDs only: no real card data in the public repo.
    const A: &str = "0000000000000001";
    const B: &str = "0000000000000002";

    #[test]
    fn cards_set_parses_slots_and_iccids() {
        let payload = format!(r#"[{{"iccid":"{A}","slot":1}},{{"iccid":"{B}","slot":3}}]"#);
        assert_eq!(
            parse_cards_set(payload.as_bytes()).unwrap(),
            vec![(1, A.to_string()), (3, B.to_string())]
        );
    }

    #[test]
    fn cards_set_may_be_empty() {
        // an empty rack, or a discovery that found nothing: every session goes
        assert_eq!(parse_cards_set(b"[]").unwrap(), Vec::<(u16, String)>::new());
    }

    #[test]
    fn cards_set_rejects_malformed_entries_as_a_whole() {
        let bad = [
            "not json".to_string(),
            r#"{"iccid":"x","slot":1}"#.to_string(), // an object, not an array
            format!(r#"[{{"iccid":"{A}","slot":0}}]"#), // slot below the range
            format!(r#"[{{"iccid":"{A}","slot":241}}]"#), // slot above the range
            r#"[{"iccid":"","slot":1}]"#.to_string(), // empty ICCID
            r#"[{"slot":1}]"#.to_string(),           // no ICCID
            format!(r#"[{{"iccid":"{A}"}}]"#),       // no slot
            format!(r#"[{{"iccid":"{A}","slot":1}},{{"iccid":"{B}","slot":1}}]"#), // slot twice
            format!(r#"[{{"iccid":"{A}","slot":1}},{{"iccid":"{A}","slot":2}}]"#), // ICCID twice
        ];
        for payload in bad {
            assert!(
                parse_cards_set(payload.as_bytes()).is_err(),
                "payload {payload} must be rejected"
            );
        }
    }
}
