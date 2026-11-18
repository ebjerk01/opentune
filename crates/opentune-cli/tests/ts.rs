//! `opentune ts …` smoke tests through the library entry point, against the
//! synthetic demo INIs and the simulated ECU (`--port sim`).
use opentune_cli::{try_run_from, CliConfig};
use opentune_tsini::{load_ini, IniOptions, Msq, MsqLoadOptions, MsqMeta, Tune, Value};
use std::path::{Path, PathBuf};

fn sample(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../samples/ts")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("opentune-ts-cli-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join(name)
}

fn run(args: &[&str]) -> anyhow::Result<()> {
    let mut v = vec!["opentune"];
    v.extend_from_slice(args);
    try_run_from(CliConfig::default(), v)
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn ini_commands() {
    let ini = sample("opentune-demo.ini");
    run(&["ts", "ini", "info", &ini]).unwrap();
    run(&[
        "ts",
        "ini",
        "info",
        &ini,
        "--warnings",
        "--set",
        "FAHRENHEIT",
    ])
    .unwrap();
    run(&["ts", "ini", "info", &ini, "--json"]).unwrap();
    run(&["ts", "ini", "tables", &ini]).unwrap();
    run(&["ts", "ini", "info", &sample("opentune-demo-ms.ini")]).unwrap();
    assert!(run(&["ts", "ini", "info", "/nonexistent.ini"]).is_err());
}

/// A tune that differs from a blank ECU in a few places.
fn edited_msq(path: &Path) {
    let ini_path = PathBuf::from(sample("opentune-demo.ini"));
    let ini = load_ini(&ini_path, &IniOptions::default()).unwrap();
    let mut t = Tune::blank(&ini);
    t.pc.insert("stoich".into(), Value::Scalar(14.7));
    t.set(&ini, "reqFuel", &Value::Scalar(7.3)).unwrap();
    t.set(&ini, "injLayout", &Value::Bits(3)).unwrap();
    t.set(&ini, "afrTarget", &Value::Scalar(0.9)).unwrap();
    t.set(
        &ini,
        "veTable",
        &Value::Array((0..48).map(|i| 30.0 + i as f64).collect()),
    )
    .unwrap();
    let (m, skipped) = Msq::from_tune(&ini, &t, &MsqMeta::default());
    assert!(skipped.is_empty(), "{skipped:?}");
    std::fs::write(path, m.to_xml()).unwrap();
}

#[test]
fn read_show_diff_write_on_the_simulator() {
    let ini = sample("opentune-demo.ini");
    let read = tmp("read.msq");
    run(&[
        "ts",
        "read",
        "--port",
        "sim",
        "--ini",
        &ini,
        "--out",
        s(&read),
    ])
    .unwrap();
    let m = Msq::load(&read).unwrap();
    assert_eq!(m.signature(), Some("opentune demo 1"));
    assert!(m.entry("veTable").is_some());

    let edited = tmp("edited.msq");
    edited_msq(&edited);
    run(&["ts", "msq", "show", s(&edited), "--ini", &ini]).unwrap();
    run(&[
        "ts",
        "msq",
        "show",
        s(&edited),
        "--ini",
        &ini,
        "--name",
        "veTable",
        "--name",
        "injLayout",
    ])
    .unwrap();
    assert!(run(&[
        "ts",
        "msq",
        "show",
        s(&edited),
        "--ini",
        &ini,
        "--name",
        "nope"
    ])
    .is_err());
    run(&["ts", "msq", "diff", s(&read), s(&edited), "--ini", &ini]).unwrap();

    // Dry run: reads, lists the changes, writes nothing (and saves nothing).
    let backup = tmp("backup.msq");
    let _ = std::fs::remove_file(&backup);
    run(&[
        "ts",
        "write",
        "--port",
        "sim",
        "--ini",
        &ini,
        s(&edited),
        "--backup",
        s(&backup),
    ])
    .unwrap();
    assert!(!backup.exists(), "a dry run saves no backup");
    // Confirmed: backup saved first, written, verified, burned.
    run(&[
        "ts",
        "write",
        "--port",
        "sim",
        "--ini",
        &ini,
        s(&edited),
        "--backup",
        s(&backup),
        "--confirm",
    ])
    .unwrap();
    let raw = std::fs::read(backup.with_extension("pages.bin")).unwrap();
    assert_eq!(
        raw,
        vec![0u8; 64 + 112],
        "raw backup of the blank simulated ECU"
    );
    let b = Msq::load(&backup).unwrap();
    let iniv = load_ini(Path::new(&ini), &IniOptions::default()).unwrap();
    let (bt, _) = b
        .to_tune(&iniv, Tune::blank(&iniv), MsqLoadOptions::default())
        .unwrap();
    assert_eq!(
        bt.get(&iniv, "reqFuel").unwrap(),
        Value::Scalar(0.0),
        "backup is the ECU's old tune"
    );
    // Already holds it: a sim preloaded with the tune has nothing to write.
    run(&[
        "ts",
        "write",
        "--port",
        "sim",
        "--sim-tune",
        s(&edited),
        "--ini",
        &ini,
        s(&edited),
        "--confirm",
        "--backup",
        s(&tmp("unused.msq")),
    ])
    .unwrap();
    assert!(!tmp("unused.msq").exists());
}

#[test]
fn write_refuses_a_tune_for_other_firmware() {
    let ini = sample("opentune-demo.ini");
    let edited = tmp("other-fw.msq");
    edited_msq(&edited);
    let xml = std::fs::read_to_string(&edited)
        .unwrap()
        .replace("opentune demo 1", "speeduino 202501");
    std::fs::write(&edited, xml).unwrap();
    let e = run(&[
        "ts",
        "write",
        "--port",
        "sim",
        "--ini",
        &ini,
        s(&edited),
        "--confirm",
    ])
    .unwrap_err();
    assert!(format!("{e:#}").contains("does not match"), "{e:#}");
    assert!(run(&["ts", "msq", "show", s(&edited), "--ini", &ini]).is_err());
    run(&[
        "ts",
        "msq",
        "show",
        s(&edited),
        "--ini",
        &ini,
        "--force-signature",
    ])
    .unwrap();
}

#[test]
fn log_writes_a_live_log_csv() {
    let ini = sample("opentune-demo.ini");
    let out = tmp("log.csv");
    run(&[
        "ts",
        "log",
        "--port",
        "sim",
        "--ini",
        &ini,
        "--channels",
        "rpm,coolant,smoothRpm",
        "--rounds",
        "3",
        "--interval-ms",
        "0",
        "--out",
        s(&out),
    ])
    .unwrap();
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.starts_with("ts,param_id,name,raw,value,units,note\n"));
    assert_eq!(text.lines().count(), 1 + 3 * 3);
    assert!(
        text.contains("unsupported"),
        "smoothRpm explains its empty value"
    );
    let log = opentune_logview::parse(&text, &opentune_logview::ParseOptions::default()).unwrap();
    assert_eq!(log.format, opentune_logview::LogFormat::OpentuneLong);
    assert_eq!(log.channel("rpm").unwrap().values.len(), 3);
    assert!(run(&[
        "ts",
        "log",
        "--port",
        "sim",
        "--ini",
        &ini,
        "--channels",
        "nope",
        "--out",
        s(&tmp("x.csv")),
    ])
    .is_err());
}

#[test]
fn plain_megasquirt_style_on_the_simulator() {
    let ini = sample("opentune-demo-ms.ini");
    let out = tmp("ms.msq");
    run(&[
        "ts",
        "read",
        "--port",
        "sim",
        "--ini",
        &ini,
        "--out",
        s(&out),
    ])
    .unwrap();
    run(&["ts", "msq", "show", s(&out), "--ini", &ini]).unwrap();
    run(&[
        "ts",
        "log",
        "--port",
        "sim",
        "--ini",
        &ini,
        "--rounds",
        "2",
        "--interval-ms",
        "0",
        "--out",
        s(&tmp("ms.csv")),
    ])
    .unwrap();
    // Forcing envelope framing on a plain-only definition still talks to the
    // simulator, which follows the INI: no valid reply, so it fails cleanly.
    assert!(run(&[
        "ts",
        "read",
        "--port",
        "sim",
        "--ini",
        &ini,
        "--framing",
        "envelope",
        "--timeout-ms",
        "5"
    ])
    .is_err());
}
