//! **Subaru transmission (TCU)** flash checksums: the Hitachi M32R automatic
//! TCU, the Denso SH7055 automatic TCU and the Mitsubishi MH8104 CVT TCU.
//!
//! All three are *balance-word* schemes over a full 512 KiB (`0x8_0000`-byte)
//! read: the image's big-endian word sum must hit a fixed target, and one
//! stored word (the balance) is chosen so that it does. The M32R TCU adds a
//! second field holding the two's complement of a second word sum.
//!
//! # Provenance (open-source port)
//!
//! Re-derived from the open-source FastECU flasher (GPL-3.0); the arithmetic,
//! offsets and targets below are facts read from these files, the Rust is
//! written fresh (no code or comments copied):
//!
//! * M32R — <https://github.com/miikasyvanen/FastECU/blob/master/modules/checksum/checksum_tcu_subaru_hitachi_m32r_can.cpp>
//! * SH7055 — <https://github.com/miikasyvanen/FastECU/blob/master/modules/checksum/checksum_tcu_subaru_denso_sh7055.cpp>
//! * MH8104 — <https://github.com/miikasyvanen/FastECU/blob/master/modules/checksum/checksum_tcu_mitsu_mh8104_can.cpp>
//!
//! Which flash protocol selects which routine is in FastECU's `file_actions.cpp`
//! (`sub_tcu_hitachi_m32r_can` / `_kline` → M32R, `sub_tcu_denso_sh7055_can` →
//! SH7055, `sub_tcu_cvt_mitsu_mh8104_can` → MH8104); the image size comes from
//! `config/protocols.cfg` (MCU `M32R_512KB`, `SH7055`, `MH8104`) and the ROM
//! sizes in `kernelmemorymodels.h` (all `512 * 1024`).
//!
//! OpenTune is GPL-3.0-or-later, compatible with FastECU's GPL-3.0. FastECU's
//! `LICENSE` is the unmodified GPL-3.0 text and the cited files carry no
//! per-file copyright header, so the notice retained here is the project's:
//!
//! ```text
//!   FastECU  Copyright (c) miikasyvanen and FastECU contributors.  GPL-3.0.
//!   https://github.com/miikasyvanen/FastECU
//! ```
//!
//! # Algorithms
//!
//! Every word is **big-endian**; every sum wraps (mod 2³² or 2¹⁶). The image
//! must be exactly `0x8_0000` bytes; anything else is refused.
//!
//! | id                  | covered words | stored field | target |
//! |---------------------|---------------|--------------|--------|
//! | `subaru-tcu-m32r`   | sum 1: 32-bit words `[0x8020, 0x8_0000)` | balance @`0x8020` (inside sum 1) | `0x5AA5_A55A` |
//! |                     | sum 2: 32-bit words `[0, 0x8_0000)` minus `0x8000..0x8008` | `-(sum 2)` @`0x8000`, copy @`0x8004` | — |
//! | `subaru-tcu-sh7055` | 16-bit words over 12 fixed regions (`0x1000..0x8_0000`, four gaps) | balance @`0x7_FFF4` (inside region 12) | `0x5AA5` |
//! | `subaru-tcu-mh8104` | 32-bit words `[0x8000, 0x8_0000)`, then `- 0xFFFF` and five `- 0xFFFF_FFFF` | balance @`0x81FC` | `0x5AA4_5AAB` |
//!
//! * **M32R.** Sum 2 covers `0x8020` (the sum-1 balance), so the balance is
//!   settled first and sum 2 taken afterwards. (Once corrected, an edit at or
//!   above `0x8020` is absorbed by the balance and sum 2 returns to its old
//!   value; only edits below `0x8020` change it for good.) The stored sum-2
//!   field is
//!   the two's complement of sum 2, built byte by byte upstream as
//!   `(0xFF-b3, 0xFF-b2, 0xFF-b1, 0x100-b0)`. When the low byte `b0` of sum 2
//!   is non-zero that is exactly `sum2.wrapping_neg()` (no carry can
//!   arise). When `b0 == 0` the upstream byte `0x100` truncates to `0x00` and
//!   the carry into `b1` is dropped, giving `!sum2 & 0xFFFF_FF00`, which is
//!   `0x100` less than the true negation. Nothing shows which of those
//!   two values the TCU itself checks, so with a zero low byte **both are
//!   treated as unknown**: `verify` accepts a stored field equal to either (and
//!   says which in the computed column). `correct_with_reference` never writes a
//!   sum-2 field in that case. It refuses unless both copies already hold the
//!   same candidate. FastECU writes the sum-2 value to `0x8000` **and** `0x8004`
//!   (it reads only `0x8000`), so the copy at `0x8004` is reported as its own
//!   block.
//! * **SH7055.** The region table is `(start, length)` with the length in
//!   **16-bit words** (FastECU doubles it). In words the twelve regions chain
//!   end-to-start into `0x1000..0x2000`, `0x3000..0x3074`, `0x3080..0x7_FAFC`
//!   and `0x7_FB80..0x8_0000`, ending exactly at the 512 KiB image end. Read as
//!   byte counts they would leave unexplained gaps between regions, which
//!   confirms the word reading. Gaps `0x0..0x1000`, `0x2000..0x3000`,
//!   `0x3074..0x3080` and `0x7_FAFC..0x7_FB80` are not covered.
//! * **MH8104.** `sum - 0xFFFF - 5·0xFFFF_FFFF ≡ sum - 0xFFFA (mod 2³²)`,
//!   compared against `0x5AA4_5AAB`. The raw word sum must therefore equal
//!   `0x5AA5_5AA5`. The fixed adjustments are kept in their upstream form in
//!   the code and pinned to the effective target by a test.
//!
//! # Validation status and anti-brick stance
//!
//! No real dump of any of these TCUs was available. The algorithms are tested
//! only with synthetic images and hand-computable vectors (an all-zero image,
//! for example, needs balance `0x5AA5` on SH7055 and `0x5AA5_5AA5` on MH8104).
//! Every result is therefore [`Recognition::Consistent`] and never "supported",
//! even when the image validates.
//!
//! The layouts are fixed offsets with no self-describing header, which is the
//! same situation as `tcu::Zf8hpCal`, so the same policy applies:
//!
//! * [`correct`](ChecksumAlgo::correct) does nothing when the image already
//!   validates and otherwise **refuses**. A mismatch at a fixed offset is
//!   ambiguous: it could be an edited image or a different firmware/layout, and
//!   writing a balance word on a guess could brick the unit.
//! * [`correct_with_reference`](ChecksumAlgo::correct_with_reference) corrects
//!   an edited image only when the owner's known-good stock dump is
//!   **exactly 512 KiB and validates in full** with the same algorithm. That
//!   proves the offsets and targets for that firmware. Only then are the
//!   balance/sum fields rewritten. The M32R zero-low-byte case is refused even
//!   with a reference (see above), and nothing is written on any refusal.
//!
//! A value that cannot be computed is never guessed.
use crate::{
    BlockReport, BlockStatus, ChecksumAlgo, ChecksumKind, Fix, Recognition, Representation,
};
use opentune_core::{Error, MemoryRegion, Result};

/// The only image size any of the three routines accepts: a full 512 KiB read.
pub const IMAGE_LEN: usize = 0x8_0000;

// ---------------------------------------------------------------------------
// Shared primitives
// ---------------------------------------------------------------------------

#[inline]
fn be32(img: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([img[off], img[off + 1], img[off + 2], img[off + 3]])
}

#[inline]
fn be16(img: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([img[off], img[off + 1]])
}

/// Wrapping sum of the big-endian 32-bit words in `bytes` (length a multiple of 4).
pub fn sum_be32(bytes: &[u8]) -> u32 {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .fold(0u32, |acc, w| acc.wrapping_add(u32::from_be_bytes(*w)))
}

/// Wrapping sum of the big-endian 16-bit words in `bytes` (length a multiple of 2).
pub fn sum_be16(bytes: &[u8]) -> u16 {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .fold(0u16, |acc, w| acc.wrapping_add(u16::from_be_bytes(*w)))
}

fn wrong_size(id: &str, len: usize) -> Error {
    Error::Unsupported(format!(
        "{id}: image is {len:#X} bytes; only a full 512 KiB ({IMAGE_LEN:#X}-byte) read has \
         the documented layout, refusing to guess (see crate::subaru_tcu docs)"
    ))
}

fn standalone_refusal(id: &str) -> Error {
    Error::Unsupported(format!(
        "{id}: checksum does not match and no reference was given. The fixed-offset layout \
         is unconfirmed (no real dump on hand), so no balance word is written. Pass the \
         owner's known-good stock dump via correct_with_reference (see crate::subaru_tcu docs)"
    ))
}

fn bad_reference(id: &str) -> Error {
    Error::Unsupported(format!(
        "{id}: the reference image is not a full 512 KiB read that validates in full with \
         this algorithm. The layout cannot be confirmed, so nothing is corrected (see \
         crate::subaru_tcu docs)"
    ))
}

/// `true` when `algo.verify(img)` yields at least one block and every block is OK.
fn validates(algo: &dyn ChecksumAlgo, img: &[u8]) -> bool {
    matches!(algo.verify(img), Ok(b) if !b.is_empty() && b.iter().all(|s| s.ok))
}

/// Every block as a full-image, embedded, **merely consistent** report: there is
/// no real dump behind these algorithms, so nothing is ever "supported".
fn consistent_reports(blocks: Vec<BlockStatus>) -> Vec<BlockReport> {
    blocks
        .into_iter()
        .map(|status| {
            BlockReport::new(
                status,
                Representation::RawFullImage,
                ChecksumKind::Embedded,
                Recognition::Consistent,
            )
        })
        .collect()
}

/// Standalone correction shared by all three: no-op when the image already
/// validates, otherwise refuse (anti-brick; see the module docs).
fn correct_standalone(algo: &dyn ChecksumAlgo, image: &[u8]) -> Result<Vec<Fix>> {
    if image.len() != IMAGE_LEN {
        return Err(wrong_size(algo.id(), image.len()));
    }
    if validates(algo, image) {
        Ok(Vec::new())
    } else {
        Err(standalone_refusal(algo.id()))
    }
}

/// Reference/size gate shared by the three `correct_with_reference` impls.
fn check_reference(algo: &dyn ChecksumAlgo, image: &[u8], reference: &[u8]) -> Result<()> {
    if image.len() != IMAGE_LEN {
        return Err(wrong_size(algo.id(), image.len()));
    }
    if reference.len() != IMAGE_LEN || !validates(algo, reference) {
        return Err(bad_reference(algo.id()));
    }
    Ok(())
}

/// Write a big-endian 32-bit field if it differs, recording the [`Fix`].
fn set_be32(img: &mut [u8], off: usize, new: u32, name: &str, fixes: &mut Vec<Fix>) {
    let old = be32(img, off);
    if old != new {
        img[off..off + 4].copy_from_slice(&new.to_be_bytes());
        fixes.push(Fix {
            name: name.into(),
            offset: off as u32,
            old: old as u64,
            new: new as u64,
        });
    }
}

// ---------------------------------------------------------------------------
// Subaru Hitachi M32R TCU (M32176F4, 512 KiB) — two 32-bit word sums
// ---------------------------------------------------------------------------

/// Sum 1 covers the 32-bit words from here to the image end.
const M32R_SUM1_START: usize = 0x8020;
/// Sum 1 balance word (the first word sum 1 covers).
const M32R_BAL1_OFF: usize = 0x8020;
/// Sum 1 must equal this.
const M32R_SUM1_TARGET: u32 = 0x5AA5_A55A;
/// Stored sum-2 field (two's complement of sum 2).
const M32R_SUM2_OFF: usize = 0x8000;
/// Copy of the sum-2 field that FastECU also writes.
const M32R_SUM2_COPY_OFF: usize = 0x8004;
/// Sum 2 covers every word except `[0x8000, 0x8008)` (its own two fields).
const M32R_SUM2_HOLE: (usize, usize) = (0x8000, 0x8008);

fn m32r_sum1(img: &[u8]) -> u32 {
    sum_be32(&img[M32R_SUM1_START..IMAGE_LEN])
}

fn m32r_sum2(img: &[u8]) -> u32 {
    sum_be32(&img[..M32R_SUM2_HOLE.0]).wrapping_add(sum_be32(&img[M32R_SUM2_HOLE.1..IMAGE_LEN]))
}

/// What the stored sum-2 field should hold for a given sum 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum M32rSum2Expect {
    /// Low byte of sum 2 non-zero: true negation and the upstream bytewise form
    /// agree, so the value is unambiguous.
    Exact(u32),
    /// Low byte of sum 2 is zero: true negation and upstream's carry-less
    /// bytewise form differ by `0x100`, and nothing confirms which one the TCU
    /// checks.
    Ambiguous { negation: u32, upstream: u32 },
}

fn m32r_sum2_expect(sum2: u32) -> M32rSum2Expect {
    if sum2 & 0xFF != 0 {
        M32rSum2Expect::Exact(sum2.wrapping_neg())
    } else {
        M32rSum2Expect::Ambiguous {
            negation: sum2.wrapping_neg(),
            upstream: !sum2 & 0xFFFF_FF00,
        }
    }
}

/// **Subaru Hitachi M32R** automatic TCU (M32176F4, 512 KiB, CAN and K-line
/// flash methods): sum 1 with a balance word at `0x8020` → `0x5AA5_A55A`, and
/// the two's complement of sum 2 stored at `0x8000` (copy at `0x8004`).
///
/// Synthetic-tested only. Standalone correct refuses a mismatch; correct with a
/// validating stock reference; a zero low byte in sum 2 is never corrected (see
/// the [module docs](self)).
pub struct SubaruTcuM32r;

impl ChecksumAlgo for SubaruTcuM32r {
    fn id(&self) -> &str {
        "subaru-tcu-m32r"
    }

    /// Three blocks: `m32r-sum1` (balance @`0x8020`; `computed` = the balance
    /// that would make sum 1 hit the target), `m32r-sum2` (@`0x8000`) and
    /// `m32r-sum2-copy` (@`0x8004`). For the sum-2 fields `computed` is the
    /// expected stored value. When sum 2's low byte is zero and the stored
    /// `0x8000` field equals one of the two candidates, that candidate is
    /// shown. Otherwise the true negation is shown. Wrong-size image → no blocks.
    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        if image.len() != IMAGE_LEN {
            return Ok(Vec::new());
        }
        let sum1 = m32r_sum1(image);
        let bal = be32(image, M32R_BAL1_OFF);
        let bal_required = bal.wrapping_add(M32R_SUM1_TARGET.wrapping_sub(sum1));

        let primary = be32(image, M32R_SUM2_OFF);
        let copy = be32(image, M32R_SUM2_COPY_OFF);
        let expected = match m32r_sum2_expect(m32r_sum2(image)) {
            M32rSum2Expect::Exact(e) => e,
            M32rSum2Expect::Ambiguous { negation, upstream } => {
                if primary == upstream {
                    upstream
                } else {
                    negation
                }
            }
        };
        let whole = MemoryRegion::new(0, IMAGE_LEN as u32);
        Ok(vec![
            BlockStatus {
                name: "m32r-sum1".into(),
                region: MemoryRegion::new(
                    M32R_SUM1_START as u32,
                    (IMAGE_LEN - M32R_SUM1_START) as u32,
                ),
                stored: bal as u64,
                computed: bal_required as u64,
                ok: sum1 == M32R_SUM1_TARGET,
            },
            BlockStatus {
                name: "m32r-sum2".into(),
                region: whole,
                stored: primary as u64,
                computed: expected as u64,
                ok: primary == expected,
            },
            BlockStatus {
                name: "m32r-sum2-copy".into(),
                region: whole,
                stored: copy as u64,
                computed: expected as u64,
                ok: copy == expected,
            },
        ])
    }

    fn verify_report(
        &self,
        image: &[u8],
        _representation: Representation,
    ) -> Result<Vec<BlockReport>> {
        Ok(consistent_reports(self.verify(image)?))
    }

    /// No-op when the image validates; otherwise refuses (see the module docs).
    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        correct_standalone(self, image)
    }

    /// Correct after a validating stock reference confirms the layout. Order
    /// matters: the sum-1 balance (which sum 2 covers) is settled first, then
    /// sum 2 is taken and both of its fields written. All-or-nothing: on a
    /// refusal the image is left untouched.
    fn correct_with_reference(&self, image: &mut [u8], reference: &[u8]) -> Result<Vec<Fix>> {
        check_reference(self, image, reference)?;
        let mut work = image.to_vec();
        let mut fixes = Vec::new();

        let sum1 = m32r_sum1(&work);
        let bal = be32(&work, M32R_BAL1_OFF).wrapping_add(M32R_SUM1_TARGET.wrapping_sub(sum1));
        set_be32(&mut work, M32R_BAL1_OFF, bal, "m32r-sum1", &mut fixes);

        let expected = match m32r_sum2_expect(m32r_sum2(&work)) {
            M32rSum2Expect::Exact(e) => e,
            M32rSum2Expect::Ambiguous { negation, upstream } => {
                let p = be32(&work, M32R_SUM2_OFF);
                let c = be32(&work, M32R_SUM2_COPY_OFF);
                if p == c && (p == negation || p == upstream) {
                    p
                } else {
                    return Err(Error::Unsupported(format!(
                        "subaru-tcu-m32r: the corrected sum 2 has a zero low byte, so the \
                         stored field is ambiguous ({negation:#010X} true negation vs \
                         {upstream:#010X} FastECU's carry-less bytewise form) and no real \
                         dump shows which one the TCU checks. Refusing to write \
                         (see crate::subaru_tcu docs)"
                    )));
                }
            }
        };
        set_be32(&mut work, M32R_SUM2_OFF, expected, "m32r-sum2", &mut fixes);
        set_be32(
            &mut work,
            M32R_SUM2_COPY_OFF,
            expected,
            "m32r-sum2-copy",
            &mut fixes,
        );
        image.copy_from_slice(&work);
        Ok(fixes)
    }
}

// ---------------------------------------------------------------------------
// Subaru Denso SH7055 TCU (512 KiB) — 16-bit word sum over 12 regions
// ---------------------------------------------------------------------------

/// `(start, length in 16-bit WORDS)`; each region covers
/// `start .. start + 2 * words`.
const SH7055_REGIONS: [(usize, usize); 12] = [
    (0x0_1000, 0x0800),
    (0x0_3000, 0x003A),
    (0x0_3080, 0x4C00),
    (0x0_C880, 0x83F6),
    (0x1_D06C, 0x83F6),
    (0x2_D858, 0x83F8),
    (0x3_E048, 0x0FDC),
    (0x4_0000, 0x741C),
    (0x4_E838, 0x83F6),
    (0x5_F024, 0x83F8),
    (0x6_F814, 0x8174),
    (0x7_FB80, 0x0240),
];
/// Balance word (inside the last region).
const SH7055_BAL_OFF: usize = 0x7_FFF4;
/// The region sum must equal this.
const SH7055_TARGET: u16 = 0x5AA5;

fn sh7055_sum(img: &[u8]) -> u16 {
    SH7055_REGIONS.iter().fold(0u16, |acc, &(start, words)| {
        acc.wrapping_add(sum_be16(&img[start..start + 2 * words]))
    })
}

/// **Subaru Denso SH7055** automatic TCU (512 KiB): 16-bit big-endian word
/// sum over 12 fixed regions must equal `0x5AA5`; balance word at `0x7_FFF4`.
///
/// Synthetic-tested only. Standalone correct refuses a mismatch; correct with a
/// validating stock reference (see the [module docs](self)).
pub struct SubaruTcuSh7055;

impl ChecksumAlgo for SubaruTcuSh7055 {
    fn id(&self) -> &str {
        "subaru-tcu-sh7055"
    }

    /// One block `sh7055-sum16`: `stored` = balance word, `computed` = the
    /// balance that makes the sum hit `0x5AA5`. The region shown is the span
    /// `0x1000..0x8_0000`; the four gaps inside it are not covered. Wrong-size
    /// image → no blocks.
    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        if image.len() != IMAGE_LEN {
            return Ok(Vec::new());
        }
        let sum = sh7055_sum(image);
        let bal = be16(image, SH7055_BAL_OFF);
        let required = bal.wrapping_add(SH7055_TARGET.wrapping_sub(sum));
        let first = SH7055_REGIONS[0].0;
        Ok(vec![BlockStatus {
            name: "sh7055-sum16".into(),
            region: MemoryRegion::new(first as u32, (IMAGE_LEN - first) as u32),
            stored: bal as u64,
            computed: required as u64,
            ok: sum == SH7055_TARGET,
        }])
    }

    fn verify_report(
        &self,
        image: &[u8],
        _representation: Representation,
    ) -> Result<Vec<BlockReport>> {
        Ok(consistent_reports(self.verify(image)?))
    }

    /// No-op when the image validates; otherwise refuses (see the module docs).
    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        correct_standalone(self, image)
    }

    /// Rewrite the balance word after a validating stock reference confirms
    /// the layout.
    fn correct_with_reference(&self, image: &mut [u8], reference: &[u8]) -> Result<Vec<Fix>> {
        check_reference(self, image, reference)?;
        let sum = sh7055_sum(image);
        let old = be16(image, SH7055_BAL_OFF);
        let new = old.wrapping_add(SH7055_TARGET.wrapping_sub(sum));
        let mut fixes = Vec::new();
        if new != old {
            image[SH7055_BAL_OFF..SH7055_BAL_OFF + 2].copy_from_slice(&new.to_be_bytes());
            fixes.push(Fix {
                name: "sh7055-sum16".into(),
                offset: SH7055_BAL_OFF as u32,
                old: old as u64,
                new: new as u64,
            });
        }
        Ok(fixes)
    }
}

// ---------------------------------------------------------------------------
// Subaru CVT TCU on Mitsubishi MH8104 (512 KiB) — adjusted 32-bit word sum
// ---------------------------------------------------------------------------

/// Covered range `[start, end)` of 32-bit words.
const MH8104_START: usize = 0x8000;
const MH8104_END: usize = 0x8_0000;
/// Balance word (inside the covered range).
const MH8104_BAL_OFF: usize = 0x81FC;
/// The adjusted sum must equal this.
const MH8104_TARGET: u32 = 0x5AA4_5AAB;

/// The covered word sum with FastECU's fixed adjustments applied
/// (`- 0xFFFF`, then `- 0xFFFF_FFFF` five times, all mod 2³²).
fn mh8104_checksum(img: &[u8]) -> u32 {
    let mut c = sum_be32(&img[MH8104_START..MH8104_END]).wrapping_sub(0xFFFF);
    for _ in 0..5 {
        c = c.wrapping_sub(0xFFFF_FFFF);
    }
    c
}

/// **Subaru CVT TCU on a Mitsubishi MH8104** (512 KiB, e.g. 2010-2014 Legacy
/// CVT): 32-bit big-endian word sum over `[0x8000, 0x8_0000)` with fixed
/// adjustments must equal `0x5AA4_5AAB`; balance word at `0x81FC`.
///
/// Synthetic-tested only. Standalone correct refuses a mismatch; correct with a
/// validating stock reference (see the [module docs](self)).
pub struct SubaruTcuMh8104;

impl ChecksumAlgo for SubaruTcuMh8104 {
    fn id(&self) -> &str {
        "subaru-tcu-mh8104"
    }

    /// One block `mh8104-sum32`: `stored` = balance word, `computed` = the
    /// balance that makes the adjusted sum hit the target. Wrong-size image → no
    /// blocks.
    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
        if image.len() != IMAGE_LEN {
            return Ok(Vec::new());
        }
        let c = mh8104_checksum(image);
        let bal = be32(image, MH8104_BAL_OFF);
        let required = bal.wrapping_add(MH8104_TARGET.wrapping_sub(c));
        Ok(vec![BlockStatus {
            name: "mh8104-sum32".into(),
            region: MemoryRegion::new(MH8104_START as u32, (MH8104_END - MH8104_START) as u32),
            stored: bal as u64,
            computed: required as u64,
            ok: c == MH8104_TARGET,
        }])
    }

    fn verify_report(
        &self,
        image: &[u8],
        _representation: Representation,
    ) -> Result<Vec<BlockReport>> {
        Ok(consistent_reports(self.verify(image)?))
    }

    /// No-op when the image validates; otherwise refuses (see the module docs).
    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        correct_standalone(self, image)
    }

    /// Rewrite the balance word after a validating stock reference confirms
    /// the layout.
    fn correct_with_reference(&self, image: &mut [u8], reference: &[u8]) -> Result<Vec<Fix>> {
        check_reference(self, image, reference)?;
        let c = mh8104_checksum(image);
        let new = be32(image, MH8104_BAL_OFF).wrapping_add(MH8104_TARGET.wrapping_sub(c));
        let mut fixes = Vec::new();
        set_be32(image, MH8104_BAL_OFF, new, "mh8104-sum32", &mut fixes);
        Ok(fixes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic non-trivial filler.
    fn pattern() -> Vec<u8> {
        (0..IMAGE_LEN)
            .map(|i| (i as u8).wrapping_mul(37).wrapping_add((i >> 9) as u8))
            .collect()
    }

    fn all_ok(algo: &dyn ChecksumAlgo, img: &[u8]) -> bool {
        validates(algo, img)
    }

    fn block<'a>(blocks: &'a [BlockStatus], name: &str) -> &'a BlockStatus {
        blocks.iter().find(|b| b.name == name).unwrap()
    }

    fn put32(img: &mut [u8], off: usize, v: u32) {
        img[off..off + 4].copy_from_slice(&v.to_be_bytes());
    }

    fn is_unsupported<T: std::fmt::Debug>(r: Result<T>) -> bool {
        matches!(r, Err(Error::Unsupported(_)))
    }

    // ---- shared ----------------------------------------------------------

    #[test]
    fn word_sums_are_big_endian_and_wrap() {
        assert_eq!(sum_be32(&[0x01, 0x02, 0x03, 0x04, 0, 0, 0, 1]), 0x0102_0305);
        assert_eq!(sum_be32(&[0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 2]), 1);
        assert_eq!(sum_be16(&[0x12, 0x34, 0x00, 0x01]), 0x1235);
        assert_eq!(sum_be16(&[0xFF, 0xFF, 0x00, 0x02]), 1);
    }

    #[test]
    fn ids_are_distinct() {
        let ids = [
            SubaruTcuM32r.id().to_owned(),
            SubaruTcuSh7055.id().to_owned(),
            SubaruTcuMh8104.id().to_owned(),
        ];
        assert_eq!(
            ids,
            ["subaru-tcu-m32r", "subaru-tcu-sh7055", "subaru-tcu-mh8104"]
        );
    }

    #[test]
    fn every_algo_refuses_wrong_sizes() {
        let algos: [&dyn ChecksumAlgo; 3] = [&SubaruTcuM32r, &SubaruTcuSh7055, &SubaruTcuMh8104];
        // 0x78000 is the length FastECU's MH8104 reader is hard-coded to as a
        // test hack; 1 MiB is the SH7058 TCU size; ±1 byte for good measure.
        for len in [0, 0x1000, 0x7_8000, IMAGE_LEN - 1, IMAGE_LEN + 1, 0x10_0000] {
            for algo in algos {
                let mut img = vec![0u8; len];
                assert!(
                    algo.verify(&img).unwrap().is_empty(),
                    "{} {len:#X}",
                    algo.id()
                );
                assert!(algo
                    .verify_report(&img, Representation::Unknown)
                    .unwrap()
                    .is_empty());
                assert!(
                    is_unsupported(algo.correct(&mut img)),
                    "{} {len:#X}",
                    algo.id()
                );
                let good_ref = vec![0u8; IMAGE_LEN]; // validity irrelevant: target size refused first
                assert!(is_unsupported(
                    algo.correct_with_reference(&mut img, &good_ref)
                ));
                assert_eq!(img, vec![0u8; len], "nothing written");
            }
        }
    }

    // ---- M32R ------------------------------------------------------------

    /// Settle the sum-1 balance, then write sum 2's negation to both fields.
    /// Panics if sum 2 lands on a zero low byte (tests pick images that don't).
    fn seal_m32r(img: &mut [u8]) {
        let bal =
            be32(img, M32R_BAL1_OFF).wrapping_add(M32R_SUM1_TARGET.wrapping_sub(m32r_sum1(img)));
        put32(img, M32R_BAL1_OFF, bal);
        let s2 = m32r_sum2(img);
        assert_ne!(s2 & 0xFF, 0, "test image must avoid the ambiguous case");
        put32(img, M32R_SUM2_OFF, s2.wrapping_neg());
        put32(img, M32R_SUM2_COPY_OFF, s2.wrapping_neg());
    }

    fn m32r_stock() -> Vec<u8> {
        let mut img = pattern();
        seal_m32r(&mut img);
        img
    }

    #[test]
    fn m32r_zero_image_vector() {
        // All-zero image: sum 1 = 0 → balance 0x5AA5A55A; sum 2 then equals
        // the balance, whose two's complement is 0xA55A5AA6
        // (0x5AA5A55A + 0xA55A5AA6 = 2^32).
        let mut img = vec![0u8; IMAGE_LEN];
        seal_m32r(&mut img);
        assert_eq!(be32(&img, 0x8020), 0x5AA5_A55A);
        assert_eq!(be32(&img, 0x8000), 0xA55A_5AA6);
        assert_eq!(be32(&img, 0x8004), 0xA55A_5AA6);
        let algo = SubaruTcuM32r;
        let blocks = algo.verify(&img).unwrap();
        assert_eq!(blocks.len(), 3);
        assert!(blocks.iter().all(|b| b.ok));
        assert!(algo.correct(&mut img.clone()).unwrap().is_empty());
        let rep = algo.verify_report(&img, Representation::Unknown).unwrap();
        assert!(rep.iter().all(|r| r.recognition == Recognition::Consistent
            && r.representation == Representation::RawFullImage
            && r.checksum_kind == ChecksumKind::Embedded
            && !r.is_supported()));
    }

    #[test]
    fn m32r_negation_equals_upstream_bytewise_form_when_low_byte_nonzero() {
        // Independent bytewise model of FastECU's construction:
        // (0xFF-b3, 0xFF-b2, 0xFF-b1, (0x100-b0) as u8).
        let bytewise = |x: u32| {
            let b = x.to_be_bytes();
            u32::from_be_bytes([
                0xFF - b[0],
                0xFF - b[1],
                0xFF - b[2],
                0u8.wrapping_sub(b[3]),
            ])
        };
        for x in [
            1u32,
            0x7F,
            0x80,
            0xFF,
            0x1234_5678,
            0xFFFF_FFFF,
            0x8000_0001,
            0x5AA5_A55A,
        ] {
            assert_eq!(m32r_sum2_expect(x), M32rSum2Expect::Exact(x.wrapping_neg()));
            assert_eq!(bytewise(x), x.wrapping_neg(), "{x:#X}");
        }
        for x in [0u32, 0x100, 0x1234_5600, 0xFFFF_FF00, 0x5AA5_A600] {
            let M32rSum2Expect::Ambiguous { negation, upstream } = m32r_sum2_expect(x) else {
                panic!("{x:#X} must be ambiguous");
            };
            assert_eq!(negation, x.wrapping_neg());
            assert_eq!(upstream, bytewise(x));
            assert_eq!(negation.wrapping_sub(upstream), 0x100, "{x:#X}");
        }
    }

    #[test]
    fn m32r_edit_coverage_and_boundaries() {
        let algo = SubaruTcuM32r;
        let stock = m32r_stock();
        assert!(all_ok(&algo, &stock));
        let flip = |off: usize| {
            let mut e = stock.clone();
            e[off] ^= 0x01;
            let b = algo.verify(&e).unwrap();
            (
                block(&b, "m32r-sum1").ok,
                block(&b, "m32r-sum2").ok,
                block(&b, "m32r-sum2-copy").ok,
            )
        };
        // Sum 1 and sum 2 both cover 0x8020.. (first word = the balance, last word).
        for off in [0x8020, 0x8023, 0x9000, IMAGE_LEN - 4, IMAGE_LEN - 1] {
            assert_eq!(flip(off), (false, false, false), "{off:#X}");
        }
        // Sum 2 only: below 0x8000 and the 0x8008..0x8020 window.
        for off in [0x0, 0x3, 0x7FFC, 0x7FFF, 0x8008, 0x801F] {
            assert_eq!(flip(off), (true, false, false), "{off:#X}");
        }
        // The stored fields are excluded from both sums: only the edited field fails.
        assert_eq!(flip(0x8000), (true, false, true));
        assert_eq!(flip(0x8007), (true, true, false));
    }

    #[test]
    fn m32r_endianness_pinned() {
        // A +1 in the MSB of a sum-1 word moves the required balance by -2^24,
        // in the LSB by -1.
        let algo = SubaruTcuM32r;
        let stock = m32r_stock();
        let bal = be32(&stock, M32R_BAL1_OFF);
        let mut msb = stock.clone();
        msb[0x9000] = msb[0x9000].wrapping_add(1);
        let mut lsb = stock.clone();
        lsb[0x9003] = lsb[0x9003].wrapping_add(1);
        let req = |img: &[u8]| block(&algo.verify(img).unwrap(), "m32r-sum1").computed as u32;
        assert_eq!(req(&msb), bal.wrapping_sub(0x0100_0000));
        assert_eq!(req(&lsb), bal.wrapping_sub(1));
    }

    #[test]
    fn m32r_edit_refused_standalone_fixed_with_reference() {
        let algo = SubaruTcuM32r;
        let stock = m32r_stock();
        let mut edited = stock.clone();
        edited[0x9001] = edited[0x9001].wrapping_add(0x10); // +0x0010_0000 to the word
        let before = edited.clone();
        assert!(is_unsupported(algo.correct(&mut edited)));
        assert_eq!(edited, before, "refusal writes nothing");

        // Before correction both sums are off (sum 2 also covers 0x9000).
        let b = algo.verify(&edited).unwrap();
        assert!(!block(&b, "m32r-sum1").ok && !block(&b, "m32r-sum2").ok);

        let fixes = algo.correct_with_reference(&mut edited, &stock).unwrap();
        // The balance absorbs the edit exactly. Sum 2 covers both the edited
        // word and the balance, whose changes cancel, so sum 2 returns to its
        // stock value and the sum-2 fields need no rewrite.
        let names: Vec<_> = fixes.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["m32r-sum1"]);
        assert_eq!(fixes[0].offset, 0x8020);
        assert_eq!(
            fixes[0].new as u32,
            (fixes[0].old as u32).wrapping_sub(0x0010_0000)
        );
        assert!(all_ok(&algo, &edited));
        assert!(algo
            .correct_with_reference(&mut edited.clone(), &stock)
            .unwrap()
            .is_empty());
        assert_eq!(edited[..0x8020], stock[..0x8020]);
    }

    #[test]
    fn m32r_sum2_only_edit_leaves_balance() {
        let algo = SubaruTcuM32r;
        let stock = m32r_stock();
        let mut edited = stock.clone();
        edited[0x10] ^= 0x40;
        let fixes = algo.correct_with_reference(&mut edited, &stock).unwrap();
        let names: Vec<_> = fixes.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["m32r-sum2", "m32r-sum2-copy"]);
        assert_eq!(be32(&edited, 0x8020), be32(&stock, 0x8020));
        assert!(all_ok(&algo, &edited));
    }

    /// Zero image whose sum 2 (after sealing sum 1) has a zero low byte:
    /// balance 0x5AA5A55A + word 0x000000A6 at offset 0 → sum 2 = 0x5AA5A600.
    fn m32r_low_byte_zero_base() -> Vec<u8> {
        let mut img = vec![0u8; IMAGE_LEN];
        put32(&mut img, 0, 0xA6);
        put32(&mut img, M32R_BAL1_OFF, M32R_SUM1_TARGET);
        assert_eq!(m32r_sum2(&img), 0x5AA5_A600);
        img
    }

    #[test]
    fn m32r_low_byte_zero_case_is_ambiguous_and_never_written() {
        let algo = SubaruTcuM32r;
        // Hand values: true negation 0xA55A5A00, FastECU's carry-less form 0xA55A5900.
        assert_eq!(
            m32r_sum2_expect(0x5AA5_A600),
            M32rSum2Expect::Ambiguous {
                negation: 0xA55A_5A00,
                upstream: 0xA55A_5900
            }
        );
        for cand in [0xA55A_5A00u32, 0xA55A_5900] {
            let mut img = m32r_low_byte_zero_base();
            put32(&mut img, 0x8000, cand);
            put32(&mut img, 0x8004, cand);
            let b = algo.verify(&img).unwrap();
            assert!(
                b.iter().all(|s| s.ok),
                "{cand:#X} is one accepted candidate"
            );
            assert_eq!(block(&b, "m32r-sum2").computed as u32, cand);
            assert!(algo.correct(&mut img.clone()).unwrap().is_empty());
            // Already consistent → reference correction writes nothing.
            assert!(algo
                .correct_with_reference(&mut img.clone(), &m32r_stock())
                .unwrap()
                .is_empty());
        }

        // Neither candidate stored: verify flags it, and even with a valid
        // reference nothing is written (the value can't be confirmed).
        let mut img = m32r_low_byte_zero_base();
        put32(&mut img, 0x8000, 0x1111_1111);
        put32(&mut img, 0x8004, 0x1111_1111);
        assert!(!all_ok(&algo, &img));
        let before = img.clone();
        assert!(is_unsupported(
            algo.correct_with_reference(&mut img, &m32r_stock())
        ));
        assert_eq!(img, before);

        // Candidates split across the two copies → also refused.
        let mut img = m32r_low_byte_zero_base();
        put32(&mut img, 0x8000, 0xA55A_5A00);
        put32(&mut img, 0x8004, 0xA55A_5900);
        assert!(!all_ok(&algo, &img));
        assert!(is_unsupported(
            algo.correct_with_reference(&mut img, &m32r_stock())
        ));
    }

    #[test]
    fn m32r_edit_landing_on_zero_low_byte_is_refused_atomically() {
        let algo = SubaruTcuM32r;
        // Ambiguous base with stale sum-2 fields plus a sum-1 edit: the balance
        // would absorb the edit (sum 2 unchanged, low byte still zero), but the
        // sum-2 fields can't be settled → refuse, and the balance must NOT have
        // been half-written.
        let mut img = m32r_low_byte_zero_base();
        put32(&mut img, 0x8000, 0x2222_2222);
        put32(&mut img, 0x8004, 0x2222_2222);
        img[0x9002] = 0x55;
        let before = img.clone();
        assert!(is_unsupported(
            algo.correct_with_reference(&mut img, &m32r_stock())
        ));
        assert_eq!(img, before, "all-or-nothing");
    }

    #[test]
    fn m32r_bad_reference_refused() {
        let algo = SubaruTcuM32r;
        let stock = m32r_stock();
        let mut target = stock.clone();
        target[0x9000] ^= 1;
        for corrupt in [0x8020usize, 0x8000, 0x8004, 0x10] {
            let mut bad = stock.clone();
            bad[corrupt] ^= 0x80;
            let before = target.clone();
            assert!(
                is_unsupported(algo.correct_with_reference(&mut target, &bad)),
                "{corrupt:#X}"
            );
            assert_eq!(target, before);
        }
        assert!(is_unsupported(
            algo.correct_with_reference(&mut target, &stock[..0x4_0000])
        ));
    }

    // ---- SH7055 ----------------------------------------------------------

    fn seal_sh7055(img: &mut [u8]) {
        let bal =
            be16(img, SH7055_BAL_OFF).wrapping_add(SH7055_TARGET.wrapping_sub(sh7055_sum(img)));
        img[SH7055_BAL_OFF..SH7055_BAL_OFF + 2].copy_from_slice(&bal.to_be_bytes());
    }

    fn sh7055_stock() -> Vec<u8> {
        let mut img = pattern();
        seal_sh7055(&mut img);
        img
    }

    #[test]
    fn sh7055_region_table_is_word_counted_and_contiguous() {
        let ends: Vec<(usize, usize)> = SH7055_REGIONS
            .iter()
            .map(|&(s, w)| (s, s + 2 * w))
            .collect();
        // Word-count reading: regions chain end-to-start except at four gaps.
        let expected = [
            (0x0_1000, 0x0_2000),
            (0x0_3000, 0x0_3074),
            (0x0_3080, 0x0_C880),
            (0x0_C880, 0x1_D06C),
            (0x1_D06C, 0x2_D858),
            (0x2_D858, 0x3_E048),
            (0x3_E048, 0x4_0000),
            (0x4_0000, 0x4_E838),
            (0x4_E838, 0x5_F024),
            (0x5_F024, 0x6_F814),
            (0x6_F814, 0x7_FAFC),
            (0x7_FB80, 0x8_0000),
        ];
        assert_eq!(ends, expected);
        let (s, e) = expected[11];
        assert!(s <= SH7055_BAL_OFF && SH7055_BAL_OFF + 2 <= e);
        assert!(SH7055_REGIONS.iter().all(|&(s, _)| s % 2 == 0));
    }

    #[test]
    fn sh7055_zero_image_vector() {
        let mut img = vec![0u8; IMAGE_LEN];
        seal_sh7055(&mut img);
        assert_eq!(be16(&img, 0x7_FFF4), 0x5AA5);
        let algo = SubaruTcuSh7055;
        assert!(all_ok(&algo, &img));
        assert!(algo.correct(&mut img.clone()).unwrap().is_empty());
        let rep = algo.verify_report(&img, Representation::Unknown).unwrap();
        assert_eq!(rep.len(), 1);
        assert_eq!(rep[0].recognition, Recognition::Consistent);
        assert_eq!(rep[0].representation, Representation::RawFullImage);
        assert!(!rep[0].is_supported());
    }

    #[test]
    fn sh7055_included_vs_gap_edits() {
        let algo = SubaruTcuSh7055;
        let stock = sh7055_stock();
        assert!(all_ok(&algo, &stock));
        let ok_after = |off: usize| {
            let mut e = stock.clone();
            e[off] ^= 0x01;
            all_ok(&algo, &e)
        };
        // Covered: first/last word of several regions, and two offsets that are
        // only covered under the word-count reading (0x1800, 0x303A).
        for off in [
            0x1000, 0x1001, 0x1800, 0x1FFE, 0x1FFF, 0x3000, 0x303A, 0x3073, 0x3080, 0xC87F, 0xC880,
            0x7_FAFB, 0x7_FB80, 0x7_FFF4, 0x7_FFFF,
        ] {
            assert!(!ok_after(off), "{off:#X} must be covered");
        }
        // Gaps between regions.
        for off in [
            0x0, 0xFFF, 0x2000, 0x2FFF, 0x3074, 0x307F, 0x7_FAFC, 0x7_FB7F,
        ] {
            assert!(ok_after(off), "{off:#X} must be outside every region");
        }
    }

    #[test]
    fn sh7055_endianness_pinned() {
        let algo = SubaruTcuSh7055;
        let stock = sh7055_stock();
        let bal = be16(&stock, SH7055_BAL_OFF);
        let req = |img: &[u8]| algo.verify(img).unwrap()[0].computed as u16;
        let mut hi = stock.clone();
        hi[0x4000] = hi[0x4000].wrapping_add(1);
        let mut lo = stock.clone();
        lo[0x4001] = lo[0x4001].wrapping_add(1);
        assert_eq!(req(&hi), bal.wrapping_sub(0x0100));
        assert_eq!(req(&lo), bal.wrapping_sub(1));
    }

    #[test]
    fn sh7055_edit_refused_standalone_fixed_with_reference() {
        let algo = SubaruTcuSh7055;
        let stock = sh7055_stock();
        let mut edited = stock.clone();
        edited[0x4_1235] = edited[0x4_1235].wrapping_add(3); // odd byte = word LSB
        let before = edited.clone();
        assert!(is_unsupported(algo.correct(&mut edited)));
        assert_eq!(edited, before);
        let fixes = algo.correct_with_reference(&mut edited, &stock).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].name, "sh7055-sum16");
        assert_eq!(fixes[0].offset, 0x7_FFF4);
        assert_eq!(fixes[0].new as u16, (fixes[0].old as u16).wrapping_sub(3));
        assert!(all_ok(&algo, &edited));
        assert!(algo
            .correct_with_reference(&mut edited.clone(), &stock)
            .unwrap()
            .is_empty());

        // A reference that doesn't validate can't confirm the layout.
        let mut bad = stock.clone();
        bad[0x7_FFF5] ^= 1;
        let mut t = before.clone();
        assert!(is_unsupported(algo.correct_with_reference(&mut t, &bad)));
        assert_eq!(t, before);
    }

    // ---- MH8104 ----------------------------------------------------------

    fn seal_mh8104(img: &mut [u8]) {
        let bal = be32(img, MH8104_BAL_OFF)
            .wrapping_add(MH8104_TARGET.wrapping_sub(mh8104_checksum(img)));
        put32(img, MH8104_BAL_OFF, bal);
    }

    fn mh8104_stock() -> Vec<u8> {
        let mut img = pattern();
        seal_mh8104(&mut img);
        img
    }

    #[test]
    fn mh8104_adjustments_equal_effective_target() {
        // -0xFFFF then five times -0xFFFF_FFFF  ==  -0xFFFA  (mod 2^32), so the
        // raw word sum must be 0x5AA4_5AAB + 0xFFFA = 0x5AA5_5AA5.
        let mut img = vec![0u8; IMAGE_LEN];
        assert_eq!(mh8104_checksum(&img), 0u32.wrapping_sub(0xFFFA));
        seal_mh8104(&mut img);
        // All-zero image: the balance alone is the raw sum.
        assert_eq!(be32(&img, 0x81FC), 0x5AA5_5AA5);
        assert_eq!(sum_be32(&img[0x8000..0x8_0000]), 0x5AA5_5AA5);
        let algo = SubaruTcuMh8104;
        assert!(all_ok(&algo, &img));
        let rep = algo.verify_report(&img, Representation::Unknown).unwrap();
        assert_eq!(rep[0].recognition, Recognition::Consistent);
        assert!(!rep[0].is_supported());
    }

    #[test]
    fn mh8104_coverage_boundaries() {
        let algo = SubaruTcuMh8104;
        let stock = mh8104_stock();
        assert!(all_ok(&algo, &stock));
        let ok_after = |off: usize| {
            let mut e = stock.clone();
            e[off] ^= 0x01;
            all_ok(&algo, &e)
        };
        for off in [0x8000, 0x8003, 0x81FC, 0x4_0000, 0x7_FFFC, 0x7_FFFF] {
            assert!(!ok_after(off), "{off:#X} must be covered");
        }
        for off in [0x0, 0x1000, 0x7FFF] {
            assert!(ok_after(off), "{off:#X} must be outside [0x8000, 0x80000)");
        }
    }

    #[test]
    fn mh8104_endianness_pinned() {
        let algo = SubaruTcuMh8104;
        let stock = mh8104_stock();
        let bal = be32(&stock, MH8104_BAL_OFF);
        let req = |img: &[u8]| algo.verify(img).unwrap()[0].computed as u32;
        let mut hi = stock.clone();
        hi[0x2_0000] = hi[0x2_0000].wrapping_add(1);
        let mut lo = stock.clone();
        lo[0x2_0003] = lo[0x2_0003].wrapping_add(1);
        assert_eq!(req(&hi), bal.wrapping_sub(0x0100_0000));
        assert_eq!(req(&lo), bal.wrapping_sub(1));
    }

    #[test]
    fn mh8104_edit_refused_standalone_fixed_with_reference() {
        let algo = SubaruTcuMh8104;
        let stock = mh8104_stock();
        let mut edited = stock.clone();
        edited[0x6_C460] = edited[0x6_C460].wrapping_add(1);
        let before = edited.clone();
        assert!(is_unsupported(algo.correct(&mut edited)));
        assert_eq!(edited, before);
        let fixes = algo.correct_with_reference(&mut edited, &stock).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(fixes[0].name, "mh8104-sum32");
        assert_eq!(fixes[0].offset, 0x81FC);
        assert!(all_ok(&algo, &edited));
        assert!(algo
            .correct_with_reference(&mut edited.clone(), &stock)
            .unwrap()
            .is_empty());

        let mut bad = stock.clone();
        bad[0x9000] ^= 1;
        let mut t = before.clone();
        assert!(is_unsupported(algo.correct_with_reference(&mut t, &bad)));
        assert_eq!(t, before);
    }

    #[test]
    fn references_are_not_interchangeable() {
        // A stock image sealed for one scheme does not validate as another, so
        // it cannot serve as the other's reference.
        let m = m32r_stock();
        let s = sh7055_stock();
        let x = mh8104_stock();
        assert!(!all_ok(&SubaruTcuSh7055, &m));
        assert!(!all_ok(&SubaruTcuMh8104, &s));
        assert!(!all_ok(&SubaruTcuM32r, &x));
        let mut t = s.clone();
        t[0x4000] ^= 1;
        assert!(is_unsupported(
            SubaruTcuSh7055.correct_with_reference(&mut t, &m)
        ));
    }
}
