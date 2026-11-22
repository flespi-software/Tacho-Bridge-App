//! Local rack operations.
//!
//! A server that supports it answers a rack's `link up` with a **profile**
//! (`rack/<id>/profile`) instead of driving the rack exchange by exchange. The
//! profile is all this side ever knows about the device: frame templates with
//! two placeholders (`{a}` — the slot address byte, `{c}` — the checksum byte)
//! and byte rules that are applied blindly. It is kept in memory only — never
//! written to the config or to the log below trace level — and nothing in this
//! file names a device or carries a byte of its protocol.
//!
//! With a profile the routine work of the rack runs here, without a server
//! round trip per step: the status poll, the slot table of the UI, card
//! identification, the removal check and the slot indicators. Card sessions
//! are opened and closed through the same reconcile a server card set uses
//! (`cards::apply_cards_set`), so everything downstream is unchanged. The
//! server keeps what must stay remote: the exchanges of the card sessions.
//!
//! The engine of a rack lives as long as its port and does not depend on the
//! application connection once it has the profile: a reconnect re-delivers the
//! same profile and the running engine is left alone. A server that drives the
//! rack itself (an older one, or local mode switched off) sends its own rack
//! commands instead, which stops the engine (see `rack::handle_app_publish`).
//!
//! Layout:
//! - `profile` — the parsed profile: templates, byte rules, frames for a slot
//! - `slots` — what the engine knows about each slot, and the rules updating it
//! - `engine` — the status poll loop of a rack and its serial work
//! - `blink` — slot blinks the user asks for to find a slot
//! - `shutdown` — darkening the indicators when the app closes

mod blink;
mod engine;
mod profile;
mod shutdown;
mod slots;
#[cfg(test)]
mod test_profile;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tauri::async_runtime::{self, JoinHandle};

use super::transport::SharedPort;
use super::{lock, rack_port_is_live};

use blink::cancel_blinks;
use engine::run;
use profile::Profile;
use shutdown::ShutdownSlots;

// Re-exported for the rest of `com_port`: with `start`, `stop` and the queries
// below, the only entry points of the local engine.
pub(super) use blink::{blink, stop_blink};
pub(super) use shutdown::{prepare_indicator_shutdown, shutdown_estimate, turn_off_indicators};

/// One running engine: its task, the profile text it was started with and
/// the port it works on.
struct Engine {
    arming: u64,
    profile_text: Vec<u8>,
    profile: Arc<Profile>,
    port: SharedPort,
    shutdown_slots: Arc<std::sync::Mutex<ShutdownSlots>>,
    handle: JoinHandle<()>,
}

/// Arming number of the next engine: tells its task whether the entry it
/// leaves behind is still its own.
static NEXT_ARMING: AtomicU64 = AtomicU64::new(1);

lazy_static::lazy_static! {
    /// Engines of the racks in local mode, keyed by rack id.
    static ref ENGINES: std::sync::Mutex<HashMap<String, Engine>> =
        std::sync::Mutex::new(HashMap::new());
}

/// Whether the rack is run locally right now.
pub(super) fn is_active(rack_id: &str) -> bool {
    lock(&ENGINES)
        .get(rack_id)
        .is_some_and(|engine| !engine.handle.inner().is_finished())
}

/// Whether the server of this rack confirms its card sessions (`card_status`
/// of the profile): a card is then unserved until its confirmation arrives.
pub(super) fn confirms_cards(rack_id: &str) -> bool {
    lock(&ENGINES)
        .get(rack_id)
        .is_some_and(|engine| engine.profile.card_status)
}

/// A `rack/<id>/profile` of the server: starts the engine of the rack. The
/// same profile for a rack whose engine already runs on this port changes
/// nothing — that is what every reconnect of the application delivers.
pub(super) fn start(payload: &[u8], rack_id: &str, port: &SharedPort, log_header: &str) {
    let profile = match Profile::parse(payload) {
        Ok(profile) => profile,
        Err(reason) => {
            log::warn!(
                "{} [LOCAL] status=profile_rejected reason={}",
                log_header,
                reason
            );
            return;
        }
    };
    let mut engines = lock(&ENGINES);
    if engines.get(rack_id).is_some_and(|engine| {
        engine.profile_text == payload
            && std::sync::Arc::ptr_eq(&engine.port, port)
            && !engine.handle.inner().is_finished()
    }) {
        log::debug!("{} [LOCAL] status=profile_unchanged", log_header);
        return;
    }
    // Same guard as the presence watch: a rack unlinked in between must not
    // get an engine that polls a dead port.
    if !rack_port_is_live(rack_id, port) {
        log::info!(
            "{} [LOCAL] status=skipped reason=rack_not_linked",
            log_header
        );
        return;
    }
    log::info!(
        "{} [LOCAL] status=started slots={} identify_steps={} interval_ms={}",
        log_header,
        profile.status_length,
        profile.identify.len(),
        profile.status_interval.as_millis()
    );
    let arming = NEXT_ARMING.fetch_add(1, Ordering::Relaxed);
    let profile = Arc::new(profile);
    let task_profile = profile.clone();
    let task_rack = rack_id.to_string();
    let task_port = port.clone();
    // A refreshed profile on the same live port must retain previously lit slots.
    let shutdown_slots = engines
        .get(rack_id)
        .filter(|engine| Arc::ptr_eq(&engine.port, port))
        .map(|engine| engine.shutdown_slots.clone())
        .unwrap_or_default();
    let task_shutdown_slots = shutdown_slots.clone();
    let handle = async_runtime::spawn(async move {
        run(
            task_profile,
            task_rack.clone(),
            task_port,
            task_shutdown_slots,
        )
        .await;
        let mut engines = lock(&ENGINES);
        if engines
            .get(&task_rack)
            .is_some_and(|engine| engine.arming == arming)
        {
            engines.remove(&task_rack);
        }
    });
    if let Some(old) = engines.insert(
        rack_id.to_string(),
        Engine {
            arming,
            profile_text: payload.to_vec(),
            profile,
            port: port.clone(),
            shutdown_slots,
            handle,
        },
    ) {
        old.handle.abort();
    }
}

/// Stops the engine of one rack, if it runs: the rack went away, or the
/// server drives it itself.
pub(super) fn stop(rack_id: &str, reason: &str) {
    let engine = lock(&ENGINES).remove(rack_id);
    if let Some(engine) = engine {
        engine.handle.abort();
        // The indicators and the slot table belong to the engine: neither outlives it.
        cancel_blinks(rack_id);
        crate::global_app_handle::rack_clear_slots(rack_id);
        log::info!(
            "RACK {} | [LOCAL] status=stopped reason={}",
            rack_id,
            reason
        );
    }
}

/// Stops every engine. Shutdown backstop: unlinking a rack already stops its
/// engine (`unlink_rack`), so this only catches an engine whose rack entry
/// was already gone.
pub(super) fn stop_all() {
    for (rack_id, engine) in lock(&ENGINES).drain() {
        engine.handle.abort();
        log::info!("RACK {} | [LOCAL] status=stopped reason=shutdown", rack_id);
    }
}
