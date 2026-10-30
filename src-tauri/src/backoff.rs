//! Shared exponential-reconnect backoff.
//!
//! Every MQTT-facing loop in the app (the per-card bridge in `mqtt.rs`, the
//! app-level connection in `app_connect.rs`, and the rack/card loops under
//! `com_port/`) retries on the same schedule: start at
//! `RECONNECT_DELAY_INITIAL_SECS`, double on each consecutive failure, and stop
//! growing at `RECONNECT_DELAY_MAX_SECS`. Keeping the schedule in one place
//! means the retry behaviour of all four loops can only be changed together.

/// Initial delay (in seconds) before the first reconnect attempt after a
/// connection failure. Subsequent failures back off exponentially up to
/// [`RECONNECT_DELAY_MAX_SECS`].
pub const RECONNECT_DELAY_INITIAL_SECS: u64 = 10;

/// Upper bound for the reconnect backoff. Past this point we keep retrying
/// at this interval until either the server comes back or the task is killed.
pub const RECONNECT_DELAY_MAX_SECS: u64 = 300;

/// Returns the next reconnect delay given the current one (exponential, capped).
pub fn next_reconnect_delay(current: u64) -> u64 {
    current.saturating_mul(2).min(RECONNECT_DELAY_MAX_SECS)
}

/// How long a connection must hold before it resets the backoff.
const STABLE_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// The reconnect schedule of one connection loop. Only a connection that held
/// for `STABLE_AFTER` resets it: one dropped right after its CONNACK (another
/// instance with the same client id kicking it, a server-side refusal) keeps
/// backing off instead of reconnecting every few seconds forever.
pub struct Backoff {
    delay_secs: u64,
    online_since: Option<std::time::Instant>,
}

impl Backoff {
    pub fn new() -> Self {
        Self {
            delay_secs: RECONNECT_DELAY_INITIAL_SECS,
            online_since: None,
        }
    }

    /// The connection is up.
    pub fn connected(&mut self) {
        self.online_since
            .get_or_insert_with(std::time::Instant::now);
    }

    /// The connection failed: returns the delay to wait before the next attempt.
    ///
    /// The delay is the schedule's value spread by up to `JITTER_PERCENT`
    /// either way (see `jittered`); the schedule itself stays exact.
    pub fn failed(&mut self) -> u64 {
        if self
            .online_since
            .take()
            .is_some_and(|since| since.elapsed() >= STABLE_AFTER)
        {
            self.delay_secs = RECONNECT_DELAY_INITIAL_SECS;
        }
        let delay = self.delay_secs;
        self.delay_secs = next_reconnect_delay(delay);
        jittered(delay)
    }
}

/// Spread of a reconnect delay, in percent of it, either way.
const JITTER_PERCENT: u64 = 20;

/// `delay` moved by a random amount of up to `JITTER_PERCENT` either way, and
/// never past `RECONNECT_DELAY_MAX_SECS`. Every card session of a rack, every
/// reader bridge and the application connection lose the broker together when
/// the network drops; with the exact schedule they would all reconnect in the
/// same second, every time, and hit the broker as one burst. The randomness
/// only has to differ between calls, not be unpredictable: `RandomState` keys
/// are random per process and change with every instance, which is enough, and
/// avoids a dependency for it.
fn jittered(delay: u64) -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let spread = delay.saturating_mul(JITTER_PERCENT) / 100;
    if spread == 0 {
        return delay;
    }
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos()),
    );
    let offset = hasher.finish() % (2 * spread + 1);
    (delay - spread + offset).min(RECONNECT_DELAY_MAX_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_reconnect_delay_doubles_then_saturates() {
        assert_eq!(next_reconnect_delay(10), 20);
        assert_eq!(next_reconnect_delay(20), 40);
        assert_eq!(next_reconnect_delay(40), 80);
        assert_eq!(next_reconnect_delay(150), RECONNECT_DELAY_MAX_SECS);
        assert_eq!(next_reconnect_delay(160), RECONNECT_DELAY_MAX_SECS);
        assert_eq!(
            next_reconnect_delay(RECONNECT_DELAY_MAX_SECS),
            RECONNECT_DELAY_MAX_SECS
        );
    }

    /// `delay` is `base` give or take the jitter, and within the cap.
    fn assert_jittered(delay: u64, base: u64) {
        let spread = base * JITTER_PERCENT / 100;
        assert!(
            delay >= base - spread && delay <= (base + spread).min(RECONNECT_DELAY_MAX_SECS),
            "delay {delay} outside {base} ± {spread}"
        );
    }

    #[test]
    fn a_connection_dropped_right_away_keeps_backing_off() {
        let mut backoff = Backoff::new();
        assert_jittered(backoff.failed(), RECONNECT_DELAY_INITIAL_SECS);
        backoff.connected();
        // dropped at once: the schedule goes on growing
        assert_jittered(backoff.failed(), RECONNECT_DELAY_INITIAL_SECS * 2);
        // held long enough: back to the initial delay
        backoff.connected();
        backoff.online_since = Some(std::time::Instant::now() - STABLE_AFTER);
        assert_jittered(backoff.failed(), RECONNECT_DELAY_INITIAL_SECS);
    }

    #[test]
    fn jitter_stays_within_bounds_and_varies() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..500 {
            let delay = jittered(RECONNECT_DELAY_INITIAL_SECS);
            assert_jittered(delay, RECONNECT_DELAY_INITIAL_SECS);
            seen.insert(delay);
        }
        // 10 s ± 20 % has five possible values; 500 draws hit more than one
        assert!(seen.len() > 1, "jitter never varied: {seen:?}");
    }

    #[test]
    fn jitter_never_exceeds_the_cap() {
        for _ in 0..500 {
            assert!(jittered(RECONNECT_DELAY_MAX_SECS) <= RECONNECT_DELAY_MAX_SECS);
        }
        // too small to spread: returned as is
        assert_eq!(jittered(0), 0);
        assert_eq!(jittered(4), 4);
    }

    #[test]
    fn schedule_climbs_to_the_cap_under_jitter() {
        // the jitter is applied to each returned delay, never fed back into
        // the schedule: it still doubles from the initial delay to the cap
        let mut backoff = Backoff::new();
        let mut base = RECONNECT_DELAY_INITIAL_SECS;
        for _ in 0..12 {
            assert_jittered(backoff.failed(), base);
            base = next_reconnect_delay(base);
        }
        assert_eq!(backoff.delay_secs, RECONNECT_DELAY_MAX_SECS);
    }

    #[test]
    fn next_reconnect_delay_overflow_safe() {
        // saturating_mul prevents overflow; should still cap at max.
        assert_eq!(next_reconnect_delay(u64::MAX), RECONNECT_DELAY_MAX_SECS);
    }

    #[test]
    fn backoff_climbs_from_initial_to_cap_monotonically() {
        // The schedule all four reconnect loops share: never decreases, never
        // exceeds the cap, and actually reaches the cap from the initial delay.
        let mut d = RECONNECT_DELAY_INITIAL_SECS;
        for _ in 0..64 {
            let next = next_reconnect_delay(d);
            assert!(next >= d, "backoff must not shrink: {d} -> {next}");
            assert!(next <= RECONNECT_DELAY_MAX_SECS, "backoff exceeded cap");
            d = next;
        }
        assert_eq!(d, RECONNECT_DELAY_MAX_SECS);
    }
}
