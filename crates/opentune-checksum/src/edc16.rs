//! Bosch EDC16 (Infineon C16x / ST10, little-endian) flash checksum engine.
//!
//! # What is reverse-engineered here
//!
//! The reference pair (`yaris_90_stock.ori` / `yaris_90_tuned.mod`, two valid
//! 512 KiB EDC16 images of the same ECU, one stock and one professionally
//! tuned) was byte-diffed. Apart from the calibration/map edits (which cluster
//! in `0x72899..=0x7A673`), **exactly one** other 32-bit field differs between
//! the two valid files:
//!
//! | offset     | stock        | tuned        |
//! |------------|--------------|--------------|
//! | `0x7BD7C`  | `0xDBCCED4D` | `0xDC88782E` | little-endian, 4 bytes
//!
//! Because a *valid* tuned file must carry a recomputed checksum, this isolated
//! field is the calibration checksum the ECU verifies. Its exact location,
//! width (32-bit) and byte order (little-endian) are therefore VERIFIED against
//! both reference files (see `docs/checksums-edc16.md`).
//!
//! # What is NOT yet reverse-engineered
//!
//! The *algorithm* that produces that 32-bit value could not be determined from
//! a single stock/tuned pair. It was exhaustively shown **not** to be:
//!
//! * any additive sum — byte, 16-bit or 32-bit, either endianness, any
//!   alignment phase, over any contiguous sub-range (the required value delta
//!   `0xBB8AE1` exceeds the maximum possible additive delta for byte sums
//!   (`0x9F3`) and 16-bit-word sums (`0x98FDB`), and no 32-bit-word sub-range
//!   reproduces both stored values);
//! * a standard CRC-32 — CRC-32, CRC-32C, Koopman, CRC-32/Q, XFER, MPEG-2,
//!   POSIX and CRC-32/K2 were all excluded (both reflections, every end offset,
//!   plus 16-/32-bit byte-swapped feed orders) via the CRC linearity identity
//!   `crc(a) ^ crc(b) == crc_linear(a ^ b)`.
//!
//! It is therefore a 32-bit avalanche checksum, consistent with a CRC-32 using
//! a non-standard/proprietary Bosch polynomial, which needs more sample pairs
//! (or the ECU boot code) to pin down uniquely. It is represented below with
//! [`Algo::Unknown`] so that [`verify`](Edc16::verify) surfaces the field while
//! [`correct`](Edc16::correct) refuses to write a value it cannot compute.
//!
//! # The engine
//!
//! Everything else is a genuine, data-driven checksum engine: a small table of
//! [`Block`]s (`{name, sum_range, store_offset, store_len, store_le, algo}`) and
//! a set of additive / CRC [`Algo`]s. Many EDC16 variants use a plain 16- or
//! 32-bit sum, so these are directly reusable; new variants are added by
//! extending the block table, not the code. The sum/CRC paths are fully covered
//! by synthetic unit tests.
use crate::{crc32, BlockStatus, ChecksumAlgo, Fix};
use opentune_core::{Error, MemoryRegion, Result};

/// How a block's checksum value is computed over its `sum_range`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algo {
    /// Additive sum of every byte, truncated to the stored width.
    Sum8,
    /// Additive sum of 16-bit little-endian words.
    Sum16Le,
    /// Additive sum of 16-bit big-endian words.
    Sum16Be,
    /// Additive sum of 32-bit little-endian words.
    Sum32Le,
    /// Additive sum of 32-bit big-endian words.
    Sum32Be,
    /// CRC-32 (IEEE 802.3, reflected) — see [`crate::crc32`].
    Crc32,
    /// Checksum field whose algorithm is identified in position but NOT yet
    /// reverse-engineered. [`verify`](Edc16::verify) reports it as not-OK;
    /// [`correct`](Edc16::correct) returns an error rather than guess.
    Unknown,
}

impl Algo {
    /// Step (in bytes) between accumulated words for the additive algorithms.
    fn step(self) -> usize {
        match self {
            Algo::Sum8 => 1,
            Algo::Sum16Le | Algo::Sum16Be => 2,
            Algo::Sum32Le | Algo::Sum32Be => 4,
            Algo::Crc32 | Algo::Unknown => 1,
        }
    }
}

/// One checksum-protected block: a summed range plus where its value is stored.
#[derive(Debug, Clone)]
pub struct Block {
    /// Human-readable name for diagnostics.
    pub name: &'static str,
    /// `[start, end)` byte range that is summed (half-open).
    pub sum_range: (u32, u32),
    /// Offset at which the checksum value is stored.
    pub store_offset: u32,
    /// Width of the stored value in bytes (2 or 4).
    pub store_len: u8,
    /// `true` if the stored value is little-endian, `false` for big-endian.
    pub store_le: bool,
    /// Algorithm used to compute the value.
    pub algo: Algo,
}

/// Default EDC16 block map.
///
/// Currently holds the one VERIFIED-in-position calibration checksum field of
/// the reference ECU (`1025 C9/ESB/G43`, firmware "TSW V0.70 120701"), whose
/// algorithm is still [`Algo::Unknown`]. The summed range is the natural
/// calibration block `[0x70000, 0x7BD7C)` (data area start up to the checksum
/// word); it is recorded as the best-estimate candidate and is *not* relied on
/// while `algo` is `Unknown`.
pub const DEFAULT_BLOCKS: &[Block] = &[Block {
    name: "cal-checksum",
    sum_range: (0x0007_0000, 0x0007_BD7C),
    store_offset: 0x0007_BD7C,
    store_len: 4,
    store_le: true,
    algo: Algo::Unknown,
}];

/// Read the stored checksum word of `block` from `image`.
fn read_stored(image: &[u8], block: &Block) -> Result<u64> {
    let off = block.store_offset as usize;
    let len = block.store_len as usize;
    let end = off
        .checked_add(len)
        .ok_or_else(|| Error::Config("store offset overflow".into()))?;
    if end > image.len() {
        return Err(Error::Config(format!(
            "block '{}' store field 0x{off:X}..0x{end:X} out of range",
            block.name
        )));
    }
    let bytes = &image[off..end];
    let mut v: u64 = 0;
    if block.store_le {
        for (i, &b) in bytes.iter().enumerate() {
            v |= (b as u64) << (8 * i);
        }
    } else {
        for &b in bytes {
            v = (v << 8) | b as u64;
        }
    }
    Ok(v)
}

/// Write `value` into the stored checksum word of `block` in `image`.
fn write_stored(image: &mut [u8], block: &Block, value: u64) -> Result<()> {
    let off = block.store_offset as usize;
    let len = block.store_len as usize;
    let end = off
        .checked_add(len)
        .ok_or_else(|| Error::Config("store offset overflow".into()))?;
    if end > image.len() {
        return Err(Error::Config(format!(
            "block '{}' store field out of range",
            block.name
        )));
    }
    if block.store_le {
        for i in 0..len {
            image[off + i] = (value >> (8 * i)) as u8;
        }
    } else {
        for i in 0..len {
            image[off + i] = (value >> (8 * (len - 1 - i))) as u8;
        }
    }
    Ok(())
}

/// Mask for the stored width.
fn width_mask(store_len: u8) -> u64 {
    match store_len {
        0 => 0,
        n if n >= 8 => u64::MAX,
        n => (1u64 << (8 * n as u32)) - 1,
    }
}

/// Compute the checksum of `block` over its `sum_range`.
///
/// Returns `None` for [`Algo::Unknown`] (no known algorithm). Fails only if the
/// range is out of bounds.
pub fn compute_block(image: &[u8], block: &Block) -> Result<Option<u64>> {
    let (start, end) = (block.sum_range.0 as usize, block.sum_range.1 as usize);
    if start > end || end > image.len() {
        return Err(Error::Config(format!(
            "block '{}' sum range 0x{start:X}..0x{end:X} out of range",
            block.name
        )));
    }
    let data = &image[start..end];
    let mask = width_mask(block.store_len);

    let value = match block.algo {
        Algo::Unknown => return Ok(None),
        Algo::Crc32 => crc32(data) as u64 & mask,
        algo => {
            let step = algo.step();
            let mut acc: u64 = 0;
            let mut i = 0;
            while i + step <= data.len() {
                let word: u64 = match algo {
                    Algo::Sum8 => data[i] as u64,
                    Algo::Sum16Le => u16::from_le_bytes([data[i], data[i + 1]]) as u64,
                    Algo::Sum16Be => u16::from_be_bytes([data[i], data[i + 1]]) as u64,
                    Algo::Sum32Le => {
                        u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as u64
                    }
                    Algo::Sum32Be => {
                        u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as u64
                    }
                    Algo::Crc32 | Algo::Unknown => unreachable!(),
                };
                acc = acc.wrapping_add(word);
                i += step;
            }
            acc & mask
        }
    };
    Ok(Some(value))
}

/// Verify every block in `blocks` against `image`.
///
/// [`Algo::Unknown`] blocks are reported with `computed == 0` and `ok == false`
/// so the field is surfaced without claiming a (non-existent) match.
pub fn verify_blocks(image: &[u8], blocks: &[Block]) -> Result<Vec<BlockStatus>> {
    let mut out = Vec::with_capacity(blocks.len());
    for block in blocks {
        let stored = read_stored(image, block)?;
        let (computed, ok) = match compute_block(image, block)? {
            Some(c) => (c, c == stored),
            None => (0, false), // Unknown algorithm: cannot confirm.
        };
        let (addr, len) = block.sum_range;
        out.push(BlockStatus {
            name: block.name.to_string(),
            region: MemoryRegion::new(addr, len.wrapping_sub(addr)),
            stored,
            computed,
            ok,
        });
    }
    Ok(out)
}

/// Recompute and write every correctable block in `blocks`.
///
/// Returns an error if any block uses [`Algo::Unknown`]: the algorithm is not
/// known, so writing a value would corrupt the image. Only maps made entirely
/// of known algorithms can be corrected.
pub fn correct_blocks(image: &mut [u8], blocks: &[Block]) -> Result<Vec<Fix>> {
    // Refuse up-front if anything is uncorrectable, so we never write a partial
    // (and therefore invalid) set of checksums.
    if let Some(b) = blocks.iter().find(|b| b.algo == Algo::Unknown) {
        return Err(Error::Unsupported(format!(
            "EDC16 block '{}' algorithm not reverse-engineered; cannot correct \
             (see docs/checksums-edc16.md)",
            b.name
        )));
    }
    let mut fixes = Vec::new();
    for block in blocks {
        let computed = compute_block(image, block)?.expect("known algo yields a value");
        let old = read_stored(image, block)?;
        if old != computed {
            write_stored(image, block, computed)?;
            fixes.push(Fix {
                name: block.name.to_string(),
                offset: block.store_offset,
                old,
                new: computed,
            });
        }
    }
    Ok(fixes)
}

/// Bosch EDC16 checksum scheme.
pub struct Edc16;

impl Edc16 {
    /// The active block map ([`DEFAULT_BLOCKS`]).
    pub fn blocks(&self) -> &'static [Block] {
        DEFAULT_BLOCKS
    }
}

impl ChecksumAlgo for Edc16 {
    fn id(&self) -> &str {
        "edc16"
    }

    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        verify_blocks(image, self.blocks())
    }

    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        correct_blocks(image, self.blocks())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a buffer with `data` plus `tail` zero bytes to hold a checksum.
    fn buf(data: &[u8], tail: usize) -> Vec<u8> {
        let mut v = data.to_vec();
        v.extend(std::iter::repeat_n(0u8, tail));
        v
    }

    fn block(algo: Algo, start: u32, end: u32, store: u32, len: u8, le: bool) -> Block {
        Block {
            name: "test",
            sum_range: (start, end),
            store_offset: store,
            store_len: len,
            store_le: le,
            algo,
        }
    }

    #[test]
    fn sum8_matches_hand_computed() {
        // bytes 1+2+3+4 = 0x0A, stored in 1 byte
        let mut img = buf(&[1, 2, 3, 4], 1);
        let b = block(Algo::Sum8, 0, 4, 4, 1, true);
        assert_eq!(compute_block(&img, &b).unwrap(), Some(0x0A));
        // correct writes it and then verify is OK
        let fixes = correct_blocks(&mut img, std::slice::from_ref(&b)).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(img[4], 0x0A);
        assert!(verify_blocks(&img, std::slice::from_ref(&b)).unwrap()[0].ok);
    }

    #[test]
    fn sum8_truncates_to_width() {
        // 0xF0 + 0x20 = 0x110 -> low byte 0x10
        let img = buf(&[0xF0, 0x20], 1);
        let b = block(Algo::Sum8, 0, 2, 2, 1, true);
        assert_eq!(compute_block(&img, &b).unwrap(), Some(0x10));
    }

    #[test]
    fn sum16_le_and_be() {
        // words: 0x0201, 0x0403  (LE)  sum = 0x0604
        let img = buf(&[0x01, 0x02, 0x03, 0x04], 2);
        let le = block(Algo::Sum16Le, 0, 4, 4, 2, true);
        assert_eq!(compute_block(&img, &le).unwrap(), Some(0x0604));
        // BE words: 0x0102, 0x0304 -> 0x0406
        let be = block(Algo::Sum16Be, 0, 4, 4, 2, true);
        assert_eq!(compute_block(&img, &be).unwrap(), Some(0x0406));
    }

    #[test]
    fn sum32_le_wraps_mod_2_32() {
        // 0xFFFFFFFF + 0x00000002 = 0x1_00000001 -> 0x00000001
        let img = buf(&[0xFF, 0xFF, 0xFF, 0xFF, 0x02, 0x00, 0x00, 0x00], 4);
        let b = block(Algo::Sum32Le, 0, 8, 8, 4, true);
        assert_eq!(compute_block(&img, &b).unwrap(), Some(0x0000_0001));
    }

    #[test]
    fn crc32_roundtrip_and_store_endianness() {
        let data = b"123456789";
        // CRC32("123456789") == 0xCBF43926 (well-known vector)
        let mut img = buf(data, 4);
        let b = block(
            Algo::Crc32,
            0,
            data.len() as u32,
            data.len() as u32,
            4,
            true,
        );
        assert_eq!(compute_block(&img, &b).unwrap(), Some(0xCBF4_3926));
        correct_blocks(&mut img, std::slice::from_ref(&b)).unwrap();
        // stored little-endian
        assert_eq!(&img[9..13], &[0x26, 0x39, 0xF4, 0xCB]);
        assert!(verify_blocks(&img, std::slice::from_ref(&b)).unwrap()[0].ok);

        // big-endian store option
        let mut img_be = buf(data, 4);
        let bb = block(
            Algo::Crc32,
            0,
            data.len() as u32,
            data.len() as u32,
            4,
            false,
        );
        correct_blocks(&mut img_be, std::slice::from_ref(&bb)).unwrap();
        assert_eq!(&img_be[9..13], &[0xCB, 0xF4, 0x39, 0x26]);
    }

    #[test]
    fn correct_is_idempotent_and_reports_only_changes() {
        let mut img = buf(&[10, 20, 30, 40], 4);
        let b = block(Algo::Sum32Le, 0, 4, 4, 4, true);
        let first = correct_blocks(&mut img, std::slice::from_ref(&b)).unwrap();
        assert_eq!(first.len(), 1); // one field written
        let second = correct_blocks(&mut img, std::slice::from_ref(&b)).unwrap();
        assert!(second.is_empty()); // already correct -> no fixes
    }

    #[test]
    fn verify_detects_corruption_then_correct_fixes_it() {
        let mut img = buf(&[1, 2, 3, 4, 5, 6, 7, 8], 4);
        let b = block(Algo::Sum16Le, 0, 8, 8, 2, true);
        correct_blocks(&mut img, std::slice::from_ref(&b)).unwrap();
        assert!(verify_blocks(&img, std::slice::from_ref(&b)).unwrap()[0].ok);
        // corrupt a data byte
        img[2] ^= 0xFF;
        assert!(!verify_blocks(&img, std::slice::from_ref(&b)).unwrap()[0].ok);
        // re-correct and it verifies again
        let fixes = correct_blocks(&mut img, std::slice::from_ref(&b)).unwrap();
        assert_eq!(fixes.len(), 1);
        assert!(verify_blocks(&img, std::slice::from_ref(&b)).unwrap()[0].ok);
    }

    #[test]
    fn unknown_algo_is_surfaced_but_not_corrected() {
        let img = buf(&[0xAA, 0xBB, 0xCC, 0xDD], 0);
        let b = block(Algo::Unknown, 0, 0, 0, 4, true);
        // verify reports it, flagged not-ok, computed 0
        let st = verify_blocks(&img, std::slice::from_ref(&b)).unwrap();
        assert_eq!(st[0].stored, 0xDDCC_BBAA);
        assert_eq!(st[0].computed, 0);
        assert!(!st[0].ok);
        // correct refuses rather than writing a guess
        let mut img2 = img.clone();
        let err = correct_blocks(&mut img2, std::slice::from_ref(&b)).unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)));
        assert_eq!(img2, img); // unchanged
    }

    #[test]
    fn default_edc16_surfaces_field_and_refuses_to_correct() {
        // 512 KiB zeroed image with the known stored constant planted.
        let mut img = vec![0u8; 0x8_0000];
        img[0x7BD7C..0x7BD80].copy_from_slice(&0xDBCC_ED4Du32.to_le_bytes());
        let algo = Edc16;
        let st = algo.verify(&img).unwrap();
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].stored, 0xDBCC_ED4D);
        assert!(!st[0].ok); // Unknown -> cannot confirm
        assert!(algo.correct(&mut img).is_err());
    }

    #[test]
    fn out_of_range_block_errors() {
        let img = buf(&[1, 2, 3], 0);
        let b = block(Algo::Sum8, 0, 100, 0, 1, true);
        assert!(compute_block(&img, &b).is_err());
    }

    // ---- Reference-file test (requires the private fixtures) -------------
    //
    // Enabled only when OPENTUNE_FIXTURES points at a directory holding
    // `yaris_90_stock.ori` and `yaris_90_tuned.mod`. CI has no access to these
    // files, so the test is #[ignore]d by default. Run with:
    //   OPENTUNE_FIXTURES=/path cargo test -p opentune-checksum -- --ignored
    #[test]
    #[ignore = "needs private EDC16 reference fixtures via OPENTUNE_FIXTURES"]
    fn reference_files() {
        let dir = match std::env::var("OPENTUNE_FIXTURES") {
            Ok(d) => d,
            Err(_) => return,
        };
        let stock = std::fs::read(format!("{dir}/yaris_90_stock.ori")).unwrap();
        let tuned = std::fs::read(format!("{dir}/yaris_90_tuned.mod")).unwrap();
        assert_eq!(stock.len(), 0x8_0000);
        assert_eq!(tuned.len(), 0x8_0000);

        // (1) VERIFIED fact: the calibration checksum field location/width/order.
        let s = u32::from_le_bytes(stock[0x7BD7C..0x7BD80].try_into().unwrap());
        let t = u32::from_le_bytes(tuned[0x7BD7C..0x7BD80].try_into().unwrap());
        assert_eq!(s, 0xDBCC_ED4D, "stock calibration checksum word");
        assert_eq!(t, 0xDC88_782E, "tuned calibration checksum word");

        // The default EDC16 map surfaces that field (but cannot confirm it).
        let algo = Edc16;
        let st = algo.verify(&stock).unwrap();
        assert_eq!(st[0].stored, 0xDBCC_ED4D);
        assert!(!st[0].ok, "cal-checksum algo is Unknown -> not confirmed");
        assert!(
            algo.correct(&mut stock.clone()).is_err(),
            "must refuse to correct the unverified block"
        );

        // (2) Validate the generic engine on REAL bytes: define a Sum32Le block
        //     over the calibration data region, storing into a scratch tail, and
        //     prove verify/correct round-trip on the real image.
        let mut img = stock.clone();
        img.extend_from_slice(&[0u8; 4]);
        let store = (img.len() - 4) as u32;
        let b = Block {
            name: "engine-roundtrip-on-real-data",
            sum_range: (0x0007_0000, 0x0007_BD7C),
            store_offset: store,
            store_len: 4,
            store_le: true,
            algo: Algo::Sum32Le,
        };
        // Freshly correct, then it must verify.
        correct_blocks(&mut img, std::slice::from_ref(&b)).unwrap();
        assert!(verify_blocks(&img, std::slice::from_ref(&b)).unwrap()[0].ok);
        // Corrupt a calibration byte -> verify fails -> correct -> verify OK.
        img[0x72899] ^= 0xFF;
        assert!(!verify_blocks(&img, std::slice::from_ref(&b)).unwrap()[0].ok);
        let fixes = correct_blocks(&mut img, std::slice::from_ref(&b)).unwrap();
        assert_eq!(fixes.len(), 1);
        assert!(verify_blocks(&img, std::slice::from_ref(&b)).unwrap()[0].ok);
    }
}
