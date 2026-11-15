//! Bosch EDC15 / EDC15C6 ("CR1", Infineon C167, little-endian) flash checksum.
//!
//! # Status: field located, algorithm NOT yet reverse-engineered (honest)
//!
//! One real EDC15C6 stock/tuned pair was analysed (Mercedes Sprinter 2.2 CDI,
//! software `1037351686`, 1 MiB). Byte-diffing the two valid images isolates a
//! single 4-byte checksum field near the top of the calibration bank, surrounded
//! by `0xC3` fill, that changes between stock and tuned:
//!
//! | fixture                    | field     | stock (LE32)  | tuned (LE32)  |
//! |----------------------------|-----------|---------------|---------------|
//! | `mb_sprinter_edc15c6_351686`| `0xFBFFC` | `0xB778D5F2`  | `0x9F85EEDE`  |
//!
//! The field reads as two 16-bit little-endian words (`{0xD5F2, 0xB778}` stock)
//! and is immediately preceded by the constant halfword `0xC058` at `0xFBFFA`.
//!
//! Unlike the MPC56x EDC16 scheme (see [`crate::edc16`]), this field is **not**
//! reproduced by any simple additive relationship that could be validated from
//! the available data:
//!
//! * no 16-bit or 32-bit word sum (LE or BE) over a contiguous `[start, field)`
//!   range reproduces either stored halfword across both images;
//! * the field is not a `{sum, ~sum}` complement pair (the two halfwords are not
//!   complements);
//! * there is no "sum over the block including the field equals a constant"
//!   invariant (the EDC16 trick) — neither 16- nor 32-bit, and the field delta
//!   does not compensate the changed calibration-data delta.
//!
//! The expected EDC15 scheme from the open references (ME7Sum / general Bosch
//! C16x practice, `docs/BOSCH-CHECKSUM.md`) is a **16-bit additive sum over one
//! or more `{start, end}` blocks, stored with its one's-complement**. The block
//! map was not recoverable from a single pair (no plaintext `{start,end,cs,~cs}`
//! descriptor table was found adjacent to the field), so the exact summed ranges
//! — and whether C6 adds a CRC/obfuscation layer over the plain sum — remain
//! open. Pinning it needs several same-ECU pairs or the C167 checksum routine.
//!
//! Following the project rule (*never write a value we cannot compute*), this
//! module therefore **surfaces** the located field via [`verify`](Edc15C6::verify)
//! and **refuses** to correct it ([`correct`](Edc15C6::correct) returns
//! [`Error::Unsupported`]). It is registered so the family is recognised and the
//! verified facts are regression-tested, not so a guessed value is ever flashed.
use crate::{BlockStatus, ChecksumAlgo, Fix};
use opentune_core::{Error, MemoryRegion, Result};

/// A located-but-unsolved EDC15C6 checksum field proven by diffing a valid pair.
#[derive(Debug, Clone, Copy)]
pub struct KnownField {
    /// Fixture base name (`<name>_ori.bin` / `<name>_mod.bin`).
    pub fixture: &'static str,
    /// Image length in bytes.
    pub image_len: usize,
    /// Byte offset of the 4-byte checksum field.
    pub store_offset: u32,
    /// Stored value in the stock image (little-endian u32).
    pub stored_stock: u32,
    /// Stored value in the tuned image (little-endian u32).
    pub stored_tuned: u32,
}

/// The validated EDC15C6 checksum-field facts (see module docs).
pub const KNOWN_FIELDS: &[KnownField] = &[KnownField {
    fixture: "mb_sprinter_edc15c6_351686",
    image_len: 0x10_0000,
    store_offset: 0x000F_BFFC,
    stored_stock: 0xB778_D5F2,
    stored_tuned: 0x9F85_EEDE,
}];

/// Bosch EDC15C6 ("CR1") checksum scheme (field located, algorithm open).
pub struct Edc15C6;

impl ChecksumAlgo for Edc15C6 {
    fn id(&self) -> &str {
        "edc15c6"
    }

    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        // Surface the located field for the one validated layout. The algorithm
        // is unknown, so it is reported not-OK (we cannot confirm it).
        let mut out = Vec::new();
        for f in KNOWN_FIELDS {
            if image.len() != f.image_len {
                continue;
            }
            let off = f.store_offset as usize;
            let Some(bytes) = image.get(off..off + 4) else {
                continue;
            };
            let stored = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64;
            out.push(BlockStatus {
                name: "cal-checksum".to_string(),
                region: MemoryRegion::new(f.store_offset, 4),
                stored,
                computed: 0,
                ok: false, // algorithm not reverse-engineered: cannot confirm
            });
        }
        Ok(out)
    }

    fn correct(&self, _image: &mut [u8]) -> Result<Vec<Fix>> {
        Err(Error::Unsupported(
            "EDC15C6 checksum algorithm not reverse-engineered; cannot correct \
             (field located only — see crate::edc15 docs)"
                .into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surfaces_field_and_refuses_to_correct() {
        let f = &KNOWN_FIELDS[0];
        let mut img = vec![0xC3u8; f.image_len];
        img[f.store_offset as usize..f.store_offset as usize + 4]
            .copy_from_slice(&f.stored_stock.to_le_bytes());
        let algo = Edc15C6;
        let st = algo.verify(&img).unwrap();
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].stored, f.stored_stock as u64);
        assert!(!st[0].ok);
        assert!(matches!(
            algo.correct(&mut img).unwrap_err(),
            Error::Unsupported(_)
        ));
    }

    #[test]
    fn ignores_images_of_other_length() {
        let algo = Edc15C6;
        assert!(algo.verify(&vec![0u8; 0x8_0000]).unwrap().is_empty());
    }

    /// Fixtures regression: the located field is at the recorded offset with the
    /// recorded stock/tuned values. Requires the private EDC15C6 pair.
    #[test]
    #[ignore = "needs private EDC15 reference fixtures via OPENTUNE_FIXTURES"]
    fn reference_field_located() {
        let dir = match std::env::var("OPENTUNE_FIXTURES") {
            Ok(d) => d,
            Err(_) => return,
        };
        let mut checked = 0;
        for f in KNOWN_FIELDS {
            let sp = format!("{dir}/edc15/{}_ori.bin", f.fixture);
            let tp = format!("{dir}/edc15/{}_mod.bin", f.fixture);
            let (Ok(stock), Ok(tuned)) = (std::fs::read(&sp), std::fs::read(&tp)) else {
                continue;
            };
            assert_eq!(stock.len(), f.image_len, "{} stock len", f.fixture);
            let o = f.store_offset as usize;
            let s = u32::from_le_bytes(stock[o..o + 4].try_into().unwrap());
            let t = u32::from_le_bytes(tuned[o..o + 4].try_into().unwrap());
            assert_eq!(s, f.stored_stock, "{} stock field", f.fixture);
            assert_eq!(t, f.stored_tuned, "{} tuned field", f.fixture);
            // Honest guard: the field is NOT a {sum, ~sum} complement pair.
            let (w0, w1) = ((s & 0xFFFF) as u16, (s >> 16) as u16);
            assert_ne!(w0 ^ w1, 0xFFFF, "{}: not a complement pair", f.fixture);
            checked += 1;
        }
        assert!(checked >= 1, "no EDC15 fixtures under OPENTUNE_FIXTURES");
    }
}
