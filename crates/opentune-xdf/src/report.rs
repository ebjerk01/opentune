//! Per-object import outcome reporting, shared by the definition importers
//! (TunerPro XDF here, EcuFlash/RomRaider/FastECU in `opentune-ecuflash`).
//!
//! Every object an importer meets ends in exactly one [`Status`]:
//!
//! * [`Status::Clean`] — imported faithfully; nothing was lost or guessed.
//! * [`Status::Warned`] — imported, but with an explicit, per-object
//!   fidelity loss ([`Issue`]s, also written into the map's `notes`). Never a
//!   silently wrong address or scale: a warned map is *correct but
//!   incomplete* (e.g. pre-scaled because the equation is non-linear, or an
//!   axis whose breakpoints could not be resolved).
//! * [`Status::Refused`] — not imported, with the reason. Used whenever the
//!   only possible import would place or decode cells wrongly (non-contiguous
//!   strides, an unsupported element width, an address outside the image…),
//!   and for object classes OpenTune does not model as maps (bit flags,
//!   patches, checksum blocks, switch/selection tables).
//!
//! A *strict* import ([`strict_failures`]) refuses the whole file when any
//! map-like object (table, function, constant) is warned or refused. Objects
//! of a class OpenTune never imports as a map ([`ObjectKind::is_map_like`] is
//! false) are always refused-with-reason and listed, but do not by themselves
//! fail a strict import — they are out of scope, not a fidelity loss.

use std::collections::BTreeMap;
use std::fmt;

/// Final outcome of importing one definition object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Status {
    /// Imported faithfully.
    Clean,
    /// Imported with explicit per-object warnings.
    Warned,
    /// Not imported; see the refusal issue.
    Refused,
}

impl Status {
    /// Stable lower-case id (`clean` / `warned` / `refused`).
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Clean => "clean",
            Status::Warned => "warned",
            Status::Refused => "refused",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What kind of definition object a report is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ObjectKind {
    /// A table / map (`<XDFTABLE>`, EcuFlash/RomRaider `<table>`, CSV row).
    Table,
    /// A 1-D lookup function (`<XDFFUNCTION>`), imported as a curve.
    Function,
    /// A scalar constant (`<XDFCONSTANT>`).
    Constant,
    /// A bit flag (`<XDFFLAG>`, `%%FLAG%%`). Never imported as a map.
    Flag,
    /// A byte patch (`<XDFPATCH>`). Never imported.
    Patch,
    /// A checksum descriptor (`<XDFCHECKSUM>`). Never imported.
    Checksum,
    /// A switch / selection table (EcuFlash `bloblist`, RomRaider `Switch`).
    Switch,
    /// A table/function/constant entry with no data address at all — a
    /// separator heading or information label (`----IDLE----`). Nothing to
    /// import.
    Placeholder,
}

impl ObjectKind {
    /// Stable lower-case id.
    pub fn as_str(self) -> &'static str {
        match self {
            ObjectKind::Table => "table",
            ObjectKind::Function => "function",
            ObjectKind::Constant => "constant",
            ObjectKind::Flag => "flag",
            ObjectKind::Patch => "patch",
            ObjectKind::Checksum => "checksum",
            ObjectKind::Switch => "switch",
            ObjectKind::Placeholder => "placeholder",
        }
    }

    /// Whether objects of this kind are ever imported as numeric maps /
    /// constants. Only these count towards a strict-mode failure.
    pub fn is_map_like(self) -> bool {
        matches!(
            self,
            ObjectKind::Table | ObjectKind::Function | ObjectKind::Constant
        )
    }
}

impl fmt::Display for ObjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Stable issue-kind ids. Grouped so a corpus run can rank them.
pub mod kinds {
    // --- scaling ---
    /// Cell equation is non-linear / unparsable; imported pre-scaled (factor 1).
    pub const MATH_NONLINEAR: &str = "math-nonlinear";
    /// Cell equation references a variable other than `X` (another address, a
    /// linked object); imported pre-scaled.
    pub const MATH_FOREIGN_VAR: &str = "math-foreign-var";
    /// Axis equation is non-linear / references other variables; the stored
    /// axis is kept raw (factor 1).
    pub const AXIS_MATH_NONLINEAR: &str = "axis-math-nonlinear";
    // --- axes ---
    /// Axis breakpoints are declared but could not be resolved (embedded axis
    /// at address 0, unresolvable link, unknown source); kept as a count only.
    pub const AXIS_UNRESOLVED: &str = "axis-unresolved";
    /// Categorical (text) axis labels dropped; the numeric axis model can't
    /// hold them.
    pub const AXIS_CATEGORICAL: &str = "axis-categorical";
    /// Axis element count disagrees with the table dimension.
    pub const AXIS_COUNT_MISMATCH: &str = "axis-count-mismatch";
    /// Stored axis storage is unusable (width, strides, address); its address
    /// was dropped.
    pub const AXIS_STORAGE: &str = "axis-storage";
    // --- storage ---
    /// Byte order not given per object; an ambiguous file default was used.
    pub const ENDIAN_DEFAULTED: &str = "endian-defaulted";
    /// Element size not given per object; a file default was used.
    pub const ELEMENT_SIZE_DEFAULTED: &str = "element-size-defaulted";
    /// Element size missing with no default (refusal).
    pub const ELEMENT_SIZE_MISSING: &str = "element-size-missing";
    /// Element width OpenTune cannot store (24-bit, 64-bit, odd sizes).
    pub const ELEMENT_SIZE_UNSUPPORTED: &str = "element-size-unsupported";
    /// Unknown storage type name (EcuFlash/RomRaider `storagetype`).
    pub const STORAGE_UNKNOWN: &str = "storage-unknown";
    /// `mmedtypeflags` bits this importer does not understand.
    pub const TYPEFLAGS_UNKNOWN: &str = "typeflags-unknown";
    /// Non-contiguous / interleaved cell layout (strides, FastECU `interval`).
    pub const STRIDE_NONCONTIGUOUS: &str = "stride-noncontiguous";
    /// Reversed (negative-stride) cell layout.
    pub const STRIDE_REVERSED: &str = "stride-reversed";
    /// Float byte order overridden to the tool's convention.
    pub const FLOAT_ENDIAN: &str = "float-endian";
    // --- addressing ---
    /// No (valid) data address.
    pub const ADDRESS_MISSING: &str = "address-missing";
    /// Base-offset rebasing under/overflowed.
    pub const ADDRESS_REBASE: &str = "address-rebase";
    /// Cells extend past the declared image/region size.
    pub const ADDRESS_OUTSIDE_IMAGE: &str = "address-outside-image";
    /// A tool-specific address fold was applied (FastECU `wrx02`).
    pub const ADDRESS_FOLDED: &str = "address-folded";
    // --- definitions ---
    /// A referenced scaling / base definition was not found.
    pub const REFERENCE_MISSING: &str = "reference-missing";
    /// Legacy text XDF: cell storage type is not recorded by the format.
    pub const LEGACY_CELL_TYPE: &str = "legacy-cell-type";
    /// Signedness not recorded by the source format.
    pub const SIGNEDNESS_UNKNOWN: &str = "signedness-unknown";
    /// Object class OpenTune does not import as a map (flag, patch, checksum,
    /// switch/selection).
    pub const UNSUPPORTED_OBJECT: &str = "unsupported-object";
}

/// One fidelity issue on one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// Stable kind id (see [`kinds`]).
    pub kind: &'static str,
    /// Human-readable detail.
    pub message: String,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.kind, self.message)
    }
}

/// The outcome of importing one definition object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectReport {
    /// Object name (title), as it appears in the source.
    pub name: String,
    /// What kind of object it is.
    pub object: ObjectKind,
    /// Final status.
    pub status: Status,
    /// Warnings (status `Warned`) or the refusal reason(s) (status `Refused`;
    /// the last issue is the refusal, earlier ones are warnings seen first).
    pub issues: Vec<Issue>,
}

impl ObjectReport {
    /// One-line description: `table "Name": [kind] msg; [kind] msg`.
    pub fn describe(&self) -> String {
        let issues: Vec<String> = self.issues.iter().map(|i| i.to_string()).collect();
        format!(
            "{} \"{}\" {}: {}",
            self.object,
            self.name,
            self.status,
            issues.join("; ")
        )
    }
}

/// Collects the issues of one object while it is being imported.
#[derive(Debug, Default, Clone)]
pub struct Tracker {
    warnings: Vec<Issue>,
    refusal: Option<Issue>,
}

impl Tracker {
    /// A fresh tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a warning (the object is still imported).
    pub fn warn(&mut self, kind: &'static str, message: impl Into<String>) {
        self.warnings.push(Issue {
            kind,
            message: message.into(),
        });
    }

    /// Record a refusal (the object is not imported). The first refusal wins.
    pub fn refuse(&mut self, kind: &'static str, message: impl Into<String>) {
        if self.refusal.is_none() {
            self.refusal = Some(Issue {
                kind,
                message: message.into(),
            });
        }
    }

    /// Whether a refusal has been recorded.
    pub fn is_refused(&self) -> bool {
        self.refusal.is_some()
    }

    /// The warnings so far.
    pub fn warnings(&self) -> &[Issue] {
        &self.warnings
    }

    /// The warnings rendered for a map's `notes` (`WARN[kind] message`).
    pub fn note_lines(&self) -> Vec<String> {
        self.warnings
            .iter()
            .map(|i| format!("WARN[{}] {}", i.kind, i.message))
            .collect()
    }

    /// Finish into a report.
    pub fn finish(self, name: impl Into<String>, object: ObjectKind) -> ObjectReport {
        let status = if self.refusal.is_some() {
            Status::Refused
        } else if self.warnings.is_empty() {
            Status::Clean
        } else {
            Status::Warned
        };
        let mut issues = self.warnings;
        issues.extend(self.refusal);
        ObjectReport {
            name: name.into(),
            object,
            status,
            issues,
        }
    }
}

/// Counts over a set of reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// Map-like objects imported clean.
    pub clean: usize,
    /// Map-like objects imported with warnings.
    pub warned: usize,
    /// Map-like objects refused.
    pub refused: usize,
    /// Out-of-scope objects (flags, patches, checksums, switches,
    /// placeholders), all refused with a reason.
    pub out_of_scope: usize,
    /// Issue-kind histogram over map-like objects (warnings and refusals).
    pub kinds: BTreeMap<&'static str, usize>,
}

impl Summary {
    /// Add the counts of another summary into this one.
    pub fn absorb(&mut self, other: &Summary) {
        self.clean += other.clean;
        self.warned += other.warned;
        self.refused += other.refused;
        self.out_of_scope += other.out_of_scope;
        for (k, v) in &other.kinds {
            *self.kinds.entry(k).or_default() += v;
        }
    }

    /// Issue kinds, most frequent first.
    pub fn top_kinds(&self) -> Vec<(&'static str, usize)> {
        let mut v: Vec<_> = self.kinds.iter().map(|(k, n)| (*k, *n)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        v
    }
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} clean, {} warned, {} refused, {} out-of-scope",
            self.clean, self.warned, self.refused, self.out_of_scope
        )
    }
}

/// Summarize a set of reports.
pub fn summarize(reports: &[ObjectReport]) -> Summary {
    let mut s = Summary::default();
    for r in reports {
        if !r.object.is_map_like() {
            s.out_of_scope += 1;
            continue;
        }
        match r.status {
            Status::Clean => s.clean += 1,
            Status::Warned => s.warned += 1,
            Status::Refused => s.refused += 1,
        }
        for i in &r.issues {
            *s.kinds.entry(i.kind).or_default() += 1;
        }
    }
    s
}

/// The reasons a strict import must fail: every map-like object that is not
/// clean, described on one line each. Empty means a strict import passes.
pub fn strict_failures(reports: &[ObjectReport]) -> Vec<String> {
    reports
        .iter()
        .filter(|r| r.object.is_map_like() && r.status != Status::Clean)
        .map(ObjectReport::describe)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_status_progression() {
        let t = Tracker::new();
        assert_eq!(t.finish("a", ObjectKind::Table).status, Status::Clean);

        let mut t = Tracker::new();
        t.warn(kinds::MATH_NONLINEAR, "x");
        let r = t.finish("b", ObjectKind::Table);
        assert_eq!(r.status, Status::Warned);
        assert_eq!(r.issues.len(), 1);

        let mut t = Tracker::new();
        t.warn(kinds::MATH_NONLINEAR, "x");
        t.refuse(kinds::STRIDE_REVERSED, "first");
        t.refuse(kinds::ADDRESS_MISSING, "second");
        let r = t.finish("c", ObjectKind::Table);
        assert_eq!(r.status, Status::Refused);
        assert_eq!(r.issues.last().unwrap().kind, kinds::STRIDE_REVERSED);
    }

    #[test]
    fn strict_ignores_out_of_scope_objects() {
        let mut flag = Tracker::new();
        flag.refuse(kinds::UNSUPPORTED_OBJECT, "bit flag");
        let reports = vec![
            Tracker::new().finish("ok", ObjectKind::Table),
            flag.finish("f", ObjectKind::Flag),
        ];
        assert!(strict_failures(&reports).is_empty());
        let s = summarize(&reports);
        assert_eq!((s.clean, s.out_of_scope), (1, 1));

        let mut w = Tracker::new();
        w.warn(kinds::AXIS_UNRESOLVED, "no breakpoints");
        let reports = vec![w.finish("w", ObjectKind::Constant)];
        let f = strict_failures(&reports);
        assert_eq!(f.len(), 1);
        assert!(f[0].contains("axis-unresolved"), "{f:?}");
    }
}
