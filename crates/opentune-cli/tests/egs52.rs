//! Offline coding-file and unconfigured bootstrap-read CLI boundaries.
use std::{fs, process::Command};

#[test]
fn offline_crc_requires_exact_size_and_acknowledgement() {
    let dir = std::env::temp_dir().join(format!("opentune-egs52-{}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    let input = dir.join("input.bin");
    let output = dir.join("candidate.bin");
    let mut record = vec![0; 42];
    record[31..40].copy_from_slice(b"123456789");
    fs::write(&input, &record).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_opentune"))
            .args(["coding", "egs52-crc"])
            .arg(&input)
            .args(args)
            .output()
            .unwrap()
    };
    assert!(!run(&[]).status.success());
    let path = output.to_str().unwrap();
    assert!(!run(&["--out", path]).status.success());
    assert!(!output.exists());
    let result = run(&["--out", path, "--allow-unconfirmed"]);
    assert!(result.status.success(), "{result:?}");
    assert!(String::from_utf8(result.stdout)
        .unwrap()
        .contains("unconfirmed on a real EGS"));
    assert_eq!(fs::read(&input).unwrap(), record);
    record[40..].copy_from_slice(&[0x3d, 0xbb]);
    assert_eq!(fs::read(&output).unwrap(), record);
    assert!(!run(&["--out", path, "--allow-unconfirmed"])
        .status
        .success());
    fs::write(&input, &record).unwrap();
    assert!(run(&[]).status.success());
    assert!(!run(&["--xorout", "0xffff"]).status.success());
    fs::write(&input, &record[..41]).unwrap();
    assert!(!run(&[]).status.success());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn plan_shows_missing_setup_and_read_checks_before_output() {
    let command = |action| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_opentune"));
        cmd.args(["bsl", action, "--preset", "egs52"]);
        cmd
    };
    let result = command("plan").output().unwrap();
    assert!(result.status.success());
    let plan = String::from_utf8(result.stdout).unwrap();
    assert!(
        plan.contains("READ BLOCKED")
            && plan.contains("ADDRSEL4")
            && plan.contains("0x080000..0x0fffff")
    );
    let out = std::env::temp_dir().join(format!("opentune-egs52-read-{}", std::process::id()));
    let read = command("read")
        .args(["--port", "sim", "--i-understand", "--out"])
        .arg(&out)
        .output()
        .unwrap();
    assert!(!read.status.success());
    assert!(String::from_utf8(read.stderr)
        .unwrap()
        .contains("missing unverified register values"));
    assert!(!out.exists());
}
