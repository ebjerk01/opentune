//! `opentune bsl …` smoke tests: the dry-run plan, the --i-understand gate
//! and a full read against the simulated C167 (`--port sim`).
use opentune_cli::{try_run_from, CliConfig};
use std::path::PathBuf;
use std::process::Command;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("opentune-bsl-cli-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join(name)
}

fn run(args: &[&str]) -> anyhow::Result<()> {
    let mut v = vec!["opentune"];
    v.extend_from_slice(args);
    try_run_from(CliConfig::default(), v)
}

fn stdout(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_opentune"))
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn presets_lists_the_edc15_boards() {
    let s = stdout(&["bsl", "presets"]);
    for id in ["edc15vm-p", "edc15c6", "c167", "ADDRSEL3", "(unverified)"] {
        assert!(s.contains(id), "{id} missing:\n{s}");
    }
}

#[test]
fn plan_is_a_dry_run_without_files() {
    let s = stdout(&["bsl", "plan", "--preset", "edc15c6", "--fcpu-mhz", "20"]);
    assert!(s.contains("DRY RUN"));
    assert!(s.contains("<32 bytes: primary loader (--loader)>"));
    assert!(s.contains("ADDRSEL3 := 0x4008"));
    assert!(s.contains("0x400000..0x4fffff"));
    assert!(s.contains("UNVERIFIED"));
    // 57600 at 20 MHz is far off; the plan says so.
    assert!(s.contains("TOO HIGH"), "{s}");

    let short = tmp("short-loader.bin");
    std::fs::write(&short, [0u8; 31]).unwrap();
    let e = run(&[
        "bsl",
        "plan",
        "--preset",
        "edc15c6",
        "--loader",
        short.to_str().unwrap(),
    ])
    .unwrap_err();
    assert!(e.to_string().contains("exactly 32"), "{e}");
    assert!(run(&["bsl", "plan", "--preset", "nope"]).is_err());
    assert!(run(&["bsl", "plan", "--preset", "c167", "--region", "ext-flash"]).is_err());
}

#[test]
fn read_needs_i_understand_and_payloads() {
    let out = tmp("gate.bin");
    let o = out.to_str().unwrap();
    let e = run(&[
        "bsl", "read", "--preset", "edc15c6", "--port", "sim", "--out", o,
    ])
    .unwrap_err();
    assert!(e.to_string().contains("--i-understand"), "{e}");
    let e = run(&[
        "bsl",
        "read",
        "--preset",
        "edc15c6",
        "--port",
        "/dev/opentune-no-such-port",
        "--baud",
        "38400",
        "--out",
        o,
        "--i-understand",
    ])
    .unwrap_err();
    assert!(e.to_string().contains("--loader and --kernel"), "{e}");
    assert!(!out.exists());
}

#[test]
fn read_against_the_simulator() {
    let out = tmp("sim-read.bin");
    let tr = tmp("sim-read.txt");
    run(&[
        "bsl",
        "read",
        "--preset",
        "edc15c6",
        "--port",
        "sim",
        "--len",
        "0x500",
        "--offset",
        "0x100",
        "--out",
        out.to_str().unwrap(),
        "--transcript",
        tr.to_str().unwrap(),
        "--i-understand",
    ])
    .unwrap();
    let data = std::fs::read(&out).unwrap();
    assert_eq!(data.len(), 0x500);
    // The simulator's pattern: a ^ a>>8 ^ a>>16 for CPU address a.
    for (i, b) in data.iter().enumerate() {
        let a = 0x40_0100 + i as u32;
        assert_eq!(*b, (a ^ (a >> 8) ^ (a >> 16)) as u8, "at {i:#x}");
    }
    let t = std::fs::read_to_string(&tr).unwrap();
    assert!(t.lines().next().unwrap().contains("zero byte"));
    assert!(t.contains("echo ["));

    // Full-duplex variant, internal ROM.
    run(&[
        "bsl",
        "read",
        "--preset",
        "c167",
        "--port",
        "sim",
        "--no-echo",
        "--len",
        "0x80",
        "--out",
        out.to_str().unwrap(),
        "--i-understand",
    ])
    .unwrap();
    assert_eq!(std::fs::read(&out).unwrap().len(), 0x80);
}
