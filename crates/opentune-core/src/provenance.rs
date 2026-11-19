//! Provenance metadata for contributed definition data.
//!
//! Every ECU definition (`ecudefs/*.json`, optional `provenance` block) and
//! every definition pack (`pack.json`: `sources`, `license`, `evidence`) says
//! **where its facts come from**, **under which licence** the data is shipped
//! and **how far it has been validated**. This module holds the shared shape
//! and the checks; the rules themselves are documented in `CONTRIBUTING.md` and
//! `docs/CONTRIBUTING-ECUDEFS.md`.
//!
//! The evidence vocabulary is the project's validation-state vocabulary
//! (`docs/DEVELOPMENT.md` §7), lowest to highest: `example` (placeholder data
//! that never identifies a real unit), `implemented`, `synthetic-tested`, `real-image-tested`, `bench-tested`, `vehicle-tested`.
//! It records the highest state the data as a whole has reached — per-map
//! confidence stays in each map's `CONFIDENCE:` label.
//!
//! These checks are mechanical: they make a reviewer's job easier, they do not
//! establish that a licence is valid or a source is genuine.

use crate::validate::ValidationIssue;
use serde::{Deserialize, Serialize};

/// The validation-state vocabulary (`docs/DEVELOPMENT.md` §7), in increasing
/// order of evidence. The only accepted values of an `evidence` field.
pub const EVIDENCE_LEVELS: [&str; 6] = [
    "example",
    "implemented",
    "synthetic-tested",
    "real-image-tested",
    "bench-tested",
    "vehicle-tested",
];

/// SPDX identifiers accepted without comment for data shipped in this
/// GPL-3.0-or-later repository. Anything else is a warning for the reviewer,
/// not an error: a licence can be compatible without being on this list.
const KNOWN_COMPATIBLE: &[&str] = &[
    "GPL-2.0-or-later",
    "GPL-3.0-only",
    "GPL-3.0-or-later",
    "LGPL-2.1-or-later",
    "LGPL-3.0-only",
    "LGPL-3.0-or-later",
    "MIT",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "Apache-2.0",
    "ISC",
    "Zlib",
    "0BSD",
    "Unlicense",
    "CC0-1.0",
    "CC-BY-4.0",
    "CC-BY-SA-4.0",
    "MPL-2.0",
    "BSL-1.0",
];

/// Licence texts that can never cover data in this repository.
const FORBIDDEN_LICENSE_PHRASES: &[&str] = &[
    "proprietary",
    "all rights reserved",
    "confidential",
    "non-commercial",
    "noncommercial",
    "not for distribution",
    "internal use",
];

/// Licence tokens that mean "no licence" (or a non-commercial one).
const FORBIDDEN_LICENSE_TOKENS: &[&str] = &["unknown", "none", "n/a", "tbd", "?", "nda"];

/// Output of commercial / OEM tools is never an acceptable source (their
/// definitions and label data are copyrighted or encrypted). Matched
/// case-insensitively inside each `sources` entry.
const FORBIDDEN_SOURCES: &[&str] = &[
    "winols",
    "ecm titanium",
    "mpps",
    "kess",
    "ktag",
    "k-tag",
    "alientech",
    "autotuner",
    "magicmotorsport",
    "pcmflash",
    "damos",
    "odis",
    ".clb",
];

/// Where a definition's facts come from, the licence its data is shipped
/// under, and how far it has been validated.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// One citation per source: a standard by name, an open project with its
    /// path/commit and licence, or "own read" / "own capture" for the
    /// contributor's own data (which itself is never committed).
    #[serde(default)]
    pub sources: Vec<String>,
    /// SPDX licence expression of the definition data, e.g. `GPL-3.0-or-later`.
    #[serde(default)]
    pub license: String,
    /// One of [`EVIDENCE_LEVELS`].
    #[serde(default)]
    pub evidence: String,
}

impl Provenance {
    /// Check this block (see [`check`]).
    pub fn check(&self, location: &str) -> Vec<ValidationIssue> {
        check(&self.sources, &self.license, &self.evidence, location)
    }
}

/// The level of placeholder/template data: an `example` definition is never
/// used to identify a real unit.
pub const EXAMPLE: &str = "example";

/// Is `s` one of [`EVIDENCE_LEVELS`]?
pub fn is_evidence_level(s: &str) -> bool {
    EVIDENCE_LEVELS.contains(&s)
}

/// Check provenance fields (shared by ECU definitions and pack manifests).
/// `location` prefixes each issue's location (e.g. `"provenance"`).
///
/// Errors: missing/blank sources, a commercial-tool source, missing or
/// forbidden licence, missing or unknown evidence level. Warning: a licence
/// identifier that is not on the known GPL-3-compatible list.
pub fn check(
    sources: &[String],
    license: &str,
    evidence: &str,
    location: &str,
) -> Vec<ValidationIssue> {
    let mut out = Vec::new();
    let at = |field: &str| {
        if location.is_empty() {
            field.to_string()
        } else {
            format!("{location}.{field}")
        }
    };

    if sources.is_empty() {
        out.push(ValidationIssue::error(
            "provenance.sources_missing",
            at("sources"),
            "no sources: cite where every fact comes from (a standard, an open project with its licence, or your own read/capture)",
        ));
    }
    for (i, s) in sources.iter().enumerate() {
        let lower = s.to_ascii_lowercase();
        if s.trim().is_empty() {
            out.push(ValidationIssue::error(
                "provenance.source_empty",
                at(&format!("sources[{i}]")),
                "empty source entry",
            ));
        } else if let Some(bad) = FORBIDDEN_SOURCES.iter().find(|t| contains_word(&lower, t)) {
            out.push(ValidationIssue::error(
                "provenance.source_commercial",
                at(&format!("sources[{i}]")),
                format!(
                    "source mentions '{bad}': definitions or labels taken from commercial / OEM tools are not accepted (see CONTRIBUTING.md)"
                ),
            ));
        }
    }

    check_license(license, &at("license"), &mut out);

    let ev = evidence.trim();
    if ev.is_empty() {
        out.push(ValidationIssue::error(
            "provenance.evidence_missing",
            at("evidence"),
            format!(
                "no evidence level: use one of {} (docs/DEVELOPMENT.md §7)",
                EVIDENCE_LEVELS.join(", ")
            ),
        ));
    } else if !is_evidence_level(ev) {
        out.push(ValidationIssue::error(
            "provenance.evidence_unknown",
            at("evidence"),
            format!(
                "evidence '{ev}' is not a validation state; use one of {}",
                EVIDENCE_LEVELS.join(", ")
            ),
        ));
    }
    out
}

/// `needle` occurs in `hay` with no ASCII alphanumeric directly before or after
/// it (so "kess" does not hit "kessler"). The boundary is only checked on a
/// side where the needle itself ends in an alphanumeric: `.clb` must match in
/// `codes.clb`, where the dot already separates it from the file name.
fn contains_word(hay: &str, needle: &str) -> bool {
    let b = hay.as_bytes();
    let n = needle.as_bytes();
    let check_before = n.first().is_some_and(|c| c.is_ascii_alphanumeric());
    let check_after = n.last().is_some_and(|c| c.is_ascii_alphanumeric());
    hay.match_indices(needle).any(|(i, m)| {
        let before = i.checked_sub(1).map(|j| b[j]);
        let after = b.get(i + m.len()).copied();
        !(check_before && before.is_some_and(|c| c.is_ascii_alphanumeric()))
            && !(check_after && after.is_some_and(|c| c.is_ascii_alphanumeric()))
    })
}

fn check_license(license: &str, loc: &str, out: &mut Vec<ValidationIssue>) {
    let lic = license.trim();
    if lic.is_empty() {
        out.push(ValidationIssue::error(
            "provenance.license_missing",
            loc,
            "no licence: give the SPDX id the data is shipped under (normally GPL-3.0-or-later)",
        ));
        return;
    }
    let lower = lic.to_ascii_lowercase();
    let tokens: Vec<&str> = lic
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',')
        .filter(|t| !t.is_empty())
        .collect();
    let forbidden_phrase = FORBIDDEN_LICENSE_PHRASES
        .iter()
        .find(|p| lower.contains(*p));
    let forbidden_token = tokens.iter().find(|t| {
        let t = t.to_ascii_lowercase();
        FORBIDDEN_LICENSE_TOKENS.contains(&t.as_str())
            // Any NC component: `CC-BY-NC-4.0`, but also `CC BY-NC 4.0`.
            || t.split('-').any(|p| p == "nc")
    });
    if let Some(bad) = forbidden_phrase.copied().or(forbidden_token.copied()) {
        out.push(ValidationIssue::error(
            "provenance.license_forbidden",
            loc,
            format!(
                "licence '{lic}' ('{bad}') cannot cover data in this GPL-3.0-or-later repository: only openly licensed, GPL-compatible data is accepted"
            ),
        ));
        return;
    }
    // Walk the SPDX expression: ids joined by AND / OR, exceptions after WITH.
    let mut skip_next = false;
    let mut unknown = Vec::new();
    for t in tokens {
        if skip_next {
            skip_next = false;
            continue;
        }
        match t {
            "AND" | "OR" => {}
            "WITH" => skip_next = true,
            id if KNOWN_COMPATIBLE.contains(&id) => {}
            id => unknown.push(id.to_string()),
        }
    }
    if !unknown.is_empty() {
        out.push(ValidationIssue::warn(
            "provenance.license_unrecognised",
            loc,
            format!(
                "licence id(s) {unknown:?} are not on the known GPL-3-compatible SPDX list; a reviewer must confirm compatibility"
            ),
        ));
    }
}

/// Promote advisory "no provenance at all" findings (code `provenance.missing`)
/// to errors. Used by `opentune ecudefs validate --require-provenance`, the CI
/// gate: every shipped and every new definition must carry the block, while
/// older third-party files still load (and only warn) elsewhere.
pub fn require(issues: &mut [ValidationIssue]) {
    for i in issues.iter_mut() {
        if i.code == "provenance.missing" {
            i.severity = crate::validate::Severity::Error;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::{has_errors, Severity};

    fn codes(v: &[ValidationIssue]) -> Vec<&str> {
        v.iter().map(|i| i.code).collect()
    }

    fn ok_sources() -> Vec<String> {
        vec!["ISO 14229-1 Annex C".into()]
    }

    #[test]
    fn complete_block_is_clean() {
        let p = Provenance {
            sources: ok_sources(),
            license: "GPL-3.0-or-later".into(),
            evidence: "synthetic-tested".into(),
        };
        assert!(p.check("provenance").is_empty());
    }

    #[test]
    fn every_field_is_required() {
        let v = check(&[], "", "", "provenance");
        let c = codes(&v);
        assert!(c.contains(&"provenance.sources_missing"));
        assert!(c.contains(&"provenance.license_missing"));
        assert!(c.contains(&"provenance.evidence_missing"));
        assert!(v.iter().all(|i| i.severity == Severity::Error));
        assert_eq!(v[0].location, "provenance.sources");
    }

    #[test]
    fn evidence_must_use_the_vocabulary() {
        let v = check(&ok_sources(), "MIT", "HIGH", "");
        assert_eq!(codes(&v), vec!["provenance.evidence_unknown"]);
        for lvl in EVIDENCE_LEVELS {
            assert!(check(&ok_sources(), "MIT", lvl, "").is_empty(), "{lvl}");
        }
    }

    #[test]
    fn forbidden_licences_error() {
        for lic in [
            "Proprietary",
            "All rights reserved",
            "unknown",
            "CC-BY-NC-4.0",
            "CC-BY-NC-SA-4.0",
            "MIT AND NDA",
            // Regression: written with spaces the NC part is its own token
            // without the `cc-` prefix, and used to pass as a mere warning.
            "CC BY-NC 4.0",
            "CC BY-NC-SA 4.0",
            "CC BY NC",
        ] {
            let v = check(&ok_sources(), lic, "implemented", "");
            assert_eq!(codes(&v), vec!["provenance.license_forbidden"], "{lic}");
        }
    }

    #[test]
    fn spdx_expressions_are_walked() {
        for lic in [
            "GPL-3.0-or-later",
            "MIT AND BSD-2-Clause",
            "(MIT OR Apache-2.0) AND GPL-3.0-or-later",
            "GPL-3.0-or-later WITH Some-exception-1.0",
        ] {
            assert!(
                check(&ok_sources(), lic, "implemented", "").is_empty(),
                "{lic}"
            );
        }
        // An unlisted id is a warning for the reviewer, not an error.
        let v = check(&ok_sources(), "EUPL-1.2", "implemented", "");
        assert_eq!(codes(&v), vec!["provenance.license_unrecognised"]);
        assert!(!has_errors(&v));
    }

    #[test]
    fn commercial_tool_sources_error() {
        let v = check(
            &["exported from WinOLS project".into()],
            "GPL-3.0-or-later",
            "implemented",
            "",
        );
        assert_eq!(codes(&v), vec!["provenance.source_commercial"]);
        // Whole words only: a surname or a longer word is not a tool name.
        let v = check(
            &["periodisch capture by A. Kessler (own read)".into()],
            "MIT",
            "implemented",
            "",
        );
        assert!(v.is_empty(), "{v:?}");
        let v = check(&["  ".into()], "MIT", "implemented", "");
        assert_eq!(codes(&v), vec!["provenance.source_empty"]);
    }

    #[test]
    fn a_label_file_extension_matches_after_a_file_name() {
        // Regression: `.clb` starts with a dot, so the old whole-word check
        // demanded a non-alphanumeric before the dot and never matched a
        // real file name.
        for src in ["labels from codes.clb", "see 00X000000.CLB", "a .clb file"] {
            let v = check(&[src.into()], "MIT", "implemented", "");
            assert_eq!(codes(&v), vec!["provenance.source_commercial"], "{src}");
        }
        // Still a whole word on the alphanumeric side.
        assert!(check(&["notes.clbx".into()], "MIT", "implemented", "").is_empty());
    }

    #[test]
    fn require_promotes_only_missing_blocks() {
        let mut v = vec![
            ValidationIssue::warn("provenance.missing", "", "x"),
            ValidationIssue::warn("provenance.license_unrecognised", "", "y"),
        ];
        require(&mut v);
        assert_eq!(v[0].severity, Severity::Error);
        assert_eq!(v[1].severity, Severity::Warn);
    }
}
