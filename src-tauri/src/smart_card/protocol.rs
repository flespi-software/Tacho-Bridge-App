//! The T protocol of a card: read out of its ATR, or taken from its config
//! entry, where the ATR-derived value is stored on the first connection.

use log::{error, info, warn};
use pcsc::Protocols;

use crate::config::{get_card_config_from_cache, mutate_card_config};

/// Renders a PCSC protocol as the config string form ("T0"/"T1").
pub fn protocol_to_str(protocol: Protocols) -> &'static str {
    if protocol == Protocols::T1 {
        "T1"
    } else {
        "T0"
    }
}

/// Parses the config string form of a protocol; accepts "T0"/"T1" case-insensitively.
pub fn protocol_from_str(value: &str) -> Option<Protocols> {
    match value.trim() {
        v if v.eq_ignore_ascii_case("T0") => Some(Protocols::T0),
        v if v.eq_ignore_ascii_case("T1") => Some(Protocols::T1),
        _ => None,
    }
}

/// Returns the protocol to use for the card: the `t_protocol` value stored in the
/// card config wins; on the first connection of a configured card the ATR-derived
/// protocol is persisted so later connects and reconnects reuse it.
/// Blocking (config file I/O on first connection) — the reader monitor thread is
/// allowed to block, so this must not be called from an async worker.
pub(super) fn resolve_t_protocol(card_number: &str, atr: &AtrProtocol) -> Protocols {
    let atr_protocol = atr.protocol();

    if card_number.is_empty() {
        // Card is not configured yet - nowhere to store the protocol.
        return atr_protocol;
    }

    match get_card_config_from_cache(card_number).and_then(|card| card.t_protocol) {
        Some(stored) => match protocol_from_str(&stored) {
            Some(protocol) => {
                if protocol != atr_protocol {
                    info!(
                        "Card {}: using stored T protocol {} (ATR suggests {})",
                        card_number,
                        protocol_to_str(protocol),
                        protocol_to_str(atr_protocol)
                    );
                }
                protocol
            }
            None => {
                warn!(
                    "Card {}: invalid t_protocol value {:?} in config (expected \"T0\" or \"T1\"), using ATR-derived {}",
                    card_number,
                    stored,
                    protocol_to_str(atr_protocol)
                );
                atr_protocol
            }
        },
        None => {
            // Only a conclusive ATR read may be persisted. Storing a fallback
            // guess would freeze it: every later connection reuses the stored
            // value without consulting the ATR again, so one truncated read
            // would pin the wrong protocol until the user edits config.yaml.
            if !atr.is_resolved() {
                warn!(
                    "Card {}: ATR did not conclusively identify the T protocol; \
                     using {} for this connection without storing it",
                    card_number,
                    protocol_to_str(atr_protocol)
                );
                return atr_protocol;
            }

            let value = protocol_to_str(atr_protocol);
            if mutate_card_config(card_number, |card| {
                card.t_protocol = Some(value.to_string());
                true
            })
            .is_ok()
            {
                info!(
                    "Card {}: stored ATR-derived T protocol {} in config",
                    card_number, value
                );
            }
            atr_protocol
        }
    }
}

/// Outcome of reading the protocol out of an ATR.
///
/// The distinction matters because the ATR-derived protocol gets *persisted* on
/// a card's first connection (see `resolve_t_protocol`). A truncated ATR and a
/// genuine T=0 ATR both used to yield `Protocols::T0`, so one malformed read
/// could pin the wrong protocol into the config forever — only fixable by hand
/// editing the YAML. Keeping "I could not tell" separate from "it is T0" lets
/// the caller decline to store a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtrProtocol {
    /// The interface bytes were present and complete: this is the card's protocol.
    Resolved(Protocols),
    /// The ATR is absent, malformed, or truncated mid-way through its interface
    /// bytes. Carries the protocol to *use* for this connection (T0, the safe
    /// default), but it must not be written to the config.
    Indeterminate(Protocols),
}

impl AtrProtocol {
    /// The protocol to open the card with, whether or not the parse was conclusive.
    pub fn protocol(&self) -> Protocols {
        match self {
            AtrProtocol::Resolved(p) | AtrProtocol::Indeterminate(p) => *p,
        }
    }

    /// True only when the ATR conclusively identified the protocol — the sole
    /// case where it is safe to persist.
    pub fn is_resolved(&self) -> bool {
        matches!(self, AtrProtocol::Resolved(_))
    }
}

/// Parses the ATR and extracts the communication protocol (T=0 or T=1).
///
/// Walks the interface bytes per ISO/IEC 7816-3: TS, T0, then the TA/TB/TC/TD
/// groups selected by each Y nibble. Every skip is bounds-checked — running past
/// the end means the ATR was cut short and nothing can be concluded from it.
///
/// # Arguments
/// - `atr`: A string containing the ATR in hexadecimal format.
///
/// # Returns
/// - `AtrProtocol`: `Resolved` when the interface bytes were complete,
///   `Indeterminate` when the ATR was absent, malformed or truncated.
pub fn parse_atr_and_get_protocol(atr: &str) -> AtrProtocol {
    fn protocol_from_td(td: u8) -> Protocols {
        match td & 0x0F {
            0x01 => Protocols::T1,
            // T=0 and every other (unsupported) protocol indication fall back
            // to T0, which is what PCSC negotiates for them anyway.
            _ => Protocols::T0,
        }
    }

    let atr_bytes = match hex::decode(atr) {
        Ok(bytes) => bytes,
        Err(_) => {
            error!("Invalid ATR format: {}", atr);
            return AtrProtocol::Indeterminate(Protocols::T0);
        }
    };

    // An empty ATR is the normal "no card in the reader" case (e.g. a card
    // removal event) — nothing to parse, and not worth a warning.
    if atr_bytes.is_empty() {
        return AtrProtocol::Indeterminate(Protocols::T0);
    }

    if atr_bytes.len() < 2 {
        warn!("ATR is too short: {:?}", atr_bytes);
        return AtrProtocol::Indeterminate(Protocols::T0);
    }

    // Advances past the TA/TB/TC bytes selected by `y`, reporting whether they
    // all fit in the ATR. Missing interface bytes make the protocol
    // indeterminate, so the fallback T0 must not be persisted.
    fn skip_interface_bytes(index: &mut usize, y: u8, len: usize) -> bool {
        for bit in [0x1, 0x2, 0x4] {
            if y & bit != 0 {
                *index += 1;
                if *index > len {
                    return false;
                }
            }
        }
        true
    }

    let len = atr_bytes.len();
    let mut index = 1;
    let y1 = atr_bytes[index] >> 4;
    index += 1;

    // TA1, TB1, TC1 — presence selected by Y1.
    if !skip_interface_bytes(&mut index, y1, len) {
        warn!("ATR truncated inside its first interface bytes: {}", atr);
        return AtrProtocol::Indeterminate(Protocols::T0);
    }

    // TD1 absent → only the default protocol (T=0) is offered, which IS
    // conclusive: an ATR without TD1 defines T=0 per ISO 7816-3.
    if y1 & 0x8 == 0 {
        return AtrProtocol::Resolved(Protocols::T0);
    }
    if index >= len {
        warn!("ATR claims TD1 but ends before it: {}", atr);
        return AtrProtocol::Indeterminate(Protocols::T0);
    }
    let td1 = atr_bytes[index];
    index += 1;

    // TA2, TB2, TC2 — presence selected by Y2 (the high nibble of TD1).
    let y2 = td1 >> 4;
    if !skip_interface_bytes(&mut index, y2, len) {
        warn!("ATR truncated inside its second interface bytes: {}", atr);
        return AtrProtocol::Indeterminate(Protocols::T0);
    }

    // TD2 absent → TD1 carries the protocol.
    if y2 & 0x8 == 0 {
        return AtrProtocol::Resolved(protocol_from_td(td1));
    }
    if index >= len {
        warn!("ATR claims TD2 but ends before it: {}", atr);
        return AtrProtocol::Indeterminate(Protocols::T0);
    }

    // TD2 present → it names the preferred protocol, unless it is T=15
    // (global interface bytes follow), which leaves TD1 as the only one.
    let td2 = atr_bytes[index];
    if td2 & 0x0F == 0x0F {
        return AtrProtocol::Resolved(protocol_from_td(td1));
    }
    AtrProtocol::Resolved(protocol_from_td(td2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_str_roundtrip() {
        assert_eq!(protocol_to_str(Protocols::T0), "T0");
        assert_eq!(protocol_to_str(Protocols::T1), "T1");
        assert_eq!(protocol_from_str("T0"), Some(Protocols::T0));
        assert_eq!(protocol_from_str("T1"), Some(Protocols::T1));
    }

    #[test]
    fn protocol_from_str_is_lenient_about_case_and_spaces() {
        assert_eq!(protocol_from_str("t0"), Some(Protocols::T0));
        assert_eq!(protocol_from_str(" t1 "), Some(Protocols::T1));
    }

    #[test]
    fn protocol_from_str_rejects_unknown_values() {
        assert_eq!(protocol_from_str(""), None);
        assert_eq!(protocol_from_str("T2"), None);
        assert_eq!(protocol_from_str("T=0"), None);
        assert_eq!(protocol_from_str("ANY"), None);
    }

    #[test]
    fn atr_without_td1_resolves_to_t0() {
        // TS=3B, T0=0x60 → Y1=0110 (TB1+TC1 present, no TD1). No TD1 means the
        // card offers only the default protocol, which is conclusive.
        assert_eq!(
            parse_atr_and_get_protocol("3B6000FF"),
            AtrProtocol::Resolved(Protocols::T0)
        );
    }

    #[test]
    fn atr_with_td1_resolves_from_td1() {
        // TS=3B, T0=0x81 → Y1=1000 (TD1 present), TD1=0x31 → protocol T=1,
        // Y2=0011 (TA2+TB2 present, no TD2) → TD1 carries the protocol.
        assert_eq!(
            parse_atr_and_get_protocol("3B8131AABB"),
            AtrProtocol::Resolved(Protocols::T1)
        );
    }

    #[test]
    fn atr_with_td2_prefers_td2() {
        // TD1=0x80 → Y2=1000 (TD2 present, no TA2/TB2/TC2), TD2=0x01 → T=1.
        // TD2 wins over TD1's own protocol nibble (0 here).
        assert_eq!(
            parse_atr_and_get_protocol("3B8180 01".replace(' ', "").as_str()),
            AtrProtocol::Resolved(Protocols::T1)
        );
    }

    #[test]
    fn truncated_atr_is_indeterminate_not_a_confident_t0() {
        // Regression: these all used to return a plain Protocols::T0, which
        // resolve_t_protocol then persisted — pinning the wrong protocol for
        // good on a card that is actually T=1.

        // Claims TD1 (Y1=1000) but ends right after T0.
        assert_eq!(
            parse_atr_and_get_protocol("3B81"),
            AtrProtocol::Indeterminate(Protocols::T0)
        );
        // Claims TA1+TB1+TC1+TD1 but only one interface byte follows.
        assert_eq!(
            parse_atr_and_get_protocol("3BFF00"),
            AtrProtocol::Indeterminate(Protocols::T0)
        );
        // TD1 claims TD2 (Y2=1000) but the ATR ends there.
        assert_eq!(
            parse_atr_and_get_protocol("3B8180"),
            AtrProtocol::Indeterminate(Protocols::T0)
        );
    }

    #[test]
    fn absent_or_malformed_atr_is_indeterminate() {
        // No card in the reader, and non-hex garbage.
        assert_eq!(
            parse_atr_and_get_protocol(""),
            AtrProtocol::Indeterminate(Protocols::T0)
        );
        assert_eq!(
            parse_atr_and_get_protocol("3B"),
            AtrProtocol::Indeterminate(Protocols::T0)
        );
        assert_eq!(
            parse_atr_and_get_protocol("nothex"),
            AtrProtocol::Indeterminate(Protocols::T0)
        );
    }

    #[test]
    fn indeterminate_still_offers_a_usable_protocol() {
        // The connection must still be attempted — T0 is the safe default.
        let parsed = parse_atr_and_get_protocol("3B81");
        assert_eq!(parsed.protocol(), Protocols::T0);
        assert!(!parsed.is_resolved(), "a truncated ATR must not be stored");

        let parsed = parse_atr_and_get_protocol("3B8131AABB");
        assert_eq!(parsed.protocol(), Protocols::T1);
        assert!(parsed.is_resolved(), "a complete ATR is safe to store");
    }
}
