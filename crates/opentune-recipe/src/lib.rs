//! # opentune-recipe
//!
//! Portable **tune recipes**: the difference between a stock and a tuned ECU
//! image captured as *anchored patches*, plus the identity of the image it was
//! made from. A recipe can be re-applied to another copy of the same software
//! (e.g. a re-read of the same ECU) and is **refused** — never guessed — when
//! the target is not provably the image the recipe was made for.
//!
//! Each patch stores its offset, the original bytes, the new bytes and a few
//! bytes of unchanged *context* on each side. [`apply`]:
//!
//! 1. checks the target's identity (size, structural family, part/software
//!    numbers, and — in [`ApplyMode::Strict`] — a hash over every byte the
//!    recipe does not touch);
//! 2. resolves every patch against the **unmodified** target: the original
//!    bytes and context must match exactly at the recorded offset, or, only in
//!    the explicit [`ApplyMode::Relocate`] mode, the context+original sequence
//!    must occur **exactly once** elsewhere (zero or several matches refuse);
//! 3. writes the new bytes, runs the checksum correction for the family, and
//!    attests the result with `opentune-attest` — an unsatisfied attestation
//!    refuses the whole apply.
//!
//! Nothing is partially applied: any refused patch refuses the recipe, and the
//! per-patch [`ApplyReport`] says why. The file format (JSON, versioned) is
//! documented in `docs/RECIPES.md`.
//!
//! The identity ladder (strict anchors first, a deliberately explicit looser
//! rung that refuses ambiguity) follows the design described by
//! `openremap-core` (MIT); this crate is an independent implementation — see
//! `NOTICE`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use opentune_core::ecudef::MapDef;
use opentune_flash::hash::sha256_hex;
use opentune_flash::ChecksumAttestation;
use serde::{Deserialize, Serialize};

/// The `format` tag every recipe file carries.
pub const FORMAT: &str = "opentune-recipe";
/// The newest recipe format version this crate reads and writes.
pub const VERSION: u32 = 1;

/// A `[start, end)` byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    /// First byte.
    pub start: u64,
    /// One past the last byte.
    pub end: u64,
}

/// The identity of the stock image a recipe was made from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceIdentity {
    /// Image size in bytes.
    pub size: u64,
    /// Structural ECU family (`opentune-scan`), e.g. `EDC16` or `Unknown`.
    pub family: String,
    /// ECU definition id the recipe author named, if any.
    #[serde(default)]
    pub def_id: Option<String>,
    /// Hardware / spare-part numbers found in the image (VAG, Bosch, Mercedes).
    #[serde(default)]
    pub part_numbers: Vec<String>,
    /// Software / calibration numbers found in the image.
    #[serde(default)]
    pub sw_numbers: Vec<String>,
    /// SHA-256 of the whole stock image (informational).
    pub sha256: String,
    /// SHA-256 over every byte **outside** the patches and checksum fields,
    /// concatenated in order: equal only for the same software and calibration.
    pub unchanged_sha256: String,
}

/// One anchored change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Patch {
    /// Byte offset of the first changed byte in the source image.
    pub offset: u64,
    /// Source bytes at `offset` (hex).
    pub original: String,
    /// Replacement bytes (hex, same length as `original`).
    pub new: String,
    /// Unchanged source bytes immediately before `offset` (hex).
    #[serde(default)]
    pub context_before: String,
    /// Unchanged source bytes immediately after the patch (hex).
    #[serde(default)]
    pub context_after: String,
    /// Names of definition maps this patch touches (only with a definition).
    #[serde(default)]
    pub maps: Vec<String>,
}

/// A portable tune recipe (see `docs/RECIPES.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipe {
    /// Always [`FORMAT`].
    pub format: String,
    /// Format version (see [`VERSION`]).
    pub version: u32,
    /// Producer, e.g. `opentune 0.1.0`.
    #[serde(default)]
    pub tool: String,
    /// Free-text note from the author.
    #[serde(default)]
    pub note: String,
    /// What the recipe may be applied to.
    pub source: SourceIdentity,
    /// Checksum algorithm id (`opentune-checksum`) to correct with, if known.
    #[serde(default)]
    pub checksum: Option<String>,
    /// Byte ranges the checksum correction rewrote in the tuned image. They are
    /// never patched (the correction recomputes them) and are excluded from
    /// [`SourceIdentity::unchanged_sha256`].
    #[serde(default)]
    pub checksum_fields: Vec<Range>,
    /// SHA-256 of the tuned image the recipe was made from.
    pub result_sha256: String,
    /// The changes, in offset order.
    pub patches: Vec<Patch>,
}

/// Why a recipe could not be created or loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipeError {
    /// Stock and tuned differ in size.
    SizeMismatch(usize, usize),
    /// Stock and tuned are identical.
    NoChanges,
    /// The named checksum algorithm is unknown.
    UnknownChecksum(String),
    /// Bad JSON / not a recipe / unsupported version / malformed patch.
    Invalid(String),
}

impl std::fmt::Display for RecipeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecipeError::SizeMismatch(a, b) => {
                write!(f, "stock is {a} bytes but tuned is {b} bytes")
            }
            RecipeError::NoChanges => write!(f, "stock and tuned images are identical"),
            RecipeError::UnknownChecksum(a) => write!(f, "unknown checksum algorithm '{a}'"),
            RecipeError::Invalid(e) => write!(f, "invalid recipe: {e}"),
        }
    }
}

impl std::error::Error for RecipeError {}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Result<Vec<u8>, RecipeError> {
    if s.len() % 2 != 0 {
        return Err(RecipeError::Invalid(format!("odd-length hex '{s}'")));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|_| RecipeError::Invalid(format!("bad hex '{s}'")))
        })
        .collect()
}

/// A patch with its byte fields decoded.
struct Decoded {
    offset: usize,
    original: Vec<u8>,
    new: Vec<u8>,
    before: Vec<u8>,
    after: Vec<u8>,
}

impl Patch {
    fn decode(&self) -> Result<Decoded, RecipeError> {
        let d = Decoded {
            offset: usize::try_from(self.offset)
                .map_err(|_| RecipeError::Invalid("offset too large".into()))?,
            original: unhex(&self.original)?,
            new: unhex(&self.new)?,
            before: unhex(&self.context_before)?,
            after: unhex(&self.context_after)?,
        };
        if d.original.len() != d.new.len() || d.original.is_empty() {
            return Err(RecipeError::Invalid(format!(
                "patch @0x{:X}: original/new lengths differ or are empty",
                self.offset
            )));
        }
        Ok(d)
    }

    /// Patch length in bytes.
    pub fn len(&self) -> usize {
        self.original.len() / 2
    }

    /// `true` for a zero-length patch (never produced by [`create`]).
    pub fn is_empty(&self) -> bool {
        self.original.is_empty()
    }
}

impl Recipe {
    /// Parse and validate a recipe file.
    pub fn from_json(s: &str) -> Result<Self, RecipeError> {
        let r: Recipe = serde_json::from_str(s).map_err(|e| RecipeError::Invalid(e.to_string()))?;
        if r.format != FORMAT {
            return Err(RecipeError::Invalid(format!(
                "format is '{}', expected '{FORMAT}'",
                r.format
            )));
        }
        if r.version == 0 || r.version > VERSION {
            return Err(RecipeError::Invalid(format!(
                "version {} is not supported (this build reads <= {VERSION})",
                r.version
            )));
        }
        for p in &r.patches {
            p.decode()?;
        }
        Ok(r)
    }

    /// Serialise as pretty JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Total number of patched bytes.
    pub fn patched_bytes(&self) -> usize {
        self.patches.iter().map(Patch::len).sum()
    }
}

/// Identity facts read from an image.
struct ImageIdentity {
    family: String,
    part_numbers: Vec<String>,
    sw_numbers: Vec<String>,
}

fn identify(image: &[u8]) -> ImageIdentity {
    let fam = opentune_scan::family::detect(image);
    let ex = opentune_scan::extract::extract(image);
    let mut parts = ex.vag_part_numbers;
    parts.extend(ex.bosch_numbers);
    parts.extend(ex.mercedes_numbers);
    // VINs are deliberately NOT recorded: a recipe must not carry personal data.
    ImageIdentity {
        family: fam.family.as_str().to_string(),
        part_numbers: parts,
        sw_numbers: ex.sw_numbers,
    }
}

/// The checksum algorithm id for a structural family, if one is known.
pub fn checksum_for_family(family: &str) -> Option<&'static str> {
    match family {
        "EDC15" => Some("edc15"),
        "EDC16" => Some("edc16"),
        "EDC17" => Some("edc17"),
        "PCR2.1" => Some("pcr21"),
        _ => None,
    }
}

/// SHA-256 over every byte outside `excluded` (sorted, disjoint).
fn unchanged_hash(image: &[u8], excluded: &[Range]) -> String {
    let mut keep = Vec::with_capacity(image.len());
    let mut pos = 0usize;
    for r in excluded {
        let s = (r.start as usize).min(image.len());
        let e = (r.end as usize).min(image.len());
        if s > pos {
            keep.extend_from_slice(&image[pos..s]);
        }
        pos = pos.max(e);
    }
    keep.extend_from_slice(&image[pos.min(image.len())..]);
    sha256_hex(&keep)
}

fn merge_ranges(mut v: Vec<Range>) -> Vec<Range> {
    v.sort_by_key(|r| r.start);
    let mut out: Vec<Range> = Vec::new();
    for r in v {
        match out.last_mut() {
            Some(l) if r.start <= l.end => l.end = l.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// Byte ranges where `a` and `b` differ, merged across gaps shorter than `gap`.
fn diff_regions(a: &[u8], b: &[u8], gap: usize) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for i in 0..a.len().min(b.len()) {
        if a[i] != b[i] {
            match out.last_mut() {
                Some((_, e)) if i < *e + gap => *e = i + 1,
                _ => out.push((i, i + 1)),
            }
        }
    }
    out
}

/// Find the changed regions that are checksum fields: bytes the checksum
/// correction itself rewrote in the tuned image.
///
/// Corrupting every small (<= 16 byte) changed region and correcting makes the
/// algorithm rewrite the fields it owns; the regions it wrote into are the
/// candidates. They are confirmed together (revert only them, correct,
/// the tuned image must come back byte-for-byte) and, if that fails, one by
/// one. Anything unconfirmed stays an ordinary patch.
fn checksum_fields(
    algo: &dyn opentune_checksum::ChecksumAlgo,
    stock: &[u8],
    tuned: &[u8],
    regions: &[(usize, usize)],
) -> Vec<Range> {
    let small: Vec<(usize, usize)> = regions
        .iter()
        .copied()
        .filter(|(s, e)| e - s <= 16)
        .collect();
    if small.is_empty() {
        return Vec::new();
    }
    let revert = |set: &[(usize, usize)]| {
        let mut x = tuned.to_vec();
        for &(s, e) in set {
            x[s..e].copy_from_slice(&stock[s..e]);
        }
        x
    };
    // Probe: invert every small region so each is certainly wrong (reverting
    // them all could reproduce the stock image, whose checksums are valid).
    let mut probe = tuned.to_vec();
    for &(s, e) in &small {
        for b in &mut probe[s..e] {
            *b = !*b;
        }
    }
    let Ok(fixes) = algo.correct_with_reference(&mut probe, stock) else {
        return Vec::new();
    };
    let cands: Vec<(usize, usize)> = small
        .iter()
        .copied()
        .filter(|&(s, e)| {
            fixes
                .iter()
                .any(|f| (f.offset as usize) < e && (f.offset as usize) + 8 > s)
        })
        .collect();
    let confirm = |set: &[(usize, usize)]| {
        let mut x = revert(set);
        algo.correct_with_reference(&mut x, stock).is_ok() && x == tuned
    };
    let ok: Vec<(usize, usize)> = if !cands.is_empty() && confirm(&cands) {
        cands
    } else {
        cands
            .into_iter()
            .filter(|r| confirm(std::slice::from_ref(r)))
            .collect()
    };
    ok.into_iter()
        .map(|(s, e)| Range {
            start: s as u64,
            end: e as u64,
        })
        .collect()
}

/// Options for [`create`].
#[derive(Debug, Clone)]
pub struct CreateOptions {
    /// Context bytes stored on each side of a patch.
    pub context: usize,
    /// Changes separated by fewer unchanged bytes than this become one patch.
    pub gap: usize,
    /// Checksum algorithm id; `None` = derive from the structural family.
    pub checksum: Option<String>,
    /// Definition id to record (informational).
    pub def_id: Option<String>,
    /// Maps used to name the patches.
    pub maps: Vec<MapDef>,
    /// Author's note.
    pub note: String,
    /// Producer string recorded in the recipe.
    pub tool: String,
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            context: 8,
            gap: 16,
            checksum: None,
            def_id: None,
            maps: Vec::new(),
            note: String::new(),
            tool: format!("opentune-recipe {}", env!("CARGO_PKG_VERSION")),
        }
    }
}

fn maps_touching(maps: &[MapDef], s: usize, e: usize) -> Vec<String> {
    let mut out = Vec::new();
    let hit = |a: usize, len: usize| a < e && s < a + len;
    for m in maps {
        if hit(m.address as usize, m.byte_len() as usize) {
            out.push(m.name.clone());
        }
        for (label, ax) in [("x axis", &m.x_axis), ("y axis", &m.y_axis)] {
            if let Some(ax) = ax {
                if let Some(a) = ax.address {
                    let len = ax.count as usize * ax.data_type.byte_len() as usize;
                    if hit(a as usize, len) {
                        out.push(format!("{} ({label})", m.name));
                    }
                }
            }
        }
    }
    out
}

/// Build a recipe from a stock image and its tuned version. Returns the recipe
/// and any warnings (e.g. no checksum algorithm could be determined).
pub fn create(
    stock: &[u8],
    tuned: &[u8],
    opts: &CreateOptions,
) -> Result<(Recipe, Vec<String>), RecipeError> {
    if stock.len() != tuned.len() {
        return Err(RecipeError::SizeMismatch(stock.len(), tuned.len()));
    }
    let mut warnings = Vec::new();
    let id = identify(stock);
    let checksum = match &opts.checksum {
        Some(a) => {
            if opentune_checksum::get(a).is_none() {
                return Err(RecipeError::UnknownChecksum(a.clone()));
            }
            Some(a.clone())
        }
        None => {
            let a = checksum_for_family(&id.family).map(str::to_string);
            if a.is_none() {
                warnings.push(format!(
                    "structural family '{}' has no known checksum algorithm: the recipe \
                     will only apply with an explicit --checksum",
                    id.family
                ));
            }
            a
        }
    };
    let gap = opts.gap.max(1);
    let regions = diff_regions(stock, tuned, gap);
    if regions.is_empty() {
        return Err(RecipeError::NoChanges);
    }

    // Which small changed regions are checksum fields? See `checksum_fields`.
    let cs_fields = match checksum.as_deref().and_then(opentune_checksum::get) {
        Some(algo) => checksum_fields(&*algo, stock, tuned, &regions),
        None => Vec::new(),
    };
    let is_cs = |s: usize| cs_fields.iter().any(|r| r.start as usize == s);

    let mut patches = Vec::new();
    let mut excluded = cs_fields.clone();
    for &(s, e) in &regions {
        if is_cs(s) {
            continue;
        }
        let b = s.saturating_sub(opts.context);
        let a = (e + opts.context).min(stock.len());
        patches.push(Patch {
            offset: s as u64,
            original: hex(&stock[s..e]),
            new: hex(&tuned[s..e]),
            context_before: hex(&stock[b..s]),
            context_after: hex(&stock[e..a]),
            maps: maps_touching(&opts.maps, s, e),
        });
        excluded.push(Range {
            start: s as u64,
            end: e as u64,
        });
    }
    if patches.is_empty() {
        return Err(RecipeError::NoChanges);
    }
    let excluded = merge_ranges(excluded);
    let recipe = Recipe {
        format: FORMAT.into(),
        version: VERSION,
        tool: opts.tool.clone(),
        note: opts.note.clone(),
        source: SourceIdentity {
            size: stock.len() as u64,
            family: id.family,
            def_id: opts.def_id.clone(),
            part_numbers: id.part_numbers,
            sw_numbers: id.sw_numbers,
            sha256: sha256_hex(stock),
            unchanged_sha256: unchanged_hash(stock, &excluded),
        },
        checksum,
        checksum_fields: cs_fields,
        result_sha256: sha256_hex(tuned),
        patches,
    };
    Ok((recipe, warnings))
}

/// How strictly [`apply`] anchors patches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyMode {
    /// The target must be the recipe's source software (unchanged-region hash)
    /// and every patch must match at its recorded offset.
    Strict,
    /// Same size, family and hardware part number required; the unchanged-region
    /// hash is informational. A patch whose anchor does not match at its offset
    /// is searched for by its context+original bytes and accepted only when it
    /// occurs exactly once.
    Relocate,
}

/// Options for [`apply`].
#[derive(Debug, Clone)]
pub struct ApplyOptions {
    /// Anchoring mode.
    pub mode: ApplyMode,
    /// Checksum algorithm override (otherwise the recipe's).
    pub checksum: Option<String>,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            mode: ApplyMode::Strict,
            checksum: None,
        }
    }
}

/// One identity comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdentityCheck {
    /// What was compared.
    pub item: String,
    /// Value recorded in the recipe.
    pub expected: String,
    /// Value found in the target.
    pub found: String,
    /// Whether it matched.
    pub ok: bool,
    /// Whether a mismatch refuses the apply in this mode.
    pub required: bool,
}

/// What happened to one patch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum PatchStatus {
    /// Original bytes and context matched at the recorded offset.
    Anchored {
        /// Where it was written.
        at: u64,
    },
    /// Found (exactly once) at a different offset — relocate mode only.
    Relocated {
        /// Recorded offset.
        from: u64,
        /// Offset it was found and written at.
        to: u64,
    },
    /// The new bytes are already present at the recorded offset.
    AlreadyApplied {
        /// Where.
        at: u64,
    },
    /// The bytes at the recorded offset differ from the recipe's original.
    Mismatch {
        /// What differed.
        detail: String,
    },
    /// Relocate mode: the context+original sequence occurs nowhere.
    NotFound,
    /// Relocate mode: the context+original sequence occurs more than once.
    Ambiguous {
        /// How many places (capped).
        matches: usize,
    },
    /// Two patches resolved onto overlapping bytes.
    Overlap,
}

impl PatchStatus {
    fn usable(&self) -> bool {
        matches!(
            self,
            PatchStatus::Anchored { .. }
                | PatchStatus::Relocated { .. }
                | PatchStatus::AlreadyApplied { .. }
        )
    }
}

/// Per-patch outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PatchOutcome {
    /// Index in the recipe.
    pub index: usize,
    /// Recorded offset.
    pub offset: u64,
    /// Length in bytes.
    pub len: usize,
    /// Map names recorded for the patch.
    pub maps: Vec<String>,
    /// What happened.
    pub status: PatchStatus,
}

/// The full result of [`apply`]. `output` is `Some` only on success.
#[derive(Debug, Clone, Serialize)]
pub struct ApplyReport {
    /// Mode used.
    pub mode: ApplyMode,
    /// Identity comparisons.
    pub identity: Vec<IdentityCheck>,
    /// Per-patch outcomes (empty when identity already refused).
    pub patches: Vec<PatchOutcome>,
    /// Checksum algorithm used.
    pub checksum: Option<String>,
    /// Checksum corrections made after patching, as text.
    pub checksum_fixes: Vec<String>,
    /// Attestation of the output (present once checksums were evaluated).
    pub attestation: Option<ChecksumAttestation>,
    /// The output is byte-identical to the tuned image the recipe came from.
    pub identical_to_recipe_result: bool,
    /// Why the apply was refused, if it was.
    pub refusal: Option<String>,
    /// The patched, checksum-corrected and attested image.
    #[serde(skip)]
    pub output: Option<Vec<u8>>,
}

impl ApplyReport {
    fn refuse(mut self, why: impl Into<String>) -> Self {
        self.refusal = Some(why.into());
        self.output = None;
        self
    }
}

fn context_matches(t: &[u8], at: usize, d: &Decoded, body: &[u8]) -> bool {
    let len = body.len();
    if at < d.before.len() || at + len + d.after.len() > t.len() {
        return false;
    }
    t[at - d.before.len()..at] == d.before[..]
        && t[at..at + len] == body[..]
        && t[at + len..at + len + d.after.len()] == d.after[..]
}

/// Offsets (of the body) where `before+body+after` occurs; stops after `cap`.
fn find_all(t: &[u8], d: &Decoded, cap: usize) -> Vec<usize> {
    let mut pat = d.before.clone();
    pat.extend_from_slice(&d.original);
    pat.extend_from_slice(&d.after);
    let mut out = Vec::new();
    if pat.is_empty() || pat.len() > t.len() {
        return out;
    }
    for (i, w) in t.windows(pat.len()).enumerate() {
        if w == &pat[..] {
            out.push(i + d.before.len());
            if out.len() >= cap {
                break;
            }
        }
    }
    out
}

fn first_diff(t: &[u8], at: usize, want: &[u8]) -> String {
    match t.get(at..at + want.len()) {
        None => "runs past the end of the target".into(),
        Some(got) => match got.iter().zip(want).position(|(a, b)| a != b) {
            Some(i) => format!(
                "byte 0x{:X}: target 0x{:02X}, recipe expects 0x{:02X}",
                at + i,
                got[i],
                want[i]
            ),
            None => "context bytes around the patch differ".into(),
        },
    }
}

fn list(v: &[String]) -> String {
    if v.is_empty() {
        "-".into()
    } else {
        v.join(", ")
    }
}

/// Apply `recipe` to `target`. Never panics on bad input; check
/// [`ApplyReport::refusal`] / [`ApplyReport::output`].
pub fn apply(recipe: &Recipe, target: &[u8], opts: &ApplyOptions) -> ApplyReport {
    let strict = opts.mode == ApplyMode::Strict;
    let checksum = opts.checksum.clone().or_else(|| recipe.checksum.clone());
    let mut rep = ApplyReport {
        mode: opts.mode,
        identity: Vec::new(),
        patches: Vec::new(),
        checksum: checksum.clone(),
        checksum_fixes: Vec::new(),
        attestation: None,
        identical_to_recipe_result: false,
        refusal: None,
        output: None,
    };
    if recipe.format != FORMAT || recipe.version == 0 || recipe.version > VERSION {
        return rep.refuse(format!(
            "unsupported recipe '{}' v{}",
            recipe.format, recipe.version
        ));
    }
    let decoded: Vec<Decoded> = match recipe.patches.iter().map(Patch::decode).collect() {
        Ok(d) => d,
        Err(e) => return rep.refuse(e.to_string()),
    };

    // ---- identity ----
    let src = &recipe.source;
    let id = identify(target);
    rep.identity.push(IdentityCheck {
        item: "size".into(),
        expected: src.size.to_string(),
        found: target.len().to_string(),
        ok: src.size == target.len() as u64,
        required: true,
    });
    rep.identity.push(IdentityCheck {
        item: "structural family".into(),
        expected: src.family.clone(),
        found: id.family.clone(),
        ok: src.family == id.family,
        required: true,
    });
    let parts_ok = if strict {
        src.part_numbers.iter().all(|p| id.part_numbers.contains(p))
    } else {
        src.part_numbers.is_empty() || src.part_numbers.iter().any(|p| id.part_numbers.contains(p))
    };
    rep.identity.push(IdentityCheck {
        item: if strict {
            "part numbers (all)".into()
        } else {
            "part numbers (any shared)".into()
        },
        expected: list(&src.part_numbers),
        found: list(&id.part_numbers),
        ok: parts_ok,
        required: true,
    });
    rep.identity.push(IdentityCheck {
        item: "software numbers".into(),
        expected: list(&src.sw_numbers),
        found: list(&id.sw_numbers),
        ok: src.sw_numbers.iter().all(|s| id.sw_numbers.contains(s)),
        required: strict,
    });
    let mut excluded: Vec<Range> = recipe.checksum_fields.clone();
    for (p, d) in recipe.patches.iter().zip(&decoded) {
        excluded.push(Range {
            start: p.offset,
            end: p.offset + d.original.len() as u64,
        });
    }
    let uh = unchanged_hash(target, &merge_ranges(excluded));
    rep.identity.push(IdentityCheck {
        item: "unchanged-region hash".into(),
        expected: src.unchanged_sha256.clone(),
        found: uh.clone(),
        ok: uh == src.unchanged_sha256,
        required: strict,
    });
    if let Some(bad) = rep.identity.iter().find(|c| c.required && !c.ok) {
        let why = format!(
            "identity mismatch: {} (recipe {}, target {}){}",
            bad.item,
            bad.expected,
            bad.found,
            if strict {
                "; this target is not the image the recipe was made from"
            } else {
                ""
            }
        );
        return rep.refuse(why);
    }

    // ---- resolve every patch against the pristine target ----
    for (i, (p, d)) in recipe.patches.iter().zip(&decoded).enumerate() {
        let at = d.offset;
        let status = if context_matches(target, at, d, &d.original) {
            PatchStatus::Anchored { at: at as u64 }
        } else if context_matches(target, at, d, &d.new) {
            PatchStatus::AlreadyApplied { at: at as u64 }
        } else if strict {
            PatchStatus::Mismatch {
                detail: first_diff(target, at, &d.original),
            }
        } else {
            match find_all(target, d, 3).as_slice() {
                [] => PatchStatus::NotFound,
                [to] => PatchStatus::Relocated {
                    from: at as u64,
                    to: *to as u64,
                },
                many => PatchStatus::Ambiguous {
                    matches: many.len(),
                },
            }
        };
        rep.patches.push(PatchOutcome {
            index: i,
            offset: p.offset,
            len: d.original.len(),
            maps: p.maps.clone(),
            status,
        });
    }
    // Resolved ranges must not overlap.
    let mut spans: Vec<(usize, usize, usize)> = rep
        .patches
        .iter()
        .filter_map(|o| match o.status {
            PatchStatus::Anchored { at } | PatchStatus::AlreadyApplied { at } => {
                Some((at as usize, at as usize + o.len, o.index))
            }
            PatchStatus::Relocated { to, .. } => Some((to as usize, to as usize + o.len, o.index)),
            _ => None,
        })
        .collect();
    spans.sort();
    for w in spans.windows(2) {
        if w[1].0 < w[0].1 {
            rep.patches[w[1].2].status = PatchStatus::Overlap;
        }
    }
    let bad = rep.patches.iter().filter(|o| !o.status.usable()).count();
    let total = rep.patches.len();
    if bad > 0 {
        return rep.refuse(format!(
            "{bad} of {total} patch(es) could not be anchored; nothing was applied"
        ));
    }
    let already = rep
        .patches
        .iter()
        .filter(|o| matches!(o.status, PatchStatus::AlreadyApplied { .. }))
        .count();
    if already > 0 && already < total {
        return rep.refuse(format!(
            "target already carries {already} of {total} patch(es) — a partial or different \
             tune; refusing to mix"
        ));
    }

    // ---- checksum algorithm ----
    let Some(algo_id) = checksum else {
        return rep.refuse(
            "no checksum algorithm is known for this family; pass an explicit checksum \
             algorithm if you know it (refusing to write an image with unverified checksums)",
        );
    };
    let Some(algo) = opentune_checksum::get(&algo_id) else {
        return rep.refuse(format!("unknown checksum algorithm '{algo_id}'"));
    };

    // ---- write, correct, attest ----
    let mut out = target.to_vec();
    for (o, d) in rep.patches.iter().zip(&decoded) {
        let at = match o.status {
            PatchStatus::Anchored { at } => at as usize,
            PatchStatus::Relocated { to, .. } => to as usize,
            _ => continue,
        };
        out[at..at + d.new.len()].copy_from_slice(&d.new);
    }
    match algo.correct_with_reference(&mut out, target) {
        Ok(fixes) => {
            rep.checksum_fixes = fixes
                .iter()
                .map(|f| {
                    format!(
                        "{} @0x{:X}: 0x{:X} -> 0x{:X}",
                        f.name, f.offset, f.old, f.new
                    )
                })
                .collect();
        }
        Err(e) => return rep.refuse(format!("checksum correction ({algo_id}) failed: {e}")),
    }
    match opentune_attest::attest_image(&algo_id, &out) {
        Ok(att) => {
            let ok = att.satisfied;
            let note = att.note.clone();
            rep.attestation = Some(att);
            if !ok {
                return rep.refuse(format!("attestation not satisfied: {note}"));
            }
        }
        Err(e) => return rep.refuse(format!("attestation failed: {e}")),
    }
    rep.identical_to_recipe_result = sha256_hex(&out) == recipe.result_sha256;
    rep.output = Some(out);
    rep
}

#[cfg(test)]
mod tests;
