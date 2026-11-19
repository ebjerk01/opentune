//! `opentune log --transport ssm|ssm-can` against the simulated SSM module
//! (`--driver ssmsim`) and the synthetic logger.xml in opentune-logdef's
//! test fixtures. No hardware, no OEM data.
use std::path::PathBuf;
use std::process::{Command, Output};

fn fixture() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../opentune-logdef/tests/fixtures/ssm_demo_logger.xml")
        .to_string_lossy()
        .into_owned()
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("opentune-ssm-cli-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join(name)
}

fn opentune(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_opentune"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn log_over_ssm_kline_writes_the_supported_channels_to_csv() {
    let def = fixture();
    let csv = tmp("kline.csv");
    let out = opentune(&[
        "log",
        "--def",
        &def,
        "--driver",
        "ssmsim",
        "--transport",
        "ssm",
        "--rounds",
        "4",
        "--out",
        csv.to_str().unwrap(),
    ]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("ROM ID 5A5A000001"), "{err}");
    assert!(err.contains("mode: ssm-a8"), "{err}");
    assert!(err.contains("capability bit clear): P_NOPE"), "{err}");
    assert!(err.contains("1.00 requests/sample"), "{err}");
    let text = std::fs::read_to_string(&csv).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "round,param_id,name,raw,value,units,note,text,ts");
    assert_eq!(lines.len(), 1 + 4 * 5, "{text}");
    let ids: Vec<&str> = lines[1..6]
        .iter()
        .map(|l| l.split(',').nth(1).unwrap())
        .collect();
    assert_eq!(ids, ["P_ECT", "P_RPM", "P_VSS", "S_DEMO", "E_FLOAT"]);
    assert!(
        lines[1..]
            .iter()
            .all(|l| !l.split(',').nth(4).unwrap().is_empty()),
        "{text}"
    );
}

#[test]
fn log_over_ssm_can_to_the_transmission_with_small_packets() {
    let def = fixture();
    let out = opentune(&[
        "log",
        "--def",
        &def,
        "--driver",
        "ssmsim",
        "--transport",
        "ssm-can",
        "--ssm-target",
        "tcu",
        "--ssm-max-packet",
        "10",
        "--rounds",
        "2",
    ]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    let csv = String::from_utf8_lossy(&out.stdout);
    let ids: Vec<&str> = csv
        .lines()
        .skip(1)
        .take(3)
        .map(|l| l.split(',').nth(1).unwrap())
        .collect();
    assert_eq!(ids, ["P_RPM", "P_VSS", "P_TCU"]);
    // One address per A8: 4 addresses, 4 requests per sample.
    assert!(err.contains("4.00 requests/sample"), "{err}");
}

#[test]
fn ssm_flags_are_checked() {
    let def = fixture();
    let out = opentune(&["log", "--def", &def, "--driver", "ssmsim"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--transport ssm"));

    let out = opentune(&[
        "log",
        "--def",
        &def,
        "--driver",
        "ssmsim",
        "--transport",
        "ssm",
        "--fast",
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--fast is UDS 0x2C"));

    // The mock driver has no raw K-line.
    let store = tmp("store.json");
    std::fs::write(&store, "{}").unwrap();
    let out = opentune(&[
        "log",
        "--def",
        &def,
        "--store",
        store.to_str().unwrap(),
        "--transport",
        "ssm",
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no raw K-line"));
}
