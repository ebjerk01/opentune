//! Checksum engine: verify and correct the integrity fields a flashed image
//! must have for the ECU to accept and run it.
//!
//! Each ECU family is a [`ChecksumAlgo`]. A generic CRC32-tail example is
//! included; real ECU algorithms (EDC16, EDC17, PCR2.1) live in their own
//! modules and must be validated against known-good reference files.
use opentune_core::{MemoryRegion, Result};

pub mod edc15;
pub mod edc16;
pub mod edc17;
pub mod generic;
pub mod pcr21;

/// Status of one checksum-protected block.
#[derive(Debug, Clone)]
pub struct BlockStatus {
    pub name: String,
    pub region: MemoryRegion,
    pub stored: u64,
    pub computed: u64,
    pub ok: bool,
}

/// A correction that was (or would be) applied.
#[derive(Debug, Clone)]
pub struct Fix {
    pub name: String,
    pub offset: u32,
    pub old: u64,
    pub new: u64,
}

/// A checksum scheme for one ECU family.
pub trait ChecksumAlgo {
    fn id(&self) -> &str;
    /// Report each protected block and whether its stored checksum matches.
    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>>;
    /// Recompute and write correct checksums in place; return what changed.
    fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>>;

    /// Correct using a known-good **reference image** (e.g. the owner's own
    /// untouched stock dump) to bootstrap per-block target values that are not
    /// baked into this library. The default ignores the reference and behaves
    /// exactly like [`correct`](Self::correct) — correct for algorithms whose
    /// targets are self-contained in the image (EDC16, PCR2.1, EDC15). EDC17
    /// overrides it: the VAG per-block constant is firmware-specific, so it is
    /// recovered from the reference rather than guessed.
    fn correct_with_reference(&self, image: &mut [u8], _reference: &[u8]) -> Result<Vec<Fix>> {
        self.correct(image)
    }

    /// Correct **only the blocks whose stored checksum does not match the
    /// computed one**, leaving every already-correct block byte-for-byte
    /// untouched, and return what changed.
    ///
    /// This is the per-block hook behind [`correct_bad_only`]. The default
    /// delegates to [`correct`](Self::correct), which is already a per-block
    /// pass: every bundled algorithm recomputes each block but writes a block
    /// (and emits a [`Fix`]) only when its stored word differs, so "fix only the
    /// bad blocks" is the existing behaviour. An algorithm override is only
    /// needed if its `correct` ever rewrites blocks that were already valid; the
    /// anti-brick stance is unchanged — a block whose target cannot be computed
    /// is never guessed, it is left as-is (or the image refused, exactly as
    /// `correct` decides).
    fn correct_bad_blocks(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
        self.correct(image)
    }
}

/// Report every checksum-protected block and whether its stored value matches
/// the computed one, without modifying the image. A thin, intention-revealing
/// alias over [`ChecksumAlgo::verify`] so a CLI/app can render a per-block table
/// (region, stored, computed, OK/BAD).
pub fn report(algo: &dyn ChecksumAlgo, image: &[u8]) -> Result<Vec<BlockStatus>> {
    algo.verify(image)
}

/// Correct only the blocks that are currently wrong, leaving valid blocks
/// untouched, and return the list of [`Fix`]es applied. Dispatches through
/// [`ChecksumAlgo::correct_bad_blocks`]. Keeps the anti-brick stance: a block
/// whose correct value cannot be computed is never guessed.
pub fn correct_bad_only(algo: &dyn ChecksumAlgo, image: &mut [u8]) -> Result<Vec<Fix>> {
    algo.correct_bad_blocks(image)
}

/// Look up a checksum algorithm by id (as named in an ECU definition).
pub fn get(id: &str) -> Option<Box<dyn ChecksumAlgo>> {
    match id {
        "crc32-tail" => Some(Box::new(generic::Crc32Tail)),
        "edc16" => Some(Box::new(edc16::Edc16)),
        "edc17" => Some(Box::new(edc17::Edc17)),
        "pcr21" => Some(Box::new(pcr21::Pcr21)),
        "edc15c6" | "edc15" => Some(Box::new(edc15::Edc15C6)),
        _ => None,
    }
}

/// A human-readable descriptor for one checksum algorithm the library knows.
///
/// This is the data-driven layer behind a "plugins as data" catalogue: a
/// CLI/app can enumerate [`algorithms`] to build its algorithm list/dropdown and
/// describe each entry instead of hard-coding them. It is a *description* only —
/// the runnable algorithm is still obtained with [`get`] (and [`algorithms`] is
/// kept consistent with `get`: every listed [`id`](AlgoInfo::id) resolves, and
/// every id `get` resolves maps to a listed entry). A full dynamic-plugin loader
/// is intentionally out of scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlgoInfo {
    /// Canonical id, as passed to [`get`] and named in an ECU definition.
    pub id: &'static str,
    /// Short human name.
    pub name: &'static str,
    /// ECU/checksum family grouping (e.g. `"edc17"`, `"generic"`).
    pub family: &'static str,
    /// Whether correcting generally needs a known-good **reference image** (the
    /// owner's own stock dump) to bootstrap a firmware-specific target value.
    /// Only EDC17's VAG per-block constant needs this when no validated library
    /// constant is bundled; everything else self-corrects from the image alone.
    pub needs_reference: bool,
    /// One-line human description, including any id aliases and known caveats.
    pub notes: &'static str,
}

/// Enumerate the checksum algorithms the library knows, as data.
///
/// Kept consistent with [`get`]: each entry's [`id`](AlgoInfo::id) resolves via
/// `get`, and every id `get` accepts (including the `edc15` alias of `edc15c6`)
/// maps to one of these entries. See the per-module docs for the full
/// reverse-engineering provenance and validation status.
pub fn algorithms() -> Vec<AlgoInfo> {
    vec![
        AlgoInfo {
            id: "crc32-tail",
            name: "Generic CRC32 tail",
            family: "generic",
            needs_reference: false,
            notes: "ECU-agnostic example: little-endian CRC-32 of the image over \
                    its last 4 bytes. Not a real ECU scheme; for tests/demos.",
        },
        AlgoInfo {
            id: "edc16",
            name: "Bosch EDC16 (MPC56x)",
            family: "edc16",
            needs_reference: false,
            notes: "On-image FADECAFE/CAFEAFFE directory of 32-bit big-endian sum \
                    blocks against a fixed Bosch constant; falls back to a \
                    calibration block table when no directory is present.",
        },
        AlgoInfo {
            id: "edc17",
            name: "Bosch EDC17 / Simos (CRC32 blocks)",
            family: "edc17",
            needs_reference: true,
            notes: "FADECAFE/CAFEAFFE directory of CRC32 blocks. The VAG per-block \
                    target constant is firmware-specific: a validated library \
                    constant corrects standalone, otherwise pass the owner's stock \
                    dump (correct_with_reference) to recover it; unknown blocks are \
                    surfaced but never guessed.",
        },
        AlgoInfo {
            id: "pcr21",
            name: "Siemens/Continental Simos PCR2.1",
            family: "pcr21",
            needs_reference: false,
            notes: "Self-describing calibration CRC directory (Bosch forward CRC-32, \
                    init 0). Validated; self-corrects from the image alone.",
        },
        AlgoInfo {
            id: "edc15c6",
            name: "Bosch EDC15 (VAG TDI v4.1v2 / EDC15C6)",
            family: "edc15",
            needs_reference: false,
            notes: "Alias: edc15. VAG TDI v4.1v2 (512 KiB) fully solved and \
                    self-correcting; Mercedes EDC15C6 (1 MiB) is located-only and \
                    refused (algorithm not yet reverse-engineered — never guessed).",
        },
    ]
}

/// CRC-32 (IEEE 802.3, reflected) — shared helper for algorithms and tests.
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

/// Forward (MSB-first) CRC-32 with polynomial `0x04C11DB7`, given `init` and no
/// final XOR. `refin = refout = false`.
///
/// This is the "main ROM" CRC family used across Bosch control units
/// (Motronic ME7 CRC blocks, Simos/MED/EDC17 main block). See
/// `docs/BOSCH-CHECKSUM.md` and the VW_Flash reference. Exposed so EDC16/EDC17
/// block tables can reuse a single, tested implementation.
fn crc32_forward(data: &[u8], init: u32) -> u32 {
    let mut crc = init;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Bosch/VAG main CRC-32: forward poly `0x04C11DB7`, **init 0**, no final XOR.
///
/// Documented in VW_Flash as the algorithm the Simos/MED families use over the
/// concatenated checksum areas. Provided for EDC16 variants whose main block
/// uses it; the EDC16 *calibration* checksum reverse-engineered here is a
/// different (still unidentified) 32-bit avalanche field — see `edc16`.
pub fn crc32_bosch(data: &[u8]) -> u32 {
    crc32_forward(data, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal two-block algorithm for exercising the generic report /
    /// correct_bad_only layer: a 10-byte image holds block A over bytes `0..4`
    /// with its 1-byte sum at offset 4, and block B over bytes `5..9` with its
    /// 1-byte sum at offset 9. `correct` rewrites a block's sum only when wrong.
    struct TwoBlock;

    impl TwoBlock {
        const BLOCKS: [(usize, usize, usize); 2] = [(0, 4, 4), (5, 9, 9)]; // (start, end, store)

        fn sum(image: &[u8], start: usize, end: usize) -> u8 {
            image[start..end]
                .iter()
                .fold(0u8, |a, &b| a.wrapping_add(b))
        }
    }

    impl ChecksumAlgo for TwoBlock {
        fn id(&self) -> &str {
            "two-block-test"
        }

        fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>> {
            let mut out = Vec::new();
            for (i, &(s, e, store)) in Self::BLOCKS.iter().enumerate() {
                let computed = Self::sum(image, s, e) as u64;
                let stored = image[store] as u64;
                out.push(BlockStatus {
                    name: format!("block-{i}"),
                    region: MemoryRegion::new(s as u32, (e - s) as u32),
                    stored,
                    computed,
                    ok: stored == computed,
                });
            }
            Ok(out)
        }

        fn correct(&self, image: &mut [u8]) -> Result<Vec<Fix>> {
            let mut fixes = Vec::new();
            for (i, &(s, e, store)) in Self::BLOCKS.iter().enumerate() {
                let computed = Self::sum(image, s, e);
                let old = image[store];
                if old != computed {
                    image[store] = computed;
                    fixes.push(Fix {
                        name: format!("block-{i}"),
                        offset: store as u32,
                        old: old as u64,
                        new: computed as u64,
                    });
                }
            }
            Ok(fixes)
        }
    }

    #[test]
    fn report_mirrors_verify() {
        let algo = TwoBlock;
        // block A valid (sum 1+2+3+4 = 10), block B deliberately wrong.
        let img = [1u8, 2, 3, 4, 10, 5, 6, 7, 8, 0];
        let via_report = report(&algo, &img).unwrap();
        let via_verify = algo.verify(&img).unwrap();
        assert_eq!(via_report.len(), 2);
        assert!(via_report[0].ok, "block A must verify");
        assert!(!via_report[1].ok, "block B must read as bad");
        // report() is the intention-revealing alias of verify(): same result.
        for (r, v) in via_report.iter().zip(via_verify.iter()) {
            assert_eq!((r.stored, r.computed, r.ok), (v.stored, v.computed, v.ok));
        }
    }

    #[test]
    fn correct_bad_only_rewrites_only_the_bad_block() {
        let algo = TwoBlock;
        // block A valid (sum = 10 stored at [4]); block B bad (sum 5+6+7+8 = 26,
        // but stored 0 at [9]).
        let mut img = [1u8, 2, 3, 4, 10, 5, 6, 7, 8, 0];
        let before = img;

        let fixes = correct_bad_only(&algo, &mut img).unwrap();

        assert_eq!(fixes.len(), 1, "exactly one block was wrong");
        assert_eq!(fixes[0].name, "block-1");
        assert_eq!(fixes[0].offset, 9);
        assert_eq!((fixes[0].old, fixes[0].new), (0, 26));

        // Block A and all its bytes are byte-for-byte untouched.
        assert_eq!(img[0..5], before[0..5], "already-correct block A untouched");
        // Only the one bad block's stored byte changed.
        assert_eq!(img[5..9], before[5..9], "block B data untouched");
        assert_eq!(img[9], 26, "block B stored checksum corrected");

        // Idempotent: a second pass finds nothing to fix, writes nothing.
        let again = correct_bad_only(&algo, &mut img).unwrap();
        assert!(again.is_empty());
        assert!(report(&algo, &img).unwrap().iter().all(|b| b.ok));
    }

    #[test]
    fn correct_bad_only_matches_generic_crc32_tail() {
        // The default hook (= correct) must behave identically via the free fn.
        let algo = generic::Crc32Tail;
        let mut img = vec![0u8; 16]; // tail CRC wrong (all zero)
        let fixes = correct_bad_only(&algo, &mut img).unwrap();
        assert_eq!(fixes.len(), 1);
        assert!(report(&algo, &img).unwrap().iter().all(|b| b.ok));
        // Correcting an already-valid image writes nothing.
        assert!(correct_bad_only(&algo, &mut img).unwrap().is_empty());
    }

    #[test]
    fn algorithms_cover_every_id_get_resolves() {
        // Every id get() accepts (including the edc15 alias) maps to a listed
        // AlgoInfo, matched by the boxed algorithm's canonical id().
        let resolvable = ["crc32-tail", "edc16", "edc17", "pcr21", "edc15c6", "edc15"];
        let infos = algorithms();
        for id in resolvable {
            let algo = get(id).unwrap_or_else(|| panic!("get({id}) must resolve"));
            let canon = algo.id();
            assert!(
                infos.iter().any(|a| a.id == canon),
                "algorithms() must list the canonical id {canon} that get({id}) resolves to"
            );
        }
        // Conversely, every listed AlgoInfo.id resolves via get().
        for info in &infos {
            assert!(get(info.id).is_some(), "get({}) must resolve", info.id);
        }
        // get() rejects unknown ids.
        assert!(get("nope").is_none());
    }

    #[test]
    fn algorithm_descriptors_are_consistent() {
        let infos = algorithms();
        // Ids are unique.
        for i in 0..infos.len() {
            for j in (i + 1)..infos.len() {
                assert_ne!(infos[i].id, infos[j].id, "duplicate AlgoInfo id");
            }
        }
        let find = |id: &str| infos.iter().find(|a| a.id == id).copied().unwrap();
        // Only EDC17 is flagged as reference-needing (the one algo that overrides
        // correct_with_reference for its firmware-specific constant).
        assert!(find("edc17").needs_reference);
        for id in ["crc32-tail", "edc16", "pcr21", "edc15c6"] {
            assert!(
                !find(id).needs_reference,
                "{id} should not need a reference"
            );
        }
    }

    #[test]
    fn crc32_known_vector() {
        // CRC32 of "123456789" == 0xCBF43926
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn crc32_forward_matches_mpeg2_vector() {
        // CRC-32/MPEG-2 is forward poly 0x04C11DB7, init 0xFFFFFFFF, no xorout;
        // its published check value over "123456789" is 0x0376E6E7. This pins
        // the forward engine against a standard vector.
        assert_eq!(crc32_forward(b"123456789", 0xFFFF_FFFF), 0x0376_E6E7);
    }

    #[test]
    fn crc32_bosch_init0_vector() {
        // Same engine with init 0 (the Bosch/VAG main-CRC parameters). Value is
        // locked as a regression against the validated implementation.
        assert_eq!(crc32_bosch(b"123456789"), 0x89A1_897F);
    }
}
