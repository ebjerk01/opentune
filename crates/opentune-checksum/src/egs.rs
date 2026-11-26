//! Mercedes-Benz **EGS52 / EGS53** (NAG 722.6 transmission control unit) flash
//! checksum: `egs52` (Infineon SAK-C167) and `egs53` (ST10F276).
//!
//! # The scheme
//!
//! Each image carries one **descriptor** per checksummed module, at a fixed CPU
//! address:
//!
//! ```text
//! +0x00  u32 LE  (N << 16) | crc16      high 16 = range count N, low 16 = stored CRC
//! +0x04  N x { u32 LE start; u32 LE end }  inclusive 24-bit CPU addresses,
//!                                          each stored as `LL HH SS 00`
//!        u32 0xFFFF_FFFF                   terminator, right after the N-th pair
//! ```
//!
//! The CRC covers the module's ranges concatenated in descriptor order (each
//! inclusive of its end) and is a reflected **CRC-16/ARC** (poly `0x8005`, LSB-first
//! `0xA001`, refin = refout = true, xorout `0`) with a **per-platform seed**:
//! EGS52 `0x7878`, EGS53 `0xFFFF`. The descriptor is carved out of the ranges, so
//! rewriting a stored CRC never changes any module's covered bytes.
//!
//! | id      | CPU      | seed     | module A header           | module B header |
//! |---------|----------|----------|---------------------------|-----------------|
//! | `egs52` | SAK-C167 | `0x7878` | `0x90124`                 | `0xB0080`       |
//! | `egs53` | ST10F276 | `0xFFFF` | `0x202D4` or `0x20334`    | `0x8E0A4`       |
//!
//! # Provenance and validation
//!
//! Clean-room: the descriptor format and the CRC parameters were derived solely
//! from the owner's own reconstructed images (no boot code, tool or library was
//! used); CRC-16/ARC itself is a public algorithm and this is a fresh
//! implementation. Validated on **34/34 modules over 17 images** (8 EGS52 + 9
//! EGS53, two modules each), every stored CRC reproduced with a single seed per
//! platform. The fixture-gated test `reference_egs_images` re-checks them when
//! `OPENTUNE_FIXTURES` is set.
//!
//! # Layouts (file offset = CPU address − base)
//!
//! Only this explicit list is tried; the image length must equal `size` exactly.
//!
//! | id      | base      | size       | status |
//! |---------|-----------|------------|--------|
//! | `egs52` | `0x00000` | `0x100000` | the owner's images (1 MiB; CPU `0x00000..=0x8FFFF` erased, flash data at `0x90000..=0xFFFFF`) |
//! | `egs52` | `0x90000` | `0x70000`  | candidate only: a read of just the flash data, CPU `0x90000..=0xFFFFF` |
//! | `egs53` | `0x00000` | `0xC0000`  | the owner's images (768 KiB; CPU `0x00000..=0x1FFFF` erased, flash data at `0x20000..=0xBFFFF`) |
//! | `egs53` | `0x20000` | `0xA0000`  | candidate only: a read of just the flash data, CPU `0x20000..=0xBFFFF` |
//!
//! The candidate layouts have not been seen in a real read; they are tried
//! only because a tool may save just the flash data. They pass the same
//! structural checks, and correction still requires a matching module.
//!
//! A layout is **accepted** only when every module has exactly one structurally
//! valid descriptor at its documented header address(es): `1 <= N <= 16`, every
//! address byte 3 zero, `start <= end`, ranges strictly ascending and
//! non-overlapping, every range inside the image under that base, the
//! `0xFFFF_FFFF` terminator right after the N-th pair, and no range of **any**
//! module overlapping **any** descriptor `[header, terminator + 4)`. If no layout
//! (or more than one) is accepted the image is refused. There is no scanning for
//! descriptors elsewhere.
//!
//! # Recognition and correction
//!
//! * A module is [`Recognition::Recognised`] when its layout was accepted **and**
//!   at least one module in the image already has a matching stored CRC (proving
//!   base and seed for this image); otherwise [`Recognition::Consistent`].
//! * [`correct`](ChecksumAlgo::correct) is **match-first**: it writes only when
//!   the layout is accepted and at least one module already matches, and then
//!   only the low 16 bits (the first two bytes) of each stale module's stored
//!   word — the range count N is never changed. Otherwise it refuses and writes
//!   nothing.
//! * [`correct_with_reference`](ChecksumAlgo::correct_with_reference) takes the
//!   owner's untouched stock read: it must be accepted under the same layout with
//!   the same descriptors (header addresses, N and ranges) as the image and with
//!   **every** module verifying; then the image's stale modules are corrected.
//!
//! Each [`BlockStatus`] is named `<id>-A` / `<id>-B`; its `region` is the file
//! offset span from the first range's start to the last range's end (inclusive,
//! so it includes the carved-out gaps); `stored` / `computed` are the 16-bit CRC.

use crate::{
    BlockReport, BlockStatus, ChecksumAlgo, ChecksumKind, Fix, Recognition, Representation,
};
use opentune_core::{Error, MemoryRegion, Result};

/// Reflected CRC-16 with poly `0x8005` (LSB-first `0xA001`), xorout `0`, from
/// `init`. `init = 0` is CRC-16/ARC, `0xFFFF` CRC-16/MODBUS.
pub fn crc16_arc(data: &[u8], init: u16) -> u16 {
    let mut crc = init;
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// Most ranges one descriptor may declare.
pub const MAX_RANGES: usize = 16;
/// End-of-table marker after the N-th range pair.
const TERMINATOR: u32 = 0xFFFF_FFFF;

/// A candidate image layout: file offset = CPU address − `base`; the image must
/// be exactly `size` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub base: u32,
    pub size: usize,
}

#[derive(Debug)]
struct ModuleDef {
    name: &'static str,
    /// Documented CPU header address(es); exactly one must validate.
    headers: &'static [u32],
}

/// One EGS platform: its id, CRC seed, candidate layouts and modules.
#[derive(Debug)]
pub struct Platform {
    id: &'static str,
    seed: u16,
    layouts: &'static [Layout],
    modules: &'static [ModuleDef],
}

impl Platform {
    /// The algorithm id (`egs52` / `egs53`).
    pub fn id(&self) -> &'static str {
        self.id
    }

    /// The CRC-16 seed.
    pub fn seed(&self) -> u16 {
        self.seed
    }
}

/// EGS52 (SAK-C167): seed `0x7878`.
pub const EGS52: Platform = Platform {
    id: "egs52",
    seed: 0x7878,
    layouts: &[
        Layout {
            base: 0x0_0000,
            size: 0x10_0000,
        },
        Layout {
            base: 0x9_0000,
            size: 0x7_0000,
        },
    ],
    modules: &[
        ModuleDef {
            name: "A",
            headers: &[0x9_0124],
        },
        ModuleDef {
            name: "B",
            headers: &[0xB_0080],
        },
    ],
};

/// EGS53 (ST10F276): seed `0xFFFF`.
pub const EGS53: Platform = Platform {
    id: "egs53",
    seed: 0xFFFF,
    layouts: &[
        Layout {
            base: 0x0_0000,
            size: 0xC_0000,
        },
        Layout {
            base: 0x2_0000,
            size: 0xA_0000,
        },
    ],
    modules: &[
        ModuleDef {
            name: "A",
            headers: &[0x2_02D4, 0x2_0334],
        },
        ModuleDef {
            name: "B",
            headers: &[0x8_E0A4],
        },
    ],
};

/// One structurally valid module descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    /// Module name (`A`, `B`).
    pub module: &'static str,
    /// CPU address of the stored word.
    pub header: u32,
    /// Range count N (high 16 bits of the stored word).
    pub count: u16,
    /// Stored CRC (low 16 bits of the stored word).
    pub stored: u16,
    /// Covered ranges, inclusive CPU addresses, in CRC order.
    pub ranges: Vec<(u32, u32)>,
}

impl Descriptor {
    /// CPU address one past the terminator.
    fn end(&self) -> u32 {
        self.header + 4 + 8 * self.count as u32 + 4
    }

    /// Whether any range touches `[lo, hi)`.
    fn covers_any(&self, lo: u32, hi: u32) -> bool {
        self.ranges.iter().any(|&(s, e)| s < hi && lo <= e)
    }
}

/// The accepted layout and every module's descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub layout: Layout,
    pub modules: Vec<Descriptor>,
}

impl Located {
    fn off(&self, cpu: u32) -> usize {
        (cpu - self.layout.base) as usize
    }

    /// CRC over the module's concatenated ranges.
    fn compute(&self, img: &[u8], seed: u16, d: &Descriptor) -> u16 {
        d.ranges.iter().fold(seed, |crc, &(s, e)| {
            crc16_arc(&img[self.off(s)..=self.off(e)], crc)
        })
    }

    fn status(&self, img: &[u8], p: &Platform, d: &Descriptor) -> BlockStatus {
        let computed = self.compute(img, p.seed, d);
        let lo = self.off(d.ranges[0].0) as u32;
        let hi = self.off(d.ranges[d.ranges.len() - 1].1) as u32;
        BlockStatus {
            name: format!("{}-{}", p.id, d.module),
            region: MemoryRegion::new(lo, hi - lo + 1),
            stored: d.stored as u64,
            computed: computed as u64,
            ok: computed == d.stored,
        }
    }
}

fn u32_le(img: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(img.get(off..off + 4)?.try_into().ok()?))
}

/// A 24-bit CPU address stored as `LL HH SS 00`.
fn addr24(img: &[u8], off: usize) -> Option<u32> {
    u32_le(img, off).filter(|w| w >> 24 == 0)
}

/// Parse and structurally validate the descriptor at CPU `header` under
/// `layout` (the image is `layout.size` bytes). `None` on any violation.
fn parse_descriptor(
    img: &[u8],
    layout: Layout,
    module: &'static str,
    header: u32,
) -> Option<Descriptor> {
    let off = header.checked_sub(layout.base)? as usize;
    let word0 = u32_le(img, off)?;
    let count = (word0 >> 16) as u16;
    let n = count as usize;
    if !(1..=MAX_RANGES).contains(&n) {
        return None;
    }
    let mut ranges: Vec<(u32, u32)> = Vec::with_capacity(n);
    for i in 0..n {
        let s = addr24(img, off + 4 + 8 * i)?;
        let e = addr24(img, off + 8 + 8 * i)?;
        if s > e || s < layout.base || (e - layout.base) as usize >= img.len() {
            return None;
        }
        // Strictly ascending, non-overlapping.
        if matches!(ranges.last(), Some(&(_, prev_end)) if s <= prev_end) {
            return None;
        }
        ranges.push((s, e));
    }
    if u32_le(img, off + 4 + 8 * n)? != TERMINATOR {
        return None;
    }
    let d = Descriptor {
        module,
        header,
        count,
        stored: word0 as u16,
        ranges,
    };
    // Self-exclusion: no range may cover the descriptor itself.
    (!d.covers_any(d.header, d.end())).then_some(d)
}

/// The single accepted layout for `img` on platform `p`, or `None` (refuse)
/// when no candidate layout, or more than one, validates structurally.
pub fn locate(p: &Platform, img: &[u8]) -> Option<Located> {
    let mut found: Vec<Located> = Vec::new();
    'layout: for &layout in p.layouts.iter().filter(|l| l.size == img.len()) {
        let mut modules = Vec::with_capacity(p.modules.len());
        for m in p.modules {
            let mut hits = m
                .headers
                .iter()
                .filter_map(|&h| parse_descriptor(img, layout, m.name, h));
            match (hits.next(), hits.next()) {
                (Some(d), None) => modules.push(d),
                _ => continue 'layout, // none, or ambiguous
            }
        }
        // No module's ranges may cover any module's descriptor.
        for d in &modules {
            if modules.iter().any(|o| o.covers_any(d.header, d.end())) {
                continue 'layout;
            }
        }
        found.push(Located { layout, modules });
    }
    if found.len() == 1 {
        found.pop()
    } else {
        None
    }
}

fn verify_platform(p: &Platform, img: &[u8]) -> Vec<BlockStatus> {
    match locate(p, img) {
        Some(loc) => loc.modules.iter().map(|d| loc.status(img, p, d)).collect(),
        None => Vec::new(),
    }
}

fn report_platform(p: &Platform, img: &[u8], representation: Representation) -> Vec<BlockReport> {
    let Some(loc) = locate(p, img) else {
        return Vec::new();
    };
    let blocks: Vec<BlockStatus> = loc.modules.iter().map(|d| loc.status(img, p, d)).collect();
    let (representation, recognition) = if blocks.iter().any(|b| b.ok) {
        (Representation::RawFullImage, Recognition::Recognised)
    } else {
        (representation, Recognition::Consistent)
    };
    blocks
        .into_iter()
        .map(|b| BlockReport::new(b, representation, ChecksumKind::Embedded, recognition))
        .collect()
}

fn not_recognised(p: &Platform) -> Error {
    Error::Unsupported(format!(
        "{}: no documented layout validates structurally (image size, descriptor \
         addresses, range table, terminator or self-exclusion); refusing to guess",
        p.id
    ))
}

/// Write the low 16 bits of every stale module's stored word (N untouched).
fn rewrite(p: &Platform, loc: &Located, img: &mut [u8]) -> Vec<Fix> {
    let mut fixes = Vec::new();
    for d in &loc.modules {
        let computed = loc.compute(img, p.seed, d);
        if computed != d.stored {
            let at = loc.off(d.header);
            img[at..at + 2].copy_from_slice(&computed.to_le_bytes());
            fixes.push(Fix {
                name: format!("{}-{}", p.id, d.module),
                offset: at as u32,
                old: d.stored as u64,
                new: computed as u64,
            });
        }
    }
    fixes
}

fn correct_platform(p: &Platform, img: &mut [u8]) -> Result<Vec<Fix>> {
    let loc = locate(p, img).ok_or_else(|| not_recognised(p))?;
    if !loc
        .modules
        .iter()
        .any(|d| loc.compute(img, p.seed, d) == d.stored)
    {
        return Err(Error::Unsupported(format!(
            "{}: no module's stored CRC matches, so base and seed are unproven on this \
             image; refusing to write (correct against your own untouched stock read)",
            p.id
        )));
    }
    Ok(rewrite(p, &loc, img))
}

fn correct_platform_with_reference(
    p: &Platform,
    img: &mut [u8],
    reference: &[u8],
) -> Result<Vec<Fix>> {
    let loc = locate(p, img).ok_or_else(|| not_recognised(p))?;
    let Some(rloc) = locate(p, reference) else {
        return Err(Error::Unsupported(format!(
            "{}: the reference does not validate structurally; refusing",
            p.id
        )));
    };
    let same = rloc.layout == loc.layout
        && rloc.modules.len() == loc.modules.len()
        && rloc
            .modules
            .iter()
            .zip(&loc.modules)
            .all(|(r, d)| r.module == d.module && r.header == d.header && r.ranges == d.ranges);
    if !same {
        return Err(Error::Unsupported(format!(
            "{}: the reference has a different layout or descriptors than the image; refusing",
            p.id
        )));
    }
    if let Some(r) = rloc
        .modules
        .iter()
        .find(|r| rloc.compute(reference, p.seed, r) != r.stored)
    {
        return Err(Error::Unsupported(format!(
            "{}: reference module {} does not verify, so it cannot prove base and seed; \
             refusing",
            p.id, r.module
        )));
    }
    Ok(rewrite(p, &loc, img))
}

macro_rules! egs_algo {
    ($ty:ident, $platform:ident, $doc:literal) => {
        #[doc = $doc]
        pub struct $ty;

        impl ChecksumAlgo for $ty {
            fn id(&self) -> &str {
                $platform.id
            }

            fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
                Ok(verify_platform(&$platform, image))
            }

            /// Recognised when the layout is accepted and at least one module
            /// matches; otherwise Consistent (see the module docs).
            fn verify_report(
                &self,
                image: &[u8],
                representation: Representation,
            ) -> Result<Vec<BlockReport>> {
                Ok(report_platform(&$platform, image, representation))
            }

            /// Match-first (see the module docs).
            fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
                correct_platform(&$platform, image)
            }

            /// Against the owner's stock read of the same software, every module
            /// of which must verify (see the module docs).
            fn correct_with_reference(
                &self,
                image: &mut [u8],
                reference: &[u8],
            ) -> Result<Vec<Fix>> {
                correct_platform_with_reference(&$platform, image, reference)
            }
        }
    };
}

egs_algo!(
    Egs52,
    EGS52,
    "Mercedes EGS52 (SAK-C167) NAG TCU: descriptor-table CRC-16/ARC, seed `0x7878`."
);
egs_algo!(
    Egs53,
    EGS53,
    "Mercedes EGS53 (ST10F276) NAG TCU: descriptor-table CRC-16/ARC, seed `0xFFFF`."
);

#[cfg(test)]
mod tests {
    use super::*;

    const EGS52_A: &[(u32, u32)] = &[
        (0x9_0010, 0x9_00D7),
        (0x9_0228, 0x9_FFFF),
        (0xA_0000, 0xA_FFFF),
        (0xD_0000, 0xD_FFFF),
        (0xE_0000, 0xE_FFFF),
        (0xF_0000, 0xF_FFFF),
    ];
    const EGS52_B: &[(u32, u32)] = &[(0xB_00B4, 0xB_FFFF), (0xC_0000, 0xC_FFFF)];
    const EGS53_A: &[(u32, u32)] = &[
        (0x2_0010, 0x2_028F),
        (0x2_0400, 0x2_FFFF),
        (0x3_0000, 0x3_FFFF),
        (0x4_0000, 0x4_FFFF),
        (0x5_0000, 0x5_FFFF),
        (0x6_0000, 0x6_FFFF),
        (0x7_0000, 0x7_FFFF),
        (0x8_0000, 0x8_D7FF),
    ];
    const EGS53_B: &[(u32, u32)] = &[
        (0x8_D800, 0x8_E03F),
        (0x8_E130, 0x8_FFFF),
        (0x9_0000, 0x9_FFFF),
        (0xA_0000, 0xA_FFFF),
        (0xB_0000, 0xB_FFFF),
    ];

    fn noise(len: usize) -> Vec<u8> {
        let mut x: u32 = 0x1234_5678;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    /// Write a descriptor (placeholder CRC, given N) at CPU `hdr`.
    fn put_desc(img: &mut [u8], base: u32, hdr: u32, n: u16, ranges: &[(u32, u32)]) {
        let off = (hdr - base) as usize;
        img[off..off + 4].copy_from_slice(&((n as u32) << 16).to_le_bytes());
        for (i, &(s, e)) in ranges.iter().enumerate() {
            let at = off + 4 + 8 * i;
            img[at..at + 4].copy_from_slice(&s.to_le_bytes());
            img[at + 4..at + 8].copy_from_slice(&e.to_le_bytes());
        }
        let t = off + 4 + 8 * ranges.len();
        img[t..t + 4].copy_from_slice(&TERMINATOR.to_le_bytes());
    }

    /// Fill in every module's CRC with an independent concatenation.
    fn seal(p: &Platform, img: &mut [u8], base: u32, mods: &[(u32, &[(u32, u32)])]) {
        for &(hdr, ranges) in mods {
            let mut stream = Vec::new();
            for &(s, e) in ranges {
                stream.extend_from_slice(&img[(s - base) as usize..=(e - base) as usize]);
            }
            let crc = crc16_arc(&stream, p.seed);
            let off = (hdr - base) as usize;
            img[off..off + 2].copy_from_slice(&crc.to_le_bytes());
        }
    }

    fn egs52_image() -> Vec<u8> {
        let mut img = noise(0x10_0000);
        put_desc(&mut img, 0, 0x9_0124, 6, EGS52_A);
        put_desc(&mut img, 0, 0xB_0080, 2, EGS52_B);
        seal(
            &EGS52,
            &mut img,
            0,
            &[(0x9_0124, EGS52_A), (0xB_0080, EGS52_B)],
        );
        img
    }

    fn egs53_image(hdr_a: u32, a: &[(u32, u32)]) -> Vec<u8> {
        let base = 0;
        let mut img = noise(0xC_0000);
        put_desc(&mut img, base, hdr_a, a.len() as u16, a);
        put_desc(&mut img, base, 0x8_E0A4, 5, EGS53_B);
        seal(&EGS53, &mut img, base, &[(hdr_a, a), (0x8_E0A4, EGS53_B)]);
        img
    }

    fn all_supported(algo: &dyn ChecksumAlgo, img: &[u8], n: usize) {
        let rep = algo.verify_report(img, Representation::Unknown).unwrap();
        assert_eq!(rep.len(), n);
        assert!(rep
            .iter()
            .all(|r| r.is_supported() && r.representation == Representation::RawFullImage));
    }

    #[test]
    fn crc_vectors() {
        assert_eq!(crc16_arc(b"123456789", 0x0000), 0xBB3D); // CRC-16/ARC
        assert_eq!(crc16_arc(b"123456789", 0xFFFF), 0x4B37); // CRC-16/MODBUS = EGS53 seed
        assert_eq!(crc16_arc(b"123456789", 0x7878), 0x3B39); // EGS52 seed (regression)
        assert_eq!(crc16_arc(b"", 0x7878), 0x7878);
        // Chaining over split input equals one pass (concatenated ranges).
        assert_eq!(
            crc16_arc(b"6789", crc16_arc(b"12345", 0x7878)),
            crc16_arc(b"123456789", 0x7878)
        );
    }

    #[test]
    fn egs52_parses_verifies_and_is_recognised() {
        let img = egs52_image();
        let loc = locate(&EGS52, &img).unwrap();
        assert_eq!(loc.layout, EGS52.layouts[0]);
        assert_eq!(loc.modules[0].count, 6);
        assert_eq!(loc.modules[0].ranges, EGS52_A);
        assert_eq!(loc.modules[1].count, 2);
        let st = Egs52.verify(&img).unwrap();
        assert_eq!(
            st.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
            ["egs52-A", "egs52-B"]
        );
        assert!(st.iter().all(|b| b.ok));
        assert_eq!(st[0].region, MemoryRegion::new(0x9_0010, 0x6_FFF0));
        assert_eq!(st[1].region, MemoryRegion::new(0xB_00B4, 0x1_FF4C));
        all_supported(&Egs52, &img, 2);
        // The other platform's seed does not reproduce it, and egs53 refuses it.
        assert!(Egs53.verify(&img).unwrap().is_empty());
    }

    #[test]
    fn egs53_both_header_addresses_and_layouts() {
        let img = egs53_image(0x2_02D4, EGS53_A);
        let loc = locate(&EGS53, &img).unwrap();
        assert_eq!(loc.layout, EGS53.layouts[0]);
        assert_eq!(loc.modules[0].header, 0x2_02D4);
        assert_eq!(loc.modules[0].count, 8);
        all_supported(&Egs53, &img, 2);
        let st = Egs53.verify(&img).unwrap();
        assert_eq!(st[0].name, "egs53-A");
        assert_eq!(st[0].region, MemoryRegion::new(0x2_0010, 0x6_D7F0));

        // Alternate module-A header with N = 7 and a larger carve-out.
        let mut a7 = vec![(0x2_0010, 0x2_02CF), (0x2_0400, 0x3_FFFF)];
        a7.extend_from_slice(&EGS53_A[3..]);
        let alt = egs53_image(0x2_0334, &a7);
        let loc = locate(&EGS53, &alt).unwrap();
        assert_eq!((loc.modules[0].header, loc.modules[0].count), (0x2_0334, 7));
        all_supported(&Egs53, &alt, 2);

        // A read of just the flash data: file offset = CPU - 0x20000.
        let data = img[0x2_0000..].to_vec();
        assert_eq!(locate(&EGS53, &data).unwrap().layout, EGS53.layouts[1]);
        all_supported(&Egs53, &data, 2);
        assert_eq!(Egs53.verify(&data).unwrap()[0].region.address, 0x10);
        assert!(Egs52.verify(&img).unwrap().is_empty());
        assert!(Egs52.verify(&data).unwrap().is_empty());
    }

    #[test]
    fn egs52_flash_data_only_read() {
        let img = egs52_image();
        let data = img[0x9_0000..].to_vec();
        assert_eq!(locate(&EGS52, &data).unwrap().layout, EGS52.layouts[1]);
        all_supported(&Egs52, &data, 2);
        assert!(Egs53.verify(&data).unwrap().is_empty());
    }

    #[test]
    fn both_headers_valid_is_ambiguous() {
        let mut img = egs53_image(0x2_02D4, EGS53_A);
        // A second, structurally valid module-A descriptor at the alternate address
        // (its own ranges avoid both descriptors).
        put_desc(&mut img, 0, 0x2_0334, 1, &[(0x2_0400, 0x2_04FF)]);
        assert!(locate(&EGS53, &img).is_none());
    }

    fn refused(algo: &dyn ChecksumAlgo, img: &[u8]) {
        assert!(algo.verify(img).unwrap().is_empty());
        assert!(algo
            .verify_report(img, Representation::Unknown)
            .unwrap()
            .is_empty());
        let mut m = img.to_vec();
        assert!(matches!(algo.correct(&mut m), Err(Error::Unsupported(_))));
        assert_eq!(m, img, "nothing written");
    }

    #[test]
    fn structural_refusals() {
        let good = egs52_image();
        let a = 0x9_0124usize;

        // Address byte 3 non-zero.
        let mut img = good.clone();
        img[a + 4 + 3] = 0x01;
        refused(&Egs52, &img);

        // Missing terminator.
        let mut img = good.clone();
        img[a + 4 + 8 * 6] = 0x00;
        refused(&Egs52, &img);

        // N smaller than the pairs present (terminator not after the N-th pair).
        let mut img = good.clone();
        img[a + 2] = 5;
        refused(&Egs52, &img);
        // N larger than the pairs present (the terminator read as an address).
        let mut img = good.clone();
        img[a + 2] = 7;
        refused(&Egs52, &img);
        // N = 0 and erased flash.
        let mut img = good.clone();
        img[a + 2] = 0;
        refused(&Egs52, &img);
        let mut img = good.clone();
        img[a..a + 4].fill(0xFF);
        refused(&Egs52, &img);

        // Range past the end of the image (the EGS53 file ends at CPU 0xBFFFF).
        let mut b = EGS53_B.to_vec();
        b[4] = (0xB_0000, 0xC_0000);
        let mut img = egs53_image(0x2_02D4, EGS53_A);
        put_desc(&mut img, 0, 0x8_E0A4, 5, &b);
        assert!(parse_descriptor(&img, EGS53.layouts[0], "B", 0x8_E0A4).is_none());
        refused(&Egs53, &img);
        // Range below the base of a flash-data-only read (base 0x20000).
        let mut b = EGS53_B.to_vec();
        b[0] = (0x1_F000, 0x1_F0FF);
        let mut img = egs53_image(0x2_02D4, EGS53_A)[0x2_0000..].to_vec();
        put_desc(&mut img, 0x2_0000, 0x8_E0A4, 5, &b);
        assert!(parse_descriptor(&img, EGS53.layouts[1], "B", 0x8_E0A4).is_none());
        refused(&Egs53, &img);

        // start > end, descending and overlapping ranges.
        for bad in [
            [(0xB_00B4, 0xB_FFFF), (0xC_FFFF, 0xC_0000)],
            [(0xC_0000, 0xC_FFFF), (0xB_00B4, 0xB_FFFF)],
            [(0xB_00B4, 0xB_FFFF), (0xB_FFFF, 0xC_FFFF)],
        ] {
            let mut img = good.clone();
            put_desc(&mut img, 0, 0xB_0080, 2, &bad);
            refused(&Egs52, &img);
        }

        // A range covering its own descriptor.
        let mut img = good.clone();
        put_desc(
            &mut img,
            0,
            0xB_0080,
            2,
            &[(0xB_0090, 0xB_FFFF), EGS52_B[1]],
        );
        refused(&Egs52, &img);
        // A range covering the OTHER module's descriptor: B parses on its own,
        // but the layout is refused.
        let mut img = good.clone();
        put_desc(
            &mut img,
            0,
            0xB_0080,
            2,
            &[(0x9_0120, 0x9_0130), EGS52_B[1]],
        );
        assert!(parse_descriptor(&img, EGS52.layouts[0], "B", 0xB_0080).is_some());
        refused(&Egs52, &img);
    }

    #[test]
    fn wrong_size_is_refused() {
        let img = egs52_image();
        refused(&Egs52, &img[..0xF_FFFF]);
        let mut grown = img.clone();
        grown.push(0xFF);
        refused(&Egs52, &grown);
        let e53 = egs53_image(0x2_02D4, EGS53_A);
        refused(&Egs53, &e53[..0xB_0000]);
        refused(&Egs52, &e53);
        refused(&Egs52, &[]);
    }

    #[test]
    fn verify_correct_round_trip_writes_only_low_16() {
        let good = egs52_image();
        let mut img = good.clone();
        img[0xD_1234] ^= 0x5A; // edit module A data
        let st = Egs52.verify(&img).unwrap();
        assert!(!st[0].ok && st[1].ok);
        // Module B still matches: recognised, and an edited A is reported.
        let rep = Egs52.verify_report(&img, Representation::Unknown).unwrap();
        assert!(rep.iter().all(|r| r.recognition == Recognition::Recognised));
        assert!(!rep[0].is_supported() && rep[1].is_supported());

        let fixes = Egs52.correct(&mut img).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].name, "egs52-A");
        assert_eq!(fixes[0].offset, 0x9_0124);
        assert_eq!(fixes[0].old, st[0].stored);
        assert_eq!(fixes[0].new, st[0].computed);
        assert!(Egs52.verify(&img).unwrap().iter().all(|b| b.ok));
        // Only the edited byte and the two low CRC bytes changed; N untouched.
        let diff: Vec<usize> = (0..img.len()).filter(|&i| img[i] != good[i]).collect();
        assert!(diff
            .iter()
            .all(|&i| i == 0xD_1234 || i == 0x9_0124 || i == 0x9_0125));
        assert_eq!(img[0x9_0126..0x9_0128], [6, 0]);
        assert!(Egs52.correct(&mut img).unwrap().is_empty(), "idempotent");
    }

    #[test]
    fn match_first_refuses_when_every_module_is_stale() {
        let mut img = egs53_image(0x2_02D4, EGS53_A);
        img[0x3_0000] ^= 1; // module A data
        img[0x9_5000] ^= 1; // module B data
        let rep = Egs53.verify_report(&img, Representation::Unknown).unwrap();
        assert_eq!(rep.len(), 2);
        assert!(rep
            .iter()
            .all(|r| r.recognition == Recognition::Consistent && !r.status.ok));
        let before = img.clone();
        assert!(matches!(
            Egs53.correct(&mut img),
            Err(Error::Unsupported(_))
        ));
        assert_eq!(img, before, "nothing written");
    }

    #[test]
    fn reference_correction() {
        let stock = egs53_image(0x2_02D4, EGS53_A);
        let mut tuned = stock.clone();
        tuned[0x3_0000] ^= 1;
        tuned[0x9_5000] ^= 1;
        let fixes = Egs53.correct_with_reference(&mut tuned, &stock).unwrap();
        assert_eq!(fixes.len(), 2);
        assert_eq!(fixes[0].offset, 0x2_02D4);
        assert_eq!(fixes[1].offset, 0x8_E0A4);
        assert!(Egs53.verify(&tuned).unwrap().iter().all(|b| b.ok));
        assert_eq!(tuned[0x2_02D6..0x2_02D8], [8, 0], "N untouched");
        assert_eq!(tuned[0x8_E0A6..0x8_E0A8], [5, 0], "N untouched");

        // A reference with a stale module is refused.
        let mut broken = stock.clone();
        broken[0x9_5000] ^= 1;
        let mut t = stock.clone();
        t[0x3_0000] ^= 1;
        let before = t.clone();
        assert!(Egs53.correct_with_reference(&mut t, &broken).is_err());
        assert_eq!(t, before);

        // A reference with different descriptors (other module-A header) is refused.
        let mut a7 = vec![(0x2_0010, 0x2_02CF), (0x2_0400, 0x3_FFFF)];
        a7.extend_from_slice(&EGS53_A[3..]);
        let other = egs53_image(0x2_0334, &a7);
        assert!(Egs53.correct_with_reference(&mut t, &other).is_err());
        // A reference of another layout (flash-data-only read) is refused.
        let data = stock[0x2_0000..].to_vec();
        assert!(Egs53.correct_with_reference(&mut t, &data).is_err());
        // An unrecognised image is refused even with a good reference.
        let mut junk = vec![0u8; 0xC_0000];
        assert!(Egs53.correct_with_reference(&mut junk, &stock).is_err());
        assert_eq!(t, before);
    }

    /// The owner's own reconstructed images (never committed):
    ///   OPENTUNE_FIXTURES=/path/to/fixtures \
    ///     cargo test -p opentune-checksum -- --ignored egs
    /// reads `$OPENTUNE_FIXTURES/egs/egs52/*.bin` and `egs/egs53/*.bin`. Every
    /// module must verify and be recognised, and wiping one module's CRC must
    /// be restored byte-for-byte by `correct`. A no-op without the variable.
    #[test]
    #[ignore = "needs the owner's EGS images via OPENTUNE_FIXTURES"]
    fn reference_egs_images() {
        let Ok(dir) = std::env::var("OPENTUNE_FIXTURES") else {
            return;
        };
        let algos: [(&str, &dyn ChecksumAlgo, &Platform); 2] =
            [("egs52", &Egs52, &EGS52), ("egs53", &Egs53, &EGS53)];
        let mut modules = 0;
        for (sub, algo, p) in algos {
            let Ok(entries) = std::fs::read_dir(format!("{dir}/egs/{sub}")) else {
                continue;
            };
            let mut paths: Vec<_> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "bin"))
                .collect();
            paths.sort();
            for (i, path) in paths.iter().enumerate() {
                let img = std::fs::read(path).unwrap();
                let rep = algo.verify_report(&img, Representation::Unknown).unwrap();
                assert_eq!(rep.len(), 2, "{sub} image #{i}: both modules located");
                for r in &rep {
                    assert!(
                        r.is_supported(),
                        "{sub} image #{i} {}: stored {:04X} computed {:04X}",
                        r.status.name,
                        r.status.stored,
                        r.status.computed
                    );
                }
                modules += rep.len();
                assert!(algo.correct(&mut img.clone()).unwrap().is_empty());
                let loc = locate(p, &img).unwrap();
                for d in &loc.modules {
                    let at = loc.off(d.header);
                    let mut m = img.clone();
                    m[at] ^= 0xFF;
                    let fixes = algo.correct(&mut m).unwrap();
                    assert_eq!(fixes.len(), 1);
                    assert_eq!(m, img, "{sub} image #{i}: CRC restored byte-for-byte");
                }
                let other = if sub == "egs52" {
                    &Egs53 as &dyn ChecksumAlgo
                } else {
                    &Egs52
                };
                assert!(
                    other.verify(&img).unwrap().is_empty(),
                    "no cross-platform hit"
                );
            }
        }
        eprintln!("egs fixtures: {modules} modules recognised and matching");
    }
}
