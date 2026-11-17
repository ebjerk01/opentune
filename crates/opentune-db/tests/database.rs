//! Validates the shipped `database/units/*.json` against the schema rules:
//! every file parses with the strict serde model, ids are unique, checksum ids
//! resolve in `opentune_checksum`, `ecudef` references exist in `ecudefs/`, and
//! the pinout honesty rules hold. Also keeps docs/database-unit.schema.json in
//! sync with the Rust model.

use opentune_db::{ecudef_ids, Database, Signal};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::PathBuf;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn db() -> Database {
    Database::load_dir(&repo().join("database")).expect("database loads (parse + unique ids)")
}

#[test]
fn every_unit_validates() {
    let db = db();
    assert!(
        db.entries().len() >= 15,
        "expected the core families to be present"
    );
    let ids = ecudef_ids(&repo().join("ecudefs")).unwrap();
    let issues = db.validate(Some(&ids));
    assert!(
        issues.is_empty(),
        "database validation failed:\n{}",
        issues
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn ids_unique_and_refs_resolve() {
    let db = db();
    let mut seen = BTreeSet::new();
    let ecudefs = ecudef_ids(&repo().join("ecudefs")).unwrap();
    for e in db.entries() {
        let u = &e.unit;
        assert!(seen.insert(u.id.clone()), "duplicate id {}", u.id);
        if let Some(c) = &u.checksum {
            assert!(
                opentune_checksum::get(c).is_some(),
                "{}: checksum {c}",
                u.id
            );
        }
        if let Some(d) = &u.ecudef {
            assert!(ecudefs.contains(d), "{}: ecudef {d}", u.id);
        }
    }
}

#[test]
fn core_families_are_covered() {
    let db = db();
    for q in [
        "EDC16U34",
        "EDC16C31",
        "EDC17CP14",
        "PCR2.1",
        "EDC15P",
        "EDC15C6",
        "ME7.5",
        "MS41",
        "MS42",
        "MS43",
        "DQ250",
        "DQ381",
        "8HP",
        "Haldex Gen4",
        "Haldex Gen5",
        "722.6",
    ] {
        assert!(!db.list(None, Some(q)).is_empty(), "no unit matches {q}");
    }
}

#[test]
fn list_contract_filters() {
    let db = db();
    let all = db.list(None, None);
    assert_eq!(all.len(), db.entries().len());
    assert_eq!(db.list(Some(""), Some("  ")).len(), all.len());
    let awd = db.list(Some("AWD"), None);
    assert!(!awd.is_empty() && awd.iter().all(|r| r.kind.as_str() == "awd"));
    // Vehicle and part-number search, case-insensitive.
    assert!(db.list(None, Some("golf")).len() > 3);
    assert!(db
        .list(None, Some("02e927770aj"))
        .iter()
        .any(|r| r.id == "temic-dq250-vag"));
    assert!(db
        .list(None, Some("tc1796 vag"))
        .iter()
        .any(|r| r.id == "bosch-edc17cp14-vag"));
    let row = db.list(None, Some("bosch-edc16u34-vag")).remove(0);
    assert!(row.has_pinout && row.vehicle_count > 0);
}

#[test]
fn get_returns_stored_json() {
    let db = db();
    let e = &db.entries()[0];
    let on_disk: Value = serde_json::from_str(&std::fs::read_to_string(&e.path).unwrap()).unwrap();
    assert_eq!(db.get(&e.unit.id), Some(&on_disk));
    assert!(db.get("does-not-exist").is_none());
}

#[test]
fn validator_catches_dishonest_pinouts() {
    // A unit that claims 'verified' on a single-source bench pin, wires an
    // unverified pin and points at an unknown checksum must be rejected.
    let text = r#"{
      "id":"bad-unit","kind":"ecu","maker":"X","family":"Y","name":"Z","cpu":null,"flash_kib":null,
      "eeprom":null,"bus":["can"],"protocols":[],
      "read_methods":{"obd":"unknown","bench":true,"boot":false,"notes":"n"},
      "checksum":"no-such-algo","ecudef":"no-such-def","vehicles":[],"part_number_examples":[],"notes":"n",
      "connectors":[{"name":"A","pins":[
        {"pin":"1","function":"gnd","signal":"gnd","sources":[0]},
        {"pin":"2","function":"+12","signal":"12v_perm","confidence":"unverified","sources":[0]}]}],
      "bench":{"power":["A-2"],"ground":["A-1"],"kline":["A-9"],"notes":"n"},
      "pinout_confidence":"verified","safety_note":null,
      "sources":[{"title":"t","url":"https://example.org"}]
    }"#;
    let dir = std::env::temp_dir().join(format!("opentune-db-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("bad-unit.json"), text).unwrap();
    let db = Database::load_dir(&dir).unwrap();
    let msgs: Vec<String> = db
        .validate(Some(&BTreeSet::new()))
        .into_iter()
        .map(|i| i.msg)
        .collect();
    std::fs::remove_dir_all(&dir).ok();
    let has = |needle: &str| msgs.iter().any(|m| m.contains(needle));
    assert!(has("is not an opentune_checksum id"), "{msgs:?}");
    assert!(has("not found in ecudefs"), "{msgs:?}");
    assert!(has("needs 2+ independent sources"), "{msgs:?}");
    assert!(has("must not be wired"), "{msgs:?}");
    assert!(has("does not resolve"), "{msgs:?}");
    assert!(has("safety_note"), "{msgs:?}");
}

#[test]
fn unknown_fields_are_rejected() {
    let dir = std::env::temp_dir().join(format!("opentune-db-test-uf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut v: Value = serde_json::from_str(
        &std::fs::read_to_string(repo().join("database/units/bosch-edc15-vag.json")).unwrap(),
    )
    .unwrap();
    v["pinuot_confidence"] = Value::from("verified");
    std::fs::write(dir.join("bosch-edc15-vag.json"), v.to_string()).unwrap();
    let r = Database::load_dir(&dir);
    std::fs::remove_dir_all(&dir).ok();
    assert!(r.is_err(), "typo'd field must not load silently");
}

#[test]
fn schema_matches_the_rust_model() {
    let schema: Value = serde_json::from_str(
        &std::fs::read_to_string(repo().join("docs/database-unit.schema.json")).unwrap(),
    )
    .expect("schema is valid JSON");
    let sig: Vec<&str> = schema["$defs"]["pin"]["properties"]["signal"]["enum"]
        .as_array()
        .expect("signal enum")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(sig, Signal::ALL);
    let required: BTreeSet<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    // Every stored unit carries exactly the schema's top-level properties.
    let props: BTreeSet<&str> = schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    for e in db().entries() {
        let keys: BTreeSet<&str> = e
            .raw
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert!(
            required.is_subset(&keys),
            "{}: missing required keys",
            e.unit.id
        );
        assert!(keys.is_subset(&props), "{}: key not in schema", e.unit.id);
    }
}
