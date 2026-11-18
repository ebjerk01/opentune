//! Signature-based ECU recognition: *which supplier/family/variant is this
//! image, how sure are we, and what exactly did we see where?*
//!
//! [`crate::family`] only knows the four families OpenTune has a checksum
//! engine for, and recognises them by checksum *structure*. This module widens
//! recognition to the other common ECU families by looking for the markers the
//! suppliers' toolchains leave in every image: Bosch project descriptors
//! (`44/1/ME7.5/...`), the EDC15 `TSW` toolchain line, ME7's `ZZ` ident block,
//! EDC16's `DECAFE` header, Siemens `5WK9…--…MS43` ident records, VAG ident
//! blocks (`03G906018DK R4 2.0l PPD1.2`), Marelli `MJD6JFHW…` tags, supplier
//! copyright strings, SuperH reset vectors, and so on.
//!
//! Every [`Detection`] carries its [`Evidence`] — the marker, the byte offset
//! it was found at and the bytes themselves — and a coarse [`Tier`]:
//!
//! * **high** — a family name inside a *structured* record (descriptor, ident
//!   record, toolchain line), a checksum directory, or two independent
//!   detectors that agree;
//! * **medium** — one family literal, or one supplier-specific anchor;
//! * **low** — a weak/ambiguous anchor (e.g. `ZZ` at 0x10000 alone), or
//!   supplier-only knowledge.
//!
//! There are deliberately **no numeric confidence scores**: a percentage would
//! be a number nobody measured. A variant is only reported when a string in the
//! image names it; layout/size observations go into the evidence text instead
//! of being promoted to a variant.
//!
//! Part of the detection knowledge is re-implemented (in Rust, from scratch)
//! from the MIT-licensed `openremap-core` project — see `NOTICE` in this crate.

mod bosch;
mod continental;
mod others;
mod tcu;
mod vag;

use serde::Serialize;

pub(crate) use bosch::descriptors;
pub(crate) use tcu::tcu_parts;
pub(crate) use vag::ident_blocks;

/// Coarse confidence tier of a detection (ordered `Low < Medium < High`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Low,
    Medium,
    High,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Low => "low",
            Tier::Medium => "medium",
            Tier::High => "high",
        }
    }
}

/// One observed fact backing a detection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Evidence {
    /// What was matched, in words.
    pub marker: String,
    /// Byte offset of the match in the image (absent for whole-image facts).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// The matched bytes, non-printables escaped, truncated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl Evidence {
    /// One-line human rendering (`marker @0x… "text"`).
    pub fn line(&self) -> String {
        let mut s = self.marker.clone();
        if let Some(o) = self.offset {
            s.push_str(&format!(" @0x{o:X}"));
        }
        if let Some(t) = &self.text {
            s.push_str(&format!(" \"{t}\""));
        }
        s
    }
}

/// Supplier names used in detections.
pub mod supplier {
    pub const BOSCH: &str = "Bosch";
    pub const CONTINENTAL: &str = "Siemens/Continental";
    pub const DELPHI: &str = "Delphi";
    pub const MARELLI: &str = "Magneti Marelli";
    pub const DENSO: &str = "Denso";
    /// Temic (Continental) — VAG DQ250 DSG TCUs.
    pub const TEMIC: &str = "Temic";
    /// VAG transmission whose TCU supplier no open source confirms.
    pub const VAG_TCU: &str = "VAG (supplier unconfirmed)";
}

/// Family name used when only the supplier is known.
pub const UNKNOWN_FAMILY: &str = "Unknown";

/// One detector's claim about an image.
#[derive(Debug, Clone, Serialize)]
pub struct Detection {
    pub supplier: &'static str,
    /// Canonical family (`"ME7"`, `"EDC17"`, `"SIMOS"`, …) or [`UNKNOWN_FAMILY`].
    pub family: &'static str,
    /// Sub-variant exactly as named by the image (`"ME7.5.10"`, `"EDC17CP14"`).
    pub variant: Option<String>,
    pub confidence: Tier,
    /// Which detector(s) produced it.
    pub detectors: Vec<&'static str>,
    pub evidence: Vec<Evidence>,
}

impl Detection {
    pub(crate) fn new(
        supplier: &'static str,
        family: &'static str,
        confidence: Tier,
        detector: &'static str,
    ) -> Self {
        Self {
            supplier,
            family,
            variant: None,
            confidence,
            detectors: vec![detector],
            evidence: Vec::new(),
        }
    }

    pub(crate) fn variant(mut self, v: Option<String>) -> Self {
        self.variant = v.filter(|v| !v.is_empty() && v != self.family);
        self
    }

    pub(crate) fn ev(
        mut self,
        marker: impl Into<String>,
        off: Option<usize>,
        text: Option<String>,
    ) -> Self {
        self.evidence.push(Evidence {
            marker: marker.into(),
            offset: off.map(|o| o as u64),
            text,
        });
        self
    }

    /// Whether this detection names a family (not just a supplier).
    pub fn has_family(&self) -> bool {
        self.family != UNKNOWN_FAMILY
    }
}

/// What kind of file this is, before any family detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImageKind {
    /// A raw flash/EEPROM image (the only kind family detection runs on).
    Raw,
    Empty,
    /// Every byte identical (erased or zeroed dump).
    Blank,
    /// Mostly printable text: DAMOS/DCM/A2L/XML and the like, not an image.
    Text,
    /// A WinOLS project (`.ols`/`.kp`) container.
    WinolsProject,
    /// A zip archive.
    ZipArchive,
    /// An SMS-Soft packed container (its header tag may name the ECU).
    SmsSoftContainer,
    /// An ECM Titanium driver/definition file (`\x84\xBB1\xBB` header).
    EcmTitaniumDriver,
}

impl ImageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ImageKind::Raw => "raw",
            ImageKind::Empty => "empty",
            ImageKind::Blank => "blank",
            ImageKind::Text => "text",
            ImageKind::WinolsProject => "winols-project",
            ImageKind::ZipArchive => "zip-archive",
            ImageKind::SmsSoftContainer => "sms-soft-container",
            ImageKind::EcmTitaniumDriver => "ecm-titanium-driver",
        }
    }
}

/// Classify the file kind from its header and byte statistics.
pub fn image_kind(b: &[u8]) -> ImageKind {
    if b.is_empty() {
        return ImageKind::Empty;
    }
    if b.iter().all(|&x| x == b[0]) {
        return ImageKind::Blank;
    }
    if b.get(4..15) == Some(b"WinOLS File".as_slice()) {
        return ImageKind::WinolsProject;
    }
    if b.starts_with(b"PK\x03\x04") {
        return ImageKind::ZipArchive;
    }
    if b.starts_with(b"SMS-Soft") {
        return ImageKind::SmsSoftContainer;
    }
    if b.starts_with(b"\x84\xBB1\xBB") {
        return ImageKind::EcmTitaniumDriver;
    }
    let head = &b[..b.len().min(0x10000)];
    let printable = head
        .iter()
        .filter(|&&c| (0x20..0x7F).contains(&c) || c == b'\t' || c == b'\n' || c == b'\r')
        .count();
    // 85 %: real images are dominated by fill (0xFF/0x00/0xC3) and code bytes;
    // text definitions (DCM/DAMOS/A2L) are ~100 % printable.
    if printable * 100 >= head.len() * 85 {
        return ImageKind::Text;
    }
    ImageKind::Raw
}

/// The full recognition result for one image.
#[derive(Debug, Clone, Serialize)]
pub struct Recognition {
    pub image_kind: ImageKind,
    /// The best-supported detection, if any.
    pub primary: Option<Detection>,
    /// Every other detection, best first (incl. ones merged into `primary`).
    pub others: Vec<Detection>,
    /// True when two detections of incompatible families tie at the top tier.
    pub contested: bool,
}

impl Recognition {
    pub fn family(&self) -> &'static str {
        self.primary.as_ref().map_or(UNKNOWN_FAMILY, |d| d.family)
    }
}

// ---------------------------------------------------------------------------
// Byte-search helpers shared by the detectors.
// ---------------------------------------------------------------------------

/// Thin wrapper over the image with the search primitives detectors need.
pub(crate) struct Img<'a> {
    pub b: &'a [u8],
}

impl<'a> Img<'a> {
    pub fn len(&self) -> usize {
        self.b.len()
    }
    pub fn find(&self, pat: &[u8]) -> Option<usize> {
        memchr::memmem::find(self.b, pat)
    }
    /// First match starting in `[start, end)`.
    pub fn find_in(&self, pat: &[u8], start: usize, end: usize) -> Option<usize> {
        let end = end.min(self.b.len());
        if start >= end {
            return None;
        }
        let stop = (end + pat.len() - 1).min(self.b.len());
        memchr::memmem::find(&self.b[start..stop], pat).map(|o| o + start)
    }
    pub fn find_iter<'p>(&self, pat: &'p [u8]) -> memchr::memmem::FindIter<'a, 'p> {
        memchr::memmem::find_iter(self.b, pat)
    }
    pub fn contains(&self, pat: &[u8]) -> bool {
        self.find(pat).is_some()
    }
    pub fn at(&self, off: usize, pat: &[u8]) -> bool {
        self.b.get(off..off + pat.len()) == Some(pat)
    }
    pub fn byte(&self, off: usize) -> Option<u8> {
        self.b.get(off).copied()
    }
    /// The run of bytes from `off` satisfying `pred`, at most `max` long.
    pub fn run(&self, off: usize, max: usize, pred: impl Fn(u8) -> bool) -> &'a [u8] {
        let s = &self.b[off.min(self.b.len())..];
        let n = s.iter().take(max).take_while(|&&c| pred(c)).count();
        &s[..n]
    }
    /// Printable rendering of `len` bytes at `off` (non-printables as `.`).
    pub fn snippet(&self, off: usize, len: usize) -> String {
        printable(&self.b[off.min(self.b.len())..(off + len).min(self.b.len())])
    }
    /// True when the byte before `off` cannot extend an alphanumeric token.
    pub fn left_bound(&self, off: usize) -> bool {
        off == 0 || !self.b[off - 1].is_ascii_alphanumeric()
    }
    /// True when the byte at `off` cannot extend an alphanumeric token.
    pub fn right_bound(&self, off: usize) -> bool {
        self.b.get(off).map_or(true, |c| !c.is_ascii_alphanumeric())
    }
    /// Literal occurrences of `pat` not glued to a preceding letter/digit.
    pub fn literal_iter<'p>(&'p self, pat: &'p [u8]) -> impl Iterator<Item = usize> + 'p {
        self.find_iter(pat).filter(move |&o| self.left_bound(o))
    }
    /// `count` ASCII digits starting at `off`.
    pub fn digits_at(&self, off: usize, count: usize) -> bool {
        self.b
            .get(off..off + count)
            .is_some_and(|s| s.iter().all(u8::is_ascii_digit))
    }
}

/// Render bytes as printable ASCII, non-printables as `.`, max 96 chars.
pub(crate) fn printable(s: &[u8]) -> String {
    s.iter()
        .take(96)
        .map(|&c| {
            if (0x20..0x7F).contains(&c) {
                c as char
            } else {
                '.'
            }
        })
        .collect()
}

pub(crate) fn is_token_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'.'
}

pub(crate) fn is_print(c: u8) -> bool {
    (0x20..0x7F).contains(&c)
}

/// Find a 10-digit Bosch number with the given 4-digit prefix (boundary checked).
pub(crate) fn find_bosch_number(
    img: &Img,
    prefix: &[u8],
    start: usize,
    end: usize,
) -> Option<usize> {
    let end = end.min(img.len());
    let mut from = start;
    while let Some(o) = img.find_in(prefix, from, end) {
        if img.left_bound(o)
            && img.digits_at(o + prefix.len(), 10 - prefix.len())
            && img.byte(o + 10).map_or(true, |c| !c.is_ascii_digit())
        {
            return Some(o);
        }
        from = o + 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Running and merging.
// ---------------------------------------------------------------------------

/// Families that share a checksum/toolchain generation: a structural EDC17
/// directory is *compatible* with a MED17 string (same generation), so the more
/// specific string family wins without the result being "contested".
fn generation(family: &str) -> &str {
    match family {
        "EDC17" | "MEDC17" | "MED17" | "MEVD17" | "ME17" | "MD1" | "MG1" => "bosch-17",
        "ME9" | "MED9" => "bosch-9",
        other => other,
    }
}

fn specificity(d: &Detection) -> u8 {
    (d.has_family() as u8) * 2 + d.variant.is_some() as u8
}

/// Run every detector over a raw image and merge the claims. `structural` are
/// detections derived from checksum structure (see [`crate::family`]).
pub fn recognize(b: &[u8], structural: Vec<Detection>) -> Recognition {
    let kind = image_kind(b);
    let mut all: Vec<Detection> = Vec::new();
    match kind {
        ImageKind::Raw => {
            all.extend(structural);
            let img = Img { b };
            bosch::detect(&img, &mut all);
            vag::detect(&img, &mut all);
            continental::detect(&img, &mut all);
            others::detect(&img, &mut all);
            tcu::detect(&img, &mut all);
        }
        ImageKind::SmsSoftContainer => bosch::sms_soft(&Img { b }, &mut all),
        _ => {}
    }
    merge(kind, all)
}

/// Fold `src` into `dst` (same family, or a compatible structural claim).
fn absorb(dst: &mut Detection, src: Detection) {
    let independent = src.detectors.iter().any(|x| !dst.detectors.contains(x));
    let both_backed = dst.confidence >= Tier::Medium && src.confidence >= Tier::Medium;
    if independent && both_backed && dst.has_family() {
        dst.confidence = Tier::High;
    }
    let better_variant = match (&dst.variant, &src.variant) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(a), Some(b)) => {
            src.confidence > dst.confidence
                || (src.confidence == dst.confidence && b.len() > a.len())
        }
    };
    if better_variant {
        dst.variant = src.variant.clone();
    }
    dst.confidence = dst.confidence.max(src.confidence);
    for x in src.detectors {
        if !dst.detectors.contains(&x) {
            dst.detectors.push(x);
        }
    }
    for e in src.evidence {
        if !dst.evidence.contains(&e) {
            dst.evidence.push(e);
        }
    }
}

/// Detector id of the checksum-structure detections built by `crate::family`.
pub const STRUCTURAL: &str = "checksum-structure";

fn merge(kind: ImageKind, all: Vec<Detection>) -> Recognition {
    // 1. Collapse claims of the same (supplier, family). Two independent
    //    medium-or-better detectors agreeing promote the result to high.
    let mut groups: Vec<Detection> = Vec::new();
    for d in all {
        match groups
            .iter_mut()
            .find(|g| g.family == d.family && g.supplier == d.supplier)
        {
            Some(g) => absorb(g, d),
            None => groups.push(d),
        }
    }

    // 2. A purely structural claim (e.g. the EDC17 checksum directory, shared
    //    by the whole EDC17/MED17/ME17 generation) is folded into a string
    //    detection of the same generation, which names the family precisely.
    let mut i = 0;
    while i < groups.len() {
        let structural_only = groups[i].detectors == [STRUCTURAL];
        let target = structural_only
            .then(|| {
                groups.iter().position(|g| {
                    g.has_family()
                        && g.supplier == groups[i].supplier
                        && g.family != groups[i].family
                        && generation(g.family) == generation(groups[i].family)
                })
            })
            .flatten();
        match target {
            Some(j) => {
                let s = groups.remove(i);
                let j = if j > i { j - 1 } else { j };
                absorb(&mut groups[j], s);
            }
            None => i += 1,
        }
    }

    groups.sort_by(|a, b| {
        b.confidence
            .cmp(&a.confidence)
            .then(specificity(b).cmp(&specificity(a)))
            .then(b.evidence.len().cmp(&a.evidence.len()))
    });
    let mut it = groups.into_iter();
    let primary = it.next();
    let others: Vec<Detection> = it.collect();
    let contested = primary.as_ref().is_some_and(|p| {
        p.has_family()
            && others.iter().any(|o| {
                o.has_family()
                    && o.confidence == p.confidence
                    && generation(o.family) != generation(p.family)
            })
    });
    Recognition {
        image_kind: kind,
        primary,
        others,
        contested,
    }
}
