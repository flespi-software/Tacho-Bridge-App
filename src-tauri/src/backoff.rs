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
