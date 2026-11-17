//! [`opentune_core::protocol::SeedKey`] implementation backed by the SA2 VM.
use opentune_core::protocol::SeedKey;
use opentune_core::{Error, Result};

use crate::vm;

/// A seed->key transform that runs a caller-supplied SA2 bytecode program.
///
/// The program is *not* bundled with OpenTune: it comes from the vehicle's own
/// A2L/ODX description or from the user. Construct one with [`Sa2SeedKey::new`]
/// and register it under the id returned by [`SeedKey::id`] (`"sa2"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sa2SeedKey {
    /// The user-supplied SA2 bytecode program.
    pub program: Vec<u8>,
}

impl Sa2SeedKey {
    /// Wrap a caller-supplied SA2 bytecode program.
    pub fn new(program: Vec<u8>) -> Self {
        Self { program }
    }
}

impl SeedKey for Sa2SeedKey {
    fn id(&self) -> &str {
        "sa2"
    }

    /// Run the SA2 program over `seed`.
    ///
    /// The SA2 VM is access-level agnostic; `level` identifies which security
    /// level the caller is requesting, and the caller is responsible for having
    /// supplied the program that belongs to that level. VM errors are surfaced
    /// as [`opentune_core::Error::Protocol`].
    fn compute(&self, seed: &[u8], _level: u8) -> Result<Vec<u8>> {
        vm::execute(seed, &self.program).map_err(|e| Error::Protocol(format!("SA2 seed/key: {e}")))
    }
}
