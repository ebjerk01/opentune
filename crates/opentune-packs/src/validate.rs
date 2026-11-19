//! Contributor-facing checks for a definition pack (`opentune packs validate`).
//!
//! Loading a pack ([`crate::load_pack`]) is deliberately lenient: a bad file is
//! noted and skipped, and a manifest needs only `id` and `name`. Validation is
//! the strict gate a contribution must pass (CI runs it on `packs/`):
//!
//! - the manifest has an id matching its folder, a name, a version, and
//!   provenance: `sources`, `license` and `evidence` (shared rules in
//!   [`opentune_core::provenance`]);
//! - every definition file loads and validates (each load note is an error);
//! - sub-directories the loader does not know are reported (not loaded);
//! - every text file in the pack passes the privacy scan
//!   ([`opentune_core::privacy`]): no VIN-like identifiers, e-mail addresses
//!   or serial-number-like values.

use std::path::{Path, PathBuf};

use opentune_core::validate::{Severity, ValidationIssue};
use opentune_core::{privacy, provenance};

use crate::{discover, load_pack, Kind, PackManifest};

/// File extensions the privacy scan reads inside a pack.
const TEXT_EXTENSIONS: &[&str] = &["json", "md", "txt", "csv"];

/// The result of validating one pack directory.
#[derive(Debug, Clone)]
pub struct PackValidation {
    /// The pack directory.
    pub dir: PathBuf,
    /// The manifest id (empty if the manifest did not load).
    pub id: String,
    /// Every finding; locations are pack-relative paths.
    pub issues: Vec<ValidationIssue>,
}

impl PackValidation {
    /// Number of errors (a contribution must have none).
    pub fn errors(&self) -> usize {
        opentune_core::validate::error_count(&self.issues)
    }
    /// Number of warnings (advisory).
    pub fn warnings(&self) -> usize {
        opentune_core::validate::warn_count(&self.issues)
    }
}

impl PackManifest {
    /// Check the manifest for contribution: identity fields and provenance.
    /// `dir_name` is the pack folder's name, which the id must equal.
    pub fn validate(&self, dir_name: Option<&str>) -> Vec<ValidationIssue> {
        let mut out = Vec::new();
        let id = self.id.trim();
        if id.is_empty() {
            out.push(ValidationIssue::error(
                "pack.id_empty",
                "pack.json",
                "manifest 'id' is empty",
            ));
        } else if !id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            || id.starts_with('-')
            || id.ends_with('-')
        {
            out.push(ValidationIssue::error(
                "pack.id_charset",
                "pack.json",
                format!("pack id '{id}' must be lowercase a-z, 0-9 and '-' only"),
            ));
        }
        if let Some(dir) = dir_name {
            if !id.is_empty() && dir != id {
                out.push(ValidationIssue::error(
                    "pack.id_dir_mismatch",
                    "pack.json",
                    format!("pack id '{id}' does not match its folder '{dir}' (rename one so they agree)"),
                ));
            }
        }
        if self.name.trim().is_empty() {
            out.push(ValidationIssue::error(
                "pack.name_empty",
                "pack.json",
                "manifest 'name' is empty",
            ));
        }
        let v = self.version.trim();
        if v.is_empty() {
            out.push(ValidationIssue::error(
                "pack.version_missing",
                "pack.json",
                "manifest 'version' is empty (use semver, e.g. 0.1.0)",
            ));
        } else if !is_semver_like(v) {
            out.push(ValidationIssue::warn(
                "pack.version_format",
                "pack.json",
                format!("version '{v}' is not MAJOR.MINOR.PATCH"),
            ));
        }
        out.extend(provenance::check(
            &self.sources,
            &self.license,
            &self.evidence,
            "pack.json",
        ));
        out
    }
}

fn is_semver_like(v: &str) -> bool {
    let core = v.split(['-', '+']).next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Validate one pack directory. `allow` lists exact values the privacy scan
/// should ignore (deliberate synthetic data).
pub fn validate_pack(dir: &Path, allow: &[String]) -> PackValidation {
    let mut v = PackValidation {
        dir: dir.to_path_buf(),
        id: String::new(),
        issues: Vec::new(),
    };
    let dir_name = dir
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    match load_pack(dir) {
        Err(e) => v.issues.push(ValidationIssue::error(
            "pack.manifest",
            "pack.json",
            e.to_string(),
        )),
        Ok(pack) => {
            v.id = pack.manifest.id.clone();
            v.issues.extend(pack.manifest.validate(dir_name.as_deref()));
            for n in &pack.report.notes {
                v.issues.push(ValidationIssue::error(
                    "pack.load",
                    n.file.replace('\\', "/"),
                    n.message.clone(),
                ));
            }
            if pack.report.loaded == 0 && pack.report.notes.is_empty() {
                v.issues.push(ValidationIssue::warn(
                    "pack.empty",
                    "",
                    "the pack carries no definition files",
                ));
            }
        }
    }
    check_layout(dir, &mut v.issues);
    privacy_scan_dir(dir, dir, allow, &mut v.issues);
    v
}

/// Discover every pack under `root` (or `root` itself) and validate each.
pub fn validate_all(root: &Path, allow: &[String]) -> Vec<PackValidation> {
    discover(root)
        .iter()
        .map(|d| validate_pack(d, allow))
        .collect()
}

/// Report sub-directories the loader ignores (a misspelt `service/` would
/// otherwise silently ship nothing).
fn check_layout(dir: &Path, out: &mut Vec<ValidationIssue>) {
    let known: Vec<&str> = Kind::all().iter().map(|k| k.dir()).collect();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subs: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('_') && !n.starts_with('.'))
        .filter(|n| !known.contains(&n.as_str()))
        .collect();
    subs.sort();
    for s in subs {
        out.push(ValidationIssue::warn(
            "pack.unknown_dir",
            format!("{s}/"),
            format!(
                "sub-directory is not loaded; definition folders are: {}",
                known.join(", ")
            ),
        ));
    }
}

fn privacy_scan_dir(root: &Path, dir: &Path, allow: &[String], out: &mut Vec<ValidationIssue>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            privacy_scan_dir(root, &p, allow, out);
            continue;
        }
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        if !ext.is_some_and(|e| TEXT_EXTENSIONS.contains(&e.as_str())) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let rel = p
            .strip_prefix(root)
            .unwrap_or(&p)
            .to_string_lossy()
            .replace('\\', "/");
        for f in privacy::scan(&text, allow) {
            out.push(ValidationIssue {
                severity: Severity::Error,
                code: f.kind.code(),
                location: format!("{rel}:{}:{}", f.line, f.column),
                message: f.message(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_pack(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir()
            .join(format!("opentune-packval-{tag}-{}-{n}", std::process::id()))
            .join(tag);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::File::create(path)
            .unwrap()
            .write_all(content.as_bytes())
            .unwrap();
    }

    fn codes(v: &PackValidation) -> Vec<&str> {
        v.issues.iter().map(|i| i.code).collect()
    }

    const GOOD: &str = r#"{"id":"good-pack","name":"Good","version":"1.0.0",
        "license":"GPL-3.0-or-later","evidence":"synthetic-tested",
        "sources":["ISO 15765-4 (11-bit OBD ids)"]}"#;

    #[test]
    fn complete_pack_is_clean() {
        let d = temp_pack("good-pack");
        write(&d, "pack.json", GOOD);
        write(&d, "modules/m.json", r#"{"vehicle":"Any","modules":[]}"#);
        let v = validate_pack(&d, &[]);
        assert_eq!(v.errors(), 0, "{:?}", v.issues);
        assert_eq!(v.id, "good-pack");
        std::fs::remove_dir_all(d.parent().unwrap()).ok();
    }

    #[test]
    fn manifest_needs_provenance_and_matching_id() {
        let d = temp_pack("other-name");
        write(&d, "pack.json", r#"{"id":"Bad_Id","name":"X"}"#);
        let v = validate_pack(&d, &[]);
        let c = codes(&v);
        for want in [
            "pack.id_charset",
            "pack.id_dir_mismatch",
            "pack.version_missing",
            "provenance.sources_missing",
            "provenance.license_missing",
            "provenance.evidence_missing",
            "pack.empty",
        ] {
            assert!(c.contains(&want), "{want} not in {c:?}");
        }
        std::fs::remove_dir_all(d.parent().unwrap()).ok();
    }

    #[test]
    fn load_notes_unknown_dirs_and_privacy_findings_are_reported() {
        let d = temp_pack("good-pack");
        write(&d, "pack.json", GOOD);
        write(&d, "coding/broken.json", r#"{"id":"x"}"#);
        write(&d, "service/typo.json", "{}");
        let vin = ["WVW", "ZZZ", "3CZ", "9E2", "58317"].concat();
        write(
            &d,
            "README.md",
            &format!("Captured from {vin}.\nAsk jane@example.com\n"),
        );
        let v = validate_pack(&d, &[]);
        let load = v.issues.iter().find(|i| i.code == "pack.load").unwrap();
        assert_eq!(load.location, "coding/broken.json");
        let dir = v
            .issues
            .iter()
            .find(|i| i.code == "pack.unknown_dir")
            .unwrap();
        assert_eq!(
            (dir.severity, dir.location.as_str()),
            (Severity::Warn, "service/")
        );
        let p: Vec<_> = v
            .issues
            .iter()
            .filter(|i| i.code.starts_with("privacy."))
            .collect();
        assert_eq!(p.len(), 1, "reserved example domain is not flagged: {p:?}");
        assert_eq!(p[0].code, "privacy.vin");
        assert_eq!(p[0].location, "README.md:1:15");
        assert!(!p[0].message.contains(&vin), "finding is redacted");
        // An allow-listed synthetic value is accepted.
        let v = validate_pack(&d, &[vin]);
        assert!(!codes(&v).iter().any(|c| c.starts_with("privacy.")));
        std::fs::remove_dir_all(d.parent().unwrap()).ok();
    }

    #[test]
    fn semver_shape() {
        assert!(is_semver_like("1.2.3") && is_semver_like("0.1.0-pre.1"));
        assert!(!is_semver_like("1.2") && !is_semver_like("v1.2.3"));
    }
}
