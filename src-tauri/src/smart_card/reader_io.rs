//! PC/SC calls still running per reader.
//!
//! A PC/SC call cannot be interrupted, so a caller that timed out leaves its
//! call running on the blocking pool. The count kept here tells the sweep and
//! the card handle that a reader is still wedged, so nothing new is queued
//! behind that call.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::sync::Arc;

use super::lock_map;

/// Number of PC/SC calls per reader currently running (or queued) on the
/// blocking pool. A PC/SC call cannot be interrupted: when its caller gives up
/// after `CARD_IO_TIMEOUT`, the call keeps its blocking-pool thread until the
/// reader lets go — possibly never. The count is taken before the closure is
/// spawned and released when the closure itself ends (not when its caller
/// stops waiting), so it tells whether the reader is still wedged.
static READER_IO_IN_FLIGHT: std::sync::Mutex<BTreeMap<CString, usize>> =
    std::sync::Mutex::new(BTreeMap::new());

/// Marks one blocking PC/SC call on a reader for as long as it lives; moved
/// into the `spawn_blocking` closure that makes the call.
pub(super) struct ReaderIoGuard(Arc<CStr>);

impl ReaderIoGuard {
    pub(super) fn new(reader_name: &Arc<CStr>) -> Self {
        *lock_map(&READER_IO_IN_FLIGHT)
            .entry(reader_name.as_ref().to_owned())
            .or_insert(0) += 1;
        Self(Arc::clone(reader_name))
    }
}

impl Drop for ReaderIoGuard {
    fn drop(&mut self) {
        let mut in_flight = lock_map(&READER_IO_IN_FLIGHT);
        if let Some(count) = in_flight.get_mut(self.0.as_ref()) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                in_flight.remove(self.0.as_ref());
            }
        }
    }
}

/// True while a blocking PC/SC call on this reader has not returned yet.
pub(super) fn reader_io_in_flight(reader_name: &CStr) -> bool {
    lock_map(&READER_IO_IN_FLIGHT).contains_key(reader_name)
}

/// `reader_io_in_flight` by the reader name as `ProcessingCard` stores it.
/// Covers the close of a dropped card handle too (see `CardCell`), which is
/// what `mqtt::shutdown_connections` waits for.
pub(crate) fn reader_io_pending(reader_name: &str) -> bool {
    CString::new(reader_name).is_ok_and(|name| reader_io_in_flight(&name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_io_marker_lives_until_the_last_guard_drops() {
        let reader: Arc<CStr> = Arc::from(c"Test Reader io-marker 00".to_owned());
        assert!(!reader_io_in_flight(&reader));
        let first = ReaderIoGuard::new(&reader);
        let second = ReaderIoGuard::new(&reader);
        drop(first);
        assert!(reader_io_in_flight(&reader), "one call is still running");
        drop(second);
        assert!(!reader_io_in_flight(&reader));
    }
}
