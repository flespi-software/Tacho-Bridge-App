//! When a card last exchanged anything: the auto-updater waits for the card
//! traffic to go quiet before an install restarts the application.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Sentinel for "no card exchange has happened in this process yet". Distinct
/// from a real timestamp because the monotonic clock starts near 0: a plain 0
/// would read as "a card was busy at process start" and needlessly postpone
/// unattended installs for the first minute of every run.
const CARD_ACTIVITY_NONE: u64 = u64::MAX;

/// Seconds (on a monotonic clock) of the most recent card-facing exchange: a
/// server request on a reader-backed card connection, or a rack serial
/// envelope. A timestamp (not a session counter) on purpose — a counter would
/// leak on an aborted task and block auto-updates forever, while an APDU flow
/// going quiet is a robust "no authentication in progress" signal.
///
/// Measured against `PROCESS_START` rather than the wall clock: the auto-updater
/// gates an app-restarting install on this going quiet, and an NTP correction
/// stepping the wall clock forward would fake the quiet period and kill the app
/// mid-authentication (a backward step would block updates just as wrongly).
static LAST_CARD_ACTIVITY_SECS: AtomicU64 = AtomicU64::new(CARD_ACTIVITY_NONE);

lazy_static::lazy_static! {
    /// Monotonic origin for `LAST_CARD_ACTIVITY_SECS`. `Instant` is not const-
    /// constructible, so the origin is captured on first use.
    static ref PROCESS_START: Instant = Instant::now();
}

fn monotonic_now_secs() -> u64 {
    PROCESS_START.elapsed().as_secs()
}

/// Records "a card exchange is happening right now". Called on every server
/// request that reaches a card (reader APDU bridge and rack serial bridge).
pub fn touch_card_activity() {
    LAST_CARD_ACTIVITY_SECS.store(monotonic_now_secs(), Ordering::Relaxed);
}

/// Seconds since the last card exchange; a large value when none happened yet.
pub fn seconds_since_card_activity() -> u64 {
    let last = LAST_CARD_ACTIVITY_SECS.load(Ordering::Relaxed);
    if last == CARD_ACTIVITY_NONE {
        return u64::MAX;
    }
    monotonic_now_secs().saturating_sub(last)
}
