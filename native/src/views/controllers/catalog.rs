//! The controller catalog: the built-in `ctrldefs/` plus the `controllers/`
//! of every definition pack (the bundled `packs/` and pack folders the user
//! loaded) plus single definition files the user opened, each validated
//! (structure, honesty, file name and links against the bundled ECU
//! definitions, database units and checksum ids, plus the pack's own ECU
//! definitions). The catalog, the link context and the checks are
//! `opentune_hub::controllers` (shared with the webview app's `ctrl_list` /
//! `ctrl_get`); this module only resolves the folders.

use opentune_ctrldef::{ControllerDef, Issue, LinkContext, Severity};
use opentune_hub::controllers::{all_issues, catalog, ctx_for, link_context, open_found, Found};
use std::path::{Path, PathBuf};

/// One listed definition, where it came from, and its checks.
pub(crate) struct Entry {
    pub path: PathBuf,
    pub def: ControllerDef,
    /// `builtin`, `pack` or `file`.
    pub source: &'static str,
    /// `pack <id> v<version>` for a pack definition.
    pub pack: Option<String>,
    pub issues: Vec<Issue>,
}

impl Entry {
    pub fn errors(&self) -> usize {
        self.issues
            .iter()
            .filter(|i| i.severity == Severity::Error)
            .count()
    }
    pub fn warnings(&self) -> usize {
        self.issues.len() - self.errors()
    }
    /// Where it came from, in words.
    pub fn source_label(&self) -> String {
        match (self.source, &self.pack) {
            ("pack", Some(p)) => p.clone(),
            ("file", _) => "file".into(),
            _ => "built-in".into(),
        }
    }
}

/// A file (or pack) that did not load, with the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FailedFile {
    pub file: String,
    pub message: String,
}

/// Where the catalog is read from.
#[derive(Clone, Debug, Default)]
pub(crate) struct Sources {
    /// Candidate built-in `ctrldefs` folders (the first existing one is used).
    pub builtin: Vec<PathBuf>,
    /// The bundled packs root (every pack under it).
    pub packs: Option<PathBuf>,
    /// Extra pack folders the user loaded.
    pub extra: Vec<String>,
    /// Single definition files the user opened.
    pub files: Vec<PathBuf>,
    /// For the link checks.
    pub ecudefs: Option<PathBuf>,
    pub database: Option<PathBuf>,
}

impl Sources {
    /// The app's bundled folders.
    pub fn bundled() -> Self {
        Self {
            builtin: crate::paths::candidates("ctrldefs"),
            packs: crate::paths::find("packs"),
            ecudefs: crate::paths::find("ecudefs"),
            database: crate::paths::find("database"),
            ..Default::default()
        }
    }
}

/// What a load produced.
pub(crate) enum Loaded {
    Ok {
        defs: Vec<Entry>,
        failed: Vec<FailedFile>,
        dir: PathBuf,
    },
    /// No built-in folder (`tried`) and nothing else to list.
    Missing { tried: Vec<PathBuf> },
    /// Nothing loaded from `dir`.
    Failed { dir: PathBuf, err: String },
}

/// Built-in, pack and opened definitions, validated with `base`.
pub(crate) fn load(src: &Sources) -> Loaded {
    let builtin = src.builtin.iter().find(|p| p.is_dir()).cloned();
    let (found, failed) = catalog(builtin.as_deref(), src.packs.as_deref(), &src.extra);
    let mut failed: Vec<FailedFile> = failed
        .iter()
        .map(|f| FailedFile {
            file: f["file"].as_str().unwrap_or_default().to_string(),
            message: f["message"].as_str().unwrap_or_default().to_string(),
        })
        .collect();
    let mut found = found;
    for p in &src.files {
        match open_found(builtin.as_deref(), p) {
            // A file already listed (e.g. a built-in one) is not listed twice.
            Ok(f) if !found.iter().any(|x| same_file(&x.path, &f.path)) => found.push(f),
            Ok(_) => {}
            Err(e) => failed.push(FailedFile {
                file: p.display().to_string(),
                message: e,
            }),
        }
    }
    let dir = match builtin.or_else(|| src.packs.clone()) {
        Some(d) => d,
        None if found.is_empty() => {
            return Loaded::Missing {
                tried: src.builtin.clone(),
            }
        }
        None => PathBuf::new(),
    };
    if found.is_empty() {
        let err = match failed.first() {
            Some(f) => format!("{}: {}", f.file, f.message),
            None => format!("no controller definitions in {}", dir.display()),
        };
        return Loaded::Failed { dir, err };
    }
    let base = base_context(src);
    let defs = found.into_iter().map(|f| entry(f, &base)).collect();
    Loaded::Ok { defs, failed, dir }
}

fn same_file(a: &Path, b: &Path) -> bool {
    let c = |x: &Path| std::fs::canonicalize(x).unwrap_or_else(|_| x.to_path_buf());
    c(a) == c(b)
}

/// The link context: the bundled ECU definitions, database unit ids and
/// public checksum ids.
pub(crate) fn base_context(src: &Sources) -> LinkContext {
    let ecudefs = src
        .ecudefs
        .as_deref()
        .and_then(|d| opentune_core::ecudef::load_dir(d).ok())
        .unwrap_or_default();
    link_context(ecudefs, src.database.as_deref())
}

fn entry(f: Found, base: &LinkContext) -> Entry {
    // A checksum id only an embedder's resolver knows is not visible here
    // (core view without the app's resolvers): it is checked against the
    // public ids.
    let issues = all_issues(&f, &ctx_for(base, &|_| false, &f));
    let pack = f.pack.as_ref().map(|p| {
        let id = p["id"].as_str().unwrap_or("?");
        match p["version"].as_str().filter(|v| !v.is_empty()) {
            Some(v) => format!("pack {id} v{v}"),
            None => format!("pack {id}"),
        }
    });
    Entry {
        path: f.path,
        def: f.def,
        source: f.source,
        pack,
        issues,
    }
}
