//! Writers of the card list, and its replay to the frontend.
//!
//! Two kinds of writers change a card entry: the frontend form saves a whole
//! entry (`persist_card`, which keeps the backend-owned metadata a stale form
//! would clobber), and the backend changes single fields on fresh file state
//! (`mutate_card_config`: the APDU sniffer, authentication results). Every
//! write refreshes the cache and emits `global-card-config-updated`.

// ───── Std Lib ─────
use std::path::Path;

// ───── Local Modules ─────
use super::cache::{cache_guard, load_config_to_cache};
use super::store::{config_write_guard, get_config_path, load_config, save_config};
use super::types::{is_valid_card_number, CardConfig};
use crate::global_app_handle::emit_card_config_event;
use crate::mqtt::remove_connections;

/// Updates the configuration with a new card.
/// This function updates the configuration file with a new card's ATR and card number.
fn update_card_config(
    config_path: &Path,
    card_number: &str,
    content: CardConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Serialize the whole read-modify-write against all other config writers.
    let _guard = config_write_guard();

    let mut config = load_config(config_path)?;
    log::debug!("Loaded configuration: {:?}", config);

    // The card number becomes the MQTT client id and part of its topics: a new
    // entry must have the card number format (the dialog checks it too). An
    // existing entry is updated as it is, whatever it was saved as.
    if !config.cards.contains_key(card_number) && !is_valid_card_number(card_number) {
        return Err(format!(
            "invalid card number '{}': 16 characters A-Z, 0-9 expected",
            card_number
        )
        .into());
    }
    if !content.iccid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("invalid ICCID '{}': hex digits expected", content.iccid).into());
    }

    // One physical card belongs to exactly one card number. Linking an ICCID
    // that another entry already holds would make `find_card_number_by_iccid`
    // ambiguous, so the card could authenticate under the wrong client_id and
    // sniffed EF data could land on the wrong entry. Reject the link here — the
    // frontend only disables entries that already carry an ICCID, so nothing
    // stops the user from linking one physical card into two empty entries.
    if !content.iccid.is_empty() {
        if let Some((owner, _)) = config
            .cards
            .iter()
            .find(|(number, card)| number.as_str() != card_number && card.iccid == content.iccid)
        {
            return Err(format!(
                "ICCID {} is already linked to card {}; unlink it there before assigning it to {}",
                content.iccid, owner, card_number
            )
            .into());
        }
    }

    let mut changed = false;

    // Metadata fields are owned by the backend (APDU sniffer, auth recorder):
    // the frontend form only owns `name` and `iccid`, and a save must not let
    // an echoed snapshot clobber a concurrent backend write (e.g. `last_auth`
    // persisted while the edit dialog was open). Same rule t_protocol always
    // had: None means "not provided", never "clear it".
    macro_rules! update_provided {
        ($existing:ident, $field:ident) => {
            if content.$field.is_some() && $existing.$field != content.$field {
                $existing.$field = content.$field.clone();
                changed = true;
            }
        };
    }

    match config.cards.get_mut(card_number) {
        Some(existing_card) => {
            if existing_card.iccid.is_empty() {
                // ICCID is being set for the first time
                log::debug!(
                    "Existing card with empty ICCID. Updating: iccid = {}, name = {:?}, expire = {:?}",
                    content.iccid,
                    content.name,
                    content.expire
                );
                existing_card.iccid = content.iccid.clone();
                existing_card.name = content.name.clone();
                changed = true;
            } else {
                // Update the user-editable field (no restart required)
                if existing_card.name != content.name {
                    log::debug!(
                        "Updating name for card {}: name = {:?}",
                        card_number,
                        content.name
                    );
                    existing_card.name = content.name.clone();
                    changed = true;
                }
            }
            update_provided!(existing_card, expire);
            update_provided!(existing_card, card_type);
            update_provided!(existing_card, structure_version);
            update_provided!(existing_card, company_name);
            update_provided!(existing_card, company_address);
            update_provided!(existing_card, last_auth);
            update_provided!(existing_card, t_protocol);
        }
        None => {
            // Add new card entirely
            log::debug!(
                "Adding new card: card_number = {}, iccid = {}, name = {:?}, expire = {:?}",
                card_number,
                content.iccid,
                content.name,
                content.expire
            );
            config.cards.insert(card_number.to_string(), content);
            changed = true;
        }
    }

    if changed {
        // Save config to file
        save_config(config_path, &config)?;
        log::debug!("Configuration saved successfully");

        // Load into runtime cache
        load_config_to_cache(&config)?;
        log::debug!("Configuration loaded to cache successfully");

        // Emit frontend update event
        if let Some(card_config) = config.cards.get(card_number) {
            emit_card_config_event(
                "global-card-config-updated",
                card_number.to_string(),
                Some(card_config.clone()),
            );
        }
    }

    Ok(())
}

/// Synchronous core of the full-content card update (file I/O + fsync under
/// the global config lock). Blocking by design — call it from a blocking
/// thread, never from an async task or the main thread.
/// NOTE: a successful save may create a new ICCID -> card number association;
/// callers must then trigger session reconciliation the way update_card does
/// (smart_card::request_rescan + com_port::connect_pending_rack_cards).
pub(super) fn persist_card(card_number: &str, content: CardConfig) -> Result<(), String> {
    let config_path = get_config_path().map_err(|e| {
        log::error!("Failed to get config path: {}", e);
        format!("configuration file is unavailable: {e}")
    })?;

    match update_card_config(&config_path, card_number, content) {
        Ok(_) => {
            log::info!("The card, {} is added to the configuration!", card_number);
            Ok(())
        }
        Err(e) => {
            log::error!("Failed to update config: {}", e);
            Err(e.to_string())
        }
    }
}

/// Outcome of `mutate_card_config`. "The card is not in the config" is a
/// routine condition (a rack card whose ICCID is not linked to a number yet),
/// not a failure — collapsing it into the same `false` as a failed write made
/// every caller log a spurious ERROR for it, which in turn masked the real
/// write failures the message was meant to surface.
#[derive(Debug, PartialEq, Eq)]
pub enum CardMutation {
    /// The mutation applied and the config was written.
    Saved,
    /// The card exists but the closure reported nothing to change; no write.
    Unchanged,
    /// No such card number in the config.
    UnknownCard,
    /// The config could not be read back or written.
    Failed,
}

impl CardMutation {
    /// True when the card's stored state now reflects the request — either it
    /// was written or it already matched.
    pub fn is_ok(&self) -> bool {
        matches!(self, CardMutation::Saved | CardMutation::Unchanged)
    }
}

/// Applies `mutate` to one card entry under the global config write lock:
/// fresh load from disk → mutate → save → refresh cache → emit the frontend
/// event. The closure returns whether it actually changed anything; when it
/// returns false the save is skipped entirely.
///
/// This is the safe primitive for "change one field" writers (sniffer, auth
/// results): mutating fresh file state under the lock means concurrent writers
/// cannot revert each other's fields, which a cache-read + full-write would do.
///
/// Blocking (file I/O + fsync) — call from a blocking thread.
pub fn mutate_card_config<F>(card_number: &str, mutate: F) -> CardMutation
where
    F: FnOnce(&mut CardConfig) -> bool,
{
    let config_path = match get_config_path() {
        Ok(path) => path,
        Err(e) => {
            log::error!("Failed to get config path: {}", e);
            return CardMutation::Failed;
        }
    };

    let _guard = config_write_guard();

    let mut config = match load_config(&config_path) {
        Ok(config) => config,
        Err(e) => {
            log::error!("mutate_card_config: failed to load config: {}", e);
            return CardMutation::Failed;
        }
    };

    let Some(card) = config.cards.get_mut(card_number) else {
        log::debug!("mutate_card_config: unknown card_number {}", card_number);
        return CardMutation::UnknownCard;
    };

    if !mutate(card) {
        // Nothing changed against the authoritative file state — no write.
        return CardMutation::Unchanged;
    }
    let updated = card.clone();

    if let Err(e) = save_config(&config_path, &config) {
        log::error!("mutate_card_config: failed to save config: {}", e);
        return CardMutation::Failed;
    }
    if let Err(e) = load_config_to_cache(&config) {
        log::error!("mutate_card_config: failed to refresh cache: {}", e);
        return CardMutation::Failed;
    }

    emit_card_config_event(
        "global-card-config-updated",
        card_number.to_string(),
        Some(updated),
    );

    CardMutation::Saved
}

/// Removes a card entry from the file and, when it was there, ends every
/// session running under its card number (reader- and rack-backed) and tells
/// the frontend. A missing entry is reported as `NotFound`.
pub(super) async fn remove_card_from_config(
    config_path: &Path,
    card_number: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = config_path.to_path_buf();
    let number = card_number.to_string();

    // The file part (load-modify-save + cache refresh) is blocking — run it on
    // the blocking pool, serialized with all other config writers by the lock.
    let removed = tauri::async_runtime::spawn_blocking(
        move || -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
            let _guard = config_write_guard();

            log::debug!("Loading configuration from {:?}", path);
            let mut config = load_config(&path)?;

            if config.cards.remove(&number).is_none() {
                return Ok(false);
            }

            save_config(&path, &config)?;
            log::debug!("Configuration saved successfully after removal");

            load_config_to_cache(&config)?;
            log::debug!("Configuration loaded to cache successfully");

            Ok(true)
        },
    )
    .await
    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
        format!("remove_card_from_config: blocking task failed: {}", e).into()
    })??;

    if removed {
        // Kill card task with the specified client_id (card number)
        remove_connections(vec![card_number.to_string()]).await;
        log::debug!("Removed connection for card {}", card_number);

        // Reader-backed sessions live in TASK_POOL and are covered above; a
        // rack-backed session for the same number lives in the rack module's
        // own map and must be closed explicitly, or it keeps running with the
        // removed card's MQTT client_id.
        crate::com_port::disconnect_rack_card(card_number);

        emit_card_config_event("global-card-config-updated", card_number.to_string(), None);

        #[cfg(target_os = "linux")]
        {
            // "Super hack" to reload card states and trigger an event to update readers.

            use crate::smart_card::manual_sync_cards;
            use tokio::time::sleep;
            use tokio::time::Duration;

            sleep(Duration::from_millis(100)).await;
            if let Err(e) = manual_sync_cards(card_number.to_string(), false).await {
                // the removal itself succeeded; a failed rescan only delays the readers list refresh
                log::warn!("remove_card: card rescan failed: {}", e);
            }
        }

        Ok(())
    } else {
        log::warn!("Cardnumber {} not found in configuration", card_number);
        Err(Box::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Card not found in configuration",
        )))
    }
}

/// Records the final state of an authentication attempt in the card config:
/// `success == true` → green "success" line in UI; `false` → red "fail".
/// The processing state while auth is running is derived from Reader.authentication
/// in the frontend and is NOT stored here (it's transient, lost on restart).
fn record_auth_result(card_number: &str, success: bool, ts: u64) {
    // Mutate fresh file state under the config lock instead of writing a full
    // cache snapshot back — a stale snapshot would revert fields a concurrent
    // writer (e.g. the sniffer) just persisted. Detached writes can run out of
    // order: an older result never replaces a newer one.
    let outcome = mutate_card_config(card_number, |card| {
        if card.last_auth.is_some_and(|(recorded, _)| recorded > ts) {
            return false;
        }
        card.last_auth = Some((ts, success));
        true
    });
    match outcome {
        // A card that is not in the config has no last_auth to record — the
        // normal case for an unlinked rack card, not a failure.
        CardMutation::UnknownCard => log::debug!(
            "record_auth_result: no config entry for card_number {}",
            card_number
        ),
        CardMutation::Failed => log::error!(
            "record_auth_result: failed to persist last_auth for card_number {}",
            card_number
        ),
        CardMutation::Saved | CardMutation::Unchanged => {}
    }
}

/// Fire-and-forget variant of `record_auth_result` for async contexts (the MQTT
/// eventloop tasks).
///
/// Deliberately does NOT await the write. `record_auth_result` takes the global
/// config lock and fsyncs the file; awaiting it on the card's MQTT task would
/// park the APDU bridge on disk I/O at the worst possible moment — right
/// between the VU's `finish` and our reply — and a slow disk (antivirus scan on
/// Windows, network home directory) would push the server past its command
/// timeout. The timestamp is UI-only state, so nothing downstream needs it to
/// have landed before the reply goes out.
pub fn record_auth_result_detached(card_number: &str, success: bool) {
    let card_number = card_number.to_string();
    // when the authentication ended, not when the blocking pool gets to it
    let ts = chrono::Utc::now().timestamp() as u64;
    tauri::async_runtime::spawn_blocking(move || {
        record_auth_result(&card_number, success, ts);
    });
}

/// Emits every known card config to the frontend. Called on each
/// `frontend-loaded` so a (re)loaded webview gets the current card list —
/// the backend initializes only once, but the frontend can reload many times.
pub fn emit_all_card_configs() {
    // Clone out under the lock, emit after releasing it.
    let cards: Vec<(String, CardConfig)> = {
        let cache = cache_guard();
        cache
            .cards
            .iter()
            .map(|(number, cfg)| (number.clone(), cfg.clone()))
            .collect()
    };

    for (card_number, card_config) in cards {
        emit_card_config_event("global-card-config-updated", card_number, Some(card_config));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::{sample_config, unique_tmp_dir};
    use std::fs;

    #[test]
    fn concurrent_card_updates_do_not_lose_writes() {
        // Regression for the read-modify-write race: without CONFIG_WRITE_LOCK
        // two writers load the same version and the last save drops the other's
        // card. Every thread adds unique cards; all must survive.
        let dir = unique_tmp_dir("concurrent");
        let path = dir.join("config.yaml");
        save_config(&path, &sample_config()).expect("seed config");

        const THREADS: usize = 8;
        const UPDATES_PER_THREAD: usize = 10;

        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for i in 0..UPDATES_PER_THREAD {
                        let number = format!("THREAD{:02}CARD{:04}", t, i);
                        let card = CardConfig {
                            iccid: format!("{:016}", t * 1000 + i),
                            expire: None,
                            name: None,
                            t_protocol: None,
                            card_type: None,
                            structure_version: None,
                            company_name: None,
                            company_address: None,
                            last_auth: None,
                        };
                        update_card_config(&path, &number, card).expect("update");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("writer thread panicked");
        }

        let loaded = load_config(&path).expect("load");
        assert_eq!(
            loaded.cards.len(),
            1 + THREADS * UPDATES_PER_THREAD,
            "every concurrent update must be preserved"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_card_config_rejects_an_iccid_owned_by_another_card() {
        let dir = unique_tmp_dir("dupiccid");
        let path = dir.join("config.yaml");

        // Two entries: one already linked to a physical card, one still empty.
        let mut cfg = sample_config();
        cfg.cards.insert(
            "FEDCBA9876543210".to_string(),
            CardConfig {
                iccid: String::new(),
                expire: None,
                name: None,
                t_protocol: None,
                card_type: None,
                structure_version: None,
                company_name: None,
                company_address: None,
                last_auth: None,
            },
        );
        save_config(&path, &cfg).expect("save");

        // Linking the same physical card into the empty entry must be refused:
        // a duplicate ICCID makes find_card_number_by_iccid ambiguous, so the
        // card could authenticate under the wrong client_id.
        let mut content = cfg
            .cards
            .get("FEDCBA9876543210")
            .cloned()
            .expect("second card");
        content.iccid = "1122334455667788".to_string();
        let err = update_card_config(&path, "FEDCBA9876543210", content)
            .expect_err("duplicate ICCID must be rejected");
        assert!(
            err.to_string().contains("already linked"),
            "unexpected error: {err}"
        );

        // The file is untouched: the second entry still has no ICCID.
        let loaded = load_config(&path).expect("load");
        assert_eq!(
            loaded
                .cards
                .get("FEDCBA9876543210")
                .expect("second card")
                .iccid,
            ""
        );

        // Re-saving the SAME card with its own ICCID stays allowed.
        let own = loaded
            .cards
            .get("ABCDEF0123456789")
            .cloned()
            .expect("first card");
        update_card_config(&path, "ABCDEF0123456789", own).expect("self-update must be allowed");

        let _ = fs::remove_dir_all(&dir);
    }

    // `mutate_card_config` reports four distinct outcomes. Callers branch on
    // them to decide the log level, and the whole point of the enum is that a
    // missing card is NOT an error — an unlinked rack card hits that path on
    // every sniffed field, and reporting it as a failure buried the real
    // write failures. Pin the classification so it cannot silently collapse
    // back into a bool.
    #[test]
    fn card_mutation_separates_missing_cards_from_failures() {
        assert!(CardMutation::Saved.is_ok());
        assert!(CardMutation::Unchanged.is_ok());
        assert!(!CardMutation::UnknownCard.is_ok());
        assert!(!CardMutation::Failed.is_ok());
        // The two non-ok outcomes must stay distinguishable: callers log the
        // first at debug and only the second at error.
        assert_ne!(CardMutation::UnknownCard, CardMutation::Failed);
    }

    #[test]
    fn update_card_config_preserves_t_protocol_when_not_provided() {
        let dir = unique_tmp_dir("tproto");
        let path = dir.join("config.yaml");
        let card_number = "ABCDEF0123456789";

        let cfg = sample_config();
        save_config(&path, &cfg).expect("save");

        // A frontend-style update carries no t_protocol - the stored value must survive.
        let mut content = cfg.cards.get(card_number).cloned().expect("card");
        content.t_protocol = None;
        content.name = Some("Renamed".to_string());
        update_card_config(&path, card_number, content).expect("update");

        let loaded = load_config(&path).expect("load");
        let card = loaded.cards.get(card_number).expect("card");
        assert_eq!(card.name.as_deref(), Some("Renamed"));
        assert_eq!(card.t_protocol.as_deref(), Some("T0"));

        // An explicit value must overwrite the stored one.
        let mut content = card.clone();
        content.t_protocol = Some("T1".to_string());
        update_card_config(&path, card_number, content).expect("update");

        let loaded = load_config(&path).expect("load");
        let card = loaded.cards.get(card_number).expect("card");
        assert_eq!(card.t_protocol.as_deref(), Some("T1"));

        let _ = fs::remove_dir_all(&dir);
    }
}
