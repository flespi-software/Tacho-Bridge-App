//! Card list synchronization with the server.
//!
//! The server keeps a copy of the card list per application ident, so a new
//! copy of the application connecting with the same ident gets its cards
//! back. The contract is in `communication_protocol.md` ("Card list
//! synchronization"); in short:
//!   * the settings report carries `sync: {rev, hash}`; only a server that
//!     knows the key answers, on the `sync` topic, with its copy
//!     (`rev`, `updated`, `hash`, and `cards` when the hashes differ);
//!   * TBA replaces the server copy by publishing `{rev, hash, cards}` to
//!     `sync`, where `rev` is the revision the list is based on; the server
//!     stores it only on top of that revision and answers with its copy again.
//!
//! Nothing is published to `sync` before the server has spoken there on the
//! current connection, so a server without the feature never sees the topic.
//!
//! Which side wins is decided here (`decide`): a side that did not change
//! since the last synchronization follows the other one silently; when both
//! changed, the user chooses (window dialog, or the console of a headless run
//! started from a terminal). Without anybody to ask the local list stays in
//! use and the conflict is logged.

// ───── Std Lib ─────
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

// ───── External Crates ─────
use rumqttc::v5::AsyncClient;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tauri::async_runtime;
use tauri::Emitter;

// ───── Local Modules ─────
use crate::config::{SyncState, SyncedCards};

/// Topic of the synchronization, both ways.
const SYNC_TOPIC: &str = "sync";

/// Frontend event carrying the conflict to resolve, or `null` once there is none.
const CONFLICT_EVENT: &str = "cards-sync-conflict";

/// The server copy as it arrives on the `sync` topic.
#[derive(Deserialize, Clone, Debug)]
struct ServerCopy {
    /// 0 = the server stores nothing for this ident.
    rev: u64,
    #[serde(default)]
    updated: u64,
    #[serde(default)]
    hash: String,
    /// Present when the server hash differs from the reported one.
    #[serde(default)]
    cards: Option<SyncedCards>,
}

/// The app connection the server has spoken `sync` on; `None` until then, so
/// nothing is published to a server without the feature.
static LINK: Mutex<Option<AsyncClient>> = Mutex::new(None);

/// The server copy of a conflict waiting for the user's choice.
static PENDING: Mutex<Option<ServerCopy>> = Mutex::new(None);

/// `(base rev, hash)` of the lists published on the current connection: tells
/// the answer to an own publish from a change made by somebody else, and keeps
/// a list the server did not take from being published in a loop.
static PUSHED: Mutex<Vec<(u64, String)>> = Mutex::new(Vec::new());

/// A console question is open (headless run): one at a time.
static ASKING: AtomicBool = AtomicBool::new(false);

fn lock<T>(mutex: &'static Mutex<T>) -> MutexGuard<'static, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Hash of a card list: SHA-256 over one `number TAB iccid TAB name TAB
/// t_protocol LF` line per card, in card number order. Opaque to the server,
/// so only TBA versions have to agree on it.
pub fn cards_hash(cards: &SyncedCards) -> String {
    let mut hasher = Sha256::new();
    for (number, card) in cards {
        let line = format!(
            "{}\t{}\t{}\t{}\n",
            number,
            card.iccid,
            card.name.as_deref().unwrap_or(""),
            card.t_protocol.as_deref().unwrap_or("")
        );
        hasher.update(line.as_bytes());
    }
    hex::encode(hasher.finalize())
}

/// The `sync` key of the settings report: the revision the local list was
/// last synchronized with and the hash of the list as it is now.
pub fn report_json() -> serde_json::Value {
    serde_json::json!({
        "rev": crate::config::sync_state().map_or(0, |state| state.rev),
        "hash": cards_hash(&crate::config::synced_cards()),
    })
}

/// A new app connection: whether its server synchronizes is not known yet.
pub fn on_app_connack() {
    *lock(&LINK) = None;
    lock(&PUSHED).clear();
}

/// What to do about a server copy.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// Nothing to synchronize.
    Nothing,
    /// The lists are equal: remember the server revision.
    InSync,
    /// Replace the local list by the server copy.
    Take,
    /// Replace the server copy by the local list, on top of this revision.
    Push(u64),
    /// Both sides changed: the user chooses.
    Conflict,
}

/// Decides by who changed since the last synchronization (`base`): the local
/// list did when its hash is not the recorded one, the server copy did when
/// its revision is not the recorded one. A list that was never synchronized
/// counts as changed when it has cards, and the server copy always does then.
fn decide(
    local_hash: &str,
    local_empty: bool,
    base: Option<&SyncState>,
    server_rev: u64,
    server_hash: &str,
) -> Decision {
    if server_rev == 0 {
        return if local_empty {
            Decision::Nothing
        } else {
            Decision::Push(0)
        };
    }
    if server_hash == local_hash {
        return Decision::InSync;
    }
    let local_changed = match base {
        Some(base) => base.hash != local_hash,
        None => !local_empty,
    };
    let server_changed = base.is_none_or(|base| base.rev != server_rev);
    match (local_changed, server_changed) {
        (false, true) => Decision::Take,
        (true, false) => Decision::Push(server_rev),
        // (false, false) cannot be reached by honest counting (a server that
        // lost its copy and counted up to the same revision again): ask.
        _ => Decision::Conflict,
    }
}

/// Takes a publish of the app connection when it is the server copy on the
/// `sync` topic. Returns false for any other topic.
pub fn handle_publish(client: &AsyncClient, log_header: &str, topic: &str, payload: &[u8]) -> bool {
    if topic != SYNC_TOPIC {
        return false;
    }
    let server = match serde_json::from_slice::<ServerCopy>(payload) {
        Ok(server) => server,
        Err(e) => {
            log::error!("{} [SYNC] status=invalid_server_copy err={}", log_header, e);
            return true;
        }
    };
    *lock(&LINK) = Some(client.clone());
    let client = client.clone();
    let log_header = log_header.to_string();
    async_runtime::spawn(async move {
        process(&client, &log_header, server).await;
    });
    true
}

/// Acts on a server copy.
async fn process(client: &AsyncClient, log_header: &str, server: ServerCopy) {
    // The answer to an own publish the server took: that revision is the
    // base now, whatever was changed locally while the answer travelled.
    let own = server.rev > 0
        && lock(&PUSHED)
            .iter()
            .any(|(rev, hash)| *rev + 1 == server.rev && *hash == server.hash);
    if own {
        save_state(log_header, &server).await;
    }

    let cards = crate::config::synced_cards();
    let local_hash = cards_hash(&cards);
    let base = crate::config::sync_state();
    let decision = decide(
        &local_hash,
        cards.is_empty(),
        base.as_ref(),
        server.rev,
        &server.hash,
    );
    log::info!(
        "{} [SYNC] status=server_copy rev={} local_rev={} cards={} decision={:?}",
        log_header,
        server.rev,
        base.as_ref().map_or(0, |state| state.rev),
        cards.len(),
        decision
    );
    match decision {
        Decision::Nothing => clear_conflict(),
        Decision::InSync => {
            save_state(log_header, &server).await;
            clear_conflict();
        }
        Decision::Take => {
            apply_server_copy(log_header, server).await;
            clear_conflict();
        }
        Decision::Push(rev) => {
            push(client, log_header, rev).await;
            clear_conflict();
        }
        Decision::Conflict => raise_conflict(log_header, server, cards.len()),
    }
}

/// Records the server copy as the one the local list is in sync with.
async fn save_state(log_header: &str, server: &ServerCopy) {
    let state = SyncState {
        rev: server.rev,
        updated: server.updated,
        hash: server.hash.clone(),
    };
    if crate::config::sync_state().as_ref() == Some(&state) {
        return;
    }
    let saved = async_runtime::spawn_blocking(move || crate::config::set_sync_state(state))
        .await
        .unwrap_or_else(|e| Err(format!("blocking task failed: {e}")));
    if let Err(e) = saved {
        log::error!("{} [SYNC] status=state_save_failed err={}", log_header, e);
    }
}

/// Replaces the local list by the server copy and brings the card sessions
/// in line with it.
async fn apply_server_copy(log_header: &str, server: ServerCopy) {
    let Some(cards) = server.cards else {
        // The server sends the cards whenever the hashes differ.
        log::error!("{} [SYNC] status=take_failed reason=no_cards", log_header);
        return;
    };
    let count = cards.len();
    let state = SyncState {
        rev: server.rev,
        updated: server.updated,
        hash: server.hash,
    };
    let replaced =
        async_runtime::spawn_blocking(move || crate::config::replace_synced_cards(cards, state))
            .await
            .unwrap_or_else(|e| Err(format!("blocking task failed: {e}")));
    let removed = match replaced {
        Ok(removed) => removed,
        Err(e) => {
            log::error!("{} [SYNC] status=take_failed err={}", log_header, e);
            return;
        }
    };
    log::info!(
        "{} [SYNC] status=taken rev={} cards={} removed={}",
        log_header,
        server.rev,
        count,
        removed.len()
    );
    // Same follow-up as a card removed or saved from the window.
    for number in &removed {
        crate::com_port::disconnect_rack_card(number);
    }
    crate::mqtt::remove_connections(removed).await;
    crate::smart_card::request_rescan();
    crate::com_port::connect_pending_rack_cards().await;
}

/// Publishes the local list to replace the server copy of revision `rev`.
async fn push(client: &AsyncClient, log_header: &str, rev: u64) {
    let cards = crate::config::synced_cards();
    let hash = cards_hash(&cards);
    {
        let mut pushed = lock(&PUSHED);
        let key = (rev, hash.clone());
        if pushed.contains(&key) {
            // Published already and the server still holds another copy.
            log::warn!(
                "{} [SYNC] status=push_skipped reason=repeat rev={}",
                log_header,
                rev
            );
            return;
        }
        pushed.push(key);
    }
    let payload = serde_json::json!({"rev": rev, "hash": hash, "cards": cards}).to_string();
    match crate::commands_settings::publish_bounded(client, SYNC_TOPIC, payload).await {
        Ok(()) => log::info!(
            "{} [SYNC] status=pushed base_rev={} cards={}",
            log_header,
            rev,
            cards.len()
        ),
        Err(e) => log::error!("{} [SYNC] status=push_failed err={}", log_header, e),
    }
}

/// The local card list was changed by the user: publish it when the server
/// synchronizes and no conflict is open. Otherwise the next connection does.
pub fn on_local_change() {
    let Some(client) = lock(&LINK).clone() else {
        return;
    };
    if lock(&PENDING).is_some() {
        return;
    }
    async_runtime::spawn(async move {
        let base = crate::config::sync_state();
        let hash = cards_hash(&crate::config::synced_cards());
        if base.as_ref().is_some_and(|state| state.hash == hash) {
            return;
        }
        push(&client, "sync |", base.map_or(0, |state| state.rev)).await;
    });
}

/// The user's answer to a conflict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Choice {
    /// Replace the local list by the server copy.
    Server,
    /// Replace the server copy by the local list.
    Local,
}

/// What the user is shown about a conflict.
fn conflict_json(server: &ServerCopy, local_cards: usize) -> serde_json::Value {
    serde_json::json!({
        "server_rev": server.rev,
        "server_updated": server.updated,
        "server_cards": server.cards.as_ref().map_or(0, |cards| cards.len()),
        "local_cards": local_cards,
    })
}

fn emit_conflict(payload: serde_json::Value) {
    if let Some(app) = crate::global_app_handle::get_app_handle() {
        if let Err(e) = app.emit(CONFLICT_EVENT, payload) {
            log::error!("emit '{}' failed: {:?}", CONFLICT_EVENT, e);
        }
    }
}

/// Replays the open conflict (or its absence) to a freshly loaded frontend.
pub fn emit_current() {
    let pending = lock(&PENDING).clone();
    let payload = match pending {
        Some(server) => conflict_json(&server, crate::config::synced_cards().len()),
        None => serde_json::Value::Null,
    };
    emit_conflict(payload);
}

fn clear_conflict() {
    if lock(&PENDING).take().is_some() {
        emit_conflict(serde_json::Value::Null);
    }
}

/// Keeps the conflict for the user to resolve and asks: the window, or the
/// console of a headless run. The local list stays in use meanwhile.
fn raise_conflict(log_header: &str, server: ServerCopy, local_cards: usize) {
    log::warn!(
        "{} [SYNC] status=conflict server_rev={} server_cards={} local_cards={} \
         hint=\"the card list was changed both here and on the server; the local one stays in use until a choice is made\"",
        log_header,
        server.rev,
        server.cards.as_ref().map_or(0, |cards| cards.len()),
        local_cards
    );
    let payload = conflict_json(&server, local_cards);
    *lock(&PENDING) = Some(server);
    emit_conflict(payload.clone());
    if crate::headless::requested() && !ASKING.swap(true, Ordering::SeqCst) {
        async_runtime::spawn(async move {
            let choice = async_runtime::spawn_blocking(move || ask_on_console(&payload))
                .await
                .unwrap_or(None);
            ASKING.store(false, Ordering::SeqCst);
            if let Some(choice) = choice {
                if let Err(e) = resolve(choice).await {
                    log::error!("[SYNC] status=resolve_failed err={}", e);
                }
            }
        });
    }
}

/// Asks on the console of a run started from a terminal; `None` when there is
/// no terminal (a service) or the input ended.
fn ask_on_console(conflict: &serde_json::Value) -> Option<Choice> {
    use std::io::{BufRead, IsTerminal, Write};

    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return None;
    }
    println!(
        "\nThe card list differs from the copy on the server and both were changed.\n  \
         server: revision {}, {} card(s), stored at unix time {}\n  \
         local:  {} card(s)",
        conflict["server_rev"],
        conflict["server_cards"],
        conflict["server_updated"],
        conflict["local_cards"]
    );
    loop {
        print!("Take the list from the [s]erver or keep the [l]ocal one and update the server? ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => return None,
            Ok(_) => match line.trim().to_ascii_lowercase().as_str() {
                "s" | "server" => return Some(Choice::Server),
                "l" | "local" => return Some(Choice::Local),
                _ => {}
            },
        }
    }
}

/// Applies the user's choice to the open conflict.
async fn resolve(choice: Choice) -> Result<(), String> {
    let Some(server) = lock(&PENDING).take() else {
        return Err("no card list conflict to resolve".to_string());
    };
    emit_conflict(serde_json::Value::Null);
    log::info!(
        "[SYNC] status=resolved choice={:?} server_rev={}",
        choice,
        server.rev
    );
    match choice {
        Choice::Server => apply_server_copy("sync |", server).await,
        Choice::Local => {
            let Some(client) = lock(&LINK).clone() else {
                return Err("not connected to the server".to_string());
            };
            // On top of the server revision the user has just been shown.
            push(&client, "sync |", server.rev).await;
        }
    }
    Ok(())
}

/// The conflict dialog of the window: `choice` is "server" or "local".
#[tauri::command]
pub async fn resolve_cards_sync(choice: String) -> Result<(), String> {
    let choice = match choice.as_str() {
        "server" => Choice::Server,
        "local" => Choice::Local,
        other => return Err(format!("unknown choice '{other}'")),
    };
    resolve(choice).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(rev: u64, hash: &str) -> SyncState {
        SyncState {
            rev,
            updated: 0,
            hash: hash.to_string(),
        }
    }

    #[test]
    fn empty_server_takes_a_local_list_and_nothing_else() {
        assert_eq!(decide("a", false, None, 0, ""), Decision::Push(0));
        assert_eq!(decide("a", true, None, 0, ""), Decision::Nothing);
        // a server that lost its copy gets it back
        assert_eq!(
            decide("a", false, Some(&base(5, "a")), 0, ""),
            Decision::Push(0)
        );
    }

    #[test]
    fn equal_lists_are_in_sync_whatever_the_history() {
        assert_eq!(decide("a", false, None, 3, "a"), Decision::InSync);
        assert_eq!(
            decide("a", false, Some(&base(1, "b")), 3, "a"),
            Decision::InSync
        );
    }

    #[test]
    fn the_unchanged_side_follows_the_changed_one() {
        // a fresh copy of the application: no cards, never synchronized
        assert_eq!(decide("e", true, None, 3, "a"), Decision::Take);
        // the server moved on, nothing was changed here
        assert_eq!(
            decide("a", false, Some(&base(5, "a")), 6, "b"),
            Decision::Take
        );
        // changed here, the server still holds the base revision
        assert_eq!(
            decide("b", false, Some(&base(5, "a")), 5, "a"),
            Decision::Push(5)
        );
    }

    #[test]
    fn both_sides_changed_is_a_conflict() {
        assert_eq!(
            decide("b", false, Some(&base(5, "a")), 6, "c"),
            Decision::Conflict
        );
        // cards entered before the first synchronization against a stored copy
        assert_eq!(decide("b", false, None, 3, "a"), Decision::Conflict);
    }

    #[test]
    fn hash_follows_the_synchronized_fields_only_in_card_order() {
        let parse = |json: &str| serde_json::from_str::<SyncedCards>(json).unwrap();
        let one = parse(
            r#"{"B000000000000000":{"iccid":"22"},"A000000000000000":{"iccid":"11","name":"first"}}"#,
        );
        let other = parse(
            r#"{"A000000000000000":{"iccid":"11","name":"first"},"B000000000000000":{"iccid":"22"}}"#,
        );
        assert_eq!(cards_hash(&one), cards_hash(&other));
        assert_eq!(cards_hash(&one).len(), 64);

        let renamed = parse(
            r#"{"A000000000000000":{"iccid":"11","name":"first"},"B000000000000000":{"iccid":"22","name":"renamed"}}"#,
        );
        assert_ne!(cards_hash(&one), cards_hash(&renamed));
    }

    #[test]
    fn server_copy_parses_with_and_without_cards() {
        let empty: ServerCopy = serde_json::from_str(r#"{"rev":0}"#).unwrap();
        assert_eq!(empty.rev, 0);
        assert!(empty.cards.is_none());

        let full: ServerCopy = serde_json::from_str(
            r#"{"rev":5,"updated":1795700000,"hash":"aa","cards":{"4220000000490000":{"iccid":"00021B3C11170122","name":"first"}}}"#,
        )
        .unwrap();
        assert_eq!(full.updated, 1_795_700_000);
        let cards = full.cards.unwrap();
        assert_eq!(cards["4220000000490000"].iccid, "00021B3C11170122");
        assert_eq!(cards["4220000000490000"].t_protocol, None);
    }
}
