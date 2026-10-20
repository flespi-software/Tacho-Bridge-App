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
        delay
    }
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

    #[test]
    fn a_connection_dropped_right_away_keeps_backing_off() {
        let mut backoff = Backoff::new();
        assert_eq!(backoff.failed(), RECONNECT_DELAY_INITIAL_SECS);
        backoff.connected();
        // dropped at once: the schedule goes on growing
        assert_eq!(backoff.failed(), RECONNECT_DELAY_INITIAL_SECS * 2);
        // held long enough: back to the initial delay
        backoff.connected();
        backoff.online_since = Some(std::time::Instant::now() - STABLE_AFTER);
        assert_eq!(backoff.failed(), RECONNECT_DELAY_INITIAL_SECS);
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
