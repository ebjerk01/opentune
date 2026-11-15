//! Checksum engine: verify and correct the integrity fields a flashed image
//! must have for the ECU to accept and run it.
//!
//! Each ECU family is a [`ChecksumAlgo`]. A generic CRC32-tail example is
//! included; real ECU algorithms (EDC16, EDC17, PCR2.1) live in their own
//! modules and must be validated against known-good reference files.
use opentune_core::{MemoryRegion, Result};

pub mod edc16;
pub mod generic;

/// Status of one checksum-protected block.
#[derive(Debug, Clone)]
pub struct BlockStatus {
    pub name: String,
    pub region: MemoryRegion,
    pub stored: u64,
    pub computed: u64,
    pub ok: bool,
}

/// A correction that was (or would be) applied.
#[derive(Debug, Clone)]
pub struct Fix {
    pub name: String,
    pub offset: u32,
    pub old: u64,
    pub new: u64,
}

/// A checksum scheme for one ECU family.
pub trait ChecksumAlgo {
    fn id(&self) -> &str;
    /// Report each protected block and whether its stored checksum matches.
    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>>;
    /// Recompute and write correct checksums in place; return what changed.
    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>>;
}

/// Look up a checksum algorithm by id (as named in an ECU definition).
pub fn get(id: &str) -> Option<Box<dyn ChecksumAlgo>> {
    match id {
        "crc32-tail" => Some(Box::new(generic::Crc32Tail)),
        "edc16" => Some(Box::new(edc16::Edc16)),
        _ => None,
    }
}

/// CRC-32 (IEEE 802.3, reflected) — shared helper for algorithms and tests.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crc32_known_vector() {
        // CRC32 of "123456789" == 0xCBF43926
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
