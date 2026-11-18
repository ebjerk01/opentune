//! `opentune gps` and `opentune log --gps` against the synthetic NMEA
//! fixtures in crates/opentune-gps/tests/fixtures (replayed, no hardware).
use std::path::PathBuf;
use std::process::Command;

fn fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../opentune-gps/tests/fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("opentune-gps-cli-{}", std::process::id()));
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
fn gps_replays_a_file_with_counters() {
    let (out, err) = run(&["gps", &fixture("broken.nmea")]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[0],
        "row,utc_date,utc_time,lat,lon,alt_m,speed_kmh,course_deg,sats,fix,hdop,good,note"
    );
    assert_eq!(lines.len(), 11);
    assert!(lines[1].starts_with("0,2026-06-15,10:00:00.000,56.5,3,4.2,0,45,9,1,0.9,1,"));
    // The outage rows: no position, fix 0, flagged.
    assert!(lines[5].contains(",,,,,4,0,99.99,0,no fix"), "{}", lines[5]);
    assert!(err.contains("10 row(s), 8 with a good fix"), "{err}");
    assert!(err.contains("2 bad checksum, 1 partial, 1 malformed, 1 noise, 1 overlong"));

    let (out, _) = run(&["gps", &fixture("route_gn.nmea"), "--count", "3"]);
    assert_eq!(out.lines().count(), 4);
}

#[test]
fn log_with_gps_adds_channels_and_speed_error() {
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
    std::fs::write(&store, r#"{ "0x2001": "32" }"#).unwrap(); // 50 km/h
    let csv = tmp("run.csv");
    let (_, err) = run(&[
        "log",
        "--def",
        def.to_str().unwrap(),
        "--store",
        store.to_str().unwrap(),
        "--rounds",
        "5",
        "--gps",
        &fixture("route_gn.nmea"),
        "--out",
        csv.to_str().unwrap(),
    ]);
    assert!(err.contains("speed_error_kmh vs 'vss'"), "{err}");
    assert!(
        err.contains("gps: 5 of 5 sample(s) with a good fix"),
        "{err}"
    );
    let text = std::fs::read_to_string(&csv).unwrap();
    // Header + 5 rounds x (1 ECU + 8 GPS + speed error).
    assert_eq!(text.lines().count(), 1 + 5 * 10);
    // Round 3: GPS 18 km/h, vehicle 50 km/h -> +32.
    let row = text
        .lines()
        .find(|l| l.starts_with("3,speed_error_kmh,"))
        .unwrap();
    assert_eq!(row.split(',').nth(4), Some("32"), "{row}");
    assert!(text.contains("\n4,gps_speed_kmh,GPS speed over ground,,24,km/h,,"));

    // An explicit vehicle-speed id that is not logged is refused.
    let out = Command::new(env!("CARGO_BIN_EXE_opentune"))
        .args([
            "log",
            "--def",
            def.to_str().unwrap(),
            "--store",
            store.to_str().unwrap(),
            "--gps",
            &fixture("route_gn.nmea"),
            "--gps-speed-param",
            "nope",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("'nope' is not among"));
}
