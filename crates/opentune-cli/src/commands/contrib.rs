//! Contributor checks for definition data: `packs validate` and `privacy-scan`.
//! (`ecudefs validate` lives in [`super::defs`] and reuses the helpers here.)
use crate::cli::PacksAction;
use anyhow::{anyhow, Context, Result};
use opentune_core::privacy;
use opentune_core::validate::{Severity, ValidationIssue};
use std::path::{Path, PathBuf};

/// File extensions `privacy-scan` reads when walking a directory. Explicitly
/// named files are scanned whatever their extension (if they are UTF-8 text).
const DATA_EXTENSIONS: &[&str] = &[
    "json", "md", "txt", "csv", "tsv", "xdf", "xml", "a2l", "dbc", "ini", "msq", "toml", "yaml",
    "yml", "log",
];

/// Directory names never descended into.
const SKIP_DIRS: &[&str] = &["target", "node_modules", ".git"];

/// Load an allow-list file (or none).
pub fn load_allow(path: Option<&Path>) -> Result<Vec<String>> {
    match path {
        None => Ok(Vec::new()),
        Some(p) => {
            let text =
                std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?;
            Ok(privacy::parse_allow_list(&text))
        }
    }
}

/// Privacy findings in `text` as validation errors (location `line:col`).
pub fn privacy_issues(text: &str, allow: &[String]) -> Vec<ValidationIssue> {
    privacy::scan(text, allow)
        .into_iter()
        .map(|f| ValidationIssue {
            severity: Severity::Error,
            code: f.kind.code(),
            location: format!("line {}:{}", f.line, f.column),
            message: f.message(),
        })
        .collect()
}

/// Print one issue in the `ecudefs validate` layout.
pub fn print_issue(i: &ValidationIssue) {
    let loc = if i.location.is_empty() {
        String::new()
    } else {
        format!(" [{}]", i.location)
    };
    println!(
        "  {:<6} {:<28}{} {}",
        i.severity.tag(),
        i.code,
        loc,
        i.message
    );
}

/// `packs <action>`
pub fn packs_action(action: PacksAction) -> Result<()> {
    match action {
        PacksAction::Validate {
            dir,
            allow,
            warnings,
        } => packs_validate(&dir, allow.as_deref(), warnings),
    }
}

fn packs_validate(dir: &Path, allow: Option<&Path>, warnings: bool) -> Result<()> {
    if !dir.is_dir() {
        return Err(anyhow!("{} is not a directory", dir.display()));
    }
    let allow = load_allow(allow)?;
    let results = opentune_packs::validate_all(dir, &allow);
    if results.is_empty() {
        return Err(anyhow!(
            "no packs found under {} (a pack is a directory with pack.json)",
            dir.display()
        ));
    }
    let (mut errs, mut warns, mut bad) = (0usize, 0usize, 0usize);
    for v in &results {
        let (e, w) = (v.errors(), v.warnings());
        errs += e;
        warns += w;
        let label = if v.id.is_empty() {
            v.dir.display().to_string()
        } else {
            format!("{} ({})", v.id, v.dir.display())
        };
        if e == 0 {
            println!("pack {label}: OK ({w} warning(s))");
        } else {
            bad += 1;
            println!("pack {label}: {e} error(s), {w} warning(s)");
        }
        for i in v
            .issues
            .iter()
            .filter(|i| warnings || i.severity == Severity::Error)
        {
            print_issue(i);
        }
    }
    println!(
        "\n{} pack(s): {errs} error(s), {warns} warning(s); {bad} pack(s) failed.",
        results.len()
    );
    if errs > 0 {
        return Err(crate::ExitStatus(2).into());
    }
    Ok(())
}

/// `privacy-scan`
pub fn privacy_scan(paths: Vec<PathBuf>, allow: Option<PathBuf>, github: bool) -> Result<()> {
    let allow = load_allow(allow.as_deref())?;
    let mut files = Vec::new();
    for p in &paths {
        if p.is_dir() {
            walk(p, &mut files);
        } else if p.is_file() {
            files.push(p.clone());
        } else {
            return Err(anyhow!("{} does not exist", p.display()));
        }
    }
    let (mut scanned, mut skipped, mut findings) = (0usize, 0usize, 0usize);
    for f in &files {
        let Ok(text) = std::fs::read_to_string(f) else {
            skipped += 1; // binary / not UTF-8: not a data file this scan reads
            continue;
        };
        scanned += 1;
        let name = f.display().to_string().replace('\\', "/");
        for hit in privacy::scan(&text, &allow) {
            findings += 1;
            println!(
                "{name}:{}:{}: {} {} ({})",
                hit.line,
                hit.column,
                hit.kind.code(),
                hit.kind.label(),
                hit.redacted()
            );
            if github {
                println!(
                    "::error file={name},line={},col={}::{}: remove it or use a clearly synthetic placeholder ({})",
                    hit.line,
                    hit.column,
                    hit.kind.label(),
                    hit.redacted()
                );
            }
        }
    }
    println!(
        "privacy-scan: {scanned} file(s) scanned, {skipped} non-text skipped, {findings} finding(s)."
    );
    if findings > 0 {
        println!(
            "Remove personal data, or replace it with an obvious placeholder (e.g. WVWZZZ00000000000, \
             name@example.com). A deliberate synthetic value can go in an allow-list (--allow)."
        );
        return Err(crate::ExitStatus(2).into());
    }
    Ok(())
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if p.is_dir() {
            if !SKIP_DIRS.contains(&name) {
                walk(&p, out);
            }
        } else if p
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| DATA_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        {
            out.push(p);
        }
    }
}
