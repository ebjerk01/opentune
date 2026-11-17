//! Integration tests for the `a2l2xdf` CLI command.
//!
//! They run the compiled `opentune` binary against a small synthetic A2L (no
//! external fixtures), capture its XDF on stdout, and re-import that XDF with
//! [`opentune_xdf::parse_xdf`] to prove the maps survive the A2L → XDF → parse
//! round-trip. They also assert the conversion-loss report on stderr flags the
//! lossy cases (a map dropped by address rebasing, a non-linear conversion
//! imported pre-scaled) rather than hiding them.

use std::io::Write;
use std::process::Command;

/// A compact, structurally complete A2L with three characteristics whose
/// addresses are chosen so a base of `0x8000_0000` exercises every path:
/// `boostmap` (affine RAT_FUNC, rebases to `0x1_1000`), `sensorcurve`
/// (TAB_INTP, non-linear → flagged), and `lowscalar` (below the base → dropped).
const SAMPLE_A2L: &str = r#"
ASAP2_VERSION 1 71
/begin PROJECT RoundTripProj "round-trip project"
  /begin HEADER "h" VERSION "1" /end HEADER
  /begin MODULE RoundTripMod "the module"

    /begin MOD_COMMON ""
      BYTE_ORDER MSB_FIRST
    /end MOD_COMMON

    /begin RECORD_LAYOUT RL_MAP
      FNC_VALUES 1 SWORD COLUMN_DIR DIRECT
    /end RECORD_LAYOUT

    /begin RECORD_LAYOUT RL_AXIS
      AXIS_PTS_X 1 UWORD INDEX_INCR DIRECT
    /end RECORD_LAYOUT

    /begin COMPU_METHOD CM_TORQUE
      "torque scaling"
      RAT_FUNC "%8.4" "Nm"
      COEFFS 0 655.350000 0.000000 0 0.000000 10.240000
    /end COMPU_METHOD

    /begin COMPU_METHOD CM_RPM
      "rpm scaling"
      RAT_FUNC "%8.0" "rpm"
      COEFFS 0 1 0 0 0 1
    /end COMPU_METHOD

    /begin COMPU_METHOD CM_TAB
      "nonlinear sensor"
      TAB_INTP "%8.2" "V"
      COMPU_TAB_REF TBL_SENS
    /end COMPU_METHOD

    /begin AXIS_PTS AX_RPM
      "rpm breakpoints"
      0x80008000
      no_input
      RL_AXIS
      0
      CM_RPM
      16
      0.0
      7000.0
    /end AXIS_PTS

    /begin CHARACTERISTIC boostmap
      "boost target"
      MAP
      0x80011000
      RL_MAP
      0
      CM_TORQUE
      -10.0
      320.0
      /begin AXIS_DESCR
        COM_AXIS rpm_in CM_RPM 16 0.0 7000.0
        AXIS_PTS_REF AX_RPM
      /end AXIS_DESCR
      /begin AXIS_DESCR
        STD_AXIS load_in CM_RPM 8 0.0 100.0
      /end AXIS_DESCR
    /end CHARACTERISTIC

    /begin CHARACTERISTIC sensorcurve
      "injector linearization"
      CURVE
      0x80022000
      RL_MAP
      0
      CM_TAB
      0.0
      3500.0
      /begin AXIS_DESCR
        STD_AXIS volt_in CM_RPM 8 0.0 5000.0
      /end AXIS_DESCR
    /end CHARACTERISTIC

    /begin CHARACTERISTIC lowscalar
      "below the memory base"
      VALUE
      0x7FFF0000
      RL_MAP
      0
      CM_RPM
      0.0
      2000.0
    /end CHARACTERISTIC

  /end MODULE
/end PROJECT
"#;

/// Write the sample A2L to a unique temp file and return its path.
fn write_sample() -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "opentune-a2l2xdf-{}-{:?}.a2l",
        std::process::id(),
        std::thread::current().id()
    ));
    let mut f = std::fs::File::create(&p).expect("create temp a2l");
    f.write_all(SAMPLE_A2L.as_bytes()).expect("write temp a2l");
    p
}

/// Run `opentune a2l2xdf <args...>` and return (stdout, stderr).
fn run_a2l2xdf(args: &[&str]) -> (String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_opentune"))
        .arg("a2l2xdf")
        .args(args)
        .output()
        .expect("spawn opentune");
    assert!(
        out.status.success(),
        "a2l2xdf failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (
        String::from_utf8(out.stdout).expect("utf8 stdout"),
        String::from_utf8(out.stderr).expect("utf8 stderr"),
    )
}

#[test]
fn roundtrip_a2l_to_xdf_and_reimport() {
    let a2l = write_sample();
    let (xdf, report) = run_a2l2xdf(&[a2l.to_str().unwrap(), "--base", "0x80000000"]);
    let _ = std::fs::remove_file(&a2l);

    // --- Re-import the generated XDF and confirm the maps survive. ---
    let imp = opentune_xdf::parse_xdf(&xdf).expect("re-import generated XDF");

    // Two maps exported; `lowscalar` was dropped because its address < base.
    assert_eq!(imp.maps.len(), 2, "expected boostmap + sensorcurve");
    assert!(
        !imp.maps.iter().any(|m| m.name == "lowscalar"),
        "below-base map must not reach the XDF"
    );

    // The affine map round-trips: rebased address, dimensions, big-endian S16,
    // and the recovered linear scaling (factor 10.24/655.35 = 0.015625).
    let boost = imp
        .maps
        .iter()
        .find(|m| m.name == "boostmap")
        .expect("boostmap present");
    assert_eq!(boost.address, 0x0001_1000, "0x80011000 - 0x80000000");
    assert_eq!(boost.cols, 16);
    assert_eq!(boost.rows, 8);
    assert_eq!(boost.data_type, opentune_core::ecudef::DataType::S16);
    assert_eq!(boost.endian, opentune_core::ecudef::Endian::Big);
    assert!((boost.scaling.factor - 10.24 / 655.35).abs() < 1e-9);
    assert!(boost.scaling.offset.abs() < 1e-12);
    // Its stored X axis rebased too (0x80008000 - base).
    let x = boost.x_axis.as_ref().expect("x axis survived");
    assert_eq!(x.address, Some(0x0000_8000));

    // --- Conversion-loss report (stderr) flags the lossy cases. ---
    assert!(report.contains("conversion-loss report"));
    assert!(
        report.contains("Maps dropped") && report.contains("lowscalar"),
        "report must name the dropped below-base map:\n{report}"
    );
    assert!(
        report.contains("Non-linear scalings flagged") && report.contains("sensorcurve"),
        "report must flag the TAB_INTP map as non-linear:\n{report}"
    );
}

#[test]
fn only_filter_excludes_other_maps() {
    let a2l = write_sample();
    let (xdf, report) = run_a2l2xdf(&[
        a2l.to_str().unwrap(),
        "--base",
        "0x80000000",
        "--only",
        "boost",
    ]);
    let _ = std::fs::remove_file(&a2l);

    let imp = opentune_xdf::parse_xdf(&xdf).expect("re-import filtered XDF");
    assert_eq!(imp.maps.len(), 1, "only boostmap matches --only boost");
    assert_eq!(imp.maps[0].name, "boostmap");

    // Filtered-out maps are reported as excluded (not loss), by name.
    assert!(report.contains("Filtered by --only"));
    assert!(report.contains("sensorcurve"), "report lists excluded maps");
}
