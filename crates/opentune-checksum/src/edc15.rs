//! Bosch EDC15 ("C16x" Siemens/Infineon, little-endian) flash checksum.
//!
//! # Provenance (clean-room)
//!
//! The checksum **algorithm shape** implemented here was ported from public,
//! open-source tooling:
//!
//! * `edc15-tools` (`edc15_checksum.py`, GPL-3.0) — a dependency-free, faithful
//!   port of `EDC15P_checksum.cs::tdi41_checksum_calculate` from VAGEDCSuite;
//! * cross-checked against VAGEDCSuite's `tdi41_checksum_calculate` /
//!   `tdi41v2_checksum_search` directly, and the published EDC15 RE notes.
//!
//! OpenTune is GPL-3.0-or-later, so the GPL-3.0 source is license-compatible;
//! it is cited, not copied — this is a fresh Rust implementation. No proprietary
//! or private tooling was used.
//!
//! # What is validated here
//!
//! The **VAG TDI "v4.1(v2)" 512 KiB layout** is fully reverse-engineered and
//! validated: every protected region of a real VW/Audi/Seat/Skoda EDC15 image is
//! reproduced bit-exactly (verified against a real stock+tuned pair —
//! `audi_a4_19tdi_352254`, 6/6 regions on both the untouched and the tuned
//! dump). [`verify`](Edc15C6::verify) reports each region and
//! [`correct`](Edc15C6::correct) recomputes and rewrites each region's stored
//! word in place.
//!
//! ## The algorithm (VAG TDI v4.1v2)
//!
//! A seeded dual-16-bit rolling checksum walks a region four bytes per turn:
//! `seed_a` is XORed with a word then rotated left by the low nibble of
//! `seed_b` (feeding a carry bit into `seed_b`), `seed_b` is decremented by the
//! next word and XORed with `seed_a`; then `seed_a` takes another word,
//! `+= 0xDAAC`, `seed_b` XORs the next word and is rotated right by the low
//! nibble of `seed_a`. After the range: `seed_a += 0xDAAC - 0x8631`,
//! `seed_b ^= 0xDF9B`; the 32-bit result `(seed_b << 16) | seed_a` is stored
//! little-endian in the region's last four bytes. The first non-empty region is
//! seeded `(0, 0)`; every later region is seeded `(0x8631, 0xEFCD)`. Regions
//! filled with `0xC3` are skipped, and a stored `0xC3C3C3C3` is a "leave alone"
//! marker.
//!
//! # Mercedes EDC15C6 ("CR1", 1 MiB)
//!
//! The Mercedes EDC15C6 (e.g. Sprinter 2.2 CDI `1037351686`) uses the **same
//! loop structure** as the VAG algorithm above with **different constants**:
//! every block is seeded `(0, 0)`, the mid-loop add is `0x3962`, and the
//! finalisation is `seed_a += 0x8432`, `seed_b ^= 0x4711`; the 32-bit result
//! `(seed_b << 16) | seed_a` is stored little-endian.
//!
//! ## Provenance of the EDC15C6 constants
//!
//! Constants derived by exhaustive search against the owner's own stock and
//! tuned dumps; loop structure = the public VAG EDC15 algorithm in this crate.
//! The search treated the seeds, the mid-loop add and the finalisation as
//! unknown (finalisation solved analytically per block), and exactly one
//! parameter set reproduces every checksum slot of the stock **and** the
//! independently ECU-flashed tuned image (including the four tune-changed
//! calibration blocks), and of three further Mercedes EDC15C6 stock/tuned pairs
//! that were not used in the search. No proprietary tooling or data was used.
//!
//! ## Block layout (discovered from the data)
//!
//! Each protected block ends with the halfword `0xC058` followed by its 4-byte
//! stored checksum, and the byte after the stored word is always 256-byte
//! aligned. The checksum covers `[start, slot)` (the `0xC058` halfword
//! included, the stored word excluded). A block **starts** either directly
//! after the previous block's stored word (chained blocks, e.g. a calibration
//! header followed by its body), or — when `0xC3`/`0xFF` fill follows the
//! previous block — at the first 4 KiB-aligned offset whose first halfword is
//! not fill. A block typically opens with a magic halfword (`0x5555`
//! calibration, `0xA5A5` code patch, `0xAAAA` program), but the walk does not
//! depend on it.
//!
//! The image is only treated as EDC15C6 when it is exactly 1 MiB, the walk is
//! well-formed and at least one block already verifies (a stock image matches
//! every block; a tuned one matches every block the tune did not touch).
//! [`correct`](Edc15C6::correct) refuses (returns [`Error::Unsupported`]) when
//! the image is not recognised, or when two chained blocks both fail (the
//! block boundary between them would then be ambiguous) — never guessing.
use crate::{BlockStatus, ChecksumAlgo, Fix};
use opentune_core::{Error, MemoryRegion, Result};

/// VAG TDI v4.1(v2) 512 KiB region directory (region boundaries; each
/// `[start, end)` stores its 32-bit LE checksum in `[end-4, end)`).
const VAG_TDI_512_DIR: &[u32] = &[
    0x1_0000, 0x1_4000, 0x5_8000, 0x5_8B80, 0x6_4000, 0x7_0000, 0x7_0B80, 0x7_C000,
];
/// Validated VAG image length (512 KiB).
const VAG_IMAGE_LEN: usize = 0x8_0000;
/// Stored "leave this region alone" marker.
const SKIP: u32 = 0xC3C3_C3C3;
/// Seed applied to every region after the first non-empty one.
const SEED_A_REST: u16 = 0x8631;
const SEED_B_REST: u16 = 0xEFCD;
/// Loop/finalisation magic halfwords (VAG TDI v4.1v2).
const MID_ADD: u16 = 0xDAAC;
const FIN_SUB: u16 = 0x8631;
const FIN_ADD: u16 = 0xDAAC;
const FIN_XOR: u16 = 0xDF9B;

#[inline]
fn word(img: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*img.get(i)?, *img.get(i + 1)?]))
}

/// The public VAG EDC15 dual 16-bit accumulator loop over `[start, end)`
/// (`end` exclusive), returning the raw `(seed_a, seed_b)` state before
/// finalisation. Returns `None` on any out-of-range / misaligned range rather
/// than panicking, so a foreign image is simply not recognised.
fn edc15_loop(
    img: &[u8],
    start: usize,
    end: usize,
    mut seed_a: u16,
    mut seed_b: u16,
    mid_add: u16,
) -> Option<(u16, u16)> {
    if end <= start || !(end - start).is_multiple_of(4) || end + 1 >= img.len() {
        return None;
    }
    let mut p = start;
    loop {
        let mut carry: u16 = 0;
        seed_a ^= word(img, p)?;
        p += 2;
        let n = seed_b & 0xF;
        if n > 0 {
            seed_a = seed_a.rotate_left(n as u32);
            carry = seed_a & 1;
        }
        seed_b = seed_b.wrapping_sub(word(img, p)?);
        seed_b = seed_b.wrapping_sub(carry);
        p += 2;
        seed_b ^= seed_a;
        if p == end {
            break;
        }
        seed_a = seed_a.wrapping_sub(word(img, p)?);
        p += 2;
        seed_a = seed_a.wrapping_add(mid_add);
        seed_b ^= word(img, p)?;
        p += 2;
        let n = seed_a & 0xF;
        if n > 0 {
            seed_b = seed_b.rotate_right(n as u32);
        }
        if p == end {
            break;
        }
    }
    Some((seed_a, seed_b))
}

/// VAG TDI v4.1v2 region checksum over `[start, end)` (`end` exclusive, i.e. the
/// offset of the stored word).
fn calc_vag(img: &[u8], start: usize, end: usize, seed_a: u16, seed_b: u16) -> Option<u32> {
    let (a, b) = edc15_loop(img, start, end, seed_a, seed_b, MID_ADD)?;
    let a = a.wrapping_sub(FIN_SUB).wrapping_add(FIN_ADD);
    let b = b ^ FIN_XOR;
    Some(((b as u32) << 16) | a as u32)
}

/// Is `[start, end-4)` entirely `0xC3` fill (an "empty" region)?
fn region_empty(img: &[u8], start: usize, end: usize) -> bool {
    let body_end = end.saturating_sub(4);
    img.get(start..body_end)
        .is_some_and(|s| s.iter().all(|&b| b == 0xC3))
}

/// One walked VAG region.
struct VagRegion {
    start: usize,
    end: usize,
    stored: u32,
    computed: u32,
    empty: bool,
    skip: bool,
}

/// Walk the VAG TDI 512 KiB directory, computing each region's checksum with the
/// correct per-region seed (first non-empty region `(0,0)`, the rest
/// `(0x8631, 0xEFCD)`). Returns `None` if the image is not the VAG length or a
/// region cannot be computed (misaligned/out-of-range ⇒ not a VAG image).
fn walk_vag(img: &[u8]) -> Option<Vec<VagRegion>> {
    if img.len() != VAG_IMAGE_LEN {
        return None;
    }
    let mut out = Vec::new();
    let mut seen_nonempty = false;
    for pair in VAG_TDI_512_DIR.windows(2) {
        let start = pair[0] as usize;
        let end = pair[1] as usize;
        let (sa, sb) = if seen_nonempty {
            (SEED_A_REST, SEED_B_REST)
        } else {
            (0, 0)
        };
        if region_empty(img, start, end) {
            out.push(VagRegion {
                start,
                end,
                stored: 0,
                computed: 0,
                empty: true,
                skip: false,
            });
            continue;
        }
        let stored = word(img, end - 4)? as u32 | ((word(img, end - 2)? as u32) << 16);
        let computed = calc_vag(img, start, end - 4, sa, sb)?;
        out.push(VagRegion {
            start,
            end,
            stored,
            computed,
            empty: false,
            skip: stored == SKIP,
        });
        seen_nonempty = true;
    }
    Some(out)
}

/// Bosch EDC15P/VM **V4.1 code-block signature** (`0x67` then six `0xFF`, then
/// the ASCII `"V4.1"` without the `.`-prefix byte). It is stamped one byte after
/// the start of each signed code block.
const V41_SIGNATURE: [u8; 11] = [
    0x67, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x56, 0x34, 0x2E, 0x31,
];

/// File offsets at which the V4.1 signature sits **only** on the *compact*
/// `tdi41v2` layout (the `0xC000`-block PD engines whose checksum table is the
/// [`VAG_TDI_512_DIR`] one). On the classic 12-point `tdi41` / `tdi41_2002`
/// layouts the signatures sit at `0x50001` / `0x60001` / `0x70001` instead, so
/// those positions distinguish the layouts.
const V2_SIGNATURE_POS: [usize; 2] = [0x5_8001, 0x6_4001];

/// Does this image carry the V4.1 signature at a **tdi41v2** code-block position
/// (`0x58001` or `0x64001`)? This mirrors ZedSuite's `detectChecksumVariant`:
/// the compact v2 checksum table ([`VAG_TDI_512_DIR`]) applies **only** to files
/// whose signed code blocks live at those offsets. A classic `tdi41` /
/// `tdi41_2002` file (signatures at `0x50001`/`0x60001`/`0x70001`) uses a
/// different table, so recognising it as v2 and rewriting the v2 slots would
/// corrupt a perfectly valid file — hence this gate.
fn is_tdi41v2_layout(img: &[u8]) -> bool {
    V2_SIGNATURE_POS
        .iter()
        .any(|&p| img.get(p..p + V41_SIGNATURE.len()) == Some(&V41_SIGNATURE[..]))
}

/// Is this a genuine VAG TDI **v4.1v2** image? True only when
///
/// 1. the image carries a V4.1 signature at a v2 code-block offset
///    ([`is_tdi41v2_layout`]) — so a classic `tdi41`/`tdi41_2002` file is never
///    mistaken for v2 and silently corrupted; **and**
/// 2. the v2 directory walks cleanly; **and**
/// 3. at least one non-empty, non-skip region's stored word already equals its
///    computed checksum (a stock image matches every region; a tuned image
///    matches every region the tune did not touch).
///
/// Requirement 1 is the layout gate (mirrors ZedSuite); requirements 2–3 are the
/// original sanity checks. A non-VAG/classic/foreign image fails gate 1.
fn is_vag_tdi(img: &[u8]) -> Option<Vec<VagRegion>> {
    if !is_tdi41v2_layout(img) {
        return None;
    }
    let regions = walk_vag(img)?;
    let any_match = regions
        .iter()
        .any(|r| !r.empty && !r.skip && r.stored == r.computed);
    if any_match {
        Some(regions)
    } else {
        None
    }
}

/// EDC15C6 (Mercedes, 1 MiB) image length.
const C6_IMAGE_LEN: usize = 0x10_0000;
/// Halfword stamped directly before every EDC15C6 stored checksum.
const C6_MARKER: u16 = 0xC058;
/// The byte after an EDC15C6 stored word is always aligned to this.
const C6_SLOT_END_ALIGN: usize = 0x100;
/// A block that follows fill starts on this alignment.
const C6_START_ALIGN: usize = 0x1000;
/// EDC15C6 constants (seed `(0, 0)` for every block). Derived by exhaustive
/// search against the owner's own stock and tuned dumps; loop structure = the
/// public VAG EDC15 algorithm in this crate (see the module docs).
const C6_MID_ADD: u16 = 0x3962;
const C6_FIN_ADD: u16 = 0x8432;
const C6_FIN_XOR: u16 = 0x4711;

/// EDC15C6 block checksum over `[start, slot)`.
fn calc_c6(img: &[u8], start: usize, slot: usize) -> Option<u32> {
    let (a, b) = edc15_loop(img, start, slot, 0, 0, C6_MID_ADD)?;
    let a = a.wrapping_add(C6_FIN_ADD);
    let b = b ^ C6_FIN_XOR;
    Some(((b as u32) << 16) | a as u32)
}

/// Is the halfword at `i` erased/fill (`0xC3C3` or `0xFFFF`)?
fn c6_fill_at(img: &[u8], i: usize) -> bool {
    matches!(word(img, i), Some(0xC3C3) | Some(0xFFFF))
}

/// Every EDC15C6 stored-checksum slot: the 4 bytes after a `0xC058` halfword
/// whose end is 256-byte aligned.
fn c6_slots(img: &[u8]) -> Vec<usize> {
    if img.len() != C6_IMAGE_LEN {
        return Vec::new();
    }
    (C6_SLOT_END_ALIGN..=img.len())
        .step_by(C6_SLOT_END_ALIGN)
        .filter(|&e| word(img, e - 6) == Some(C6_MARKER))
        .map(|e| e - 4)
        .collect()
}

/// One walked EDC15C6 block: checksum over `[start, slot)`, stored at `slot`.
struct C6Block {
    start: usize,
    slot: usize,
    stored: u32,
    computed: u32,
    /// Starts directly after the previous block's stored word.
    chained: bool,
}

/// Walk the EDC15C6 block chain (see the module docs for the start rule).
/// `None` if the image is not 1 MiB, has no slots, or a block start cannot be
/// placed.
fn walk_c6(img: &[u8]) -> Option<Vec<C6Block>> {
    let slots = c6_slots(img);
    if slots.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(slots.len());
    let mut prev_end = 0usize;
    for slot in slots {
        let chained = prev_end > 0 && !c6_fill_at(img, prev_end);
        let start = if chained {
            prev_end
        } else {
            let first = prev_end.next_multiple_of(C6_START_ALIGN);
            (first..slot)
                .step_by(C6_START_ALIGN)
                .find(|&a| !c6_fill_at(img, a))?
        };
        let stored = u32::from_le_bytes(img.get(slot..slot + 4)?.try_into().ok()?);
        let computed = calc_c6(img, start, slot)?;
        out.push(C6Block {
            start,
            slot,
            stored,
            computed,
            chained,
        });
        prev_end = slot + 4;
    }
    Some(out)
}

/// Is this a recognised Mercedes EDC15C6 image? The walk must be well-formed
/// and at least one block must already verify.
fn is_c6(img: &[u8]) -> Option<Vec<C6Block>> {
    let blocks = walk_c6(img)?;
    if blocks.iter().any(|b| b.stored == b.computed) {
        Some(blocks)
    } else {
        None
    }
}

/// Two chained blocks that both fail leave their shared boundary ambiguous (a
/// coincidental `0xC058` could have split one block), so correction refuses.
fn c6_ambiguous(blocks: &[C6Block]) -> bool {
    blocks
        .windows(2)
        .any(|w| w[1].chained && w[0].stored != w[0].computed && w[1].stored != w[1].computed)
}

/// Located-but-unconfirmed EDC15C6 fields of a 1 MiB image whose block walk
/// does not validate (unknown variant): surfaced, never corrected.
fn edc15c6_fields(img: &[u8]) -> Vec<(usize, u32)> {
    c6_slots(img)
        .into_iter()
        .map(|slot| {
            let v = u32::from_le_bytes([img[slot], img[slot + 1], img[slot + 2], img[slot + 3]]);
            (slot, v)
        })
        .collect()
}

/// Bosch EDC15 checksum scheme: VAG TDI v4.1v2 (512 KiB) and Mercedes EDC15C6
/// (1 MiB), both verified and self-correcting; anything else is refused.
pub struct Edc15C6;

impl ChecksumAlgo for Edc15C6 {
    fn id(&self) -> &str {
        "edc15c6"
    }

    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        // Recognised VAG TDI v4.1v2 image: report every protected region.
        if let Some(regions) = is_vag_tdi(image) {
            let mut out = Vec::new();
            for r in regions {
                if r.empty {
                    continue;
                }
                out.push(BlockStatus {
                    name: format!("vag-region-{:#07x}", r.start),
                    region: MemoryRegion::new(r.start as u32, (r.end - r.start) as u32),
                    stored: r.stored as u64,
                    computed: r.computed as u64,
                    // a stored C3C3C3C3 skip marker is intentionally "valid"
                    ok: r.skip || r.stored == r.computed,
                });
            }
            return Ok(out);
        }
        // Recognised Mercedes EDC15C6 image: report every block.
        if let Some(blocks) = is_c6(image) {
            return Ok(blocks
                .iter()
                .map(|b| BlockStatus {
                    name: format!("edc15c6-block-{:#07x}", b.start),
                    region: MemoryRegion::new(b.start as u32, (b.slot + 4 - b.start) as u32),
                    stored: b.stored as u64,
                    computed: b.computed as u64,
                    ok: b.stored == b.computed,
                })
                .collect());
        }
        // Unrecognised 1 MiB image with 0xC058 slots: surface the located
        // fields, reported not-confirmed (the block walk did not validate).
        let mut out = Vec::new();
        for (off, stored) in edc15c6_fields(image) {
            out.push(BlockStatus {
                name: format!("edc15c6-field-{off:#07x}"),
                region: MemoryRegion::new(off as u32, 4),
                stored: stored as u64,
                computed: 0,
                ok: false, // walk not validated: cannot confirm
            });
        }
        Ok(out)
    }

    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        // Mercedes EDC15C6: correct only a recognised, unambiguous walk.
        if let Some(blocks) = is_c6(image) {
            if c6_ambiguous(&blocks) {
                return Err(Error::Unsupported(
                    "EDC15C6: two chained blocks both fail, so the block boundary is \
                     ambiguous; refusing to guess"
                        .into(),
                ));
            }
            let mut fixes = Vec::new();
            for b in blocks.iter().filter(|b| b.stored != b.computed) {
                image[b.slot..b.slot + 4].copy_from_slice(&b.computed.to_le_bytes());
                fixes.push(Fix {
                    name: format!("edc15c6-block-{:#07x}", b.start),
                    offset: b.slot as u32,
                    old: b.stored as u64,
                    new: b.computed as u64,
                });
            }
            return Ok(fixes);
        }
        // Otherwise only correct a confidently-recognised VAG TDI v4.1v2 image.
        let Some(regions) = is_vag_tdi(image) else {
            return Err(Error::Unsupported(
                "image is neither a recognised VAG TDI v4.1v2 nor a recognised Mercedes \
                 EDC15C6 layout; refusing to guess (see crate::edc15 docs)"
                    .into(),
            ));
        };
        let mut fixes = Vec::new();
        for r in regions {
            if r.empty || r.skip || r.stored == r.computed {
                continue;
            }
            let slot = r.end - 4;
            image[slot..slot + 4].copy_from_slice(&r.computed.to_le_bytes());
            fixes.push(Fix {
                name: format!("vag-region-{:#07x}", r.start),
                offset: slot as u32,
                old: r.stored as u64,
                new: r.computed as u64,
            });
        }
        Ok(fixes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal synthetic VAG 512 KiB image: fill with `0xC3`, drop some
    /// non-fill data into two regions, and write each region's correct checksum.
    fn synthetic_vag() -> Vec<u8> {
        let mut img = vec![0xC3u8; VAG_IMAGE_LEN];
        // put distinct data into the first two regions so they are not "empty"
        for (k, b) in img[0x1_0000..0x1_0100].iter_mut().enumerate() {
            *b = (k as u8) ^ 0x5A;
        }
        for (k, b) in img[0x1_4000..0x1_4100].iter_mut().enumerate() {
            *b = (k as u8).wrapping_mul(3);
        }
        // Stamp the V4.1 signature at the tdi41v2 code-block offset so the image
        // is recognised as the compact v2 layout (the recognition gate).
        img[0x5_8001..0x5_8001 + V41_SIGNATURE.len()].copy_from_slice(&V41_SIGNATURE);
        // write each region's correct checksum (walk_vag applies the right
        // per-region seed) so the image is self-consistent.
        for r in walk_vag(&img).unwrap() {
            if r.empty {
                continue;
            }
            let slot = r.end - 4;
            img[slot..slot + 4].copy_from_slice(&r.computed.to_le_bytes());
        }
        img
    }

    #[test]
    fn synthetic_image_verifies_then_needs_no_fix() {
        let img = synthetic_vag();
        let algo = Edc15C6;
        let st = algo.verify(&img).unwrap();
        assert!(!st.is_empty(), "expected reported VAG regions");
        assert!(st.iter().all(|s| s.ok), "freshly built image must verify");
        // correcting a valid image changes nothing
        assert!(algo.correct(&mut img.clone()).unwrap().is_empty());
    }

    #[test]
    fn corrupting_a_region_is_detected_and_corrected() {
        let mut img = synthetic_vag();
        // flip a data byte in the first region → its stored checksum is now stale
        img[0x1_0040] ^= 0xFF;
        let algo = Edc15C6;
        let before = algo.verify(&img).unwrap();
        assert!(
            before.iter().any(|s| !s.ok),
            "a corrupted region must read as bad"
        );
        let fixes = algo.correct(&mut img).unwrap();
        assert!(
            !fixes.is_empty(),
            "correction must rewrite the stale region"
        );
        // after correcting, everything verifies again and is idempotent
        let after = algo.verify(&img).unwrap();
        assert!(after.iter().all(|s| s.ok));
        assert!(algo.correct(&mut img.clone()).unwrap().is_empty());
    }

    #[test]
    fn non_vag_image_refuses_correction() {
        // random-ish 512 KiB blob (not a VAG layout) → must refuse, never guess
        let mut img = vec![0u8; VAG_IMAGE_LEN];
        for (i, b) in img.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(31).wrapping_add(7);
        }
        let algo = Edc15C6;
        assert!(matches!(
            algo.correct(&mut img).unwrap_err(),
            Error::Unsupported(_)
        ));
    }

    #[test]
    fn classic_tdi41_layout_is_refused_not_corrupted() {
        // Regression for the brick-risk bug: a file whose V4.1 signatures sit at
        // the CLASSIC positions (0x50001/0x60001/0x70001) is NOT the compact v2
        // layout, even though some v2 windows can coincidentally validate.
        // Applying the v2 corrector to it would overwrite valid code/cal data.
        // The layout gate must refuse such a file outright.
        let mut img = synthetic_vag(); // valid v2 image (sig @0x58001)
                                       // Remove the v2 signature and stamp the signature at the classic 0x50001
                                       // position instead. At least one v2 region still matches (e.g. the first
                                       // code region), so the OLD any_match-only gate would have accepted it.
        img[0x5_8001..0x5_8001 + V41_SIGNATURE.len()].fill(0xC3);
        img[0x5_0001..0x5_0001 + V41_SIGNATURE.len()].copy_from_slice(&V41_SIGNATURE);

        assert!(
            !is_tdi41v2_layout(&img),
            "classic-position sig must not read as v2"
        );
        let algo = Edc15C6;
        // Sanity: at least one v2 region still validates (so the refusal is due
        // to the layout gate, not an empty/no-match image).
        assert!(
            walk_vag(&img)
                .unwrap()
                .iter()
                .any(|r| !r.empty && !r.skip && r.stored == r.computed),
            "a v2 region should still coincidentally match"
        );
        assert!(
            matches!(algo.correct(&mut img).unwrap_err(), Error::Unsupported(_)),
            "a classic-layout file must be refused, never v2-corrected"
        );
    }

    /// Write the 0xC058 marker in front of `slot`.
    fn c6_mark(img: &mut [u8], slot: usize) {
        img[slot - 2..slot].copy_from_slice(&C6_MARKER.to_le_bytes());
    }

    /// Synthetic 1 MiB EDC15C6 image: `0xFF` boot area, `0xC3` fill, a
    /// calibration header + chained body, and a fill-anchored code-patch block,
    /// each with its correct checksum written.
    fn synthetic_c6() -> Vec<u8> {
        let mut img = vec![0xC3u8; C6_IMAGE_LEN];
        img[..0x1_0000].fill(0xFF);
        // header [0x10000, 0x10ffc) + chained body [0x11000, 0x1bffc)
        for (k, b) in img[0x1_0000..0x1_0100].iter_mut().enumerate() {
            *b = (k as u8).wrapping_mul(5) ^ 0x21;
        }
        img[0x1_0000..0x1_0002].copy_from_slice(&0x5555u16.to_le_bytes());
        for (k, b) in img[0x1_1000..0x1_1800].iter_mut().enumerate() {
            *b = (k as u8).wrapping_mul(13).wrapping_add(1);
        }
        // fill-anchored code patch [0x4c000, 0x4c2fc)
        for (k, b) in img[0x4_c000..0x4_c200].iter_mut().enumerate() {
            *b = (k as u8) ^ 0x96;
        }
        img[0x4_c000..0x4_c002].copy_from_slice(&0xA5A5u16.to_le_bytes());
        for slot in [0x1_0FFC, 0x1_BFFC, 0x4_C2FC] {
            c6_mark(&mut img, slot);
        }
        for b in walk_c6(&img).unwrap() {
            img[b.slot..b.slot + 4].copy_from_slice(&b.computed.to_le_bytes());
        }
        img
    }

    #[test]
    fn c6_known_answer() {
        // Pins the derived constants: an independent reference implementation
        // gives 0xA85E1F2C for this block.
        let mut img = vec![0xC3u8; C6_IMAGE_LEN];
        for (k, b) in img.iter_mut().enumerate().take(0x1_00FA).skip(0x1_0000) {
            *b = (k as u8).wrapping_mul(7);
        }
        img[0x1_0000..0x1_0002].copy_from_slice(&0x5555u16.to_le_bytes());
        c6_mark(&mut img, 0x1_00FC);
        assert_eq!(calc_c6(&img, 0x1_0000, 0x1_00FC), Some(0xA85E_1F2C));
    }

    #[test]
    fn c6_walk_finds_chained_and_fill_anchored_blocks() {
        let img = synthetic_c6();
        let blocks = walk_c6(&img).unwrap();
        let geo: Vec<(usize, usize, bool)> = blocks
            .iter()
            .map(|b| (b.start, b.slot, b.chained))
            .collect();
        assert_eq!(
            geo,
            vec![
                (0x1_0000, 0x1_0FFC, false),
                (0x1_1000, 0x1_BFFC, true),
                (0x4_C000, 0x4_C2FC, false),
            ]
        );
    }

    #[test]
    fn c6_synthetic_verifies_then_needs_no_fix() {
        let img = synthetic_c6();
        let algo = Edc15C6;
        let st = algo.verify(&img).unwrap();
        assert_eq!(st.len(), 3);
        assert!(st
            .iter()
            .all(|s| s.ok && s.name.starts_with("edc15c6-block-")));
        assert!(algo.correct(&mut img.clone()).unwrap().is_empty());
    }

    #[test]
    fn c6_corrupted_block_is_detected_and_corrected() {
        let mut img = synthetic_c6();
        img[0x1_1234] ^= 0x5A; // body data changed, stored word now stale
        let algo = Edc15C6;
        assert_eq!(
            algo.verify(&img).unwrap().iter().filter(|s| !s.ok).count(),
            1
        );
        let fixes = algo.correct(&mut img).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].offset, 0x1_BFFC);
        assert!(algo.verify(&img).unwrap().iter().all(|s| s.ok));
        assert!(algo.correct(&mut img.clone()).unwrap().is_empty());
    }

    #[test]
    fn c6_two_failing_chained_blocks_are_refused() {
        // Header and its chained body both fail: the boundary is ambiguous.
        let mut img = synthetic_c6();
        img[0x1_0010] ^= 0x01;
        img[0x1_1010] ^= 0x01;
        let before = img.clone();
        assert!(matches!(
            Edc15C6.correct(&mut img).unwrap_err(),
            Error::Unsupported(_)
        ));
        assert_eq!(img, before, "a refused image is left untouched");
    }

    #[test]
    fn unrecognised_c6_field_is_located_but_refused() {
        // 1 MiB of fill with one C058-marked field and a stored word that no
        // block reproduces: surfaced as not-confirmed, correction refused.
        let mut img = vec![0xC3u8; C6_IMAGE_LEN];
        let slot = 0x0F_BFFC;
        c6_mark(&mut img, slot);
        img[slot..slot + 4].copy_from_slice(&0xB778_D5F2u32.to_le_bytes());
        let algo = Edc15C6;
        let st = algo.verify(&img).unwrap();
        assert_eq!(st.len(), 1, "one EDC15C6 field located");
        assert_eq!(st[0].stored, 0xB778_D5F2);
        assert!(!st[0].ok, "unvalidated field cannot be confirmed");
        assert!(
            matches!(algo.correct(&mut img).unwrap_err(), Error::Unsupported(_)),
            "an unrecognised EDC15C6-like image must refuse correction, never guess"
        );
    }

    /// Fixture regression on the owner's Mercedes EDC15C6 dumps (stock + an
    /// independently ECU-flashed tune, plus further stock/tuned pairs when
    /// present). CI has no access, so `#[ignore]`d. Run with:
    ///   OPENTUNE_FIXTURES=/path/to/opentune-fixtures \
    ///     cargo test -p opentune-checksum -- --ignored --nocapture
    #[test]
    #[ignore = "needs owner EDC15 fixtures via OPENTUNE_FIXTURES"]
    fn reference_mercedes_edc15c6_dumps() {
        let dir = match std::env::var("OPENTUNE_FIXTURES") {
            Ok(d) => d,
            Err(_) => return,
        };
        let algo = Edc15C6;
        let base = format!("{dir}/edc15/mb_sprinter_edc15c6_351686");
        let (Ok(ori), Ok(modf)) = (
            std::fs::read(format!("{base}_ori.bin")),
            std::fs::read(format!("{base}_mod.bin")),
        ) else {
            panic!("EDC15C6 Sprinter fixture missing under OPENTUNE_FIXTURES");
        };

        // (1) every one of the 15 blocks verifies in BOTH dumps.
        for (what, img) in [("stock", &ori), ("tuned", &modf)] {
            let st = algo.verify(img).unwrap();
            assert_eq!(st.len(), 15, "{what}: expected 15 EDC15C6 blocks");
            assert!(st.iter().all(|s| s.ok), "{what}: every block must verify");
            assert!(algo.correct(&mut img.clone()).unwrap().is_empty());
        }

        // (2) the strong proof: wipe the stored word of every block the tune
        //     changed, then correct(): it must re-derive exactly the ECU's own
        //     stored values, i.e. reproduce the real tuned dump byte-for-byte.
        let tuned: Vec<usize> = walk_c6(&modf)
            .unwrap()
            .iter()
            .filter(|b| ori[b.start..b.slot] != modf[b.start..b.slot])
            .map(|b| b.slot)
            .collect();
        assert_eq!(
            tuned.len(),
            4,
            "the tune changes the four calibration bodies"
        );
        let mut wiped = modf.clone();
        for &slot in &tuned {
            wiped[slot..slot + 4].fill(0);
        }
        assert_eq!(
            algo.verify(&wiped)
                .unwrap()
                .iter()
                .filter(|s| !s.ok)
                .count(),
            4
        );
        let fixes = algo.correct(&mut wiped).unwrap();
        assert_eq!(fixes.len(), 4);
        assert_eq!(
            wiped, modf,
            "corrected image is byte-identical to the real tune"
        );

        // (3) the stock image with the tune's calibration data (old checksums)
        //     is corrected to exactly the tuned dump.
        let mut stale = modf.clone();
        for &slot in &tuned {
            stale[slot..slot + 4].copy_from_slice(&ori[slot..slot + 4]);
        }
        assert_eq!(algo.correct(&mut stale).unwrap().len(), 4);
        assert_eq!(stale, modf);

        // (4) further owner EDC15C6 pairs, when present: every block verifies.
        for name in ["mb_c200_351737", "mb_c220_351637", "mb_c220_351736"] {
            for sfx in ["ori", "mod"] {
                let Ok(img) = std::fs::read(format!("{dir}/edc15/{name}_{sfx}.bin")) else {
                    continue;
                };
                let st = algo.verify(&img).unwrap();
                assert!(
                    st.len() >= 10,
                    "{name}_{sfx}: too few blocks ({})",
                    st.len()
                );
                assert!(
                    st.iter()
                        .all(|s| s.ok && s.name.starts_with("edc15c6-block-")),
                    "{name}_{sfx}: every block must verify"
                );
            }
        }
    }

    /// Fixtures regression against the real owner dumps. Requires the EDC15
    /// pairs under `OPENTUNE_FIXTURES/edc15/`. CI has no access, so it is
    /// `#[ignore]`d. Run with:
    ///   OPENTUNE_FIXTURES=/path/to/opentune-fixtures \
    ///     cargo test -p opentune-checksum -- --ignored --nocapture
    #[test]
    #[ignore = "needs owner EDC15 fixtures via OPENTUNE_FIXTURES"]
    fn reference_vag_audi_dump() {
        let dir = match std::env::var("OPENTUNE_FIXTURES") {
            Ok(d) => d,
            Err(_) => return,
        };
        let algo = Edc15C6;

        // The Audi A4 1.9 TDI is a real VAG TDI v4.1v2 image: every non-empty
        // region verifies in BOTH the stock and the tuned dump (the ECU wrote
        // both), and correcting the stock needs no change.
        let base = format!("{dir}/edc15/audi_a4_19tdi_352254");
        let (Ok(ori), Ok(mut modf)) = (
            std::fs::read(format!("{base}_ori.bin")),
            std::fs::read(format!("{base}_mod.bin")),
        ) else {
            panic!("audi fixture missing under OPENTUNE_FIXTURES");
        };
        assert_eq!(ori.len(), VAG_IMAGE_LEN);

        // (1) stock: all regions OK.
        let st = algo.verify(&ori).unwrap();
        let nonempty = st.len();
        assert!(
            nonempty >= 6,
            "expected >=6 reported regions, got {nonempty}"
        );
        assert!(
            st.iter().all(|s| s.ok),
            "every VAG region must verify in the stock Audi dump"
        );
        // stock needs no correction (idempotent).
        assert!(algo.correct(&mut ori.clone()).unwrap().is_empty());

        // (2) tuned: it was flashed by the ECU, so every region ALSO verifies
        //     (the tune already carries correct checksums). This proves we
        //     reproduce the ECU's own stored value for the tune-touched blocks.
        let stm = algo.verify(&modf).unwrap();
        assert!(
            stm.iter().all(|s| s.ok),
            "every VAG region must verify in the valid tuned Audi dump"
        );

        // (3) the strong proof: wipe the stored word of the tune-touched region
        //     [0x58B80, 0x64000) in the tuned image (the other regions still
        //     anchor layout detection), then correct(). Because the checksum is
        //     computed over the cal data (unchanged) and excludes the stored
        //     word, correct() must re-derive EXACTLY the ECU's own stored value,
        //     so the corrected image is byte-identical to the real tuned dump.
        let orig_mod = modf.clone();
        let slot = 0x6_3FFC; // region [0x58B80, 0x64000) end-4
        let real = u32::from_le_bytes(orig_mod[slot..slot + 4].try_into().unwrap());
        modf[slot..slot + 4].copy_from_slice(&[0, 0, 0, 0]);
        let bad = algo.verify(&modf).unwrap();
        assert!(
            bad.iter().any(|s| !s.ok),
            "a wiped checksum slot must read as bad"
        );
        let fixes = algo.correct(&mut modf).unwrap();
        assert_eq!(fixes.len(), 1, "correct() rewrites exactly the wiped slot");
        assert_eq!(
            fixes[0].new as u32, real,
            "correct() must reproduce the ECU's exact stored checksum"
        );
        assert_eq!(
            modf, orig_mod,
            "corrected image is byte-identical to the real tuned dump"
        );
    }

    /// Fixture regression on a real **classic** `tdi41` dump (brick-risk guard).
    /// `vw_golf_classic_tdi41_ori.bin` is a stock VW 1.9 TDI EDC15 whose V4.1
    /// signatures sit at the classic positions (0x50001/0x60001/0x70001). Before
    /// the layout gate, `correct()` rewrote 16 bytes of this valid file; it must
    /// now be refused. CI has no access, so `#[ignore]`d:
    ///   OPENTUNE_FIXTURES=/path/to/opentune-fixtures \
    ///     cargo test -p opentune-checksum -- --ignored --nocapture
    #[test]
    #[ignore = "needs owner EDC15 fixtures via OPENTUNE_FIXTURES"]
    fn reference_classic_tdi41_refused() {
        let dir = match std::env::var("OPENTUNE_FIXTURES") {
            Ok(d) => d,
            Err(_) => return,
        };
        let Ok(mut img) = std::fs::read(format!("{dir}/edc15/vw_golf_classic_tdi41_ori.bin"))
        else {
            return;
        };
        assert_eq!(img.len(), VAG_IMAGE_LEN);
        assert!(
            !is_tdi41v2_layout(&img),
            "the classic dump must not read as tdi41v2"
        );
        let algo = Edc15C6;
        let before = img.clone();
        assert!(
            matches!(algo.correct(&mut img).unwrap_err(), Error::Unsupported(_)),
            "a real classic tdi41 file must be refused, not corrupted"
        );
        assert_eq!(img, before, "the file must be left byte-for-byte unchanged");
    }
}
