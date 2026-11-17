//! VAG/ZF **transmission (TCU)** and BorgWarner **Haldex AWD** flash checksums.
//!
//! Covers the VAG DSG/automatic TCUs (DQ250-MQB, DQ381), the ZF 8HP automatic
//! TCU (AL551 generation), and the BorgWarner Haldex Gen5 4Motion AWD coupler.
//!
//! # Provenance (open-source port)
//!
//! Re-derived from open-source, licence-compatible tooling; cited, not copied:
//!
//! * **JAMCRC (DQ250-MQB)** — `TheFlashBold/DQ250-MQB-Bench-Flash` (GPL-3.0).
//!   `jamcrc = !crc32` (reflected CRC-32 with **no** final inversion), stored
//!   little-endian in the **last 4 bytes** of each flash block; SBOOT checks it
//!   on every boot.
//! * **CRC32-VW (DQ381)** and **ZF 8HP CAL CRC-32** — `dspl1236/vag-tcu-tools`
//!   (MIT). DQ381 protects each of its **BOOT, ASW and CAL** programming blocks
//!   with a big-endian zlib **CRC-32**; every block is self-describing with the
//!   same header (big-endian range pointers at `+0x38`/`+0x3C`, inclusive end,
//!   CRC at `+0x44`) at its flash base (BOOT `0x01_0200`, ASW `0x03_0200`, CAL
//!   `0x14_0200`). The ZF 8HP **CAL** checksum (from the AL551 Gen2 A2L memory
//!   map in `platforms/zf8hp`) is a zlib **CRC-32** over the CAL_DATA region
//!   `0x19_0000..=0x1F_FD5F`, stored **big-endian at `0x18_0244`**. Only the
//!   AL551-generation (method 0x22, e.g. 4G0/4H1 — 8HP55/8HP90) variants whose
//!   layout is documented are supported; the method-0xAA AES variants
//!   (4M0/8W0 — ALX520/AL552) and the Gen3 TriCore 8HP are **not** claimed.
//! * **Haldex Gen5 word-sum** — `bri3d/VW_Flash/lib/haldex_checksum.py` +
//!   `lib/modules/haldex4motion.py` (BSD-2-Clause). A per-block 16-bit
//!   little-endian **word-sum, ones-complemented** (`0xFFFF - sum`) over the
//!   block with its 10-byte checksum-header window `[cs_loc, cs_loc+0xA)`
//!   excluded, stored little-endian at `cs_loc+0x8`. The 8 header bytes are the
//!   block address and length; the CAL/ASW block lengths vary by SW version and
//!   are read from that embedded length field rather than assumed.
//!
//! OpenTune is GPL-3.0-or-later; GPL-3, MIT and BSD-2-Clause sources are all
//! compatible. The upstream copyright notices are retained here per the licences:
//!
//! ```text
//!   vag-tcu-tools  Copyright (c) 2026 dspl1236.                   MIT licence.
//!   VW_Flash       Copyright (c) 2022-2024 Brian Ledbetter et al. BSD-2-Clause.
//! ```
//!
//! # Algorithms and validation status
//!
//! | id           | family      | algorithm                       | validated against |
//! |--------------|-------------|---------------------------------|-------------------|
//! | `dq381`      | CRC32-VW    | zlib CRC-32, BE, self-describing| real `dq381_full.bin` (BOOT `0xADCC4D12`, ASW `0x828A625B`, CAL `0x3D34EE10`) |
//! | `dq250`      | JAMCRC      | `!crc32`, LE, per-block trailer | algorithm vectors + synthetic (no 0D9 bench dump on hand — validation pending) |
//! | `tcu-ccitt`  | CRC-CCITT   | CCITT `0x1021`, BE trailer      | CCITT vectors (no CCITT-protected dump on hand — validation pending) |
//! | `zf8hp`      | ZF 8HP CAL  | zlib CRC-32 over CAL_DATA, BE @`0x180244` | CRC-32 vectors + synthetic **only** (no real ZF 8HP dump on hand — refuses to self-correct, see below) |
//! | `haldex`     | Haldex Gen5 | 16-bit LE word-sum `0xFFFF - sum`, per block | synthetic + algorithm-vector **only** (no real Haldex Gen5 dump on hand — validation pending) |
//!
//! The ZF 8HP and Haldex Gen5 entries are **algorithm-vector + synthetic
//! validated only**: no real dump of either was available, so neither has been
//! confirmed end-to-end against an ECU-stored value. The fixture-gated tests
//! (`OPENTUNE_FIXTURES`) are no-ops until such a dump is supplied.
//!
//! # Anti-brick stance
//!
//! Each scheme refuses an image it cannot confirm. `dq250` only writes when the
//! block layout fits **and at least one block's JAMCRC already matches** (so the
//! algorithm is proven against a real stored value before any write). `dq381`
//! only writes a block whose self-describing header is present at a known base
//! and whose covered region validates structurally; each of BOOT/ASW/CAL is
//! handled independently.
//!
//! * **`zf8hp`** has a *fixed-offset* layout with no self-describing header, and
//!   no real dump was available to confirm it, so standalone `correct` is
//!   deliberately conservative: it does nothing when the image is already
//!   self-consistent and otherwise **refuses** (a mismatch is ambiguous — it
//!   could be an edited CAL or a non-ZF-8HP image, and writing a CRC at a fixed
//!   offset on an unconfirmed layout could brick). Correcting an edited image
//!   requires the owner's known-good stock dump via
//!   [`correct_with_reference`](ChecksumAlgo::correct_with_reference): the
//!   reference must itself validate at the fixed layout (proving the offsets for
//!   that firmware) before any write is made.
//! * **`haldex`** identifies a block by its *data-independent* embedded length
//!   field (`cs_loc+0x4`, which an edited calibration never changes), so it can
//!   self-correct exactly like `dq381`: it only writes the 2 checksum bytes of a
//!   structurally-located block and refuses an image where no block is located.
//!
//! A value that cannot be computed is never guessed.
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

#[inline]
fn u32le(img: &[u8], i: usize) -> Option<u32> {
    img.get(i..i + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

#[inline]
fn u16le(img: &[u8], i: usize) -> Option<u16> {
    img.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
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

// ---------------------------------------------------------------------------
// ZF 8HP (AL551 generation) — CAL block zlib CRC-32, big-endian @ 0x180244
// ---------------------------------------------------------------------------

/// Big-endian file offset of the stored ZF 8HP CAL CRC-32.
const ZF8HP_CAL_CRC_OFF: usize = 0x18_0244;
/// First byte of the CAL_DATA region the CRC covers.
const ZF8HP_CAL_REGION_START: usize = 0x19_0000;
/// Last byte of the CAL_DATA region the CRC covers (inclusive).
const ZF8HP_CAL_REGION_END: usize = 0x1F_FD5F;
/// Minimum image length to contain the full CAL_DATA region (`END + 1`).
const ZF8HP_MIN_LEN: usize = ZF8HP_CAL_REGION_END + 1;

/// `(stored, computed)` ZF 8HP CAL CRC-32 for a full decrypted image, or `None`
/// when the image is too short to carry the documented CAL layout.
///
/// `computed` is the zlib CRC-32 over `CAL_DATA` (`0x19_0000..=0x1F_FD5F`). The
/// CRC field at `0x18_0244` sits *before* that region on the documented layout,
/// so it is never part of the covered bytes; the defensive zeroing branch
/// mirrors [`Dq381Located::compute`] and stays inert here.
fn zf8hp_eval(img: &[u8]) -> Option<(u32, u32)> {
    if img.len() < ZF8HP_MIN_LEN || ZF8HP_CAL_CRC_OFF + 4 > img.len() {
        return None;
    }
    let region = img.get(ZF8HP_CAL_REGION_START..=ZF8HP_CAL_REGION_END)?;
    let computed = if ZF8HP_CAL_CRC_OFF >= ZF8HP_CAL_REGION_START
        && ZF8HP_CAL_CRC_OFF + 4 <= ZF8HP_CAL_REGION_END + 1
    {
        let mut buf = region.to_vec();
        let z = ZF8HP_CAL_CRC_OFF - ZF8HP_CAL_REGION_START;
        buf[z..z + 4].fill(0);
        crc32(&buf)
    } else {
        crc32(region)
    };
    let stored = u32be(img, ZF8HP_CAL_CRC_OFF)?;
    Some((stored, computed))
}

/// Build the single CAL [`BlockStatus`] for an image, if the layout fits.
fn zf8hp_status(img: &[u8]) -> Option<BlockStatus> {
    let (stored, computed) = zf8hp_eval(img)?;
    Some(BlockStatus {
        name: "zf8hp-CAL".into(),
        region: MemoryRegion::new(
            ZF8HP_CAL_REGION_START as u32,
            (ZF8HP_CAL_REGION_END - ZF8HP_CAL_REGION_START + 1) as u32,
        ),
        stored: stored as u64,
        computed: computed as u64,
        ok: stored == computed,
    })
}

/// **ZF 8HP** automatic TCU (AL551 generation, Renesas SH72549) — CAL block
/// zlib **CRC-32** over `CAL_DATA`, stored big-endian at `0x18_0244`.
///
/// Supports only the AL551-generation layout documented in the reference A2L
/// memory map (method 0x22 variants such as 4G0/4H1 — 8HP55/8HP90). It makes no
/// claim over the AES-encrypted 4M0/8W0 variants or the Gen3 TriCore 8HP.
///
/// Validation: CRC-32 vectors + synthetic self-consistency **only** — no real ZF
/// 8HP dump was on hand. Because the layout is fixed-offset with no
/// self-describing header, standalone [`correct`](ChecksumAlgo::correct) refuses
/// to write unless the image is already self-consistent; an edited image is
/// corrected only through
/// [`correct_with_reference`](ChecksumAlgo::correct_with_reference) with a
/// known-good stock dump that itself validates the layout (anti-brick).
pub struct Zf8hpCal;

impl Zf8hpCal {
    /// Recognised only when the CAL CRC already validates — proving the fixed
    /// layout against a real stored value, exactly like [`Dq250Jamcrc`].
    fn recognised(img: &[u8]) -> bool {
        matches!(zf8hp_eval(img), Some((s, c)) if s == c)
    }
}

impl ChecksumAlgo for Zf8hpCal {
    fn id(&self) -> &str {
        "zf8hp"
    }

    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        Ok(zf8hp_status(image).into_iter().collect())
    }

    /// Representation-aware report. The ZF 8HP CAL CRC is an [embedded](ChecksumKind::Embedded)
    /// checksum over a [`Representation::RawFullImage`] (the CAL block lives at its
    /// absolute flash base inside a full decrypted read). Recognition is
    /// [`Recognition::Recognised`] only when the stored CRC validates (proving the
    /// fixed layout against a real value); otherwise the arithmetic result is
    /// merely [`Recognition::Consistent`] and never surfaced as supported.
    fn verify_report(
        &self,
        image: &[u8],
        _representation: Representation,
    ) -> Result<Vec<BlockReport>> {
        let recognition = if Self::recognised(image) {
            Recognition::Recognised
        } else {
            Recognition::Consistent
        };
        Ok(zf8hp_status(image)
            .into_iter()
            .map(|status| {
                BlockReport::new(
                    status,
                    Representation::RawFullImage,
                    ChecksumKind::Embedded,
                    recognition,
                )
            })
            .collect())
    }

    /// Anti-brick: do nothing when already self-consistent, otherwise **refuse**.
    /// A mismatch at the fixed offset is ambiguous (edited CAL vs non-ZF-8HP
    /// image) and no real dump was available to confirm the layout, so writing a
    /// CRC here could brick. Use
    /// [`correct_with_reference`](Self::correct_with_reference) with a stock dump
    /// to correct an edited image.
    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        match zf8hp_eval(image) {
            Some((stored, computed)) if stored == computed => Ok(Vec::new()),
            Some(_) => Err(Error::Unsupported(
                "ZF 8HP CAL CRC does not match and no reference was given: the fixed-offset \
                 layout is unconfirmed (no real dump on hand), so refusing to write a CRC at \
                 0x180244 — pass the owner's known-good stock dump via correct_with_reference \
                 (see crate::tcu docs)"
                    .into(),
            )),
            None => Err(Error::Unsupported(
                "not a ZF 8HP full image (too short to hold the CAL_DATA region \
                 0x190000..=0x1FFD5F); refusing to guess (see crate::tcu docs)"
                    .into(),
            )),
        }
    }

    /// Correct the CAL CRC using the owner's known-good **stock dump** to confirm
    /// the fixed-offset layout first. The reference must itself validate at the
    /// documented layout (its stored CRC equals the CRC computed over its own
    /// `CAL_DATA`); only then is `image` re-checksummed at the same offsets and
    /// its CRC rewritten if stale. If the reference does not validate, the layout
    /// is unconfirmed and the call refuses rather than risk a brick.
    fn correct_with_reference(&self, image: &mut [u8], reference: &[u8]) -> Result<Vec<Fix>> {
        if !Self::recognised(reference) {
            return Err(Error::Unsupported(
                "ZF 8HP reference image does not validate at the documented CAL layout \
                 (stored CRC at 0x180244 != CRC over 0x190000..=0x1FFD5F); cannot confirm the \
                 fixed-offset layout, refusing to correct (see crate::tcu docs)"
                    .into(),
            ));
        }
        let Some((stored, computed)) = zf8hp_eval(image) else {
            return Err(Error::Unsupported(
                "target is not a ZF 8HP full image (too short for the CAL_DATA region); \
                 refusing to guess (see crate::tcu docs)"
                    .into(),
            ));
        };
        let mut fixes = Vec::new();
        if stored != computed {
            image[ZF8HP_CAL_CRC_OFF..ZF8HP_CAL_CRC_OFF + 4]
                .copy_from_slice(&computed.to_be_bytes());
            fixes.push(Fix {
                name: "zf8hp-CAL".into(),
                offset: ZF8HP_CAL_CRC_OFF as u32,
                old: stored as u64,
                new: computed as u64,
            });
        }
        Ok(fixes)
    }
}

// ---------------------------------------------------------------------------
// Haldex Gen5 (BorgWarner 4Motion AWD) — per-block 16-bit word-sum checksum
// ---------------------------------------------------------------------------

/// Size of the 10-byte checksum-header window `[cs_loc, cs_loc+0xA)` excluded
/// from the word-sum: a 4-byte block address, a 4-byte block length, then the
/// 2-byte stored checksum.
const HALDEX_HDR_WINDOW: usize = 0xA;
/// Offset, within the header window, of the 4-byte little-endian block length.
const HALDEX_LEN_OFF: usize = 0x4;
/// Offset, within the header window, of the 2-byte little-endian checksum.
const HALDEX_CKSUM_OFF: usize = 0x8;
/// Canonical Haldex Gen5 full bin-file size (`haldex_binfile_size`).
const HALDEX_BINFILE_SIZE: usize = 327_680; // 0x50000

/// A checksum-bearing Haldex Gen5 block. DRIVER (block 1) carries no checksum and
/// is intentionally absent. `cs_loc` is the checksum-header offset *within the
/// block*; `binfile_off`/`max_len` place and bound the block inside a full
/// bin-file. (From `haldex4motion.py`: `checksum_block_location`,
/// `haldex_binfile_offsets`.)
#[derive(Debug, Clone, Copy)]
struct HaldexBlockDef {
    name: &'static str,
    cs_loc: usize,
    binfile_off: usize,
    max_len: usize,
}

/// CAL, ASW and VERSION in bin-file order. `max_len` bounds each block at the
/// start of the next, so a bogus embedded length cannot run a block over its
/// neighbour when scanning a full bin-file.
const HALDEX_BLOCKS: &[HaldexBlockDef] = &[
    HaldexBlockDef {
        name: "CAL",
        cs_loc: 0x10,
        binfile_off: 0x00B400,
        max_len: 0x0_4C00, // ASW(0x10000) - CAL(0xB400)
    },
    HaldexBlockDef {
        name: "ASW",
        cs_loc: 0x200,
        binfile_off: 0x010000,
        max_len: 0x3_DC00, // VERSION(0x4DC00) - ASW(0x10000)
    },
    HaldexBlockDef {
        name: "VERSION",
        cs_loc: 0x0,
        binfile_off: 0x04DC00,
        max_len: 0x0_2400, // END(0x50000) - VERSION(0x4DC00)
    },
];

/// The Haldex Gen5 per-block checksum over the slice `blk`: a 16-bit
/// little-endian word-sum of every byte **except** the 10-byte header window
/// `[cs_loc, cs_loc+0xA)`, ones-complemented (`0xFFFF - sum`). Returns `None`
/// when the window does not fit or the covered region is not an even number of
/// bytes (the reference reads it as whole little-endian `u16` words).
fn haldex_checksum(blk: &[u8], cs_loc: usize) -> Option<u16> {
    let n = blk.len();
    if cs_loc.checked_add(HALDEX_HDR_WINDOW)? > n {
        return None;
    }
    // Covered length is `n - 0xA` regardless of cs_loc; it must be even.
    if !(n - HALDEX_HDR_WINDOW).is_multiple_of(2) {
        return None;
    }
    let mut sum: u32 = 0;
    let mut add_words = |slice: &[u8]| {
        // `slice` is whole 16-bit words: cs_loc is even on every known block and
        // the header window ends on an even offset, so both halves are aligned.
        let (words, _) = slice.as_chunks::<2>();
        for w in words {
            sum = (sum + (w[0] as u32 | (w[1] as u32) << 8)) & 0xFFFF;
        }
    };
    add_words(&blk[..cs_loc]); // bytes before the header window
    add_words(&blk[cs_loc + HALDEX_HDR_WINDOW..]); // bytes after it
    Some(0xFFFF - sum as u16)
}

/// A located Haldex block: its slice within the image, the in-slice checksum
/// offset, and its name.
#[derive(Debug, Clone, Copy)]
struct HaldexLocated {
    name: &'static str,
    slice_start: usize,
    slice_len: usize,
    cs_loc: usize,
}

impl HaldexLocated {
    fn block<'a>(&self, img: &'a [u8]) -> Option<&'a [u8]> {
        img.get(self.slice_start..self.slice_start + self.slice_len)
    }

    /// `(stored, computed)` for this located block.
    fn eval(&self, img: &[u8]) -> Option<(u16, u16)> {
        let blk = self.block(img)?;
        let computed = haldex_checksum(blk, self.cs_loc)?;
        let stored = u16le(blk, self.cs_loc + HALDEX_CKSUM_OFF)?;
        Some((stored, computed))
    }

    /// Absolute file offset of the 2 stored checksum bytes.
    fn cksum_file_off(&self) -> usize {
        self.slice_start + self.cs_loc + HALDEX_CKSUM_OFF
    }

    fn status(&self, img: &[u8]) -> Option<BlockStatus> {
        let (stored, computed) = self.eval(img)?;
        Some(BlockStatus {
            name: format!("haldex-{}", self.name),
            region: MemoryRegion::new(self.slice_start as u32, self.slice_len as u32),
            stored: stored as u64,
            computed: computed as u64,
            ok: stored == computed,
        })
    }
}

/// Validate a candidate block header at `base_off` and, if its embedded length
/// field is structurally sound, return the located block. `require_full` demands
/// the embedded length span the whole remaining buffer (the extracted-block
/// case, where the length field declares the exact block size); otherwise the
/// length is bounded by `def.max_len` (the full-bin-file case).
fn locate_haldex(
    def: HaldexBlockDef,
    base_off: usize,
    img: &[u8],
    require_full: bool,
) -> Option<HaldexLocated> {
    let len = u32le(img, base_off + def.cs_loc + HALDEX_LEN_OFF)? as usize;
    if len < def.cs_loc + HALDEX_HDR_WINDOW {
        return None; // too short to even hold its own checksum header
    }
    if !(len - HALDEX_HDR_WINDOW).is_multiple_of(2) {
        return None; // covered region must be whole 16-bit words
    }
    if base_off.checked_add(len)? > img.len() {
        return None; // declared block runs past the buffer
    }
    if require_full {
        // Extracted block: the embedded length declares the whole buffer.
        if base_off != 0 || len != img.len() {
            return None;
        }
    } else if len > def.max_len {
        // Full bin-file: a block may not overrun into the next one.
        return None;
    }
    Some(HaldexLocated {
        name: def.name,
        slice_start: base_off,
        slice_len: len,
        cs_loc: def.cs_loc,
    })
}

/// Locate every Haldex Gen5 checksum block and classify the representation.
///
/// * **Full bin-file** ([`Representation::RawFullImage`]): the image is exactly
///   the canonical [`HALDEX_BINFILE_SIZE`] and one or more block headers sit at
///   their fixed bin-file offsets with a sane embedded length.
/// * **Extracted block** ([`Representation::ProgrammingBlock`]): a header at file
///   offset 0 whose embedded length declares the whole buffer identifies exactly
///   one block type (CAL/ASW/VERSION), data-independently.
///
/// Returns [`Representation::Unknown`] with an empty list when nothing validates
/// structurally — the caller then refuses to guess.
fn find_haldex_blocks(img: &[u8]) -> (Representation, Vec<HaldexLocated>) {
    if img.len() == HALDEX_BINFILE_SIZE {
        let full: Vec<HaldexLocated> = HALDEX_BLOCKS
            .iter()
            .filter_map(|&d| locate_haldex(d, d.binfile_off, img, false))
            .collect();
        if !full.is_empty() {
            return (Representation::RawFullImage, full);
        }
    }
    for &d in HALDEX_BLOCKS {
        if let Some(l) = locate_haldex(d, 0, img, true) {
            return (Representation::ProgrammingBlock, vec![l]);
        }
    }
    (Representation::Unknown, Vec::new())
}

/// **Haldex Gen5** (BorgWarner 4Motion AWD coupler) — per-block 16-bit
/// little-endian word-sum checksum (`0xFFFF - sum`).
///
/// Operates on either a single extracted programming block (CAL/ASW/VERSION) or
/// the canonical full bin-file. A block is identified by its *data-independent*
/// embedded length field, so an edited calibration never changes which block is
/// recognised and the scheme self-corrects like DQ381.
///
/// Validation: synthetic + algorithm-vector **only** — no real Haldex Gen5 dump
/// was on hand, so end-to-end validation against an ECU-stored value is pending.
pub struct HaldexGen5;

impl ChecksumAlgo for HaldexGen5 {
    fn id(&self) -> &str {
        "haldex"
    }

    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        let (_, blocks) = find_haldex_blocks(image);
        Ok(blocks.iter().filter_map(|b| b.status(image)).collect())
    }

    /// Representation-aware report. Each located block carries the detected
    /// representation (full bin-file vs extracted block),
    /// [`ChecksumKind::Embedded`], and [`Recognition::Recognised`] — a block is
    /// only located when its embedded length field is structurally sound, so a
    /// match is a confirmed-family result, not a coincidence. When nothing is
    /// located, the declared `representation` is echoed over the (empty) list.
    fn verify_report(
        &self,
        image: &[u8],
        representation: Representation,
    ) -> Result<Vec<BlockReport>> {
        let (detected, blocks) = find_haldex_blocks(image);
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
        let (_, blocks) = find_haldex_blocks(image);
        if blocks.is_empty() {
            return Err(Error::Unsupported(
                "not a recognised Haldex Gen5 image (no block whose embedded length field \
                 identifies a CAL/ASW/VERSION block, as a full bin-file or an extracted block); \
                 refusing to guess (see crate::tcu docs)"
                    .into(),
            ));
        }
        let mut fixes = Vec::new();
        for b in &blocks {
            // Anti-brick: only correct a structurally-located block whose checksum
            // both computes and reads; never guess an out-of-range one.
            let Some((stored, computed)) = b.eval(image) else {
                continue;
            };
            if stored != computed {
                let off = b.cksum_file_off();
                image[off..off + 2].copy_from_slice(&computed.to_le_bytes());
                fixes.push(Fix {
                    name: format!("haldex-{}", b.name),
                    offset: off as u32,
                    old: stored as u64,
                    new: computed as u64,
                });
            }
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

    // -----------------------------------------------------------------------
    // ZF 8HP — synthetic CAL CRC-32 (no real dump: vector + self-consistency)
    // -----------------------------------------------------------------------

    /// Build a synthetic ZF 8HP full image (2 MiB) with a correct CAL CRC-32 at
    /// `0x180244` over `CAL_DATA` (`0x190000..=0x1FFD5F`).
    fn synthetic_zf8hp() -> Vec<u8> {
        let mut img = vec![0u8; ZF8HP_MIN_LEN.max(0x20_0000)];
        for (i, b) in img.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(31).wrapping_add(5);
        }
        let region = &img[ZF8HP_CAL_REGION_START..=ZF8HP_CAL_REGION_END];
        let c = crc32(region);
        img[ZF8HP_CAL_CRC_OFF..ZF8HP_CAL_CRC_OFF + 4].copy_from_slice(&c.to_be_bytes());
        img
    }

    #[test]
    fn zf8hp_vector_and_self_consistency() {
        let img = synthetic_zf8hp();
        let algo = Zf8hpCal;
        // Algorithm vector: the stored CRC is exactly the zlib CRC-32 of CAL_DATA.
        let (stored, computed) = zf8hp_eval(&img).unwrap();
        assert_eq!(stored, computed);
        assert_eq!(
            stored,
            crc32(&img[ZF8HP_CAL_REGION_START..=ZF8HP_CAL_REGION_END])
        );
        // verify + already-valid correct is a no-op.
        assert!(algo.verify(&img).unwrap().iter().all(|s| s.ok));
        assert!(algo.correct(&mut img.clone()).unwrap().is_empty());

        // Recognised + supported in the detailed report.
        let rep = algo.verify_report(&img, Representation::Unknown).unwrap();
        assert_eq!(rep.len(), 1);
        assert_eq!(rep[0].representation, Representation::RawFullImage);
        assert_eq!(rep[0].recognition, Recognition::Recognised);
        assert_eq!(rep[0].checksum_kind, ChecksumKind::Embedded);
        assert!(rep[0].is_supported());
    }

    #[test]
    fn zf8hp_edited_cal_refused_standalone_but_fixed_with_reference() {
        let stock = synthetic_zf8hp();
        let algo = Zf8hpCal;

        // Edit a CAL_DATA byte: the CAL CRC goes stale.
        let mut edited = stock.clone();
        edited[ZF8HP_CAL_REGION_START + 0x100] ^= 0xFF;
        assert!(algo.verify(&edited).unwrap().iter().any(|s| !s.ok));
        // A tampered image is now only "consistent", never supported.
        let rep = algo
            .verify_report(&edited, Representation::Unknown)
            .unwrap();
        assert_eq!(rep[0].recognition, Recognition::Consistent);
        assert!(!rep[0].is_supported());

        // Anti-brick: standalone correct REFUSES (ambiguous, unconfirmed layout).
        assert!(matches!(
            algo.correct(&mut edited.clone()).unwrap_err(),
            Error::Unsupported(_)
        ));

        // With the owner's validating stock dump, the layout is confirmed and the
        // edited image is corrected; it then round-trips and is idempotent.
        let fixes = algo.correct_with_reference(&mut edited, &stock).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].name, "zf8hp-CAL");
        assert_eq!(fixes[0].offset, ZF8HP_CAL_CRC_OFF as u32);
        assert!(algo.verify(&edited).unwrap().iter().all(|s| s.ok));
        assert!(algo
            .correct_with_reference(&mut edited.clone(), &stock)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn zf8hp_refuses_bad_reference_and_short_image() {
        let algo = Zf8hpCal;
        let stock = synthetic_zf8hp();

        // A reference that does not validate cannot confirm the layout → refuse.
        let mut bad_ref = stock.clone();
        bad_ref[ZF8HP_CAL_CRC_OFF] ^= 0xFF; // corrupt the reference's own CRC
        let mut target = stock.clone();
        target[ZF8HP_CAL_REGION_START] ^= 0x01;
        assert!(matches!(
            algo.correct_with_reference(&mut target, &bad_ref)
                .unwrap_err(),
            Error::Unsupported(_)
        ));

        // Too short to hold CAL_DATA → verify empty, correct refuses.
        let mut small = vec![0u8; 0x1000];
        assert!(algo.verify(&small).unwrap().is_empty());
        assert!(matches!(
            algo.correct(&mut small).unwrap_err(),
            Error::Unsupported(_)
        ));
    }

    // -----------------------------------------------------------------------
    // Haldex Gen5 — synthetic word-sum (no real dump: vector + round-trip)
    // -----------------------------------------------------------------------

    #[test]
    fn haldex_checksum_known_vector() {
        // cs_loc = 0, length 14: window [0,0xA) excluded, region = blk[0xA..0xE].
        // region = 01 02 03 04 -> LE words 0x0201 + 0x0403 = 0x0604;
        // checksum = 0xFFFF - 0x0604 = 0xF9FB.
        let mut blk = [0xAAu8; 14];
        blk[0xA..0xE].copy_from_slice(&[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(haldex_checksum(&blk, 0x0), Some(0xF9FB));
        // Odd covered length is rejected (reference reads whole u16 words).
        let odd = [0u8; 13];
        assert_eq!(haldex_checksum(&odd, 0x0), None);
    }

    /// Look up a Haldex block descriptor by name.
    fn haldex_def(name: &str) -> HaldexBlockDef {
        HALDEX_BLOCKS
            .iter()
            .find(|b| b.name == name)
            .copied()
            .unwrap()
    }

    /// Write a Haldex block header at `header_off` for `def`, declaring block
    /// length `len`, and fill in the correct little-endian word-sum checksum.
    fn put_haldex_block(img: &mut [u8], def: HaldexBlockDef, header_off: usize, len: usize) {
        // Embedded little-endian length field at cs_loc+0x4.
        img[header_off + def.cs_loc + HALDEX_LEN_OFF..header_off + def.cs_loc + HALDEX_LEN_OFF + 4]
            .copy_from_slice(&(len as u32).to_le_bytes());
        let blk = &img[header_off..header_off + len];
        let c = haldex_checksum(blk, def.cs_loc).unwrap();
        let off = header_off + def.cs_loc + HALDEX_CKSUM_OFF;
        img[off..off + 2].copy_from_slice(&c.to_le_bytes());
    }

    /// Build a synthetic **extracted** Haldex block (header at offset 0) of the
    /// given length with a correct checksum.
    fn synthetic_haldex_block(name: &str, len: usize) -> Vec<u8> {
        let def = haldex_def(name);
        let mut img = vec![0u8; len];
        for (i, b) in img.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(17).wrapping_add(3);
        }
        put_haldex_block(&mut img, def, 0, len);
        img
    }

    #[test]
    fn haldex_extracted_cal_roundtrip() {
        // CAL length that varies by SW version (even); read from the length field.
        let img = synthetic_haldex_block("CAL", 0x333E);
        let algo = HaldexGen5;
        let st = algo.verify(&img).unwrap();
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].name, "haldex-CAL");
        assert!(st[0].ok, "freshly built extracted CAL must verify");
        assert!(algo.correct(&mut img.clone()).unwrap().is_empty());

        let rep = algo.verify_report(&img, Representation::Unknown).unwrap();
        assert_eq!(rep.len(), 1);
        assert_eq!(rep[0].representation, Representation::ProgrammingBlock);
        assert_eq!(rep[0].recognition, Recognition::Recognised);
        assert!(rep[0].is_supported());

        // Edit the calibration: checksum goes stale and is re-derived; the
        // embedded length field is untouched so the block stays recognised.
        let mut m = img.clone();
        m[0x500] ^= 0xFF;
        assert!(algo.verify(&m).unwrap().iter().any(|s| !s.ok));
        let fixes = algo.correct(&mut m).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].name, "haldex-CAL");
        assert!(algo.verify(&m).unwrap().iter().all(|s| s.ok));
        assert!(algo.correct(&mut m.clone()).unwrap().is_empty());
    }

    #[test]
    fn haldex_extracted_asw_roundtrip() {
        let img = synthetic_haldex_block("ASW", 0x3DB80);
        let algo = HaldexGen5;
        let st = algo.verify(&img).unwrap();
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].name, "haldex-ASW");
        assert!(st[0].ok);
        let mut m = img.clone();
        m[0x9000] ^= 0xFF;
        let fixes = algo.correct(&mut m).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].name, "haldex-ASW");
        assert!(algo.verify(&m).unwrap().iter().all(|s| s.ok));
    }

    /// Build a synthetic full Haldex bin-file (`0x50000`) with CAL/ASW/VERSION at
    /// their fixed offsets, each with a correct checksum.
    fn synthetic_haldex_binfile() -> Vec<u8> {
        let mut img = vec![0u8; HALDEX_BINFILE_SIZE];
        for (i, b) in img.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(17).wrapping_add(3);
        }
        put_haldex_block(
            &mut img,
            haldex_def("CAL"),
            haldex_def("CAL").binfile_off,
            0x333E,
        );
        put_haldex_block(
            &mut img,
            haldex_def("ASW"),
            haldex_def("ASW").binfile_off,
            0x3DB80,
        );
        put_haldex_block(
            &mut img,
            haldex_def("VERSION"),
            haldex_def("VERSION").binfile_off,
            0xE,
        );
        img
    }

    #[test]
    fn haldex_full_binfile_three_blocks() {
        let img = synthetic_haldex_binfile();
        let algo = HaldexGen5;
        let st = algo.verify(&img).unwrap();
        assert_eq!(st.len(), 3, "CAL + ASW + VERSION located");
        let names: Vec<_> = st.iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, ["haldex-CAL", "haldex-ASW", "haldex-VERSION"]);
        assert!(st.iter().all(|s| s.ok));
        assert!(algo.correct(&mut img.clone()).unwrap().is_empty());

        let rep = algo.verify_report(&img, Representation::Unknown).unwrap();
        assert_eq!(rep.len(), 3);
        assert!(rep
            .iter()
            .all(|r| r.representation == Representation::RawFullImage
                && r.recognition == Recognition::Recognised
                && r.is_supported()));

        // Edit the ASW region: only ASW goes stale; CAL/VERSION stay untouched.
        let mut m = img.clone();
        m[haldex_def("ASW").binfile_off + 0x1000] ^= 0xFF;
        let bad = algo.verify(&m).unwrap();
        assert_eq!(bad.iter().filter(|s| !s.ok).count(), 1);
        let fixes = algo.correct(&mut m).unwrap();
        assert_eq!(fixes.len(), 1, "only the touched block corrected");
        assert_eq!(fixes[0].name, "haldex-ASW");
        assert!(algo.verify(&m).unwrap().iter().all(|s| s.ok));
        assert!(algo.correct(&mut m.clone()).unwrap().is_empty());
    }

    #[test]
    fn haldex_unrecognised_is_refused() {
        let algo = HaldexGen5;
        // No sane embedded length anywhere → refuse.
        let mut img = vec![0u8; 0x4000];
        assert!(algo.verify(&img).unwrap().is_empty());
        assert!(matches!(
            algo.correct(&mut img).unwrap_err(),
            Error::Unsupported(_)
        ));
        // Canonical bin-file size but all zero → no block located → refuse.
        let mut zeros = vec![0u8; HALDEX_BINFILE_SIZE];
        assert!(matches!(
            algo.correct(&mut zeros).unwrap_err(),
            Error::Unsupported(_)
        ));
    }

    // -----------------------------------------------------------------------
    // Fixture-gated real-dump regressions (no-ops without OPENTUNE_FIXTURES)
    // -----------------------------------------------------------------------

    /// Real ZF 8HP dump regression. No access in CI, so `#[ignore]`d:
    ///   OPENTUNE_FIXTURES=/path/to/opentune-fixtures \
    ///     cargo test -p opentune-checksum -- --ignored --nocapture
    #[test]
    #[ignore = "needs owner ZF 8HP fixtures via OPENTUNE_FIXTURES"]
    fn reference_zf8hp_cal() {
        let Ok(dir) = std::env::var("OPENTUNE_FIXTURES") else {
            return;
        };
        let Ok(img) = std::fs::read(format!("{dir}/tcu/zf8hp_full.bin")) else {
            return;
        };
        let algo = Zf8hpCal;
        let st = algo.verify(&img).unwrap();
        assert_eq!(st.len(), 1, "CAL located in the full ZF 8HP read");
        assert!(st[0].ok, "the real ZF 8HP CAL CRC must verify");
        assert!(algo.correct(&mut img.clone()).unwrap().is_empty());
        // The real dump validates the fixed layout, so it is its own reference.
        let mut m = img.clone();
        m[ZF8HP_CAL_CRC_OFF..ZF8HP_CAL_CRC_OFF + 4].fill(0);
        let fixes = algo.correct_with_reference(&mut m, &img).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(m, img, "corrected image byte-identical to the real dump");
    }

    /// Real Haldex Gen5 dump regression. No-op without fixtures.
    #[test]
    #[ignore = "needs owner Haldex Gen5 fixtures via OPENTUNE_FIXTURES"]
    fn reference_haldex() {
        let Ok(dir) = std::env::var("OPENTUNE_FIXTURES") else {
            return;
        };
        let Ok(img) = std::fs::read(format!("{dir}/tcu/haldex_binfile.bin")) else {
            return;
        };
        let algo = HaldexGen5;
        let st = algo.verify(&img).unwrap();
        assert!(!st.is_empty(), "at least one Haldex block located");
        assert!(
            st.iter().all(|s| s.ok),
            "every real Haldex block checksum must verify"
        );
        assert!(algo.correct(&mut img.clone()).unwrap().is_empty());
    }
}
