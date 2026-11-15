//! Bosch EDC16 checksum framework.
//!
//! EDC16 flash images carry several checksum-protected blocks. The exact block
//! table and algorithm vary slightly by variant and must be validated against
//! known-good reference files before this is trusted to correct a real image.
//!
//! Status: FRAMEWORK ONLY. `verify`/`correct` return [`Error::Unsupported`]
//! until the block map is validated (tracked in ROADMAP M1). See
//! `docs/checksums-edc16.md`.
use crate::{BlockStatus, ChecksumAlgo, Fix};
use opentune_core::{Error, Result};

pub struct Edc16;

impl ChecksumAlgo for Edc16 {
    fn id(&self) -> &str {
        "edc16"
    }

    fn verify(&self, _image: &[u8]) -> Result<Vec<BlockStatus>> {
        Err(Error::Unsupported(
            "EDC16 checksum not yet validated against reference files (ROADMAP M1)".into(),
        ))
    }

    fn correct(&self, _image: &mut [u8]) -> Result<Vec<Fix>> {
        Err(Error::Unsupported(
            "EDC16 checksum not yet validated against reference files (ROADMAP M1)".into(),
        ))
    }
}
