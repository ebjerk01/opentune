//! Checksum engine: verify and correct the integrity fields a flashed image
//! must have for the ECU to accept and run it.
//!
//! Each ECU family is a [`ChecksumAlgo`]. A generic CRC32-tail example is
//! included; real ECU algorithms (EDC16, EDC17, PCR2.1) live in their own
//! modules and must be validated against known-good reference files.
use opentune_core::{MemoryRegion, Result};

pub mod edc15;
pub mod edc16;
pub mod edc17;
pub mod generic;
pub mod pcr21;

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

    /// Correct using a known-good **reference image** (e.g. the owner's own
    /// untouched stock dump) to bootstrap per-block target values that are not
    /// baked into this library. The default ignores the reference and behaves
    /// exactly like [`correct`](Self::correct) — correct for algorithms whose
    /// targets are self-contained in the image (EDC16, PCR2.1, EDC15). EDC17
    /// overrides it: the VAG per-block constant is firmware-specific, so it is
    /// recovered from the reference rather than guessed.
    fn correct_with_reference(&self, image: &mut [u8], _reference: &[u8]) -> Result<Vec<Fix>> {
        self.correct(image)
    }
}

/// Look up a checksum algorithm by id (as named in an ECU definition).
pub fn get(id: &str) -> Option<Box<dyn ChecksumAlgo>> {
    match id {
        "crc32-tail" => Some(Box::new(generic::Crc32Tail)),
        "edc16" => Some(Box::new(edc16::Edc16)),
        "edc17" => Some(Box::new(edc17::Edc17)),
        "pcr21" => Some(Box::new(pcr21::Pcr21)),
        "edc15c6" | "edc15" => Some(Box::new(edc15::Edc15C6)),
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

/// Forward (MSB-first) CRC-32 with polynomial `0x04C11DB7`, given `init` and no
/// final XOR. `refin = refout = false`.
///
/// This is the "main ROM" CRC family used across Bosch control units
/// (Motronic ME7 CRC blocks, Simos/MED/EDC17 main block). See
/// `docs/BOSCH-CHECKSUM.md` and the VW_Flash reference. Exposed so EDC16/EDC17
/// block tables can reuse a single, tested implementation.
fn crc32_forward(data: &[u8], init: u32) -> u32 {
    let mut crc = init;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Bosch/VAG main CRC-32: forward poly `0x04C11DB7`, **init 0**, no final XOR.
///
/// Documented in VW_Flash as the algorithm the Simos/MED families use over the
/// concatenated checksum areas. Provided for EDC16 variants whose main block
/// uses it; the EDC16 *calibration* checksum reverse-engineered here is a
/// different (still unidentified) 32-bit avalanche field — see `edc16`.
pub fn crc32_bosch(data: &[u8]) -> u32 {
    crc32_forward(data, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crc32_known_vector() {
        // CRC32 of "123456789" == 0xCBF43926
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn crc32_forward_matches_mpeg2_vector() {
        // CRC-32/MPEG-2 is forward poly 0x04C11DB7, init 0xFFFFFFFF, no xorout;
        // its published check value over "123456789" is 0x0376E6E7. This pins
        // the forward engine against a standard vector.
        assert_eq!(crc32_forward(b"123456789", 0xFFFF_FFFF), 0x0376_E6E7);
    }

    #[test]
    fn crc32_bosch_init0_vector() {
        // Same engine with init 0 (the Bosch/VAG main-CRC parameters). Value is
        // locked as a regression against the validated implementation.
        assert_eq!(crc32_bosch(b"123456789"), 0x89A1_897F);
    }
}
