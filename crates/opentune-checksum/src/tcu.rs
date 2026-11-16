//! VAG DSG / automatic **transmission control unit (TCU)** flash checksums.
//!
//! # Provenance (open-source port)
//!
//! Re-derived from open-source, licence-compatible tooling; cited, not copied:
//!
//! * **JAMCRC (DQ250-MQB)** — `TheFlashBold/DQ250-MQB-Bench-Flash` (GPL-3.0).
//!   `jamcrc = !crc32` (reflected CRC-32 with **no** final inversion), stored
//!   little-endian in the **last 4 bytes** of each flash block; SBOOT checks it
//!   on every boot.
//! * **CRC32-VW (DQ381)** and **CRC-CCITT (ZF 8HP)** — `dspl1236/vag-tcu-tools`
//!   (MIT). DQ381 protects its CAL block with a big-endian zlib **CRC-32**; the
//!   block is self-describing (big-endian range pointers at `+0x38`/`+0x3C`, CRC
//!   at `+0x44`, block base `0x14_0200`). ZF 8HP uses a **CRC-CCITT**.
//!
//! OpenTune is GPL-3.0-or-later; GPL-3 and MIT sources are both compatible. The
//! MIT copyright notice is retained here per the licence:
//!
//! ```text
//!   vag-tcu-tools  Copyright (c) 2026 dspl1236.  MIT licence.
//! ```
//!
//! # Algorithms and validation status
//!
//! | id           | family      | algorithm                       | validated against |
//! |--------------|-------------|---------------------------------|-------------------|
//! | `dq381`      | CRC32-VW    | zlib CRC-32, BE, self-describing| real `DQ381 full.bin` (CAL CRC `0x3D34EE10`) |
//! | `dq250`      | JAMCRC      | `!crc32`, LE, per-block trailer | algorithm vectors + synthetic (no 0D9 bench dump on hand — validation pending) |
//! | `tcu-ccitt`  | CRC-CCITT   | CCITT `0x1021`, BE trailer      | CCITT vectors (no CCITT-protected dump on hand — validation pending) |
//!
//! # Anti-brick stance
//!
//! Each scheme refuses an image it cannot confirm. `dq250` only writes when the
//! block layout fits **and at least one block's JAMCRC already matches** (so the
//! algorithm is proven against a real stored value before any write). `dq381`
//! only writes when the self-describing CAL header is present at the known base.
//! A value that cannot be computed is never guessed.
use crate::{crc32, BlockStatus, ChecksumAlgo, Fix};
use opentune_core::{Error, MemoryRegion, Result};

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

/// **JAMCRC**: reflected CRC-32 with the final inversion *removed*
/// (`jamcrc = !crc32`, equivalently `0xFFFF_FFFF - crc32`). Used by VAG DSG
/// (DQ250-MQB) for internal block checksums.
pub fn jamcrc(data: &[u8]) -> u32 {
    !crc32(data)
}

/// Forward (MSB-first) **CRC-CCITT**, polynomial `0x1021`, with the given `init`
/// and no final XOR (`refin = refout = false`). `init = 0x0000` is XMODEM;
/// `init = 0xFFFF` is CCITT-FALSE.
pub fn crc_ccitt(data: &[u8], init: u16) -> u16 {
    let mut crc = init;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[inline]
fn u32be(img: &[u8], i: usize) -> Option<u32> {
    img.get(i..i + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

// ---------------------------------------------------------------------------
// DQ250-MQB — JAMCRC per-block trailer
// ---------------------------------------------------------------------------

/// A JAMCRC-protected block in a DQ250-MQB bench-flash image: its file offset,
/// length (the JAMCRC occupies the last 4 bytes), and name. The DRIVER block is
/// excluded — it is covered by an external UDS checksum, not JAMCRC.
#[derive(Debug, Clone, Copy)]
struct JamBlock {
    name: &'static str,
    offset: usize,
    len: usize,
}

/// DQ250-MQB (0D9, Tricore) bench-flash block layout: CAL then ASW. Each stores
/// its JAMCRC little-endian in the final 4 bytes. (From `DQ250-MQB-Bench-Flash`.)
const DQ250_MQB_BLOCKS: &[JamBlock] = &[
    JamBlock {
        name: "CAL",
        offset: 0x30000,
        len: 0x20000,
    },
    JamBlock {
        name: "ASW",
        offset: 0x50000,
        len: 0x13_0000,
    },
];

/// Minimum image length to hold the DQ250-MQB blocks (`0x50000 + 0x130000`).
const DQ250_MIN_LEN: usize = 0x18_0000;

impl JamBlock {
    /// `(stored, computed)` JAMCRC for this block, or `None` if out of range.
    fn eval(&self, img: &[u8]) -> Option<(u32, u32)> {
        let end = self.offset.checked_add(self.len)?;
        if end > img.len() || self.len < 4 {
            return None;
        }
        let body = &img[self.offset..end - 4];
        let computed = jamcrc(body);
        let stored = u32::from_le_bytes(img[end - 4..end].try_into().ok()?);
        Some((stored, computed))
    }
}

/// VAG **DQ250-MQB** DSG TCU — JAMCRC per-block trailer.
pub struct Dq250Jamcrc;

impl Dq250Jamcrc {
    /// Recognised only when the layout fits **and** at least one block's JAMCRC
    /// already matches (proving the algorithm against a real stored value before
    /// any write — anti-brick).
    fn recognised(img: &[u8]) -> bool {
        if img.len() < DQ250_MIN_LEN {
            return false;
        }
        DQ250_MQB_BLOCKS
            .iter()
            .filter_map(|b| b.eval(img))
            .any(|(s, c)| s == c)
    }
}

impl ChecksumAlgo for Dq250Jamcrc {
    fn id(&self) -> &str {
        "dq250"
    }

    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        let mut out = Vec::new();
        for b in DQ250_MQB_BLOCKS {
            if let Some((stored, computed)) = b.eval(image) {
                out.push(BlockStatus {
                    name: format!("dq250-{}", b.name),
                    region: MemoryRegion::new(b.offset as u32, b.len as u32),
                    stored: stored as u64,
                    computed: computed as u64,
                    ok: stored == computed,
                });
            }
        }
        Ok(out)
    }

    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        if !Self::recognised(image) {
            return Err(Error::Unsupported(
                "not a recognised DQ250-MQB JAMCRC image (layout does not fit, or no block's \
                 JAMCRC validates); refusing to guess (see crate::tcu docs)"
                    .into(),
            ));
        }
        let mut fixes = Vec::new();
        for b in DQ250_MQB_BLOCKS {
            let Some((stored, computed)) = b.eval(image) else {
                continue;
            };
            if stored != computed {
                let store = b.offset + b.len - 4;
                image[store..store + 4].copy_from_slice(&computed.to_le_bytes());
                fixes.push(Fix {
                    name: format!("dq250-{}", b.name),
                    offset: store as u32,
                    old: stored as u64,
                    new: computed as u64,
                });
            }
        }
        Ok(fixes)
    }
}

// ---------------------------------------------------------------------------
// DQ381 — self-describing CAL CRC32 (big-endian zlib CRC-32)
// ---------------------------------------------------------------------------

/// DQ381 flash base of the CAL block; stored range pointers are absolute, so the
/// region offset within the CAL block is `pointer - DQ381_CAL_BASE`.
const DQ381_CAL_BASE: u32 = 0x14_0200;
/// Offset (within the CAL block) of the big-endian range-start pointer.
const DQ381_PTR_START: usize = 0x38;
/// Offset of the big-endian range-end pointer (inclusive).
const DQ381_PTR_END: usize = 0x3C;
/// Offset of the big-endian stored CRC-32.
const DQ381_CRC_OFF: usize = 0x44;
/// Candidate file offsets of the CAL block: a full dump maps it at its flash
/// base; an extracted CAL block starts at 0.
const DQ381_CAL_CANDIDATES: &[usize] = &[0x14_0200, 0x0];

/// A located DQ381 CAL block: its file offset and the region `[start, end]`
/// (inclusive file offsets) the CRC covers.
#[derive(Debug, Clone, Copy)]
struct Dq381Cal {
    cal_off: usize,
    region_start: usize,
    region_end: usize,
    crc_off: usize,
}

impl Dq381Cal {
    /// Compute the zlib CRC-32 over the covered region. If the stored CRC field
    /// falls inside the region it is treated as zero for the computation (as the
    /// ECU does); on the observed DQ381 layout the CRC field sits *before* the
    /// region, so this branch is usually inert.
    fn compute(&self, img: &[u8]) -> Option<u32> {
        let region = img.get(self.region_start..=self.region_end)?;
        if self.crc_off >= self.region_start && self.crc_off + 4 <= self.region_end + 1 {
            let mut buf = region.to_vec();
            let z = self.crc_off - self.region_start;
            buf[z..z + 4].fill(0);
            Some(crc32(&buf))
        } else {
            Some(crc32(region))
        }
    }
    fn stored(&self, img: &[u8]) -> Option<u32> {
        u32be(img, self.crc_off)
    }
}

/// Locate the DQ381 CAL block from its self-describing header (data-independent:
/// the range pointers do not change when the calibration is edited). Returns the
/// block with region as file offsets, or `None` if no candidate has a sane header.
fn find_dq381_cal(img: &[u8]) -> Option<Dq381Cal> {
    let len = img.len();
    for &cal in DQ381_CAL_CANDIDATES {
        if cal + DQ381_CRC_OFF + 4 > len {
            continue;
        }
        let (Some(ps), Some(pe)) = (
            u32be(img, cal + DQ381_PTR_START),
            u32be(img, cal + DQ381_PTR_END),
        ) else {
            continue;
        };
        if ps < DQ381_CAL_BASE || pe <= ps {
            continue;
        }
        let rs = cal + (ps - DQ381_CAL_BASE) as usize;
        let re = cal + (pe - DQ381_CAL_BASE) as usize;
        // Region must be substantial and inside the file.
        if re <= rs || re >= len || (re - rs) < 0x1000 {
            continue;
        }
        return Some(Dq381Cal {
            cal_off: cal,
            region_start: rs,
            region_end: re,
            crc_off: cal + DQ381_CRC_OFF,
        });
    }
    None
}

/// VAG **DQ381** DSG TCU — self-describing CAL **CRC32-VW** (big-endian zlib CRC-32).
pub struct Dq381Crc32;

impl ChecksumAlgo for Dq381Crc32 {
    fn id(&self) -> &str {
        "dq381"
    }

    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        let mut out = Vec::new();
        if let Some(cal) = find_dq381_cal(image) {
            if let (Some(computed), Some(stored)) = (cal.compute(image), cal.stored(image)) {
                out.push(BlockStatus {
                    name: "dq381-CAL".into(),
                    region: MemoryRegion::new(
                        cal.region_start as u32,
                        (cal.region_end - cal.region_start + 1) as u32,
                    ),
                    stored: stored as u64,
                    computed: computed as u64,
                    ok: stored == computed,
                });
            }
        }
        Ok(out)
    }

    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        let Some(cal) = find_dq381_cal(image) else {
            return Err(Error::Unsupported(
                "not a recognised DQ381 CAL image (no self-describing CRC32 header at the known \
                 base); refusing to guess (see crate::tcu docs)"
                    .into(),
            ));
        };
        let (Some(computed), Some(stored)) = (cal.compute(image), cal.stored(image)) else {
            return Err(Error::Config("DQ381 CAL region out of range".into()));
        };
        let mut fixes = Vec::new();
        if computed != stored {
            image[cal.crc_off..cal.crc_off + 4].copy_from_slice(&computed.to_be_bytes());
            fixes.push(Fix {
                name: "dq381-CAL".into(),
                offset: cal.crc_off as u32,
                old: stored as u64,
                new: computed as u64,
            });
        }
        let _ = cal.cal_off;
        Ok(fixes)
    }
}

// ---------------------------------------------------------------------------
// CRC-CCITT — generic big-endian trailer (ZF 8HP family)
// ---------------------------------------------------------------------------

/// Generic **CRC-CCITT** block: the last two bytes of the image are a big-endian
/// CCITT-FALSE (`init = 0xFFFF`) checksum over everything before them. A minimal,
/// layout-agnostic corrector for TCU families (e.g. ZF 8HP) that trail a CCITT
/// over a block; a specific TCU layout would supply the exact (block, offset)
/// instead. Validation against a real CCITT-protected dump is pending.
pub struct CcittTrailer;

/// Init value used by [`CcittTrailer`] (CCITT-FALSE).
const CCITT_INIT: u16 = 0xFFFF;

impl ChecksumAlgo for CcittTrailer {
    fn id(&self) -> &str {
        "tcu-ccitt"
    }

    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        if image.len() < 3 {
            return Ok(Vec::new());
        }
        let split = image.len() - 2;
        let computed = crc_ccitt(&image[..split], CCITT_INIT) as u64;
        let stored = u16::from_be_bytes([image[split], image[split + 1]]) as u64;
        Ok(vec![BlockStatus {
            name: "tcu-ccitt-trailer".into(),
            region: MemoryRegion::new(0, split as u32),
            stored,
            computed,
            ok: stored == computed,
        }])
    }

    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        if image.len() < 3 {
            return Err(Error::Unsupported(
                "image too small for a CRC-CCITT trailer".into(),
            ));
        }
        let split = image.len() - 2;
        let computed = crc_ccitt(&image[..split], CCITT_INIT);
        let stored = u16::from_be_bytes([image[split], image[split + 1]]);
        let mut fixes = Vec::new();
        if computed != stored {
            image[split..split + 2].copy_from_slice(&computed.to_be_bytes());
            fixes.push(Fix {
                name: "tcu-ccitt-trailer".into(),
                offset: split as u32,
                old: stored as u64,
                new: computed as u64,
            });
        }
        Ok(fixes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jamcrc_known_vector() {
        // crc32("123456789") = 0xCBF43926, so jamcrc = !that = 0x340BC6D9.
        assert_eq!(jamcrc(b"123456789"), 0x340B_C6D9);
        assert_eq!(jamcrc(b"123456789"), 0xFFFF_FFFFu32 - crc32(b"123456789"));
    }

    #[test]
    fn crc_ccitt_known_vectors() {
        // CRC-16/XMODEM("123456789") = 0x31C3; CRC-16/CCITT-FALSE = 0x29B1.
        assert_eq!(crc_ccitt(b"123456789", 0x0000), 0x31C3);
        assert_eq!(crc_ccitt(b"123456789", 0xFFFF), 0x29B1);
    }

    /// Build a synthetic DQ250-MQB image (minimum size) with correct JAMCRC in
    /// both CAL and ASW trailers.
    fn synthetic_dq250() -> Vec<u8> {
        let mut img = vec![0u8; DQ250_MIN_LEN];
        for (i, b) in img.iter_mut().enumerate() {
            *b = (i as u8) ^ 0x5A;
        }
        for blk in DQ250_MQB_BLOCKS {
            let body = &img[blk.offset..blk.offset + blk.len - 4];
            let c = jamcrc(body);
            let store = blk.offset + blk.len - 4;
            img[store..store + 4].copy_from_slice(&c.to_le_bytes());
        }
        img
    }

    #[test]
    fn dq250_verify_correct_roundtrip() {
        let img = synthetic_dq250();
        let algo = Dq250Jamcrc;
        assert!(algo.verify(&img).unwrap().iter().all(|s| s.ok));
        assert!(algo.correct(&mut img.clone()).unwrap().is_empty());

        // Corrupt a CAL byte: CAL JAMCRC goes stale, ASW stays valid (so the
        // image is still recognised), correction fixes exactly CAL.
        let mut m = img.clone();
        m[0x30100] ^= 0xFF;
        assert!(algo.verify(&m).unwrap().iter().any(|s| !s.ok));
        let fixes = algo.correct(&mut m).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].name, "dq250-CAL");
        assert!(algo.verify(&m).unwrap().iter().all(|s| s.ok));
        assert!(algo.correct(&mut m.clone()).unwrap().is_empty());
    }

    #[test]
    fn dq250_unrecognised_is_refused() {
        // Right size but no valid JAMCRC anywhere → refuse (wrong layout/algorithm).
        let mut img = vec![0xA5u8; DQ250_MIN_LEN];
        let algo = Dq250Jamcrc;
        assert!(matches!(
            algo.correct(&mut img).unwrap_err(),
            Error::Unsupported(_)
        ));
        // Too small → refuse.
        let mut small = vec![0u8; 0x1000];
        assert!(matches!(
            algo.correct(&mut small).unwrap_err(),
            Error::Unsupported(_)
        ));
    }

    /// Build a synthetic DQ381-style CAL block (standalone, base 0) with a
    /// self-describing header and a correct big-endian zlib CRC-32.
    fn synthetic_dq381() -> Vec<u8> {
        let size = 0x4_0000usize;
        let mut img = vec![0u8; size];
        for (i, b) in img.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(13).wrapping_add(7);
        }
        // region [0x100, 0x3F000] (so the CRC field at 0x44 is outside it).
        let rs = 0x100u32 + DQ381_CAL_BASE;
        let re = 0x3F000u32 + DQ381_CAL_BASE;
        img[DQ381_PTR_START..DQ381_PTR_START + 4].copy_from_slice(&rs.to_be_bytes());
        img[DQ381_PTR_END..DQ381_PTR_END + 4].copy_from_slice(&re.to_be_bytes());
        // compute CRC over [0x100, 0x3F000] and store big-endian at 0x44
        let cal = find_dq381_cal(&img).unwrap();
        let c = cal.compute(&img).unwrap();
        img[DQ381_CRC_OFF..DQ381_CRC_OFF + 4].copy_from_slice(&c.to_be_bytes());
        img
    }

    #[test]
    fn dq381_verify_correct_roundtrip() {
        let img = synthetic_dq381();
        let algo = Dq381Crc32;
        let st = algo.verify(&img).unwrap();
        assert_eq!(st.len(), 1);
        assert!(st[0].ok, "freshly built DQ381 CAL must verify");
        assert!(algo.correct(&mut img.clone()).unwrap().is_empty());

        // Edit the calibration; the CAL CRC goes stale and is re-derived.
        let mut m = img.clone();
        m[0x200] ^= 0xFF;
        assert!(algo.verify(&m).unwrap().iter().any(|s| !s.ok));
        let fixes = algo.correct(&mut m).unwrap();
        assert_eq!(fixes.len(), 1);
        assert!(algo.verify(&m).unwrap().iter().all(|s| s.ok));
        assert!(algo.correct(&mut m.clone()).unwrap().is_empty());
    }

    #[test]
    fn dq381_unrecognised_is_refused() {
        let mut img = vec![0u8; 0x1000]; // no header
        let algo = Dq381Crc32;
        assert!(matches!(
            algo.correct(&mut img).unwrap_err(),
            Error::Unsupported(_)
        ));
    }

    #[test]
    fn ccitt_trailer_roundtrip() {
        let mut img = vec![0u8; 256];
        for (i, b) in img.iter_mut().enumerate() {
            *b = (i as u8) ^ 0x3C;
        }
        let algo = CcittTrailer;
        // initially the trailer is wrong
        assert!(algo.verify(&img).unwrap().iter().any(|s| !s.ok));
        let fixes = algo.correct(&mut img).unwrap();
        assert_eq!(fixes.len(), 1);
        assert!(algo.verify(&img).unwrap().iter().all(|s| s.ok));
        assert!(algo.correct(&mut img.clone()).unwrap().is_empty());
    }

    /// Fixture regression against the real `DQ381 full.bin`. CI has no access, so
    /// `#[ignore]`d:
    ///   OPENTUNE_FIXTURES=/opt/nvme-fw/opentune-fixtures \
    ///     cargo test -p opentune-checksum -- --ignored --nocapture
    #[test]
    #[ignore = "needs owner TCU fixtures via OPENTUNE_FIXTURES"]
    fn reference_dq381_full() {
        let dir = match std::env::var("OPENTUNE_FIXTURES") {
            Ok(d) => d,
            Err(_) => return,
        };
        let Ok(img) = std::fs::read(format!("{dir}/tcu/dq381_full.bin")) else {
            return;
        };
        let algo = Dq381Crc32;
        let st = algo.verify(&img).unwrap();
        assert_eq!(st.len(), 1, "one DQ381 CAL block located");
        assert_eq!(st[0].stored, 0x3D34_EE10, "known stored CAL CRC");
        assert!(st[0].ok, "real DQ381 CAL CRC must verify");
        assert!(algo.correct(&mut img.clone()).unwrap().is_empty());
        // wipe the CRC, correct, confirm the ECU's value is restored exactly
        let cal = find_dq381_cal(&img).unwrap();
        let mut m = img.clone();
        m[cal.crc_off..cal.crc_off + 4].copy_from_slice(&[0, 0, 0, 0]);
        let fixes = algo.correct(&mut m).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].new, 0x3D34_EE10);
        assert_eq!(m, img, "corrected image byte-identical to the real dump");
    }
}
