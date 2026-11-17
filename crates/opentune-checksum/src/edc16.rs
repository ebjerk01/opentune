//! Bosch EDC16 flash checksum engine.
//!
//! # The MPC56x scheme — REVERSE-ENGINEERED (cracked from full dumps)
//!
//! The common EDC16 variants built on the Freescale/Motorola **MPC56x**
//! (PowerPC, big-endian) core — `EDC16C35` (BMW), `EDC16C39` (Kia/Hyundai) and
//! their siblings — protect the flash with a **block directory** plus a plain
//! **32-bit big-endian additive checksum to a fixed constant**. The scheme was
//! fully recovered by analysing complete (boot+program+calibration) dumps:
//!
//! * The image carries a directory of 16-byte records
//!   `{ start:u32be, end:u32be, 0xFADECAFE, 0xCAFEAFFE }`. `start`/`end` are the
//!   inclusive CPU-address bounds of a checksum-protected block; the two trailing
//!   magic words `0xFADECAFE` / `0xCAFEAFFE` are the (unused) checksum /
//!   complement placeholder slots and are the directory's structural fingerprint.
//! * CPU addresses map to file offsets by masking with `len-1` (equivalently,
//!   subtracting the per-image ROM base: `0` for the 2 MiB BMW/Kia images,
//!   `0x0080_0000` for the 1 MiB BMW 320d).
//! * For **every** block the ECU verifies:
//!   `sum32be(flash[start..=end]) == 0xD01F_E500` — the sum of big-endian 32-bit
//!   words over the block, truncated to 32 bits, equals the fixed Bosch constant
//!   [`EDC16_MPC_SUM_CONST`]. (Verified identical across BMW C35 318d, BMW C35
//!   320d and five Kia C39 pairs — 14 images, every directory block.)
//! * The **last dword of each block** is that block's compensation / checksum
//!   word: `last = (K - sum_of_the_rest) mod 2^32`, stored big-endian. For the
//!   calibration block this is the lone "clean field" surrounded by fill that
//!   earlier calibration-only analysis found (BMW 318d `0xFDFF8`, 320d `0xFEFF8`)
//!   — read **big-endian** it is exactly this compensation word.
//!
//! [`verify_directory`] walks the real on-image directory and checks each block;
//! [`correct_directory`] rewrites each off block's compensation word (innermost
//! block first, so nested sub-blocks are fixed before their enclosing block).
//! This is wired into [`Edc16::verify`] / [`Edc16::correct`]: when a directory is
//! present (a full dump) it is used; otherwise the engine falls back to surfacing
//! the calibration field from [`KNOWN_FIELDS`] / [`DEFAULT_BLOCKS`].
//!
//! ## How this reconciles the earlier (calibration-only) finding
//!
//! The prior pass — which had only the calibration partition — correctly located
//! the field but could not identify the algorithm, because the relationship is
//! *not* `sum(range) == stored` (which it searched for) but
//! `sum(block including the field) == constant`, and because the field reads as
//! **big-endian** on this PowerPC core, not little-endian. Those two facts, plus
//! the directory living in the boot/program area (absent from a cal-only dump),
//! are exactly why it looked like an unidentified avalanche value and why the
//! GF(2) GCD never found a degree-32 CRC generator: it is an additive checksum,
//! not a CRC.
//!
//! # The generic block engine (reused)
//!
//! A reusable, reference-backed Bosch forward CRC-32 ([`Algo::Crc32Bosch`]) and
//! additive algorithms are retained for EDC16/EDC17/MED variants whose blocks use
//! them, and the [`KnownField`] table records the validated calibration-field
//! facts for documentation and regression.
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
    /// Bosch/VAG main CRC-32 (forward poly `0x04C11DB7`, init 0, no xorout) —
    /// see [`crate::crc32_bosch`] and `docs/BOSCH-CHECKSUM.md`.
    Crc32Bosch,
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
            Algo::Crc32 | Algo::Crc32Bosch | Algo::Unknown => 1,
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

/// Default EDC16 block map (fallback for calibration-only images).
///
/// Holds the one VERIFIED-in-position calibration checksum field of the Toyota
/// reference ECU (`1025 C9/ESB/G43`, firmware "TSW V0.70 120701"). It is left as
/// [`Algo::Unknown`] because the on-image directory needed to drive the
/// MPC56x [`verify_directory`] walker is absent from a calibration-only dump
/// (the directory lives in the boot/program area). When a full dump is supplied,
/// [`Edc16::verify`] / [`Edc16::correct`] use the directory walker instead of
/// this table; this entry only surfaces the field when no directory is present.
pub const DEFAULT_BLOCKS: &[Block] = &[Block {
    name: "cal-checksum",
    sum_range: (0x0007_0000, 0x0007_BD7C),
    store_offset: 0x0007_BD7C,
    store_len: 4,
    store_le: true,
    algo: Algo::Unknown,
}];

/// A verified EDC16 calibration-checksum field from a real stock/tuned pair.
///
/// Records only what is *proven* by diffing two valid images: the field's
/// position/width/order and the two stored 32-bit values. The algorithm that
/// produces them is still [`Algo::Unknown`]; this table exists so the fixtures
/// test can assert the field locations across every harvested pair.
#[derive(Debug, Clone, Copy)]
pub struct KnownField {
    /// Fixture base name (`<name>_ori.bin` / `<name>_mod.bin`, Yaris uses the
    /// legacy `yaris_90_stock.ori` / `yaris_90_tuned.mod`).
    pub fixture: &'static str,
    /// Image length in bytes.
    pub image_len: usize,
    /// Byte offset of the 32-bit little-endian checksum field.
    pub store_offset: u32,
    /// Stored value in the stock image (little-endian).
    pub stored_stock: u32,
    /// Stored value in the tuned image (little-endian).
    pub stored_tuned: u32,
}

/// The validated clean-field EDC16 calibration checksums (see module docs).
pub const KNOWN_FIELDS: &[KnownField] = &[
    KnownField {
        fixture: "yaris_90",
        image_len: 0x8_0000,
        store_offset: 0x0007_BD7C,
        stored_stock: 0xDBCC_ED4D,
        stored_tuned: 0xDC88_782E,
    },
    KnownField {
        fixture: "bmw_e46_318d_376967_2m",
        image_len: 0x20_0000,
        store_offset: 0x000F_DFF8,
        stored_stock: 0x5A19_65D7,
        stored_tuned: 0x70C4_3539,
    },
    KnownField {
        fixture: "bmw_e46_320d_361811_2m",
        image_len: 0x10_0000,
        store_offset: 0x000F_EFF8,
        stored_stock: 0x309C_C08C,
        stored_tuned: 0xA1B4_2CB4,
    },
];

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
        Algo::Crc32Bosch => crate::crc32_bosch(data) as u64 & mask,
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
                    Algo::Crc32 | Algo::Crc32Bosch | Algo::Unknown => unreachable!(),
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

// ---------------------------------------------------------------------------
// MPC56x directory engine (EDC16C35 / EDC16C39 and siblings) — the cracked
// scheme. See the module docs for the reverse-engineering summary.
// ---------------------------------------------------------------------------

/// The fixed Bosch constant every MPC56x EDC16 block's 32-bit big-endian word
/// sum must equal. Recovered empirically and verified identical across BMW C35
/// (318d, 320d) and five Kia C39 pairs — every directory block of 14 full dumps.
pub const EDC16_MPC_SUM_CONST: u32 = 0xD01F_E500;

/// The directory record's trailing checksum/complement placeholder magics, in
/// file order (big-endian). Their presence marks a `{start,end,..}` record.
const MARK_CS: [u8; 4] = [0xFA, 0xDE, 0xCA, 0xFE]; // 0xFADECAFE
const MARK_CP: [u8; 4] = [0xCA, 0xFE, 0xAF, 0xFE]; // 0xCAFEAFFE

/// One checksum-protected block parsed from the on-image directory, as file
/// offsets. `end` is inclusive. The compensation word is the last dword
/// (`[end-3 ..= end]`, big-endian).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirBlock {
    /// First byte of the summed block (file offset).
    pub start: u32,
    /// Last byte of the summed block (file offset, inclusive).
    pub end: u32,
}

impl DirBlock {
    /// File offset of the 4-byte big-endian compensation (checksum) word.
    pub fn store_offset(self) -> u32 {
        self.end - 3
    }
}

/// Read a big-endian u32 at `off`, or `None` if out of range.
fn read_u32_be(image: &[u8], off: usize) -> Option<u32> {
    image
        .get(off..off + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// 32-bit big-endian additive checksum over `image[start..=end]` (inclusive),
/// mod 2^32. A trailing partial (< 4-byte) word is ignored, matching the ECU.
fn sum32be_range(image: &[u8], start: u32, end: u32) -> u32 {
    let (s, e) = (start as usize, end as usize);
    let hi = (e + 1).min(image.len());
    let mut acc: u32 = 0;
    let mut i = s;
    while i + 4 <= hi {
        acc = acc.wrapping_add(u32::from_be_bytes([
            image[i],
            image[i + 1],
            image[i + 2],
            image[i + 3],
        ]));
        i += 4;
    }
    acc
}

/// Map a directory CPU address to a file offset within the detected base window.
///
/// EDC16 images are power-of-two sized (512 KiB / 1 MiB / 2 MiB) and mapped at a
/// power-of-two-aligned ROM `base` (0 for most 2 MiB images, `0x0018_0000` for
/// 512 KiB, `0x0080_0000` for the 1 MiB BMW 320d). A genuine record encodes an
/// address inside `[base, base + len)`, so the file offset is `addr - base`.
///
/// Addresses OUTSIDE that window are rejected, not wrapped. The previous
/// `addr & (len - 1)` wrap turned out-of-window coincidental `{start,end}` words
/// into phantom in-range regions — e.g. two large (16–18 KiB) ghost blocks on
/// stock Audi EDC16 dumps whose records encode `0x0040_0000` / `0x0041_0100`; a
/// subsequent `correct()` would then rewrite real data there and brick the file.
fn addr_to_offset(addr: u32, len: usize, base: u32, pow2: bool) -> Option<u32> {
    if !pow2 {
        // Non-power-of-two: only accept addresses that already are offsets.
        return if (addr as usize) < len {
            Some(addr)
        } else {
            None
        };
    }
    let off = addr.checked_sub(base)?;
    if (off as usize) < len {
        Some(off)
    } else {
        None
    }
}

/// Minimum size (bytes) of a genuine EDC16 checksum block. The smallest real
/// block across 60 reference dumps is `0x40` bytes; the only sub-`0x40` "regions"
/// are coincidental 8-byte marker pairs sitting in the padding between genuine
/// blocks (e.g. `[0x1D_FFF8, 0x1D_FFFF]` on the UAZ EDC16C39 dump). `0x10` is a
/// conservative floor that keeps every real block and drops those tiny ghosts.
const MIN_BLOCK_LEN: u32 = 0x10;

/// Detect the image's power-of-two ROM base from its directory records: the most
/// common `addr & !(len - 1)` over every `FADECAFE`/`CAFEAFFE` record's encoded
/// start address. Genuine records (the overwhelming majority) agree on the real
/// base; a rare out-of-window ghost record is outvoted. Ties break toward the
/// smaller base for determinism — real images never tie, since every genuine
/// record shares one base.
fn detect_base(image: &[u8], len: usize) -> u32 {
    let mask = !((len as u32).wrapping_sub(1));
    let mut counts: std::collections::BTreeMap<u32, usize> = std::collections::BTreeMap::new();
    let mut i = 0usize;
    while i + 8 <= len {
        if image[i..i + 4] == MARK_CS && image[i + 4..i + 8] == MARK_CP && i >= 8 {
            if let Some(sa) = read_u32_be(image, i - 8) {
                *counts.entry(sa & mask).or_default() += 1;
            }
        }
        i += 4;
    }
    // BTreeMap iterates bases ascending; replace the best only on a strictly
    // greater count, so the smallest base with the maximum count wins.
    let mut best: Option<(u32, usize)> = None;
    for (b, c) in counts {
        match best {
            Some((_, bc)) if c <= bc => {}
            _ => best = Some((b, c)),
        }
    }
    best.map(|(b, _)| b).unwrap_or(0)
}

/// Parse the on-image checksum directory.
///
/// Scans for the `0xFADECAFE`/`0xCAFEAFFE` marker pair; the two big-endian u32
/// words immediately before it are the record's `{start, end}` (inclusive CPU
/// addresses). A record is kept only when it has the structure of a *genuine*
/// descriptor, which rejects the `FADECAFE`/`CAFEAFFE` byte sequences that occur
/// inside ordinary calibration/code data (phantom "ghost" regions). The guards,
/// validated to leave zero ghost regions across 60 reference dumps (BMW / Kia /
/// Mercedes / Audi / VW / UAZ, 512 KiB–2 MiB), are:
///
/// * **base window** — both `start` and `end` map inside the detected ROM base
///   window `[base, base + len)` (see [`detect_base`] / [`addr_to_offset`]);
///   out-of-window records — whose high address the old code *wrapped* into
///   range — are rejected, which is what eliminated the large Audi EDC16 ghosts;
/// * **word alignment** — `start % 4 == 0 && (end + 1) % 4 == 0`, rejecting the
///   off-by-one duplicate records some directories carry and a word-unaligned
///   ghost tail seen on the UAZ dump. Bounds are deliberately NOT required to be
///   page-aligned: real blocks start past a header and end mid-page (e.g.
///   `0x17FFFB`), so a page-aligned guard wrongly drops genuine blocks;
/// * **minimum size** — the region spans at least [`MIN_BLOCK_LEN`] bytes, which
///   drops the tiny 8-byte padding-gap ghosts;
/// * `start < end` and `end` in range.
///
/// Survivors are de-duplicated and returned sorted by `end` ascending so
/// [`correct_directory`] fixes inner blocks before the blocks that enclose them.
///
/// NOTE: these are *structural* guards against coincidental markers in real
/// dumps, not a cryptographic guarantee. A genuine block always sums to
/// [`EDC16_MPC_SUM_CONST`] on a stock image, so [`verify_directory`] is the
/// authoritative check, and the safest fix workflow diffs against a known-stock
/// reference (see ROADMAP).
pub fn parse_directory(image: &[u8]) -> Vec<DirBlock> {
    let len = image.len();
    let mut out: Vec<DirBlock> = Vec::new();
    if len < 16 {
        return out;
    }
    let pow2 = (len & (len - 1)) == 0;
    let base = if pow2 { detect_base(image, len) } else { 0 };
    let mut i = 0usize;
    while i + 8 <= len {
        if image[i..i + 4] == MARK_CS && image[i + 4..i + 8] == MARK_CP {
            // Record = { start@(i-8), end@(i-4), MARK_CS@i, MARK_CP@(i+4) }.
            if i >= 8 {
                if let (Some(sa), Some(ea)) = (read_u32_be(image, i - 8), read_u32_be(image, i - 4))
                {
                    if let (Some(start), Some(end)) = (
                        addr_to_offset(sa, len, base, pow2),
                        addr_to_offset(ea, len, base, pow2),
                    ) {
                        // Anti-ghost guards (see fn docs): both bounds inside the
                        // base window (handled by addr_to_offset), word-aligned,
                        // at least MIN_BLOCK_LEN bytes, sane range.
                        let aligned = start % 4 == 0 && end.wrapping_add(1) % 4 == 0;
                        let valid = start < end
                            && (end as usize) < len
                            && (end - start + 1) >= MIN_BLOCK_LEN;
                        let blk = DirBlock { start, end };
                        if aligned && valid && !out.contains(&blk) {
                            out.push(blk);
                        }
                    }
                }
            }
        }
        i += 4; // records/markers are dword-aligned
    }
    out.sort_by_key(|b| (b.end, b.start));
    out
}

/// Walk the directory and report each block's 32-bit big-endian sum against the
/// fixed constant. `ok` means the block currently sums to [`EDC16_MPC_SUM_CONST`].
pub fn verify_directory(image: &[u8]) -> Result<Vec<BlockStatus>> {
    let blocks = parse_directory(image);
    let mut out = Vec::with_capacity(blocks.len());
    for b in &blocks {
        let computed = sum32be_range(image, b.start, b.end) as u64;
        out.push(BlockStatus {
            name: format!("mpc-block@0x{:06X}", b.start),
            region: MemoryRegion::new(b.start, b.end - b.start + 1),
            stored: EDC16_MPC_SUM_CONST as u64,
            computed,
            ok: computed == EDC16_MPC_SUM_CONST as u64,
        });
    }
    Ok(out)
}

/// Walk the directory and rewrite the compensation word of every block whose sum
/// is off, so each block sums to [`EDC16_MPC_SUM_CONST`].
///
/// Blocks are processed inner-first (sorted by `end`), so a nested sub-block is
/// fixed before its enclosing block — the enclosing block's compensation word
/// (its own last dword) always lies outside any inner block, so no fix is undone.
pub fn correct_directory(image: &mut [u8]) -> Result<Vec<Fix>> {
    let blocks = parse_directory(image);
    let mut fixes = Vec::new();
    for b in &blocks {
        let store = b.store_offset() as usize;
        // Sum of the block excluding its last (compensation) dword.
        let rest = sum32be_range(image, b.start, b.end.wrapping_sub(4));
        let want = EDC16_MPC_SUM_CONST.wrapping_sub(rest);
        let old = read_u32_be(image, store)
            .ok_or_else(|| Error::Config("mpc block store offset out of range".into()))?;
        if old != want {
            image[store..store + 4].copy_from_slice(&want.to_be_bytes());
            fixes.push(Fix {
                name: format!("mpc-block@0x{:06X}", b.start),
                offset: store as u32,
                old: old as u64,
                new: want as u64,
            });
        }
    }
    Ok(fixes)
}

/// Bosch EDC16 checksum scheme.
pub struct Edc16;

impl Edc16 {
    /// The fallback block map ([`DEFAULT_BLOCKS`]), used only when no on-image
    /// directory is present (a calibration-only dump).
    pub fn blocks(&self) -> &'static [Block] {
        DEFAULT_BLOCKS
    }
}

impl ChecksumAlgo for Edc16 {
    fn id(&self) -> &str {
        "edc16"
    }

    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        // Prefer the real directory walker (MPC56x full dumps); fall back to the
        // calibration-field surface when no directory is present.
        if !parse_directory(image).is_empty() {
            return verify_directory(image);
        }
        verify_blocks(image, self.blocks())
    }

    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        if !parse_directory(image).is_empty() {
            return correct_directory(image);
        }
        correct_blocks(image, self.blocks())
    }

    /// EDC16 already corrects per block: both the on-image directory walker
    /// ([`correct_directory`]) and the fallback block table ([`correct_blocks`])
    /// recompute every block but rewrite — and emit a [`Fix`] for — only the
    /// blocks whose stored word is wrong, leaving valid blocks untouched. So
    /// "fix only the bad blocks" is exactly the standard [`correct`](Edc16::correct)
    /// path; the whole-image refusal for a calibration dump that contains an
    /// uncracked block is preserved (anti-brick: never a partial, invalid set).
    fn correct_bad_blocks(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        self.correct(image)
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
    fn crc32_bosch_roundtrip() {
        // Forward Bosch CRC (poly 0x04C11DB7, init 0). crc32_bosch("123456789")
        // == 0x89A1897F (locked in lib.rs); exercise the block path end-to-end.
        let data = b"123456789";
        let mut img = buf(data, 4);
        let b = block(
            Algo::Crc32Bosch,
            0,
            data.len() as u32,
            data.len() as u32,
            4,
            true,
        );
        assert_eq!(compute_block(&img, &b).unwrap(), Some(0x89A1_897F));
        correct_blocks(&mut img, std::slice::from_ref(&b)).unwrap();
        assert_eq!(&img[9..13], &[0x7F, 0x89, 0xA1, 0x89]); // stored little-endian
        assert!(verify_blocks(&img, std::slice::from_ref(&b)).unwrap()[0].ok);
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

    // ---- MPC56x directory engine: synthetic coverage (no fixtures) -------

    /// Build a tiny power-of-two image carrying one genuine directory record for
    /// the block `[start, end]` (file offsets, base 0). As on a real EDC16, the
    /// record `{start, end, MARK_CS, MARK_CP}` lives **inside** the region at
    /// `start + 0x34`. Bounds should be word-aligned (`start % 4 == 0 &&
    /// (end + 1) % 4 == 0`) and at least `MIN_BLOCK_LEN` bytes.
    fn mpc_image(len: usize, start: u32, end: u32) -> Vec<u8> {
        let mut img = vec![0u8; len];
        // arbitrary block data across the whole region
        for (k, b) in img[start as usize..=(end as usize)].iter_mut().enumerate() {
            *b = (k as u8).wrapping_mul(7).wrapping_add(1);
        }
        stamp_record(&mut img, start, end);
        img
    }

    /// Stamp a genuine descriptor for `[start, end]` at `start + 0x34` (marker at
    /// `start + 0x3C`). The record bytes become part of the summed region, as on
    /// a real EDC16.
    fn stamp_record(img: &mut [u8], start: u32, end: u32) {
        let rec = start as usize + 0x34;
        img[rec..rec + 4].copy_from_slice(&start.to_be_bytes());
        img[rec + 4..rec + 8].copy_from_slice(&end.to_be_bytes());
        img[rec + 8..rec + 12].copy_from_slice(&MARK_CS);
        img[rec + 12..rec + 16].copy_from_slice(&MARK_CP);
    }

    #[test]
    fn mpc_parse_correct_verify_roundtrip() {
        // 64 KiB image; page-aligned block [0x100, 0x1FF], record at 0x134.
        let (len, start, end) = (0x1_0000usize, 0x100u32, 0x1FFu32);
        let mut img = mpc_image(len, start, end);
        let blocks = parse_directory(&img);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0], DirBlock { start, end });
        assert_eq!(blocks[0].store_offset(), end - 3);

        // Freshly computed: correct writes the compensation word, then every
        // block sums to the constant and a second correct is a no-op.
        let fixes = correct_directory(&mut img).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(sum32be_range(&img, start, end), EDC16_MPC_SUM_CONST);
        assert!(verify_directory(&img).unwrap()[0].ok);
        assert!(correct_directory(&mut img).unwrap().is_empty());

        // Corrupt a data byte -> verify fails -> correct -> verify ok.
        img[(start + 8) as usize] ^= 0xFF;
        assert!(!verify_directory(&img).unwrap()[0].ok);
        assert_eq!(correct_directory(&mut img).unwrap().len(), 1);
        assert!(verify_directory(&img).unwrap()[0].ok);
    }

    #[test]
    fn mpc_nested_blocks_fixed_inner_first() {
        // Page-aligned outer [0x100,0x3FF] and inner [0x200,0x2FF]; two genuine
        // records (each at its region_start + 0x34). Correcting must leave BOTH
        // summing to the constant (inner fixed before outer).
        let len = 0x1_0000usize;
        let mut img = vec![0u8; len];
        for b in img[0x100..0x400].iter_mut() {
            *b = 0x5A;
        }
        stamp_record(&mut img, 0x100, 0x3FF); // outer, marker @0x13C
        stamp_record(&mut img, 0x200, 0x2FF); // inner, marker @0x23C

        let blocks = parse_directory(&img);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].end, 0x2FF); // inner first (sorted by end)
        correct_directory(&mut img).unwrap();
        for st in verify_directory(&img).unwrap() {
            assert!(st.ok, "{} not corrected", st.name);
        }
    }

    #[test]
    fn mpc_rejects_misaligned_duplicate_records() {
        // A genuine record plus an off-by-one duplicate (end+1 word-unaligned):
        // only the genuine one is kept.
        let len = 0x1_0000usize;
        let mut img = mpc_image(len, 0x100, 0x1FF);
        // duplicate with end = 0x200 ((end+1)=0x201: fails the word-alignment
        // guard), placed outside the region.
        img[0x80..0x84].copy_from_slice(&0x100u32.to_be_bytes());
        img[0x84..0x88].copy_from_slice(&0x200u32.to_be_bytes());
        img[0x88..0x8C].copy_from_slice(&MARK_CS);
        img[0x8C..0x90].copy_from_slice(&MARK_CP);
        let blocks = parse_directory(&img);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].end, 0x1FF);
    }

    #[test]
    fn mpc_rejects_ghost_out_of_base_window() {
        // Regression for the Audi EDC16 brick-risk bug: an embedded
        // FADECAFE/CAFEAFFE whose preceding {start,end} encode an address OUTSIDE
        // the image's base window. The old `addr & (len-1)` wrap folded it into a
        // phantom in-range region that correct() then bricked; it must be rejected.
        let len = 0x1_0000usize; // base-0 window [0, 0x10000)
                                 // Two genuine base-0 records so base detection resolves to 0.
        let mut img = vec![0u8; len];
        for b in img[0x100..0x400].iter_mut() {
            *b = 0x5A;
        }
        stamp_record(&mut img, 0x100, 0x3FF);
        stamp_record(&mut img, 0x200, 0x2FF);
        // Ghost: encode {0x1_0900, 0x1_09FF} — 0x10000 above the window.
        img[0x800..0x804].copy_from_slice(&0x1_0900u32.to_be_bytes());
        img[0x804..0x808].copy_from_slice(&0x1_09FFu32.to_be_bytes());
        img[0x808..0x80C].copy_from_slice(&MARK_CS);
        img[0x80C..0x810].copy_from_slice(&MARK_CP);

        let blocks = parse_directory(&img);
        assert_eq!(blocks.len(), 2, "out-of-window ghost must be rejected");
        assert!(blocks.iter().all(|b| b.start < 0x1_0000));
    }

    #[test]
    fn mpc_rejects_tiny_ghost() {
        // Regression for the UAZ EDC16 tiny-ghost bug: an 8-byte marker pair in
        // the padding between genuine blocks encodes a tiny in-window, word-aligned
        // region. It is below MIN_BLOCK_LEN and must not be treated as a block.
        let len = 0x1_0000usize;
        let mut img = mpc_image(len, 0x100, 0x1FF); // one genuine region
                                                    // Ghost: {0x900, 0x907} — 8 bytes, in-window, word-aligned.
        img[0x800..0x804].copy_from_slice(&0x900u32.to_be_bytes());
        img[0x804..0x808].copy_from_slice(&0x907u32.to_be_bytes());
        img[0x808..0x80C].copy_from_slice(&MARK_CS);
        img[0x80C..0x810].copy_from_slice(&MARK_CP);

        let blocks = parse_directory(&img);
        assert_eq!(blocks.len(), 1, "sub-MIN_BLOCK_LEN ghost must be rejected");
        assert_eq!(blocks[0].end, 0x1FF);
    }

    #[test]
    fn mpc_no_directory_falls_back_to_field_surface() {
        // No markers -> parse yields nothing -> Edc16 uses DEFAULT_BLOCKS and
        // refuses to correct the still-Unknown cal-only field.
        let mut img = vec![0u8; 0x8_0000];
        img[0x7BD7C..0x7BD80].copy_from_slice(&0xDBCC_ED4Du32.to_le_bytes());
        assert!(parse_directory(&img).is_empty());
        let algo = Edc16;
        let st = algo.verify(&img).unwrap();
        assert_eq!(st[0].stored, 0xDBCC_ED4D);
        assert!(!st[0].ok);
        assert!(algo.correct(&mut img).is_err());
    }

    #[test]
    fn out_of_range_block_errors() {
        let img = buf(&[1, 2, 3], 0);
        let b = block(Algo::Sum8, 0, 100, 0, 1, true);
        assert!(compute_block(&img, &b).is_err());
    }

    // ---- Reference-file tests (require the private fixtures) -------------
    //
    // Enabled only when OPENTUNE_FIXTURES points at a directory holding
    // `yaris_90_stock.ori` / `yaris_90_tuned.mod` and, optionally, the harvested
    // pairs under `edc16-pairs/<name>_ori.bin` / `<name>_mod.bin`. CI has no
    // access to these files, so the tests are #[ignore]d by default. Run with:
    //   OPENTUNE_FIXTURES=/path cargo test -p opentune-checksum -- --ignored

    /// Load `(stock, tuned)` for a `KnownField` fixture, or `None` if missing.
    fn load_pair(dir: &str, f: &KnownField) -> Option<(Vec<u8>, Vec<u8>)> {
        let (sp, tp) = if f.fixture == "yaris_90" {
            (
                format!("{dir}/yaris_90_stock.ori"),
                format!("{dir}/yaris_90_tuned.mod"),
            )
        } else {
            (
                format!("{dir}/edc16-pairs/{}_ori.bin", f.fixture),
                format!("{dir}/edc16-pairs/{}_mod.bin", f.fixture),
            )
        };
        Some((std::fs::read(sp).ok()?, std::fs::read(tp).ok()?))
    }

    /// Every harvested clean-field pair: the field location/width/order and both
    /// stored values are reproduced exactly (the VERIFIED facts), and the
    /// default engine refuses to "correct" the still-Unknown algorithm.
    #[test]
    #[ignore = "needs private EDC16 reference fixtures via OPENTUNE_FIXTURES"]
    fn reference_known_fields() {
        let dir = match std::env::var("OPENTUNE_FIXTURES") {
            Ok(d) => d,
            Err(_) => return,
        };
        let mut checked = 0;
        for f in KNOWN_FIELDS {
            let Some((stock, tuned)) = load_pair(&dir, f) else {
                continue;
            };
            assert_eq!(stock.len(), f.image_len, "{} stock len", f.fixture);
            assert_eq!(tuned.len(), f.image_len, "{} tuned len", f.fixture);
            let o = f.store_offset as usize;
            let s = u32::from_le_bytes(stock[o..o + 4].try_into().unwrap());
            let t = u32::from_le_bytes(tuned[o..o + 4].try_into().unwrap());
            assert_eq!(s, f.stored_stock, "{} stock field", f.fixture);
            assert_eq!(t, f.stored_tuned, "{} tuned field", f.fixture);

            // The stored value is a genuine avalanche field: it is NOT the XOR of
            // the changed 32-bit words (regression on the exclusion finding).
            let mut wx = 0u32;
            let mut i = 0;
            while i + 4 <= stock.len().min(tuned.len()) {
                if i != o {
                    let wo = u32::from_le_bytes(stock[i..i + 4].try_into().unwrap());
                    let wm = u32::from_le_bytes(tuned[i..i + 4].try_into().unwrap());
                    wx ^= wo ^ wm;
                }
                i += 4;
            }
            assert_ne!(
                wx,
                s ^ t,
                "{}: must not be a 32-bit XOR checksum",
                f.fixture
            );
            checked += 1;
        }
        assert!(
            checked >= 1,
            "no EDC16 fixtures found under OPENTUNE_FIXTURES"
        );
    }

    // ---- MPC56x full-dump fixtures (the cracked directory scheme) --------
    //
    // Enabled when OPENTUNE_FIXTURES/edc16-full/<name>_ori.bin and _mod.bin are
    // present. Each is a complete (boot+program+calibration) EDC16C35 / C39 read.

    /// Full-dump fixtures with their calibration-block compensation ("clean")
    /// field: offset and the big-endian stored value in the stock image.
    struct FullFixture {
        name: &'static str,
        len: usize,
        cal_field_off: u32,
        cal_field_be_ori: u32,
    }

    const FULL_FIXTURES: &[FullFixture] = &[
        FullFixture {
            name: "bmw_318d_376967",
            len: 0x20_0000,
            cal_field_off: 0x000F_DFF8,
            cal_field_be_ori: 0xD765_195A, // == 0x5A1965D7 read little-endian
        },
        FullFixture {
            name: "bmw_320d_361811",
            len: 0x10_0000,
            cal_field_off: 0x000F_EFF8,
            cal_field_be_ori: 0x8CC0_9C30, // == 0x309CC08C read little-endian
        },
        FullFixture {
            name: "kia_sorento_381743",
            len: 0x20_0000,
            cal_field_off: 0x001F_DFFC,
            cal_field_be_ori: 0xC96D_875F,
        },
        FullFixture {
            name: "kia_sorento_388042",
            len: 0x20_0000,
            cal_field_off: 0x001F_DFFC,
            cal_field_be_ori: 0x3BFF_7E44,
        },
        FullFixture {
            name: "kia_sorento_390051",
            len: 0x20_0000,
            cal_field_off: 0x001F_DFFC,
            cal_field_be_ori: 0xA750_532D,
        },
        FullFixture {
            name: "kia_sorento_394237",
            len: 0x20_0000,
            cal_field_off: 0x001F_DFFC,
            cal_field_be_ori: 0xBD81_7ACA,
        },
        FullFixture {
            name: "kia_sorento_394261",
            len: 0x20_0000,
            cal_field_off: 0x001F_DFFC,
            cal_field_be_ori: 0x2657_FE66,
        },
    ];

    fn load_full(dir: &str, name: &str) -> Option<(Vec<u8>, Vec<u8>)> {
        let o = std::fs::read(format!("{dir}/edc16-full/{name}_ori.bin")).ok()?;
        let m = std::fs::read(format!("{dir}/edc16-full/{name}_mod.bin")).ok()?;
        Some((o, m))
    }

    /// The MPC56x directory scheme, validated end-to-end on every full dump:
    /// the directory parses, EVERY block sums to the constant in BOTH stock and
    /// tuned images, the calibration compensation word is reproduced exactly, and
    /// corrupt -> correct -> verify round-trips (including re-deriving the exact
    /// stored field after zeroing it).
    #[test]
    #[ignore = "needs private EDC16 full-dump fixtures via OPENTUNE_FIXTURES"]
    fn reference_mpc_directory() {
        let dir = match std::env::var("OPENTUNE_FIXTURES") {
            Ok(d) => d,
            Err(_) => return,
        };
        let mut checked = 0;
        for f in FULL_FIXTURES {
            let Some((stock, tuned)) = load_full(&dir, f.name) else {
                continue;
            };
            assert_eq!(stock.len(), f.len, "{} stock len", f.name);
            assert_eq!(tuned.len(), f.len, "{} tuned len", f.name);

            // (1) Directory parses and is non-trivial.
            let blocks = parse_directory(&stock);
            assert!(
                blocks.len() >= 2,
                "{}: expected a multi-block directory, got {}",
                f.name,
                blocks.len()
            );

            // (2) EVERY block sums to the constant in BOTH valid images.
            for img in [&stock, &tuned] {
                for st in verify_directory(img).unwrap() {
                    assert!(
                        st.ok,
                        "{}: block {} failed in a valid image",
                        f.name, st.name
                    );
                    assert_eq!(st.computed, EDC16_MPC_SUM_CONST as u64);
                }
            }

            // (3) A valid image needs no correction (idempotent / no false fixes).
            assert!(
                correct_directory(&mut stock.clone()).unwrap().is_empty(),
                "{}: valid stock should need no fixes",
                f.name
            );

            // (4) The calibration block is in the directory and its stored
            //     compensation word is the big-endian clean field.
            assert!(
                blocks.iter().any(|b| b.store_offset() == f.cal_field_off),
                "{}: cal field 0x{:X} not a block compensation word",
                f.name,
                f.cal_field_off
            );
            let o = f.cal_field_off as usize;
            let got = u32::from_be_bytes(stock[o..o + 4].try_into().unwrap());
            assert_eq!(got, f.cal_field_be_ori, "{} cal field (BE)", f.name);

            // (5) Full round-trip: corrupt a calibration byte, verify fails,
            //     correct, verify passes again.
            let mut img = stock.clone();
            let cal = blocks
                .iter()
                .copied()
                .find(|b| b.store_offset() == f.cal_field_off)
                .unwrap();
            // A data byte inside the block, just before the compensation word
            // (avoids the directory markers that sit in the block header).
            let victim = (cal.store_offset() - 8) as usize;
            img[victim] ^= 0xFF;
            assert!(
                verify_directory(&img).unwrap().iter().any(|s| !s.ok),
                "{}: corruption must be detected",
                f.name
            );
            correct_directory(&mut img).unwrap();
            assert!(
                verify_directory(&img).unwrap().iter().all(|s| s.ok),
                "{}: correct must restore every block",
                f.name
            );

            // (6) Re-derivation: zero the stored field, correct, and the exact
            //     original big-endian stored value comes back.
            let mut img2 = stock.clone();
            img2[o..o + 4].copy_from_slice(&[0, 0, 0, 0]);
            correct_directory(&mut img2).unwrap();
            let rederived = u32::from_be_bytes(img2[o..o + 4].try_into().unwrap());
            assert_eq!(rederived, f.cal_field_be_ori, "{} re-derived field", f.name);

            // (7) The public ChecksumAlgo surface uses the directory on full dumps.
            let algo = Edc16;
            assert!(algo.verify(&stock).unwrap().iter().all(|s| s.ok));
            assert!(algo.correct(&mut stock.clone()).unwrap().is_empty());

            checked += 1;
        }
        assert!(
            checked >= 1,
            "no EDC16 full-dump fixtures under OPENTUNE_FIXTURES"
        );
    }

    /// Fixture regression on a real dump that contains embedded FADECAFE/CAFEAFFE
    /// byte sequences (ghost markers). `uaz_edc16c39_398273_ori.bin` has 17 magic
    /// occurrences. Before the base-window / min-size guards, `parse_directory`
    /// admitted a tiny 8-byte ghost at `[0x1D_FFF8, 0x1D_FFFF]` (the padding gap
    /// between two genuine blocks) whose sum ≠ constant, so `correct()` rewrote 4
    /// bytes of this stock file — a brick. It must now keep only genuine blocks
    /// (each summing to the constant) and leave the stock dump byte-for-byte
    /// unchanged. CI has no access, so `#[ignore]`d:
    ///   OPENTUNE_FIXTURES=/path/to/opentune-fixtures \
    ///     cargo test -p opentune-checksum -- --ignored --nocapture
    #[test]
    #[ignore = "needs owner EDC16 fixtures via OPENTUNE_FIXTURES"]
    fn reference_ghost_markers_rejected() {
        let dir = match std::env::var("OPENTUNE_FIXTURES") {
            Ok(d) => d,
            Err(_) => return,
        };
        let Ok(stock) = std::fs::read(format!("{dir}/edc16/uaz_edc16c39_398273_ori.bin")) else {
            return;
        };
        let blocks = parse_directory(&stock);
        // All 8 genuine regions are kept; the tiny padding-gap ghost is gone.
        assert_eq!(
            blocks.len(),
            8,
            "genuine blocks kept, ghosts rejected (got {:?})",
            blocks
        );
        let ghost = DirBlock {
            start: 0x1D_FFF8,
            end: 0x1D_FFFF,
        };
        assert!(
            !blocks.contains(&ghost),
            "the 8-byte padding-gap ghost must be rejected"
        );
        // Every surviving block must be a genuine checksum block: on a stock dump
        // each sums to the constant, so verify is all-ok and correct() is a no-op.
        let algo = Edc16;
        assert!(
            algo.verify(&stock).unwrap().iter().all(|s| s.ok),
            "every kept block must sum to the constant on a stock dump"
        );
        let before = stock.clone();
        let mut m = stock.clone();
        assert!(
            algo.correct(&mut m).unwrap().is_empty(),
            "a valid stock dump must not be modified"
        );
        assert_eq!(m, before, "stock dump left byte-for-byte unchanged");
    }

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
