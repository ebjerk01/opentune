//! [`opentune_core::protocol::SeedKey`] implementation backed by the SA2 VM.
use opentune_core::protocol::SeedKey;
use opentune_core::{Error, Result};

use crate::error::Sa2Result;
use crate::program::parse_hex;
use crate::vm;

/// A seed→key transform that runs a caller-supplied SA2 script.
///
/// The script is *not* bundled with OpenTune: it comes from the user's own
/// flash container (ODX `SECURITY` block or SGO header) or is typed in by the
/// user. Register it under the id returned by [`SeedKey::id`] (`"sa2"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sa2SeedKey {
    /// The user-supplied SA2 script bytes.
    pub program: Vec<u8>,
}

impl Sa2SeedKey {
    /// Wrap a caller-supplied SA2 script.
    pub fn new(program: Vec<u8>) -> Self {
        Self { program }
    }

    /// Wrap a script given as hex (see [`parse_hex`]).
    pub fn from_hex(hex: &str) -> Sa2Result<Self> {
        Ok(Self::new(parse_hex(hex)?))
    }
}

impl SeedKey for Sa2SeedKey {
    fn id(&self) -> &str {
        "sa2"
    }

    /// Run the SA2 script over the 4-byte `seed`, returning the 4-byte key.
    ///
    /// SA2 scripts are per security level; the caller supplies the script that
    /// belongs to `level` (VAG programming access is level `0x11`). VM errors
    /// surface as [`opentune_core::Error::Protocol`].
    fn compute(&self, seed: &[u8], _level: u8) -> Result<Vec<u8>> {
        vm::execute(seed, &self.program).map_err(|e| Error::Protocol(format!("SA2 seed/key: {e}")))
    }
}
