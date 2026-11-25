//! VAG **Simos ECM3 calibration-monitor sum** (`ecm3`) — verify-first.
//!
//! # Provenance (open reference)
//!
//! Re-derived from `bri3d/VW_Flash` (BSD-2-Clause): the algorithm and range
//! discovery from `lib/checksum.py` (`locate_ecm3_with_asw1`, `validate_ecm3`),
//! the offsets from `lib/modules/simosshared.py`, and the per-ECU CAL/ASW1 block
//! bases and lengths from `lib/modules/simos{8,10,12,122,16,18,1810,184}.py`
//! (<https://github.com/bri3d/VW_Flash/blob/master/lib/checksum.py>). Only the
//! scheme was taken; this is a fresh implementation. OpenTune is
//! GPL-3.0-or-later, which BSD-2-Clause is compatible with; the upstream notice
//! is retained per its licence:
//!
//! ```text
//!   VW_Flash       Copyright (c) 2022-2024 Brian Ledbetter et al. BSD-2-Clause.
//! ```
//!
//! # The scheme
//!
//! Besides the block CRC headers ([`crate::simos`]), the ECU's ECM3 monitor
//! keeps re-summing parts of the calibration at run time. Its record lives in
//! the **CAL block** at block offset `0x400` (all words little-endian `u32`):
//!
//! ```text
//! +0x00 u32 stored_hi   stored 64-bit sum, high word   (new layout)
//! +0x04 u32 stored_lo   stored 64-bit sum, low word
//! +0x08 u32 init_hi     initial value, high word
//! +0x0C u32 init_lo     initial value, low word
//! +0x10 u32 count       number of {start, end} address pairs
//! +0x18 {u32 start, u32 end} * count  — only in older software; newer software
//!                       leaves +0x18 zero and keeps the list in ASW1
//! +0x38 u32 stored_hi / +0x3C u32 stored_lo   ("old-school" field position,
//!                       selected when the byte at +0x38 is non-zero)
//! ```
//!
//! * **Value**: `init = init_hi << 32 | init_lo`, plus every little-endian `u32`
//!   at `start, start + 4, …` **strictly below `end`**, summed as a wide
//!   (64-bit) integer — carries from the low word propagate into the high word.
//!   It must equal `stored_hi << 32 | stored_lo`.
//! * **Address list**: at CAL `+0x418` when that word is non-zero; otherwise in
//!   the **ASW1 block** at `0x520`, or at `0x540` ("early" software) when the
//!   first address of the `0x520` list does not map into the CAL block.
//! * **Address → CAL offset**: `addr - cal_base`; if that is negative, the
//!   address is a cached (`0x80…`) alias and maps as
//!   `addr + 0x2000_0000 - cal_base`.
//!
//! # Accepted image shape (everything else is refused)
//!
//! VW_Flash runs this on **extracted programming blocks**, so this engine only
//! accepts an extracted **CAL block** of exactly one of the VW_Flash block
//! lengths, identified by its block CRC header at `+0x300` (init `0`, non-zero
//! CRC, 1..=16 areas, all inside the CAL block's own address window — this
//! fixes the CAL base, and tells Simos 8 from Simos 10, which share a length):
//!
//! | variant (VW_Flash module)   | CAL base      | CAL length | ASW1 length |
//! |-----------------------------|---------------|------------|-------------|
//! | Simos 8                     | `0xA004_0000` | `0x3C000`  | `0x17FE00`  |
//! | Simos 10                    | `0xA01C_0000` | `0x3C000`  | `0x19FA00`  |
//! | Simos 12 / 12.2             | `0xA004_0000` | `0x6FC00`  | `0xBFC00`   |
//! | Simos 16 / 18.1             | `0xA080_0000` | `0x7FC00`  | `0xFFC00`   |
//! | Simos 18.10 / 18.41         | `0xA082_0000` | `0x9FC00`  | `0xDFC00`   |
//!
//! Full flash reads are refused: their block placement differs per tool and
//! variant and no real Simos read was available to confirm one. Through the
//! [`ChecksumAlgo`] trait (one image) only CAL blocks that carry their own
//! address list can be checked; for the ASW1-list software use
//! [`verify_with_asw1`] / [`correct_with_asw1_and_reference`] with the matching
//! extracted ASW1 block.
//!
//! On top of upstream, this engine refuses (instead of reading garbage or, as
//! upstream can, recursing forever): a count outside `1..=16` (an OpenTune
//! sanity bound — upstream has none), any address that does not map into the
//! CAL block (upstream only checks the first), a range with `start >= end` or a
//! start that is not word-aligned, a range whose words would cover the stored
//! field itself, and an in-CAL list long enough to reach `+0x38` (there the
//! old-school selector byte is part of the list, so the field position is
//! undefined).
//!
//! # Validation and anti-brick stance
//!
//! **Synthetic only**: no real VAG Simos image was available, so results are
//! always [`Recognition::Consistent`] (never "supported").
//!
//! * [`correct`](Ecm3::correct) never writes: it is a no-op when the stored sum
//!   already matches and **refuses** otherwise — a mismatch cannot be told apart
//!   from a layout this implementation does not understand.
//! * [`correct_with_reference`](Ecm3::correct_with_reference) takes the owner's
//!   own **stock CAL block of the same software**. It writes only when the
//!   reference locates to the *identical* layout (variant, base, list, ranges,
//!   initial value, field position) **and the reference's stored sum verifies**
//!   — proving the algorithm and layout on that software — and only the eight
//!   bytes of the stored field change.
//! * A sum that exceeds 64 bits is reported as not matching and never written:
//!   upstream's arithmetic is unbounded there (it can never match, and its fix
//!   would fail to pack), so the ECU's behaviour is unknown.
//!
//! The CAL block CRC header at `+0x300` normally covers the ECM3 field, so after
//! correcting ECM3 the block CRC must be recomputed (VW_Flash fixes ECM3 first,
//! then the CRC). This module does not touch the CRC.

use crate::{
    BlockReport, BlockStatus, ChecksumAlgo, ChecksumKind, Fix, Recognition, Representation,
};
use opentune_core::{Error, MemoryRegion, Result};

/// CAL-block offset of the ECM3 record (new-layout stored field).
pub const ECM3_RECORD: usize = 0x400;
/// Offset of the old-school stored field relative to [`ECM3_RECORD`].
const OLDSCHOOL_DELTA: usize = 0x38;
/// CAL-block offset of an in-CAL address list.
const LIST_IN_CAL: usize = ECM3_RECORD + 0x18;
/// ASW1-block offsets of the address list (normal / early software).
const ASW1_LIST: usize = 0x520;
const ASW1_LIST_EARLY: usize = 0x540;
/// Cached-segment alias distance (`0x80…` vs `0xA0…`).
const CACHED_DELTA: i64 = 0x2000_0000;
/// CAL-block offset of the block CRC header used to identify the block.
const CRC_HEADER: usize = 0x300;
/// Most address pairs accepted (OpenTune sanity bound; upstream has none).
pub const MAX_RANGES: usize = 16;

/// One Simos variant's block geometry, from the VW_Flash module files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Variant {
    pub name: &'static str,
    pub cal_base: u32,
    pub cal_len: usize,
    pub asw1_len: usize,
}

/// The CAL/ASW1 geometries this engine accepts.
pub const VARIANTS: &[Variant] = &[
    Variant {
        name: "Simos 8",
        cal_base: 0xA004_0000,
        cal_len: 0x3_C000,
        asw1_len: 0x17_FE00,
    },
    Variant {
        name: "Simos 10",
        cal_base: 0xA01C_0000,
        cal_len: 0x3_C000,
        asw1_len: 0x19_FA00,
    },
    Variant {
        name: "Simos 12 / 12.2",
        cal_base: 0xA004_0000,
        cal_len: 0x6_FC00,
        asw1_len: 0xB_FC00,
    },
    Variant {
        name: "Simos 16 / 18.1",
        cal_base: 0xA080_0000,
        cal_len: 0x7_FC00,
        asw1_len: 0xF_FC00,
    },
    Variant {
        name: "Simos 18.10 / 18.41",
        cal_base: 0xA082_0000,
        cal_len: 0x9_FC00,
        asw1_len: 0xD_FC00,
    },
];

/// Where the address list was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListSource {
    /// CAL block `+0x418`.
    Cal,
    /// ASW1 block `0x520`.
    Asw1,
    /// ASW1 block `0x540` ("early" software).
    Asw1Early,
}

/// A fully located ECM3 record of one CAL block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub variant: Variant,
    pub source: ListSource,
    /// `(start, end)` CAL-block offsets: words at `start + 4k` with
    /// `start + 4k < end` are summed.
    pub ranges: Vec<(u32, u32)>,
    /// The initial value (`init_hi << 32 | init_lo`).
    pub init: u64,
    /// CAL-block offset of the stored field (high word first).
    pub field: u32,
}

impl Layout {
    /// The monitor sum as the ECU computes it, unbounded (see the module docs
    /// for why a value above 64 bits is never accepted).
    pub fn compute(&self, cal: &[u8]) -> u128 {
        let mut sum = self.init as u128;
        for &(s, e) in &self.ranges {
            for j in (s as usize..e as usize).step_by(4) {
                sum += u32_le(cal, j).unwrap_or(0) as u128;
            }
        }
        sum
    }

    /// The stored 64-bit value.
    pub fn stored(&self, cal: &[u8]) -> u64 {
        read_u64_hilo(cal, self.field as usize).unwrap_or(0)
    }

    fn name(&self) -> String {
        format!("ecm3@0x{:06X}", self.field)
    }

    fn region(&self) -> MemoryRegion {
        let lo = self.ranges.iter().map(|r| r.0).min().unwrap_or(0);
        let hi = self
            .ranges
            .iter()
            .map(|r| last_word_end(*r))
            .max()
            .unwrap_or(0);
        MemoryRegion::new(lo, hi - lo)
    }

    fn status(&self, cal: &[u8]) -> BlockStatus {
        let computed = self.compute(cal);
        let stored = self.stored(cal);
        BlockStatus {
            name: self.name(),
            region: self.region(),
            stored,
            computed: computed as u64,
            ok: computed == stored as u128,
        }
    }
}

fn u32_le(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

/// The stored/initial 64-bit layout: high word first, each word little-endian.
fn read_u64_hilo(b: &[u8], off: usize) -> Option<u64> {
    Some(((u32_le(b, off)? as u64) << 32) | u32_le(b, off + 4)? as u64)
}

/// Exclusive end of the bytes a range's last word reads.
fn last_word_end((s, e): (u32, u32)) -> u32 {
    s + 4 * ((e - s - 1) / 4) + 4
}

fn refuse<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Unsupported(format!("ecm3: {}", msg.into())))
}

/// Whether the block CRC header at `+0x300` describes this variant's CAL block.
fn crc_header_fits(cal: &[u8], v: &Variant) -> bool {
    let (Some(init), Some(crc), Some(&count)) = (
        u32_le(cal, CRC_HEADER),
        u32_le(cal, CRC_HEADER + 4),
        cal.get(CRC_HEADER + 8),
    ) else {
        return false;
    };
    if init != 0 || crc == 0 || !(1..=16).contains(&count) {
        return false;
    }
    if cal.get(CRC_HEADER + 9..CRC_HEADER + 12) != Some(&[0, 0, 0]) {
        return false;
    }
    let lo = v.cal_base as u64;
    let hi = lo + v.cal_len as u64;
    (0..count as usize).all(|i| {
        let at = CRC_HEADER + 12 + 8 * i;
        match (u32_le(cal, at), u32_le(cal, at + 4)) {
            (Some(s), Some(e)) => {
                let (s, e) = (s as u64, e as u64);
                lo <= s && s <= e && e < hi
            }
            _ => false,
        }
    })
}

/// The unique variant an extracted CAL block belongs to, if any.
pub fn identify(cal: &[u8]) -> Option<Variant> {
    let mut hits = VARIANTS
        .iter()
        .filter(|v| v.cal_len == cal.len() && crc_header_fits(cal, v));
    let v = *hits.next()?;
    hits.next().is_none().then_some(v)
}

enum ListError {
    /// The first address does not map into the CAL block (upstream's cue to
    /// try the early ASW1 list).
    FirstUnmapped,
    Invalid(String),
}

/// Read and validate `count` address pairs at `at` of `src`.
fn read_list(
    src: &[u8],
    at: usize,
    count: usize,
    v: &Variant,
) -> std::result::Result<Vec<(u32, u32)>, ListError> {
    let base = v.cal_base as i64;
    let map = |addr: u32| -> i64 {
        let off = addr as i64 - base;
        if off < 0 {
            addr as i64 + CACHED_DELTA - base
        } else {
            off
        }
    };
    let mut ranges = Vec::with_capacity(count);
    for i in 0..count {
        let p = at + 8 * i;
        let (Some(sa), Some(ea)) = (u32_le(src, p), u32_le(src, p + 4)) else {
            return Err(ListError::Invalid(format!(
                "address pair {i} lies past the block"
            )));
        };
        let (s, e) = (map(sa), map(ea));
        if i == 0 && s < 0 {
            return Err(ListError::FirstUnmapped);
        }
        if s < 0 || e < 0 || s >= e || s % 4 != 0 {
            return Err(ListError::Invalid(format!(
                "range {i} (0x{sa:08X}..0x{ea:08X}) does not map to an aligned, \
                 non-empty CAL range"
            )));
        }
        let r = (s as u32, e as u32);
        if last_word_end(r) as usize > v.cal_len {
            return Err(ListError::Invalid(format!(
                "range {i} (0x{sa:08X}..0x{ea:08X}) runs past the CAL block"
            )));
        }
        ranges.push(r);
    }
    Ok(ranges)
}

/// Locate the ECM3 record of an extracted CAL block. `asw1` is the matching
/// extracted ASW1 block, needed when the address list is not in the CAL.
pub fn locate(cal: &[u8], asw1: Option<&[u8]>) -> Result<Layout> {
    let Some(v) = identify(cal) else {
        return refuse(
            "not a recognised extracted Simos CAL block (exact VW_Flash CAL length plus \
             a CAL CRC header at +0x300); full reads and other shapes are refused",
        );
    };
    let count = u32_le(cal, ECM3_RECORD + 0x10).unwrap_or(0) as usize;
    if !(1..=MAX_RANGES).contains(&count) {
        return refuse(format!("range count {count} is outside 1..={MAX_RANGES}"));
    }
    let init = read_u64_hilo(cal, ECM3_RECORD + 8).unwrap_or(0);
    let invalid = |m: String| refuse::<Layout>(m);
    let (source, ranges) = if u32_le(cal, LIST_IN_CAL).unwrap_or(0) > 0 {
        if LIST_IN_CAL + 8 * count > ECM3_RECORD + OLDSCHOOL_DELTA {
            return refuse(
                "the in-CAL address list reaches the old-school field at +0x438, so the \
                 stored-field position is undefined",
            );
        }
        match read_list(cal, LIST_IN_CAL, count, &v) {
            Ok(r) => (ListSource::Cal, r),
            Err(ListError::FirstUnmapped) => {
                return invalid("the first in-CAL address does not map into the CAL".into())
            }
            Err(ListError::Invalid(m)) => return invalid(m),
        }
    } else {
        let Some(asw1) = asw1 else {
            return refuse(
                "the address list lives in ASW1 (CAL +0x418 is zero); pass the matching \
                 ASW1 block (verify_with_asw1)",
            );
        };
        if asw1.len() != v.asw1_len {
            return refuse(format!(
                "ASW1 block length 0x{:X} does not match {} (0x{:X})",
                asw1.len(),
                v.name,
                v.asw1_len
            ));
        }
        match read_list(asw1, ASW1_LIST, count, &v) {
            Ok(r) => (ListSource::Asw1, r),
            Err(ListError::Invalid(m)) => return invalid(m),
            Err(ListError::FirstUnmapped) => match read_list(asw1, ASW1_LIST_EARLY, count, &v) {
                Ok(r) => (ListSource::Asw1Early, r),
                Err(ListError::FirstUnmapped) => {
                    return invalid(
                        "neither ASW1 list (0x520 / early 0x540) maps into the CAL".into(),
                    )
                }
                Err(ListError::Invalid(m)) => return invalid(m),
            },
        }
    };
    let field = if cal[ECM3_RECORD + OLDSCHOOL_DELTA] > 0 {
        (ECM3_RECORD + OLDSCHOOL_DELTA) as u32
    } else {
        ECM3_RECORD as u32
    };
    if ranges
        .iter()
        .any(|&r| r.0 < field + 8 && field < last_word_end(r))
    {
        return refuse(format!(
            "a summed range covers the stored field at 0x{field:X}"
        ));
    }
    Ok(Layout {
        variant: v,
        source,
        ranges,
        init,
        field,
    })
}

/// Verify a CAL block whose address list lives in its matching ASW1 block.
pub fn verify_with_asw1(cal: &[u8], asw1: &[u8]) -> Result<BlockStatus> {
    Ok(locate(cal, Some(asw1))?.status(cal))
}

/// Write the image's ECM3 field, but only when `reference` (the owner's stock
/// CAL block of the same software) has the identical layout and verifies.
fn correct_by_reference(
    cal: &mut [u8],
    layout: &Layout,
    reference: &[u8],
    ref_layout: &Layout,
) -> Result<Vec<Fix>> {
    if reference.len() != cal.len() {
        return refuse("reference and image differ in length");
    }
    if layout != ref_layout {
        return refuse(
            "the reference's ECM3 layout (ranges, initial value, field) differs from the \
             image's; not the same software",
        );
    }
    if !ref_layout.status(reference).ok {
        return refuse("the reference's own ECM3 sum does not verify; nothing proves the layout");
    }
    let computed = layout.compute(cal);
    let Ok(new) = u64::try_from(computed) else {
        return refuse("the sum exceeds 64 bits; the ECU's behaviour there is unknown");
    };
    let old = layout.stored(cal);
    if new == old {
        return Ok(Vec::new());
    }
    let at = layout.field as usize;
    cal[at..at + 4].copy_from_slice(&((new >> 32) as u32).to_le_bytes());
    cal[at + 4..at + 8].copy_from_slice(&(new as u32).to_le_bytes());
    debug_assert!(layout.status(cal).ok);
    Ok(vec![Fix {
        name: layout.name(),
        offset: layout.field,
        old,
        new,
    }])
}

/// [`Ecm3::correct_with_reference`] for CAL blocks whose address list lives in
/// ASW1: `asw1` is the (unchanged) ASW1 block of the same software, and
/// `reference_cal` the owner's stock CAL block, which must verify with it.
pub fn correct_with_asw1_and_reference(
    cal: &mut [u8],
    asw1: &[u8],
    reference_cal: &[u8],
) -> Result<Vec<Fix>> {
    let layout = locate(cal, Some(asw1))?;
    let ref_layout = locate(reference_cal, Some(asw1))?;
    correct_by_reference(cal, &layout, reference_cal, &ref_layout)
}

/// The `ecm3` checksum algorithm (see the module docs).
pub struct Ecm3;

impl ChecksumAlgo for Ecm3 {
    fn id(&self) -> &str {
        "ecm3"
    }

    /// One entry for a recognised CAL block that carries its own address list;
    /// empty for anything else.
    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        Ok(locate(image, None)
            .map(|l| vec![l.status(image)])
            .unwrap_or_default())
    }

    /// Always [`Recognition::Consistent`] (synthetic-only validation), as an
    /// extracted [`Representation::ProgrammingBlock`].
    fn verify_report(
        &self,
        image: &[u8],
        _representation: Representation,
    ) -> Result<Vec<BlockReport>> {
        Ok(self
            .verify(image)?
            .into_iter()
            .map(|b| {
                BlockReport::new(
                    b,
                    Representation::ProgrammingBlock,
                    ChecksumKind::Embedded,
                    Recognition::Consistent,
                )
            })
            .collect())
    }

    /// No-op when the stored sum matches; otherwise refuses (see module docs).
    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        let layout = locate(image, None)?;
        if layout.status(image).ok {
            return Ok(Vec::new());
        }
        refuse(
            "the stored monitor sum does not match and no real Simos image has validated \
             this implementation; standalone correction refuses — use \
             correct_with_reference with your own stock CAL block of the same software",
        )
    }

    /// Rewrite the stored sum only when the reference (stock CAL block of the
    /// same software) has the identical layout and its own sum verifies.
    fn correct_with_reference(&self, image: &mut [u8], reference: &[u8]) -> Result<Vec<Fix>> {
        let layout = locate(image, None)?;
        let ref_layout = locate(reference, None)?;
        correct_by_reference(image, &layout, reference, &ref_layout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S18: Variant = VARIANTS[3];

    fn put32(img: &mut [u8], off: usize, v: u32) {
        img[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// Write the high-word-first 64-bit field.
    fn put64(img: &mut [u8], off: usize, v: u64) {
        put32(img, off, (v >> 32) as u32);
        put32(img, off + 4, v as u32);
    }

    /// A blank CAL block of `v` with a one-area CRC header at +0x300.
    fn blank_cal(v: &Variant) -> Vec<u8> {
        let mut img: Vec<u8> = (0..v.cal_len).map(|i| (i * 13 + (i >> 8)) as u8).collect();
        img[0x300..0x480].fill(0);
        put32(&mut img, 0x304, 0xDEAD_BEEF); // stored CRC (not checked here)
        img[0x308] = 1;
        put32(&mut img, 0x30C, v.cal_base + 0x400);
        put32(&mut img, 0x310, v.cal_base + v.cal_len as u32 - 1);
        img
    }

    /// A CAL block with an in-CAL list of `ranges` (raw CPU addresses), a
    /// valid stored sum at +0x400.
    fn cal_with(v: &Variant, init: u64, ranges: &[(u32, u32)]) -> Vec<u8> {
        let mut img = blank_cal(v);
        put64(&mut img, 0x408, init);
        put32(&mut img, 0x410, ranges.len() as u32);
        for (i, &(s, e)) in ranges.iter().enumerate() {
            put32(&mut img, 0x418 + 8 * i, s);
            put32(&mut img, 0x41C + 8 * i, e);
        }
        let l = locate(&img, None).unwrap();
        let c = u64::try_from(l.compute(&img)).unwrap();
        put64(&mut img, 0x400, c);
        img
    }

    /// Two ranges: one uncached (0xA0…), one cached alias (0x80…).
    fn s18_cal() -> Vec<u8> {
        cal_with(
            &S18,
            0x0000_0001_0000_0000,
            &[(0xA080_1000, 0xA080_1FFF), (0x8080_4000, 0x8080_8000)],
        )
    }

    fn ok(img: &[u8]) -> bool {
        let st = Ecm3.verify(img).unwrap();
        assert_eq!(st.len(), 1);
        st[0].ok
    }

    #[test]
    fn locates_and_validates() {
        let img = s18_cal();
        let l = locate(&img, None).unwrap();
        assert_eq!(l.variant, S18);
        assert_eq!(l.source, ListSource::Cal);
        assert_eq!(l.ranges, vec![(0x1000, 0x1FFF), (0x4000, 0x8000)]);
        assert_eq!(l.field, 0x400);
        assert!(ok(&img));
        let rep = Ecm3
            .verify_report(&img, Representation::RawFullImage)
            .unwrap();
        assert_eq!(rep.len(), 1);
        assert_eq!(rep[0].representation, Representation::ProgrammingBlock);
        assert_eq!(rep[0].recognition, Recognition::Consistent);
        assert!(!rep[0].is_supported(), "never claimed as supported");
    }

    #[test]
    fn covered_word_change_fails() {
        for off in [0x1000, 0x1FFC, 0x4000, 0x7FFC, 0x5003] {
            let mut img = s18_cal();
            img[off] ^= 0x01;
            assert!(!ok(&img), "offset 0x{off:X} is covered");
        }
    }

    #[test]
    fn change_outside_ranges_is_ignored() {
        // Gap between the ranges, the exclusive end word of range 2 (0x8000),
        // the CRC header and the block tail.
        for off in [0x2000, 0x3FFF, 0x8000, 0x8003, 0x304, 0x7_FBFF] {
            let mut img = s18_cal();
            img[off] ^= 0xFF;
            assert!(ok(&img), "offset 0x{off:X} is not covered");
        }
    }

    #[test]
    fn end_bound_is_exclusive_of_words_starting_at_end() {
        // end 0x...1FFF: the word at 0x1FFC (bytes up to 0x1FFF) is summed;
        // end 0x8000: the word at 0x8000 is not.
        let img = s18_cal();
        let l = locate(&img, None).unwrap();
        let words = |s: usize, e: usize| -> u128 {
            (s..e)
                .step_by(4)
                .map(|j| u32_le(&img, j).unwrap() as u128)
                .sum()
        };
        let expect = 0x1_0000_0000u128 + words(0x1000, 0x2000) + words(0x4000, 0x8000);
        assert_eq!(l.compute(&img), expect);
    }

    #[test]
    fn carry_from_low_into_high_word() {
        // init lo = 0xFFFF_FFF0; two words 0x10 + 0x05 → the low word wraps and
        // the high word must increment: 0x12345678_FFFFFFF0 + 0x15.
        let mut img = blank_cal(&S18);
        img[0x1000..0x1008].fill(0);
        put32(&mut img, 0x1000, 0x10);
        put32(&mut img, 0x1004, 0x05);
        put64(&mut img, 0x408, 0x1234_5678_FFFF_FFF0);
        put32(&mut img, 0x410, 1);
        put32(&mut img, 0x418, 0xA080_1000);
        put32(&mut img, 0x41C, 0xA080_1008);
        let l = locate(&img, None).unwrap();
        assert_eq!(l.compute(&img), 0x1234_5679_0000_0005);
        // A store that forgot the carry (high word unchanged) does not verify.
        put64(&mut img, 0x400, 0x1234_5678_0000_0005);
        assert!(!ok(&img));
        put64(&mut img, 0x400, 0x1234_5679_0000_0005);
        assert!(ok(&img));
        // Many words: the full sum is wide, not mod 2^32.
        let mut big = blank_cal(&S18);
        big[0x1000..0x2000].fill(0xFF); // 1024 words of 0xFFFF_FFFF
        put64(&mut big, 0x408, 0);
        put32(&mut big, 0x410, 1);
        put32(&mut big, 0x418, 0xA080_1000);
        put32(&mut big, 0x41C, 0xA080_2000);
        let l = locate(&big, None).unwrap();
        assert_eq!(l.compute(&big), 1024 * 0xFFFF_FFFFu128);
        assert_eq!(l.compute(&big), 0x3FF_FFFF_FC00);
    }

    #[test]
    fn endianness_is_pinned() {
        // Words are little-endian; the stored/initial fields are high word
        // first, each word little-endian.
        let mut img = blank_cal(&S18);
        img[0x1000..0x1004].copy_from_slice(&[0x01, 0x02, 0x03, 0x04]);
        img[0x408..0x410].copy_from_slice(&[0x11, 0x00, 0x00, 0x00, 0x22, 0x00, 0x00, 0x00]);
        put32(&mut img, 0x410, 1);
        put32(&mut img, 0x418, 0xA080_1000);
        put32(&mut img, 0x41C, 0xA080_1004);
        let l = locate(&img, None).unwrap();
        assert_eq!(l.init, 0x0000_0011_0000_0022);
        assert_eq!(l.compute(&img), 0x0000_0011_0403_0223);
        // Correct via a reference that verifies; check the written bytes.
        let mut reference = img.clone();
        put64(&mut reference, 0x400, 0x0000_0011_0403_0223);
        assert!(ok(&reference));
        let fixes = Ecm3.correct_with_reference(&mut img, &reference).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(
            &img[0x400..0x408],
            &[0x11, 0x00, 0x00, 0x00, 0x23, 0x02, 0x03, 0x04]
        );
    }

    #[test]
    fn oldschool_field_position() {
        // init 0x42_0000_0000 + one word 0x10: stored 0x0000_0042_0000_0010,
        // whose first byte (the selector at +0x438) is 0x42.
        let mut img = blank_cal(&S18);
        img[0x1000..0x1004].copy_from_slice(&0x10u32.to_le_bytes());
        put64(&mut img, 0x408, 0x42_0000_0000);
        put32(&mut img, 0x410, 1);
        put32(&mut img, 0x418, 0xA080_1000);
        put32(&mut img, 0x41C, 0xA080_1004);
        assert_eq!(locate(&img, None).unwrap().field, 0x400);
        put64(&mut img, 0x438, 0x42_0000_0010);
        assert_eq!(img[0x438], 0x42);
        let l = locate(&img, None).unwrap();
        assert_eq!(l.field, 0x438);
        assert!(ok(&img));
        // The new-layout field is then ignored.
        put64(&mut img, 0x400, 0x1234);
        assert!(ok(&img));
    }

    #[test]
    fn standalone_correct_is_noop_or_refuses() {
        let mut img = s18_cal();
        assert!(Ecm3.correct(&mut img).unwrap().is_empty());
        img[0x5000] ^= 0x10;
        let before = img.clone();
        assert!(matches!(Ecm3.correct(&mut img), Err(Error::Unsupported(_))));
        assert_eq!(img, before, "left untouched");
    }

    #[test]
    fn reference_correction() {
        let stock = s18_cal();
        let mut tuned = stock.clone();
        tuned[0x5000] = tuned[0x5000].wrapping_add(1);
        let fixes = Ecm3.correct_with_reference(&mut tuned, &stock).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].offset, 0x400);
        assert_eq!(fixes[0].new, fixes[0].old + 1);
        assert!(ok(&tuned));
        // Only the data byte and the 8-byte field differ.
        let diff: Vec<usize> = (0..tuned.len()).filter(|&i| tuned[i] != stock[i]).collect();
        assert!(diff
            .iter()
            .all(|&i| i == 0x5000 || (0x400..0x408).contains(&i)));
        // Idempotent.
        assert!(Ecm3
            .correct_with_reference(&mut tuned, &stock)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn reference_correction_refusals() {
        let stock = s18_cal();
        let mut tuned = stock.clone();
        tuned[0x5000] ^= 0x01;
        // Reference that does not verify.
        let mut bad_ref = stock.clone();
        bad_ref[0x5001] ^= 0x01;
        let before = tuned.clone();
        assert!(Ecm3.correct_with_reference(&mut tuned, &bad_ref).is_err());
        // Reference with a different layout (other ranges).
        let other = cal_with(&S18, 0x1_0000_0000, &[(0xA080_1000, 0xA080_1FFF)]);
        assert!(Ecm3.correct_with_reference(&mut tuned, &other).is_err());
        // Different initial value.
        let mut init_ref = stock.clone();
        put64(&mut init_ref, 0x408, 0x2_0000_0000);
        let c = locate(&init_ref, None).unwrap().compute(&init_ref) as u64;
        put64(&mut init_ref, 0x400, c);
        assert!(Ecm3.correct_with_reference(&mut tuned, &init_ref).is_err());
        // Different length.
        assert!(Ecm3
            .correct_with_reference(&mut tuned, &stock[..stock.len() - 4])
            .is_err());
        assert_eq!(tuned, before, "left untouched");
    }

    #[test]
    fn overflow_past_64_bits_is_never_written() {
        let mut img = blank_cal(&S18);
        img[0x1000..0x1004].fill(0xFF);
        put32(&mut img, 0x410, 1);
        put32(&mut img, 0x418, 0xA080_1000);
        put32(&mut img, 0x41C, 0xA080_1004);
        put64(&mut img, 0x408, u64::MAX - 1);
        let l = locate(&img, None).unwrap();
        assert!(l.compute(&img) > u64::MAX as u128);
        let wrapped = l.compute(&img) as u64;
        put64(&mut img, 0x400, wrapped);
        assert!(!ok(&img), "a wrapped value is not accepted");
        // A verifying reference of the same layout (word 0, sum u64::MAX - 1)
        // still does not let the overflowing sum be written.
        let mut reference = img.clone();
        reference[0x1000..0x1004].fill(0);
        put64(&mut reference, 0x400, u64::MAX - 1);
        assert!(ok(&reference));
        let before = img.clone();
        let err = Ecm3
            .correct_with_reference(&mut img, &reference)
            .unwrap_err();
        assert!(format!("{err}").contains("64 bits"), "{err}");
        assert_eq!(img, before);
    }

    #[test]
    fn unrecognised_images_are_refused() {
        let good = s18_cal();
        let with = |f: &dyn Fn(&mut Vec<u8>)| {
            let mut img = good.clone();
            f(&mut img);
            img
        };
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("full power-of-two read", vec![0u8; 0x40_0000]),
            ("trimmed", good[..good.len() - 0x100].to_vec()),
            ("no CRC header", with(&|i| i[0x308] = 0)),
            (
                "CRC header of another base",
                with(&|i| put32(i, 0x30C, 0xA004_0400)),
            ),
            ("count 0", with(&|i| put32(i, 0x410, 0))),
            ("count 17", with(&|i| put32(i, 0x410, 17))),
            (
                "range past the block",
                with(&|i| put32(i, 0x424, 0xA088_0000)),
            ),
            (
                "address below both aliases",
                with(&|i| put32(i, 0x420, 0x8000_0000)),
            ),
            ("start >= end", with(&|i| put32(i, 0x41C, 0xA080_0FFF))),
            ("misaligned start", with(&|i| put32(i, 0x418, 0xA080_1002))),
            (
                "list in ASW1, no ASW1 given",
                with(&|i| i[0x418..0x438].fill(0)),
            ),
        ];
        for (what, mut img) in cases {
            assert!(Ecm3.verify(&img).unwrap().is_empty(), "{what}");
            assert!(Ecm3.correct(&mut img).is_err(), "{what}");
            let reference = img.clone();
            assert!(
                Ecm3.correct_with_reference(&mut img, &reference).is_err(),
                "{what}"
            );
        }
    }

    #[test]
    fn ambiguous_layouts_are_refused() {
        // A range covering the stored field.
        let mut img = blank_cal(&S18);
        put32(&mut img, 0x410, 1);
        put32(&mut img, 0x418, 0xA080_0300);
        put32(&mut img, 0x41C, 0xA080_0500);
        assert!(locate(&img, None).is_err());
        // In-CAL list of 5 pairs reaches the old-school selector at +0x438.
        let mut img = blank_cal(&S18);
        put32(&mut img, 0x410, 5);
        for i in 0..5u32 {
            put32(&mut img, 0x418 + 8 * i as usize, 0xA080_1000 + 0x100 * i);
            put32(&mut img, 0x41C + 8 * i as usize, 0xA080_1080 + 0x100 * i);
        }
        assert!(locate(&img, None).is_err());
        // Four pairs end exactly at +0x438 and are fine.
        put32(&mut img, 0x410, 4);
        img[0x438..0x440].fill(0);
        assert!(locate(&img, None).is_ok());
    }

    #[test]
    fn simos8_and_simos10_are_told_apart_by_the_crc_header() {
        for v in [VARIANTS[0], VARIANTS[1]] {
            let base = v.cal_base;
            let img = cal_with(&v, 7, &[(base + 0x1000, base + 0x2000)]);
            assert_eq!(identify(&img), Some(v));
            assert!(ok(&img));
        }
        // Simos 12 / 18.10 geometries too.
        for v in [VARIANTS[2], VARIANTS[4]] {
            let base = v.cal_base;
            let img = cal_with(&v, 0, &[(base + 0x800, base + 0x9000)]);
            assert_eq!(identify(&img), Some(v));
        }
    }

    /// CAL without its own list + matching ASW1 block with the list at `at`.
    fn cal_and_asw1(at: usize, ranges: &[(u32, u32)]) -> (Vec<u8>, Vec<u8>) {
        let mut cal = blank_cal(&S18);
        put64(&mut cal, 0x408, 0xABCD);
        put32(&mut cal, 0x410, ranges.len() as u32);
        let mut asw1 = vec![0u8; S18.asw1_len];
        // The other list position holds an address that does not map.
        put32(&mut asw1, 0x520, 0x8000_0000);
        for (i, &(s, e)) in ranges.iter().enumerate() {
            put32(&mut asw1, at + 8 * i, s);
            put32(&mut asw1, at + 4 + 8 * i, e);
        }
        let c = locate(&cal, Some(&asw1)).unwrap().compute(&cal) as u64;
        put64(&mut cal, 0x400, c);
        (cal, asw1)
    }

    #[test]
    fn asw1_list_normal_and_early() {
        let r = [(0x8080_2000, 0x8080_3000)];
        let (cal, asw1) = cal_and_asw1(0x520, &r);
        assert_eq!(locate(&cal, Some(&asw1)).unwrap().source, ListSource::Asw1);
        assert!(verify_with_asw1(&cal, &asw1).unwrap().ok);
        // The trait path cannot see ASW1: nothing to verify, correction refuses.
        assert!(Ecm3.verify(&cal).unwrap().is_empty());

        let (cal, asw1) = cal_and_asw1(0x540, &r);
        let l = locate(&cal, Some(&asw1)).unwrap();
        assert_eq!(l.source, ListSource::Asw1Early);
        assert_eq!(l.ranges, vec![(0x2000, 0x3000)]);
        assert!(verify_with_asw1(&cal, &asw1).unwrap().ok);

        // Reference correction through ASW1.
        let stock = cal.clone();
        let mut tuned = cal;
        tuned[0x2800] ^= 0x40;
        assert!(!verify_with_asw1(&tuned, &asw1).unwrap().ok);
        let fixes = correct_with_asw1_and_reference(&mut tuned, &asw1, &stock).unwrap();
        assert_eq!(fixes.len(), 1);
        assert!(verify_with_asw1(&tuned, &asw1).unwrap().ok);

        // Wrong ASW1 length, and neither list mapping: refused.
        assert!(verify_with_asw1(&tuned, &asw1[..asw1.len() - 4]).is_err());
        let mut no_list = asw1.clone();
        put32(&mut no_list, 0x540, 0x8000_0000);
        assert!(verify_with_asw1(&tuned, &no_list).is_err());
    }
}
