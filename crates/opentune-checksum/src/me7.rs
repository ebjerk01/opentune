//! Bosch Motronic **ME7.x** petrol flash checksum (VAG, Porsche, Ferrari and
//! other C16x/ST10-based Motronic ECUs).
//!
//! # Provenance (open-source port)
//!
//! The checksum **algorithm shape** implemented here was re-derived from the
//! open-source **ME7Sum** tool (`360trev`/`nyetlabs`, BSD licence — see
//! `me7sum.c`, `crc32.c`). OpenTune is GPL-3.0-or-later and the BSD source is
//! licence-compatible; ME7Sum is cited, not copied — this is a fresh Rust
//! implementation, re-derived and then validated byte-for-byte against real
//! VAG ME7.5 / ME7.1 dumps (see the tests). The BSD copyright notice is retained
//! here per the licence.
//!
//! ```text
//!   ME7Sum  Copyright (c) 360trev and nyet.  BSD licence.
//!   CRC32   Copyright (C) 1986 Gary S. Brown (public domain table).
//! ```
//!
//! # The ME7 integrity scheme
//!
//! A ME7 image maps into the C16x address space at **base `0x80_0000`**: every
//! address stored in the on-image checksum tables is ROM-absolute, so the file
//! offset is `addr - 0x80_0000`. Three structures protect the ROM:
//!
//! 1. **Main ROM checksum (ROMSYS)** — two `{start, end}` ranges (inclusive,
//!    ROM-absolute) are each summed as little-endian **16-bit words** into a
//!    32-bit accumulator; the two sums are added. The 32-bit result is stored,
//!    together with its one's-complement, as an 8-byte `{value, ~value}` record
//!    at file offset **`len - 0x20`**. The classic VAG layout sums
//!    `[0x80_0000, 0x80_FBFF]` (startup/program) and `[0x82_0000, base+len-1]`
//!    (program/parameter pages), deliberately skipping the map window
//!    `0x81_0000..0x81_FFFF` (that window is covered by the multipoint table
//!    instead).
//!
//! 2. **Multipoint block** — a self-describing table of 16-byte descriptors
//!    `{start:u32, end:u32, csum:u32, ~csum:u32}` (all little-endian). Each
//!    descriptor's `csum` is the same 16-bit-word sum over its `[start, end]`
//!    ROM range. The table is terminated by a `start == 0xFFFF_FFFF` record (or
//!    simply runs to its end). The descriptors tile the **whole** ROM in 16 KiB
//!    chunks, so they protect the calibration/map pages that the main checksum
//!    skips.
//!
//! 3. **CAL CRC32** — *some* ME7 variants additionally protect the calibration
//!    range with a forward-table **CRC-32** (IEEE 802.3, reflected — the same
//!    `crate::crc32`) stored at a firmware-specific offset. The validated VAG
//!    ME7.5/ME7.1 dumps do **not** carry these (their calibration is covered by
//!    the multipoint table), and the CRC block offsets are firmware-specific
//!    (located from the ECU's own checksum routine, as ME7Sum's `.ini` files do).
//!    [`crc32_cal`] exposes the primitive so a known layout can be checked, but
//!    this module never *guesses* CRC block locations — see the anti-brick note.
//!
//! # Auto-location (no per-file config)
//!
//! Unlike ME7Sum (which reads region addresses from an `.ini`), this module
//! locates the layout from the image itself, using **data-independent**
//! signatures so that a *modified* (stale-checksum) image is still recognised
//! and corrected:
//!
//! * the **multipoint block** is found by scanning for a run of structurally
//!   valid descriptors (ROM-range `start < end`, and `csum == ~(~csum)` i.e. the
//!   stored complement is consistent — this holds even when the *data* changed);
//! * the **main ROMSYS struct** is found by its fixed address signature
//!   (`start1 == 0x80_0000`, `start2 == 0x82_0000`, `end2 == base + len - 1`),
//!   independent of the current checksum value.
//!
//! # Anti-brick stance
//!
//! An image is only treated as ME7 when a multipoint block of at least
//! [`MIN_MULTIPOINT`] valid descriptors is found; otherwise [`correct`] returns
//! [`Error::Unsupported`] and writes nothing. A region whose correct value
//! cannot be computed is never guessed. CAL-CRC32 blocks are only ever acted on
//! when their layout is known — this module does not invent one.
//!
//! [`correct`]: Me7::correct
use crate::{crc32, BlockStatus, ChecksumAlgo, Fix};
use opentune_core::{Error, MemoryRegion, Result};

/// ROM base address: ME7 maps flash into the C16x space at `0x80_0000`, so every
/// stored (ROM-absolute) address becomes a file offset by subtracting this.
const BASE: u32 = 0x80_0000;

/// Offset of the main ROMSYS `{value, ~value}` record, measured back from the
/// end of the image (`0x7F_FFE0` on a 512 KiB ROM, `0x0F_FFE0` on 1 MiB).
const MAIN_FINAL_FROM_END: usize = 0x20;

/// Start of the map/cal window that the main checksum skips and the second
/// summed range begins after (`BASE + 0x2_0000 == 0x82_0000`).
const RANGE2_START: u32 = BASE + 0x2_0000;

/// Minimum number of consecutive valid descriptors for a run to be accepted as
/// the genuine multipoint block (real blocks hold 32–64). Eight structurally
/// valid 16-byte descriptors in a row do not occur by chance.
pub const MIN_MULTIPOINT: usize = 8;

/// 16-bit-word-sum contribution of the main record's `{value, ~value}` pair.
/// The ROMSYS checksum is stored *inside* the second summed range, so summing
/// it contiguously is only self-consistent because `value + ~value` is always
/// `0xFFFF_FFFF`: its four little-endian 16-bit words therefore always sum to
/// `0xFFFF + 0xFFFF = 0x1_FFFE`, independent of the value. Excluding the record
/// and adding this constant makes the target a stable fixed point even when the
/// stored value has been wiped.
const MAIN_PAIR_CONTRIB: u32 = 0x1_FFFE;

/// Multipoint descriptor terminator (`start == 0xFFFF_FFFF`).
const TERMINATOR: u32 = 0xFFFF_FFFF;

#[inline]
fn u16le(img: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*img.get(i)?, *img.get(i + 1)?]))
}

#[inline]
fn u32le(img: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *img.get(i)?,
        *img.get(i + 1)?,
        *img.get(i + 2)?,
        *img.get(i + 3)?,
    ]))
}

/// ME7 16-bit-word checksum over the **inclusive** ROM range `[start, end]`,
/// given as file offsets. Sums little-endian 16-bit words from `start`, stepping
/// by two, including the word that contains `end` (matching ME7Sum's
/// `nIndex <= nEndAddr/2` loop). Returns `None` if the range is out of bounds.
fn word_sum(img: &[u8], start: usize, end: usize) -> Option<u32> {
    word_sum_excluding(img, start, end, 0, 0)
}

/// As [`word_sum`], but any 16-bit word whose offset lies in `[skip_lo, skip_hi)`
/// is omitted from the accumulation. Used to drop the main record's own
/// `{value, ~value}` bytes from the ROMSYS sum (see [`MAIN_PAIR_CONTRIB`]).
fn word_sum_excluding(
    img: &[u8],
    start: usize,
    end: usize,
    skip_lo: usize,
    skip_hi: usize,
) -> Option<u32> {
    if start > end || end >= img.len() {
        return None;
    }
    let mut acc: u32 = 0;
    let mut i = start;
    while i <= end {
        if !(skip_lo..skip_hi).contains(&i) {
            acc = acc.wrapping_add(u16le(img, i)? as u32);
        }
        i += 2;
    }
    Some(acc)
}

/// Standard reflected **CRC-32** (IEEE 802.3) over `data` — the primitive ME7
/// CAL-CRC32 variants use. A thin alias over [`crate::crc32`] so a caller with a
/// known CRC block layout can check/compute it; this module never auto-locates
/// such blocks (see the anti-brick note in the module docs).
pub fn crc32_cal(data: &[u8]) -> u32 {
    crc32(data)
}

/// The located main ROMSYS structure: two inclusive file-offset ranges and the
/// file offset of the 8-byte `{value, ~value}` record.
#[derive(Debug, Clone, Copy)]
struct MainStruct {
    r1: (usize, usize),
    r2: (usize, usize),
    final_off: usize,
}

impl MainStruct {
    /// The expected (correct) ROMSYS value: the combined 16-bit-word sum over
    /// both ranges, with the 8-byte `{value, ~value}` record excluded and its
    /// constant [`MAIN_PAIR_CONTRIB`] added back. Deterministic regardless of the
    /// bytes currently at the record, so it is the stable fixed point to write.
    fn expected(&self, img: &[u8]) -> Option<u32> {
        let (lo, hi) = (self.final_off, self.final_off + 8);
        let a = word_sum_excluding(img, self.r1.0, self.r1.1, lo, hi)?;
        let b = word_sum_excluding(img, self.r2.0, self.r2.1, lo, hi)?;
        Some(a.wrapping_add(b).wrapping_add(MAIN_PAIR_CONTRIB))
    }
    fn stored(&self, img: &[u8]) -> Option<u32> {
        u32le(img, self.final_off)
    }
}

/// Locate the main ROMSYS two-range struct by its fixed, data-independent
/// address signature: a 16-byte record of four little-endian ROM addresses
/// `{start1, end1, start2, end2}` with `start1 == BASE`, `start2 == 0x82_0000`,
/// `end2 == BASE + len - 1`, and `BASE < end1 < start2`. Returns the struct with
/// ranges converted to file offsets, or `None` if the signature is not present.
fn find_main(img: &[u8]) -> Option<MainStruct> {
    let len = img.len();
    if len < MAIN_FINAL_FROM_END + 8 {
        return None;
    }
    let end2_addr = BASE.wrapping_add(len as u32).wrapping_sub(1);
    let mut off = 0usize;
    while off + 16 <= len {
        if u32le(img, off) == Some(BASE) && u32le(img, off + 8) == Some(RANGE2_START) {
            if let (Some(e1), Some(e2)) = (u32le(img, off + 4), u32le(img, off + 12)) {
                if e2 == end2_addr && e1 > BASE && e1 < RANGE2_START {
                    return Some(MainStruct {
                        r1: (0usize, (e1 - BASE) as usize),
                        r2: ((RANGE2_START - BASE) as usize, (e2 - BASE) as usize),
                        final_off: len - MAIN_FINAL_FROM_END,
                    });
                }
            }
        }
        off += 2;
    }
    None
}

/// One multipoint descriptor, as file offsets.
#[derive(Debug, Clone, Copy)]
struct MpDesc {
    /// File offset of the 16-byte descriptor record.
    rec: usize,
    /// Summed range (inclusive file offsets).
    start: usize,
    end: usize,
    /// Stored checksum value.
    stored: u32,
}

impl MpDesc {
    fn compute(&self, img: &[u8]) -> Option<u32> {
        word_sum(img, self.start, self.end)
    }
}

/// Is the 16-byte record at `off` a structurally valid descriptor? Valid means
/// both addresses are inside the ROM, `start < end`, and the stored complement
/// is consistent (`csum == !inv`). The complement test holds even when the
/// summed *data* has changed, so this recognises a modified image's table.
fn parse_desc(img: &[u8], off: usize, len: usize) -> Option<MpDesc> {
    let s = u32le(img, off)?;
    let e = u32le(img, off + 4)?;
    let v = u32le(img, off + 8)?;
    let iv = u32le(img, off + 12)?;
    if s == TERMINATOR {
        return None;
    }
    if s < BASE || e < BASE || s >= e {
        return None;
    }
    let (fs, fe) = ((s - BASE) as usize, (e - BASE) as usize);
    if fe >= len {
        return None;
    }
    if v != !iv {
        return None;
    }
    Some(MpDesc {
        rec: off,
        start: fs,
        end: fe,
        stored: v,
    })
}

/// Length (in descriptors) of the run of valid descriptors starting at `off`.
fn run_len(img: &[u8], off: usize, len: usize) -> usize {
    let mut p = off;
    let mut n = 0usize;
    while p + 16 <= len {
        if parse_desc(img, p, len).is_some() {
            n += 1;
            p += 16;
        } else {
            break;
        }
    }
    n
}

/// Locate the multipoint block: the longest run of consecutive valid
/// descriptors, provided it reaches [`MIN_MULTIPOINT`]. Ties keep the lowest
/// offset. Returns the parsed descriptors, or `None` if no run qualifies.
fn find_multipoint(img: &[u8]) -> Option<Vec<MpDesc>> {
    let len = img.len();
    let mut best_off = 0usize;
    let mut best_len = 0usize;
    let mut off = 0usize;
    while off + 16 <= len {
        let n = run_len(img, off, len);
        if n > best_len {
            best_len = n;
            best_off = off;
        }
        // Skip past a found run (its interior offsets cannot start a longer run).
        off += if n > 1 { n * 16 } else { 2 };
    }
    if best_len < MIN_MULTIPOINT {
        return None;
    }
    let mut out = Vec::with_capacity(best_len);
    let mut p = best_off;
    for _ in 0..best_len {
        let d = parse_desc(img, p, len)?;
        out.push(d);
        p += 16;
    }
    Some(out)
}

/// Bosch Motronic ME7.x checksum scheme. Auto-locates and
/// verifies/corrects the main ROMSYS checksum and the multipoint block.
pub struct Me7;

impl Me7 {
    /// True when the image is confidently a ME7 layout (a multipoint block is
    /// present). Used to gate correction — an unrecognised image is refused.
    fn recognised(image: &[u8]) -> bool {
        find_multipoint(image).is_some()
    }
}

impl ChecksumAlgo for Me7 {
    fn id(&self) -> &str {
        "me7"
    }

    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        let mut out = Vec::new();
        if let Some(main) = find_main(image) {
            if let (Some(computed), Some(stored)) = (main.expected(image), main.stored(image)) {
                out.push(BlockStatus {
                    name: "me7-main-romsys".into(),
                    region: MemoryRegion::new(main.r1.0 as u32, (main.r2.1 - main.r1.0 + 1) as u32),
                    stored: stored as u64,
                    computed: computed as u64,
                    ok: stored == computed,
                });
            }
        }
        if let Some(descs) = find_multipoint(image) {
            for d in descs {
                let computed = d.compute(image).unwrap_or(0);
                out.push(BlockStatus {
                    name: format!("me7-mp@0x{:06X}", d.start),
                    region: MemoryRegion::new(d.start as u32, (d.end - d.start + 1) as u32),
                    stored: d.stored as u64,
                    computed: computed as u64,
                    ok: d.stored == computed,
                });
            }
        }
        Ok(out)
    }

    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        if !Self::recognised(image) {
            return Err(Error::Unsupported(
                "not a recognised Bosch ME7 image (no multipoint checksum block found); \
                 refusing to guess (see crate::me7 docs)"
                    .into(),
            ));
        }
        let mut fixes = Vec::new();

        // Main ROMSYS: recompute both-range word sum, write value + complement.
        if let Some(main) = find_main(image) {
            if let (Some(computed), Some(stored)) = (main.expected(image), main.stored(image)) {
                if computed != stored {
                    image[main.final_off..main.final_off + 4]
                        .copy_from_slice(&computed.to_le_bytes());
                    image[main.final_off + 4..main.final_off + 8]
                        .copy_from_slice(&(!computed).to_le_bytes());
                    fixes.push(Fix {
                        name: "me7-main-romsys".into(),
                        offset: main.final_off as u32,
                        old: stored as u64,
                        new: computed as u64,
                    });
                }
            }
        }

        // Multipoint descriptors: recompute each, write value + complement.
        if let Some(descs) = find_multipoint(image) {
            for d in descs {
                let Some(computed) = d.compute(image) else {
                    continue;
                };
                if computed != d.stored {
                    image[d.rec + 8..d.rec + 12].copy_from_slice(&computed.to_le_bytes());
                    image[d.rec + 12..d.rec + 16].copy_from_slice(&(!computed).to_le_bytes());
                    fixes.push(Fix {
                        name: format!("me7-mp@0x{:06X}", d.start),
                        offset: (d.rec + 8) as u32,
                        old: d.stored as u64,
                        new: computed as u64,
                    });
                }
            }
        }
        Ok(fixes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CRC-32 primitive sanity (the CAL-CRC32 variant uses the standard
    /// reflected CRC-32: check value of "123456789" is 0xCBF43926).
    #[test]
    fn crc32_cal_known_vector() {
        assert_eq!(crc32_cal(b"123456789"), 0xCBF4_3926);
    }

    /// Build a minimal synthetic 512 KiB ME7-shaped image: a main ROMSYS struct,
    /// its `{value, ~value}` record at `len - 0x20`, and a small multipoint block
    /// whose descriptors cover a couple of regions, all with correct sums.
    fn synthetic_me7() -> Vec<u8> {
        let len = 0x8_0000usize;
        let mut img = vec![0u8; len];
        // scatter some non-trivial data across the ROM
        for (i, b) in img.iter_mut().enumerate() {
            *b = (i as u8) ^ ((i >> 8) as u8).wrapping_mul(31);
        }
        // Main ROMSYS struct at 0x1E000: ranges [0x800000,0x80FBFF] and
        // [0x820000, base+len-1].
        let main_off = 0x1_E000usize;
        let e1 = BASE + 0xFBFF; // first range ends at 0x80_FBFF (file 0xFBFF)
        let e2 = BASE + len as u32 - 1;
        img[main_off..main_off + 4].copy_from_slice(&BASE.to_le_bytes());
        img[main_off + 4..main_off + 8].copy_from_slice(&e1.to_le_bytes());
        img[main_off + 8..main_off + 12].copy_from_slice(&RANGE2_START.to_le_bytes());
        img[main_off + 12..main_off + 16].copy_from_slice(&e2.to_le_bytes());

        // Multipoint block records at 0x1FC20: 8 descriptors tiling the data
        // window 0x840000..0x85FFFF (file 0x40000..0x5FFFF) in 16 KiB chunks.
        // The covered regions deliberately avoid the metadata (main struct at
        // 0x1E000, the block records at 0x1FC20, the main record at len-0x20) so
        // writing a checksum never alters a summed region (no self-reference).
        let mp_off = 0x1_FC20usize;
        let region0 = 0x4_0000u32; // file offset of the first tiled region
        for k in 0..8u32 {
            let s = BASE + region0 + k * 0x4000;
            let e = s + 0x3FFF;
            let rec = mp_off + (k as usize) * 16;
            img[rec..rec + 4].copy_from_slice(&s.to_le_bytes());
            img[rec + 4..rec + 8].copy_from_slice(&e.to_le_bytes());
        }
        // terminator
        let term = mp_off + 8 * 16;
        img[term..term + 4].copy_from_slice(&TERMINATOR.to_le_bytes());

        // Write each descriptor's correct checksum (regions are final data).
        for k in 0..8u32 {
            let s = (region0 + k * 0x4000) as usize;
            let e = s + 0x3FFF;
            let rec = mp_off + (k as usize) * 16;
            let cs = word_sum(&img, s, e).unwrap();
            img[rec + 8..rec + 12].copy_from_slice(&cs.to_le_bytes());
            img[rec + 12..rec + 16].copy_from_slice(&(!cs).to_le_bytes());
        }
        // main ROMSYS: write the fixed-point value + complement.
        let main = find_main(&img).unwrap();
        let cs = main.expected(&img).unwrap();
        img[main.final_off..main.final_off + 4].copy_from_slice(&cs.to_le_bytes());
        img[main.final_off + 4..main.final_off + 8].copy_from_slice(&(!cs).to_le_bytes());
        img
    }

    #[test]
    fn locates_main_and_multipoint() {
        let img = synthetic_me7();
        assert!(find_main(&img).is_some(), "main struct must be located");
        let mp = find_multipoint(&img).expect("multipoint block must be located");
        assert_eq!(mp.len(), 8, "expected 8 descriptors");
    }

    #[test]
    fn synthetic_image_verifies_and_needs_no_fix() {
        let img = synthetic_me7();
        let algo = Me7;
        let st = algo.verify(&img).unwrap();
        assert!(!st.is_empty());
        assert!(st.iter().all(|s| s.ok), "freshly built image must verify");
        assert!(algo.correct(&mut img.clone()).unwrap().is_empty());
    }

    #[test]
    fn corruption_detected_and_corrected_roundtrip() {
        let mut img = synthetic_me7();
        // flip a data byte inside the first multipoint region (file 0x40000..),
        // which also lies in the main ROMSYS second range — both the covering
        // descriptor and the main checksum go stale.
        img[0x4_0100] ^= 0xFF;
        let algo = Me7;
        assert!(
            algo.verify(&img).unwrap().iter().any(|s| !s.ok),
            "corruption must be detected"
        );
        let fixes = algo.correct(&mut img).unwrap();
        assert!(
            fixes.len() >= 2,
            "main + one multipoint block must be fixed"
        );
        // everything verifies again, and correction is idempotent
        assert!(algo.verify(&img).unwrap().iter().all(|s| s.ok));
        assert!(algo.correct(&mut img.clone()).unwrap().is_empty());
    }

    #[test]
    fn non_me7_image_is_refused() {
        let mut img = vec![0u8; 0x8_0000];
        for (i, b) in img.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(97).wrapping_add(3);
        }
        let algo = Me7;
        assert!(matches!(
            algo.correct(&mut img).unwrap_err(),
            Error::Unsupported(_)
        ));
    }

    /// Fixture regression against real owner dumps. Requires ME7 bins under
    /// `OPENTUNE_FIXTURES/me7/`. CI has no access, so it is `#[ignore]`d:
    ///   OPENTUNE_FIXTURES=/opt/nvme-fw/opentune-fixtures \
    ///     cargo test -p opentune-checksum -- --ignored --nocapture
    ///
    /// Each real stock dump must verify (every block OK); wiping the stored main
    /// record and correcting must reproduce the ECU's exact stored value.
    #[test]
    #[ignore = "needs owner ME7 fixtures via OPENTUNE_FIXTURES"]
    fn reference_me7_dumps() {
        let dir = match std::env::var("OPENTUNE_FIXTURES") {
            Ok(d) => d,
            Err(_) => return,
        };
        let algo = Me7;
        for name in [
            "passat_me75_ori.bin",
            "golf_me7510_ori.bin",
            "golf23_me71_ori.bin",
        ] {
            let path = format!("{dir}/me7/{name}");
            let Ok(img) = std::fs::read(&path) else {
                continue;
            };
            let st = algo.verify(&img).unwrap();
            assert!(
                st.len() > 8,
                "{name}: expected main + many multipoint blocks"
            );
            assert!(
                st.iter().all(|s| s.ok),
                "{name}: stock dump must fully verify"
            );
            // stock needs no correction
            assert!(algo.correct(&mut img.clone()).unwrap().is_empty(), "{name}");
            // wipe the main record, correct, and confirm the ECU's value is restored
            let main = find_main(&img).unwrap();
            let real =
                u32::from_le_bytes(img[main.final_off..main.final_off + 4].try_into().unwrap());
            let mut m = img.clone();
            m[main.final_off..main.final_off + 4].copy_from_slice(&[0, 0, 0, 0]);
            let fixes = algo.correct(&mut m).unwrap();
            assert!(
                fixes.iter().any(|f| f.new as u32 == real),
                "{name}: main restored"
            );
            assert_eq!(
                m, img,
                "{name}: corrected image is byte-identical to the dump"
            );
        }
    }
}
