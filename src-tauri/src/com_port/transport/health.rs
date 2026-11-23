//! Bookkeeping per open port: its I/O-fault counter, whether its rack was
//! unlinked, and the bytes the rack sent on its own. Every entry is keyed by
//! `port_key`.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serialport::SerialPort;
use tokio::sync::Mutex as AsyncMutex;

use super::SharedPort;
use crate::com_port::{lock, SHUTTING_DOWN};

/// A port held weakly: it does not keep the port open.
type WeakPort = std::sync::Weak<AsyncMutex<Box<dyn SerialPort>>>;

/// Transport health of one open port, keyed by `port_key`.
#[derive(Default)]
struct PortHealth {
    /// The operation running now hit an I/O error (cleared by a good read).
    op_faulted: bool,
    /// Consecutive operations that ended with an I/O error.
    failed_ops: u32,
    /// When the last operation on the port ended without a usable reply: the
    /// rack may still answer it, and bytes found on the line soon after are
    /// that late answer rather than an event of the rack.
    last_failed_op: Option<Instant>,
}

lazy_static::lazy_static! {
    /// Health of every linked port. A USB brown-out or a cable reseat inside
    /// one presence poll brings the device back under the SAME name, so the
    /// monitor sees no change while our handle is dead: every write fails
    /// from then on. Counting the I/O errors per port is what lets the monitor
    /// notice and reopen it (see `port_failed_ops`). Plain timeouts are not
    /// faults — a silent device is a protocol matter, not a dead handle.
    static ref PORT_HEALTH: std::sync::Mutex<std::collections::HashMap<usize, PortHealth>> =
        std::sync::Mutex::new(std::collections::HashMap::new());

    /// Ports whose rack was unlinked (`release_port`), held weakly: an entry
    /// lives exactly as long as an operation still holds the port, so its
    /// `port_key` cannot be confused with a later port at the same address.
    static ref RELEASED_PORTS: std::sync::Mutex<Vec<(usize, WeakPort)>> =
        std::sync::Mutex::new(Vec::new());
}

/// Identity of an open port for `PORT_HEALTH`: the address of the boxed port
/// object, stable for as long as the port stays open. Computable both from the
/// box before it is shared and from a locked guard.
pub(in crate::com_port) fn port_key(port: &dyn SerialPort) -> usize {
    port as *const dyn SerialPort as *const () as usize
}

/// Bytes a rack sent on its own, outside any operation, kept per port (by
/// `port_key`) until the local engine of the rack takes them: the engine
/// matches them against its profile to learn which slots to look at again
/// before the next status poll. Bounded, since a rack the server drives has
/// no engine to drain them.
static UNSOLICITED: std::sync::Mutex<Vec<(usize, Vec<u8>)>> = std::sync::Mutex::new(Vec::new());
const UNSOLICITED_MAX: usize = 64;

/// How long after a failed operation stale bytes on its port are taken for
/// its late reply and kept out of the rack events: a timed-out card operation
/// may still answer, and that answer cannot be told from unsolicited input.
const LATE_REPLY_WINDOW: Duration = Duration::from_secs(10);

/// Keeps bytes the rack on this port sent on its own (see `UNSOLICITED`),
/// dropping the oldest entry once the list is full.
pub(super) fn record_unsolicited(key: usize, bytes: &[u8]) {
    let mut list = lock(&UNSOLICITED);
    if list.len() >= UNSOLICITED_MAX {
        list.remove(0);
    }
    list.push((key, bytes.to_vec()));
}

/// Takes the bytes the rack on this port sent on its own since the last call,
/// one entry per burst, oldest first.
pub(in crate::com_port) fn take_unsolicited(key: usize) -> Vec<Vec<u8>> {
    let mut list = lock(&UNSOLICITED);
    let (mine, rest): (Vec<_>, Vec<_>) = list.drain(..).partition(|(k, _)| *k == key);
    *list = rest;
    mine.into_iter().map(|(_, bytes)| bytes).collect()
}

/// Starts tracking a freshly opened port. Only registered ports are counted,
/// so a fault recorded after `untrack_port` cannot leave a stale entry for an
/// address a later port may reuse.
pub(in crate::com_port) fn track_port(key: usize) {
    lock(&PORT_HEALTH).insert(key, PortHealth::default());
}

/// Stops tracking a closed port (see `track_port`).
pub(in crate::com_port) fn untrack_port(key: usize) {
    lock(&PORT_HEALTH).remove(&key);
    // Events of a closed port have no engine left to take them.
    lock(&UNSOLICITED).retain(|(k, _)| *k != key);
}

/// Marks the port of an unlinked rack: an operation still running on it stops
/// at its next check (see `must_stop`).
pub(in crate::com_port) fn release_port(key: usize, port: &SharedPort) {
    let mut released = lock(&RELEASED_PORTS);
    released.retain(|(_, port)| port.strong_count() > 0);
    released.push((key, Arc::downgrade(port)));
}

/// The app is closing, or the port's rack was unlinked: an operation still
/// running on the port stops waiting, so its blocking closure drops the
/// handle — `spawn_blocking` cannot be aborted from outside, and until the
/// handle closes the device cannot be reopened (on Windows: "Access is
/// denied").
pub(super) fn must_stop(port: &dyn SerialPort) -> bool {
    let key = port_key(port);
    SHUTTING_DOWN.load(Ordering::SeqCst)
        || lock(&RELEASED_PORTS)
            .iter()
            .any(|(released, port)| *released == key && port.strong_count() > 0)
}

/// Consecutive operations on this port that failed with an I/O error.
pub(in crate::com_port) fn port_failed_ops(key: usize) -> u32 {
    lock(&PORT_HEALTH)
        .get(&key)
        .map_or(0, |health| health.failed_ops)
}

/// Records an I/O error of the operation running on the port.
pub(super) fn note_io_fault(port: &dyn SerialPort) {
    if let Some(health) = lock(&PORT_HEALTH).get_mut(&port_key(port)) {
        health.op_faulted = true;
    }
}

/// Records an operation that ended without a usable reply (see
/// `PortHealth::last_failed_op`).
pub(super) fn note_failed_op(port: &dyn SerialPort) {
    if let Some(health) = lock(&PORT_HEALTH).get_mut(&port_key(port)) {
        health.last_failed_op = Some(Instant::now());
    }
}

/// Whether stale bytes on the port may be the late reply of an operation that
/// failed shortly before.
pub(super) fn late_reply_possible(port: &dyn SerialPort) -> bool {
    lock(&PORT_HEALTH)
        .get(&port_key(port))
        .and_then(|health| health.last_failed_op)
        .is_some_and(|at| at.elapsed() < LATE_REPLY_WINDOW)
}

/// Records bytes received from the port: the handle is alive, its fault
/// counter starts over.
pub(super) fn note_io_ok(port: &dyn SerialPort) {
    if let Some(health) = lock(&PORT_HEALTH).get_mut(&port_key(port)) {
        health.op_faulted = false;
        health.failed_ops = 0;
    }
}

/// Closes the bookkeeping of one operation; called under the port lock, so
/// operations of one port never interleave here.
pub(super) fn finish_op(port: &dyn SerialPort) {
    if let Some(health) = lock(&PORT_HEALTH).get_mut(&port_key(port)) {
        if std::mem::take(&mut health.op_faulted) {
            health.failed_ops = health.failed_ops.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_port::ScriptedPort;
    use super::*;

    #[test]
    fn port_health_counts_failed_operations_until_a_good_read() {
        // Any boxed port works as a key holder; the counter itself is what is tested.
        let port: Box<dyn SerialPort> = Box::new(ScriptedPort::new(&[]));
        let key = port_key(&*port);
        // untracked ports are ignored
        note_io_fault(&*port);
        finish_op(&*port);
        assert_eq!(port_failed_ops(key), 0);

        track_port(key);
        // several faults inside one operation count once
        note_io_fault(&*port);
        note_io_fault(&*port);
        finish_op(&*port);
        assert_eq!(port_failed_ops(key), 1);
        // an operation without faults leaves the count alone (timeouts are not faults)
        finish_op(&*port);
        assert_eq!(port_failed_ops(key), 1);
        note_io_fault(&*port);
        finish_op(&*port);
        assert_eq!(port_failed_ops(key), 2);
        // a byte received proves the handle alive
        note_io_ok(&*port);
        finish_op(&*port);
        assert_eq!(port_failed_ops(key), 0);

        untrack_port(key);
        assert_eq!(port_failed_ops(key), 0);
    }

    #[test]
    fn stale_bytes_after_a_failed_operation_are_taken_for_its_late_reply() {
        let port: Box<dyn SerialPort> = Box::new(ScriptedPort::new(&[]));
        let key = port_key(&*port);
        track_port(key);
        assert!(!late_reply_possible(&*port));
        note_failed_op(&*port);
        assert!(late_reply_possible(&*port));
        untrack_port(key);
        assert!(!late_reply_possible(&*port));
    }
}
