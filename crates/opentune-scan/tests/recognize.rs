//! Signature recognition on synthetic images (no real dumps needed), plus
//! fixture-gated checks against real dumps under `OPENTUNE_FIXTURES`.

use opentune_scan::extract::extract;
use opentune_scan::family;
use opentune_scan::identity::identity;
use opentune_scan::recognize::{image_kind, recognize, ImageKind, Recognition, Tier};
use opentune_scan::synth::{self, SynthFamily};

fn run(img: &[u8]) -> Recognition {
    let det = family::detect(img);
    recognize(img, family::structural_detections(&det))
}

#[test]
fn every_synthetic_family_is_recognised_with_its_variant_and_evidence() {
    for &f in synth::ALL {
        let img = synth::build(f);
        let exp = synth::expected(f);
        let r = run(&img);
        let p = r
            .primary
            .as_ref()
            .unwrap_or_else(|| panic!("{f:?}: nothing recognised"));
        assert_eq!(p.family, exp.family, "{f:?}: {r:#?}");
        assert_eq!(p.supplier, exp.supplier, "{f:?}");
        assert_eq!(p.variant.as_deref(), exp.variant, "{f:?}: {r:#?}");
        assert!(!r.contested, "{f:?} must not be contested: {r:#?}");
        assert!(p.confidence >= Tier::Medium, "{f:?}: {:?}", p.confidence);
        assert!(
            p.evidence.iter().any(|e| e.offset.is_some()),
            "{f:?}: evidence must carry offsets"
        );
        // Every offset must point inside the image.
        for e in &p.evidence {
            if let Some(o) = e.offset {
                assert!(
                    (o as usize) < img.len(),
                    "{f:?}: offset {o:#X} out of range"
                );
            }
        }
    }
}

#[test]
fn evidence_offsets_point_at_the_marker_bytes() {
    let img = synth::build(SynthFamily::Edc15);
    let r = run(&img);
    let p = r.primary.unwrap();
    let tsw = p
        .evidence
        .iter()
        .find(|e| e.marker.contains("TSW"))
        .expect("TSW evidence");
    assert_eq!(tsw.offset, Some(0x8000));
    assert_eq!(&img[0x8000..0x8005], b"TSW V");
}

#[test]
fn me7_zz_block_alone_is_only_low_confidence() {
    let mut img = vec![0xFFu8; 0x8_0000];
    img[0x1_0000..0x1_0004].copy_from_slice(b"ZZ\xFF\xFF");
    let r = run(&img);
    let p = r.primary.expect("ZZ block is a (weak) ME7 signal");
    assert_eq!(p.family, "ME7");
    assert_eq!(p.confidence, Tier::Low);
    assert_eq!(p.variant, None);
}

#[test]
fn marelli_style_printable_zz_is_not_me7() {
    let mut img = vec![0xFFu8; 0x8_0000];
    img[0x1_0000..0x1_0004].copy_from_slice(b"ZZ43");
    let r = run(&img);
    assert!(r.primary.is_none(), "{r:#?}");
}

#[test]
fn noise_and_blank_images_are_not_recognised() {
    for seed in 1..=8u64 {
        let img = synth::noise(0x8_0000, seed);
        let r = run(&img);
        assert!(
            r.primary.as_ref().map_or(true, |p| !p.has_family()),
            "seed {seed}: false positive {r:#?}"
        );
    }
    for fill in [0x00u8, 0xFF, 0xC3] {
        let img = vec![fill; 0x8_0000];
        let r = run(&img);
        assert_eq!(r.image_kind, ImageKind::Blank);
        assert!(r.primary.is_none());
    }
}

#[test]
fn text_definitions_are_not_treated_as_images() {
    // A DCM-style text file that *mentions* families must not be "recognised".
    let text =
        "KENNLINIE KFMIOP 8 \n EDC17 MED17 ME7.5 SIMOS PPD1.2 TSW V2.40 280700 1718 C7/ESB/G40\n"
            .repeat(200);
    assert_eq!(image_kind(text.as_bytes()), ImageKind::Text);
    let r = run(text.as_bytes());
    assert!(r.primary.is_none(), "{r:#?}");
}

#[test]
fn winols_projects_are_containers_not_images() {
    let mut img = vec![0u8; 0x1000];
    img[4..15].copy_from_slice(b"WinOLS File");
    img[0x200..0x205].copy_from_slice(b"MS43\x00");
    assert_eq!(image_kind(&img), ImageKind::WinolsProject);
    assert!(run(&img).primary.is_none());
}

#[test]
fn sms_soft_header_tag_is_reported_at_low_confidence() {
    let mut img = vec![0x5Au8; 0x4_0000];
    img[..16].copy_from_slice(b"SMS-SoftBschM797");
    let r = run(&img);
    assert_eq!(r.image_kind, ImageKind::SmsSoftContainer);
    let p = r.primary.expect("tag names the ECU");
    assert_eq!((p.family, p.confidence), ("M7", Tier::Low));
}

#[test]
fn incompatible_top_tier_claims_are_flagged_contested() {
    // A Bosch ME7 descriptor and a Siemens MS43 ident record in one image.
    let mut img = synth::build(SynthFamily::Ms43);
    let desc = b"\x0041/1/ME7.5/3/4000.00//24C/Dst01o/010199//\x00";
    img[0x2_0000..0x2_0000 + desc.len()].copy_from_slice(desc);
    let r = run(&img);
    // ms4x() refuses images carrying "ME7." — so only the ME7 claim stands.
    assert_eq!(r.family(), "ME7");
    // Two different string families at the same tier ⇒ contested.
    let mut img2 = synth::build(SynthFamily::MarelliMjd);
    img2[0x1000..0x1018].copy_from_slice(b"\x00PPD1.2 CASN 111SN0000\x00\x00");
    let r2 = run(&img2);
    assert!(r2.contested, "{r2:#?}");
}

#[test]
fn identity_fields_carry_offsets() {
    let img = synth::build(SynthFamily::Me7);
    let ex = extract(&img);
    let id = identity(&img, &ex);
    let get = |k: &str| {
        id.iter()
            .find(|f| f.kind == k)
            .unwrap_or_else(|| panic!("{k} missing: {id:#?}"))
    };
    let hw = get("bosch_hw");
    assert_eq!((hw.value.as_str(), hw.offset), ("0261209999", 0x1_4000));
    let sw = get("bosch_sw");
    assert_eq!((sw.value.as_str(), sw.offset), ("1037999999", 0x1_400B));
    assert_eq!(get("dataset").value, "4000.00");
    assert_eq!(get("vag_part").value, "06A906099X");
    assert_eq!(get("engine").value, "1.8l R4/5VT");
    let d = get("bosch_descriptor");
    assert_eq!(d.offset, 0x1_0004);
    for f in &id {
        assert!((f.offset as usize) < img.len());
    }
}

#[test]
fn identity_reads_continental_and_ford_parts() {
    let mut img = synth::build(SynthFamily::Sid);
    img[0x7000..0x7010].copy_from_slice(b"\x006G99-12A999-XX\x00");
    let id = identity(&img, &extract(&img));
    assert!(id
        .iter()
        .any(|f| f.kind == "continental_part" && f.value == "5WS49999X-T" && f.offset == 0x6300));
    assert!(id
        .iter()
        .any(|f| f.kind == "ford_part" && f.value == "6G99-12A999-XX" && f.offset == 0x7001));
}

// ---- Real dumps (private; never committed) --------------------------------
//
//   OPENTUNE_FIXTURES=/path/to/opentune-fixtures \
//     cargo test -p opentune-scan --test recognize -- --ignored --nocapture

#[test]
#[ignore = "needs private dumps via OPENTUNE_FIXTURES"]
fn reference_dumps_are_recognised() {
    let Ok(dir) = std::env::var("OPENTUNE_FIXTURES") else {
        eprintln!("OPENTUNE_FIXTURES unset; skipping");
        return;
    };
    // (sub-directory, accepted families)
    let cases: &[(&str, &[&str])] = &[
        ("me7", &["ME7"]),
        ("edc15", &["EDC15"]),
        ("edc16-cr", &["EDC16"]),
        ("edc16-pd", &["EDC16"]),
        ("edc16-full", &["EDC16"]),
        ("edc16-pairs", &["EDC16"]),
        ("edc17", &["EDC17", "MEDC17", "MED17", "ME17"]),
        ("pcr21", &["PCR2.1"]),
    ];
    let mut checked = 0;
    for (sub, fams) in cases {
        let Ok(rd) = std::fs::read_dir(format!("{dir}/{sub}")) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue;
            }
            let img = std::fs::read(&p).unwrap();
            let r = run(&img);
            let fam = r.family();
            println!(
                "{sub}/{}: {fam} {:?} {:?}",
                p.file_name().unwrap().to_string_lossy(),
                r.primary.as_ref().and_then(|d| d.variant.clone()),
                r.primary.as_ref().map(|d| d.confidence)
            );
            assert!(fams.contains(&fam), "{}: got {fam}", p.display());
            assert!(!r.contested, "{}: contested {r:#?}", p.display());
            checked += 1;
        }
    }
    assert!(checked > 0, "no fixtures found under {dir}");
}

#[test]
fn scan_record_json_keeps_old_keys_and_adds_recognition() {
    let ctx = opentune_scan::ScanContext {
        catalog: Vec::new(),
        ecudefs: Vec::new(),
    };
    let img = synth::build(SynthFamily::Simos);
    let p = std::path::Path::new("dir/simos.bin");
    let rec = opentune_scan::scan_file(&ctx, std::path::Path::new("dir"), p, &img);
    let v = serde_json::to_value(&rec).unwrap();
    for k in [
        "path",
        "rel_path",
        "size",
        "ecu_family",
        "family_how",
        "size_class",
        "part_numbers",
        "bosch_numbers",
        "sw_numbers",
        "vin",
        "catalog_match",
        "ecudef_match",
        "assigned_core",
        "confidence_core",
        "assigned_boot",
        "confidence_boot",
    ] {
        assert!(v.get(k).is_some(), "old key {k} missing");
    }
    assert_eq!(v["ecu_family"], "SIMOS");
    assert_eq!(v["ecu_supplier"], "Siemens/Continental");
    assert_eq!(v["ecu_variant"], "SIMOS3PB");
    assert_eq!(v["family_confidence"], "high");
    assert_eq!(v["recognition"]["image_kind"], "raw");
    assert!(v["recognition"]["primary"]["evidence"][0]["offset"].is_u64());
    assert!(v["identity"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["kind"] == "continental_part"));
    let text = opentune_scan::describe(&rec).join("\n");
    assert!(text.contains("SIMOS (SIMOS3PB) [high]"), "{text}");
}
