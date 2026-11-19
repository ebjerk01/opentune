//! Parse every shipped ECU definition in `ecudefs/` and sanity-check it.
//!
//! This guards the JSON files against drifting out of sync with the `EcuDef`
//! schema: a malformed or renamed field fails the build, not a car.
use opentune_core::ecudef::{self, FullRead};
use opentune_core::validate;
use std::path::PathBuf;

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

/// The identity a VW 1.6 TDI CR (EA189) engine ECU reported live over UDS via
/// `elm327obd` (2026-11-18): ECU name (0xF197) and hardware/part number
/// (0xF191). `identify` must resolve it to the identification-only definition,
/// and that definition must not claim anything an image read would be needed
/// for (layout, maps, checksum, seed/key).
/// Regression: `bosch-edc16-toyota` carries placeholder data but real-looking
/// keys (`0281011…`, sw `1037…`) that overlap `bosch-edc16u34`; it sorted first
/// and could claim a real EDC16U34. As an `example` it must never match.
#[test]
fn example_definitions_never_match_a_real_unit() {
    let defs = ecudef::load_dir(&ecudefs_dir()).expect("ecudefs parse");
    let toyota = defs.iter().find(|d| d.id == "bosch-edc16-toyota").expect("toyota def");
    assert!(toyota.is_example(), "placeholder def is evidence `example`");
    assert!(toyota.name.contains("Example — placeholder data"), "{}", toyota.name);
    // Every key the example def carries, alone and together.
    let mut ids = Vec::new();
    for p in &toyota.identify.part_numbers {
        ids.push(opentune_core::EcuId { hw_number: Some(format!("{p}999")), ..Default::default() });
    }
    for s in &toyota.identify.sw_numbers {
        ids.push(opentune_core::EcuId { sw_number: Some(format!("{s}382088")), ..Default::default() });
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
    assert_eq!(ecudef::match_ecu(&defs, &u34).map(|d| d.id.as_str()), Some("bosch-edc16u34"));
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
    assert_eq!(def.id, "vag-1.6tdi-cr-simos-pcr21-ident");
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
