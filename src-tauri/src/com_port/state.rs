//! Rack state shown in the UI: which card sits in which slot, and whether its
//! session is up or busy. Kept apart from the transport and the MQTT loops so
//! the presentation state has exactly one owner.

use crate::global_app_handle::{rack_update_cards, RackCard};

use super::cards::RACK_CARDS_UI;

/// Puts one discovered rack card into the UI card list (keyed by slot) and re-emits the
/// rack state. Cards without a local config entry are shown too — with no card number.
pub(super) fn update_rack_card_ui(slot: u16, iccid: &str, card_number: Option<String>) {
    let name = card_number
        .as_deref()
        .and_then(|number| crate::config::get_card_config_from_cache(number).and_then(|c| c.name));
    let cards = {
        let mut ui = match RACK_CARDS_UI.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        ui.retain(|c| c.slot != slot);
        ui.push(RackCard {
            slot,
            iccid: Some(iccid.to_string()),
            card_number,
            name,
            // A freshly discovered card has no session yet; `set_rack_card_state`
            // fills these in once its MQTT loop connects and starts exchanging.
            online: None,
            authentication: None,
        });
        ui.sort_by_key(|c| c.slot);
        ui.clone()
    };
    rack_update_cards(cards);
}

/// Updates the live session state of one rack card (looked up by the ICCID it
/// was spawned with) and re-emits the rack state so the UI can show activity.
///
/// A no-op when the card is not in the list — it may have been removed from the
/// rack while its session task was still winding down, and resurrecting a row
/// for a card that is physically gone would be worse than losing one blink.
pub(super) fn set_rack_card_state(iccid: &str, online: bool, authentication: bool) {
    let cards = {
        let mut ui = match RACK_CARDS_UI.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(card) = ui.iter_mut().find(|c| c.iccid.as_deref() == Some(iccid)) else {
            return;
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
        ui.clone()
    };
    rack_update_cards(cards);
}
