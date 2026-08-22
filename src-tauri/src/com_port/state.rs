//! Rack state shown in the UI: which card sits in which slot of which rack,
//! and whether its session is up or busy. Kept apart from the transport and
//! the MQTT loops so the presentation state has exactly one owner.

use crate::global_app_handle::{rack_update_cards, RackCard};

use super::cards::RACK_CARDS_UI;
use super::lock;

/// Puts one discovered rack card into that rack's UI card list (keyed by slot)
/// and re-emits the rack state. Cards without a local config entry are shown
/// too — with no card number.
///
/// Also enforces one row per ICCID across ALL racks: a physical card sits in
/// exactly one slot, so its appearance here removes any stale row another rack
/// still shows (e.g. the old rack's `disconnect` was lost while its connection
/// was down). Without the purge the card would be displayed in two rack blocks,
/// and the stale row would feed `connect_pending_rack_cards` a wrong location.
pub(super) fn update_rack_card_ui(
    rack_id: &str,
    slot: u16,
    iccid: &str,
    card_number: Option<String>,
) {
    let name = card_number
        .as_deref()
        .and_then(|number| crate::config::get_card_config_from_cache(number).and_then(|c| c.name));
    let updates: Vec<(String, Vec<RackCard>)> = {
        let mut ui = lock(&RACK_CARDS_UI);
        let mut updates = Vec::new();
        for (other_id, list) in ui.iter_mut() {
            if other_id == rack_id {
                continue;
            }
            let before = list.len();
            list.retain(|c| c.iccid.as_deref() != Some(iccid));
            if list.len() != before {
                log::info!(
                    "RACK {} | [SPAWN] status=stale_row_purged iccid={} now_in_rack={}",
                    other_id,
                    iccid,
                    rack_id
                );
                updates.push((other_id.clone(), list.clone()));
            }
        }
        let list = ui.entry(rack_id.to_string()).or_default();
        // One row per ICCID also INSIDE the target rack, not only across racks:
        // a card moved slot-to-slot with a lost `disconnect` would otherwise
        // keep both rows, and `set_rack_card_state` (which looks the card up by
        // ICCID and stops at the first hit) could land on the dead one.
        if list
            .iter()
            .any(|c| c.slot != slot && c.iccid.as_deref() == Some(iccid))
        {
            log::info!(
                "RACK {} | [SPAWN] status=stale_row_purged iccid={} now_in_slot={}",
                rack_id,
                iccid,
                slot
            );
        }
        list.retain(|c| c.slot != slot && c.iccid.as_deref() != Some(iccid));
        list.push(RackCard {
            slot,
            iccid: Some(iccid.to_string()),
            card_number,
            name,
            // A freshly discovered card has no session yet; `set_rack_card_state`
            // fills these in once its MQTT loop connects and starts exchanging.
            online: None,
            authentication: None,
        });
        list.sort_by_key(|c| c.slot);
        updates.push((rack_id.to_string(), list.clone()));
        updates
    };
    for (id, cards) in updates {
        rack_update_cards(&id, cards);
    }
}

/// Updates the live session state of one rack card (looked up by the ICCID it
/// was spawned with — `update_rack_card_ui` keeps ICCIDs unique across racks)
/// and re-emits the rack state so the UI can show activity.
///
/// A no-op when the card is not in any list — it may have been removed from
/// its rack while the session task was still winding down, and resurrecting a
/// row for a card that is physically gone would be worse than losing one blink.
pub(super) fn set_rack_card_state(iccid: &str, online: bool, authentication: bool) {
    let update = {
        let mut ui = lock(&RACK_CARDS_UI);
        let mut update = None;
        for (rack_id, list) in ui.iter_mut() {
            let Some(card) = list.iter_mut().find(|c| c.iccid.as_deref() == Some(iccid)) else {
                continue;
            };
            // Skip the emit when nothing actually changed: the activity setter is
            // called on every request of an authentication burst, and re-emitting an
            // identical state would push a rack-state event per APDU to the webview.
            // This early return is what keeps the indicator steady through a burst —
            // the icon is never re-rendered, so its animation never restarts.
            if card.online == Some(online) && card.authentication == Some(authentication) {
                return;
            }
            card.online = Some(online);
            card.authentication = Some(authentication);
            update = Some((rack_id.clone(), list.clone()));
            break;
        }
        update
    };
    if let Some((rack_id, cards)) = update {
        rack_update_cards(&rack_id, cards);
    }
}
