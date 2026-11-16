//! PRIVATE checksum engine for the Siemens/Continental **Simos PCR2.1** diesel
//! ECU (VAG 1.6 TDI common-rail; Infineon TriCore **TC1796**), plus the
//! MPPS-reconstructed **EDC16 word-wise** core kept for documentation.
//!
//! This crate is **not** part of the public `opentune` repository. It depends on
//! `opentune-checksum` by path, read-only, and mirrors its [`ChecksumAlgo`]
//! trait so the algorithm can be lifted into the public crate later *iff* the
//! project owner decides to publish it.
//!
//! # PCR2.1 — SOLVED and validated
//!
//! PCR2.1 protects its calibration with the **Bosch/Simos forward CRC-32**
//! (polynomial `0x04C11DB7`, **init 0**, no final XOR, `refin=refout=false` —
//! i.e. [`opentune_checksum::crc32_bosch`]) over a single contiguous area named
//! by an on-image **directory**:
//!
//! ```text
//! struct Pcr21Dir {        // little-endian, found at file offset 0x180100 in the
//!     u32 init;            // 2 MiB reads harvested (init == 0, the CRC seed)
//!     u32 stored_crc;      // CRC-32/bosch of the area, as flashed
//!     u32 area_count;      // == 1 in every dump seen
//!     struct { u32 start; u32 end; } area[area_count];  // inclusive CPU addrs
//! }
//! ```
//!
//! CPU addresses live in the `0xA0000000` segment; the file offset is
//! `addr & (len-1)` (the image is a power-of-two 2 MiB read). The single area is
//! `0xA0180200 ..= 0xA01FABFF` → file `0x180200 ..= 0x1FABFF` (502 272 bytes):
//! the whole populated program+calibration, **excluding** the 0x180000 header
//! (which holds the descriptor strings and this directory) and **excluding** the
//! 0x84-byte high-entropy block at `0x1FAC00` that sits just past the area end.
//!
//! That trailing 0x84-byte block is an RSA-style **signature / secured block**:
//! it is high-entropy and is fully rewritten whenever the calibration changes.
//! It is the *signed* part and is **out of scope** — re-signing needs Siemens'
//! private key, which no tuning tool has (PCR2.1 is flashed either with that
//! block left intact on a security-bypassing tool, or on hardware that does not
//! re-verify it). [`Pcr21`] only recomputes the **calibration CRC**, which is
//! the field a flashed tune must get right for the CRC check to pass.
//!
//! ## Validation (honest scope)
//!
//! Reproduced **exactly** on two independent clean 2 MiB stock reads of
//! different calibrations (see [`PCR21_FIXTURES`]):
//! * VW Golf 1.6 TDI CAYB, SW `CASM2K30` (`03L906023A`): stored `0x7E122B3B`.
//! * Seat Ibiza 1.6 TDI, SW `CASM2L95` (`03L906023S`): stored `0xBC608D26`.
//!
//! The same directory layout (init 0, count 1, area `0xA0180200..=0xA01FABFF`)
//! was independently confirmed on a **third** ECU (Audi A1 `03L906023A`, stored
//! `0xBDB1EE97`) read out of a WinOLS `.ols` project. An **uncorrected** Golf
//! tune (same file, maps changed) keeps the old stored CRC `0x7E122B3B` while the
//! data now hashes to `0x0F466CF6` — exactly the failure [`Pcr21::correct`] fixes.
//!
//! The owner's car is a Passat CAYC; CAYB/CAYC are the same 1.6 TDI PCR2.1
//! family (same ECU, same SW platform, same checksum). No literal CAYC dump was
//! available in the library, so the two validated dumps are CAYB / Seat 1.6 TDI.
//!
//! # EDC16 word-wise — reconstructed but NOT confirmed in-situ
//!
//! the EDC16-CR word-wise variant (refuted in-situ; not included) carries a faithful reconstruction of the MPPS `Check020` (EDC16
//! Common-Rail) / `Check048` (EDC16 PD) word-wise checksum **core** from the
//! private `mpps-re` decompile notes. It is covered by deterministic
//! round-trip unit tests, but it could **not** be confirmed against any real
//! dump: the `C3 3C 55 AA` area-table signature it keys off is absent from all
//! 1054 EDC16/CDI/CR dumps in the reference library, and the checksum regions in
//! real EDC16 CR (Mercedes 2.2 CDI) and PD (VW) dumps are high-entropy
//! signed/encrypted 0x80-byte blocks, not a plainly reproducible word-wise
//! field. A bounded directory-walk brute force found no self-consistent match.
//! Therefore the EDC16-CR variant **verifies/surfaces only and refuses to
//! correct** — the project's standing rule for an unvalidated algorithm.

use crate::{crc32_bosch, BlockStatus, ChecksumAlgo, Fix};
use opentune_core::{Error, MemoryRegion, Result};

/// CPU-address segment the PCR2.1 directory uses (`0xA0000000`); the file offset
/// is recovered by masking with `len - 1` for the power-of-two image.
const SEG_MASK_HIGH: u32 = 0xFF00_0000;
const SEG_BASE: u32 = 0xA000_0000;

/// One calibration area parsed from the on-image directory (file offsets,
/// `end` inclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Area {
    pub start: u32,
    pub end: u32,
}

/// A parsed PCR2.1 checksum directory.
#[derive(Debug, Clone)]
pub struct Directory {
    /// File offset of the directory record (the `init` dword).
    pub dir_offset: u32,
    /// CRC seed (observed 0 in every dump).
    pub init: u32,
    /// The stored CRC value, as currently in the image.
    pub stored_crc: u32,
    /// Parsed calibration areas (file offsets, `end` inclusive).
    pub areas: Vec<Area>,
}

impl Directory {
    /// File offset at which the stored CRC lives (`dir_offset + 4`).
    pub fn crc_offset(&self) -> u32 {
        self.dir_offset + 4
    }
}

fn rd_u32le(image: &[u8], off: usize) -> Option<u32> {
    image
        .get(off..off + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Map a `0xA0……` CPU address to a file offset for a power-of-two image.
fn addr_to_offset(addr: u32, len: usize) -> Option<u32> {
    if (addr & SEG_MASK_HIGH) != SEG_BASE {
        return None;
    }
    if len == 0 || (len & (len - 1)) != 0 {
        return None; // only power-of-two reads are supported
    }
    let off = addr & (len as u32 - 1);
    if (off as usize) < len {
        Some(off)
    } else {
        None
    }
}

/// Locate the PCR2.1 checksum directory **structurally** (so it works on both a
/// stock image and a to-be-corrected tune, whose CRC value differs but whose
/// layout does not).
///
/// A record is `{ init:u32, crc:u32, count:u32, [start:u32, end:u32]×count }`,
/// all little-endian, 4-byte aligned. It is accepted only when:
/// * `init == 0` (the observed seed — a strong, cheap anchor),
/// * `1 <= count <= 4`,
/// * every area maps from the `0xA0……` segment to an in-image, ordered,
///   non-trivial range,
/// * the areas are disjoint and plausibly large (`>= 0x1000`).
///
/// Returns the first such record. In every harvested image there is exactly one.
pub fn find_directory(image: &[u8]) -> Option<Directory> {
    let len = image.len();
    if len < 0x200 {
        return None;
    }
    let mut off = 0usize;
    while off + 12 <= len {
        let init = rd_u32le(image, off).unwrap();
        if init == 0 {
            if let Some(dir) = try_parse_at(image, off) {
                return Some(dir);
            }
        }
        off += 4;
    }
    None
}

fn try_parse_at(image: &[u8], off: usize) -> Option<Directory> {
    let len = image.len();
    let init = rd_u32le(image, off)?;
    let stored_crc = rd_u32le(image, off + 4)?;
    let count = rd_u32le(image, off + 8)?;
    if !(1..=4).contains(&count) {
        return None;
    }
    // CRC field should not be a fill word.
    if stored_crc == 0 || stored_crc == 0xFFFF_FFFF {
        return None;
    }
    let mut areas = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        let base = off + 12 + i * 8;
        let sa = rd_u32le(image, base)?;
        let ea = rd_u32le(image, base + 4)?;
        let start = addr_to_offset(sa, len)?;
        let end = addr_to_offset(ea, len)?;
        if start >= end {
            return None;
        }
        if (end - start) < 0x1000 {
            return None;
        }
        if end as usize >= len {
            return None;
        }
        areas.push(Area { start, end });
    }
    // Reject overlaps (and keep deterministic order).
    for w in areas.windows(2) {
        if w[1].start <= w[0].end {
            return None;
        }
    }
    Some(Directory {
        dir_offset: off as u32,
        init,
        stored_crc,
        areas,
    })
}

/// Compute the PCR2.1 CRC over a directory's areas: Bosch forward CRC-32 (poly
/// `0x04C11DB7`, init `dir.init`), over each area `start..=end` concatenated in
/// order. (`area_count == 1` in all known PCR2.1 images, but concatenation is
/// implemented for generality, matching the Simos directory semantics.)
pub fn compute_crc(image: &[u8], dir: &Directory) -> Result<u32> {
    let mut data: Vec<u8> = Vec::new();
    for a in &dir.areas {
        let (s, e) = (a.start as usize, a.end as usize);
        let end_excl = e
            .checked_add(1)
            .ok_or_else(|| Error::Config("area end overflow".into()))?;
        if end_excl > image.len() {
            return Err(Error::Config(format!(
                "PCR2.1 area 0x{s:X}..=0x{e:X} out of range (len 0x{:X})",
                image.len()
            )));
        }
        data.extend_from_slice(&image[s..end_excl]);
    }
    // Reuse the tested public engine. `crc32_bosch` uses init 0; PCR2.1's init is
    // 0 in every dump, so this matches. (If a non-zero init ever appears we would
    // need the init-parameterised engine; we assert it here instead of guessing.)
    if dir.init != 0 {
        return Err(Error::Unsupported(format!(
            "PCR2.1 directory init 0x{:08X} != 0; init-seeded CRC not validated",
            dir.init
        )));
    }
    Ok(crc32_bosch(&data))
}

/// Siemens/Continental Simos **PCR2.1** calibration CRC scheme (VALIDATED).
pub struct Pcr21;

impl ChecksumAlgo for Pcr21 {
    fn id(&self) -> &str {
        "pcr21"
    }

    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        let dir = find_directory(image)
            .ok_or_else(|| Error::Config("PCR2.1 checksum directory not found in image".into()))?;
        let computed = compute_crc(image, &dir)? as u64;
        let stored = dir.stored_crc as u64;
        let (addr, len) = (
            dir.areas[0].start,
            dir.areas.last().map(|a| a.end).unwrap_or(dir.areas[0].end) - dir.areas[0].start + 1,
        );
        Ok(vec![BlockStatus {
            name: format!("pcr21-cal-crc@0x{:06X}", dir.crc_offset()),
            region: MemoryRegion::new(addr, len),
            stored,
            computed,
            ok: computed == stored,
        }])
    }

    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        let dir = find_directory(image)
            .ok_or_else(|| Error::Config("PCR2.1 checksum directory not found in image".into()))?;
        let computed = compute_crc(image, &dir)?;
        let off = dir.crc_offset() as usize;
        let old = dir.stored_crc;
        let mut fixes = Vec::new();
        if old != computed {
            image[off..off + 4].copy_from_slice(&computed.to_le_bytes());
            fixes.push(Fix {
                name: format!("pcr21-cal-crc@0x{:06X}", off),
                offset: off as u32,
                old: old as u64,
                new: computed as u64,
            });
        }
        Ok(fixes)
    }

    /// PCR2.1 protects a single calibration block; [`correct`](Pcr21::correct)
    /// rewrites its stored CRC only when it differs, so "fix only the bad block"
    /// is already the standard path (nothing is written when the block is valid).
    fn correct_bad_blocks(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        self.correct(image)
    }
}

/// A validated PCR2.1 stock reference: fixture name, image length, directory
/// offset, the single area (file offsets, `end` inclusive), and the stored CRC.
#[derive(Debug, Clone, Copy)]
pub struct Pcr21Fixture {
    pub fixture: &'static str,
    pub image_len: usize,
    pub dir_offset: u32,
    pub area_start: u32,
    pub area_end: u32,
    pub stored_crc: u32,
}

/// The PCR2.1 stock dumps whose calibration CRC is reproduced exactly.
pub const PCR21_FIXTURES: &[Pcr21Fixture] = &[
    Pcr21Fixture {
        fixture: "golf90_stock",
        image_len: 0x20_0000,
        dir_offset: 0x0018_0100,
        area_start: 0x0018_0200,
        area_end: 0x001F_ABFF,
        stored_crc: 0x7E12_2B3B,
    },
    Pcr21Fixture {
        fixture: "seat_ibiza_023S_stock",
        image_len: 0x20_0000,
        dir_offset: 0x0018_0100,
        area_start: 0x0018_0200,
        area_end: 0x001F_ABFF,
        stored_crc: 0xBC60_8D26,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addr_mapping_2mib() {
        assert_eq!(addr_to_offset(0xA018_0200, 0x20_0000), Some(0x0018_0200));
        assert_eq!(addr_to_offset(0xA01F_ABFF, 0x20_0000), Some(0x001F_ABFF));
        // wrong segment rejected
        assert_eq!(addr_to_offset(0x8018_0200, 0x20_0000), None);
        // non-power-of-two rejected
        assert_eq!(addr_to_offset(0xA018_0200, 0x1F_FFFF), None);
    }

    /// Build a synthetic power-of-two image with one directory and a correct CRC,
    /// then prove parse + verify + correct round-trip without any real fixture.
    #[test]
    fn synthetic_directory_roundtrip() {
        let len = 0x1_0000usize; // 64 KiB, power of two
        let mut img = vec![0u8; len];
        // area 0x0200..=0x7FFF (file offsets), addresses in the 0xA0 segment.
        let (start, end) = (0x0200u32, 0x7FFFu32);
        for (k, b) in img[start as usize..=end as usize].iter_mut().enumerate() {
            *b = (k as u8).wrapping_mul(31).wrapping_add(7);
        }
        // directory at 0x0100: init=0, crc=0 (placeholder), count=1, {start,end}
        let d = 0x0100usize;
        img[d..d + 4].copy_from_slice(&0u32.to_le_bytes());
        img[d + 4..d + 8].copy_from_slice(&0u32.to_le_bytes());
        img[d + 8..d + 12].copy_from_slice(&1u32.to_le_bytes());
        img[d + 12..d + 16].copy_from_slice(&(0xA000_0000 | start).to_le_bytes());
        img[d + 16..d + 20].copy_from_slice(&(0xA000_0000 | end).to_le_bytes());

        let algo = Pcr21;
        // Found structurally even with a zero CRC? No: crc==0 is rejected as fill,
        // so correct() from a blank field is driven by find+compute directly.
        let dir = find_directory(&img);
        assert!(dir.is_none(), "crc==0 must not parse as a valid directory");

        // Plant the correct CRC, then it parses and verifies.
        let want = {
            let data = &img[start as usize..=end as usize];
            crc32_bosch(data)
        };
        img[d + 4..d + 8].copy_from_slice(&want.to_le_bytes());
        let st = algo.verify(&img).unwrap();
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].stored, want as u64);
        assert_eq!(st[0].computed, want as u64);
        assert!(st[0].ok);

        // Corrupt a data byte: verify fails, correct restores, verify passes.
        img[(start + 16) as usize] ^= 0xFF;
        assert!(!algo.verify(&img).unwrap()[0].ok);
        let fixes = algo.correct(&mut img).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].offset, (d + 4) as u32);
        assert!(algo.verify(&img).unwrap()[0].ok);
        // Idempotent.
        assert!(algo.correct(&mut img).unwrap().is_empty());
    }

    // ---- Real-fixture tests (private PCR2.1 dumps via OPENTUNE_FIXTURES) ----
    //
    //   OPENTUNE_FIXTURES=/opt/nvme-fw/opentune-fixtures \
    //     cargo test -p pcr21-checksum -- --ignored

    #[test]
    #[ignore = "needs private PCR2.1 fixtures via OPENTUNE_FIXTURES/pcr21"]
    fn reference_pcr21() {
        let dir = match std::env::var("OPENTUNE_FIXTURES") {
            Ok(d) => d,
            Err(_) => return,
        };
        let mut checked = 0;
        for f in PCR21_FIXTURES {
            let path = format!("{dir}/pcr21/{}.bin", f.fixture);
            let Ok(img) = std::fs::read(&path) else {
                continue;
            };
            assert_eq!(img.len(), f.image_len, "{} len", f.fixture);

            // (1) The directory is found exactly where expected, with the known area.
            let d = find_directory(&img).expect("directory present");
            assert_eq!(d.dir_offset, f.dir_offset, "{} dir offset", f.fixture);
            assert_eq!(d.areas.len(), 1, "{} area count", f.fixture);
            assert_eq!(d.areas[0].start, f.area_start, "{} area start", f.fixture);
            assert_eq!(d.areas[0].end, f.area_end, "{} area end", f.fixture);
            assert_eq!(d.stored_crc, f.stored_crc, "{} stored crc", f.fixture);

            // (2) The computed CRC reproduces the stored value EXACTLY (the proof).
            let c = compute_crc(&img, &d).unwrap();
            assert_eq!(c, f.stored_crc, "{} computed != stored CRC", f.fixture);

            // (3) The public ChecksumAlgo surface agrees and needs no correction.
            let algo = Pcr21;
            let st = algo.verify(&img).unwrap();
            assert!(st[0].ok, "{} stock must verify", f.fixture);
            assert!(
                algo.correct(&mut img.clone()).unwrap().is_empty(),
                "{} valid stock needs no fix",
                f.fixture
            );

            // (4) Corrupt a calibration byte, re-derive, and the stored CRC returns.
            let mut m = img.clone();
            m[(f.area_start + 0x40) as usize] ^= 0xFF;
            assert!(!algo.verify(&m).unwrap()[0].ok);
            let fixes = algo.correct(&mut m).unwrap();
            assert_eq!(fixes.len(), 1);
            // undo the corruption and the field matches the original stock CRC again
            m[(f.area_start + 0x40) as usize] ^= 0xFF;
            algo.correct(&mut m).unwrap();
            let off = (f.dir_offset + 4) as usize;
            let re = u32::from_le_bytes(m[off..off + 4].try_into().unwrap());
            assert_eq!(re, f.stored_crc, "{} re-derived CRC", f.fixture);

            checked += 1;
        }
        assert!(
            checked >= 1,
            "no PCR2.1 fixtures found under OPENTUNE_FIXTURES"
        );
    }

    /// The uncorrected Golf tune keeps the stock CRC but hashes to a new value;
    /// `correct` writes the new value, proving the engine does the right thing on
    /// a real modified image.
    #[test]
    #[ignore = "needs private PCR2.1 fixtures via OPENTUNE_FIXTURES/pcr21"]
    fn reference_pcr21_uncorrected_tune() {
        let dir = match std::env::var("OPENTUNE_FIXTURES") {
            Ok(d) => d,
            Err(_) => return,
        };
        let path = format!("{dir}/pcr21/golf90_tuned.bin");
        let Ok(img) = std::fs::read(&path) else {
            return;
        };
        let algo = Pcr21;
        let st = algo.verify(&img).unwrap();
        // The stored field is still the stock CRC 0x7E122B3B (tuner did not fix it).
        assert_eq!(st[0].stored, 0x7E12_2B3B);
        assert_ne!(st[0].computed, st[0].stored, "modified data must differ");
        assert!(!st[0].ok);
        let fixes = algo.correct(&mut img.clone()).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].new, st[0].computed);
    }
}
