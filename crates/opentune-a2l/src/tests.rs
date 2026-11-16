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
fn std_axis_address_computed_from_record_layout() {
    // A CURVE whose record layout stores the X breakpoints inline, BEFORE the
    // function values: AXIS_PTS_X (position 1) then FNC_VALUES (position 2). The
    // X axis begins at the characteristic address; the table data follows it.
    let src = r#"
/begin PROJECT P ""
  /begin MODULE M ""
    /begin MOD_COMMON "" BYTE_ORDER MSB_FIRST /end MOD_COMMON
    /begin RECORD_LAYOUT RL_STD
      AXIS_PTS_X 1 UWORD INDEX_INCR DIRECT
      FNC_VALUES 2 UBYTE COLUMN_DIR DIRECT
    /end RECORD_LAYOUT
    /begin COMPU_METHOD C "" IDENTITY "%.0" "" /end COMPU_METHOD
    /begin CHARACTERISTIC crv "c" CURVE 0x1000 RL_STD 0 C 0 100
      /begin AXIS_DESCR
        STD_AXIS rpm_in C 8 0.0 7000.0
      /end AXIS_DESCR
    /end CHARACTERISTIC
  /end MODULE
/end PROJECT"#;
    let imp = parse_a2l(src).expect("parse");
    let m = &imp.maps[0];
    assert_eq!(m.cols, 8);
    // 8 UWORD axis points = 16 bytes precede the function values.
    assert_eq!(
        m.address,
        0x1000 + 16,
        "table data address shifted past axis"
    );
    let x = m.x_axis.as_ref().expect("x axis");
    assert_eq!(x.address, Some(0x1000), "inline axis at char base");
    assert_eq!(x.data_type, DataType::U16);
    assert_eq!(x.count, 8);
    assert!(m.notes.contains("address computed"), "notes: {}", m.notes);
}

#[test]
fn std_axis_address_declined_when_layout_unmodeled() {
    // Same inline axis, but the record layout has an ALIGNMENT entry whose
    // padding this crate does not model: the address must NOT be guessed.
    let src = r#"
/begin PROJECT P ""
  /begin MODULE M ""
    /begin MOD_COMMON "" BYTE_ORDER MSB_FIRST /end MOD_COMMON
    /begin RECORD_LAYOUT RL_STD
      AXIS_PTS_X 1 UWORD INDEX_INCR DIRECT
      ALIGNMENT_WORD 2
      FNC_VALUES 2 UBYTE COLUMN_DIR DIRECT
    /end RECORD_LAYOUT
    /begin COMPU_METHOD C "" IDENTITY "%.0" "" /end COMPU_METHOD
    /begin CHARACTERISTIC crv "c" CURVE 0x1000 RL_STD 0 C 0 100
      /begin AXIS_DESCR
        STD_AXIS rpm_in C 8 0.0 7000.0
      /end AXIS_DESCR
    /end CHARACTERISTIC
  /end MODULE
/end PROJECT"#;
    let imp = parse_a2l(src).expect("parse");
    let m = &imp.maps[0];
    assert_eq!(m.address, 0x1000, "address left at char base (no guess)");
    let x = m.x_axis.as_ref().expect("x axis");
    assert!(x.address.is_none(), "inline axis address not guessed");
    assert!(
        m.notes.contains("address not computed"),
        "notes: {}",
        m.notes
    );
}

#[test]
fn cuboid_sliced_into_separate_maps() {
    // A 3-D CUBOID (X=4, Y=3, Z=2) becomes two 2-D X*Y faces, each a MapDef.
    let src = r#"
/begin PROJECT P ""
  /begin MODULE M ""
    /begin MOD_COMMON "" BYTE_ORDER MSB_FIRST /end MOD_COMMON
    /begin RECORD_LAYOUT R FNC_VALUES 1 UWORD COLUMN_DIR DIRECT /end RECORD_LAYOUT
    /begin COMPU_METHOD C "" IDENTITY "%.0" "deg" /end COMPU_METHOD
    /begin CHARACTERISTIC cube "3d table" CUBOID 0x5000 R 0 C 0 100
      /begin AXIS_DESCR STD_AXIS x_in C 4 0 100 /end AXIS_DESCR
      /begin AXIS_DESCR STD_AXIS y_in C 3 0 100 /end AXIS_DESCR
      /begin AXIS_DESCR STD_AXIS z_in C 2 0 100 /end AXIS_DESCR
    /end CHARACTERISTIC
  /end MODULE
/end PROJECT"#;
    let imp = parse_a2l(src).expect("parse");
    assert_eq!(imp.maps.len(), 2, "two Z slices");
    let (s0, s1) = (&imp.maps[0], &imp.maps[1]);
    // Each face is 4x3 UWORD = 24 bytes.
    assert_eq!(s0.cols, 4);
    assert_eq!(s0.rows, 3);
    assert_eq!(s0.address, 0x5000);
    assert_eq!(s1.address, 0x5000 + 24, "second face one plane along");
    assert!(s0.name.contains("z_in=0"), "name: {}", s0.name);
    assert!(s1.name.contains("z_in=1"), "name: {}", s1.name);
    assert!(s0.notes.contains("flattened"), "notes: {}", s0.notes);
    assert_eq!(s0.unit, "deg");
}

#[test]
fn cuboid_slice_cap_flags_overflow() {
    // A CUBOID whose Z axis exceeds the slice cap: the surplus is flagged, not
    // silently dropped.
    let big_z = super::CUBOID_SLICE_CAP + 5;
    let src = format!(
        r#"
/begin PROJECT P ""
  /begin MODULE M ""
    /begin MOD_COMMON "" BYTE_ORDER MSB_FIRST /end MOD_COMMON
    /begin RECORD_LAYOUT R FNC_VALUES 1 UBYTE COLUMN_DIR DIRECT /end RECORD_LAYOUT
    /begin COMPU_METHOD C "" IDENTITY "%.0" "" /end COMPU_METHOD
    /begin CHARACTERISTIC cube "big" CUBOID 0x6000 R 0 C 0 100
      /begin AXIS_DESCR STD_AXIS x_in C 2 0 100 /end AXIS_DESCR
      /begin AXIS_DESCR STD_AXIS y_in C 2 0 100 /end AXIS_DESCR
      /begin AXIS_DESCR STD_AXIS z_in C {big_z} 0 100 /end AXIS_DESCR
    /end CHARACTERISTIC
  /end MODULE
/end PROJECT"#
    );
    let imp = parse_a2l(&src).expect("parse");
    assert_eq!(imp.maps.len(), super::CUBOID_SLICE_CAP as usize);
    assert!(
        imp.maps[0].notes.contains("omitted"),
        "notes: {}",
        imp.maps[0].notes
    );
}

#[test]
fn empty_and_garbage_inputs_are_clean_errors() {
    assert!(parse_a2l("").is_err());
    assert!(parse_a2l("\0\0\0\0").is_err());
    assert!(parse_a2l("not an a2l file at all").is_err());
    // A truncated /begin with no /end must not panic.
    assert!(parse_a2l("/begin MODULE M").is_err());
}

#[test]
fn fix_axis_par_huge_count_is_capped() {
    // A FIX_AXIS_PAR_DIST declaring billions of points must not try to allocate
    // them; the generated axis is capped instead of exhausting memory.
    let src = r#"
/begin PROJECT P ""
  /begin MODULE M ""
    /begin MOD_COMMON "" BYTE_ORDER MSB_FIRST /end MOD_COMMON
    /begin RECORD_LAYOUT R FNC_VALUES 1 UBYTE COLUMN_DIR DIRECT /end RECORD_LAYOUT
    /begin COMPU_METHOD C "" IDENTITY "%.0" "" /end COMPU_METHOD
    /begin CHARACTERISTIC crv "c" CURVE 0x1000 R 0 C 0 100
      /begin AXIS_DESCR
        FIX_AXIS in C 8 0 100
        FIX_AXIS_PAR_DIST 0 10 4000000000
      /end AXIS_DESCR
    /end CHARACTERISTIC
  /end MODULE
/end PROJECT"#;
    let imp = parse_a2l(src).expect("parse");
    let x = imp.maps[0].x_axis.as_ref().expect("x axis");
    assert!(
        x.values.len() <= super::MAX_AXIS_POINTS as usize,
        "axis points must be capped, got {}",
        x.values.len()
    );
}

#[test]
fn fuzz_random_inputs_never_panic() {
    // Deterministic pseudo-random fuzz: mutate the known-good sample and feed
    // pure garbage. The only requirement is that the parser returns (Ok or Err)
    // and never panics, overflows or hangs. Kept small so CI stays cheap.
    let mut state: u64 = 0x2026_1116_a210_cbf0;
    let mut rng = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let seed = SAMPLE.as_bytes();
    for _ in 0..512 {
        let mut buf: Vec<u8> = seed.to_vec();
        let edits = (rng() % 40) as usize;
        for _ in 0..edits {
            if buf.is_empty() {
                break;
            }
            let idx = (rng() as usize) % buf.len();
            buf[idx] = (rng() & 0xff) as u8;
        }
        // Also append some random bytes (including structural tokens).
        for _ in 0..(rng() % 32) {
            buf.push((rng() & 0xff) as u8);
        }
        let s = String::from_utf8_lossy(&buf);
        let _ = parse_a2l(&s); // must not panic
    }
    // A handful of all-structural-token streams (unbalanced begins/ends).
    for n in [1usize, 10, 100] {
        let s = "/begin A ".repeat(n) + &"/end A ".repeat(n / 2);
        let _ = parse_a2l(&s);
    }
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
