//! `opentune log` CSV → the Log viewer's reader (`opentune-logview`): a CLI
//! log opens as an OpenTune Live log, timed by its `ts` column.
use std::path::PathBuf;
use std::process::Command;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("opentune-logview-cli-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join(name)
}

#[test]
fn cli_log_opens_in_the_log_viewer() {
    let def = tmp("logger.xml");
    std::fs::write(
        &def,
        r#"<logger><protocols><protocol id="UDS"><parameters mode="22">
  <parameter id="vss" name="Vehicle speed" desc="">
    <address length="1">0x2001</address>
    <conversions><conversion units="km/h" expr="x" storagetype="uint8"/></conversions>
  </parameter>
  <parameter id="ect" name="Coolant" desc="">
    <address length="1">0x2002</address>
    <conversions><conversion units="C" expr="x-40" storagetype="uint8"/></conversions>
  </parameter>
</parameters></protocol></protocols></logger>"#,
    )
    .unwrap();
    let store = tmp("store.json");
    std::fs::write(&store, r#"{ "0x2001": "32", "0x2002": "82" }"#).unwrap();
    let csv = tmp("run.csv");
    let out = Command::new(env!("CARGO_BIN_EXE_opentune"))
        .args([
            "log",
            "--def",
            def.to_str().unwrap(),
            "--store",
            store.to_str().unwrap(),
            "--rounds",
            "3",
            "--interval-ms",
            "5",
            "--out",
            csv.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");

    let text = std::fs::read_to_string(&csv).unwrap();
    assert!(
        text.starts_with("round,param_id,name,raw,value,units,note,text,ts\n"),
        "{text}"
    );
    let log = opentune_logview::parse(&text, &opentune_logview::ParseOptions::default()).unwrap();
    assert_eq!(log.format, opentune_logview::LogFormat::OpentuneLong);
    assert_eq!(log.time_column.as_deref(), Some("ts"));
    assert!(log.epoch_start.is_some(), "ts is Unix ms: {:?}", log.notes);
    assert_eq!(log.t.len(), 3, "one sample per round: {:?}", log.t);
    assert_eq!(log.t[0], 0.0);
    assert!(log.t.windows(2).all(|w| w[1] > w[0]), "{:?}", log.t);
    let vss = log.channel("Vehicle speed").unwrap();
    assert_eq!(vss.unit, "km/h");
    assert_eq!(vss.values, vec![50.0; 3]);
    assert_eq!(log.channel("Coolant").unwrap().values, vec![90.0; 3]);
}
