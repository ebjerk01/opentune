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
//!   (MIT). DQ381 protects each of its **BOOT, ASW and CAL** programming blocks
//!   with a big-endian zlib **CRC-32**; every block is self-describing with the
//!   same header (big-endian range pointers at `+0x38`/`+0x3C`, inclusive end,
//!   CRC at `+0x44`) at its flash base (BOOT `0x01_0200`, ASW `0x03_0200`, CAL
//!   `0x14_0200`). ZF 8HP uses a **CRC-CCITT**.
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
//! | `dq381`      | CRC32-VW    | zlib CRC-32, BE, self-describing| real `dq381_full.bin` (BOOT `0xADCC4D12`, ASW `0x828A625B`, CAL `0x3D34EE10`) |
//! | `dq250`      | JAMCRC      | `!crc32`, LE, per-block trailer | algorithm vectors + synthetic (no 0D9 bench dump on hand — validation pending) |
//! | `tcu-ccitt`  | CRC-CCITT   | CCITT `0x1021`, BE trailer      | CCITT vectors (no CCITT-protected dump on hand — validation pending) |
//!
//! # Anti-brick stance
//!
//! Each scheme refuses an image it cannot confirm. `dq250` only writes when the
//! block layout fits **and at least one block's JAMCRC already matches** (so the
//! algorithm is proven against a real stored value before any write). `dq381`
//! only writes a block whose self-describing header is present at a known base
//! and whose covered region validates structurally; each of BOOT/ASW/CAL is
//! handled independently. A value that cannot be computed is never guessed.
use crate::{
    crc32, BlockReport, BlockStatus, ChecksumAlgo, ChecksumKind, Fix, Recognition, Representation,
};
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

    /// A DQ250-MQB image is only *recognised* once at least one block's stored
    /// JAMCRC proves the algorithm against a real value (see [`Self::recognised`]).
    /// Until then the per-block results are merely mathematically consistent, so
    /// they are reported [`Recognition::Consistent`] and never "supported".
    fn verify_report(
        &self,
        image: &[u8],
        representation: Representation,
    ) -> Result<Vec<BlockReport>> {
        let recognition = if Self::recognised(image) {
            Recognition::Recognised
        } else {
            Recognition::Consistent
        };
        Ok(self
            .verify(image)?
            .into_iter()
            .map(|status| {
                BlockReport::new(status, representation, ChecksumKind::Embedded, recognition)
            })
            .collect())
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
// DQ381 — self-describing BOOT/ASW/CAL CRC32 (big-endian zlib CRC-32)
// ---------------------------------------------------------------------------

/// Offset (within a block header) of the big-endian range-start pointer.
const DQ381_PTR_START: usize = 0x38;
/// Offset of the big-endian range-end pointer (inclusive).
const DQ381_PTR_END: usize = 0x3C;
/// Offset of the big-endian stored CRC-32.
const DQ381_CRC_OFF: usize = 0x44;

/// Minimum covered-region length for a block header to be believed (guards
/// against a coincidental sane-looking header over tiny/garbage ranges).
const DQ381_MIN_REGION: usize = 0x1000;

/// One VW_Flash DQ381 programming block. Every block is self-describing with the
/// **same** header layout (`+0x38` start, `+0x3C` inclusive end, `+0x44` CRC-32,
/// all big-endian); `base` is the block's flash base, i.e. its file offset in a
/// full ECU read, and the stored range pointers are absolute to that base.
///
/// Validated against the owner's real `dq381_full.bin`: an independent
/// `struct`/`zlib.crc32` cross-check matched all three stored CRCs — BOOT
/// `0xADCC4D12`, ASW `0x828A625B`, CAL `0x3D34EE10` (see `TCU-DQ381-VALIDATION.md`).
/// The CRC covers the pointer-declared range only, so OpenTune is unaffected by
/// the `ecudefs` CAL writable-extent discrepancy noted there (it uses the
/// embedded pointers, never a declared block length).
#[derive(Debug, Clone, Copy)]
struct Dq381BlockDef {
    name: &'static str,
    base: u32,
}

/// The three DQ381 blocks in flash order.
const DQ381_BLOCKS: &[Dq381BlockDef] = &[
    Dq381BlockDef {
        name: "BOOT",
        base: 0x01_0200,
    },
    Dq381BlockDef {
        name: "ASW",
        base: 0x03_0200,
    },
    Dq381BlockDef {
        name: "CAL",
        base: 0x14_0200,
    },
];

/// A located DQ381 block: its header's file offset and the region `[start, end]`
/// (inclusive file offsets) the CRC covers.
#[derive(Debug, Clone, Copy)]
struct Dq381Located {
    name: &'static str,
    region_start: usize,
    region_end: usize,
    crc_off: usize,
}

impl Dq381Located {
    /// Compute the zlib CRC-32 over the covered region. If the stored CRC field
    /// falls inside the region it is treated as zero for the computation (as the
    /// ECU does); on the observed DQ381 layout the CRC field sits *before* each
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

    fn status(&self, img: &[u8]) -> Option<BlockStatus> {
        let (computed, stored) = (self.compute(img)?, self.stored(img)?);
        Some(BlockStatus {
            name: format!("dq381-{}", self.name),
            region: MemoryRegion::new(
                self.region_start as u32,
                (self.region_end - self.region_start + 1) as u32,
            ),
            stored: stored as u64,
            computed: computed as u64,
            ok: stored == computed,
        })
    }
}

/// Try to read a sane self-describing header for `block` with its header at file
/// offset `base_off`. The range pointers are absolute to the block's flash
/// `base`, so the covered file offsets are `base_off + (pointer - base)`. Returns
/// `None` unless the pointers are well-formed and the region fits in the file and
/// is at least [`DQ381_MIN_REGION`] long (data-independent: editing the
/// calibration never moves these pointers).
fn locate_dq381(block: Dq381BlockDef, base_off: usize, img: &[u8]) -> Option<Dq381Located> {
    let len = img.len();
    if base_off + DQ381_CRC_OFF + 4 > len {
        return None;
    }
    let ps = u32be(img, base_off + DQ381_PTR_START)?;
    let pe = u32be(img, base_off + DQ381_PTR_END)?;
    // Pointers are absolute to the block base: start at/after the base, end after
    // start. This also disambiguates an extracted block — only the descriptor
    // whose base matches the header's own pointers yields an in-range region.
    if ps < block.base || pe <= ps {
        return None;
    }
    let rs = base_off + (ps - block.base) as usize;
    let re = base_off + (pe - block.base) as usize;
    if re <= rs || re >= len || (re - rs) < DQ381_MIN_REGION {
        return None;
    }
    Some(Dq381Located {
        name: block.name,
        region_start: rs,
        region_end: re,
        crc_off: base_off + DQ381_CRC_OFF,
    })
}

/// Locate every DQ381 block present and classify the representation.
///
/// * **Full ECU read** ([`Representation::RawFullImage`]): one or more block
///   headers sit at their absolute flash bases. Each block found is returned.
/// * **Extracted block** ([`Representation::ProgrammingBlock`]): no full-image
///   header, but a header at file offset 0 whose absolute pointers identify
///   exactly one block.
///
/// Returns [`Representation::Unknown`] with an empty list when nothing validates
/// structurally — the caller then refuses to guess.
fn find_dq381_blocks(img: &[u8]) -> (Representation, Vec<Dq381Located>) {
    let full: Vec<Dq381Located> = DQ381_BLOCKS
        .iter()
        .filter_map(|&b| locate_dq381(b, b.base as usize, img))
        .collect();
    if !full.is_empty() {
        return (Representation::RawFullImage, full);
    }
    for &b in DQ381_BLOCKS {
        if let Some(l) = locate_dq381(b, 0, img) {
            return (Representation::ProgrammingBlock, vec![l]);
        }
    }
    (Representation::Unknown, Vec::new())
}

/// VAG **DQ381** DSG TCU — self-describing **CRC32-VW** (big-endian zlib CRC-32)
/// over the BOOT, ASW and CAL programming blocks.
pub struct Dq381Crc32;

impl ChecksumAlgo for Dq381Crc32 {
    fn id(&self) -> &str {
        "dq381"
    }

    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        let (_, blocks) = find_dq381_blocks(image);
        Ok(blocks.iter().filter_map(|b| b.status(image)).collect())
    }

    /// Representation-aware report. Each located block carries the detected
    /// representation (full read vs extracted block — more specific than the
    /// caller's declaration), [`ChecksumKind::Embedded`] (the internal CRC the
    /// ECU checks; the external UDS/transfer checksum is a separate
    /// [`ChecksumKind::Transfer`] concern not computed here), and
    /// [`Recognition::Recognised`] because a self-describing header was found at a
    /// documented base — so a match is a confirmed-family result, never a
    /// coincidence. When nothing is located, the declared `representation` is
    /// echoed and the (empty) list carries no recognised claim.
    fn verify_report(
        &self,
        image: &[u8],
        representation: Representation,
    ) -> Result<Vec<BlockReport>> {
        let (detected, blocks) = find_dq381_blocks(image);
        let rep = if blocks.is_empty() {
            representation
        } else {
            detected
        };
        Ok(blocks
            .iter()
            .filter_map(|b| b.status(image))
            .map(|status| {
                BlockReport::new(status, rep, ChecksumKind::Embedded, Recognition::Recognised)
            })
            .collect())
    }

    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        let (_, blocks) = find_dq381_blocks(image);
        if blocks.is_empty() {
            return Err(Error::Unsupported(
                "not a recognised DQ381 image (no self-describing CRC32 header for BOOT/ASW/CAL \
                 at a known base); refusing to guess (see crate::tcu docs)"
                    .into(),
            ));
        }
        let mut fixes = Vec::new();
        for b in &blocks {
            // Anti-brick: only correct a block that structurally validates (its
            // region computes and its stored field reads). A block whose region
            // is out of range is skipped, never guessed.
            let (Some(computed), Some(stored)) = (b.compute(image), b.stored(image)) else {
                continue;
            };
            if computed != stored {
                image[b.crc_off..b.crc_off + 4].copy_from_slice(&computed.to_be_bytes());
                fixes.push(Fix {
                    name: format!("dq381-{}", b.name),
                    offset: b.crc_off as u32,
                    old: stored as u64,
                    new: computed as u64,
                });
            }
        }
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

    /// Look up a block descriptor by name.
    fn dq381_def(name: &str) -> Dq381BlockDef {
        DQ381_BLOCKS
            .iter()
            .find(|b| b.name == name)
            .copied()
            .unwrap()
    }

    /// Write a self-describing DQ381 block header for `def` at file offset
    /// `header_off`, declaring absolute pointers `[ptr_start, ptr_end_incl]`, and
    /// fill in the correct big-endian zlib CRC-32. For a full read `header_off`
    /// equals the flash base; for an extracted block it is 0.
    fn put_dq381_block(
        img: &mut [u8],
        def: Dq381BlockDef,
        header_off: usize,
        ptr_start: usize,
        ptr_end_incl: usize,
    ) {
        img[header_off + DQ381_PTR_START..header_off + DQ381_PTR_START + 4]
            .copy_from_slice(&(ptr_start as u32).to_be_bytes());
        img[header_off + DQ381_PTR_END..header_off + DQ381_PTR_END + 4]
            .copy_from_slice(&(ptr_end_incl as u32).to_be_bytes());
        let loc = locate_dq381(def, header_off, img).unwrap();
        let c = loc.compute(img).unwrap();
        img[loc.crc_off..loc.crc_off + 4].copy_from_slice(&c.to_be_bytes());
    }

    /// Build a synthetic **full-read** DQ381 image with all three self-describing
    /// blocks (BOOT/ASW/CAL) at their flash bases, each with a correct CRC-32.
    fn synthetic_dq381_full() -> Vec<u8> {
        let size = 0x18_0000usize;
        let mut img = vec![0u8; size];
        for (i, b) in img.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(13).wrapping_add(7);
        }
        put_dq381_block(&mut img, dq381_def("BOOT"), 0x01_0200, 0x01_0300, 0x02_F000);
        put_dq381_block(&mut img, dq381_def("ASW"), 0x03_0200, 0x03_0300, 0x13_F000);
        put_dq381_block(&mut img, dq381_def("CAL"), 0x14_0200, 0x14_0300, 0x17_F000);
        img
    }

    /// Build a synthetic **extracted CAL** block (standalone, header at offset 0)
    /// carrying absolute DQ381 pointers, with a correct big-endian zlib CRC-32.
    fn synthetic_dq381_cal_block() -> Vec<u8> {
        const CAL_BASE: usize = 0x14_0200;
        let size = 0x4_0000usize;
        let mut img = vec![0u8; size];
        for (i, b) in img.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(13).wrapping_add(7);
        }
        // Absolute pointers [CAL_BASE+0x100, CAL_BASE+0x3F000]; with the header at
        // file offset 0 these map to covered file offsets [0x100, 0x3F000].
        put_dq381_block(
            &mut img,
            dq381_def("CAL"),
            0,
            CAL_BASE + 0x100,
            CAL_BASE + 0x3F000,
        );
        img
    }

    #[test]
    fn dq381_full_three_blocks_verify_correct() {
        let img = synthetic_dq381_full();
        let algo = Dq381Crc32;
        let st = algo.verify(&img).unwrap();
        assert_eq!(st.len(), 3, "BOOT + ASW + CAL located");
        let names: Vec<_> = st.iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, ["dq381-BOOT", "dq381-ASW", "dq381-CAL"]);
        assert!(st.iter().all(|s| s.ok), "all three must verify as built");
        assert!(algo.correct(&mut img.clone()).unwrap().is_empty());

        // Representation is detected as a full read; every block recognised.
        let rep = algo.verify_report(&img, Representation::Unknown).unwrap();
        assert_eq!(rep.len(), 3);
        assert!(rep
            .iter()
            .all(|r| r.representation == Representation::RawFullImage));
        assert!(rep.iter().all(|r| r.recognition == Recognition::Recognised));
        assert!(rep
            .iter()
            .all(|r| r.checksum_kind == ChecksumKind::Embedded));
        assert!(rep.iter().all(|r| r.is_supported()));

        // Edit the ASW region; only ASW goes stale and is re-derived, BOOT/CAL
        // stay byte-for-byte untouched.
        let mut m = img.clone();
        m[0x03_1000] ^= 0xFF;
        let bad = algo.verify(&m).unwrap();
        assert_eq!(bad.iter().filter(|s| !s.ok).count(), 1);
        assert!(!bad.iter().find(|s| s.name == "dq381-ASW").unwrap().ok);
        let fixes = algo.correct(&mut m).unwrap();
        assert_eq!(fixes.len(), 1, "only the touched block corrected");
        assert_eq!(fixes[0].name, "dq381-ASW");
        assert!(algo.verify(&m).unwrap().iter().all(|s| s.ok));
        assert!(algo.correct(&mut m.clone()).unwrap().is_empty());
    }

    #[test]
    fn dq381_extracted_cal_block() {
        let img = synthetic_dq381_cal_block();
        let algo = Dq381Crc32;
        let st = algo.verify(&img).unwrap();
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].name, "dq381-CAL");
        assert!(st[0].ok, "freshly built extracted CAL must verify");

        let rep = algo.verify_report(&img, Representation::Unknown).unwrap();
        assert_eq!(rep.len(), 1);
        assert_eq!(
            rep[0].representation,
            Representation::ProgrammingBlock,
            "an extracted block is recognised as a programming block"
        );
        assert_eq!(rep[0].recognition, Recognition::Recognised);
        assert!(rep[0].is_supported());

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
        // No recognised block → an empty, un-supported detailed report.
        let rep = algo
            .verify_report(&img, Representation::RawFullImage)
            .unwrap();
        assert!(rep.is_empty());
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
    ///   OPENTUNE_FIXTURES=/path/to/opentune-fixtures \
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
        assert_eq!(st.len(), 3, "BOOT + ASW + CAL located in the full read");
        // Independently cross-checked stored CRCs (TCU-DQ381-VALIDATION.md).
        let stored_of = |name: &str| st.iter().find(|s| s.name == name).unwrap().stored;
        assert_eq!(stored_of("dq381-BOOT"), 0xADCC_4D12);
        assert_eq!(stored_of("dq381-ASW"), 0x828A_625B);
        assert_eq!(stored_of("dq381-CAL"), 0x3D34_EE10);
        assert!(
            st.iter().all(|s| s.ok),
            "every real DQ381 block CRC must verify (computed == stored)"
        );
        assert!(algo.correct(&mut img.clone()).unwrap().is_empty());

        // Detailed report: full read, all three recognised and supported.
        let rep = algo.verify_report(&img, Representation::Unknown).unwrap();
        assert_eq!(rep.len(), 3);
        assert!(rep
            .iter()
            .all(|r| r.representation == Representation::RawFullImage
                && r.recognition == Recognition::Recognised
                && r.is_supported()));

        // Wipe every stored CRC, correct, confirm the ECU's values are restored
        // exactly (byte-identical to the real dump).
        let (_, blocks) = find_dq381_blocks(&img);
        assert_eq!(blocks.len(), 3);
        let mut m = img.clone();
        for b in &blocks {
            m[b.crc_off..b.crc_off + 4].copy_from_slice(&[0, 0, 0, 0]);
        }
        let fixes = algo.correct(&mut m).unwrap();
        assert_eq!(fixes.len(), 3, "all three CRCs re-derived");
        assert_eq!(m, img, "corrected image byte-identical to the real dump");
    }
}
