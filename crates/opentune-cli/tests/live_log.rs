//! `opentune log` and `opentune obd read` on the resilient loop, offline:
//! `--rounds 0 --duration`, `--reconnect` on a healthy link, and the
//! `obd read --out` Live-log CSV opened by the Log viewer.
use std::path::PathBuf;
use std::process::Command;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("opentune-livelog-cli-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join(name)
}

fn run(args: &[&str]) -> (String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_opentune"))
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?}: {out:?}");
    (
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

#[test]
fn obd_read_records_a_live_log_the_viewer_opens() {
    let csv = tmp("obd.csv");
    let (_, err) = run(&[
        "obd",
        "read",
        "--driver",
        "obdsim",
        "--rounds",
        "3",
        "--interval-ms",
        "5",
        "--reconnect",
        "--out",
        csv.to_str().unwrap(),
        "0C",
        "0D",
        "78",
    ]);
    assert!(err.contains("Recorded 3 poll(s) of 3 PID(s)"), "{err}");
    let text = std::fs::read_to_string(&csv).unwrap();
    assert!(text.starts_with("ts,param_id,name,raw,value,units,note,text\n"));
    assert!(text.contains(",rpm,Engine speed,,1800,rpm,,"), "{text}");
    let log =
        opentune_logview::parse_file(&csv, &opentune_logview::ParseOptions::default()).unwrap();
    assert_eq!(log.format, opentune_logview::LogFormat::OpentuneLong);
    assert_eq!(log.t.len(), 3);
    let names: Vec<&str> = log.channels.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"Engine speed"), "{names:?}");
    assert!(names.iter().any(|n| n.ends_with(" S1")), "{names:?}");
}

#[test]
fn log_runs_until_the_duration_with_rounds_zero() {
    let def = tmp("logger.xml");
    std::fs::write(
        &def,
        r#"<logger><protocols><protocol id="UDS"><parameters mode="22">
  <parameter id="vss" name="Vehicle speed" desc="">
    <address length="1">0x2001</address>
    <conversions><conversion units="km/h" expr="x" storagetype="uint8"/></conversions>
  </parameter>
</parameters></protocol></protocols></logger>"#,
    )
    .unwrap();
    let store = tmp("store.json");
    std::fs::write(&store, r#"{ "0x2001": "32" }"#).unwrap();
    let csv = tmp("run.csv");
    let (out, err) = run(&[
        "log",
        "--def",
        def.to_str().unwrap(),
        "--store",
        store.to_str().unwrap(),
        "--rounds",
        "0",
        "--duration",
        "0.3",
        "--interval-ms",
        "40",
        "--reconnect",
        "--out",
        csv.to_str().unwrap(),
    ]);
    let rows: Vec<String> = std::fs::read_to_string(&csv)
        .unwrap()
        .lines()
        .skip(1)
        .map(String::from)
        .collect();
    assert!(rows.len() >= 3, "{rows:?}");
    assert!(
        rows.iter()
            .all(|r| r.contains(",vss,Vehicle speed,50,50,km/h,")),
        "{rows:?}"
    );
    assert!(
        out.contains(&format!("Logged {} round(s)", rows.len())),
        "{out}"
    );
    assert!(
        !err.contains("link:"),
        "a healthy link reports nothing: {err}"
    );
}
