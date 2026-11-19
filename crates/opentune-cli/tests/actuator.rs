//! `opentune adaptation actuator list|run` end to end on the virtual car
//! (`--driver vcar`) and its definition pack: the dry-run plan, a real timed
//! test with control returned, and the refusals (range, undeclared option,
//! unknown target, SecurityAccess without a handler).
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn cli(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_opentune"))
        .current_dir(root())
        .args(args)
        .output()
        .expect("run opentune");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
    )
}

fn act(def: &str, rest: &[&str]) -> (bool, String) {
    let def = format!("packs/vcar-demo/adaptation/{def}.json");
    let mut a = vec![
        "adaptation",
        "--def",
        def.as_str(),
        "--driver",
        "vcar",
        "actuator",
    ];
    a.extend_from_slice(rest);
    cli(&a)
}

#[test]
fn list_shows_declared_channels() {
    let (ok, out) = act("engine", &["list"]);
    assert!(ok, "{out}");
    assert!(out.contains("Radiator fan"), "{out}");
    assert!(out.contains("value 0..100 %"), "{out}");
    assert!(out.contains("freezeCurrentState"), "{out}");
    assert!(
        !out.contains("Idle speed target"),
        "value channels are not actuators:\n{out}"
    );
    let (ok, out) = act("airbag", &["list"]);
    assert!(ok && out.contains("SecurityAccess level 0x01"), "{out}");
}

#[test]
fn run_without_ack_is_a_plan() {
    let (ok, out) = act(
        "engine",
        &[
            "run",
            "Radiator fan",
            "--value",
            "40",
            "--duration-ms",
            "500",
        ],
    );
    assert!(ok, "{out}");
    assert!(out.contains("2F 05 00 03 28"), "{out}");
    assert!(out.contains("2F 05 00 00"), "{out}");
    assert!(out.contains("nothing was sent"), "{out}");
}

#[test]
fn run_with_ack_drives_and_returns_control() {
    let (ok, out) = act(
        "engine",
        &[
            "run",
            "Radiator fan",
            "--value",
            "55",
            "--duration-ms",
            "300",
            "--ack",
            "--port",
            "demo@cli-actuator",
        ],
    );
    assert!(ok, "{out}");
    assert!(out.contains("Outcome: completed"), "{out}");
    assert!(out.contains("control returned: yes"), "{out}");
    let (ok, out) = act(
        "body",
        &[
            "run",
            "Interior light",
            "--op",
            "freeze",
            "--duration-ms",
            "200",
            "--ack",
            "--port",
            "demo@cli-actuator",
            "--json",
        ],
    );
    assert!(ok, "{out}");
    let v: serde_json::Value = serde_json::from_str(out.trim_end_matches(|c| c != '}')).unwrap();
    assert_eq!(v["outcome"], "completed");
    assert_eq!(v["option"], "freeze_current_state");
    assert_eq!(v["control_returned"], true);
}

#[test]
fn refusals_happen_before_anything_is_sent() {
    let (ok, out) = act(
        "engine",
        &["run", "Radiator fan", "--value", "150", "--ack"],
    );
    assert!(!ok && out.contains("declared range"), "{out}");
    let (ok, out) = act("gearbox", &["run", "Shift-lock solenoid", "--op", "freeze"]);
    assert!(!ok && out.contains("not declared"), "{out}");
    let (ok, out) = act(
        "engine",
        &[
            "run",
            "Radiator fan",
            "--value",
            "10",
            "--duration-ms",
            "99999",
        ],
    );
    assert!(!ok && out.contains("limit"), "{out}");
    let (ok, out) = act("engine", &["run", "Idle speed target"]);
    assert!(!ok && out.contains("not a actuator channel"), "{out}");
    let (ok, out) = act(
        "airbag",
        &[
            "run",
            "Airbag warning lamp",
            "--ack",
            "--port",
            "demo@cli-actuator",
        ],
    );
    assert!(!ok && out.contains("SecurityAccess"), "{out}");
    // A definition without req_id/resp_id and no --tx-id/--rx-id.
    let dir = std::env::temp_dir().join(format!("ot-actuator-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut def: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root().join("packs/vcar-demo/adaptation/engine.json")).unwrap(),
    )
    .unwrap();
    def.as_object_mut().unwrap().remove("req_id");
    def.as_object_mut().unwrap().remove("resp_id");
    let p = dir.join("noaddr.json");
    std::fs::write(&p, def.to_string()).unwrap();
    let (ok, out) = cli(&[
        "adaptation",
        "--def",
        p.to_str().unwrap(),
        "--driver",
        "vcar",
        "actuator",
        "run",
        "Radiator fan",
        "--value",
        "10",
        "--ack",
    ]);
    assert!(!ok && out.contains("CAN address is unknown"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}
