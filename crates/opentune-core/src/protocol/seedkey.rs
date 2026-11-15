//! Seed → key transform for ECU security access (service 0x27).
//!
//! No OEM algorithms ship in this crate. A driver or plugin provides an
//! implementation keyed by the id named in an ECU definition's `seedkey` field.
use crate::Result;

pub trait SeedKey {
    /// Identifier matching an ECU definition's `seedkey` field.
    fn id(&self) -> &str;
    /// Compute the key for a seed at the given access level.
    fn compute(&self, seed: &[u8], level: u8) -> Result<Vec<u8>>;
}
