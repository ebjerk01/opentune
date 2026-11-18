//! `opentune controller` end to end on the shipped definitions, with a small
//! synthetic candump log of EGS51 frames (built from the published layouts;
//! not a car capture).
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

#[test]
fn list_show_validate_match() {
    let (ok, out) = cli(&["controller", "list"]);
    assert!(ok, "{out}");
    for id in [
        "mercedes-egs5x-7226",
        "borgwarner-haldex-gen4",
        "vag-dq250-02e",
        "zf-8hp-gen1",
    ] {
        assert!(out.contains(id), "{id} missing:\n{out}");
    }
    let (ok, out) = cli(&["controller", "validate"]);
    assert!(ok, "{out}");
    assert!(out.contains("0 error(s)"), "{out}");
    let (ok, out) = cli(&["controller", "show", "722.6"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("CAN set 'egs51'") && out.contains("GS_418"),
        "{out}"
    );
    assert!(
        out.contains("upshift schedule") && out.contains("3.932"),
        "{out}"
    );
    let (ok, out) = cli(&["controller", "match", "0AY 907 554 C"]);
    assert!(ok && out.contains("borgwarner-haldex-gen4"), "{out}");
    let (ok, out) = cli(&["controller", "match", "02E300042M"]);
    assert!(ok && out.contains("vag-dq250-02e"), "{out}");
}

#[test]
fn decode_synthetic_egs51_log() {
    let dir = std::env::temp_dir().join(format!("ot-ctrl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("egs51.log");
    // GS_418: 'D' 'S', ATF 80 degC, small NAG, GZC D4 / GIC D3, loss 10 Nm, WHST D.
    // MS_308: 2500 rpm, oil 90 degC.
    std::fs::write(
        &log,
        "(1.000000) can0 418#4453820443282000\n\
         (1.010000) can0 308#0009C40000820000\n\
         (1.020000) can0 7E8#0102\n",
    )
    .unwrap();
    let log_s = log.to_str().unwrap();

    // Only the egs51 set ships: an unknown set is refused, naming the sets.
    let (ok, out) = cli(&[
        "controller",
        "decode",
        "mercedes-egs5x-7226",
        log_s,
        "--bus",
        "egs53",
    ]);
    assert!(!ok && out.contains("egs51"), "{out}");

    // One set: no --bus needed.
    let (ok, out) = cli(&["controller", "decode", "722.6", log_s, "--json", "--frames"]);
    assert!(ok, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let s = &v["summary"];
    assert_eq!(s["frames"], 3);
    assert_eq!(s["decoded"], 2);
    assert_eq!(s["live"]["ATF temperature"]["max"], 80.0);
    assert_eq!(s["live"]["engine speed"]["max"], 2500.0);
    assert_eq!(s["live"]["engine oil temperature"]["min"], 90.0);
    let gs = &s["per_frame"]["GS_418"]["signals"];
    assert_eq!(gs["GIC"]["counts"]["D3"], 1);
    assert_eq!(gs["GZC"]["counts"]["D4"], 1);
    assert_eq!(gs["MECH"]["counts"]["small NAG"], 1);
    assert_eq!(gs["M_VERL"]["max"], 10.0);
    assert_eq!(gs["FSC"]["counts"]["'D'"], 1);
    assert_eq!(v["frames"].as_array().unwrap().len(), 2);

    let (ok, out) = cli(&["controller", "decode", "722.6", log_s, "--bus", "egs51"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("ATF temperature") && out.contains("80 .. 80 degC"),
        "{out}"
    );
    assert!(out.contains("synthetic-tested"), "{out}");
    std::fs::remove_dir_all(&dir).ok();
}
