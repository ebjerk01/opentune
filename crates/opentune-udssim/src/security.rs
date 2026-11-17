//! The injected seed → key relationship the simulated ECU enforces for
//! SecurityAccess (service 0x27).
//!
//! The simulator ships **no** OEM algorithm. A test (or an embedder) supplies a
//! [`SecurityChecker`] that maps a seed to the key the ECU will accept, exactly
//! the way a real ECU's internal transform does. The client under test knows
//! nothing about this relationship; it must obtain the seed, run its own
//! transform, and send the key — so the pairing is what makes the 0x27 exchange
//! a genuine two-party test rather than a tautology.

/// Computes the key a simulated ECU will accept for a given seed and
/// request-seed (odd) access level.
///
/// The ECU compares the key the client sends against [`expected_key`]. Any
/// deterministic function is valid for conformance work; the point is only that
/// the client and the ECU agree on it.
///
/// [`expected_key`]: SecurityChecker::expected_key
pub trait SecurityChecker {
    /// The key the ECU expects for `seed` at the given **request-seed (odd)**
    /// access level (e.g. `0x01`, `0x03`). The key sent with the paired
    /// send-key (even) level must equal this byte-for-byte.
    fn expected_key(&self, seed: &[u8], request_level: u8) -> Vec<u8>;
}

/// A trivial `seed XOR mask` relationship, enough to exercise a full
/// seed→key→unlock round trip without any real algorithm. Each seed byte is
/// XORed with `mask`; the key is the same length as the seed.
#[derive(Debug, Clone, Copy)]
pub struct XorSecurity {
    /// The byte every seed byte is XORed with to form the expected key.
    pub mask: u8,
}

impl XorSecurity {
    /// Build an XOR checker with the given mask.
    pub fn new(mask: u8) -> Self {
        Self { mask }
    }
}

impl SecurityChecker for XorSecurity {
    fn expected_key(&self, seed: &[u8], _request_level: u8) -> Vec<u8> {
        seed.iter().map(|b| b ^ self.mask).collect()
    }
}

/// A checker that accepts no key (every key is wrong), to drive the client's
/// invalid-key / attempt-exhaustion handling deterministically.
#[derive(Debug, Clone, Copy, Default)]
pub struct AlwaysReject;

impl SecurityChecker for AlwaysReject {
    fn expected_key(&self, _seed: &[u8], _request_level: u8) -> Vec<u8> {
        // A key that can never equal what the client derives: longer than any
        // reasonable seed-derived key and a fixed sentinel.
        vec![0xFF; 64]
    }
}
