//! **opentune-scan** — an offline ECU-file scanner (prototype).
//!
//! Given a directory of raw ECU dumps, for each file it extracts — *without any
//! hardware* — (1) the ECU family/generation by structure, (2) OEM identity
//! strings (VAG/Bosch part numbers, software numbers, VIN, Mercedes numbers),
//! (3) a catalog/ecudef/VIN match to a car, and (4) a filename/path signal. It
//! then assigns a (make, model, engine, ecu) with an honest confidence grade,
//! reporting hit-rates both with and without the filename signal.
//!
//! It reuses the real OpenTune tools as path dependencies:
//! * [`opentune_checksum`] — structural family fingerprints (see [`family`]).
//! * [`opentune_core::catalog`] / [`opentune_core::ecudef`] — identity matching.
//! * [`opentune_vin`] — VIN decode + part-number parse.
//!
//! ## Wiring into the CLI
//! The library entry points are [`ScanContext::load`] + [`scan_dir`]. A
//! `scan <dir>` subcommand in `opentune-cli` is a thin wrapper:
//! ```ignore
//! let ctx = opentune_scan::ScanContext::load("catalog".as_ref(), "ecudefs".as_ref())?;
//! let report = opentune_scan::scan_dir(&ctx, dir)?;
//! println!("{}", serde_json::to_string_pretty(&report)?);
//! ```

pub mod extract;
pub mod family;
pub mod filename;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use opentune_core::catalog::{self, CatalogEntry};
use opentune_core::ecudef::{self, EcuDef};
use opentune_core::types::EcuId;

use family::EcuFamily;
use filename::FilenameParse;

/// Honest confidence in the assigned car.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Confidence {
    None,
    Low,
    Medium,
    High,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::None => "None",
            Confidence::Low => "Low",
            Confidence::Medium => "Medium",
            Confidence::High => "High",
        }
    }
}

/// What we decided this file is.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Assignment {
    pub make: Option<String>,
    pub model: Option<String>,
    pub engine: Option<String>,
    pub ecu: Option<String>,
    /// How the assignment was justified (catalog / ecudef / vin / filename / ...).
    pub basis: &'static str,
}

impl Assignment {
    fn make_model_engine(&self) -> bool {
        self.make.is_some() && self.model.is_some() && self.engine.is_some()
    }
    fn make_model(&self) -> bool {
        self.make.is_some() && self.model.is_some()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VinDecoded {
    pub vin: String,
    pub make: Option<String>,
    pub country: Option<String>,
    pub year: Option<u16>,
    pub year_ambiguous: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CatalogHit {
    pub make: String,
    pub models: Vec<String>,
    pub ecu: Option<String>,
    pub family: Option<String>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EcudefHit {
    pub id: String,
    pub name: String,
    pub maker: String,
    pub family: String,
}

/// One JSON record per file.
#[derive(Debug, Clone, Serialize)]
pub struct ScanRecord {
    pub path: String,
    pub rel_path: String,
    pub source_folder: String,
    pub size: u64,
    pub ecu_family: &'static str,
    pub family_how: Vec<String>,
    pub size_class: &'static str,
    pub part_numbers: Vec<String>,
    pub bosch_numbers: Vec<String>,
    pub sw_numbers: Vec<String>,
    pub mercedes_numbers: Vec<String>,
    pub epk_hits: usize,
    pub vin: Option<String>,
    pub vin_decoded: Option<VinDecoded>,
    pub catalog_match: Option<CatalogHit>,
    pub ecudef_match: Option<EcudefHit>,
    pub filename_parse: FilenameParse,
    /// Assignment from structure + catalog + VIN only (no filename signal).
    pub assigned_core: Assignment,
    pub confidence_core: Confidence,
    /// Assignment adding the filename signal + part#-bootstrap index.
    pub assigned_boot: Assignment,
    pub confidence_boot: Confidence,
    #[serde(skip)]
    had_identity_string: bool,
}

/// Loaded identification databases.
pub struct ScanContext {
    pub catalog: Vec<CatalogEntry>,
    pub ecudefs: Vec<EcuDef>,
}

impl ScanContext {
    pub fn load(catalog_dir: &Path, ecudef_dir: &Path) -> anyhow::Result<Self> {
        let catalog = catalog::load_catalog(catalog_dir)
            .map_err(|e| anyhow::anyhow!("load catalog {}: {e}", catalog_dir.display()))?;
        let ecudefs = ecudef::load_dir(ecudef_dir)
            .map_err(|e| anyhow::anyhow!("load ecudefs {}: {e}", ecudef_dir.display()))?;
        Ok(Self { catalog, ecudefs })
    }
}

/// How large a file we are willing to read fully (skip obviously-huge blobs).
const MAX_READ: u64 = 16 * 1024 * 1024;
/// Archive extensions we never try to parse as a raw image.
const ARCHIVE_EXT: &[&str] = &[
    "zip", "rar", "7z", "kgb", "gz", "tar", "tgz", "cab", "arj", "z", "lzh",
];

fn ext_lower(p: &Path) -> String {
    p.extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Full result of scanning a directory.
#[derive(Debug, Serialize)]
pub struct ScanReport {
    pub root: String,
    pub records: Vec<ScanRecord>,
    pub summary: Summary,
    pub skipped_archives: usize,
    pub skipped_oversize: usize,
    pub errors: usize,
}

/// Scan every regular file under `root` (recursive), skipping archives.
pub fn scan_dir(ctx: &ScanContext, root: &Path) -> anyhow::Result<ScanReport> {
    let mut files: Vec<PathBuf> = Vec::new();
    collect_files(root, &mut files)?;
    files.sort();

    let mut records: Vec<ScanRecord> = Vec::new();
    let mut skipped_archives = 0usize;
    let mut skipped_oversize = 0usize;
    let mut errors = 0usize;

    for path in &files {
        if ARCHIVE_EXT.contains(&ext_lower(path).as_str()) {
            skipped_archives += 1;
            continue;
        }
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        if meta.len() > MAX_READ {
            skipped_oversize += 1;
            continue;
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => {
                errors += 1;
                continue;
            }
        };
        records.push(scan_file(ctx, root, path, &bytes));
    }

    // --- Phase B: build part#→identity index from well-named files, apply it ---
    let index = build_bootstrap_index(&records);
    for rec in &mut records {
        apply_bootstrap(rec, &index);
    }

    let summary = Summary::compute(&records);
    Ok(ScanReport {
        root: root.display().to_string(),
        records,
        summary,
        skipped_archives,
        skipped_oversize,
        errors,
    })
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(&path, out)?;
        } else if path.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

/// Scan one already-read image into a record (core/no-filename pass only;
/// the bootstrap pass is applied later by [`apply_bootstrap`]).
pub fn scan_file(ctx: &ScanContext, root: &Path, path: &Path, bytes: &[u8]) -> ScanRecord {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let rel_path = rel.display().to_string();
    let source_folder = rel
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .unwrap_or_default();

    let det = family::detect(bytes);
    let ex = extract::extract(bytes);
    let fnp = filename::parse(&rel_path);

    // Best OEM identity -> EcuId for catalog/ecudef matching.
    let best_part = ex
        .vag_part_numbers
        .first()
        .or_else(|| ex.bosch_numbers.first())
        .or_else(|| ex.mercedes_numbers.first())
        .cloned();
    let best_sw = ex.sw_numbers.first().cloned();
    let best_vin = ex.vins.first().cloned();

    let id = EcuId {
        vin: best_vin.clone(),
        hw_number: best_part.clone(),
        sw_number: best_sw.clone(),
        ..Default::default()
    };

    // Catalog match (first hit).
    let catalog_match = catalog::match_catalog(&ctx.catalog, &id)
        .into_iter()
        .next()
        .map(|e| CatalogHit {
            make: e.make.clone(),
            models: e.models.clone(),
            ecu: e.ecu.clone(),
            family: e.family.clone(),
            source: e.source.clone(),
        });

    // Ecudef match.
    let ecudef_match = ecudef::match_ecu(&ctx.ecudefs, &id).map(|d| EcudefHit {
        id: d.id.clone(),
        name: d.name.clone(),
        maker: d.maker.clone(),
        family: d.family.clone(),
    });

    // VIN decode.
    let vin_decoded = best_vin.as_ref().and_then(|v| {
        opentune_vin::decode_vin(v).ok().map(|d| VinDecoded {
            vin: d.vin.clone(),
            make: d.wmi_info.manufacturer.map(|s| s.to_string()),
            country: d.wmi_info.country.map(|s| s.to_string()),
            year: d.model_year.resolved,
            year_ambiguous: d.model_year.resolved.is_none(),
        })
    });

    let had_identity_string = best_part.is_some()
        || best_sw.is_some()
        || best_vin.is_some()
        || !ex.mercedes_numbers.is_empty();

    // --- core assignment (structure + catalog + VIN, NO filename) ---
    let (assigned_core, confidence_core) = grade_core(
        &det.family,
        &catalog_match,
        &ecudef_match,
        &vin_decoded,
        &best_part,
        &ex,
    );

    ScanRecord {
        path: path.display().to_string(),
        rel_path,
        source_folder,
        size: bytes.len() as u64,
        ecu_family: det.family.as_str(),
        family_how: det.how,
        size_class: det.size_class,
        part_numbers: ex.vag_part_numbers.clone(),
        bosch_numbers: ex.bosch_numbers.clone(),
        sw_numbers: ex.sw_numbers.clone(),
        mercedes_numbers: ex.mercedes_numbers.clone(),
        epk_hits: ex.epk_hits,
        vin: best_vin,
        vin_decoded,
        catalog_match,
        ecudef_match,
        filename_parse: fnp,
        assigned_boot: assigned_core.clone(),
        confidence_boot: confidence_core,
        assigned_core,
        confidence_core,
        had_identity_string,
    }
}

/// Grade the DB/structure-only assignment.
fn grade_core(
    fam: &EcuFamily,
    cat: &Option<CatalogHit>,
    ecd: &Option<EcudefHit>,
    vin: &Option<VinDecoded>,
    part: &Option<String>,
    ex: &extract::Extracted,
) -> (Assignment, Confidence) {
    // HIGH: catalog gave make + at least one model (a real car from a DB).
    if let Some(c) = cat {
        if !c.models.is_empty() {
            return (
                Assignment {
                    make: Some(c.make.clone()),
                    model: Some(c.models[0].clone()),
                    engine: None,
                    ecu: c.ecu.clone().or_else(|| c.family.clone()),
                    basis: "catalog",
                },
                Confidence::High,
            );
        }
        // catalog hit with make only -> Medium.
        return (
            Assignment {
                make: Some(c.make.clone()),
                model: None,
                engine: None,
                ecu: c.ecu.clone().or_else(|| c.family.clone()),
                basis: "catalog",
            },
            Confidence::Medium,
        );
    }

    // ecudef match identifies the *ECU* (family + supplier), NOT the car: its
    // `maker` is Bosch/Siemens, not a vehicle make. So it corroborates the ECU
    // family via a part#, but yields no car -> Low, make left None. (The shipped
    // ecudef identify keys are also loose — e.g. a "1037" sw-prefix matches most
    // Bosch diesel cals — so trusting this for a car would be dishonest.)
    if let Some(d) = ecd {
        return (
            Assignment {
                make: None,
                model: None,
                engine: None,
                ecu: Some(d.family.clone()),
                basis: "ecudef",
            },
            Confidence::Low,
        );
    }

    // VIN decoded to a known manufacturer -> make (+ corroborating part# raises
    // nothing above make, since the VIN WMI does not encode model/engine).
    if let Some(v) = vin {
        if let Some(make) = &v.make {
            let conf = Confidence::Medium;
            return (
                Assignment {
                    make: Some(make.clone()),
                    model: None,
                    engine: None,
                    ecu: Some(fam.as_str().to_string()),
                    basis: "vin",
                },
                conf,
            );
        }
    }

    // A parsed VAG/Bosch part number with no DB hit: supplier/family only.
    if let Some(p) = part {
        let pn = opentune_vin::parse_part_number(p);
        use opentune_vin::PartKind;
        match pn.kind {
            PartKind::Vag => {
                return (
                    Assignment {
                        make: Some("VAG (VW/Audi/Seat/Skoda)".to_string()),
                        model: None,
                        engine: None,
                        ecu: Some(fam.as_str().to_string()),
                        basis: "part-vag",
                    },
                    Confidence::Low,
                );
            }
            PartKind::Bosch | PartKind::Unknown => { /* fall through */ }
        }
    }

    // Structural family only (or EPK marker) -> Low. Nothing at all -> None.
    if *fam != EcuFamily::Unknown {
        return (
            Assignment {
                make: None,
                model: None,
                engine: None,
                ecu: Some(fam.as_str().to_string()),
                basis: "structure",
            },
            Confidence::Low,
        );
    }
    if ex.epk_hits > 0 || !ex.bosch_numbers.is_empty() || !ex.sw_numbers.is_empty() {
        return (
            Assignment {
                basis: "identity-string-only",
                ..Default::default()
            },
            Confidence::Low,
        );
    }
    (Assignment::default(), Confidence::None)
}

/// part# -> best (make, model, engine) learned from well-named files.
type BootstrapIndex = BTreeMap<String, (String, Option<String>, Option<String>)>;

fn norm_part(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase()
}

fn build_bootstrap_index(records: &[ScanRecord]) -> BootstrapIndex {
    let mut idx: BootstrapIndex = BTreeMap::new();
    for r in records {
        let fp = &r.filename_parse;
        let Some(make) = &fp.make else { continue };
        // part numbers associated with this file: from the filename AND the image.
        let mut parts: Vec<String> = fp.part_numbers.clone();
        parts.extend(r.part_numbers.iter().cloned());
        parts.extend(r.bosch_numbers.iter().cloned());
        for p in parts {
            let key = norm_part(&p);
            if key.len() < 6 {
                continue;
            }
            let cand = (make.clone(), fp.model.clone(), fp.engine.clone());
            idx.entry(key)
                .and_modify(|e| {
                    // prefer the richer identity (more Some fields)
                    let score = |t: &(String, Option<String>, Option<String>)| {
                        t.1.is_some() as u8 + t.2.is_some() as u8
                    };
                    if score(&cand) > score(e) {
                        *e = cand.clone();
                    }
                })
                .or_insert(cand);
        }
    }
    idx
}

/// Apply the filename signal + bootstrap index to produce the boot assignment.
fn apply_bootstrap(rec: &mut ScanRecord, index: &BootstrapIndex) {
    // If the DB pass already reached a full car, keep it.
    if rec.confidence_core == Confidence::High && rec.assigned_core.make_model() {
        return;
    }

    let fp = &rec.filename_parse;

    // 1) Filename itself asserts a car.
    if fp.full_identity() {
        rec.assigned_boot = Assignment {
            make: fp.make.clone(),
            model: fp.model.clone(),
            engine: fp.engine.clone(),
            ecu: Some(rec.ecu_family.to_string()),
            basis: "filename",
        };
        rec.confidence_boot = Confidence::High;
        return;
    }

    // 2) Bootstrap index: a part# this file carries was seen on a well-named file.
    let mut keys: Vec<String> = rec.part_numbers.iter().map(|p| norm_part(p)).collect();
    keys.extend(rec.bosch_numbers.iter().map(|p| norm_part(p)));
    keys.extend(fp.part_numbers.iter().map(|p| norm_part(p)));
    for k in keys {
        if k.len() < 6 {
            continue;
        }
        if let Some((make, model, engine)) = index.get(&k) {
            // Only an improvement if it adds model/engine we didn't have.
            if model.is_some() || engine.is_some() {
                rec.assigned_boot = Assignment {
                    make: Some(make.clone()),
                    model: model.clone(),
                    engine: engine.clone(),
                    ecu: Some(rec.ecu_family.to_string()),
                    basis: "bootstrap-index",
                };
                rec.confidence_boot = if model.is_some() && engine.is_some() {
                    Confidence::High
                } else {
                    Confidence::Medium
                };
                return;
            }
        }
    }

    // 3) Filename gives make (+ maybe model) but not a full car.
    if fp.make.is_some() {
        let better = (fp.make.is_some() as u8 + fp.model.is_some() as u8)
            > (rec.assigned_core.make.is_some() as u8 + rec.assigned_core.model.is_some() as u8);
        if better {
            rec.assigned_boot = Assignment {
                make: fp.make.clone(),
                model: fp.model.clone(),
                engine: fp.engine.clone(),
                ecu: Some(rec.ecu_family.to_string()),
                basis: "filename",
            };
            rec.confidence_boot = if fp.make.is_some() && fp.model.is_some() {
                Confidence::Medium
            } else {
                Confidence::Low.max(rec.confidence_core)
            };
        }
    }
}

/// Aggregate hit-rate numbers — the deliverable.
#[derive(Debug, Default, Serialize)]
pub struct Summary {
    pub total: usize,
    pub family_by_structure: usize,
    pub family_breakdown: BTreeMap<String, usize>,
    pub had_part_or_sw_or_vin: usize,
    pub had_vin: usize,
    pub catalog_matches: usize,
    pub ecudef_matches: usize,
    // core (no filename)
    pub high_core_make_model_engine: usize,
    pub high_core_make_model: usize,
    pub conf_core_breakdown: BTreeMap<String, usize>,
    // with filename bootstrap
    pub high_boot_make_model_engine: usize,
    pub high_boot_make_model: usize,
    pub conf_boot_breakdown: BTreeMap<String, usize>,
    pub boot_basis_breakdown: BTreeMap<String, usize>,
    // cross-tabs
    pub by_source_folder: BTreeMap<String, FolderStats>,
    pub by_family: BTreeMap<String, FamilyStats>,
}

#[derive(Debug, Default, Serialize)]
pub struct FolderStats {
    pub total: usize,
    pub family_known: usize,
    pub high_core_mme: usize,
    pub high_boot_mme: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct FamilyStats {
    pub total: usize,
    pub had_identity: usize,
    pub high_boot_mme: usize,
}

impl Summary {
    fn compute(records: &[ScanRecord]) -> Self {
        let mut s = Summary {
            total: records.len(),
            ..Default::default()
        };
        for r in records {
            if r.ecu_family != "Unknown" {
                s.family_by_structure += 1;
            }
            *s.family_breakdown
                .entry(r.ecu_family.to_string())
                .or_default() += 1;
            if r.had_identity_string {
                s.had_part_or_sw_or_vin += 1;
            }
            if r.vin.is_some() {
                s.had_vin += 1;
            }
            if r.catalog_match.is_some() {
                s.catalog_matches += 1;
            }
            if r.ecudef_match.is_some() {
                s.ecudef_matches += 1;
            }

            if r.confidence_core == Confidence::High && r.assigned_core.make_model_engine() {
                s.high_core_make_model_engine += 1;
            }
            if r.confidence_core == Confidence::High && r.assigned_core.make_model() {
                s.high_core_make_model += 1;
            }
            *s.conf_core_breakdown
                .entry(r.confidence_core.as_str().to_string())
                .or_default() += 1;

            if r.confidence_boot == Confidence::High && r.assigned_boot.make_model_engine() {
                s.high_boot_make_model_engine += 1;
            }
            if r.confidence_boot == Confidence::High && r.assigned_boot.make_model() {
                s.high_boot_make_model += 1;
            }
            *s.conf_boot_breakdown
                .entry(r.confidence_boot.as_str().to_string())
                .or_default() += 1;
            *s.boot_basis_breakdown
                .entry(r.assigned_boot.basis.to_string())
                .or_default() += 1;

            let f = s
                .by_source_folder
                .entry(r.source_folder.clone())
                .or_default();
            f.total += 1;
            if r.ecu_family != "Unknown" {
                f.family_known += 1;
            }
            if r.confidence_core == Confidence::High && r.assigned_core.make_model_engine() {
                f.high_core_mme += 1;
            }
            if r.confidence_boot == Confidence::High && r.assigned_boot.make_model_engine() {
                f.high_boot_mme += 1;
            }

            let fam = s.by_family.entry(r.ecu_family.to_string()).or_default();
            fam.total += 1;
            if r.had_identity_string {
                fam.had_identity += 1;
            }
            if r.confidence_boot == Confidence::High && r.assigned_boot.make_model_engine() {
                fam.high_boot_mme += 1;
            }
        }
        s
    }
}
