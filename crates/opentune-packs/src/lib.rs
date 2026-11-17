//! # opentune-packs
//!
//! A **definition pack** is one versioned, loadable bundle of OpenTune's
//! data-defined knowledge for a vehicle/platform: ECU definitions, coding-label
//! sets, adaptation-channel maps, service-function scripts, logger definitions
//! and module lists. It is how OpenTune's coverage grows by *data* rather than
//! code — the app (or a community contributor) ships a pack, and every service
//! engine (`opentune-coding`, `-adaptation`, `-servicefn`, `-logdef`,
//! `-autoscan`) picks up what it needs.
//!
//! ## Layout
//!
//! A pack is a directory containing a `pack.json` manifest and conventional
//! sub-directories of `*.json` definition files:
//!
//! ```text
//!   my-pack/
//!     pack.json           # PackManifest (id, name, version, vehicles, license…)
//!     ecudefs/*.json      # opentune_core::ecudef::EcuDef
//!     coding/*.json       # opentune_coding::CodingDef
//!     adaptation/*.json   # opentune_adaptation::AdaptationDef
//!     services/*.json     # opentune_servicefn::ServiceScript
//!     loggers/*.json      # opentune_logdef::LogDef
//!     modules/*.json      # opentune_autoscan::VehicleModules
//! ```
//!
//! Loading is **honest**: a file that fails to parse or validate is recorded in
//! the [`LoadReport`] (with the reason) and skipped — it never aborts the whole
//! pack or silently vanishes. [`load_pack`] errors only when the manifest itself
//! is missing or unparseable. Definitions that carry a `validate()` (coding,
//! adaptation, service scripts, ECU defs) are validated on load.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use opentune_adaptation::AdaptationDef;
use opentune_autoscan::VehicleModules;
use opentune_coding::CodingDef;
use opentune_core::ecudef::{is_scaffolding_file, EcuDef};
use opentune_core::validate::has_errors;
use opentune_logdef::LogDef;
use opentune_servicefn::ServiceScript;

/// The pack manifest (`pack.json`): identity + provenance metadata. Definitions
/// themselves live in the conventional sub-directories (see the crate docs);
/// unknown fields are ignored for forward compatibility.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackManifest {
    /// Stable pack id, e.g. `"vag-mqb-community"`.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Pack version (semver-style string, e.g. `"1.2.0"`).
    #[serde(default)]
    pub version: String,
    /// Vehicle maker(s) this pack targets.
    #[serde(default)]
    pub maker: String,
    /// Platform / generation, e.g. `"MQB"`, `"E46"`.
    #[serde(default)]
    pub platform: String,
    /// Specific vehicles/models covered.
    #[serde(default)]
    pub vehicles: Vec<String>,
    /// Who authored/curated the pack.
    #[serde(default)]
    pub author: String,
    /// License of the pack's definition data (e.g. `"GPL-3.0-or-later"`).
    #[serde(default)]
    pub license: String,
    /// Free-text description / provenance note.
    #[serde(default)]
    pub description: String,
}

impl PackManifest {
    /// Parse a manifest from JSON.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

/// The kinds of definition a pack can carry (and their sub-directory name).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `ecudefs/` — [`EcuDef`].
    EcuDef,
    /// `coding/` — [`CodingDef`].
    Coding,
    /// `adaptation/` — [`AdaptationDef`].
    Adaptation,
    /// `services/` — [`ServiceScript`].
    Service,
    /// `loggers/` — [`LogDef`].
    Logger,
    /// `modules/` — [`VehicleModules`].
    Modules,
}

impl Kind {
    /// The pack sub-directory this kind lives in.
    pub fn dir(self) -> &'static str {
        match self {
            Kind::EcuDef => "ecudefs",
            Kind::Coding => "coding",
            Kind::Adaptation => "adaptation",
            Kind::Service => "services",
            Kind::Logger => "loggers",
            Kind::Modules => "modules",
        }
    }
    /// All kinds, in load order.
    pub fn all() -> [Kind; 6] {
        [
            Kind::EcuDef,
            Kind::Coding,
            Kind::Adaptation,
            Kind::Service,
            Kind::Logger,
            Kind::Modules,
        ]
    }
}

/// A per-file note from loading a pack (parse error, validation issue, skip).
#[derive(Debug, Clone)]
pub struct LoadNote {
    /// Pack-relative path of the file.
    pub file: String,
    /// What kind the file was being loaded as.
    pub kind: Kind,
    /// The problem.
    pub message: String,
}

/// Summary of a pack load.
#[derive(Debug, Clone, Default)]
pub struct LoadReport {
    /// Number of definition files successfully loaded (and validated).
    pub loaded: usize,
    /// Files skipped because they failed to parse or validate, with the reason.
    pub notes: Vec<LoadNote>,
}

impl LoadReport {
    /// Whether every discovered file loaded cleanly.
    pub fn is_clean(&self) -> bool {
        self.notes.is_empty()
    }
}

/// A loaded definition pack.
#[derive(Debug, Clone)]
pub struct Pack {
    /// The manifest.
    pub manifest: PackManifest,
    /// The pack's root directory.
    pub root: PathBuf,
    /// ECU definitions.
    pub ecudefs: Vec<EcuDef>,
    /// Coding-label sets.
    pub coding: Vec<CodingDef>,
    /// Adaptation-channel maps.
    pub adaptation: Vec<AdaptationDef>,
    /// Service-function scripts.
    pub services: Vec<ServiceScript>,
    /// Logger definitions.
    pub loggers: Vec<LogDef>,
    /// Module lists (for auto-scan).
    pub modules: Vec<VehicleModules>,
    /// Load diagnostics.
    pub report: LoadReport,
}

/// Error opening a pack (the manifest is what makes a directory a pack).
#[derive(Debug)]
pub enum PackError {
    /// The directory has no `pack.json`.
    NoManifest(PathBuf),
    /// `pack.json` could not be read.
    Io(String),
    /// `pack.json` could not be parsed.
    Manifest(String),
}

impl std::fmt::Display for PackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackError::NoManifest(p) => write!(f, "no pack.json in {}", p.display()),
            PackError::Io(s) => write!(f, "reading pack.json: {s}"),
            PackError::Manifest(s) => write!(f, "parsing pack.json: {s}"),
        }
    }
}
impl std::error::Error for PackError {}

/// List the `*.json` files in a pack sub-directory (sorted; scaffolding skipped).
fn list_json(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if is_scaffolding_file(name) {
                continue;
            }
            if p.extension().and_then(|s| s.to_str()) == Some("json") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Load a pack from a directory. Errors only when the manifest is missing or
/// unparseable; per-file failures become [`LoadReport`] notes.
pub fn load_pack(dir: &Path) -> Result<Pack, PackError> {
    let manifest_path = dir.join("pack.json");
    if !manifest_path.is_file() {
        return Err(PackError::NoManifest(dir.to_path_buf()));
    }
    let raw = std::fs::read_to_string(&manifest_path).map_err(|e| PackError::Io(e.to_string()))?;
    let manifest = PackManifest::from_json(&raw).map_err(|e| PackError::Manifest(e.to_string()))?;

    let mut pack = Pack {
        manifest,
        root: dir.to_path_buf(),
        ecudefs: Vec::new(),
        coding: Vec::new(),
        adaptation: Vec::new(),
        services: Vec::new(),
        loggers: Vec::new(),
        modules: Vec::new(),
        report: LoadReport::default(),
    };

    for kind in Kind::all() {
        let sub = dir.join(kind.dir());
        for path in list_json(&sub) {
            let rel = path
                .strip_prefix(dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    pack.report.notes.push(LoadNote {
                        file: rel,
                        kind,
                        message: format!("read error: {e}"),
                    });
                    continue;
                }
            };
            if let Some(msg) = load_one(kind, &text, &mut pack) {
                pack.report.notes.push(LoadNote {
                    file: rel,
                    kind,
                    message: msg,
                });
            } else {
                pack.report.loaded += 1;
            }
        }
    }
    Ok(pack)
}

/// Parse + validate one definition of `kind` from `text`, pushing it into the
/// right vec. Returns `Some(reason)` if it should be skipped (not counted).
fn load_one(kind: Kind, text: &str, pack: &mut Pack) -> Option<String> {
    match kind {
        Kind::EcuDef => match serde_json::from_str::<EcuDef>(text) {
            Ok(d) => {
                let issues = d.validate();
                if has_errors(&issues) {
                    return Some(format!(
                        "ECU def validation errors: {}",
                        issues
                            .iter()
                            .filter(|i| i.severity == opentune_core::Severity::Error)
                            .map(|i| i.message.clone())
                            .collect::<Vec<_>>()
                            .join("; ")
                    ));
                }
                pack.ecudefs.push(d);
                None
            }
            Err(e) => Some(format!("parse error: {e}")),
        },
        Kind::Coding => match serde_json::from_str::<CodingDef>(text) {
            Ok(d) => match d.validate() {
                Ok(()) => {
                    pack.coding.push(d);
                    None
                }
                Err(e) => Some(format!("coding validation: {e}")),
            },
            Err(e) => Some(format!("parse error: {e}")),
        },
        Kind::Adaptation => match serde_json::from_str::<AdaptationDef>(text) {
            Ok(d) => match d.validate() {
                Ok(()) => {
                    pack.adaptation.push(d);
                    None
                }
                Err(e) => Some(format!("adaptation validation: {e}")),
            },
            Err(e) => Some(format!("parse error: {e}")),
        },
        Kind::Service => match serde_json::from_str::<ServiceScript>(text) {
            Ok(d) => match d.validate() {
                Ok(()) => {
                    pack.services.push(d);
                    None
                }
                Err(e) => Some(format!("service-script validation: {e}")),
            },
            Err(e) => Some(format!("parse error: {e}")),
        },
        Kind::Logger => match serde_json::from_str::<LogDef>(text) {
            Ok(d) => {
                pack.loggers.push(d);
                None
            }
            Err(e) => Some(format!("parse error: {e}")),
        },
        Kind::Modules => match serde_json::from_str::<VehicleModules>(text) {
            Ok(d) => {
                pack.modules.push(d);
                None
            }
            Err(e) => Some(format!("parse error: {e}")),
        },
    }
}

/// Find every pack directory (one that contains a `pack.json`) under `root`,
/// recursively. `root` itself is included if it is a pack.
pub fn discover(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
        if dir.join("pack.json").is_file() {
            found.push(dir.to_path_buf());
            return; // a pack's sub-dirs are its contents, not nested packs
        }
        if let Ok(rd) = std::fs::read_dir(dir) {
            let mut subs: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            subs.sort();
            for s in subs {
                walk(&s, found);
            }
        }
    }
    walk(root, &mut found);
    found
}

/// A collection of loaded packs, with simple cross-pack lookups.
#[derive(Debug, Clone, Default)]
pub struct PackLibrary {
    /// The loaded packs.
    pub packs: Vec<Pack>,
    /// Pack directories that failed to open, with the reason.
    pub failed: Vec<(PathBuf, String)>,
}

impl PackLibrary {
    /// Discover and load every pack under `root`.
    pub fn load_all(root: &Path) -> Self {
        let mut lib = PackLibrary::default();
        for dir in discover(root) {
            match load_pack(&dir) {
                Ok(p) => lib.packs.push(p),
                Err(e) => lib.failed.push((dir, e.to_string())),
            }
        }
        lib
    }

    /// Total definitions loaded across all packs, by kind.
    pub fn counts(&self) -> [(Kind, usize); 6] {
        let mut c = [
            (Kind::EcuDef, 0usize),
            (Kind::Coding, 0),
            (Kind::Adaptation, 0),
            (Kind::Service, 0),
            (Kind::Logger, 0),
            (Kind::Modules, 0),
        ];
        for p in &self.packs {
            c[0].1 += p.ecudefs.len();
            c[1].1 += p.coding.len();
            c[2].1 += p.adaptation.len();
            c[3].1 += p.services.len();
            c[4].1 += p.loggers.len();
            c[5].1 += p.modules.len();
        }
        c
    }

    /// All coding defs across packs.
    pub fn coding(&self) -> impl Iterator<Item = &CodingDef> {
        self.packs.iter().flat_map(|p| p.coding.iter())
    }
    /// All adaptation defs across packs.
    pub fn adaptation(&self) -> impl Iterator<Item = &AdaptationDef> {
        self.packs.iter().flat_map(|p| p.adaptation.iter())
    }
    /// All service scripts across packs.
    pub fn services(&self) -> impl Iterator<Item = &ServiceScript> {
        self.packs.iter().flat_map(|p| p.services.iter())
    }
    /// All ECU defs across packs.
    pub fn ecudefs(&self) -> impl Iterator<Item = &EcuDef> {
        self.packs.iter().flat_map(|p| p.ecudefs.iter())
    }
    /// Find a coding def by its `id`.
    pub fn coding_by_id(&self, id: &str) -> Option<&CodingDef> {
        self.coding().find(|d| d.id == id)
    }
    /// Find a service script by its `id`.
    pub fn service_by_id(&self, id: &str) -> Option<&ServiceScript> {
        self.services().find(|s| s.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Create a unique temp directory for a test pack.
    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("opentune-packs-{tag}-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    const CODING: &str = r#"{"id":"test-bcm","name":"BCM","addressing":{"kind":"did","dids":[1025]},
      "params":[{"name":"X","byte":0,"bit":0,"len":1,"kind":"bool"}]}"#;

    #[test]
    fn manifest_parses_with_defaults() {
        let m = PackManifest::from_json(r#"{"id":"p","name":"P"}"#).unwrap();
        assert_eq!(m.id, "p");
        assert!(m.version.is_empty() && m.vehicles.is_empty());
    }

    #[test]
    fn loads_a_pack_and_counts_definitions() {
        let d = temp_dir("load");
        write(
            &d,
            "pack.json",
            r#"{"id":"vag-test","name":"VAG test","version":"1.0.0","license":"GPL-3.0-or-later"}"#,
        );
        write(&d, "coding/bcm.clb.json", CODING);
        // a deliberately broken coding file -> should be noted, not fatal
        write(&d, "coding/broken.json", r#"{"id":"dup","name":"bad"}"#); // missing addressing -> parse error
        let pack = load_pack(&d).unwrap();
        assert_eq!(pack.manifest.id, "vag-test");
        assert_eq!(pack.coding.len(), 1);
        assert_eq!(pack.report.loaded, 1);
        assert_eq!(pack.report.notes.len(), 1);
        assert_eq!(pack.report.notes[0].kind, Kind::Coding);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn invalid_coding_def_is_noted_not_loaded() {
        let d = temp_dir("invalid");
        write(&d, "pack.json", r#"{"id":"p","name":"P"}"#);
        // valid JSON + valid shape but semantically invalid: duplicate DID
        write(
            &d,
            "coding/dupdid.json",
            r#"{"id":"x","name":"X","addressing":{"kind":"did","dids":[1,1]},"params":[]}"#,
        );
        let pack = load_pack(&d).unwrap();
        assert_eq!(pack.coding.len(), 0);
        assert_eq!(pack.report.notes.len(), 1);
        assert!(pack.report.notes[0].message.contains("validation"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn missing_manifest_is_an_error() {
        let d = temp_dir("nomanifest");
        assert!(matches!(load_pack(&d), Err(PackError::NoManifest(_))));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn discover_and_library_merge() {
        let root = temp_dir("lib");
        write(&root.join("packA"), "pack.json", r#"{"id":"a","name":"A"}"#);
        write(&root.join("packA"), "coding/c.json", CODING);
        write(
            &root.join("sub").join("packB"),
            "pack.json",
            r#"{"id":"b","name":"B"}"#,
        );
        let found = discover(&root);
        assert_eq!(found.len(), 2);
        let lib = PackLibrary::load_all(&root);
        assert_eq!(lib.packs.len(), 2);
        assert_eq!(lib.coding().count(), 1);
        assert!(lib.coding_by_id("test-bcm").is_some());
        assert_eq!(lib.counts()[1], (Kind::Coding, 1));
        std::fs::remove_dir_all(&root).ok();
    }
}
