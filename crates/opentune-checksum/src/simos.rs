//! Continental / Siemens **Simos-family secured-block CRC headers** (`simos-crc`).
//!
//! # The scheme (open reference)
//!
//! VW_Flash (bri3d, BSD-2-Clause, `lib/checksum.py`) documents the block
//! checksum of the VAG Simos ECUs: each secured block carries a header
//!
//! ```text
//! +0  u32 init      initial CRC value (0 in every image seen)
//! +4  u32 crc       stored CRC                      (little-endian)
//! +8  u8  count     number of areas, 1..=16 (then 3 zero bytes)
//! +12 { u32 start, u32 end } * count   absolute CPU addresses, inclusive
//! ```
//!
//! and the CRC is the Bosch/Simos forward CRC-32 (poly `0x04C11DB7`, init =
//! the header's `init`, no reflection, no final XOR — [`crate::crc32_bosch`]'s
//! engine) over the **concatenation** of the areas in table order. The header
//! is not inside its own areas, so rewriting the stored CRC never changes the
//! data it covers. (The same record is the PCR2.1 directory, see
//! [`crate::pcr21`].) Only the layout was taken from VW_Flash; this is a fresh
//! implementation.
//!
//! # What this engine does
//!
//! It works on a **full flash read** whose length is a power of two. Headers are
//! found structurally at every 256-byte-aligned offset: `init == 0`, a non-zero
//! stored CRC, `count` in `1..=16` followed by three zero bytes, and every area
//! in the TriCore flash segments `0x80xx_xxxx` / `0xA0xx_xxxx` with its
//! in-segment offset inside the image (file offset = `addr & 0x00FF_FFFF`; an
//! address past the image is rejected, never wrapped). An image is
//! **recognised** when at least one header verifies.
//!
//! The `+0x340` "CBOOT_TEMP" header VW_Flash describes is not 256-byte aligned
//! and is skipped: it describes the boot block at its temporary update address,
//! so it cannot be checked from a full read (it never verifies on stock reads).
//!
//! # Validation (honest scope)
//!
//! **Real-image-tested** on full 2 MiB reads of Continental/Siemens ECUs from
//! the owner's library (`OPENTUNE_FIXTURES/conti-crc`), not on VAG Simos:
//!
//! * Sim2K-24x (Hyundai ix35, Kia Sportage) and a Continental diesel
//!   (SsangYong/Renault K2053): boot `0x300`, CBOOT `0xC300`, ASW `0x20300`
//!   (16 areas) and the CAL header at `0x1C0400` verify on stock **and** on the
//!   independently tuned file (the Kia tuner rewrote the CAL CRC exactly as
//!   [`correct`](SimosCrc::correct) does; the Hyundai and K2053 tuners patched
//!   a compensation dword instead, leaving the stock CRC valid);
//! * Ford EMS (Focus III 1.6) CAL header `0x140300` and Lada/Renault EMS3120
//!   `0x1C300`: the tuned file carries a new, verifying CRC, reproduced byte
//!   for byte by [`correct_with_reference`](SimosCrc::correct_with_reference).
//!   These reads carry a single header, so an uncorrected tune has no
//!   verifying header left and standalone `correct` refuses (no recognition);
//! * a read-only scan of the library (29 296 files of full-read sizes,
//!   headers at block base + `0x300`) found 778 headers in Continental/Siemens
//!   reads (Sim2K-24x, Ford EMS, EMS3120, K205x, BMW MSD8x); 776 verify. The
//!   two that do not are the startup-boot header (`0x300`) of a BMW MSD81
//!   read, a variant this engine does not understand — so
//!   [`correct`](SimosCrc::correct) refuses an image whose boot header fails.
//!
//! VAG Simos 8 / 10 / 12 / 18 use the same header (per VW_Flash) at
//! block-relative offsets, but **no VAG Simos read was available**, so those
//! ECUs are not claimed. The ECM3 monitor sum (64-bit additive, address list
//! in ASW) is not implemented: it has no open reference image to validate on.

use crate::{
    crc32_forward, BlockReport, BlockStatus, ChecksumAlgo, ChecksumKind, Fix, Recognition,
    Representation,
};
use opentune_core::{Error, MemoryRegion, Result};

/// Header alignment searched in the image.
const HEADER_ALIGN: usize = 0x100;
/// Most areas one header may combine (VW_Flash: "up to 16").
pub const MAX_AREAS: usize = 16;
/// Smallest image handled (a full read; extracted blocks are not supported).
const MIN_IMAGE: usize = 0x1_0000;

/// One secured-block CRC header found in the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// File offset of the header (its `init` word); the CRC is at `offset + 4`.
    pub offset: u32,
    /// The CRC seed (`init`).
    pub init: u32,
    /// The stored CRC.
    pub stored: u32,
    /// Covered areas as inclusive file offsets, in CRC order.
    pub areas: Vec<(u32, u32)>,
}

impl Header {
    /// The CRC of the concatenated areas, as the ECU computes it.
    pub fn compute(&self, image: &[u8]) -> u32 {
        self.areas.iter().fold(self.init, |crc, &(s, e)| {
            crc32_forward(&image[s as usize..=e as usize], crc)
        })
    }

    /// Whether any area covers file offset 0 (the startup boot).
    fn covers_boot(&self) -> bool {
        self.areas.iter().any(|&(s, _)| s == 0)
    }

    fn name(&self) -> String {
        format!("crc-hdr@0x{:06X}", self.offset)
    }

    fn region(&self) -> MemoryRegion {
        let lo = self.areas.iter().map(|a| a.0).min().unwrap_or(0);
        let hi = self.areas.iter().map(|a| a.1).max().unwrap_or(0);
        MemoryRegion::new(lo, hi - lo + 1)
    }
}

fn u32_le(image: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        image.get(off..off + 4)?.try_into().ok()?,
    ))
}

/// Map a TriCore flash address to a file offset, or `None` if it is not in a
/// flash segment or lies past the image (never wrapped).
fn map(addr: u32, len: usize) -> Option<u32> {
    if !matches!(addr >> 24, 0x80 | 0xA0) {
        return None;
    }
    let off = addr & 0x00FF_FFFF;
    ((off as usize) < len).then_some(off)
}

fn header_at(image: &[u8], off: usize) -> Option<Header> {
    let len = image.len();
    let init = u32_le(image, off)?;
    let stored = u32_le(image, off + 4)?;
    let count = *image.get(off + 8)? as usize;
    if init != 0 || stored == 0 || !(1..=MAX_AREAS).contains(&count) {
        return None;
    }
    if image.get(off + 9..off + 12)? != [0, 0, 0] {
        return None;
    }
    let mut areas = Vec::with_capacity(count);
    for i in 0..count {
        let s = map(u32_le(image, off + 12 + 8 * i)?, len)?;
        let e = map(u32_le(image, off + 16 + 8 * i)?, len)?;
        if s > e {
            return None;
        }
        // The header must not cover its own CRC field.
        let crc_field = (off + 4) as u32;
        if s <= crc_field + 3 && crc_field <= e {
            return None;
        }
        areas.push((s, e));
    }
    Some(Header {
        offset: off as u32,
        init,
        stored,
        areas,
    })
}

/// Every structurally valid header in a full read (power-of-two length,
/// at least 64 KiB), in file order. Empty for anything else.
pub fn parse_headers(image: &[u8]) -> Vec<Header> {
    let len = image.len();
    if len < MIN_IMAGE || !len.is_power_of_two() {
        return Vec::new();
    }
    (0..len - 12)
        .step_by(HEADER_ALIGN)
        .filter_map(|off| header_at(image, off))
        .collect()
}

/// Headers whose stored CRC does not match, with the computed value.
fn failing(image: &[u8], headers: &[Header]) -> Vec<(usize, u32)> {
    headers
        .iter()
        .enumerate()
        .filter_map(|(i, h)| {
            let c = h.compute(image);
            (c != h.stored).then_some((i, c))
        })
        .collect()
}

/// Rewrite the given headers' CRCs on a copy until every header verifies (a
/// CRC field could lie inside another header's areas), then commit.
fn rewrite(image: &mut [u8], headers: &[Header], fix: &[usize]) -> Result<Vec<Fix>> {
    let mut work = image.to_vec();
    let mut hs = headers.to_vec();
    let mut fixes: Vec<Fix> = Vec::new();
    for _ in 0..4 {
        let mut changed = false;
        for &i in fix {
            let c = hs[i].compute(&work);
            if c == hs[i].stored {
                continue;
            }
            let at = hs[i].offset as usize + 4;
            work[at..at + 4].copy_from_slice(&c.to_le_bytes());
            match fixes.iter_mut().find(|f| f.offset == at as u32) {
                Some(f) => f.new = c as u64,
                None => fixes.push(Fix {
                    name: hs[i].name(),
                    offset: at as u32,
                    old: hs[i].stored as u64,
                    new: c as u64,
                }),
            }
            hs[i].stored = c;
            changed = true;
        }
        if !changed {
            break;
        }
    }
    if fix.iter().any(|&i| hs[i].compute(&work) != hs[i].stored) {
        return Err(Error::Unsupported(
            "simos-crc: interdependent CRC headers did not converge; refusing".into(),
        ));
    }
    image.copy_from_slice(&work);
    Ok(fixes)
}

/// The `simos-crc` checksum algorithm (see the module docs).
pub struct SimosCrc;

impl SimosCrc {
    fn recognised(image: &[u8]) -> Result<Vec<Header>> {
        let headers = parse_headers(image);
        if headers.is_empty() {
            return Err(Error::Unsupported(
                "simos-crc: no secured-block CRC header found (needs a full power-of-two read)"
                    .into(),
            ));
        }
        if headers.iter().all(|h| h.compute(image) != h.stored) {
            return Err(Error::Unsupported(
                "simos-crc: no header verifies, so the layout is not recognised; refusing to guess"
                    .into(),
            ));
        }
        Ok(headers)
    }
}

impl ChecksumAlgo for SimosCrc {
    fn id(&self) -> &str {
        "simos-crc"
    }

    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        Ok(parse_headers(image)
            .iter()
            .map(|h| {
                let computed = h.compute(image);
                BlockStatus {
                    name: h.name(),
                    region: h.region(),
                    stored: h.stored as u64,
                    computed: computed as u64,
                    ok: computed == h.stored,
                }
            })
            .collect())
    }

    /// Recognised (at least one header verifies) or merely consistent.
    fn verify_report(
        &self,
        image: &[u8],
        representation: Representation,
    ) -> Result<Vec<BlockReport>> {
        let blocks = self.verify(image)?;
        let recognition = if blocks.iter().any(|b| b.ok) {
            Recognition::Recognised
        } else {
            Recognition::Consistent
        };
        Ok(blocks
            .into_iter()
            .map(|b| BlockReport::new(b, representation, ChecksumKind::Embedded, recognition))
            .collect())
    }

    /// Rewrite every failing header's CRC. Refuses an unrecognised image and
    /// one whose startup-boot header fails (a variant this engine does not
    /// understand — tunes never touch the startup boot).
    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        let headers = Self::recognised(image)?;
        let bad = failing(image, &headers);
        if let Some((i, _)) = bad.iter().find(|(i, _)| headers[*i].covers_boot()) {
            return Err(Error::Unsupported(format!(
                "simos-crc: the startup-boot header at 0x{:X} does not verify — unknown \
                 variant; refusing (use correct_with_reference with your stock read)",
                headers[*i].offset
            )));
        }
        let idx: Vec<usize> = bad.into_iter().map(|(i, _)| i).collect();
        rewrite(image, &headers, &idx)
    }

    /// Rewrite only headers that verify on the **reference** (the owner's own
    /// stock read of the same software). A header that already fails the same
    /// way on the reference is left untouched; any other failing header makes
    /// the whole correction refuse.
    fn correct_with_reference(&self, image: &mut [u8], reference: &[u8]) -> Result<Vec<Fix>> {
        if reference.len() != image.len() {
            return Err(Error::Unsupported(
                "simos-crc: reference and image differ in length".into(),
            ));
        }
        // Recognition comes from the reference: the tuned image may fail
        // every header it shares with it.
        let headers = parse_headers(image);
        Self::recognised(reference)?;
        let reference_headers = parse_headers(reference);
        let mut idx = Vec::new();
        for (i, _) in failing(image, &headers) {
            let h = &headers[i];
            let twin = reference_headers
                .iter()
                .find(|r| r.offset == h.offset && r.areas == h.areas);
            match twin {
                Some(r) if r.compute(reference) == r.stored => idx.push(i),
                Some(r) if r.stored == h.stored => {} // fails on stock too: leave it
                _ => {
                    return Err(Error::Unsupported(format!(
                        "simos-crc: header at 0x{:X} is not validated by the reference; refusing",
                        h.offset
                    )))
                }
            }
        }
        rewrite(image, &headers, &idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEN: usize = 0x2_0000;

    fn put_header(img: &mut [u8], off: usize, areas: &[(u32, u32)]) {
        img[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
        img[off + 4..off + 8].copy_from_slice(&1u32.to_le_bytes()); // placeholder
        img[off + 8] = areas.len() as u8;
        img[off + 9..off + 12].fill(0);
        for (i, &(s, e)) in areas.iter().enumerate() {
            let at = off + 12 + 8 * i;
            img[at..at + 4].copy_from_slice(&s.to_le_bytes());
            img[at + 4..at + 8].copy_from_slice(&e.to_le_bytes());
        }
    }

    /// A boot header at 0x300 and a CAL header at 0x10300 (two areas).
    fn image() -> Vec<u8> {
        let mut img: Vec<u8> = (0..LEN).map(|i| (i * 7 + (i >> 9)) as u8).collect();
        img[..0x300].fill(0x11); // keep the headers' neighbourhood tidy
        put_header(
            &mut img,
            0x300,
            &[(0x8000_0000, 0x8000_02FF), (0x8000_0400, 0x8000_FFFF)],
        );
        put_header(
            &mut img,
            0x1_0300,
            &[(0xA001_0400, 0xA001_7FFF), (0xA001_8000, 0xA001_FFFF)],
        );
        for h in parse_headers(&img) {
            let c = h.compute(&img);
            let at = h.offset as usize + 4;
            img[at..at + 4].copy_from_slice(&c.to_le_bytes());
        }
        img
    }

    #[test]
    fn crc_engine_is_the_bosch_forward_crc_concatenated() {
        let img = image();
        let h = &parse_headers(&img)[1];
        let mut msg = img[0x1_0400..=0x1_7FFF].to_vec();
        msg.extend_from_slice(&img[0x1_8000..=0x1_FFFF]);
        assert_eq!(h.compute(&img), crate::crc32_bosch(&msg));
    }

    #[test]
    fn finds_headers_and_verifies() {
        let img = image();
        let hs = parse_headers(&img);
        assert_eq!(
            hs.iter().map(|h| h.offset).collect::<Vec<_>>(),
            vec![0x300, 0x1_0300]
        );
        let st = SimosCrc.verify(&img).unwrap();
        assert!(st.iter().all(|b| b.ok));
        let rep = SimosCrc
            .verify_report(&img, Representation::RawFullImage)
            .unwrap();
        assert!(rep.iter().all(|r| r.is_supported()));
    }

    #[test]
    fn corrupt_cal_is_detected_and_corrected() {
        let good = image();
        let mut img = good.clone();
        img[0x1_9000] ^= 0xFF;
        let st = SimosCrc.verify(&img).unwrap();
        assert!(st[0].ok && !st[1].ok);
        let fixes = SimosCrc.correct(&mut img).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].offset, 0x1_0304);
        assert!(SimosCrc.verify(&img).unwrap().iter().all(|b| b.ok));
        assert!(SimosCrc.correct(&mut img).unwrap().is_empty(), "idempotent");
        // Only the CRC field changed.
        let diff: Vec<usize> = (0..LEN).filter(|&i| img[i] != good[i]).collect();
        assert!(diff
            .iter()
            .all(|&i| i == 0x1_9000 || (0x1_0304..0x1_0308).contains(&i)));
    }

    #[test]
    fn failing_boot_header_refuses_standalone_correction() {
        let mut img = image();
        img[0x100] ^= 0xFF; // inside the boot header's first area
        img[0x1_9000] ^= 0xFF;
        let before = img.clone();
        assert!(matches!(
            SimosCrc.correct(&mut img),
            Err(Error::Unsupported(_))
        ));
        assert_eq!(img, before, "left untouched");
    }

    #[test]
    fn reference_correction_leaves_stock_failures_alone() {
        // Stock with a boot header that does not verify (as on BMW MSD81).
        let mut stock = image();
        stock[0x304] ^= 0x01;
        let mut tuned = stock.clone();
        tuned[0x1_9000] ^= 0xFF;
        let fixes = SimosCrc.correct_with_reference(&mut tuned, &stock).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].offset, 0x1_0304);
        assert_eq!(
            &tuned[0x304..0x308],
            &stock[0x304..0x308],
            "boot CRC untouched"
        );
        // A header failing on the image but verifying nowhere: refused.
        let mut other = image();
        other[0x1_9000] ^= 0xFF;
        let mut broken_ref = image();
        broken_ref[0x1_0304] ^= 0x01;
        broken_ref[0x1_9001] ^= 0x01;
        assert!(SimosCrc
            .correct_with_reference(&mut other, &broken_ref)
            .is_err());
    }

    #[test]
    fn unrecognised_images_are_refused() {
        // No header at all.
        let mut blank = vec![0u8; LEN];
        assert!(SimosCrc.verify(&blank).unwrap().is_empty());
        assert!(SimosCrc.correct(&mut blank).is_err());
        // Headers present but none verifies.
        let mut img = image();
        img[0x1_9000] ^= 0xFF;
        img[0x100] ^= 0xFF;
        assert!(SimosCrc.correct(&mut img).is_err());
        let rep = SimosCrc
            .verify_report(&img, Representation::RawFullImage)
            .unwrap();
        assert!(rep.iter().all(|r| !r.is_supported()));
        // Not a power-of-two full read.
        assert!(parse_headers(&image()[..LEN - 0x100]).is_empty());
    }

    #[test]
    fn ghost_headers_are_rejected() {
        let mut img = image();
        // Area past the image (would wrap with a mask), wrong segment, reversed,
        // non-zero init, and an area covering the header's own CRC field.
        put_header(&mut img, 0x8000, &[(0xA004_0000, 0xA004_00FF)]);
        put_header(&mut img, 0x8100, &[(0x9000_0000, 0x9000_00FF)]);
        put_header(&mut img, 0x8200, &[(0x8000_9000, 0x8000_8000)]);
        put_header(&mut img, 0x8300, &[(0x8000_9000, 0x8000_90FF)]);
        img[0x8300] = 1;
        put_header(&mut img, 0x8400, &[(0x8000_8000, 0x8000_8FFF)]);
        let offs: Vec<u32> = parse_headers(&img).iter().map(|h| h.offset).collect();
        assert_eq!(offs, vec![0x300, 0x1_0300]);
    }

    // ---- Real-image tests (owner's library reads via OPENTUNE_FIXTURES) ----
    //
    //   OPENTUNE_FIXTURES=/path/to/opentune-fixtures \
    //     cargo test -p opentune-checksum simos -- --ignored

    /// Stock/tuned pairs under `conti-crc/`: (stem, headers whose stored CRC the
    /// tuner rewrote). The Hyundai and K2053 tunes instead patched the last
    /// dword of the CAL area so its CRC stays the stock value (both still
    /// verify).
    const PAIRS: &[(&str, &[u32])] = &[
        ("kia_sim2k_scd0rc1a", &[0x1C_0400]),
        ("hyundai_sim2k_el4yp2as1f1a", &[]),
        ("renault_k2053zzs1d1f", &[]),
        ("ford_ems_bv61_cpe", &[0x14_0300]),
        ("lada_ems3120_90047", &[0x1_C300]),
    ];

    fn fixture(name: &str) -> Option<Vec<u8>> {
        let dir = std::env::var("OPENTUNE_FIXTURES").ok()?;
        std::fs::read(format!("{dir}/conti-crc/{name}.bin")).ok()
    }

    #[test]
    #[ignore = "needs the owner's Continental reads via OPENTUNE_FIXTURES/conti-crc"]
    fn reference_stock_and_tuned_reads_verify() {
        let mut checked = 0;
        for (stem, changed) in PAIRS {
            let (Some(ori), Some(m)) = (
                fixture(&format!("{stem}_ori")),
                fixture(&format!("{stem}_mod")),
            ) else {
                continue;
            };
            for img in [&ori, &m] {
                let st = SimosCrc.verify(img).unwrap();
                assert!(!st.is_empty() && st.iter().all(|b| b.ok), "{stem}: {st:?}");
            }
            // The tuner rewrote exactly the expected headers' CRCs.
            let rewritten: Vec<u32> = parse_headers(&ori)
                .iter()
                .filter(|h| u32_le(&m, h.offset as usize + 4) != Some(h.stored))
                .map(|h| h.offset)
                .collect();
            assert_eq!(&rewritten, changed, "{stem}");
            // An uncorrected tune (stock CRCs put back) is detected, and
            // correction reproduces the tuner's file byte for byte.
            let mut uncorrected = m.clone();
            for h in parse_headers(&ori) {
                let at = h.offset as usize + 4;
                uncorrected[at..at + 4].copy_from_slice(&h.stored.to_le_bytes());
            }
            let bad = SimosCrc
                .verify(&uncorrected)
                .unwrap()
                .iter()
                .filter(|b| !b.ok)
                .count();
            assert_eq!(bad, changed.len(), "{stem}");
            let mut a = uncorrected.clone();
            if SimosCrc.verify(&uncorrected).unwrap().iter().any(|b| b.ok) {
                SimosCrc.correct(&mut a).unwrap();
                assert_eq!(a, m, "{stem}: standalone correction");
            } else {
                // The only header (Ford / EMS3120 CAL) fails: nothing left to
                // recognise the layout by, so standalone correction refuses.
                assert!(SimosCrc.correct(&mut a).is_err(), "{stem}");
                assert_eq!(a, uncorrected);
            }
            let mut b = uncorrected;
            SimosCrc.correct_with_reference(&mut b, &ori).unwrap();
            assert_eq!(b, m, "{stem}: reference correction");
            checked += 1;
        }
        assert!(
            checked >= 1,
            "no conti-crc fixtures under OPENTUNE_FIXTURES"
        );
    }

    #[test]
    #[ignore = "needs the owner's Continental reads via OPENTUNE_FIXTURES/conti-crc"]
    fn reference_bmw_msd81_boot_header_is_not_understood() {
        let Some(stock) = fixture("bmw_msd81_07611560_ori") else {
            return;
        };
        let st = SimosCrc.verify(&stock).unwrap();
        let boot = st.iter().find(|b| b.name == "crc-hdr@0x000300").unwrap();
        assert!(!boot.ok, "the MSD81 startup-boot header does not verify");
        assert!(st.iter().any(|b| b.ok), "the CBOOT header does");
        let mut img = stock.clone();
        assert!(SimosCrc.correct(&mut img).is_err(), "standalone refuses");
        assert_eq!(img, stock);
        // With the stock read as reference, the stock failure is left alone.
        assert!(SimosCrc
            .correct_with_reference(&mut img, &stock)
            .unwrap()
            .is_empty());
    }
}
