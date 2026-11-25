//! Checksum engine: verify and correct the integrity fields a flashed image
//! must have for the ECU to accept and run it.
//!
//! Each ECU family is a [`ChecksumAlgo`]. A generic CRC32-tail example is
//! included; real ECU algorithms (EDC15, EDC16, EDC17, PCR2.1, ME7, the
//! Continental/Siemens Simos-family block CRC headers in [`simos`], and the
//! TCU/AWD set in [`tcu`]: DQ250, DQ381, ZF 8HP, Haldex Gen5, CCITT trailer)
//! live in their own modules and must be validated against known-good
//! reference files. [`get`] maps an id to its algorithm.
use opentune_core::{MemoryRegion, Result};

pub mod dsg;
pub mod edc15;
pub mod edc16;
pub mod edc17;
pub mod generic;
pub mod me7;
pub mod pcr21;
pub mod simos;
pub mod subaru_tcu;
pub mod tcu;

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

/// Which *representation* of a firmware image a checksum result applies to.
///
/// The same bytes mean different things depending on how they were obtained: a
/// full ECU read places every programming block at its flash base, an extracted
/// block is base-relative, and a container segment is carved from a transport
/// container. This is declared by the caller (it cannot always be inferred from
/// the bytes); an algorithm that can positively identify the representation
/// (e.g. DQ381, from its self-describing block headers) may report a more
/// specific value than the one declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Representation {
    /// A full ECU read: all programming blocks present in one image at their
    /// absolute flash bases.
    RawFullImage,
    /// A single extracted programming block (e.g. a VW_Flash/ODX block), which is
    /// addressed relative to its own base.
    ProgrammingBlock,
    /// A segment carved from a transport/update container (e.g. an ODX-F frame).
    ContainerSegment,
    /// Not declared by the caller and not determined.
    #[default]
    Unknown,
}

/// Whether a checksum lives *inside* the running image or guards a *transfer*.
///
/// Some families carry both: a DSG image has an internal CRC the ECU checks at
/// boot (**embedded**) and, separately, an external UDS/container checksum over
/// the programming payload in transit (**transfer**). Distinguishing them stops
/// an embedded-CRC result from being read as transfer integrity, or vice versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChecksumKind {
    /// Stored in the image and verified by the ECU at boot/run (internal CRC).
    #[default]
    Embedded,
    /// External integrity over a programming payload in transit (e.g. a UDS
    /// transfer or container CRC); not part of the running image.
    Transfer,
}

/// How strongly a block result is tied to a *known, confirmed* checksum family.
///
/// This is the guard against over-claiming: arithmetic agreement alone does not
/// prove an image belongs to a family (a short field can match by chance, and a
/// generic trailer matches any bytes once corrected).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Recognition {
    /// A known family was structurally identified and validated — e.g. a
    /// self-describing header found at its documented base, or a real stored
    /// value proven against the algorithm. A match here is genuinely supported.
    Recognised,
    /// The math checked out, but the layout/identity was **not** confirmed, so
    /// the match may be coincidental. Never reported as "supported".
    #[default]
    Consistent,
}

/// A representation-aware report for one checksum-protected block: the plain
/// [`BlockStatus`] plus how it must be interpreted.
///
/// Additive wrapper — code that only needs stored/computed/OK keeps using
/// [`BlockStatus`] via [`ChecksumAlgo::verify`]; callers that must know *which*
/// representation and *how confident* the recognition is use
/// [`ChecksumAlgo::verify_report`] / [`report_detailed`].
#[derive(Debug, Clone)]
pub struct BlockReport {
    /// The underlying stored-vs-computed status.
    pub status: BlockStatus,
    /// The representation this result applies to.
    pub representation: Representation,
    /// Embedded (internal) vs transfer (external) checksum.
    pub checksum_kind: ChecksumKind,
    /// Whether the family was recognised or the match is merely consistent.
    pub recognition: Recognition,
}

impl BlockReport {
    /// Construct a report from its parts.
    pub fn new(
        status: BlockStatus,
        representation: Representation,
        checksum_kind: ChecksumKind,
        recognition: Recognition,
    ) -> Self {
        Self {
            status,
            representation,
            checksum_kind,
            recognition,
        }
    }

    /// `true` only when the block both verifies **and** belongs to a recognised
    /// family — i.e. it is safe to report as "supported". A merely
    /// mathematically-consistent (possibly coincidental) match returns `false`
    /// even when [`status.ok`](BlockStatus::ok) is set, so an unverified or
    /// chance match is never surfaced as supported.
    pub fn is_supported(&self) -> bool {
        self.status.ok && self.recognition == Recognition::Recognised
    }
}

/// A checksum scheme for one ECU family.
pub trait ChecksumAlgo {
    fn id(&self) -> &str;
    /// Report each protected block and whether its stored checksum matches.
    fn verify(&self, image: &[u8]) -> Result<Vec<BlockStatus>>;

    /// Representation-aware [`verify`](Self::verify): each block as a
    /// [`BlockReport`] carrying the declared [`Representation`], the
    /// [`ChecksumKind`] (embedded vs transfer) and a [`Recognition`] flag.
    ///
    /// The default wraps [`verify`](Self::verify) conservatively: it echoes the
    /// caller-declared `representation`, marks every block [`ChecksumKind::Embedded`]
    /// and — crucially — [`Recognition::Consistent`], **not** `Recognised`. The
    /// default never claims family recognition, so an algorithm whose `verify`
    /// can match coincidentally (e.g. a generic trailer) is never reported as
    /// "supported". Algorithms that structurally identify their family (DQ381's
    /// self-describing headers, DQ250's proven stored value) override this to
    /// mark [`Recognition::Recognised`] and may report a more specific
    /// representation than the one declared.
    fn verify_report(
        &self,
        image: &[u8],
        representation: Representation,
    ) -> Result<Vec<BlockReport>> {
        Ok(self
            .verify(image)?
            .into_iter()
            .map(|status| {
                BlockReport::new(
                    status,
                    representation,
                    ChecksumKind::Embedded,
                    Recognition::Consistent,
                )
            })
            .collect())
    }
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

/// Representation-aware counterpart to [`report`]: every protected block as a
/// [`BlockReport`] that says which [`Representation`] it applies to, whether the
/// checksum is [`ChecksumKind::Embedded`] or [`ChecksumKind::Transfer`], and
/// whether the family was [`Recognition::Recognised`] or the match is merely
/// [`Recognition::Consistent`]. Dispatches through
/// [`ChecksumAlgo::verify_report`]; use [`BlockReport::is_supported`] to render a
/// coincidental/unverified match without ever labelling it "supported".
pub fn report_detailed(
    algo: &dyn ChecksumAlgo,
    image: &[u8],
    representation: Representation,
) -> Result<Vec<BlockReport>> {
    algo.verify_report(image, representation)
}

/// Look up a checksum algorithm by id (as named in an ECU definition).
pub fn get(id: &str) -> Option<Box<dyn ChecksumAlgo>> {
    match id {
        "crc32-tail" => Some(Box::new(generic::Crc32Tail)),
        #[cfg(any(test, feature = "test-algos"))]
        "test-crc32-tail" => Some(Box::new(generic::TestCrc32Tail)),
        "edc16" => Some(Box::new(edc16::Edc16)),
        "edc17" => Some(Box::new(edc17::Edc17)),
        "pcr21" => Some(Box::new(pcr21::Pcr21)),
        "edc15c6" | "edc15" => Some(Box::new(edc15::Edc15C6)),
        "me7" | "me7x" => Some(Box::new(me7::Me7)),
        "dq250" | "tcu-jamcrc" => Some(Box::new(tcu::Dq250Jamcrc)),
        "dq250-02e" | "dq250-pq" => Some(Box::new(dsg::Dq250PqCrc16)),
        "dq381" | "tcu-crc32" => Some(Box::new(tcu::Dq381Crc32)),
        "tcu-ccitt" => Some(Box::new(tcu::CcittTrailer)),
        "zf8hp" | "zf-8hp" => Some(Box::new(tcu::Zf8hpCal)),
        "haldex" | "haldex-gen5" => Some(Box::new(tcu::HaldexGen5)),
        "simos-crc" | "conti-crc" => Some(Box::new(simos::SimosCrc)),
        "subaru-tcu-m32r" => Some(Box::new(subaru_tcu::SubaruTcuM32r)),
        "subaru-tcu-sh7055" => Some(Box::new(subaru_tcu::SubaruTcuSh7055)),
        "subaru-tcu-mh8104" => Some(Box::new(subaru_tcu::SubaruTcuMh8104)),
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
            notes: "Alias: edc15. VAG TDI v4.1v2 (512 KiB) and Mercedes EDC15C6 \
                    (1 MiB, 0xC058-marked block chain) both validated against real \
                    stock+tuned dumps and self-correcting; anything unrecognised is \
                    refused (never guessed).",
        },
        AlgoInfo {
            id: "me7",
            name: "Bosch Motronic ME7.x (petrol)",
            family: "me7",
            needs_reference: false,
            notes: "Alias: me7x. Main ROMSYS 16-bit word-sum (two ranges, value + \
                    complement) and the self-describing multipoint block, both \
                    auto-located from the image (base 0x800000). Validated on real \
                    VAG ME7.5/ME7.1 dumps. CAL-CRC32 variants: primitive exposed, \
                    block layout firmware-specific and never guessed.",
        },
        AlgoInfo {
            id: "dq250",
            name: "VAG DQ250-MQB DSG (JAMCRC)",
            family: "tcu",
            needs_reference: false,
            notes: "Alias: tcu-jamcrc. JAMCRC (= !crc32) per-block trailer over the \
                    DQ250-MQB CAL/ASW layout; corrects only when a block's JAMCRC \
                    validates. Algorithm vectors + synthetic validated; real 0D9 \
                    bench-dump validation pending. Does not apply to PQ 02E \
                    software (see dq250-02e).",
        },
        AlgoInfo {
            id: "dq250-02e",
            name: "VAG DQ250 PQ 02E DSG (CRC-16 sector table)",
            family: "tcu",
            needs_reference: false,
            notes: "Alias: dq250-pq. 0x68000-byte PowerPC-generation 02E read: \
                    CRC-16/MCRF4XX (0x8408 reflected, init 0xFFFF) of twelve 32 KiB \
                    code sectors + the CAL sector 0x60000..0x67FDF, stored as 13 \
                    big-endian words at 0x67FE2. Real-image-tested on six 02E reads \
                    (78/78 words); also handles the extracted 0x8000-byte CAL sector. \
                    TriCore-generation 02E reads are refused.",
        },
        AlgoInfo {
            id: "dq381",
            name: "VAG DQ381 DSG (CRC32-VW)",
            family: "tcu",
            needs_reference: false,
            notes: "Alias: tcu-crc32. Self-describing BOOT/ASW/CAL CRC-32 (big-endian \
                    zlib; pointers @0x38/0x3C inclusive, CRC @0x44; bases BOOT \
                    0x10200, ASW 0x30200, CAL 0x140200). All three validated on a \
                    real DQ381 full dump; reported per block.",
        },
        AlgoInfo {
            id: "tcu-ccitt",
            name: "VAG/ZF TCU (CRC-CCITT)",
            family: "tcu",
            needs_reference: false,
            notes: "Generic big-endian CRC-CCITT (0x1021, CCITT-FALSE) trailer over \
                    the block body; layout-agnostic. CCITT vectors validated; a \
                    specific CCITT-protected dump for end-to-end validation is pending.",
        },
        AlgoInfo {
            id: "zf8hp",
            name: "ZF 8HP TCU (AL551 CAL CRC-32)",
            family: "tcu",
            needs_reference: true,
            notes: "Alias: zf-8hp. AL551-generation (method 0x22, e.g. 4G0/4H1 — \
                    8HP55/8HP90) CAL block: zlib CRC-32 over CAL_DATA \
                    0x190000..=0x1FFD5F, stored big-endian at 0x180244. AES variants \
                    (4M0/8W0) and Gen3 TriCore not claimed. CRC-32 vectors + synthetic \
                    only (no real dump): standalone correct refuses an edited image \
                    (fixed-offset layout unconfirmed) — correct_with_reference with a \
                    validating stock dump is required; never guessed.",
        },
        AlgoInfo {
            id: "haldex",
            name: "BorgWarner Haldex Gen5 AWD",
            family: "tcu",
            needs_reference: false,
            notes: "Alias: haldex-gen5. Per-block 16-bit little-endian word-sum \
                    (0xFFFF - sum) over the block minus its 10-byte checksum header, \
                    stored LE at cs_loc+0x8 (CAL 0x10, ASW 0x200, VERSION 0x0; DRIVER \
                    unchecked). Block identified by its data-independent embedded \
                    length field, so it self-corrects like dq381 across SW versions. \
                    Synthetic + algorithm-vector validated only; real-dump validation \
                    pending.",
        },
        AlgoInfo {
            id: "simos-crc",
            name: "Continental/Siemens Simos-family block CRC headers",
            family: "simos",
            needs_reference: false,
            notes: "Alias: conti-crc. The VW_Flash-documented secured-block header \
                    ({init, crc, count, [start,end]...}, Bosch forward CRC-32 over the \
                    concatenated areas), found at 256-byte-aligned offsets of a full \
                    power-of-two read. Real-image-tested on Continental Sim2K-24x \
                    (Hyundai/Kia), K205x diesel, Ford EMS and EMS3120 stock+tuned \
                    reads; VAG Simos 8/10/12/18 not claimed (no read available). \
                    Standalone correct needs one header still verifying (single-header \
                    Ford/EMS3120 reads: pass the stock read as reference) and refuses \
                    an image whose startup-boot header fails (BMW MSD8x).",
        },
        AlgoInfo {
            id: "subaru-tcu-m32r",
            name: "Subaru Hitachi M32R TCU (512 KiB)",
            family: "subaru-tcu",
            needs_reference: true,
            notes: "Big-endian 32-bit word sums over a 0x80000-byte read: sum of \
                    0x8020.. must equal 0x5AA5A55A (balance @0x8020), and the two's \
                    complement of the sum of every word except 0x8000..0x8007 is \
                    stored @0x8000 (copy @0x8004). Synthetic-tested only (FastECU \
                    port): standalone correct refuses an edited image; pass a \
                    validating stock dump. A sum-2 low byte of zero is ambiguous \
                    upstream and never written.",
        },
        AlgoInfo {
            id: "subaru-tcu-sh7055",
            name: "Subaru Denso SH7055 TCU (512 KiB)",
            family: "subaru-tcu",
            needs_reference: true,
            notes: "Big-endian 16-bit word sum over 12 fixed regions \
                    (0x1000..0x80000 minus four gaps) must equal 0x5AA5; balance \
                    @0x7FFF4. Synthetic-tested only (FastECU port): standalone \
                    correct refuses an edited image; pass a validating stock dump.",
        },
        AlgoInfo {
            id: "subaru-tcu-mh8104",
            name: "Subaru CVT TCU, Mitsubishi MH8104 (512 KiB)",
            family: "subaru-tcu",
            needs_reference: true,
            notes: "Big-endian 32-bit word sum over 0x8000..0x80000 minus 0xFFFA \
                    (FastECU's fixed adjustments) must equal 0x5AA45AAB; balance \
                    @0x81FC. Synthetic-tested only (FastECU port): standalone \
                    correct refuses an edited image; pass a validating stock dump.",
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
        let algo = generic::TestCrc32Tail;
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
        let resolvable = [
            "crc32-tail",
            "edc16",
            "edc17",
            "pcr21",
            "edc15c6",
            "edc15",
            "me7",
            "me7x",
            "dq250",
            "tcu-jamcrc",
            "dq250-02e",
            "dq250-pq",
            "dq381",
            "tcu-crc32",
            "tcu-ccitt",
            "zf8hp",
            "zf-8hp",
            "haldex",
            "haldex-gen5",
            "simos-crc",
            "conti-crc",
            "subaru-tcu-m32r",
            "subaru-tcu-sh7055",
            "subaru-tcu-mh8104",
        ];
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
    fn default_verify_report_is_consistent_not_recognised() {
        // The default verify_report wraps verify() but must NOT claim family
        // recognition: even a matching block is only "mathematically consistent",
        // so a possibly-coincidental match is never surfaced as supported.
        let algo = TwoBlock;
        // block A valid (sum 1+2+3+4 = 10), block B bad.
        let img = [1u8, 2, 3, 4, 10, 5, 6, 7, 8, 0];
        let reports = report_detailed(&algo, &img, Representation::ProgrammingBlock).unwrap();
        assert_eq!(reports.len(), 2);
        // Representation is echoed from the caller's declaration.
        assert!(reports
            .iter()
            .all(|r| r.representation == Representation::ProgrammingBlock));
        // Default kind is embedded.
        assert!(reports
            .iter()
            .all(|r| r.checksum_kind == ChecksumKind::Embedded));
        // Block A verifies, but is only "consistent" — never "supported".
        assert!(reports[0].status.ok);
        assert_eq!(reports[0].recognition, Recognition::Consistent);
        assert!(
            !reports[0].is_supported(),
            "a merely-consistent match must not report as supported even when ok"
        );
        // Block B is both wrong and unrecognised.
        assert!(!reports[1].status.ok);
        assert!(!reports[1].is_supported());
    }

    #[test]
    fn recognised_vs_consistent_supported_rule() {
        // is_supported() requires BOTH ok and Recognised.
        let base = BlockStatus {
            name: "b".into(),
            region: MemoryRegion::new(0, 4),
            stored: 1,
            computed: 1,
            ok: true,
        };
        let recognised = BlockReport::new(
            base.clone(),
            Representation::RawFullImage,
            ChecksumKind::Embedded,
            Recognition::Recognised,
        );
        assert!(recognised.is_supported(), "ok + recognised = supported");

        let mut bad = base.clone();
        bad.ok = false;
        let recognised_but_bad = BlockReport::new(
            bad,
            Representation::RawFullImage,
            ChecksumKind::Embedded,
            Recognition::Recognised,
        );
        assert!(
            !recognised_but_bad.is_supported(),
            "recognised family with a bad checksum is not supported"
        );

        let consistent = BlockReport::new(
            base,
            Representation::ContainerSegment,
            ChecksumKind::Transfer,
            Recognition::Consistent,
        );
        assert!(
            !consistent.is_supported(),
            "a coincidental (consistent-only) match is never supported"
        );
        // Enum defaults are the conservative choices.
        assert_eq!(Recognition::default(), Recognition::Consistent);
        assert_eq!(Representation::default(), Representation::Unknown);
        assert_eq!(ChecksumKind::default(), ChecksumKind::Embedded);
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
