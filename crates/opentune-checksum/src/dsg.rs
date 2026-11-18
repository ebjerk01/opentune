//! VAG **DQ250 PQ (`02E`)** six-speed DSG TCU — per-sector CRC-16 table.
//!
//! The PQ35/PQ46-era `02E` mechatronic (Temic) runs big-endian PowerPC code and
//! is read as a **0x68000-byte** (416 KiB) image:
//!
//! ```text
//! 0x00000 .. 0x5FFFF   code, twelve 32 KiB sectors (S00..S11)
//! 0x60000 .. 0x67FDF   calibration (CAL) sector, ident record near its start
//! 0x67FC0 .. 0x67FDF   version tag  "v069…__getriebe_DSG_XXXX"
//! 0x67FE0 .. 0x67FFF   footer: 00 00 | 13 × u16 BE CRC table | 00 00 FF FF
//! ```
//!
//! Footer word `i` (`0x67FE2 + 2·i`, big-endian) is the **CRC-16/MCRF4XX**
//! (polynomial `0x1021` reflected = `0x8408`, init `0xFFFF`, reflected in/out,
//! no final XOR) of sector `i`: `S00..S11` are `[i·0x8000, (i+1)·0x8000)`, the
//! thirteenth word covers the CAL sector `[0x60000, 0x67FE0)` — everything up to,
//! but not including, the footer itself.
//!
//! # Provenance and validation
//!
//! Found by OpenTune (no third-party source) on real `02E` reads: across six
//! independent images of five software versions (`02E300011BL`, `02E300011C`,
//! `02E300041N`, `02E300042F`, `02E300042G`, `02E300046K`) **every one of the 13
//! words matches** (78/78). It is therefore **real-image-tested**; bench and
//! vehicle validation of a corrected image are still open. A seventh read
//! (`02E300042M`) has its reset vectors and footer erased (`0xFF`), i.e. it is a
//! partial/tool-exported read rather than a flash image — it is refused, as it
//! must be.
//!
//! This is **not** the JAMCRC of the later DQ250-MQB (`0D9`, TriCore) — that
//! scheme (`dq250` in [`crate::tcu`]) does not hold on any `02E` read. Later `02E`
//! software built for an Infineon TriCore (e.g. `02E300013N`, little-endian, read
//! as 0xB0000 bytes with a 64 KiB CAL first) uses neither scheme as far as the
//! available reads show; such images are not recognised here and are refused.
//!
//! # Anti-brick stance
//!
//! * **Full image** (exactly 0x68000 bytes): recognised only when the footer
//!   framing is present **and** at least [`MIN_CODE_SECTORS_OK`] of the twelve
//!   code sectors already validate — proving the layout on this very image from
//!   data a calibration edit does not touch. `correct` then rewrites only the
//!   table words of sectors that mismatch.
//! * **Extracted CAL sector** (exactly 0x8000 bytes, the `0x60000..0x67FFF`
//!   slice): recognised from its data-independent framing (`00 00` at `+0x7FE0`,
//!   `00 00 FF FF` at `+0x7FFC`, `getriebe_DSG` in the version tag); only the CAL
//!   word at `+0x7FFA` is checked or written. The code-sector words cannot be
//!   checked without the code, so they are left alone.
//! * Anything else is refused; nothing is ever guessed.
use crate::{
    BlockReport, BlockStatus, ChecksumAlgo, ChecksumKind, Fix, Recognition, Representation,
};
use opentune_core::{Error, MemoryRegion, Result};

/// Size of a full `02E` (PowerPC generation) flash read.
pub const DQ250_02E_IMAGE_LEN: usize = 0x6_8000;
/// Size of one flash sector (and of the extracted CAL sector).
pub const DQ250_02E_SECTOR: usize = 0x8000;
/// Offset of the CAL sector inside the full read.
pub const DQ250_02E_CAL_BASE: usize = 0x6_0000;
/// Number of code sectors before the CAL sector.
const CODE_SECTORS: usize = 12;
/// Footer offset inside a sector (the CAL CRC covers `[0, FOOTER)`).
const FOOTER: usize = 0x7FE0;
/// CRC table offset inside the CAL sector (13 big-endian u16).
const TABLE: usize = 0x7FE2;
/// The CAL word's offset inside the CAL sector (`TABLE + 2·12`).
const CAL_WORD: usize = TABLE + 2 * CODE_SECTORS;
/// Version-tag window inside the CAL sector.
const TAG: std::ops::Range<usize> = 0x7FC0..0x7FE0;
/// Code sectors that must already validate before a full image is recognised.
pub const MIN_CODE_SECTORS_OK: usize = 7;

/// **CRC-16/MCRF4XX**: reflected polynomial `0x8408` (`0x1021` reversed), init
/// `0xFFFF`, no final XOR. Check value: `"123456789"` → `0x6F91`.
pub fn crc16_mcrf4xx(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x8408
            } else {
                crc >> 1
            };
        }
    }
    crc
}

#[inline]
fn u16be(img: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([img[i], img[i + 1]])
}

/// The two image shapes this module understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Full 0x68000-byte read; the CAL sector starts at `0x60000`.
    Full,
    /// The 0x8000-byte CAL sector on its own.
    CalOnly,
}

impl Shape {
    fn of(img: &[u8]) -> Option<Self> {
        match img.len() {
            DQ250_02E_IMAGE_LEN => Some(Shape::Full),
            DQ250_02E_SECTOR => Some(Shape::CalOnly),
            _ => None,
        }
    }

    /// File offset of the CAL sector.
    fn cal_base(self) -> usize {
        match self {
            Shape::Full => DQ250_02E_CAL_BASE,
            Shape::CalOnly => 0,
        }
    }

    /// The protected sectors: `(name, data start, data end, table word offset)`.
    fn sectors(self) -> Vec<(String, usize, usize, usize)> {
        let cal = self.cal_base();
        let mut v = Vec::new();
        if self == Shape::Full {
            for i in 0..CODE_SECTORS {
                let s = i * DQ250_02E_SECTOR;
                v.push((
                    format!("dq250-02e-S{i:02}"),
                    s,
                    s + DQ250_02E_SECTOR,
                    cal + TABLE + 2 * i,
                ));
            }
        }
        v.push((
            "dq250-02e-CAL".to_string(),
            cal,
            cal + FOOTER,
            cal + CAL_WORD,
        ));
        v
    }
}

/// The data-independent footer framing of a CAL sector at `cal`.
fn framing_ok(img: &[u8], cal: usize) -> bool {
    let Some(sec) = img.get(cal..cal + DQ250_02E_SECTOR) else {
        return false;
    };
    let tag = &sec[TAG];
    sec[FOOTER..FOOTER + 2] == [0, 0]
        && sec[0x7FFC..0x8000] == [0, 0, 0xFF, 0xFF]
        && tag.windows(12).any(|w| w == b"getriebe_DSG")
}

fn statuses(img: &[u8], shape: Shape) -> Vec<BlockStatus> {
    shape
        .sectors()
        .into_iter()
        .map(|(name, s, e, word)| {
            let computed = crc16_mcrf4xx(&img[s..e]);
            let stored = u16be(img, word);
            BlockStatus {
                name,
                region: MemoryRegion::new(s as u32, (e - s) as u32),
                stored: stored as u64,
                computed: computed as u64,
                ok: stored == computed,
            }
        })
        .collect()
}

/// VAG **DQ250 PQ (`02E`)** DSG TCU — CRC-16/MCRF4XX sector table.
pub struct Dq250PqCrc16;

impl Dq250PqCrc16 {
    /// Recognised only when the image shape and footer framing fit and (for a
    /// full image) enough untouched code sectors already validate.
    fn recognised(img: &[u8]) -> bool {
        let Some(shape) = Shape::of(img) else {
            return false;
        };
        if !framing_ok(img, shape.cal_base()) {
            return false;
        }
        match shape {
            Shape::CalOnly => true,
            Shape::Full => {
                statuses(img, shape)
                    .iter()
                    .take(CODE_SECTORS)
                    .filter(|b| b.ok)
                    .count()
                    >= MIN_CODE_SECTORS_OK
            }
        }
    }
}

impl ChecksumAlgo for Dq250PqCrc16 {
    fn id(&self) -> &str {
        "dq250-02e"
    }

    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        Ok(Shape::of(image)
            .map(|shape| statuses(image, shape))
            .unwrap_or_default())
    }

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
                "not a recognised DQ250 02E image (expects a 0x68000-byte read with the \
                 getriebe_DSG footer and validating code sectors, or the 0x8000-byte CAL \
                 sector); refusing to guess (see crate::dsg docs)"
                    .into(),
            ));
        }
        let shape = Shape::of(image).expect("recognised implies a known shape");
        let mut fixes = Vec::new();
        for ((name, _, _, word), st) in shape.sectors().into_iter().zip(statuses(image, shape)) {
            if !st.ok {
                image[word..word + 2].copy_from_slice(&(st.computed as u16).to_be_bytes());
                fixes.push(Fix {
                    name,
                    offset: word as u32,
                    old: st.stored,
                    new: st.computed,
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
    fn mcrf4xx_check_value() {
        assert_eq!(crc16_mcrf4xx(b"123456789"), 0x6F91);
        assert_eq!(crc16_mcrf4xx(b""), 0xFFFF);
    }

    /// Deterministic pseudo-random fill (xorshift), so tests need no RNG crate.
    fn fill(buf: &mut [u8], seed: u32) {
        let mut x = seed | 1;
        for b in buf.iter_mut() {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *b = x as u8;
        }
    }

    /// A synthetic full read with a correct footer table.
    fn synthetic_full() -> Vec<u8> {
        let mut img = vec![0u8; DQ250_02E_IMAGE_LEN];
        fill(&mut img, 0x02E3_0004);
        let cal = DQ250_02E_CAL_BASE;
        img[cal + 0x7FC0..cal + 0x7FE0].copy_from_slice(b"v0690000ea___getriebe_DSG_TEST  ");
        img[cal + FOOTER..cal + FOOTER + 2].copy_from_slice(&[0, 0]);
        img[cal + 0x7FFC..cal + 0x8000].copy_from_slice(&[0, 0, 0xFF, 0xFF]);
        for (_, s, e, word) in Shape::Full.sectors() {
            let c = crc16_mcrf4xx(&img[s..e]);
            img[word..word + 2].copy_from_slice(&c.to_be_bytes());
        }
        img
    }

    #[test]
    fn full_image_verify_correct_roundtrip() {
        let algo = Dq250PqCrc16;
        let mut img = synthetic_full();
        let st = algo.verify(&img).unwrap();
        assert_eq!(st.len(), 13);
        assert!(st.iter().all(|b| b.ok));
        assert!(algo
            .verify_report(&img, Representation::RawFullImage)
            .unwrap()
            .iter()
            .all(|r| r.is_supported()));
        // Edit a calibration byte: only the CAL word goes bad and is fixed.
        img[DQ250_02E_CAL_BASE + 0x5E0] ^= 0x55;
        let st = algo.verify(&img).unwrap();
        assert_eq!(st.iter().filter(|b| !b.ok).count(), 1);
        assert!(!st[12].ok);
        let fixes = algo.correct(&mut img).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].name, "dq250-02e-CAL");
        assert_eq!(fixes[0].offset as usize, DQ250_02E_CAL_BASE + CAL_WORD);
        assert!(algo.verify(&img).unwrap().iter().all(|b| b.ok));
        // Already valid: nothing written.
        assert!(algo.correct(&mut img).unwrap().is_empty());
    }

    #[test]
    fn code_patch_is_corrected_too() {
        let algo = Dq250PqCrc16;
        let mut img = synthetic_full();
        img[0x1_2345] ^= 0xFF; // sector S02
        img[DQ250_02E_CAL_BASE + 0x100] ^= 0x01;
        let fixes = algo.correct(&mut img).unwrap();
        let names: Vec<_> = fixes.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["dq250-02e-S02", "dq250-02e-CAL"]);
        assert!(algo.verify(&img).unwrap().iter().all(|b| b.ok));
    }

    #[test]
    fn extracted_cal_sector_roundtrip() {
        let algo = Dq250PqCrc16;
        let full = synthetic_full();
        let mut cal = full[DQ250_02E_CAL_BASE..].to_vec();
        assert_eq!(cal.len(), DQ250_02E_SECTOR);
        let st = algo.verify(&cal).unwrap();
        assert_eq!(st.len(), 1);
        assert!(st[0].ok);
        cal[0x10] ^= 0x80;
        assert!(!algo.verify(&cal).unwrap()[0].ok);
        let fixes = algo.correct(&mut cal).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].offset as usize, CAL_WORD);
        assert!(algo.verify(&cal).unwrap()[0].ok);
    }

    #[test]
    fn unrecognised_images_are_refused() {
        let algo = Dq250PqCrc16;
        // Wrong size.
        let mut other = vec![0x5Au8; 0x10_0000];
        assert!(algo.verify(&other).unwrap().is_empty());
        assert!(algo.correct(&mut other).is_err());
        // Right size, random content (no framing).
        let mut rnd = vec![0u8; DQ250_02E_IMAGE_LEN];
        fill(&mut rnd, 7);
        assert!(algo.correct(&mut rnd).is_err());
        assert!(algo
            .verify_report(&rnd, Representation::Unknown)
            .unwrap()
            .iter()
            .all(|r| !r.is_supported()));
        // Framing present but the footer table erased (a partial read): too few
        // code sectors validate, so the layout is not proven on this image.
        let mut erased = synthetic_full();
        let t = DQ250_02E_CAL_BASE + TABLE;
        erased[t..t + 26].fill(0xFF);
        assert!(algo.correct(&mut erased).is_err());
        // Footer framing missing on an otherwise valid image.
        let mut nofooter = synthetic_full();
        nofooter[DQ250_02E_CAL_BASE + 0x7FFF] = 0;
        assert!(algo.correct(&mut nofooter).is_err());
    }

    /// Real `02E` reads (private, never committed). No access in CI, so ignored:
    ///   OPENTUNE_FIXTURES=/path/to/opentune-fixtures \
    ///     cargo test -p opentune-checksum dq250_02e -- --ignored
    #[test]
    #[ignore = "needs owner DQ250 02E fixtures via OPENTUNE_FIXTURES/tcu/dq250-02e"]
    fn reference_dq250_02e() {
        let Ok(dir) = std::env::var("OPENTUNE_FIXTURES") else {
            return;
        };
        let Ok(rd) = std::fs::read_dir(format!("{dir}/tcu/dq250-02e")) else {
            return;
        };
        let algo = Dq250PqCrc16;
        let mut verified = 0;
        for entry in rd.flatten() {
            let img = std::fs::read(entry.path()).unwrap();
            if img.len() != DQ250_02E_IMAGE_LEN {
                // TriCore-generation reads: refused, never guessed.
                assert!(algo.correct(&mut img.clone()).is_err());
                continue;
            }
            let st = algo.verify(&img).unwrap();
            if st.iter().all(|b| b.ok) {
                verified += 1;
                assert!(algo
                    .verify_report(&img, Representation::RawFullImage)
                    .unwrap()
                    .iter()
                    .all(|r| r.is_supported()));
                // Edit a calibration cell, correct, and confirm the round trip
                // touched only the CAL word.
                let mut ed = img.clone();
                ed[DQ250_02E_CAL_BASE + 0x700] ^= 0x01;
                let fixes = algo.correct(&mut ed).unwrap();
                assert_eq!(fixes.len(), 1);
                assert_eq!(fixes[0].name, "dq250-02e-CAL");
                assert!(algo.verify(&ed).unwrap().iter().all(|b| b.ok));
                let diff = img.iter().zip(&ed).filter(|(a, b)| a != b).count();
                assert!(diff <= 3, "edit + 2 CRC bytes, got {diff}");
                // The extracted CAL sector validates on its own as well.
                let cal = &img[DQ250_02E_CAL_BASE..];
                assert!(algo.verify(cal).unwrap()[0].ok);
            } else {
                // Partial reads (erased footer) must be refused.
                assert!(algo.correct(&mut img.clone()).is_err());
            }
        }
        assert!(
            verified >= 5,
            "expected the real 02E reads to verify, got {verified}"
        );
    }
}
