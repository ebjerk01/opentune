//! `opentune seedkey sa2` through the built binary. Every script here is
//! synthetic (written for the test); none is an OEM SA2 script.
use std::path::PathBuf;
use std::process::{Command, Output};

fn opentune(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_opentune"))
        .args(args)
        .output()
        .expect("run opentune")
}

fn ok_stdout(args: &[&str]) -> String {
    let out = opentune(args);
    assert!(out.status.success(), "opentune {args:?} failed: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

fn fixture_odx() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../opentune-container/tests/fixtures/synthetic-sa2.odx")
        .to_string_lossy()
        .into_owned()
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("opentune-seedkey-cli-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join(name)
}

#[test]
fn key_from_hex_script() {
    let out = ok_stdout(&[
        "seedkey",
        "sa2",
        "--script",
        "87 FF FF FF FF 4C",
        "--seed",
        "00000001",
    ]);
    assert!(out.contains("Key        : FFFFFFFE"), "{out}");
    assert!(out.contains("SA2 script : 6 bytes"), "{out}");
}

#[test]
fn disasm_and_trace_from_assembler_file() {
    let f = tmp("synthetic.sa2");
    std::fs::write(&f, "loop 2\n  rsl\nnext\nfinish\n").unwrap();
    let p = f.to_str().unwrap();
    let out = ok_stdout(&["seedkey", "sa2", "--script-file", p, "--disasm"]);
    assert!(out.contains("assembled"), "{out}");
    assert!(out.contains("loop 2") && out.contains("finish"), "{out}");
    assert!(!out.contains("Key"), "{out}");
    let out = ok_stdout(&[
        "seedkey",
        "sa2",
        "--script-file",
        p,
        "--seed",
        "0x00,0x00,0x00,0x01",
        "--trace",
    ]);
    assert!(out.contains("Key        : 00000004"), "{out}");
    assert!(out.contains("Steps      : 6"), "{out}");
    assert!(out.contains("reg=00000002 carry=0 loops=1"), "{out}");
}

#[test]
fn key_from_container_script() {
    let odx = fixture_odx();
    let script = [
        0x68, 0x03, 0x81, 0x4A, 0x05, 0x87, 0x5A, 0x5A, 0x5A, 0x5A, 0x49, 0x93, 0x00, 0x00, 0x01,
        0x00, 0x4C,
    ];
    let want = opentune_sa2::run(&script, 0x1234_5678).unwrap();
    let out = ok_stdout(&[
        "seedkey",
        "sa2",
        "--container",
        &odx,
        "--seed",
        "12345678",
        "--disasm",
    ]);
    assert!(out.contains("FW-SIGNATURE, session SES_SYNTHETIC"), "{out}");
    assert!(out.contains(&format!("Key        : {want:08X}")), "{out}");
    assert!(out.contains("bcc L000A"), "{out}");
    // `container unpack` points at the script too.
    let out = ok_stdout(&["container", "unpack", "--in", &odx]);
    assert!(
        out.contains("SA2 script: #0 17 bytes from FW-SIGNATURE"),
        "{out}"
    );
}

#[test]
fn refusals() {
    // No SA2 block in the container.
    let f = tmp("no-sa2.odx");
    std::fs::write(&f, "<?xml version=\"1.0\"?><ODX></ODX>").unwrap();
    assert!(!opentune(&[
        "seedkey",
        "sa2",
        "--container",
        f.to_str().unwrap(),
        "--seed",
        "00000000"
    ])
    .status
    .success());
    // Index past the end.
    assert!(!opentune(&[
        "seedkey",
        "sa2",
        "--container",
        &fixture_odx(),
        "--index",
        "1",
        "--seed",
        "00000000"
    ])
    .status
    .success());
    // Seed must be 4 bytes; a script error is reported, not panicked.
    for args in [
        &["seedkey", "sa2", "--script", "814C", "--seed", "0102"][..],
        &["seedkey", "sa2", "--script", "8100", "--seed", "01020304"][..],
        &["seedkey", "sa2", "--script", "814C"][..],
        &["seedkey", "sa2", "--seed", "01020304"][..],
    ] {
        let out = opentune(args);
        assert!(!out.status.success(), "{args:?} should fail");
        assert_ne!(out.status.code(), Some(101), "{args:?} panicked");
    }
}
