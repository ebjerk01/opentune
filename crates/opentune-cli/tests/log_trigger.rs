//! `--trigger` / `--split` on `opentune obd read --out` and `opentune log`,
//! offline (`--driver obdsim`, `--driver mock`): gated windows with start and
//! end marker rows, one file per window, mark-only triggers, the shipped
//! generic-obd2 trigger file, and the gated CSV opened by the Log viewer.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("opentune-trigger-cli-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn tmp(name: &str) -> PathBuf {
    dir().join(name)
}

fn exec(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_opentune"))
        .args(args)
        .output()
        .unwrap()
}

fn run(args: &[&str]) -> (String, String) {
    let out = exec(args);
    assert!(out.status.success(), "{args:?}: {out:?}");
    (
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

fn write(name: &str, text: &str) -> PathBuf {
    let p = tmp(name);
    std::fs::write(&p, text).unwrap();
    p
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// `(param_id, text)` of every row of a `ts,param_id,...,text` CSV.
fn ids_and_text(csv: &str) -> Vec<(String, String)> {
    csv.lines()
        .skip(1)
        .map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            (f[1].to_string(), f.last().unwrap().to_string())
        })
        .collect()
}

/// obdsim idles at 1800 rpm: `rpm > 1000` is true from the first poll, so
/// with a 100 ms arm hold the first polls are dropped and the window runs to
/// the end of the log.
const ARMED: &str = r#"{"triggers":[
  {"name":"running","action":"record","arm_ms":100,
   "when":{"all":[{"channel":"rpm","op":">","value":1000,"units":"1/min"},
                  {"channel":"throttle_pos","op":"<","value":50,"units":"%"}]}}
]}"#;

#[test]
fn obd_read_records_only_the_window() {
    let trig = write("armed.json", ARMED);
    let csv = tmp("gated.csv");
    let (_, err) = run(&[
        "obd",
        "read",
        "--driver",
        "obdsim",
        "--rounds",
        "12",
        "--interval-ms",
        "30",
        "--out",
        s(&csv),
        "--trigger",
        s(&trig),
        "0C",
        "11",
    ]);
    assert!(err.contains("Recorded 12 poll(s) of 2 PID(s) to"), "{err}");
    assert!(
        err.contains("trigger: 1 fire(s) (running 1); 1 window(s)"),
        "{err}"
    );
    assert!(
        err.contains("window 1:") && err.contains("cut by the end of the log"),
        "{err}"
    );
    let text = std::fs::read_to_string(&csv).unwrap();
    assert!(
        text.starts_with("ts,param_id,name,raw,value,units,note,text\n"),
        "{text}"
    );
    let rows = ids_and_text(&text);
    assert_eq!(rows[0], ("trigger".into(), "start".into()), "{text}");
    assert_eq!(
        rows.last().unwrap(),
        &("trigger".to_string(), "end".to_string())
    );
    let polls = rows.iter().filter(|r| r.0 == "rpm").count();
    assert!(
        (1..12).contains(&polls),
        "the armed hold drops the first polls: {text}"
    );
    let log =
        opentune_logview::parse_file(&csv, &opentune_logview::ParseOptions::default()).unwrap();
    assert_eq!(log.format, opentune_logview::LogFormat::OpentuneLong);
    assert_eq!(log.t.len(), polls);
    let names: Vec<&str> = log.channels.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"Engine speed"), "{names:?}");
}

#[test]
fn split_writes_numbered_files() {
    let trig = write("armed-split.json", ARMED);
    let csv = tmp("pulls.csv");
    let (_, err) = run(&[
        "obd",
        "read",
        "--driver",
        "obdsim",
        "--rounds",
        "6",
        "--interval-ms",
        "40",
        "--out",
        s(&csv),
        "--trigger",
        s(&trig),
        "--split",
        "0C",
        "11",
    ]);
    assert!(err.contains("1 window file(s) next to"), "{err}");
    assert!(!csv.exists(), "--split writes only the numbered files");
    let first = tmp("pulls-001.csv");
    assert!(err.contains(s(&first)), "{err}");
    let text = std::fs::read_to_string(&first).unwrap();
    let rows = ids_and_text(&text);
    assert_eq!(rows[0].1, "start");
    assert_eq!(rows.last().unwrap().1, "end");
    assert!(!tmp("pulls-002.csv").exists());
}

#[test]
fn the_shipped_wot_trigger_runs_and_idle_records_nothing() {
    let trig = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packs/generic-obd2/triggers/wot-pull.json");
    let csv = tmp("idle.csv");
    let (_, err) = run(&[
        "obd",
        "read",
        "--driver",
        "obdsim",
        "--rounds",
        "4",
        "--out",
        s(&csv),
        "--trigger",
        s(&trig),
        "0C",
        "11",
        "05",
    ]);
    assert!(!err.contains("warning"), "all channels are logged: {err}");
    assert!(
        err.contains("trigger: 0 fire(s) (wot-pull 0, coolant-hot 0); 0 window(s)"),
        "{err}"
    );
    assert_eq!(
        std::fs::read_to_string(&csv).unwrap(),
        "ts,param_id,name,raw,value,units,note,text\n"
    );
}

const LOGGER: &str = r#"<logger><protocols><protocol id="UDS"><parameters mode="22">
  <parameter id="vss" name="Vehicle speed" desc="">
    <address length="1">0x2001</address>
    <conversions><conversion units="km/h" expr="x" storagetype="uint8"/></conversions>
  </parameter>
</parameters></protocol></protocols></logger>"#;

#[test]
fn log_marks_without_gating_and_warns_about_unlogged_channels() {
    let def = write("logger.xml", LOGGER);
    let store = write("store.json", r#"{ "0x2001": "32" }"#);
    // 50 km/h = 31.07 mph: the mark fires once (no release in between).
    let trig = write(
        "marks.json",
        r#"{"triggers":[
          {"name":"over30mph","action":"mark","when":{"channel":"vss","op":">","value":30,"units":"mph"}},
          {"name":"knock","action":"mark","when":{"channel":"knock_retard","op":">","value":3,"units":"deg"}}
        ]}"#,
    );
    let csv = tmp("marks.csv");
    let (out, err) = run(&[
        "log",
        "--def",
        s(&def),
        "--store",
        s(&store),
        "--rounds",
        "5",
        "--out",
        s(&csv),
        "--trigger",
        s(&trig),
    ]);
    assert!(out.contains("Logged 5 round(s) to"), "{out}");
    assert!(
        err.contains("trigger: warning: 'knock_retard' is not among the logged channels"),
        "{err}"
    );
    assert!(
        err.contains("trigger: 1 fire(s) (over30mph 1, knock 0); 0 window(s); 1 mark(s)"),
        "{err}"
    );
    let text = std::fs::read_to_string(&csv).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "round,param_id,name,raw,value,units,note,text,ts");
    assert_eq!(lines.len(), 1 + 5 + 1, "{text}");
    assert!(lines[1].starts_with("0,vss,"), "{text}");
    assert!(
        lines[2].starts_with("0,trigger,over30mph,,,,over30mph fired: vss > 30 mph,mark,"),
        "{text}"
    );
}

#[test]
fn log_gated_with_no_fire_writes_only_the_header() {
    let def = write("logger2.xml", LOGGER);
    let store = write("store2.json", r#"{ "0x2001": "32" }"#);
    let trig = write(
        "fast.json",
        r#"{"triggers":[{"name":"fast","action":"record","pre_ms":2000,
           "when":{"channel":"vss","op":">=","value":200,"units":"km/h"}}]}"#,
    );
    let (_, err) = run(&[
        "log",
        "--def",
        s(&def),
        "--store",
        s(&store),
        "--rounds",
        "3",
        "--trigger",
        s(&trig),
    ]);
    assert!(
        err.contains("0 window(s); 0 mark(s); 3 row(s) outside windows dropped"),
        "{err}"
    );
    // --split needs --out; obd read --trigger needs --out.
    let o = exec(&["log", "--def", s(&def), "--trigger", s(&trig), "--split"]);
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("needs --out"),
        "{o:?}"
    );
    let o = exec(&[
        "obd",
        "read",
        "--driver",
        "obdsim",
        "--trigger",
        s(&trig),
        "0C",
    ]);
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("add --out"),
        "{o:?}"
    );
    // A broken trigger file is refused before anything is logged.
    let bad = write("bad.json", r#"{"triggers":[]}"#);
    let o = exec(&["log", "--def", s(&def), "--trigger", s(&bad)]);
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("no triggers"),
        "{o:?}"
    );
}
