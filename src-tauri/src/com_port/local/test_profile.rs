//! The made-up device profile the unit tests of `local` share.

use super::profile::{sum8, Profile};

// A made-up device, unrelated to any real one: frames are
// `D2 4B <addr> <kind> <code> <data..> <check>`, commands of kind `Bx`,
// short replies of kind `E7`, results of kind `E8`.
pub(super) const PROFILE: &str = r#"{
        "version": 1,
        "check": "sum8",
        "status": {
            "cmd": "d24b00a1c0", "interval_ms": 1000, "idle_ms": 100, "deadline_ms": 900,
            "head": "D24B00A1", "len": 18, "offset": 5, "length": 12,
            "mask": "7F", "stable": 2, "gap": 4,
            "classes": {"free": "1A", "none": "2C", "idle": "35", "error": "4E59"}
        },
        "slot": {
            "idle_ms": 40, "deadline_ms": 400,
            "probe": {"cmd": "D24B{a}B1{c}", "empty": ["D24B{a}E71A{c}"]},
            "led": {"served": "D24B{a}B5{c}", "unserved": "D24B{a}B6{c}", "off": "D24B{a}B7{c}"},
            "poll": {"cmd": "D24B{a}B1{c}", "while": "D24B{a}E7A7{c}", "interval_ms": 20, "deadline_ms": 3000},
            "identify": [
                {"cmd": "D24B{a}B9{c}", "expect": "D24B{a}E75E{c}", "deadline_ms": 1200,
                 "ok": {"head": "D24B{a}", "match": [{"at": 3, "mask": "FF", "eq": "E8"}]}},
                {"cmd": "D24B{a}BAC0DE{c}", "expect": "D24B{a}E75E{c}",
                 "ok": {"len": 10, "head": "D24B{a}E8", "match": [{"at": 4, "mask": "F3", "eq": "C3"}],
                        "take_at": 5, "take_len": 4}}
            ]
        }
    }"#;

/// Status bytes of the made-up device.
pub(super) const FREE: u8 = 0x1A;
pub(super) const NONE: u8 = 0x2C;
pub(super) const IDLE: u8 = 0x35;
pub(super) const ERROR: u8 = 0x4E;
pub(super) const BUSY: u8 = 0x63;

pub(super) fn profile() -> Profile {
    Profile::parse(PROFILE.as_bytes()).unwrap()
}

/// A frame of the made-up device with its checksum appended.
pub(super) fn frame(body: &[u8]) -> Vec<u8> {
    let mut bytes = body.to_vec();
    bytes.push(0u8.wrapping_sub(sum8(&bytes)));
    bytes
}
