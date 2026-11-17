//! Plugin-supplied seed/key algorithms (UDS / KWP SecurityAccess).
//!
//! OpenTune ships **no** OEM seed/key algorithm, and this module adds none. It
//! is an extension point: an embedding app (a plugin build) passes resolvers in
//! [`DesktopConfig::seedkeys`](crate::DesktopConfig::seedkeys) and the core asks
//! them, by id, wherever it can unlock a module:
//!
//! * a service script that names a `seedkey` id ([`crate::feat_service`]);
//! * the Utilities seed/key calculator, for an id that is not one of its
//!   generic transforms (the result is labelled as plugin-supplied);
//! * a plugin's own read/flash commands, which attach a resolved handler to a
//!   hub-opened driver with [`crate::adapter::apply_seed_key`].
//!
//! The plugin owns the algorithm and its provenance; when no resolver knows an
//! id, every caller keeps the honest "plan only / not available" behaviour.

use opentune_core::protocol::seedkey::SeedKey;

/// Looks up a seed/key algorithm by id (see [`crate::DesktopConfig::seedkeys`]).
/// Return `None` for ids the resolver does not know.
pub type SeedKeyResolver = fn(&str) -> Option<Box<dyn SeedKey>>;

/// Seed/key algorithm lookup (managed state): the embedder's resolvers, in
/// order. The public app has none, so [`get`](SeedKeys::get) always returns
/// `None` there.
#[derive(Clone, Default)]
pub struct SeedKeys(Vec<SeedKeyResolver>);

impl SeedKeys {
    /// A lookup over `resolvers`, consulted in order.
    pub fn new(resolvers: Vec<SeedKeyResolver>) -> Self {
        SeedKeys(resolvers)
    }

    /// Resolve `id` (trimmed) with the first resolver that knows it. A blank id
    /// never resolves.
    pub fn get(&self, id: &str) -> Option<Box<dyn SeedKey>> {
        let id = id.trim();
        if id.is_empty() {
            return None;
        }
        self.0.iter().find_map(|r| r(id))
    }

    /// Like [`get`](SeedKeys::get), with the standard "not available" error.
    pub fn require(&self, id: &str) -> Result<Box<dyn SeedKey>, String> {
        self.get(id).ok_or_else(|| unavailable(id))
    }

    /// Whether no resolver is registered (the public app).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The message for an id no resolver knows.
pub fn unavailable(id: &str) -> String {
    format!(
        "no seed/key algorithm '{}' is available in this build — OpenTune ships none; it must come from a plugin",
        id.trim()
    )
}

/// Run a resolved handler as a `KeyProvider`-shaped closure body.
pub(crate) fn compute(
    handler: &dyn SeedKey,
    seed: &[u8],
    level: u8,
) -> std::result::Result<Vec<u8>, String> {
    handler
        .compute(seed, level)
        .map_err(|e| format!("seed/key '{}': {e}", handler.id()))
}

#[cfg(test)]
pub(crate) mod testkeys {
    //! Test-only resolvers. `vcar-test` is the virtual car's documented
    //! TEST-ONLY transform (`opentune_driver_vcar::test_security_key`), never a
    //! manufacturer algorithm.
    use super::*;

    pub struct VcarTest;

    impl SeedKey for VcarTest {
        fn id(&self) -> &str {
            "vcar-test"
        }
        fn compute(&self, seed: &[u8], _level: u8) -> opentune_core::Result<Vec<u8>> {
            let mask = opentune_driver_vcar::Profile::demo().security.xor_mask;
            Ok(opentune_driver_vcar::test_security_key(seed, mask))
        }
    }

    /// A fixed transform tagged with the resolver that produced it.
    pub struct Tagged(pub &'static str, pub u8);

    impl SeedKey for Tagged {
        fn id(&self) -> &str {
            self.0
        }
        fn compute(&self, seed: &[u8], level: u8) -> opentune_core::Result<Vec<u8>> {
            Ok(seed.iter().map(|b| b ^ self.1 ^ level).collect())
        }
    }

    pub fn vcar(id: &str) -> Option<Box<dyn SeedKey>> {
        (id == "vcar-test").then(|| Box::new(VcarTest) as Box<dyn SeedKey>)
    }

    pub fn first(id: &str) -> Option<Box<dyn SeedKey>> {
        (id == "shared" || id == "only-first").then(|| Box::new(Tagged("first", 0x10)) as _)
    }

    pub fn second(id: &str) -> Option<Box<dyn SeedKey>> {
        (id == "shared" || id == "only-second").then(|| Box::new(Tagged("second", 0x20)) as _)
    }
}

#[cfg(test)]
mod tests {
    use super::testkeys::{first, second};
    use super::*;

    #[test]
    fn resolvers_are_asked_in_order_and_none_is_honest() {
        let keys = SeedKeys::new(vec![first, second]);
        assert_eq!(keys.get("shared").unwrap().id(), "first", "first wins");
        assert_eq!(keys.get(" only-second ").unwrap().id(), "second");
        assert!(keys.get("unknown").is_none());
        assert!(keys.get("  ").is_none(), "blank ids never resolve");
        let e = keys.require("unknown").err().unwrap();
        assert!(e.contains("'unknown'") && e.contains("plugin"), "{e}");

        let public = SeedKeys::default();
        assert!(public.is_empty());
        assert!(public.get("shared").is_none(), "the public app has none");
    }

    #[test]
    fn compute_reports_the_handler() {
        let k = first("shared").unwrap();
        assert_eq!(
            compute(k.as_ref(), &[0x00, 0x01], 0x01).unwrap(),
            [0x11, 0x10]
        );
    }
}
