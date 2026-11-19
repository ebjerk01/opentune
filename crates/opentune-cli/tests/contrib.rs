//! The contributor checks, end to end through the binary: `packs validate`,
//! `ecudefs validate --require-provenance` and `privacy-scan`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

fn run(args: &[&str]) -> (Option<i32>, String) {
    let out: Output = Command::new(env!("CARGO_BIN_EXE_opentune"))
        .args(args)
        .output()
        .expect("run opentune");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
    )
}

fn temp_dir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("opentune-contrib-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn write(dir: &Path, rel: &str, text: &str) -> PathBuf {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, text).unwrap();
    p
}

/// Assembled at run time so this file stays clean under the scanner.
fn vin_like() -> String {
    ["WVW", "ZZZ", "3CZ", "9E2", "58317"].concat()
}

#[test]
fn shipped_packs_and_ecudefs_pass_the_contributor_gates() {
    let packs = repo("packs");
    let (code, out) = run(&["packs", "validate", packs.to_str().unwrap()]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("0 error(s)"), "{out}");

    let defs = repo("ecudefs");
    let (code, out) = run(&[
        "ecudefs",
        "validate",
        defs.to_str().unwrap(),
        "--require-provenance",
    ]);
    assert_eq!(code, Some(0), "{out}");

    // The plain listing still works next to the new subcommand.
    let (code, out) = run(&["packs", packs.to_str().unwrap()]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("pack generic-obd2"), "{out}");
}

#[test]
fn packs_validate_reports_provenance_and_privacy_errors() {
    let root = temp_dir("packs");
    let pack = root.join("my-pack");
    write(
        &pack,
        "pack.json",
        r#"{"id":"my-pack","name":"Mine","version":"0.1.0"}"#,
    );
    write(&pack, "modules/m.json", r#"{"vehicle":"Any","modules":[]}"#);
    write(&pack, "NOTES.md", &format!("read from {}\n", vin_like()));
    let (code, out) = run(&["packs", "validate", root.to_str().unwrap()]);
    assert_eq!(code, Some(2), "{out}");
    for want in [
        "provenance.sources_missing",
        "provenance.license_missing",
        "provenance.evidence_missing",
        "privacy.vin",
        "NOTES.md:1:11",
    ] {
        assert!(out.contains(want), "{want} missing from:\n{out}");
    }
    assert!(
        !out.contains(&vin_like()),
        "findings must be redacted:\n{out}"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn ecudefs_validate_requires_provenance_only_when_asked() {
    let dir = temp_dir("ecudefs");
    let def = write(
        &dir,
        "bosch-demo.json",
        r#"{"id":"bosch-demo","name":"Demo","maker":"Bosch","family":"DEMO",
            "protocol":"iso_tp","diag_layer":"uds","bitrate":500000,
            "identify":{"part_numbers":["0281011"]}}"#,
    );
    let p = def.to_str().unwrap();
    let (code, out) = run(&["ecudefs", "validate", p]);
    assert_eq!(code, Some(0), "legacy file only warns:\n{out}");
    let (code, out) = run(&["ecudefs", "validate", p, "--require-provenance"]);
    assert_eq!(code, Some(2), "{out}");
    assert!(out.contains("provenance.missing"), "{out}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn privacy_scan_finds_redacts_and_honours_the_allow_list() {
    let dir = temp_dir("scan");
    write(
        &dir,
        "clean/a.json",
        r#"{"vin":"WVWZZZ00000000000","by":"dev@example.com"}"#,
    );
    write(&dir, "clean/blob.bin", "not scanned by extension");
    let (code, out) = run(&["privacy-scan", dir.join("clean").to_str().unwrap()]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("1 file(s) scanned"), "{out}");

    let dirty = write(&dir, "dirty/b.csv", &format!("t,vin\n0,{}\n", vin_like()));
    let (code, out) = run(&["privacy-scan", dirty.to_str().unwrap(), "--github"]);
    assert_eq!(code, Some(2), "{out}");
    assert!(out.contains("b.csv:2:3: privacy.vin"), "{out}");
    assert!(out.contains("::error file="), "{out}");
    assert!(!out.contains(&vin_like()), "{out}");

    let allow = write(
        &dir,
        "allow.txt",
        &format!("# bench fixture\n{}\n", vin_like()),
    );
    let (code, out) = run(&[
        "privacy-scan",
        dirty.to_str().unwrap(),
        "--allow",
        allow.to_str().unwrap(),
    ]);
    assert_eq!(code, Some(0), "{out}");
    std::fs::remove_dir_all(&dir).ok();
}
