//! Every shipped `ctrldefs/*.json` parses, validates without errors, is named
//! after its id, and its links (ecudefs, database units, checksum ids, ecudef
//! maps) resolve. Also checks the JSON schema in `docs/ctrldef.schema.json`
//! names exactly the fields the loader knows.
use opentune_ctrldef::{load_dir, ControllerDef, LinkContext, Severity};
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn link_context() -> LinkContext {
    let ecudefs = opentune_core::ecudef::load_dir(&root().join("ecudefs")).expect("ecudefs");
    let mut unit_ids = Vec::new();
    for e in std::fs::read_dir(root().join("database/units")).expect("database/units") {
        let p = e.unwrap().path();
        if p.extension().and_then(|s| s.to_str()) == Some("json") {
            let v: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
            unit_ids.push(v["id"].as_str().unwrap().to_string());
        }
    }
    let checksum_ids = opentune_checksum::algorithms()
        .into_iter()
        .map(|a| a.id.to_string())
        .collect();
    LinkContext {
        ecudefs,
        unit_ids,
        checksum_ids,
    }
}

fn shipped() -> Vec<(PathBuf, ControllerDef)> {
    load_dir(&root().join("ctrldefs")).expect("every ctrldefs/*.json parses")
}

#[test]
fn every_shipped_definition_is_valid_and_linked() {
    let defs = shipped();
    assert!(
        defs.len() >= 8,
        "expected the shipped set, got {}",
        defs.len()
    );
    let ctx = link_context();
    let mut errors = Vec::new();
    for (path, d) in &defs {
        let stem = path.file_stem().unwrap().to_str().unwrap();
        let issues: Vec<_> = d
            .validate()
            .into_iter()
            .chain(d.validate_filename(stem))
            .chain(d.validate_links(&ctx))
            .collect();
        for i in issues.iter().filter(|i| i.severity == Severity::Error) {
            errors.push(format!("{}: {i}", path.display()));
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn ids_are_unique() {
    let defs = shipped();
    let mut ids: Vec<&str> = defs.iter().map(|(_, d)| d.id.as_str()).collect();
    let n = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), n);
}

#[test]
fn migrated_definitions_keep_their_ecudef_facts() {
    // The controller views of the existing DSG/Haldex ecudefs must agree with
    // them on identity and diagnostics, so nothing drifts in the migration.
    let ctx = link_context();
    for (_, d) in shipped() {
        for e in &d.ecudefs {
            let ecu = ctx.ecudefs.iter().find(|x| &x.id == e).unwrap();
            for pn in &ecu.identify.part_numbers {
                assert!(
                    d.matches(pn),
                    "{}: ecudef {e} part number {pn} not matched by the controller def",
                    d.id
                );
            }
            let ecu_ch = d.diagnostics.iter().any(|c| {
                c.transport == ecu.protocol
                    && c.layer == ecu.diag_layer
                    && match ecu.protocol {
                        opentune_core::Protocol::Vwtp20 => {
                            c.tp20_address == Some(ecu.tx_id as u8) && c.rx_id == Some(ecu.rx_id)
                        }
                        _ => c.tx_id == Some(ecu.tx_id) && c.rx_id == Some(ecu.rx_id),
                    }
            });
            assert!(
                ecu_ch,
                "{}: no diagnostics channel matches ecudef {e} ({:?}/{:?} 0x{:X}/0x{:X})",
                d.id, ecu.protocol, ecu.diag_layer, ecu.tx_id, ecu.rx_id
            );
        }
    }
}

/// docs/CONTROLLER-DEFS.md documents every rule code, every top-level field and
/// every shipped definition (doc-drift gate: document new items, don't loosen).
#[test]
fn guide_documents_rules_fields_and_shipped_defs() {
    let doc = std::fs::read_to_string(root().join("docs/CONTROLLER-DEFS.md")).unwrap();
    let src =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/validate.rs"))
            .unwrap();
    // Rule codes are the `"area.rule"` string literals in validate.rs.
    let mut codes: Vec<&str> = src
        .split('"')
        .skip(1)
        .step_by(2)
        .filter(|s| {
            let mut parts = s.split('.');
            matches!((parts.next(), parts.next(), parts.next()), (Some(a), Some(b), None)
                if !a.is_empty() && !b.is_empty()
                    && s.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'.'))
        })
        .collect();
    codes.sort_unstable();
    codes.dedup();
    assert!(codes.len() > 40, "rule-code scan found only {codes:?}");
    let schema: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root().join("docs/ctrldef.schema.json")).unwrap(),
    )
    .unwrap();
    let fields = schema_props(&schema);
    let ids: Vec<String> = shipped().into_iter().map(|(_, d)| d.id).collect();
    let missing: Vec<String> = codes
        .iter()
        .map(|s| s.to_string())
        .chain(fields)
        .chain(ids)
        .filter(|c| !doc.contains(&format!("`{c}`")))
        .collect();
    assert!(
        missing.is_empty(),
        "missing from docs/CONTROLLER-DEFS.md: {missing:?}"
    );
}

/// Property names declared under a schema object (`properties` keys).
fn schema_props(v: &serde_json::Value) -> Vec<String> {
    let mut k: Vec<String> = v["properties"]
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    k.sort();
    k
}

#[test]
fn schema_matches_the_loader() {
    let schema: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root().join("docs/ctrldef.schema.json")).expect("schema"),
    )
    .expect("schema is JSON");
    // Top level: serialise the fullest shipped definition and compare keys.
    let (_, d) = shipped()
        .into_iter()
        .find(|(_, d)| d.id == "mercedes-egs5x-7226")
        .unwrap();
    let v = serde_json::to_value(&d).unwrap();
    let mut keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(schema_props(&schema), keys, "top-level properties");
    // Each sub-object definition lists exactly the struct's fields.
    let defs = &schema["definitions"];
    let sample = |name: &str| -> Vec<String> { schema_props(&defs[name]) };
    let fields = |v: &serde_json::Value| -> Vec<String> {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    // Build full examples so every optional field is present.
    let signal = serde_json::json!({"name":"s","start":0,"len":8,"byte_order":"little","signed":false,
        "kind":"number","factor":2.0,"offset":1.0,"curve":[[0.0,0.0],[1.0,1.0]],"unit":"u","invalid":[1],
        "raw_range":[0,9],"values":[{"raw":1,"name":"a"}],"notes":""});
    let s: opentune_ctrldef::Signal = serde_json::from_value(signal).unwrap();
    assert_eq!(
        sample("signal"),
        fields(&serde_json::to_value(&s).unwrap()),
        "signal"
    );
    let frame = &v["buses"][0]["frames"][0];
    let mut want = fields(frame);
    for k in ["extended", "min_len", "cycle_ms", "validation"] {
        if !want.contains(&k.to_string()) {
            want.push(k.to_string());
        }
    }
    want.sort();
    assert_eq!(sample("frame"), want, "frame");
    assert_eq!(sample("bus"), fields(&v["buses"][0]), "bus");
    // The 722.6 definition declares no diagnostic channel; take Haldex Gen4's.
    let (_, gen4) = shipped()
        .into_iter()
        .find(|(_, d)| d.id == "borgwarner-haldex-gen4")
        .unwrap();
    let g4 = serde_json::to_value(&gen4).unwrap();
    assert_eq!(
        sample("diagChannel"),
        fields(&g4["diagnostics"][0]),
        "diagChannel"
    );
    assert_eq!(sample("table"), fields(&v["tables"][0]), "table");
    assert_eq!(sample("gearSet"), fields(&v["gearsets"][0]), "gearSet");
    assert_eq!(
        sample("adaptation"),
        fields(&v["adaptations"][0]),
        "adaptation"
    );
    assert_eq!(sample("source"), fields(&v["sources"][0]), "source");
    assert_eq!(sample("identify"), fields(&v["identify"]), "identify");
}
