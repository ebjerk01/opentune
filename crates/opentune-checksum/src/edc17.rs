//! Bosch EDC17 (Infineon TriCore) flash checksum engine — REVERSE-ENGINEERED.
//!
//! # The scheme (cracked from real full dumps)
//!
//! EDC17 is the TriCore (TC1796 / TC1797) generation. Unlike the EDC16 MPC56x
//! additive-sum-to-constant scheme, EDC17 protects each flash block with a
//! **reflected IEEE CRC-32** (the standard zlib/PNG CRC: polynomial `0xEDB88320`
//! reflected, `init = 0xFFFFFFFF`, final XOR `0xFFFFFFFF`, `refin = refout =
//! true`) that is **held equal to a fixed per-block family constant** by a
//! compensation word — exactly the EDC16 philosophy, but with a CRC instead of a
//! plain sum.
//!
//! ## Block directory
//!
//! The image carries a directory of 16-byte records, located by the SAME magic
//! pair EDC16 uses:
//!
//! ```text
//! struct DirRecord {           // little-endian (TriCore)
//!     u32 start;               // inclusive CPU address, e.g. 0x80040000
//!     u32 end;                 // inclusive CPU address, e.g. 0x8007FFFB
//!     u32 mark_cs;             // 0xFADECAFE
//!     u32 mark_cp;             // 0xCAFEAFFE
//! }
//! ```
//!
//! CPU addresses map to file offsets by stripping the segment nibble
//! (`addr & 0x00FF_FFFF`): PFLASH is mapped at `0x8000_0000` (and mirror
//! `0xA000_0000`). `end` is INCLUSIVE, so the summed range is the file bytes
//! `[start_off, end_off + 1)`.
//!
//! ## The invariant (what the ECU checks)
//!
//! For every *calibration* block the ECU verifies:
//!
//! > `crc32_iso_hdlc(image[start_off ..= end_off]) == K_block`
//!
//! where `K_block` is a constant baked into the ECU firmware (NOT stored in the
//! image — searched for, never found). The block's **last dword**
//! (`[end_off-3 ..= end_off]`, little-endian) is the compensation word: a tuning
//! tool recomputes it so the whole-block CRC lands back on `K_block`. Because
//! CRC-32 is affine, the required last dword is *uniquely determined* by the rest
//! of the block and the target `K` (a 32×32 GF(2) solve), and it reproduces the
//! real stored value exactly.
//!
//! Recovered constants (empirically, identical across every dump of a family):
//!
//! | Variant    | block (file offsets)        | K (`crc32`) |
//! |------------|-----------------------------|-------------|
//! | EDC17CP14  | `0x040000..=0x07FFFB`       | `0xA18564EC`|
//! | EDC17CP14  | `0x080000..=0x0FEEFB`       | `0xF497D557`|
//! | EDC17C08   | `0x100000..=0x11FFFB`       | `0x3092DCA9`|
//! | EDC17C08   | `0x120000..=0x177DFB`       | `0xBB1BE6EF`|
//! | EDC17CP11  | `0x020000..=0x0FFFFB`       | `0xD53B4047`|
//! | EDC17CP42  | `0x300000..=0x3FEFFB`       | `0x1C67AB6A`|
//!
//! Validation: on 15 real stock/tuned EDC17 pairs the per-block CRC is identical
//! between stock and tuned (the tune changes thousands of calibration bytes yet
//! the block CRC stays on `K`), and re-forging the last dword of the *tuned*
//! block reproduces its real stored compensation word in **26/26** changed cal
//! blocks. See `FINDINGS.md`.
//!
//! ## What is NOT solved (honest scope)
//!
//! * Small **nested "map-descriptor" sub-blocks** (e.g. CP42 `0x3005A8..`) also
//!   carry a FADECAFE record but their CRC is NOT held invariant by a last-dword
//!   comp word; they use a different (unconfirmed) rule. They are left as
//!   `Unknown` here — surfaced, never written.
//! * The VAG constant for the owner's Passat CC (CFGB) CP14 is UNKNOWN: no VAG
//!   EDC17 dump was available. The *algorithm* is identical, but `K` is
//!   firmware-specific, so a VAG stock dump is needed to pin it (or bootstrap it
//!   as `crc32(stock_block)` before editing).
//! * EDC17 OBD flashes can be RSA-signed (boot/program area). Those signatures
//!   are a separate, out-of-scope protection; the CALIBRATION CRCs above are what
//!   tuning requires and are flashed via boot/bench mode.
//!
//! This mirrors `crate::ChecksumAlgo`; drop-in as `edc17`.

#![allow(clippy::needless_range_loop)]

use crate::{BlockStatus, ChecksumAlgo, Fix};
use opentune_core::{Error, MemoryRegion, Result};

/// Reflected IEEE CRC-32 (ISO-HDLC / zlib / PNG): poly `0xEDB88320`,
/// `init = 0xFFFF_FFFF`, final XOR `0xFFFF_FFFF`, `refin = refout = true`.
///
/// This is the exact CRC EDC17 uses over its calibration blocks.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// The directory record marker pair, in file (little-endian) byte order.
const MARK_CS: [u8; 4] = [0xFE, 0xCA, 0xDE, 0xFA]; // 0xFADECAFE LE
const MARK_CP: [u8; 4] = [0xFE, 0xAF, 0xFE, 0xCA]; // 0xCAFEAFFE LE

/// One checksum-protected block parsed from the on-image directory, as file
/// offsets. `end` is INCLUSIVE. The compensation word is the last dword
/// (`[end-3 ..= end]`, little-endian).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirBlock {
    /// First byte of the summed block (file offset).
    pub start: u32,
    /// Last byte of the summed block (file offset, INCLUSIVE).
    pub end: u32,
}

impl DirBlock {
    /// File offset of the 4-byte little-endian compensation (checksum) word.
    pub fn store_offset(self) -> u32 {
        self.end - 3
    }
    /// Half-open summed range `[start, end+1)`.
    pub fn range(self) -> (usize, usize) {
        (self.start as usize, self.end as usize + 1)
    }
}

/// A validated per-block target constant for one EDC17 family.
#[derive(Debug, Clone, Copy)]
pub struct KnownConst {
    /// Family label (for diagnostics).
    pub variant: &'static str,
    /// Block start file offset.
    pub start: u32,
    /// Block end file offset (inclusive).
    pub end: u32,
    /// The constant `crc32(image[start..=end])` must equal.
    pub k: u32,
}

/// The empirically recovered, cross-dump-validated calibration constants.
///
/// Every entry was confirmed invariant between stock and tuned on real dumps and
/// its last-dword compensation reproduced exactly (see module docs / `FINDINGS.md`).
pub const KNOWN_CONSTS: &[KnownConst] = &[
    KnownConst {
        variant: "EDC17CP14",
        start: 0x0004_0000,
        end: 0x0007_FFFB,
        k: 0xA185_64EC,
    },
    KnownConst {
        variant: "EDC17CP14",
        start: 0x0008_0000,
        end: 0x000F_EEFB,
        k: 0xF497_D557,
    },
    KnownConst {
        variant: "EDC17C08",
        start: 0x0010_0000,
        end: 0x0011_FFFB,
        k: 0x3092_DCA9,
    },
    KnownConst {
        variant: "EDC17C08",
        start: 0x0012_0000,
        end: 0x0017_7DFB,
        k: 0xBB1B_E6EF,
    },
    KnownConst {
        variant: "EDC17CP11",
        start: 0x0002_0000,
        end: 0x000F_FFFB,
        k: 0xD53B_4047,
    },
    KnownConst {
        variant: "EDC17CP42",
        start: 0x0030_0000,
        end: 0x003F_EFFB,
        k: 0x1C67_AB6A,
    },
];

/// Look up the validated constant for a block by its file-offset bounds.
pub fn known_const(start: u32, end: u32) -> Option<&'static KnownConst> {
    KNOWN_CONSTS
        .iter()
        .find(|c| c.start == start && c.end == end)
}

/// Map a directory CPU address to a file offset (strip the segment nibble).
fn addr_to_offset(addr: u32, len: usize) -> Option<u32> {
    let off = addr & 0x00FF_FFFF;
    if (off as usize) < len {
        Some(off)
    } else {
        None
    }
}

fn read_u32_le(image: &[u8], off: usize) -> Option<u32> {
    image
        .get(off..off + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Parse the on-image checksum directory.
///
/// Scans for the `0xFADECAFE`/`0xCAFEAFFE` marker pair; the two little-endian u32
/// words immediately before it are `{start, end}` (inclusive CPU addresses).
/// Records are mapped to file offsets, kept only when sane (`start < end`,
/// `end < len`, `end+1` dword-aligned, span ≥ 8), and de-duplicated. Returned
/// sorted by `end` ascending.
pub fn parse_directory(image: &[u8]) -> Vec<DirBlock> {
    let len = image.len();
    let mut out: Vec<DirBlock> = Vec::new();
    if len < 16 {
        return out;
    }
    let mut i = 0usize;
    while i + 8 <= len {
        if image[i..i + 4] == MARK_CS && image[i + 4..i + 8] == MARK_CP && i >= 8 {
            if let (Some(sa), Some(ea)) = (read_u32_le(image, i - 8), read_u32_le(image, i - 4)) {
                if let (Some(start), Some(end)) = (addr_to_offset(sa, len), addr_to_offset(ea, len))
                {
                    let aligned = start % 4 == 0 && (end.wrapping_add(1)) % 4 == 0;
                    let valid = start < end && (end as usize) < len && (end - start) >= 8;
                    let blk = DirBlock { start, end };
                    if aligned && valid && !out.contains(&blk) {
                        out.push(blk);
                    }
                }
            }
        }
        i += 4; // records/markers are dword-aligned
    }
    out.sort_by_key(|b| (b.end, b.start));
    out
}

/// Compute the trailing 4-byte compensation word that makes
/// `crc32(block) == target`, by an affine GF(2) solve over the free last dword.
///
/// `block` is the full block bytes (length multiple of 4, ≥ 4). The last 4 bytes
/// are treated as the free variable; the returned value is what they must hold.
/// This is exact (not a search) and reproduces the ECU's real stored word.
pub fn forge_last_dword(block: &[u8], target: u32) -> [u8; 4] {
    assert!(block.len() >= 4 && block.len().is_multiple_of(4));
    let fp = block.len() - 4;
    let mut scratch = block.to_vec();
    let eval = |scratch: &mut Vec<u8>, x: u32| -> u32 {
        scratch[fp..fp + 4].copy_from_slice(&x.to_le_bytes());
        crc32(scratch)
    };
    let f0 = eval(&mut scratch, 0);
    // Column i = effect of flipping input bit i of the free dword.
    let mut cols = [0u32; 32];
    for (i, col) in cols.iter_mut().enumerate() {
        *col = eval(&mut scratch, 1u32 << i) ^ f0;
    }
    let tgt = target ^ f0;
    // Solve sum_{i in X} cols[i] == tgt over GF(2). Augmented matrix: 32 rows
    // (output bits), 32 variables (input bits) + rhs.
    let mut rows = [[0u8; 33]; 32];
    for (r, row) in rows.iter_mut().enumerate() {
        for (j, col) in cols.iter().enumerate() {
            row[j] = ((col >> r) & 1) as u8;
        }
        row[32] = ((tgt >> r) & 1) as u8;
    }
    let mut pivot_of = [usize::MAX; 32];
    let mut r = 0usize;
    for c in 0..32 {
        if let Some(pr) = (r..32).find(|&rr| rows[rr][c] == 1) {
            rows.swap(r, pr);
            for rr in 0..32 {
                if rr != r && rows[rr][c] == 1 {
                    for k in 0..33 {
                        rows[rr][k] ^= rows[r][k];
                    }
                }
            }
            pivot_of[c] = r;
            r += 1;
        }
    }
    let mut x = 0u32;
    for c in 0..32 {
        let p = pivot_of[c];
        if p != usize::MAX && rows[p][32] == 1 {
            x |= 1u32 << c;
        }
    }
    x.to_le_bytes()
}

/// Bosch EDC17 (TriCore) calibration checksum scheme.
pub struct Edc17;

impl Edc17 {
    /// The validated per-block constants.
    pub fn constants(&self) -> &'static [KnownConst] {
        KNOWN_CONSTS
    }
}

impl ChecksumAlgo for Edc17 {
    fn id(&self) -> &str {
        "edc17"
    }

    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        let blocks = parse_directory(image);
        let mut out = Vec::new();
        for b in &blocks {
            let (s, e) = b.range();
            if e > image.len() {
                continue;
            }
            let computed = crc32(&image[s..e]) as u64;
            match known_const(b.start, b.end) {
                Some(kc) => out.push(BlockStatus {
                    name: format!("{}@0x{:06X}", kc.variant, b.start),
                    region: MemoryRegion::new(b.start, b.end - b.start + 1),
                    stored: kc.k as u64,
                    computed,
                    ok: computed == kc.k as u64,
                }),
                None => out.push(BlockStatus {
                    // Block present in the directory but with no validated
                    // constant (code block / nested descriptor). Surfaced,
                    // never confirmed — we do not know its target.
                    name: format!("edc17-unknown@0x{:06X}", b.start),
                    region: MemoryRegion::new(b.start, b.end - b.start + 1),
                    stored: 0,
                    computed,
                    ok: false,
                }),
            }
        }
        if out.is_empty() {
            return Err(Error::Config(
                "no EDC17 checksum directory (FADECAFE/CAFEAFFE) found".into(),
            ));
        }
        Ok(out)
    }

    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        let blocks = parse_directory(image);
        // We only ever write the exact, forged compensation word of a block whose
        // target constant is VALIDATED. Unknown blocks are left untouched (never
        // guessed). If not a single known block is present, we cannot validate
        // anything and refuse rather than pretend.
        if !blocks.iter().any(|b| known_const(b.start, b.end).is_some()) {
            return Err(Error::Unsupported(
                "EDC17: no block with a validated target constant in this image; \
                 refusing to guess (see FINDINGS.md)"
                    .into(),
            ));
        }
        let mut fixes = Vec::new();
        for b in &blocks {
            let Some(kc) = known_const(b.start, b.end) else {
                continue;
            };
            let (s, e) = b.range();
            if e > image.len() {
                continue;
            }
            let store = b.store_offset() as usize;
            let old = read_u32_le(image, store)
                .ok_or_else(|| Error::Config("EDC17 store offset out of range".into()))?;
            let want = forge_last_dword(&image[s..e], kc.k);
            let want_u = u32::from_le_bytes(want);
            if old != want_u {
                image[store..store + 4].copy_from_slice(&want);
                fixes.push(Fix {
                    name: format!("{}@0x{:06X}", kc.variant, b.start),
                    offset: store as u32,
                    old: old as u64,
                    new: want_u as u64,
                });
            }
        }
        Ok(fixes)
    }

    /// Correct EDC17 blocks using the owner's own stock dump to recover each
    /// block's firmware constant `K = crc32(stock_block)` (valid because the
    /// ECU wrote self-consistent checksums). The tuned image's trailing
    /// compensation word is then forged to that same `K`. Only blocks that
    /// appear with identical bounds in **both** the target and the reference
    /// directory are touched; when a validated library constant also exists for
    /// a block, the reference must agree with it or that block is skipped — so a
    /// wrong/mismatched reference can never silently corrupt the image.
    fn correct_with_reference(&self, image: &mut [u8], reference: &[u8]) -> Result<Vec<Fix>> {
        let blocks = parse_directory(image);
        let ref_blocks = parse_directory(reference);
        if blocks.is_empty() {
            return Err(Error::Config(
                "no EDC17 checksum directory (FADECAFE/CAFEAFFE) in the target image".into(),
            ));
        }
        let shared: Vec<DirBlock> = blocks
            .iter()
            .copied()
            .filter(|b| ref_blocks.contains(b))
            .collect();
        if shared.is_empty() {
            return Err(Error::Unsupported(
                "EDC17: the reference shares no checksum block with this image \
                 (different firmware?); refusing to guess"
                    .into(),
            ));
        }
        let mut fixes = Vec::new();
        for b in shared {
            let (s, e) = b.range();
            if e > image.len() || e > reference.len() {
                continue;
            }
            // K recovered from the owner's valid stock dump = the firmware's
            // real per-block constant.
            let k = crc32(&reference[s..e]);
            // If we also have a validated library constant for this block, the
            // reference must match it; a mismatch means it isn't this firmware.
            if let Some(kc) = known_const(b.start, b.end) {
                if kc.k != k {
                    continue;
                }
            }
            let store = b.store_offset() as usize;
            let old = read_u32_le(image, store)
                .ok_or_else(|| Error::Config("EDC17 store offset out of range".into()))?;
            let want = forge_last_dword(&image[s..e], k);
            let want_u = u32::from_le_bytes(want);
            if old != want_u {
                image[store..store + 4].copy_from_slice(&want);
                fixes.push(Fix {
                    name: format!("edc17@0x{:06X} (K from reference)", b.start),
                    offset: store as u32,
                    old: old as u64,
                    new: want_u as u64,
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
    fn crc32_known_vector() {
        // Reflected IEEE CRC-32 of "123456789" == 0xCBF43926.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn forge_sets_crc_to_target_synthetic() {
        // A small block: header + a free trailing dword. Forging must drive the
        // whole-block CRC to an arbitrary target and be deterministic.
        let mut block = vec![0u8; 64];
        for (i, b) in block.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(31).wrapping_add(7);
        }
        let target = 0xA185_64EC;
        let x = forge_last_dword(&block, target);
        let n = block.len();
        block[n - 4..].copy_from_slice(&x);
        assert_eq!(crc32(&block), target);
    }

    #[test]
    fn parse_and_roundtrip_synthetic_directory() {
        // Build a 0x2000-byte image with one directory record for block
        // [0x100, 0x1FF] (CPU 0x80000100..0x800001FF) at offset 0x40, register a
        // temporary constant, and prove verify/correct round-trip.
        let len = 0x2000usize;
        let mut img = vec![0u8; len];
        // record { start, end, FADECAFE, CAFEAFFE } @ 0x40
        img[0x40..0x44].copy_from_slice(&0x8000_0100u32.to_le_bytes());
        img[0x44..0x48].copy_from_slice(&0x8000_01FFu32.to_le_bytes());
        img[0x48..0x4C].copy_from_slice(&0xFADE_CAFEu32.to_le_bytes());
        img[0x4C..0x50].copy_from_slice(&0xCAFE_AFFEu32.to_le_bytes());
        for b in img[0x100..0x1FC].iter_mut() {
            *b = 0x5A;
        }
        let blocks = parse_directory(&img);
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0],
            DirBlock {
                start: 0x100,
                end: 0x1FF
            }
        );
        assert_eq!(blocks[0].store_offset(), 0x1FC);

        // Pick the block's current CRC as "the constant" and forge to it: the
        // forge path is what correct() uses. (verify() uses KNOWN_CONSTS, which
        // this synthetic block is deliberately not in — so it reports unknown.)
        let (s, e) = blocks[0].range();
        let k = crc32(&img[s..e]);
        let x = forge_last_dword(&img[s..e], k);
        img[0x1FC..0x200].copy_from_slice(&x);
        assert_eq!(crc32(&img[s..e]), k);

        // Not a known constant -> verify surfaces it as unknown/not-ok.
        let st = Edc17.verify(&img).unwrap();
        assert_eq!(st.len(), 1);
        assert!(!st[0].ok);
        // correct() refuses: no validated constant present.
        assert!(Edc17.correct(&mut img).is_err());
    }

    /// Build a 0x2000 image with one directory block [0x100,0x1FF] whose
    /// trailing dword is forged so the block CRC equals an arbitrary `k` — a
    /// self-consistent "stock" dump for a block that is NOT in KNOWN_CONSTS
    /// (exactly the owner's-CC case: a real VAG block with no shipped constant).
    fn synthetic_stock(k: u32) -> Vec<u8> {
        let mut img = vec![0u8; 0x2000];
        img[0x40..0x44].copy_from_slice(&0x8000_0100u32.to_le_bytes());
        img[0x44..0x48].copy_from_slice(&0x8000_01FFu32.to_le_bytes());
        img[0x48..0x4C].copy_from_slice(&0xFADE_CAFEu32.to_le_bytes());
        img[0x4C..0x50].copy_from_slice(&0xCAFE_AFFEu32.to_le_bytes());
        for b in img[0x100..0x1FC].iter_mut() {
            *b = 0x5A;
        }
        let (s, e) = (0x100usize, 0x200usize);
        let x = forge_last_dword(&img[s..e], k);
        img[0x1FC..0x200].copy_from_slice(&x);
        assert_eq!(crc32(&img[s..e]), k);
        img
    }

    #[test]
    fn correct_with_reference_bootstraps_unknown_constant() {
        // Stock: a block whose firmware constant K is NOT in KNOWN_CONSTS.
        let k = 0x1234_ABCDu32;
        let stock = synthetic_stock(k);
        // correct() alone must refuse — no validated constant for this block.
        assert!(Edc17.correct(&mut stock.clone()).is_err());

        // "Tune" it: change a calibration byte, which breaks the block CRC.
        let mut tuned = stock.clone();
        tuned[0x150] ^= 0x5A;
        let (s, e) = (0x100usize, 0x200usize);
        assert_ne!(crc32(&tuned[s..e]), k);

        // Bootstrap K from the owner's own stock dump and correct.
        let fixes = Edc17.correct_with_reference(&mut tuned, &stock).unwrap();
        assert_eq!(fixes.len(), 1);
        // Block CRC is now back to the firmware constant recovered from stock.
        assert_eq!(crc32(&tuned[s..e]), k);
        // Only the trailing compensation dword moved; the edited data byte stays.
        assert_eq!(tuned[0x150], stock[0x150] ^ 0x5A);
    }

    #[test]
    fn correct_with_reference_rejects_foreign_image() {
        // A reference whose directory shares no block with the target is refused.
        let target = synthetic_stock(0x1111_1111);
        let foreign = vec![0u8; 0x400]; // no directory at all
        assert!(Edc17
            .correct_with_reference(&mut target.clone(), &foreign)
            .is_err());
    }

    #[test]
    fn empty_image_has_no_directory() {
        assert!(parse_directory(&[0u8; 8]).is_empty());
        assert!(Edc17.verify(&[0u8; 8]).is_err());
    }

    // ---- Real-dump fixtures (private) -----------------------------------
    //
    // Enabled only when OPENTUNE_FIXTURES points at a dir holding the harvested
    // EDC17 pairs `edc17/<name>_ori.bin` / `_mod.bin`. CI has no access, so this
    // is #[ignore]d. Run with:
    //   OPENTUNE_FIXTURES=/opt/nvme-fw/opentune-fixtures \
    //     cargo test -p edc17-checksum -- --ignored --nocapture

    const FIXTURES: &[&str] = &[
        "cp14_ix35_505301",
        "cp14_ix35_510061",
        "cp14_ix35_514109",
        "cp14_ix35_516092",
        "cp14_santafe_504642",
        "cp14_santafe_509514",
        "cp14_sorento_503823",
        "cp14_sportage_508178",
        "c08_soul_398530",
        "c08_soul_501556",
        "c08_sportage_510091",
        "cp11_disco_503430",
        "cp11_rrsport_397145",
        "cp42_evoque_513068",
        "cp42_freelander_508786",
    ];

    fn load(dir: &str, name: &str, suf: &str) -> Option<Vec<u8>> {
        std::fs::read(format!("{dir}/edc17/{name}_{suf}.bin")).ok()
    }

    #[test]
    #[ignore = "needs private EDC17 fixtures via OPENTUNE_FIXTURES"]
    fn reference_edc17_dumps() {
        let dir = match std::env::var("OPENTUNE_FIXTURES") {
            Ok(d) => d,
            Err(_) => return,
        };
        let mut checked = 0;
        let mut cal_blocks = 0;
        for name in FIXTURES {
            let (Some(stock), Some(tuned)) = (load(&dir, name, "ori"), load(&dir, name, "mod"))
            else {
                continue;
            };
            // (1) Every KNOWN block verifies in BOTH stock and tuned.
            for img in [&stock, &tuned] {
                for st in Edc17.verify(img).unwrap() {
                    if st.name.starts_with("edc17-unknown") {
                        continue;
                    }
                    assert!(st.ok, "{name}: {} failed in a valid image", st.name);
                }
            }
            // (2) A valid stock image needs no correction (idempotent).
            assert!(
                Edc17.correct(&mut stock.clone()).unwrap().is_empty(),
                "{name}: valid stock should need no fixes"
            );
            // (3) For each known block that the tune CHANGED, re-deriving the
            //     compensation word of the TUNED block reproduces its real stored
            //     value exactly, and correcting a zeroed comp word restores it.
            for b in parse_directory(&tuned) {
                let Some(kc) = known_const(b.start, b.end) else {
                    continue;
                };
                let (s, e) = b.range();
                let changed = stock[s..e] != tuned[s..e];
                if !changed {
                    continue;
                }
                let stored = &tuned[b.store_offset() as usize..b.store_offset() as usize + 4];
                let forged = forge_last_dword(&tuned[s..e], kc.k);
                assert_eq!(
                    forged, stored,
                    "{name}: forged != stored for {}",
                    kc.variant
                );

                // Zero the comp word, correct, and the exact value comes back.
                let mut img = tuned.clone();
                let off = b.store_offset() as usize;
                img[off..off + 4].copy_from_slice(&[0, 0, 0, 0]);
                Edc17.correct(&mut img).unwrap();
                assert_eq!(&img[off..off + 4], stored, "{name}: re-derived comp word");
                cal_blocks += 1;
            }
            checked += 1;
        }
        assert!(checked >= 1, "no EDC17 fixtures under OPENTUNE_FIXTURES");
        assert!(cal_blocks >= 1, "no changed calibration blocks exercised");
        eprintln!("EDC17: validated {checked} dumps, {cal_blocks} changed cal blocks");
    }
}
