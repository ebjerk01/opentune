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
//! # What is NOT solved: Mercedes EDC15C6 ("CR1", 1 MiB)
//!
//! The owner's Mercedes EDC15C6 dumps use the **same loop shape** but
//! **non-standard seed/finalisation constants** and a different, self-describing
//! block directory (each block ends with the halfword `0xC058`, a 4-byte stored
//! word, then `0xC3` fill). No standard VAG seed/constant reproduces those
//! blocks, and the C6-specific constants are not yet recovered. Following the
//! project rule *never write a value we cannot compute*, this module therefore
//! only **surfaces** the located EDC15C6 fields ([`verify`](Edc15C6::verify)
//! reports them as not-confirmed) and **refuses** to correct them
//! ([`correct`](Edc15C6::correct) returns [`Error::Unsupported`]). The same
//! refusal applies to any image whose blocks it cannot self-validate.
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

/// VAG TDI v4.1v2 region checksum over `[start, end)` (`end` exclusive, i.e. the
/// offset of the stored word). Returns `None` on any out-of-range / misaligned
/// range rather than panicking, so a non-VAG image is simply not recognised.
fn calc_vag(img: &[u8], start: usize, end: usize, mut seed_a: u16, mut seed_b: u16) -> Option<u32> {
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
        seed_a = seed_a.wrapping_add(MID_ADD);
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
    seed_a = seed_a.wrapping_sub(FIN_SUB).wrapping_add(FIN_ADD);
    seed_b ^= FIN_XOR;
    Some(((seed_b as u32) << 16) | seed_a as u32)
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

/// A located EDC15C6 (Mercedes, 1 MiB) checksum field: the 4 bytes after a
/// `0xC058` halfword that are immediately followed by `0xC3` fill.
fn edc15c6_fields(img: &[u8]) -> Vec<(usize, u32)> {
    let mut out = Vec::new();
    if img.len() != 0x10_0000 {
        return out;
    }
    let mut i = 0usize;
    while i + 8 <= img.len() {
        // marker 0xC058 (LE bytes 0x58 0xC0), then 4-byte slot, then 0xC3 fill
        if img[i] == 0x58 && img[i + 1] == 0xC0 && img[i + 6] == 0xC3 && img[i + 7] == 0xC3 {
            let slot = i + 2;
            let v = u32::from_le_bytes([img[slot], img[slot + 1], img[slot + 2], img[slot + 3]]);
            out.push((slot, v));
            i += 6;
        } else {
            i += 1;
        }
    }
    out
}

/// Bosch EDC15 checksum scheme. VAG TDI v4.1v2 (512 KiB) is fully solved;
/// Mercedes EDC15C6 (1 MiB) is located-only and refused (not yet solved).
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
        // Mercedes EDC15C6 (or any unrecognised image): surface located fields
        // but report them not-confirmed (the C6 algorithm is not solved).
        let mut out = Vec::new();
        for (off, stored) in edc15c6_fields(image) {
            out.push(BlockStatus {
                name: format!("edc15c6-field-{off:#07x}"),
                region: MemoryRegion::new(off as u32, 4),
                stored: stored as u64,
                computed: 0,
                ok: false, // algorithm not reverse-engineered: cannot confirm
            });
        }
        Ok(out)
    }

    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        // Only correct a confidently-recognised VAG TDI v4.1v2 image.
        let Some(regions) = is_vag_tdi(image) else {
            return Err(Error::Unsupported(
                "EDC15C6 (Mercedes CR1) checksum not reverse-engineered, and image is not a \
                 recognised VAG TDI v4.1v2 layout; refusing to guess (see crate::edc15 docs)"
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

    #[test]
    fn mercedes_edc15c6_is_located_but_refused() {
        // synthesize a 1 MiB image with one C058-marked field surrounded by C3.
        let mut img = vec![0xC3u8; 0x10_0000];
        let slot = 0x0F_BFFC;
        img[slot - 2] = 0x58; // 0xC058 marker (LE)
        img[slot - 1] = 0xC0;
        img[slot..slot + 4].copy_from_slice(&0xB778_D5F2u32.to_le_bytes());
        // followed by C3 fill already (whole image is C3)
        let algo = Edc15C6;
        let st = algo.verify(&img).unwrap();
        assert_eq!(st.len(), 1, "one EDC15C6 field located");
        assert_eq!(st[0].stored, 0xB778_D5F2);
        assert!(!st[0].ok, "EDC15C6 field cannot be confirmed (unsolved)");
        assert!(
            matches!(algo.correct(&mut img).unwrap_err(), Error::Unsupported(_)),
            "EDC15C6 must refuse correction, never guess"
        );
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
