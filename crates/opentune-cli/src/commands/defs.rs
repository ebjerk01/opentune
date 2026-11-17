//! Definition data: ECU definitions, the control-unit database, packs.
use crate::cli::{DbAction, EcudefsAction};
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

/// `ecudefs <action>`
pub fn ecudefs(action: EcudefsAction) -> Result<()> {
    match action {
        EcudefsAction::List { dir } => {
            let defs = opentune_core::ecudef::load_dir(&dir).map_err(|e| anyhow!("{e}"))?;
            println!("{} ECU definition(s):", defs.len());
            for d in &defs {
                println!(
                    "  {:<22} {:<16} {:>5} KiB  {:?}",
                    d.id,
                    d.name,
                    d.flash_size / 1024,
                    d.protocol
                );
            }
        }
        EcudefsAction::Validate {
            path,
            image,
            warnings,
        } => {
            let img = match &image {
                Some(p) => Some(std::fs::read(p).with_context(|| format!("read {}", p.display()))?),
                None => None,
            };
            let files = collect_ecudef_files(&path)?;
            if files.is_empty() {
                return Err(anyhow!(
                    "no *.json ECU definitions found at {}",
                    path.display()
                ));
            }
            let mut total_errors = 0usize;
            let mut total_warns = 0usize;
            let mut bad_files = 0usize;
            for file in &files {
                let name = file
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?")
                    .to_string();
                let text = std::fs::read_to_string(file)
                    .with_context(|| format!("read {}", file.display()))?;
                let def = match opentune_core::ecudef::EcuDef::from_json(&text) {
                    Ok(d) => d,
                    Err(e) => {
                        total_errors += 1;
                        bad_files += 1;
                        println!("{name}: 1 error, 0 warnings");
                        println!("  ERROR  parse                    {e}");
                        continue;
                    }
                };
                let mut issues = def.validate();
                issues.extend(def.validate_filename(&name));
                if let Some(img) = &img {
                    issues.extend(def.validate_against_image(img));
                }
                let errs = opentune_core::validate::error_count(&issues);
                let warns = opentune_core::validate::warn_count(&issues);
                total_errors += errs;
                total_warns += warns;
                if errs > 0 {
                    bad_files += 1;
                }
                let shown: Vec<_> = issues
                    .iter()
                    .filter(|i| warnings || i.severity == opentune_core::Severity::Error)
                    .collect();
                if errs == 0 && (warns == 0 || !warnings) {
                    println!("{name}: OK ({warns} warning(s))");
                } else {
                    println!("{name}: {errs} error(s), {warns} warning(s)");
                }
                for i in shown {
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
            }
            println!(
                "\n{} file(s): {} error(s), {} warning(s); {} file(s) failed.",
                files.len(),
                total_errors,
                total_warns,
                bad_files
            );
            if total_errors > 0 {
                std::process::exit(2);
            }
        }
    }
    Ok(())
}

/// `db <action>`
pub fn db(action: DbAction) -> Result<()> {
    match action {
        DbAction::List { kind, q, dir } => {
            let db = opentune_db::Database::load_dir(&dir).map_err(|e| anyhow!("{e}"))?;
            let rows = db.list(kind.as_deref(), q.as_deref());
            println!("{} unit(s):", rows.len());
            for r in rows {
                println!(
                    "  {:<24} {:<4} {:<44} {:<10} pinout: {}",
                    r.id,
                    r.kind.as_str(),
                    r.name,
                    r.checksum.as_deref().unwrap_or("-"),
                    r.pinout_confidence.map_or("none", |c| c.as_str())
                );
            }
        }
        DbAction::Show { id, dir } => {
            let db = opentune_db::Database::load_dir(&dir).map_err(|e| anyhow!("{e}"))?;
            let v = db.get(&id).ok_or_else(|| anyhow!("unknown unit '{id}'"))?;
            println!("{}", serde_json::to_string_pretty(v)?);
        }
        DbAction::Validate { dir, ecudefs } => {
            let db = opentune_db::Database::load_dir(&dir).map_err(|e| anyhow!("{e}"))?;
            let ids = opentune_db::ecudef_ids(&ecudefs).map_err(|e| anyhow!("{e}"))?;
            let issues = db.validate(Some(&ids));
            for i in &issues {
                println!("  {i}");
            }
            if !issues.is_empty() {
                return Err(anyhow!("{} issue(s)", issues.len()));
            }
            println!("{} unit(s) OK", db.entries().len());
        }
    }
    Ok(())
}

/// `packs`
pub fn packs(dir: PathBuf) -> Result<()> {
    let lib = opentune_packs::PackLibrary::load_all(&dir);
    if lib.packs.is_empty() && lib.failed.is_empty() {
        println!(
            "No packs found under {} (a pack is a directory with pack.json).",
            dir.display()
        );
    }
    for p in &lib.packs {
        let m = &p.manifest;
        let name = if m.name.is_empty() {
            "(unnamed)"
        } else {
            &m.name
        };
        let ver = if m.version.is_empty() {
            "?"
        } else {
            &m.version
        };
        println!("pack {} — {name} v{ver}", m.id);
        if !m.maker.is_empty() || !m.platform.is_empty() {
            println!("  {} {}", m.maker, m.platform);
        }
        if !m.license.is_empty() {
            println!("  license: {}", m.license);
        }
        println!(
            "  defs: {} ecu · {} coding · {} adaptation · {} service · {} logger · {} module-list  ({} loaded)",
            p.ecudefs.len(),
            p.coding.len(),
            p.adaptation.len(),
            p.services.len(),
            p.loggers.len(),
            p.modules.len(),
            p.report.loaded,
        );
        for n in &p.report.notes {
            println!("  ! {} [{}]: {}", n.file, n.kind.dir(), n.message);
        }
    }
    for (d, e) in &lib.failed {
        println!("! failed to open pack {}: {e}", d.display());
    }
    let c = lib.counts();
    println!(
        "TOTAL across {} pack(s): {} ecu · {} coding · {} adaptation · {} service · {} logger · {} module-list",
        lib.packs.len(),
        c[0].1,
        c[1].1,
        c[2].1,
        c[3].1,
        c[4].1,
        c[5].1,
    );
    Ok(())
}

/// Resolve an ecudef path to the list of *.json files to validate: a single
/// file is returned as-is; a directory yields every *.json inside it (sorted).
fn collect_ecudef_files(path: &std::path::Path) -> Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if path.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(path)
            .with_context(|| format!("read dir {}", path.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .filter(|p| {
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                !opentune_core::ecudef::is_scaffolding_file(name)
            })
            .collect();
        files.sort();
        return Ok(files);
    }
    Err(anyhow!(
        "{} is neither a file nor a directory",
        path.display()
    ))
}
