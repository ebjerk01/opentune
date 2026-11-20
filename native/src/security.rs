//! SecurityAccess (0x27) unlock through the configured seed/key resolvers.
//!
//! The algorithm is never in the core: an embedding build supplies resolvers
//! ([`crate::NativeConfig::seedkeys`]), the ECU definition names the one it
//! needs (its `seedkey` id), and the panel names the access level of its
//! operation. [`Unlocker`] is what a view takes into its adapter job:
//!
//! ```ignore
//! let unlock = host.unlocker()?;               // the definition's seedkey id
//! adapter.read("read flash", host.target()?, move |d| {
//!     unlock.unlock_uds(d, 0x01)?;             // 27 01 -> key -> 27 02
//!     /* protected reads */
//! })?;
//! ```
//!
//! Safety: nothing is sent when the id cannot be resolved or the level is not
//! a request-seed (odd) level; a rejected key is reported and never retried
//! (each wrong key spends one of the ECU's attempts); an all-zero seed means
//! "already unlocked" and no key is sent.

use crate::host::RawTransport;
use opentune_core::protocol::uds::seed_is_unlocked;
use opentune_core::protocol::{Kwp2000Client, UdsClient};
use opentune_core::{Error, RawDiag};
use opentune_hub::seedkeys::SeedKeys;

/// How an unlock ended (both mean the level is now open).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unlocked {
    /// The key was accepted.
    KeyAccepted,
    /// The ECU sent an all-zero seed: it was already unlocked at this level.
    AlreadyUnlocked,
}

/// A seed/key algorithm bound to an id, ready to run inside an adapter job
/// (`Send`, cheap to clone).
#[derive(Clone)]
pub struct Unlocker {
    keys: SeedKeys,
    id: String,
}

fn check_level(level: u8) -> Result<(), String> {
    if level % 2 == 1 && level < 0x7F {
        Ok(())
    } else {
        Err(format!(
            "security level 0x{level:02X} is not a request-seed level (odd, 0x01..0x7D); \
             the key is sent at level + 1"
        ))
    }
}

fn denial(e: Error, level: u8) -> String {
    match e {
        Error::SecurityDenied => format!(
            "the ECU rejected the key for level 0x{level:02X} (invalid key or attempts \
             exhausted); not retried — a wrong algorithm spends the ECU's attempts"
        ),
        Error::NegativeResponse { code: 0x37, .. } => format!(
            "level 0x{level:02X}: the ECU's security delay has not expired (NRC 0x37); \
             wait (often 10 s, after a power cycle on some ECUs) before trying again"
        ),
        e => format!("security access 0x{level:02X}: {e}"),
    }
}

impl Unlocker {
    pub(crate) fn new(keys: SeedKeys, id: &str) -> Result<Self, String> {
        let id = id.trim().to_string();
        // Resolve now so a missing algorithm fails before any request.
        keys.require(&id)?;
        Ok(Self { keys, id })
    }

    /// The seed/key id this unlocker computes with.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The resolved seed/key algorithm, for core engines that take one
    /// directly (a secured actuator test's login, a service script's key
    /// provider, a KWP login). It only computes keys: an ECU change still
    /// needs `Host::confirm_ecu` + a backup + `mutate` / `actuate`.
    pub fn seed_key(&self) -> Result<Box<dyn opentune_core::protocol::seedkey::SeedKey>, String> {
        self.keys.require(&self.id)
    }

    fn key(&self, seed: &[u8], level: u8) -> Result<Vec<u8>, String> {
        let h = self.keys.require(&self.id)?;
        opentune_hub::seedkeys::compute(h.as_ref(), seed, level)
    }

    /// UDS (ISO 14229) SecurityAccess at request-seed `level` (odd):
    /// `27 level` → seed → key → `27 level+1 key`.
    pub fn unlock_uds(&self, d: &mut dyn RawDiag, level: u8) -> Result<Unlocked, String> {
        check_level(level)?;
        let mut t = RawTransport::new(d);
        let mut uds = UdsClient::new(&mut t);
        let seed = uds.request_seed(level).map_err(|e| denial(e, level))?;
        if seed.is_empty() {
            // A key over nothing would only spend one of the ECU's attempts.
            return Err(format!(
                "security access 0x{level:02X}: the ECU sent an empty seed; no key sent"
            ));
        }
        if seed_is_unlocked(&seed) {
            return Ok(Unlocked::AlreadyUnlocked);
        }
        let key = self.key(&seed, level)?;
        uds.send_key(level + 1, &key)
            .map_err(|e| denial(e, level))?;
        Ok(Unlocked::KeyAccepted)
    }

    /// KWP2000 (ISO 14230) securityAccess at request-seed `level` (odd).
    pub fn unlock_kwp(&self, d: &mut dyn RawDiag, level: u8) -> Result<Unlocked, String> {
        check_level(level)?;
        let mut t = RawTransport::new(d);
        let mut kwp = Kwp2000Client::new(&mut t);
        let seed = kwp.request_seed(level).map_err(|e| denial(e, level))?;
        if seed_is_unlocked(&seed) {
            return Ok(Unlocked::AlreadyUnlocked);
        }
        let key = self.key(&seed, level)?;
        kwp.send_key(level + 1, &key)
            .map_err(|e| denial(e, level))?;
        Ok(Unlocked::KeyAccepted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentune_core::protocol::seedkey::SeedKey;

    /// Synthetic algorithm for tests: key = bitwise NOT of the seed.
    struct NotSeed;
    impl SeedKey for NotSeed {
        fn id(&self) -> &str {
            "test-not"
        }
        fn compute(&self, seed: &[u8], _level: u8) -> opentune_core::Result<Vec<u8>> {
            Ok(seed.iter().map(|b| !b).collect())
        }
    }
    fn resolver(id: &str) -> Option<Box<dyn SeedKey>> {
        (id == "test-not").then(|| Box::new(NotSeed) as Box<dyn SeedKey>)
    }

    fn keys() -> SeedKeys {
        SeedKeys::new(vec![resolver])
    }

    /// A scripted ECU: seed `seed` at level 1, accepts NOT(seed) at level 2.
    fn ecu(
        seed: Vec<u8>,
        sent: &mut Vec<Vec<u8>>,
    ) -> impl FnMut(&[u8]) -> Result<Vec<u8>, String> + '_ {
        move |req: &[u8]| {
            sent.push(req.to_vec());
            match req {
                [0x27, 0x01] => Ok([vec![0x67, 0x01], seed.clone()].concat()),
                [0x27, 0x02, key @ ..] => {
                    let want: Vec<u8> = seed.iter().map(|b| !b).collect();
                    Ok(if key == want.as_slice() {
                        vec![0x67, 0x02]
                    } else {
                        vec![0x7F, 0x27, 0x35]
                    })
                }
                _ => Ok(vec![0x7F, req[0], 0x11]),
            }
        }
    }

    #[test]
    fn seed_key_hands_out_the_resolved_algorithm() {
        let u = Unlocker::new(keys(), "test-not").unwrap();
        let sk = u.seed_key().unwrap();
        assert_eq!(sk.id(), "test-not");
        assert_eq!(sk.compute(&[0x12, 0x34], 1).unwrap(), vec![0xED, 0xCB]);
    }

    #[test]
    fn uds_unlock_sends_the_computed_key() {
        let u = Unlocker::new(keys(), " test-not ").unwrap();
        assert_eq!(u.id(), "test-not");
        let mut sent = Vec::new();
        let mut d = ecu(vec![0x12, 0x34, 0x56, 0x78], &mut sent);
        assert_eq!(u.unlock_uds(&mut d, 0x01), Ok(Unlocked::KeyAccepted));
        drop(d);
        assert_eq!(sent[1], vec![0x27, 0x02, 0xED, 0xCB, 0xA9, 0x87]);
    }

    #[test]
    fn zero_seed_means_already_unlocked() {
        let u = Unlocker::new(keys(), "test-not").unwrap();
        let mut sent = Vec::new();
        let mut d = ecu(vec![0, 0, 0, 0], &mut sent);
        assert_eq!(u.unlock_uds(&mut d, 0x01), Ok(Unlocked::AlreadyUnlocked));
        drop(d);
        assert_eq!(sent.len(), 1, "no key sent");
    }

    #[test]
    fn nothing_is_sent_without_an_algorithm_or_with_a_bad_level() {
        let e = Unlocker::new(keys(), "vendor-x").err().unwrap();
        assert!(e.contains("vendor-x"), "{e}");
        assert!(Unlocker::new(SeedKeys::default(), "test-not").is_err());
        let u = Unlocker::new(keys(), "test-not").unwrap();
        let mut sent = Vec::new();
        let mut d = ecu(vec![1], &mut sent);
        assert!(u.unlock_uds(&mut d, 0x02).is_err(), "even = send-key level");
        assert!(u.unlock_kwp(&mut d, 0x7F).is_err());
        drop(d);
        assert!(sent.is_empty());
    }

    #[test]
    fn a_rejected_key_is_not_retried() {
        let u = Unlocker::new(keys(), "test-not").unwrap();
        let mut sent = Vec::new();
        let mut d = |req: &[u8]| -> Result<Vec<u8>, String> {
            sent.push(req.to_vec());
            Ok(match req[1] {
                0x01 => vec![0x67, 0x01, 0xAA],
                _ => vec![0x7F, 0x27, 0x35],
            })
        };
        let e = u.unlock_uds(&mut d, 0x01).unwrap_err();
        assert!(e.contains("rejected") && e.contains("not retried"), "{e}");
        assert_eq!(sent.iter().filter(|r| r[1] == 0x02).count(), 1);
        // The delay NRC is reported as such.
        let mut d = |req: &[u8]| -> Result<Vec<u8>, String> { Ok(vec![0x7F, req[0], 0x37]) };
        let e = u.unlock_uds(&mut d, 0x01).unwrap_err();
        assert!(e.contains("delay"), "{e}");
    }

    #[test]
    fn kwp_unlock() {
        let u = Unlocker::new(keys(), "test-not").unwrap();
        let mut sent = Vec::new();
        let mut d = ecu(vec![0x0F, 0xF0], &mut sent);
        assert_eq!(u.unlock_kwp(&mut d, 0x01), Ok(Unlocked::KeyAccepted));
        drop(d);
        assert_eq!(sent[1], vec![0x27, 0x02, 0xF0, 0x0F]);
    }

    #[test]
    fn unlocker_moves_into_an_adapter_job() {
        fn is_send<T: Send + 'static>(_: &T) {}
        is_send(&Unlocker::new(keys(), "test-not").unwrap());
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    /// Synthetic algorithm: always a 2-byte key (sum of the seed, level).
    struct Short;
    impl SeedKey for Short {
        fn id(&self) -> &str {
            "test-short"
        }
        fn compute(&self, seed: &[u8], level: u8) -> opentune_core::Result<Vec<u8>> {
            let s = seed.iter().fold(0u8, |a, b| a.wrapping_add(*b));
            Ok(vec![s, level])
        }
    }
    /// Synthetic algorithm that cannot compute a key.
    struct Fails;
    impl SeedKey for Fails {
        fn id(&self) -> &str {
            "test-fails"
        }
        fn compute(&self, _seed: &[u8], _level: u8) -> opentune_core::Result<Vec<u8>> {
            Err(opentune_core::Error::Unsupported(
                "synthetic: seed length".into(),
            ))
        }
    }
    fn more(id: &str) -> Option<Box<dyn SeedKey>> {
        match id {
            "test-short" => Some(Box::new(Short)),
            "test-fails" => Some(Box::new(Fails)),
            _ => None,
        }
    }
    fn all_keys() -> SeedKeys {
        SeedKeys::new(vec![resolver, more])
    }

    /// A scripted ECU at `level`: answers the seed request with `seed_reply`
    /// and accepts any key with `67 level+1`.
    fn scripted(
        seed_reply: Vec<u8>,
        sent: &mut Vec<Vec<u8>>,
    ) -> impl FnMut(&[u8]) -> Result<Vec<u8>, String> + '_ {
        move |req: &[u8]| {
            sent.push(req.to_vec());
            Ok(match req {
                [0x27, l] if l % 2 == 1 => seed_reply.clone(),
                [0x27, l, ..] => vec![0x67, *l],
                _ => vec![0x7F, req[0], 0x11],
            })
        }
    }

    #[test]
    fn a_key_of_another_length_than_the_seed_is_sent_as_computed() {
        let u = Unlocker::new(all_keys(), "test-short").unwrap();
        let mut sent = Vec::new();
        let mut d = scripted(vec![0x67, 0x11, 0x01, 0x02, 0x03, 0x04], &mut sent);
        assert_eq!(u.unlock_uds(&mut d, 0x11), Ok(Unlocked::KeyAccepted));
        drop(d);
        assert_eq!(sent, vec![vec![0x27, 0x11], vec![0x27, 0x12, 0x0A, 0x11]]);
        // A one-byte seed, and the highest request-seed level.
        let u = Unlocker::new(all_keys(), "test-not").unwrap();
        let mut sent = Vec::new();
        let mut d = scripted(vec![0x50, 0x7D, 0x5A], &mut sent);
        // (A reply that is not a 0x67 at all is an error, not a seed.)
        let e = u.unlock_kwp(&mut d, 0x7D).unwrap_err();
        assert!(e.contains("0x7D") && e.contains("0x50"), "{e}");
        drop(d);
        assert_eq!(sent.len(), 1);
        let mut sent = Vec::new();
        let mut d = scripted(vec![0x67, 0x7D, 0x5A], &mut sent);
        assert_eq!(u.unlock_kwp(&mut d, 0x7D), Ok(Unlocked::KeyAccepted));
        drop(d);
        assert_eq!(sent[1], vec![0x27, 0x7E, 0xA5]);
    }

    #[test]
    fn a_seed_echo_for_another_level_sends_no_key() {
        let u = Unlocker::new(keys(), "test-not").unwrap();
        for kwp in [false, true] {
            let mut sent = Vec::new();
            // Asked for level 0x01, the ECU answers with level 0x03's seed.
            let mut d = scripted(vec![0x67, 0x03, 0x12, 0x34], &mut sent);
            let r = if kwp {
                u.unlock_kwp(&mut d, 0x01)
            } else {
                u.unlock_uds(&mut d, 0x01)
            };
            drop(d);
            let e = r.unwrap_err();
            assert!(e.contains("0x03") && e.contains("0x01"), "kwp={kwp}: {e}");
            assert_eq!(sent, vec![vec![0x27, 0x01]], "kwp={kwp}: no key sent");
        }
    }

    #[test]
    fn a_key_acknowledged_for_another_level_is_not_an_unlock() {
        let u = Unlocker::new(keys(), "test-not").unwrap();
        for kwp in [false, true] {
            let mut d = |req: &[u8]| -> Result<Vec<u8>, String> {
                Ok(match req[1] {
                    0x01 => vec![0x67, 0x01, 0x12],
                    _ => vec![0x67, 0x04],
                })
            };
            let r = if kwp {
                u.unlock_kwp(&mut d, 0x01)
            } else {
                u.unlock_uds(&mut d, 0x01)
            };
            assert!(r.is_err(), "kwp={kwp}: {r:?}");
        }
    }

    #[test]
    fn a_resolver_that_cannot_compute_sends_no_key() {
        let u = Unlocker::new(all_keys(), "test-fails").unwrap();
        for kwp in [false, true] {
            let mut sent = Vec::new();
            let mut d = scripted(vec![0x67, 0x01, 0xAB, 0xCD], &mut sent);
            let r = if kwp {
                u.unlock_kwp(&mut d, 0x01)
            } else {
                u.unlock_uds(&mut d, 0x01)
            };
            drop(d);
            let e = r.unwrap_err();
            assert!(
                e.contains("test-fails") && e.contains("synthetic: seed length"),
                "kwp={kwp}: {e}"
            );
            assert_eq!(sent.len(), 1, "kwp={kwp}: only the seed request");
        }
    }

    #[test]
    fn levels_and_seeds_at_the_edges() {
        let u = Unlocker::new(keys(), "test-not").unwrap();
        let mut sent = Vec::new();
        let mut d = scripted(vec![0x67, 0x01, 0x00], &mut sent);
        for bad in [0x00, 0x7F, 0x80, 0x81, 0xFF] {
            assert!(u.unlock_uds(&mut d, bad).is_err(), "0x{bad:02X}");
            assert!(u.unlock_kwp(&mut d, bad).is_err(), "0x{bad:02X}");
        }
        drop(d);
        assert!(sent.is_empty(), "nothing sent for a bad level");
        // A seed that is only partly zero is a real seed.
        let mut sent = Vec::new();
        let mut d = scripted(vec![0x67, 0x01, 0x00, 0x00, 0x01], &mut sent);
        assert_eq!(u.unlock_uds(&mut d, 0x01), Ok(Unlocked::KeyAccepted));
        drop(d);
        assert_eq!(sent[1], vec![0x27, 0x02, 0xFF, 0xFF, 0xFE]);
        // KWP: an all-zero seed means already unlocked; an empty one is refused.
        let mut sent = Vec::new();
        let mut d = scripted(vec![0x67, 0x01, 0x00, 0x00], &mut sent);
        assert_eq!(u.unlock_kwp(&mut d, 0x01), Ok(Unlocked::AlreadyUnlocked));
        drop(d);
        assert_eq!(sent.len(), 1);
        let mut sent = Vec::new();
        let mut d = scripted(vec![0x67, 0x01], &mut sent);
        assert!(u.unlock_kwp(&mut d, 0x01).is_err());
        drop(d);
        assert_eq!(sent.len(), 1, "no key for an empty KWP seed");
    }

    /// UDS: a `67 01` reply without seed bytes must not lead to a key
    /// (KWP already refuses it as "empty seed"); each sent key can spend
    /// one of the ECU's attempts.
    #[test]
    fn an_empty_uds_seed_sends_no_key() {
        let u = Unlocker::new(keys(), "test-not").unwrap();
        let mut sent = Vec::new();
        let mut d = scripted(vec![0x67, 0x01], &mut sent);
        let r = u.unlock_uds(&mut d, 0x01);
        drop(d);
        assert!(r.is_err(), "{r:?}");
        assert_eq!(sent.len(), 1, "only the seed request: {sent:02X?}");
    }

    #[test]
    fn exhausted_attempts_and_a_lost_link_are_reported_without_a_retry() {
        let u = Unlocker::new(keys(), "test-not").unwrap();
        for kwp in [false, true] {
            let mut sent = Vec::new();
            let mut d = |req: &[u8]| -> Result<Vec<u8>, String> {
                sent.push(req.to_vec());
                Ok(match req[1] {
                    0x01 => vec![0x67, 0x01, 0x42],
                    _ => vec![0x7F, 0x27, 0x36],
                })
            };
            let r = if kwp {
                u.unlock_kwp(&mut d, 0x01)
            } else {
                u.unlock_uds(&mut d, 0x01)
            };
            let e = r.unwrap_err();
            assert!(e.contains("rejected"), "kwp={kwp}: {e}");
            assert_eq!(sent.len(), 2, "kwp={kwp}: one key, never retried");
        }
        let mut sent = Vec::new();
        let mut d = |req: &[u8]| -> Result<Vec<u8>, String> {
            sent.push(req.to_vec());
            Err(format!(
                "{}adapter reset",
                opentune_core::resilient::LINK_LOST_PREFIX
            ))
        };
        let e = u.unlock_uds(&mut d, 0x01).unwrap_err();
        assert!(e.contains("adapter reset"), "{e}");
        assert_eq!(sent.len(), 1);
    }
}
