//! Integration tests: synthetic inline A2L snippets that pass without any
//! external fixtures. They cover a MAP with a RAT_FUNC conversion and a
//! COM_AXIS, a CURVE, a scalar VALUE, MSB_FIRST vs MSB_LAST, a TAB_INTP that
//! must be flagged rather than guessed, and a comment/whitespace torture case.

use super::*;

/// A compact but structurally complete A2L: MOD_COMMON (big-endian default),
/// two record layouts, three compu methods (one affine RAT_FUNC, one identity,
/// one TAB_INTP), one AXIS_PTS (shared breakpoints), and three characteristics
/// (a MAP, a CURVE, a scalar VALUE).
const SAMPLE: &str = r#"
ASAP2_VERSION 1 71
/begin PROJECT DemoProj "demo project"
  /begin HEADER "h" VERSION "1" /end HEADER
  /begin MODULE DemoMod "the module"

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
      0x8000
      no_input
      RL_AXIS
      0
      CM_RPM
      16
      0.0
      7000.0
    /end AXIS_PTS

    /begin CHARACTERISTIC map0001
      "boost target"
      MAP
      0x1000
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

    /begin CHARACTERISTIC curve0001
      "injector linearization"
      CURVE
      0x2000
      RL_MAP
      0
      CM_TAB
      0.0
      3500.0
      /begin AXIS_DESCR
        STD_AXIS volt_in CM_RPM 8 0.0 5000.0
      /end AXIS_DESCR
    /end CHARACTERISTIC

    /begin CHARACTERISTIC scalar0001
      "idle rpm setpoint"
      VALUE
      0x3000
      RL_MAP
      0
      CM_RPM
      0.0
      2000.0
    /end CHARACTERISTIC

  /end MODULE
/end PROJECT
"#;

#[test]
fn project_and_module_names() {
    let imp = parse_a2l(SAMPLE).expect("parse");
    assert_eq!(imp.project.as_deref(), Some("DemoProj"));
    assert_eq!(imp.module.as_deref(), Some("DemoMod"));
    assert_eq!(imp.maps.len(), 3);
}

#[test]
fn map_with_ratfunc_and_com_axis() {
    let imp = parse_a2l(SAMPLE).expect("parse");
    let m = imp.maps.iter().find(|m| m.name == "map0001").expect("map");

    assert_eq!(m.address, 0x1000);
    // MAP: first axis = X (16 points = cols), second axis = Y (8 points = rows).
    assert_eq!(m.cols, 16);
    assert_eq!(m.rows, 8);
    assert_eq!(m.data_type, DataType::S16);
    assert_eq!(m.endian, Endian::Big); // MSB_FIRST
    assert_eq!(m.unit, "Nm");
    assert_eq!(m.min, Some(-10.0));
    assert_eq!(m.max, Some(320.0));

    // RAT_FUNC 0 655.35 0 / 0 0 10.24  ->  factor = 10.24/655.35 = 0.015625.
    assert!((m.scaling.factor - 10.24 / 655.35).abs() < 1e-12);
    assert!(m.scaling.offset.abs() < 1e-12);

    // X axis: COM_AXIS resolved to the AXIS_PTS block.
    let x = m.x_axis.as_ref().expect("x axis");
    assert_eq!(x.count, 16);
    assert_eq!(x.address, Some(0x8000));
    assert_eq!(x.data_type, DataType::U16); // from RL_AXIS AXIS_PTS_X UWORD
    assert_eq!(x.unit, "rpm");

    // Y axis: STD_AXIS, inline — flagged that its address is not computed.
    let y = m.y_axis.as_ref().expect("y axis");
    assert_eq!(y.count, 8);
    assert!(y.address.is_none());
    assert!(
        m.notes.contains("STD_AXIS stored inline"),
        "notes: {}",
        m.notes
    );
    // The long identifier is preserved in notes.
    assert!(m.notes.contains("boost target"), "notes: {}", m.notes);
}

#[test]
fn curve_with_tab_intp_is_flagged_not_guessed() {
    let imp = parse_a2l(SAMPLE).expect("parse");
    let c = imp
        .maps
        .iter()
        .find(|m| m.name == "curve0001")
        .expect("curve");
    assert_eq!(c.rows, 1);
    assert_eq!(c.cols, 8);
    // TAB_INTP must NOT be turned into a linear scaling.
    assert_eq!(c.scaling, Scaling::default());
    assert!(
        c.notes.contains("TAB_INTP"),
        "TAB_INTP must be noted, got: {}",
        c.notes
    );
    // The physical unit is still known ("V"); only the non-linear conversion is
    // dropped (pre-scaled), never approximated into a fake factor.
    assert_eq!(c.unit, "V");
}

#[test]
fn scalar_value_is_one_by_one() {
    let imp = parse_a2l(SAMPLE).expect("parse");
    let s = imp
        .maps
        .iter()
        .find(|m| m.name == "scalar0001")
        .expect("scalar");
    assert_eq!(s.rows, 1);
    assert_eq!(s.cols, 1);
    assert_eq!(s.address, 0x3000);
    assert_eq!(s.unit, "rpm");
    assert!(s.x_axis.is_none());
    assert!(s.y_axis.is_none());
}

#[test]
fn msb_first_vs_msb_last() {
    // MSB_FIRST -> big.
    let big = parse_a2l(SAMPLE).expect("parse");
    assert_eq!(big.maps[0].endian, Endian::Big);

    // Flip the module default to MSB_LAST -> little.
    let little_src = SAMPLE.replace("BYTE_ORDER MSB_FIRST", "BYTE_ORDER MSB_LAST");
    let little = parse_a2l(&little_src).expect("parse");
    for m in &little.maps {
        assert_eq!(m.endian, Endian::Little, "map {} endianness", m.name);
    }
}

#[test]
fn record_layout_byte_order_overrides_module_default() {
    // Give RL_MAP its own MSB_LAST while the module default stays MSB_FIRST.
    let src = SAMPLE.replace(
        "FNC_VALUES 1 SWORD COLUMN_DIR DIRECT",
        "FNC_VALUES 1 SWORD COLUMN_DIR DIRECT\n      BYTE_ORDER MSB_LAST",
    );
    let imp = parse_a2l(&src).expect("parse");
    let m = imp.maps.iter().find(|m| m.name == "map0001").unwrap();
    assert_eq!(m.endian, Endian::Little);
}

#[test]
fn defaulted_endianness_is_flagged() {
    // Remove MOD_COMMON entirely -> no byte order anywhere -> assumed big + note.
    let src = SAMPLE.replace(
        "/begin MOD_COMMON \"\"\n      BYTE_ORDER MSB_FIRST\n    /end MOD_COMMON",
        "",
    );
    let imp = parse_a2l(&src).expect("parse");
    let m = &imp.maps[0];
    assert_eq!(m.endian, Endian::Big);
    assert!(m.notes.contains("assumed big-endian"), "notes: {}", m.notes);
}

#[test]
fn comment_and_whitespace_torture() {
    // Comments mid-token-stream, // line comments, tabs, CRLF, nested /* */,
    // a "" string escape, and blocks squashed onto single lines.
    let src = "ASAP2_VERSION 1 71\r\n\
/* file header comment */\r\n\
/begin PROJECT P /* inline */ \"p\"\r\n\
\t/begin MODULE M \"\" // trailing comment\r\n\
\t\t/begin MOD_COMMON \"\" BYTE_ORDER MSB_FIRST /end MOD_COMMON\r\n\
\t\t/begin RECORD_LAYOUT R FNC_VALUES 1 UBYTE COLUMN_DIR DIRECT /end RECORD_LAYOUT\r\n\
\t\t/begin COMPU_METHOD C \"say \"\"hi\"\"\" IDENTITY \"%.0\" \"deg\" /end COMPU_METHOD\r\n\
\t\t/begin CHARACTERISTIC v /* name */ \"desc\" VALUE 0x40 R 0 C 0 255 /end CHARACTERISTIC\r\n\
\t/end MODULE\r\n\
/end PROJECT\r\n";
    let imp = parse_a2l(src).expect("parse torture case");
    assert_eq!(imp.project.as_deref(), Some("P"));
    assert_eq!(imp.module.as_deref(), Some("M"));
    assert_eq!(imp.maps.len(), 1);
    let m = &imp.maps[0];
    assert_eq!(m.name, "v");
    assert_eq!(m.address, 0x40);
    assert_eq!(m.data_type, DataType::U8);
    assert_eq!(m.endian, Endian::Big);
    assert_eq!(m.unit, "deg");
    // IDENTITY conversion -> unit scaling.
    assert_eq!(m.scaling, Scaling::default());
}

#[test]
fn no_module_is_an_error() {
    assert_eq!(parse_a2l("ASAP2_VERSION 1 71"), Err(Error::NoModule));
}

#[test]
fn module_without_characteristics_is_empty() {
    let src = r#"/begin PROJECT P "" /begin MODULE M "" /end MODULE /end PROJECT"#;
    assert_eq!(parse_a2l(src), Err(Error::Empty));
}

#[test]
fn val_blk_matrix_dim() {
    let src = r#"
/begin PROJECT P ""
  /begin MODULE M ""
    /begin MOD_COMMON "" BYTE_ORDER MSB_LAST /end MOD_COMMON
    /begin RECORD_LAYOUT R FNC_VALUES 1 UWORD COLUMN_DIR DIRECT /end RECORD_LAYOUT
    /begin COMPU_METHOD C "" IDENTITY "%.0" "" /end COMPU_METHOD
    /begin CHARACTERISTIC blk "block" VAL_BLK 0x100 R 0 C 0 100
      MATRIX_DIM 4 3 1
    /end CHARACTERISTIC
  /end MODULE
/end PROJECT"#;
    let imp = parse_a2l(src).expect("parse");
    let m = &imp.maps[0];
    assert_eq!(m.cols, 4);
    assert_eq!(m.rows, 3);
    assert_eq!(m.data_type, DataType::U16);
    assert_eq!(m.endian, Endian::Little);
}

#[test]
fn unresolved_com_axis_is_flagged() {
    let src = r#"
/begin PROJECT P ""
  /begin MODULE M ""
    /begin MOD_COMMON "" BYTE_ORDER MSB_FIRST /end MOD_COMMON
    /begin RECORD_LAYOUT R FNC_VALUES 1 SWORD COLUMN_DIR DIRECT /end RECORD_LAYOUT
    /begin COMPU_METHOD C "" IDENTITY "%.0" "" /end COMPU_METHOD
    /begin CHARACTERISTIC crv "c" CURVE 0x200 R 0 C 0 100
      /begin AXIS_DESCR
        COM_AXIS in C 12 0 100
        AXIS_PTS_REF MISSING_AXIS
      /end AXIS_DESCR
    /end CHARACTERISTIC
  /end MODULE
/end PROJECT"#;
    let imp = parse_a2l(src).expect("parse");
    let m = &imp.maps[0];
    assert_eq!(m.cols, 12); // falls back to AXIS_DESCR MaxAxisPoints
    let x = m.x_axis.as_ref().expect("x axis");
    assert_eq!(x.axis_ref.as_deref(), Some("MISSING_AXIS"));
    assert!(m.notes.contains("unresolved"), "notes: {}", m.notes);
}
