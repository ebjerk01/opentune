//! Parse every shipped ECU definition in `ecudefs/` and sanity-check it.
//!
//! This guards the JSON files against drifting out of sync with the `EcuDef`
//! schema: a malformed or renamed field fails the build, not a car.
use opentune_core::ecudef::{self, FullRead, MIN_IDENTIFY_KEY_LEN};
use opentune_core::validate;
use std::path::{Path, PathBuf};

/// Locate the repo-root `ecudefs/` directory from this crate.
fn ecudefs_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR = <repo>/crates/opentune-core
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("crate is two levels below the repo root")
        .join("ecudefs")
}

#[test]
fn all_ecudefs_parse() {
    let dir = ecudefs_dir();
    assert!(
        dir.is_dir(),
        "ecudefs directory not found at {}",
        dir.display()
    );

    let defs = ecudef::load_dir(&dir).expect("every ecudefs/*.json must parse");
    assert!(
        !defs.is_empty(),
        "expected at least one ECU definition in {}",
        dir.display()
    );

    for def in &defs {
        // Identity fields must be populated.
        assert!(!def.id.is_empty(), "ecu def with empty id");
        assert!(!def.name.is_empty(), "{}: empty name", def.id);
        assert!(!def.maker.is_empty(), "{}: empty maker", def.id);
        assert!(!def.family.is_empty(), "{}: empty family", def.id);

        // A bench/boot-only ECU should still declare a flash size.
        if def.full_read == FullRead::BenchBoot {
            assert!(
                def.flash_size > 0,
                "{}: bench_boot def must set flash_size",
                def.id
            );
        }

        // Every named region must fit inside the declared flash image.
        if def.flash_size > 0 {
            for r in &def.regions {
                let end = r.region.address as u64 + r.region.length as u64;
                let base = def.regions.first().map(|f| f.region.address).unwrap_or(0);
                let image_end = base as u64 + def.flash_size as u64;
                assert!(
                    end <= image_end,
                    "{}: region '{}' ends past the flash image",
                    def.id,
                    r.name
                );
            }
        }
    }
}

/// Every shipped definition in `ecudefs/` must pass the validator with zero
/// errors (warnings are allowed). This is the same gate CI runs via
/// `opentune ecudefs validate ecudefs`, pinned here so `cargo test` catches a
/// regression without the CLI. Scaffolding files (TEMPLATE.json, `_*`) are
/// skipped, exactly as a directory scan skips them.
#[test]
fn all_ecudefs_pass_validation() {
    let dir = ecudefs_dir();
    let mut checked = 0usize;
    for entry in std::fs::read_dir(&dir).expect("read ecudefs dir") {
        let path = entry.expect("dir entry").path();
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if ecudef::is_scaffolding_file(&name) {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read def");
        let def = ecudef::EcuDef::from_json(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut issues = def.validate();
        issues.extend(def.validate_filename(&name));
        // Shipped definitions must carry provenance (the CI gate's rule).
        opentune_core::provenance::require(&mut issues);
        let errs: Vec<_> = issues
            .iter()
            .filter(|i| i.severity == validate::Severity::Error)
            .collect();
        assert!(errs.is_empty(), "{name} has validation errors: {errs:#?}");
        checked += 1;
    }
    assert!(checked > 0, "no ecudefs validated in {}", dir.display());
}

/// The three BMW Siemens MS4x definitions imported from the ms4x-net XDFs
/// (GPL-3.0-only, see docs/BMW-MS4X-IMPORT.md): each parses, passes the
/// validator with zero errors, and keeps its imported identity, image size,
/// map count and provenance. The expected values pin the files as imported;
/// a re-import that changes them must update this test on purpose.
#[test]
fn bmw_ms4x_imports_load_and_validate() {
    // (file, id, family, flash_size, map count)
    let cases = [
        (
            "bmw-ms4x-ms41.json",
            "bmw-ms4x-ms41",
            "Siemens MS41",
            256 * 1024,
            437,
        ),
        (
            "bmw-ms4x-ms42.json",
            "bmw-ms4x-ms42",
            "Siemens MS42",
            512 * 1024,
            1002,
        ),
        (
            "bmw-ms4x-ms43.json",
            "bmw-ms4x-ms43",
            "Siemens MS43",
            512 * 1024,
            1469,
        ),
    ];
    for (file, id, family, size, maps) in cases {
        let text = std::fs::read_to_string(ecudefs_dir().join(file))
            .unwrap_or_else(|e| panic!("{file}: {e}"));
        let def = ecudef::EcuDef::from_json(&text).unwrap_or_else(|e| panic!("{file}: {e}"));
        assert_eq!(def.id, id, "{file}");
        assert_eq!(def.maker, "BMW", "{file}");
        assert_eq!(def.family, family, "{file}");
        assert_eq!(def.flash_size, size, "{file}");
        assert_eq!(def.maps.len(), maps, "{file}");
        assert_eq!(def.full_read, FullRead::BenchBoot, "{file}");
        assert_eq!(def.diag_layer, opentune_core::DiagLayer::Kwp2000, "{file}");
        assert!(def.checksum.is_none(), "{file}: checksum is out of scope");
        assert!(def.seedkey.is_none(), "{file}: no seed/key");

        let p = def
            .provenance
            .as_ref()
            .unwrap_or_else(|| panic!("{file}: no provenance"));
        assert_eq!(p.license, "GPL-3.0-only", "{file}");
        assert_eq!(p.evidence, "implemented", "{file}");
        assert!(
            p.sources.iter().any(|s| s.starts_with("ms4x-net/")),
            "{file}: source must cite ms4x-net: {:?}",
            p.sources
        );

        let mut issues = def.validate();
        issues.extend(def.validate_filename(file));
        opentune_core::provenance::require(&mut issues);
        let errs: Vec<_> = issues
            .iter()
            .filter(|i| i.severity == validate::Severity::Error)
            .collect();
        assert!(errs.is_empty(), "{file} has validation errors: {errs:#?}");
    }
}

/// The identity a VW 1.6 TDI CR (EA189) engine ECU reported live over UDS via
/// `elm327obd` (2026-11-18): ECU name (0xF197) and hardware/part number
/// (0xF191). `identify` must resolve it to the identification-only definition,
/// and that definition must not claim anything an image read would be needed
/// for (layout, maps, checksum, seed/key).
/// Regression: `bosch-edc16-toyota` carries placeholder data but real-looking
/// keys (`0281011…`, formerly also sw `1037`) that overlap `bosch-edc16u34`; it sorted first
/// and could claim a real EDC16U34. As an `example` it must never match.
#[test]
fn example_definitions_never_match_a_real_unit() {
    let defs = ecudef::load_dir(&ecudefs_dir()).expect("ecudefs parse");
    let toyota = defs
        .iter()
        .find(|d| d.id == "bosch-edc16-toyota")
        .expect("toyota def");
    assert!(toyota.is_example(), "placeholder def is evidence `example`");
    assert!(
        toyota.name.contains("Example — placeholder data"),
        "{}",
        toyota.name
    );
    // Every key the example def carries, alone and together.
    let mut ids = Vec::new();
    for p in &toyota.identify.part_numbers {
        ids.push(opentune_core::EcuId {
            hw_number: Some(format!("{p}999")),
            ..Default::default()
        });
    }
    for s in &toyota.identify.sw_numbers {
        ids.push(opentune_core::EcuId {
            sw_number: Some(format!("{s}382088")),
            ..Default::default()
        });
    }
    for id in &ids {
        assert!(!toyota.matches_id(id), "example matched {id:?}");
        if let Some(d) = ecudef::match_ecu(&defs, id) {
            assert!(!d.is_example(), "{id:?} resolved to example def {}", d.id);
        }
    }
    // An EDC16U34-style identity (as read from an EDC16U34 image) resolves to
    // the real definition, not the example.
    let u34 = opentune_core::EcuId {
        hw_number: Some("0281011900".into()),
        sw_number: Some("1037382088".into()),
        ..Default::default()
    };
    assert_eq!(
        ecudef::match_ecu(&defs, &u34).map(|d| d.id.as_str()),
        Some("bosch-edc16u34")
    );
}

/// A synthetic identity: the hardware and software numbers an ECU reports.
fn ecu(hw: Option<&str>, sw: Option<&str>) -> opentune_core::EcuId {
    opentune_core::EcuId {
        hw_number: hw.map(str::to_string),
        sw_number: sw.map(str::to_string),
        ..Default::default()
    }
}

/// The shipped definitions in load order and reversed, so a test proves the
/// match does not depend on which definition sorts first.
fn both_orders() -> [Vec<ecudef::EcuDef>; 2] {
    let defs = ecudef::load_dir(&ecudefs_dir()).expect("ecudefs parse");
    let mut rev = defs.clone();
    rev.reverse();
    [defs, rev]
}

fn resolve(defs: &[ecudef::EcuDef], id: &opentune_core::EcuId) -> Option<String> {
    ecudef::match_ecu(defs, id).map(|d| d.id.clone())
}

/// Regression: every Bosch software number starts with `1037`, and three
/// definitions (EDC16U34, ME7, the EDC16 example) carried a bare `"1037"` sw
/// key, so a Mercedes EDC15C6 dump was claimed by `bosch-edc16u34` (and any
/// ME7 dump too, because `edc16u34` sorts first). Synthetic identities only.
#[test]
fn edc15c6_identity_is_not_claimed_by_a_generic_bosch_prefix() {
    for defs in both_orders() {
        // EDC15C6 software 1037351686 with no hardware number, a Bosch
        // 0281010… one, a Mercedes A611… one, and a Bosch number that falls
        // inside `bosch-edc16u34`'s `0281011` family prefix: the full software
        // number is the more specific key and wins every time.
        for hw in [
            None,
            Some("0281010999"),
            Some("A6111539999"),
            Some("0281011999"),
        ] {
            let id = ecu(hw, Some("1037351686"));
            assert_eq!(
                resolve(&defs, &id).as_deref(),
                Some("bosch-edc15c6"),
                "{id:?}"
            );
        }
        // An EDC15C6 software variant the definition does not list resolves to
        // nothing, never to another family through the shared `1037` prefix.
        for hw in [None, Some("0281010999")] {
            let id = ecu(hw, Some("1037351999"));
            assert_eq!(resolve(&defs, &id), None, "{id:?}");
            let u34 = defs.iter().find(|d| d.id == "bosch-edc16u34").unwrap();
            assert!(!u34.matches_id(&id), "{id:?}");
        }
        // A Bosch identity with nothing but the `1037` family in common with a
        // definition matches nothing.
        for hw in [None, Some("0281099999"), Some("0261209999")] {
            let id = ecu(hw, Some("1037999999"));
            assert_eq!(resolve(&defs, &id), None, "{id:?}");
        }
        // An ME7 identity resolves to ME7 (it used to lose to `edc16u34`'s
        // `1037` key, which sorts first).
        let me7 = ecu(Some("0261206999"), Some("1037999999"));
        assert_eq!(resolve(&defs, &me7).as_deref(), Some("bosch-me7"));
    }
}

/// Every identify key of every real (non-example) shipped definition still
/// resolves to its own definition, in either load order: the specificity rule
/// changes nothing for genuinely specific keys.
#[test]
fn every_identify_key_resolves_to_its_own_definition() {
    for defs in both_orders() {
        let mut checked = 0usize;
        for d in defs.iter().filter(|d| !d.is_example()) {
            let parts = d
                .identify
                .part_numbers
                .iter()
                .map(|k| ecu(Some(&format!("{k}999")), None));
            let sws = d
                .identify
                .sw_numbers
                .iter()
                .map(|k| ecu(None, Some(&format!("{k}999"))));
            for id in parts.chain(sws) {
                assert_eq!(
                    resolve(&defs, &id).as_deref(),
                    Some(d.id.as_str()),
                    "{id:?}"
                );
                checked += 1;
            }
        }
        assert!(checked > 20, "only {checked} identify keys checked");
    }
}

/// The identify keys of every definition file the project ships: the root
/// set (including `TEMPLATE.json`, which contributors copy), the Android app's
/// bundled copy and the demo definition in `samples/`.
fn identify_keys_in(dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read json");
        let v: serde_json::Value =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        for field in ["part_numbers", "sw_numbers"] {
            for k in v["identify"][field].as_array().into_iter().flatten() {
                let k = k
                    .as_str()
                    .unwrap_or_else(|| panic!("{}: non-string key", path.display()));
                out.push((
                    format!("{} identify.{field}", path.display()),
                    k.to_string(),
                ));
            }
        }
    }
    out
}

/// No shipped definition carries an identify key shorter than
/// [`MIN_IDENTIFY_KEY_LEN`] (whitespace ignored): such a key is a generic
/// prefix like `1037`, which the matcher ignores and the validator rejects.
#[test]
fn no_identify_key_is_shorter_than_the_minimum() {
    let root = ecudefs_dir().parent().expect("repo root").to_path_buf();
    let mut keys = Vec::new();
    for dir in ["ecudefs", "android/app/src/main/assets/ecudefs", "samples"] {
        keys.extend(identify_keys_in(&root.join(dir)));
    }
    assert!(keys.len() > 20, "only {} identify keys found", keys.len());
    let short: Vec<_> = keys
        .iter()
        .filter(|(_, k)| k.split_whitespace().collect::<String>().len() < MIN_IDENTIFY_KEY_LEN)
        .collect();
    assert!(
        short.is_empty(),
        "identify keys shorter than {MIN_IDENTIFY_KEY_LEN} characters: {short:#?}"
    );
}

#[test]
fn live_vw_16tdi_identity_matches_identification_only_def() {
    let defs = ecudef::load_dir(&ecudefs_dir()).expect("ecudefs parse");
    let id = opentune_core::EcuId {
        ecu_name: Some("R4 1.6l TDI".into()),
        hw_number: Some("03L907425C".into()),
        ..Default::default()
    };
    let def = ecudef::match_ecu(&defs, &id).expect("03L907425C must match a definition");
    assert_eq!(def.id, "siemens-pcr21-vag16tdi-ident");
    assert_eq!(def.diag_layer, opentune_core::DiagLayer::Uds);
    assert_eq!((def.tx_id, def.rx_id), (0x7E0, 0x7E8));
    // Identification only: no checksum, seed/key, maps, layout or size claim.
    assert!(def.checksum.is_none(), "no checksum claim");
    assert!(def.seedkey.is_none(), "no seed/key claim");
    assert!(def.maps.is_empty(), "no maps");
    assert!(def.regions.is_empty(), "no flash layout");
    assert_eq!(def.flash_size, 0, "no flash size");

    // The ECU name is generic across VAG 1.6 TDI ECUs and is not a match key.
    let name_only = opentune_core::EcuId {
        ecu_name: Some("R4 1.6l TDI".into()),
        ..Default::default()
    };
    assert!(ecudef::match_ecu(&defs, &name_only).is_none());
}
