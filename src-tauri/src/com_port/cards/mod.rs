//! Rack-backed per-card MQTT sessions.
//!
//! The server discovers cards by ICCID and publishes the set of cards of a
//! rack to serve (`rack/<id>/cards`); the sessions are reconciled with that
//! set. Each session is an MQTT connection under the card's own number,
//! funnelling its envelopes into the serial port of the rack that holds the
//! card. Also hosts the card presence watches, one per connected rack, armed by
//! the server once its discovery has walked that rack.
//!
//! Layout:
//! - `card_set` — the server's card sets and the pending-card retry
//! - `sessions` — the session registry: spawning, aborting, inventory
//! - `session` — one card session: its MQTT loop and serial worker
//! - `watch` — the per-rack card presence watches
//!
//! The state shared by those parts lives here: the session registry
//! (`RACK_CARD_TASKS`), the UI card lists (`RACK_CARDS_UI`) and the card-set
//! epochs (`RACK_CARD_EPOCH`).
//!
//! Lock order:
//! - `TASK_POOL` (PC/SC sessions) is held across `RACK_CARD_TASKS` in
//!   `spawn_rack_card_checked`;
//! - `RACK_CARD_TASKS` and `RACK_CARDS_UI` are never nested, each is dropped
//!   before the other is taken;
//! - `RACK_CARD_EPOCH` is a leaf (see its docs).

mod card_set;
mod session;
mod sessions;
mod watch;

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};

use rumqttc::v5::AsyncClient;
use tauri::async_runtime::JoinHandle;

use crate::global_app_handle::RackCard;

use super::lock;

pub use card_set::connect_pending_rack_cards;
pub(super) use card_set::{apply_cards_set, handle_cards_set, parse_cards_set};
pub use sessions::{abort_rack_card_session, disconnect_rack_card};
pub(super) use sessions::{inventory, log_sessions_snapshot, stop_all_rack_cards, stop_rack_cards};
pub(super) use watch::{start_rack_watch, stop_all_rack_watches, stop_rack_watch};

/// One rack-backed card session: the running task plus the identity it was
/// spawned with. `rack_id` is the id of the rack whose serial port the session
/// writes to — server messages of one rack must never touch sessions that
/// belong to another.
struct RackCardTask {
    card_number: String,
    slot: u16,
    rack_id: String,
    handle: JoinHandle<()>,
    /// MQTT client of the session, set by its loop once built. Lets a repeated
    /// card set that still lists this card ask the live session to re-publish
    /// its rack link report without disturbing the session itself — the server
    /// needs that report to know the card is still served (it repaints the
    /// slot indicator from it) after the application connection re-established and
    /// its discovery re-ran.
    client: Arc<OnceLock<AsyncClient>>,
    /// True while the session's MQTT connection is up (set on its CONNACK,
    /// cleared on a connection error). A re-bind report is only worth sending
    /// over a live connection: one queued while offline sits in the client's
    /// bounded request channel, and enough of them fill it — after which the
    /// session's own CONNACK link report can no longer be enqueued. An
    /// offline session needs no re-bind anyway: its next CONNACK sends the
    /// link report by itself.
    online: Arc<AtomicBool>,
}

lazy_static::lazy_static! {
    /// Rack-backed per-card MQTT tasks, keyed by ICCID (the stable card
    /// identifier — a physical card sits in exactly one slot of one rack).
    /// Keying by the config-resolved card number would leak the session if the
    /// config entry is deleted or edited while the card sits in the rack — the
    /// reconcile lookup would then miss the running task. The (rack, slot) the
    /// session was spawned for is kept so a card set naming the same slot
    /// with a different ICCID (card swapped) evicts the stale session, and so
    /// a set of another rack naming this card (card moved while the old
    /// rack's set is delayed) replaces the session instead of being skipped.
    static ref RACK_CARD_TASKS: std::sync::Mutex<HashMap<String, RackCardTask>> =
        std::sync::Mutex::new(HashMap::new());

    /// Cards currently exposed by each rack (RackState.cards), keyed by the
    /// rack id.
    pub(super) static ref RACK_CARDS_UI: std::sync::Mutex<HashMap<String, Vec<RackCard>>> =
        std::sync::Mutex::new(HashMap::new());

    /// Card-set epoch of each rack: bumped by every server card set and every
    /// teardown of the rack's sessions. Card sets run as separate tasks and
    /// yield between their spawns (each waits for the PC/SC pool lock), so an
    /// older set can resume after a newer one completed — e.g. the set that
    /// still listed a card that has since been pulled. A spawn or a UI row
    /// carries the epoch its set started under and is dropped once a newer
    /// one exists. Lock order: a leaf — taken under `RACK_CARD_TASKS` (the
    /// epoch check of `spawn_rack_card`) or under `RACK_CARDS_UI` (the
    /// `still_current` check of `state::update_rack_card_ui`), and no other
    /// lock is ever taken while it is held.
    static ref RACK_CARD_EPOCH: std::sync::Mutex<HashMap<String, u64>> =
        std::sync::Mutex::new(HashMap::new());
}

/// Starts a new card-set epoch of the rack and returns it.
pub(super) fn bump_card_epoch(rack_id: &str) -> u64 {
    let mut epochs = lock(&RACK_CARD_EPOCH);
    let epoch = epochs.entry(rack_id.to_string()).or_insert(0);
    *epoch += 1;
    *epoch
}

/// The current card-set epoch of the rack.
fn card_epoch(rack_id: &str) -> u64 {
    lock(&RACK_CARD_EPOCH).get(rack_id).copied().unwrap_or(0)
}
