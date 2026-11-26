//! The config side of the card list synchronization (see `cards_sync`): the
//! part of a card entry that is synchronized, the state of the last
//! synchronization, and the replacement of the local list by the server copy.

// ───── Std Lib ─────
use std::collections::{BTreeMap, HashSet};
use std::fs;

// ───── External Crates ─────
use serde::{Deserialize, Serialize};

// ───── Local Modules ─────
use super::cache::{cache_guard, load_config_to_cache};
use super::store::{config_write_guard, get_config_path, load_config, save_config};
use super::types::{is_valid_card_number, CardConfig, SyncState};
use crate::global_app_handle::emit_card_config_event;

/// The synchronized part of a card entry: what links a physical card to its
/// number plus what the user set by hand. Everything else in `CardConfig` is
/// read off the card or recorded by this instance and is not carried over.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SyncedCard {
    #[serde(default)]
    pub iccid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t_protocol: Option<String>,
}

/// The card list as it is synchronized, keyed (and so ordered) by card number.
pub type SyncedCards = BTreeMap<String, SyncedCard>;

/// The synchronized view of the current card list.
pub fn synced_cards() -> SyncedCards {
    let cache = cache_guard();
    cache
        .cards
        .iter()
        .map(|(number, card)| {
            (
                number.clone(),
                SyncedCard {
                    iccid: card.iccid.clone(),
                    name: card.name.clone(),
                    t_protocol: card.t_protocol.clone(),
                },
            )
        })
        .collect()
}

/// The server copy the list was last synchronized with; `None` = never.
pub fn sync_state() -> Option<SyncState> {
    cache_guard().sync.clone()
}

/// Records the server copy the list is now in sync with. Blocking disk I/O.
pub fn set_sync_state(state: SyncState) -> Result<(), String> {
    let config_path = get_config_path().map_err(|e| e.to_string())?;
    let _guard = config_write_guard();
    let mut config = load_config(&config_path).map_err(|e| e.to_string())?;
    if config.sync.as_ref() == Some(&state) {
        return Ok(());
    }
    config.sync = Some(state);
    save_config(&config_path, &config).map_err(|e| e.to_string())?;
    load_config_to_cache(&config).map_err(|e| e.to_string())
}

/// Replaces the local card list by the server copy and records `state` with
/// it, in one write. The file as it was is kept next to it as
/// `config.yaml.before-sync`. A card that stays keeps what this instance
/// knows about it beyond the synchronized fields. Returns the numbers of the
/// removed cards: the caller ends their sessions. Blocking disk I/O.
pub fn replace_synced_cards(cards: SyncedCards, state: SyncState) -> Result<Vec<String>, String> {
    let config_path = get_config_path().map_err(|e| e.to_string())?;
    let _guard = config_write_guard();
    let mut config = load_config(&config_path).map_err(|e| e.to_string())?;

    let mut backup_name = config_path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    backup_name.push(".before-sync");
    let backup_path = config_path.with_file_name(backup_name);
    // Without the copy the replaced list could not be brought back.
    fs::copy(&config_path, &backup_path)
        .map_err(|e| format!("cannot back up the configuration: {e}"))?;

    let removed: Vec<String> = config
        .cards
        .keys()
        .filter(|number| !cards.contains_key(*number))
        .cloned()
        .collect();
    for number in &removed {
        config.cards.remove(number);
    }
    let mut updated: Vec<String> = Vec::new();
    let mut linked_iccids: HashSet<String> = HashSet::new();
    for (number, synced) in cards {
        // Same rules as a card saved from the form: the number becomes an
        // MQTT client id, the ICCID is matched against what a card reports.
        if !is_valid_card_number(&number) || !synced.iccid.bytes().all(|b| b.is_ascii_hexdigit()) {
            log::error!(
                "[SYNC] status=card_skipped reason=invalid card_number={}",
                number
            );
            continue;
        }
        // One physical card belongs to one number (see `update_card_config`):
        // of two entries with the same ICCID the first in number order keeps it.
        if !synced.iccid.is_empty() && !linked_iccids.insert(synced.iccid.clone()) {
            log::error!(
                "[SYNC] status=card_skipped reason=duplicate_iccid card_number={} iccid={}",
                number,
                synced.iccid
            );
            continue;
        }
        match config.cards.get_mut(&number) {
            Some(existing) => {
                existing.iccid = synced.iccid;
                existing.name = synced.name;
                existing.t_protocol = synced.t_protocol;
            }
            None => {
                config.cards.insert(
                    number.clone(),
                    CardConfig {
                        iccid: synced.iccid,
                        expire: None,
                        name: synced.name,
                        t_protocol: synced.t_protocol,
                        card_type: None,
                        structure_version: None,
                        company_name: None,
                        company_address: None,
                        last_auth: None,
                    },
                );
            }
        }
        updated.push(number);
    }
    config.sync = Some(state);

    save_config(&config_path, &config).map_err(|e| e.to_string())?;
    load_config_to_cache(&config).map_err(|e| e.to_string())?;

    for number in &removed {
        emit_card_config_event("global-card-config-updated", number.clone(), None);
    }
    for number in updated {
        let card = config.cards.get(&number).cloned();
        emit_card_config_event("global-card-config-updated", number, card);
    }
    Ok(removed)
}
