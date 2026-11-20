//! The offline analysis of one image: everything determinable with NO
//! hardware (structural family, identity strings with offsets, definition
//! and vehicle-catalog candidates, the checksum blocks of the detected
//! family). Pure: no UI and no `Host`, so it runs inside a background job and
//! is unit-tested with synthetic images.
//!
//! The scan is `opentune_scan::scan_file` (as the webview app's Analyze bin
//! and the native Flash view use it); the family → algorithm mapping is
//! `opentune_recipe::checksum_for_family`; catalog matching is
//! `opentune_core::catalog::match_catalog`.

use crate::host::{resolve_checksum, ChecksumResolver};
use opentune_checksum::{Recognition, Representation};
use opentune_core::catalog::{self, CatalogEntry};
use opentune_core::ecudef::{self, EcuDef};
use opentune_core::EcuId;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// Vehicle-catalog candidates shown at most (the webview app shows 16).
pub(crate) const MAX_CATALOG: usize = 16;
/// Definition candidates shown at most.
const MAX_DEFS: usize = 6;

/// The checksum algorithm id for a detected structural family: the recipe
/// crate's mapping, plus ME7 (recognised by signature; its validated engine
/// refuses images whose tables it cannot locate), as the webview app's
/// Analyze bin maps it.
pub(crate) fn family_algo(family: &str) -> Option<&'static str> {
    opentune_recipe::checksum_for_family(family).or(match family {
        "ME7" => Some("me7"),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// The vehicle catalog
// ---------------------------------------------------------------------------

/// The loaded vehicle catalog (`catalog/` data root), shared between
/// analyses. `note` says why it is empty, if it is.
#[derive(Clone, Debug, Default)]
pub(crate) struct Catalog {
    pub entries: Arc<Vec<CatalogEntry>>,
    pub note: Option<String>,
}

impl Catalog {
    /// A catalog built in memory (tests, embedders).
    #[cfg(test)]
    pub(crate) fn of(entries: Vec<CatalogEntry>) -> Self {
        Self {
            entries: Arc::new(entries),
            note: None,
        }
    }
}

/// Load every catalog file under `dir` (blocking: call it from a job).
pub(crate) fn load_catalog(dir: Option<&Path>) -> Catalog {
    match dir {
        None => Catalog {
            entries: Arc::default(),
            note: Some(
                "No catalog/ data folder was found, so there are no vehicle candidates.".into(),
            ),
        },
        Some(d) => match catalog::load_catalog(d) {
            Ok(v) => Catalog {
                note: v
                    .is_empty()
                    .then(|| format!("The catalog folder {} holds no entries.", d.display())),
                entries: Arc::new(v),
            },
            Err(e) => Catalog {
                entries: Arc::default(),
                note: Some(format!(
                    "The catalog in {} could not be read: {e}",
                    d.display()
                )),
            },
        },
    }
}

/// One vehicle-catalog candidate.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CatalogCand {
    pub make: String,
    pub models: String,
    pub years: Option<String>,
    pub ecu: Option<String>,
    pub family: Option<String>,
    pub source: Option<String>,
    pub ecudef_id: Option<String>,
    /// The entry names the same family as the image's structure.
    pub family_agrees: bool,
}

/// The catalog entries matching `id`, those agreeing with the detected
/// `family` first, at most [`MAX_CATALOG`].
pub(crate) fn catalog_candidates(
    entries: &[CatalogEntry],
    id: &EcuId,
    family: &str,
) -> Vec<CatalogCand> {
    let fam = norm(family);
    let mut out: Vec<CatalogCand> = catalog::match_catalog(entries, id)
        .into_iter()
        .map(|e| CatalogCand {
            make: e.make.clone(),
            models: e.models.join(", "),
            years: e.years.clone(),
            ecu: e.ecu.clone(),
            family: e.family.clone(),
            source: e.source.clone(),
            ecudef_id: e.ecudef_id.clone(),
            family_agrees: !fam.is_empty()
                && fam != "unknown"
                && e.family.as_deref().map(norm).as_deref() == Some(fam.as_str()),
        })
        .collect();
    // Stable: the catalog's own order inside each group.
    out.sort_by_key(|c| !c.family_agrees);
    out.truncate(MAX_CATALOG);
    out
}

// ---------------------------------------------------------------------------
// Definitions
// ---------------------------------------------------------------------------

/// One ECU-definition candidate.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DefCand {
    pub id: String,
    pub name: String,
    pub family: String,
    pub maps: usize,
    pub checksum: Option<String>,
    /// Why it is a candidate.
    pub why: String,
}

fn norm(s: &str) -> String {
    s.to_ascii_lowercase().replace([' ', '-', '_', '.'], "")
}

fn def_cand(d: &EcuDef, why: String) -> DefCand {
    DefCand {
        id: d.id.clone(),
        name: d.name.clone(),
        family: d.family.clone(),
        maps: d.maps.len(),
        checksum: d.checksum.clone(),
        why,
    }
}

/// Definition candidates: the scanner's structural variant (e.g. `EDC15C6`,
/// from the checksum layout) first, as the Flash view picks, then the
/// HW/SW identify-key match (flagged when its family disagrees with the
/// structure: its keys can be as broad as a Bosch `1037` prefix).
pub(crate) fn def_candidates(
    defs: &[EcuDef],
    family: &str,
    variant: Option<&str>,
    matched: Option<&str>,
) -> Vec<DefCand> {
    let mut out: Vec<DefCand> = Vec::new();
    if let Some(v) = variant.filter(|v| !norm(v).is_empty()) {
        let nv = norm(v);
        for d in defs
            .iter()
            .filter(|d| norm(&d.name).contains(&nv) || norm(&d.id).contains(&nv))
        {
            out.push(def_cand(d, format!("structural variant {v}")));
        }
    }
    if let Some(d) = matched.and_then(|id| defs.iter().find(|d| d.id == id)) {
        if !out.iter().any(|c| c.id == d.id) {
            let fam = norm(family);
            let agrees = fam.is_empty() || fam == "unknown" || norm(&d.family) == fam;
            out.push(def_cand(
                d,
                if agrees {
                    "HW/SW identify key".into()
                } else {
                    format!(
                        "HW/SW identify key only: its family {} differs from the structure ({family})",
                        d.family
                    )
                },
            ));
        }
    }
    out.truncate(MAX_DEFS);
    out
}

/// The "Match HW / SW" box: a typed hardware / software number against the
/// definitions and the catalog (as the webview app's `match_identity`).
pub(crate) fn match_typed(
    defs: &[EcuDef],
    entries: &[CatalogEntry],
    hw: &str,
    sw: &str,
) -> (Option<DefCand>, Vec<CatalogCand>) {
    let id = EcuId {
        hw_number: (!hw.trim().is_empty()).then(|| hw.trim().to_string()),
        sw_number: (!sw.trim().is_empty()).then(|| sw.trim().to_string()),
        ..Default::default()
    };
    if id.hw_number.is_none() && id.sw_number.is_none() {
        return (None, Vec::new());
    }
    let def = ecudef::match_ecu(defs, &id).map(|d| def_cand(d, "HW/SW identify key".into()));
    (def, catalog_candidates(entries, &id, ""))
}

// ---------------------------------------------------------------------------
// Checksums
// ---------------------------------------------------------------------------

/// One checksum-protected block.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BlockRow {
    pub name: String,
    pub start: u32,
    pub len: u32,
    pub stored: u64,
    pub computed: u64,
    pub ok: bool,
}

/// The checksum verification of the analysed image.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChecksumReport {
    pub algo: String,
    /// Where the algorithm came from ("family EDC16", "definition …", "chosen").
    pub why: String,
    /// The blocks, or why verification failed.
    pub blocks: Result<Vec<BlockRow>, String>,
    /// Every block's layout was structurally recognised (not merely
    /// consistent math).
    pub recognised: bool,
}

impl ChecksumReport {
    /// `(ok blocks, all blocks)`.
    pub fn tally(&self) -> (usize, usize) {
        match &self.blocks {
            Ok(b) => (b.iter().filter(|b| b.ok).count(), b.len()),
            Err(_) => (0, 0),
        }
    }
}

/// Verify `bytes` with algorithm `algo` through the build's resolvers first
/// (as `Host::checksum`), then the core set.
pub(crate) fn verify(
    resolvers: &[ChecksumResolver],
    algo: &str,
    why: String,
    bytes: &[u8],
) -> ChecksumReport {
    let mut recognised = false;
    let blocks = match resolve_checksum(resolvers, algo) {
        None => Err(format!("unknown checksum algorithm '{algo}'")),
        Some(a) => a
            .verify_report(bytes, Representation::RawFullImage)
            .map(|v| {
                recognised =
                    !v.is_empty() && v.iter().all(|r| r.recognition == Recognition::Recognised);
                v.into_iter()
                    .map(|r| BlockRow {
                        name: r.status.name,
                        start: r.status.region.address,
                        len: r.status.region.length,
                        stored: r.status.stored,
                        computed: r.status.computed,
                        ok: r.status.ok,
                    })
                    .collect()
            })
            .map_err(|e| e.to_string()),
    };
    ChecksumReport {
        algo: algo.to_string(),
        why,
        blocks,
        recognised,
    }
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// What to analyse.
pub(crate) struct Input {
    /// File name shown in the report.
    pub name: String,
    /// `None`: the dump opened in the Flash view (its working image).
    pub file: Option<PathBuf>,
    pub bytes: Vec<u8>,
    pub defs: Vec<EcuDef>,
    pub catalog: Catalog,
    pub resolvers: Vec<ChecksumResolver>,
    /// A checksum algorithm chosen by the user (`None`: from the family).
    pub algo: Option<String>,
}

/// One identifier read from the image.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IdRow {
    pub kind: &'static str,
    pub value: String,
    pub offset: u64,
    pub count: usize,
}

/// The decoded VIN found in the image.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct VinRow {
    pub vin: String,
    pub make: Option<String>,
    pub country: Option<String>,
    pub year: Option<u16>,
}

/// The full offline report.
#[derive(Clone, Debug)]
pub(crate) struct Report {
    pub name: String,
    pub file: Option<PathBuf>,
    pub size: usize,
    pub size_class: &'static str,
    pub image_kind: &'static str,
    pub family: String,
    pub supplier: Option<&'static str>,
    pub variant: Option<String>,
    /// Confidence of the recognised family (`high` / `medium` / `low`).
    pub family_confidence: Option<&'static str>,
    pub family_how: Vec<String>,
    /// Two incompatible families tie at the top.
    pub contested: bool,
    pub identity: Vec<IdRow>,
    pub vin: Option<VinRow>,
    /// The best part number and its breakdown (key, value).
    pub part: Option<(String, Vec<(&'static str, String)>)>,
    /// The scanner's car assignment (make / model / engine / ECU), its basis
    /// and its honest confidence.
    pub assignment: Vec<(&'static str, String)>,
    pub basis: &'static str,
    pub confidence: &'static str,
    pub defs: Vec<DefCand>,
    /// The map names of the first definition candidate (its map indices).
    pub def_map_names: Vec<String>,
    pub catalog: Vec<CatalogCand>,
    pub catalog_size: usize,
    pub catalog_note: Option<String>,
    /// `None`: no algorithm is known for the family (and none was chosen).
    pub checksum: Option<ChecksumReport>,
    pub took_ms: f64,
}

fn part_breakdown(p: &str) -> (String, Vec<(&'static str, String)>) {
    let pn = opentune_vin::parse_part_number(p);
    let mut rows: Vec<(&'static str, String)> = vec![("kind", format!("{:?}", pn.kind))];
    let mut add = |k: &'static str, v: &Option<String>| {
        if let Some(v) = v.as_ref().filter(|v| !v.is_empty()) {
            rows.push((k, v.clone()));
        }
    };
    add("supplier", &pn.supplier.map(str::to_string));
    add("VAG type group", &pn.vag_type_group);
    add("VAG main group", &pn.vag_main_group);
    add("VAG subgroup", &pn.vag_subgroup);
    add("VAG version", &pn.vag_version);
    add("Bosch family", &pn.bosch_family);
    add("Bosch number", &pn.bosch_consecutive);
    if !pn.flags.is_empty() {
        rows.push(("notes", pn.flags.join("; ")));
    }
    (pn.normalized, rows)
}

/// Analyse one image (blocking: call it from a job).
pub(crate) fn analyze(input: &Input) -> Report {
    let t = Instant::now();
    let ctx = opentune_scan::ScanContext {
        catalog: input.catalog.entries.to_vec(),
        ecudefs: input.defs.clone(),
    };
    let path = input
        .file
        .clone()
        .unwrap_or_else(|| PathBuf::from(&input.name));
    let root = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let rec = opentune_scan::scan_file(&ctx, &root, &path, &input.bytes);

    // The identity the scanner matched with: best part, first SW, VIN.
    let best_part = rec
        .part_numbers
        .first()
        .or_else(|| rec.bosch_numbers.first())
        .or_else(|| rec.mercedes_numbers.first())
        .cloned();
    let id = EcuId {
        vin: rec.vin.clone(),
        hw_number: best_part.clone(),
        sw_number: rec.sw_numbers.first().cloned(),
        ..Default::default()
    };
    let defs = def_candidates(
        &input.defs,
        rec.ecu_family,
        rec.ecu_variant.as_deref(),
        rec.ecudef_match.as_ref().map(|h| h.id.as_str()),
    );
    let catalog = catalog_candidates(&input.catalog.entries, &id, rec.ecu_family);

    // The algorithm: the user's choice, else the family's, else the first
    // definition candidate's.
    let algo = match &input.algo {
        Some(a) => Some((a.clone(), "chosen".to_string())),
        None => family_algo(rec.ecu_family)
            .map(|a| (a.to_string(), format!("family {}", rec.ecu_family)))
            .or_else(|| {
                defs.iter().find_map(|d| {
                    d.checksum
                        .clone()
                        .filter(|c| !c.is_empty())
                        .map(|c| (c, format!("definition {}", d.name)))
                })
            }),
    };
    let checksum = algo.map(|(a, why)| verify(&input.resolvers, &a, why, &input.bytes));

    let a = &rec.assigned_core;
    let assignment: Vec<(&'static str, String)> = [
        ("make", &a.make),
        ("model", &a.model),
        ("engine", &a.engine),
        ("ecu", &a.ecu),
    ]
    .into_iter()
    .filter_map(|(k, v)| v.clone().map(|v| (k, v)))
    .collect();

    Report {
        name: input.name.clone(),
        file: input.file.clone(),
        size: input.bytes.len(),
        size_class: rec.size_class,
        image_kind: rec.recognition.image_kind.as_str(),
        family: rec.ecu_family.to_string(),
        supplier: rec.ecu_supplier,
        variant: rec.ecu_variant.clone(),
        family_confidence: rec.family_confidence.map(|c| c.as_str()),
        family_how: rec.family_how.clone(),
        contested: rec.recognition.contested,
        identity: rec
            .identity
            .iter()
            .map(|f| IdRow {
                kind: f.kind,
                value: f.value.clone(),
                offset: f.offset,
                count: f.count,
            })
            .collect(),
        vin: rec.vin_decoded.as_ref().map(|v| VinRow {
            vin: v.vin.clone(),
            make: v.make.clone(),
            country: v.country.clone(),
            year: v.year,
        }),
        part: best_part.as_deref().map(part_breakdown),
        def_map_names: defs
            .first()
            .and_then(|c| input.defs.iter().find(|d| d.id == c.id))
            .map(|d| d.maps.iter().map(|m| m.name.clone()).collect())
            .unwrap_or_default(),
        assignment,
        basis: rec.assigned_core.basis,
        confidence: rec.confidence_core.as_str(),
        defs,
        catalog,
        catalog_size: input.catalog.entries.len(),
        catalog_note: input.catalog.note.clone(),
        checksum,
        took_ms: t.elapsed().as_secs_f64() * 1000.0,
    }
}

/// A human label for an identity kind.
pub(crate) fn kind_label(kind: &str) -> &str {
    match kind {
        "bosch_hw" => "Bosch HW",
        "bosch_sw" => "Bosch SW",
        "bosch_number" => "Bosch number",
        "vag_part" => "VAG part",
        "mercedes_part" => "Mercedes part",
        "ford_part" => "Ford part",
        "continental_part" => "Continental part",
        "vin" => "VIN",
        "dataset" => "Dataset",
        "calibration_id" => "Calibration id",
        "bosch_descriptor" => "Bosch descriptor",
        "oem_ident_block" => "OEM ident block",
        "engine" => "Engine",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentune_scan::synth::{build, SynthFamily};

    /// A synthetic definition (no maps unless given).
    pub(crate) fn def(id: &str, name: &str, family: &str, checksum: Option<&str>) -> EcuDef {
        let mut d = EcuDef::from_json(&format!(
            r#"{{ "id": "{id}", "name": "{name}", "maker": "Synthetic", "family": "{family}",
                 "protocol": "iso_tp", "diag_layer": "uds", "bitrate": 500000 }}"#
        ))
        .unwrap();
        d.checksum = checksum.map(str::to_string);
        d
    }

    fn entry(make: &str, part: &str, family: Option<&str>) -> CatalogEntry {
        CatalogEntry {
            make: make.into(),
            models: vec![format!("{make} Model")],
            part_numbers: vec![part.into()],
            family: family.map(str::to_string),
            source: Some("synthetic".into()),
            ..Default::default()
        }
    }

    #[test]
    fn family_maps_to_its_checksum_algorithm() {
        assert_eq!(family_algo("EDC15"), Some("edc15"));
        assert_eq!(family_algo("EDC16"), Some("edc16"));
        assert_eq!(family_algo("EDC17"), Some("edc17"));
        assert_eq!(family_algo("PCR2.1"), Some("pcr21"));
        assert_eq!(family_algo("ME7"), Some("me7"));
        assert_eq!(family_algo("Unknown"), None);
        // Every mapped id resolves in the core set.
        for f in ["EDC15", "EDC16", "EDC17", "PCR2.1", "ME7"] {
            assert!(
                opentune_checksum::get(family_algo(f).unwrap()).is_some(),
                "{f}"
            );
        }
    }

    #[test]
    fn catalog_candidates_match_and_put_the_family_first() {
        let entries = vec![
            entry("Alpha", "0261209", Some("ME9")),
            entry("Beta", "0261209", Some("ME7")),
            entry("Gamma", "99999", None),
        ];
        let id = EcuId {
            hw_number: Some("0261209999".into()),
            ..Default::default()
        };
        let c = catalog_candidates(&entries, &id, "ME7");
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].make.as_str(), c[0].family_agrees), ("Beta", true));
        assert_eq!((c[1].make.as_str(), c[1].family_agrees), ("Alpha", false));
        assert_eq!(c[0].models, "Beta Model");
        // At most MAX_CATALOG.
        let many: Vec<CatalogEntry> = (0..40).map(|_| entry("X", "0261209", None)).collect();
        assert_eq!(catalog_candidates(&many, &id, "").len(), MAX_CATALOG);
    }

    #[test]
    fn definition_candidates_prefer_the_structural_variant() {
        let defs = vec![
            def("syn-a", "Synthetic EDC15C6", "EDC15", Some("edc15c6")),
            def("syn-b", "Synthetic other", "EDC16", None),
        ];
        let c = def_candidates(&defs, "EDC15", Some("EDC15C6"), Some("syn-b"));
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].id, "syn-a");
        assert!(c[0].why.contains("structural"));
        assert!(c[1].why.contains("differs"), "{}", c[1].why);
        // Same id from both routes: listed once.
        let c = def_candidates(&defs, "EDC15", Some("edc15-c6"), Some("syn-a"));
        assert_eq!(c.len(), 1);
        assert!(def_candidates(&defs, "Unknown", None, None).is_empty());
    }

    #[test]
    fn typed_hw_sw_matches_definitions_and_catalog() {
        let mut d = def("syn-a", "Synthetic", "ME7", None);
        d.identify.part_numbers = vec!["0261209".into()];
        let entries = vec![entry("Beta", "0261209", Some("ME7"))];
        let (m, cat) = match_typed(std::slice::from_ref(&d), &entries, " 0261209999 ", "");
        assert_eq!(m.map(|m| m.id), Some("syn-a".to_string()));
        assert_eq!(cat.len(), 1);
        assert_eq!(cat[0].make, "Beta");
        assert_eq!(match_typed(&[d], &entries, "  ", ""), (None, Vec::new()));
    }

    #[test]
    fn analysis_of_a_synthetic_me7_image() {
        let bytes = build(SynthFamily::Me7);
        let defs = vec![def("syn-me7", "Synthetic ME7.5", "ME7", Some("me7"))];
        let input = Input {
            name: "synthetic.bin".into(),
            file: None,
            bytes: bytes.clone(),
            defs,
            catalog: Catalog::of(vec![
                entry("Beta", "0261209", Some("ME7")),
                entry("Other", "123456", None),
            ]),
            resolvers: Vec::new(),
            algo: None,
        };
        let r = analyze(&input);
        assert_eq!(r.size, bytes.len());
        assert_eq!(r.family, "ME7");
        assert!(!r.family_how.is_empty());
        assert!(r.identity.iter().any(|f| f.value == "0261209999"));
        // The identity offsets point at the synthetic marker.
        let hw = r.identity.iter().find(|f| f.value == "0261209999").unwrap();
        assert_eq!(hw.offset, 0x1_4000);
        assert_eq!(r.catalog_size, 2);
        assert_eq!(r.catalog.len(), 1);
        assert_eq!(r.catalog[0].make, "Beta");
        assert!(r.catalog[0].family_agrees);
        let ck = r.checksum.as_ref().expect("ME7 has an algorithm");
        assert_eq!((ck.algo.as_str(), ck.why.as_str()), ("me7", "family ME7"));
        assert!(r.defs.iter().any(|d| d.id == "syn-me7"));
    }

    #[test]
    fn chosen_algorithm_wins_and_unknown_ones_are_reported() {
        let input = Input {
            name: "x.bin".into(),
            file: Some(PathBuf::from("/nonexistent/x.bin")),
            bytes: vec![0x5A; 4096],
            defs: Vec::new(),
            catalog: Catalog::default(),
            resolvers: Vec::new(),
            algo: Some("no-such-algo".into()),
        };
        let r = analyze(&input);
        let ck = r.checksum.unwrap();
        assert_eq!(ck.why, "chosen");
        assert!(ck.blocks.unwrap_err().contains("unknown"));
        assert!(!ck.recognised);
        // No family, no definition: no algorithm.
        let r = analyze(&Input {
            algo: None,
            ..input
        });
        assert!(r.checksum.is_none());
        assert!(r.catalog.is_empty());
    }

    #[test]
    fn plugin_resolvers_are_consulted_first() {
        fn r(id: &str) -> Option<Box<dyn opentune_checksum::ChecksumAlgo>> {
            (id == "plugin-crc").then(|| opentune_checksum::get("crc32-tail"))?
        }
        let img = vec![0u8; 256];
        let ck = verify(&[r], "plugin-crc", "chosen".into(), &img);
        assert_eq!(ck.blocks.as_ref().map(Vec::len), Ok(1), "{:?}", ck.blocks);
        assert_eq!(ck.tally().1, 1);
        assert!(verify(&[], "plugin-crc", "chosen".into(), &img)
            .blocks
            .is_err());
    }

    #[test]
    fn missing_catalog_folder_is_explained() {
        let c = load_catalog(None);
        assert!(c.entries.is_empty() && c.note.unwrap().contains("catalog/"));
        let c = load_catalog(Some(Path::new("/nonexistent/opentune-catalog")));
        assert!(c.note.unwrap().contains("could not be read"));
    }

    #[test]
    fn part_numbers_are_broken_down() {
        let (n, rows) = part_breakdown("06A 906 099 X");
        assert_eq!(n, "06A906099X");
        assert!(rows
            .iter()
            .any(|(k, v)| *k == "VAG main group" && v == "906"));
        assert_eq!(kind_label("vag_part"), "VAG part");
        assert_eq!(kind_label("other"), "other");
    }
}
