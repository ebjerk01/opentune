//! The access profile (`access` block, docs/UNIT-ACCESS-PROFILE.md): schema
//! round-trip, one test per validator rule, the shipped profiles, and the
//! `.access-pending` gate behind `opentune db validate --require-access`.
//! Synthetic units only, apart from the repository checks.

use opentune_db::{
    ecudef_ids, load_access_pending, Database, ObdAccess, Unit, ACCESS_PENDING_FILE, NO_KEY_PHRASE,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A fresh, empty temp directory per call.
fn tmp() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!(
        "opentune-db-access-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::remove_dir_all(&d).ok();
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A valid synthetic unit with a complete access profile and no pinout.
fn base() -> Value {
    json!({
      "id": "test-unit", "kind": "ecu", "maker": "X", "family": "Y", "name": "Test unit",
      "aliases": [], "cpu": "C167CR", "flash_kib": 512,
      "flash_note": "29F400 (512 KiB) or 29F800 (1 MiB) by revision - confirm from the chip",
      "eeprom": null, "bus": ["kline"], "protocols": ["kwp2000"],
      "read_methods": {"obd": "read", "bench": false, "boot": true, "notes": "n"},
      "checksum": null, "ecudef": null, "vehicles": [], "part_number_examples": [],
      "notes": "No pinout: synthetic unit.", "connectors": [], "bench": null,
      "pinout_confidence": null, "safety_note": null,
      "sources": [
        {"title": "Datasheet A", "url": "https://example.org/a.pdf"},
        {"title": "Forum B", "url": "https://example.org/b"}
      ],
      "access": {
        "obd": {"mode": "read", "protocol": "KWP2000 on K-line", "notes": "n", "confidence": "community", "sources": [1]},
        "bench": {"possible": false, "how": "not documented", "confidence": "unverified", "sources": [1]},
        "boot": {
          "possible": true, "entry": "board-pad", "mcu_mode": "C167 BSL on ASC0",
          "points": [{"name": "P0L.4", "location": "pcb:flash DQ4", "action": "pull-to-gnd",
                      "resistor_ohms": 8000, "note": "confirm by continuity", "confidence": "unverified"}],
          "reset_timing": "hold through reset", "reachable": "open-case", "notes": "n",
          "confidence": "verified", "sources": [0, 1]
        },
        "security": {"diag_read": "required", "flash_write": "unknown",
                     "notes": format!("Security access needed. {NO_KEY_PHRASE}."),
                     "confidence": "community", "sources": [1]}
      }
    })
}

/// Validate one unit (written to a temp database directory) and return the
/// messages.
fn issues(v: &Value) -> Vec<String> {
    let dir = tmp();
    let id = v["id"].as_str().unwrap();
    std::fs::write(dir.join(format!("{id}.json")), v.to_string()).unwrap();
    let db = Database::load_dir(&dir).expect("unit loads");
    let out = db.validate(None).into_iter().map(|i| i.msg).collect();
    std::fs::remove_dir_all(&dir).ok();
    out
}

fn assert_issue(v: &Value, needle: &str) {
    let m = issues(v);
    assert!(
        m.iter().any(|s| s.contains(needle)),
        "expected '{needle}' in {m:?}"
    );
}

#[test]
fn base_unit_is_valid() {
    assert_eq!(issues(&base()), Vec::<String>::new());
}

#[test]
fn schema_round_trip() {
    let raw = base();
    let u: Unit = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(u.read_methods.obd, ObdAccess::Read);
    assert!(u.access.is_some());
    // Serialising the typed unit gives the stored JSON back.
    assert_eq!(serde_json::to_value(&u).unwrap(), raw);
    // A boot pin with a strap round-trips too; optional fields stay absent.
    let mut v = base();
    v["connectors"] = json!([{"name": "A", "description": null, "pins": [
        {"pin": "7", "function": "Boot", "signal": "boot", "note": null, "confidence": null,
         "sources": [0], "strap": {"action": "pull-to-gnd", "resistor_ohms": 4700}},
        {"pin": "8", "function": "Gnd", "signal": "gnd", "note": null, "confidence": null, "sources": [0]}]}]);
    let u: Unit = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(serde_json::to_value(&u).unwrap(), v);
    // A unit without the new optional fields keeps them absent.
    let mut v = base();
    v.as_object_mut().unwrap().remove("access");
    v.as_object_mut().unwrap().remove("flash_note");
    let u: Unit = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(serde_json::to_value(&u).unwrap(), v);
}

#[test]
fn no_field_for_key_material() {
    for (part, field) in [
        ("security", "key"),
        ("security", "seed"),
        ("security", "algorithm"),
        ("boot", "password"),
    ] {
        let mut v = base();
        v["access"][part][field] = json!("00 11 22 33");
        assert!(
            serde_json::from_value::<Unit>(v).is_err(),
            "access.{part}.{field} must not be accepted"
        );
    }
    let mut v = base();
    v["access"]["immobiliser"] = json!({});
    assert!(serde_json::from_value::<Unit>(v).is_err());
}

#[test]
fn rule_read_methods_agree_with_access() {
    let mut v = base();
    v["read_methods"]["obd"] = json!("read+write");
    assert_issue(
        &v,
        "access.obd.mode 'read' disagrees with read_methods.obd 'read+write'",
    );
    let mut v = base();
    v["read_methods"]["bench"] = json!(true);
    assert_issue(&v, "access.bench.possible (false) disagrees");
    let mut v = base();
    v["read_methods"]["boot"] = json!(false);
    assert_issue(&v, "access.boot.possible (true) disagrees");
}

#[test]
fn rule_every_part_cites_a_source() {
    for part in ["obd", "bench", "boot", "security"] {
        let mut v = base();
        v["access"][part]["sources"] = json!([]);
        if part == "boot" {
            v["access"]["boot"]["confidence"] = json!("community");
        }
        assert_issue(&v, &format!("access.{part}: must cite at least one source"));
        let mut v = base();
        v["access"][part]["sources"] = json!([9]);
        assert_issue(&v, &format!("access.{part}: source index 9 out of range"));
    }
}

#[test]
fn rule_verified_needs_two_sources() {
    let mut v = base();
    v["access"]["obd"]["confidence"] = json!("verified");
    assert_issue(&v, "access.obd: 'verified' needs 2+ independent sources");
    // The same index twice is still one source.
    let mut v = base();
    v["access"]["obd"]["confidence"] = json!("verified");
    v["access"]["obd"]["sources"] = json!([1, 1]);
    assert_issue(&v, "access.obd: 'verified' needs 2+ independent sources");
    let mut v = base();
    v["access"]["boot"]["sources"] = json!([0]);
    v["access"]["boot"]["confidence"] = json!("community");
    v["access"]["boot"]["points"][0]["confidence"] = json!("verified");
    assert_issue(&v, "'verified' needs 2+ independent boot sources");
}

#[test]
fn rule_boot_possible_needs_point_entry_and_reachable() {
    let mut v = base();
    v["access"]["boot"]["points"] = json!([]);
    assert_issue(&v, "possible=true needs at least one boot point");
    let mut v = base();
    v["access"]["boot"]["reachable"] = json!("unknown");
    assert_issue(&v, "possible=true needs 'reachable'");
    let mut v = base();
    v["access"]["boot"]["entry"] = json!("unknown");
    assert_issue(&v, "possible=true needs a known 'entry'");
    // Not documented: "unknown" with entry/reachable unknown and no points.
    let mut v = base();
    v["read_methods"]["boot"] = json!(false);
    v["access"]["boot"] = json!({"possible": "unknown", "entry": "unknown", "reachable": "unknown",
        "notes": "No public source documents a boot mode.", "confidence": "unverified", "sources": [1]});
    assert_eq!(issues(&v), Vec::<String>::new());
    let mut w = v.clone();
    w["access"]["boot"]["entry"] = json!("board-pad");
    assert_issue(&w, "possible \"unknown\" needs entry 'unknown'");
    let mut w = v.clone();
    w["access"]["boot"]["reachable"] = json!("open-case");
    assert_issue(&w, "possible \"unknown\" needs reachable 'unknown'");
    let mut w = v.clone();
    w["access"]["boot"]["points"] = base()["access"]["boot"]["points"].clone();
    assert_issue(&w, "possible \"unknown\" cannot list boot points");
    // "unknown" agrees with read_methods.boot = false, never with true.
    let mut w = v.clone();
    w["read_methods"]["boot"] = json!(true);
    assert_issue(&w, "access.boot.possible (unknown) disagrees");
    // No route: false needs entry 'none', no points, reachable unknown.
    let mut v = base();
    v["read_methods"]["boot"] = json!(false);
    v["access"]["boot"] = json!({"possible": false, "entry": "none", "reachable": "unknown",
        "notes": "The datasheet says the boot pins are fused off.", "confidence": "community", "sources": [1]});
    assert_eq!(issues(&v), Vec::<String>::new());
    let mut w = v.clone();
    w["access"]["boot"]["entry"] = json!("unknown");
    assert_issue(&w, "possible=false (no boot route) needs entry 'none'");
    v["access"]["boot"]["points"] = base()["access"]["boot"]["points"].clone();
    assert_issue(&v, "possible=false cannot list boot points");
}

#[test]
fn possible_reads_bools_and_unknown_and_rejects_other_strings() {
    use opentune_db::Possible;
    assert_eq!(
        serde_json::from_value::<Possible>(json!(true)).unwrap(),
        Possible::Yes
    );
    assert_eq!(
        serde_json::from_value::<Possible>(json!(false)).unwrap(),
        Possible::No
    );
    assert_eq!(
        serde_json::from_value::<Possible>(json!("unknown")).unwrap(),
        Possible::Unknown
    );
    assert!(serde_json::from_value::<Possible>(json!("maybe")).is_err());
    for p in [Possible::Yes, Possible::No, Possible::Unknown] {
        let back: Possible = serde_json::from_value(serde_json::to_value(p).unwrap()).unwrap();
        assert_eq!(back, p);
    }
    assert_eq!(
        serde_json::to_value(Possible::Unknown).unwrap(),
        json!("unknown")
    );
    assert_eq!(serde_json::to_value(Possible::Yes).unwrap(), json!(true));
}

#[test]
fn rule_boot_point_locations() {
    let mut v = base();
    v["access"]["boot"]["points"][0]["location"] = json!("pin 24");
    assert_issue(
        &v,
        "location must be 'connector:<connector>-<pin>' or 'pcb:<label>'",
    );
    let mut v = base();
    v["access"]["boot"]["points"][0]["location"] = json!("pcb: ");
    assert_issue(&v, "'pcb:' needs a label");
    let mut v = base();
    v["access"]["boot"]["reachable"] = json!("external-connector");
    assert_issue(
        &v,
        "a board point cannot be reachable from the external connector",
    );
    let mut v = base();
    v["access"]["boot"]["points"][0]["location"] = json!("connector:A-9");
    assert_issue(&v, "'connector:A-9' does not resolve to a pin");
    let mut v = base();
    v["access"]["boot"]["points"][0]["resistor_ohms"] = json!(0);
    assert_issue(&v, "resistor_ohms must be > 0");
    let mut v = base();
    v["access"]["boot"]["confidence"] = json!("unverified");
    v["access"]["boot"]["points"][0]["confidence"] = json!("community");
    assert_issue(
        &v,
        "confidence 'community' is higher than access.boot's 'unverified'",
    );
}

#[test]
fn rule_connector_boot_point_must_be_a_boot_pin() {
    let mut v = base();
    v["connectors"] = json!([{"name": "A", "pins": [
        {"pin": "1", "function": "Ground", "signal": "gnd", "sources": [0, 1]},
        {"pin": "2", "function": "+12 V", "signal": "12v_perm", "sources": [0, 1]},
        {"pin": "7", "function": "Boot", "signal": "boot", "sources": [0, 1],
         "strap": {"action": "pull-to-gnd", "resistor_ohms": 1000}}]}]);
    v["bench"] = json!({"power": ["A-2"], "ground": ["A-1"], "notes": "n"});
    v["pinout_confidence"] = json!("verified");
    v["safety_note"] = json!("Bench safety.");
    v["access"]["boot"]["entry"] = json!("connector-pin");
    v["access"]["boot"]["reachable"] = json!("external-connector");
    v["access"]["boot"]["points"][0]["location"] = json!("connector:A-7");
    assert_eq!(issues(&v), Vec::<String>::new());
    v["access"]["boot"]["points"][0]["location"] = json!("connector:A-1");
    assert_issue(&v, "'connector:A-1' is not a 'boot' pin");
}

#[test]
fn rule_connect_is_for_debug_probe_entries_only() {
    let mut v = base();
    v["access"]["boot"]["points"][0]["action"] = json!("connect");
    assert_issue(
        &v,
        "action 'connect' (attach a debug probe) needs entry 'bdm-jtag'",
    );
    v["access"]["boot"]["entry"] = json!("bdm-jtag");
    assert_eq!(issues(&v), Vec::<String>::new());

    let mut v = base();
    v["connectors"] = json!([{"name": "A", "pins": [
        {"pin": "7", "function": "Boot", "signal": "boot", "sources": [0],
         "strap": {"action": "connect"}}]}]);
    assert_issue(
        &v,
        "A-7: strap.action 'connect' is for bdm-jtag boot points, not a pin strap",
    );
}

#[test]
fn rule_strap_only_on_boot_pins() {
    let mut v = base();
    v["connectors"] = json!([{"name": "A", "pins": [
        {"pin": "1", "function": "Ground", "signal": "gnd", "sources": [0],
         "strap": {"action": "pull-to-gnd"}}]}]);
    assert_issue(&v, "A-1: strap is only allowed on a 'boot' pin");
}

#[test]
fn rule_security_states_no_key_is_shipped() {
    let mut v = base();
    v["access"]["security"]["notes"] = json!("Security access is needed.");
    assert_issue(&v, "access.security.notes must state");
    // Case does not matter.
    v["access"]["security"]["notes"] = json!(NO_KEY_PHRASE.to_uppercase());
    assert_eq!(issues(&v), Vec::<String>::new());
}

#[test]
fn rule_texts_not_empty() {
    let mut v = base();
    v["access"]["bench"]["how"] = json!(" ");
    assert_issue(&v, "access.bench.how must not be empty");
    let mut v = base();
    v["flash_note"] = json!("");
    assert_issue(&v, "flash_note must not be empty");
}

// ---------------------------------------------------------------------------
// Shipped data and the .access-pending gate
// ---------------------------------------------------------------------------

const FILLED: [&str; 4] = [
    "bosch-edc15c6-mercedes",
    "mercedes-egs52-7226",
    "temic-dq250-vag",
    "haldex-gen4",
];

#[test]
fn filled_units_validate() {
    let db = Database::load_dir(&repo().join("database")).unwrap();
    let ids = ecudef_ids(&repo().join("ecudefs")).unwrap();
    let issues = db.validate(Some(&ids));
    assert!(issues.is_empty(), "{issues:?}");
    for id in FILLED {
        let u = db.unit(id).unwrap_or_else(|| panic!("{id} missing"));
        let a = u
            .access
            .as_ref()
            .unwrap_or_else(|| panic!("{id}: no access"));
        assert!(a.security.notes.contains(NO_KEY_PHRASE), "{id}");
    }
    // The worked example: board-level boot through the flash's DQ4 pin.
    let a = db
        .unit("bosch-edc15c6-mercedes")
        .unwrap()
        .access
        .clone()
        .unwrap();
    assert!(a.boot.possible.is_yes());
    assert_eq!(
        serde_json::to_value(a.boot.reachable).unwrap(),
        json!("open-case")
    );
    assert!(a.boot.points.iter().any(|p| p.location.contains("DQ4")
        && serde_json::to_value(p.confidence).unwrap() == json!("unverified")));
}

#[test]
fn repository_access_gate() {
    let dir = repo().join("database");
    let db = Database::load_dir(&dir).unwrap();
    let pending = load_access_pending(&dir).unwrap();
    let gate = db.access_gate(&pending);
    assert!(gate.errors.is_empty(), "{:?}", gate.errors);
    assert_eq!(gate.warnings.len(), pending.ids.len());
    // The list only shrinks: 16 units predate the profile. Lower this number
    // when you fill one; never raise it (a new unit must carry `access`).
    assert!(
        pending.ids.len() <= 16,
        "{ACCESS_PENDING_FILE} grew to {} ids",
        pending.ids.len()
    );
    for id in FILLED {
        assert!(
            !pending.ids.contains(id),
            "{id} is filled but still pending"
        );
    }
}

fn write_unit(dir: &Path, v: &Value) {
    let id = v["id"].as_str().unwrap();
    std::fs::write(dir.join(format!("{id}.json")), v.to_string()).unwrap();
}

#[test]
fn access_gate_behaviour() {
    let dir = tmp();
    let filled = base();
    let mut old = base();
    old["id"] = json!("old-unit");
    old.as_object_mut().unwrap().remove("access");
    let mut new = old.clone();
    new["id"] = json!("new-unit");
    let mut done = base();
    done["id"] = json!("done-unit");
    for v in [&filled, &old, &new, &done] {
        write_unit(&dir, v);
    }
    std::fs::write(
        dir.join(ACCESS_PENDING_FILE),
        "# comment\nold-unit\ndone-unit  # filled, should be removed\nghost-unit\n",
    )
    .unwrap();
    let db = Database::load_dir(&dir).unwrap();
    let pending = load_access_pending(&dir).unwrap();
    assert_eq!(pending.ids.len(), 3);
    let gate = db.access_gate(&pending);
    let warn: Vec<_> = gate.warnings.iter().map(|i| i.unit.as_str()).collect();
    assert_eq!(warn, ["old-unit"], "a listed, unfilled unit only warns");
    let err = |unit: &str, needle: &str| {
        assert!(
            gate.errors
                .iter()
                .any(|i| i.unit == unit && i.msg.contains(needle)),
            "{unit}: '{needle}' in {:?}",
            gate.errors
        )
    };
    err("new-unit", "a new unit must carry `access`");
    err("done-unit", "remove it from .access-pending");
    err("ghost-unit", "stale .access-pending entry");
    assert_eq!(gate.errors.len(), 3, "{:?}", gate.errors);

    // Missing file = empty list: every unfilled unit is an error.
    std::fs::remove_file(dir.join(ACCESS_PENDING_FILE)).unwrap();
    let pending = load_access_pending(&dir).unwrap();
    assert!(pending.ids.is_empty());
    let gate = db.access_gate(&pending);
    assert_eq!(gate.errors.len(), 2);
    assert!(gate.warnings.is_empty());

    // A duplicate id in the list is refused.
    std::fs::write(dir.join(ACCESS_PENDING_FILE), "old-unit\nold-unit\n").unwrap();
    assert!(load_access_pending(&dir).is_err());
    std::fs::remove_dir_all(&dir).ok();
}
