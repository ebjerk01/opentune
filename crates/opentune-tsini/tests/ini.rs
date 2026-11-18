//! INI parsing on the synthetic demo definitions and hand-written snippets.
use opentune_tsini::*;
use std::path::PathBuf;

fn sample(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../samples/ts")
        .join(name)
}

fn demo(opts: &IniOptions) -> Ini {
    load_ini(&sample("opentune-demo.ini"), opts).unwrap()
}

#[test]
fn demo_header_pages_and_comms() {
    let ini = demo(&IniOptions::default());
    assert!(ini.is_clean(), "{:#?}", ini.warnings);
    assert_eq!(ini.signature.as_deref(), Some("opentune demo 1"));
    assert_eq!(ini.ini_spec_version.as_deref(), Some("3.64"));
    assert_eq!(ini.endianness, Endian::Little);
    assert_eq!(ini.pages.len(), 2);
    assert_eq!(ini.pages[0].size, 64);
    assert_eq!(ini.pages[1].size, 112);
    let p = &ini.pages[1];
    assert_eq!(p.read.as_ref().unwrap().raw, "p%2i%2o%2c");
    assert!(p.chunk_write.as_ref().unwrap().has_value());
    assert_eq!(
        p.identifier.as_ref().unwrap().parts,
        vec![CmdPart::Var("tsCanId".into()), CmdPart::Bytes(vec![2])]
    );
    assert_eq!(p.burn.as_ref().unwrap().raw, "b%2i");
    assert_eq!(p.crc32.as_ref().unwrap().raw, "d%2i");
    let c = &ini.comms;
    assert_eq!(c.blocking_factor, Some(32));
    assert!(c.uses_envelope());
    assert_eq!(c.page_activation_delay_ms, Some(10));
    assert_eq!(c.inter_write_delay_ms, Some(10));
    assert_eq!(c.block_read_timeout_ms, Some(2000));
    assert_eq!(c.delay_after_port_open_ms, Some(1000));
    assert_eq!(c.ts_write_blocks, Some(true));
    assert_eq!(c.och_block_size, Some(12));
    assert_eq!(c.query_command.as_ref().unwrap().raw, "Q");
    assert_eq!(c.version_info.as_ref().unwrap().raw, "S");
    assert_eq!(
        c.och_get_command.as_ref().unwrap().raw,
        "r\\$tsCanId\\x30%2o%2c"
    );
    // [GaugeConfigurations] is skipped on purpose, and says so.
    assert!(ini
        .warnings_of(WarnKind::Ignored)
        .any(|w| w.message.contains("[GaugeConfigurations]")));
}

#[test]
fn demo_constants() {
    let ini = demo(&IniOptions::default());
    assert_eq!(ini.constants.len(), 25);
    assert_eq!(
        ini.constant("wueBinsK").unwrap().offset,
        5,
        "lastOffset aliases wueBins"
    );
    assert_eq!(
        ini.warnings_of(WarnKind::Reference).count(),
        0,
        "{:#?}",
        ini.warnings
    );
    let wr = ini.constant("wueRates").unwrap();
    assert_eq!(wr.offset, 11);
    assert_eq!(wr.kind, Kind::Array { cols: 1, rows: 6 });
    let ve = ini.constant("veTable").unwrap();
    assert_eq!(ve.kind, Kind::Array { cols: 8, rows: 6 });
    assert_eq!(ve.page, Some(1));
    assert_eq!(ve.byte_len(), 48);
    let inj = ini.constant("injLayout").unwrap();
    match &inj.kind {
        Kind::Bits {
            lo, hi, options, ..
        } => {
            assert_eq!((*lo, *hi), (0, 1));
            assert_eq!(
                options,
                &["Paired", "Semi-Sequential", "INVALID", "Sequential"]
            );
        }
        k => panic!("{k:?}"),
    }
    let n = ini.constant("nCyl").unwrap();
    assert_eq!(n.bit_slots(), 16);
    assert_eq!(n.bit_label(3), "3");
    assert_eq!(n.bit_label(9), "9", "missing labels are synthesised");
    let afr = ini.constant("afrTarget").unwrap();
    assert_eq!(afr.scale, Num::Expr("0.1 / stoich".into()));
    let it = ini.constant("idleTemp").unwrap();
    assert_eq!(it.units, "C");
    assert_eq!(it.translate, Num::Lit(-40.0));
    let s = ini.constant("tuneName").unwrap();
    assert_eq!(
        (s.ty, s.kind.clone()),
        (DataType::Ascii, Kind::String { len: 16 })
    );
    assert!(ini.constant("unusedTail").unwrap().no_msq_save);
    let io = ini.constant("injOpen").unwrap();
    assert_eq!(
        (io.ty, io.scale.lit(), io.digits_lit()),
        (DataType::S16, Some(0.001), Some(3))
    );
    assert_eq!(io.min, Some(Num::Lit(-32.0)));
    // PC variables live outside the pages.
    assert_eq!(ini.pc_variables.len(), 3);
    assert_eq!(ini.constant("stoich").unwrap().page, None);
}

#[test]
fn demo_channels_editors_menus() {
    let ini = demo(&IniOptions::default());
    assert_eq!(ini.output_channels.len(), 16);
    let c = ini.output_channel("coolant").unwrap();
    assert_eq!(
        c.kind,
        ChannelKind::Expr {
            expr: "coolantRaw - 40".into()
        }
    );
    assert_eq!(c.units, "C");
    assert!(matches!(
        ini.output_channel("running").unwrap().kind,
        ChannelKind::Bits { lo: 0, hi: 0, .. }
    ));
    assert_eq!(ini.output_channel("map").unwrap().units, "kPa");

    assert_eq!(ini.tables.len(), 2);
    let t = ini.table("veTbl").unwrap();
    assert_eq!(t.page, Some(1));
    assert_eq!(t.title, "VE Table");
    assert_eq!(t.x_bins.as_ref().unwrap().constant, "rpmBins");
    assert_eq!(t.x_bins.as_ref().unwrap().channel.as_deref(), Some("rpm"));
    assert_eq!(t.y_bins.as_ref().unwrap().constant, "mapBins");
    assert_eq!(t.z_bins.as_deref(), Some("veTable"));
    assert_eq!(t.up_down_labels, vec!["(RICHER)", "(LEANER)"]);
    assert_eq!(t.topic_help.as_deref(), Some("Volumetric efficiency"));
    assert!(ini.table("advMap").is_some(), "lookup by map3d id");

    assert_eq!(ini.curves.len(), 2);
    let w = ini.curve("wueCurve").unwrap();
    assert_eq!(w.column_labels, vec!["Coolant", "WUE %"]);
    assert_eq!(
        w.x_bins.as_ref().unwrap().channel.as_deref(),
        Some("coolant")
    );
    assert_eq!(w.y_bins.len(), 1);
    assert_eq!(w.x_axis, vec!["-40", "120", "9"]);

    assert_eq!(ini.menus.len(), 2);
    assert_eq!(ini.menus[0].title, "&Fuel");
    assert_eq!(ini.menus[0].dialog, "main");
    assert_eq!(ini.menus[0].items.len(), 3);
    assert_eq!(ini.menus[0].items[2].condition.as_deref(), Some("nCyl > 0"));
    assert_eq!(ini.setting_groups.len(), 1);
    assert_eq!(ini.setting_groups[0].options.len(), 2);
}

#[test]
fn conditions_switch_definitions() {
    let f = demo(&IniOptions::with_set(["FAHRENHEIT"]));
    assert!(f.conditions.contains("FAHRENHEIT"));
    assert_eq!(f.constant("idleTemp").unwrap().units, "F");
    assert_eq!(
        f.output_channel("coolant").unwrap().kind,
        ChannelKind::Expr {
            expr: "(coolantRaw - 40) * 1.8 + 32".into()
        }
    );
}

const SNIPPET: &str = r#"
; leading comment
#set A
#set B
stray = 1
[MegaTune]
   signature = "snip" ; trailing comment
   queryCommand = "Q"
   somethingNew = 5
[Constants]
   endianness = big
   nPages = 1
   pageSize = 20
   pageReadCommand = "R%2o%2c"
page = 1
#if A
#  if B
   x = scalar, U08, 0, "u", 1, 0, 0, 255, 0
#  else
   x = scalar, U08, 1, "u", 1, 0, 0, 255, 0
#  endif
#elif C
   x = scalar, U08, 2, "u", 1, 0, 0, 255, 0
#else
   x = scalar, U08, 3, "u", 1, 0, 0, 255, 0
#endif
#ifdef C
   onlyC = scalar, U08, 4, "u", 1, 0, 0, 255, 0
#endif
#if !C
   notC = scalar, U08, 5, "u", 1, 0, 0, 255, 0
#endif
   long = array, U16, 6, [2x2], "u", \
          0.5, 0, 0, 100, 1
   bad = scalar, X99, 0, "u"
   badShape = array, U08, 0, [axb], "u"
   past = scalar, U32, 18, "u", 1, 0, 0, 1, 0
   x = scalar, U08, 9, "dup", 1, 0, 0, 255, 0
   withQuote = bits, U08, 14, [0:0], "semi;colon", "q\"uote"
   glitch = bits, U08, , 15, [0:2+1], "a", "b"
   fused = bits, U08, 16[1:1], "off", "on"
   glued = scalar, U08, 17, "sparks"   2.0, 0, 0, 20, 0
   alias = scalar, U08, lastOffset, "x2", 4.0, 0, 0, 20, 0
   tc = scalar, U08    19   "mins", 1, 0, 0, 255, 0
   spaced = bits, U08, 19, [0:3]  "x", "y"
[OutputChannels]
   ochBlockSize = 4
   z = { 1 + }
[TableEditor]
   table = t1, m1, "T", 1
      xBins = nope, rpm
      zBins = long
"#;

fn snip(opts: IniOptions) -> Ini {
    parse_ini(SNIPPET, &opts)
}

#[test]
fn preprocessor_and_robustness() {
    let ini = snip(IniOptions::default());
    assert_eq!(ini.constant("x").unwrap().offset, 0, "A && B");
    assert!(ini.constant("onlyC").is_none());
    assert!(ini.constant("notC").is_some());
    let l = ini.constant("long").unwrap();
    assert_eq!(l.kind, Kind::Array { cols: 2, rows: 2 });
    assert_eq!(l.scale, Num::Lit(0.5), "continuation line joined");
    match &ini.constant("withQuote").unwrap().kind {
        Kind::Bits { options, .. } => assert_eq!(options, &["semi;colon", "q\"uote"]),
        k => panic!("{k:?}"),
    }
    // Hand-editing slips TunerStudio tolerates are repaired (and noted).
    let g = ini.constant("glitch").unwrap();
    assert_eq!(g.offset, 15);
    assert!(matches!(
        g.kind,
        Kind::Bits {
            lo: 0,
            hi: 2,
            label_offset: 1,
            ..
        }
    ));
    assert_eq!(
        g.bit_label(2),
        "3",
        "[0:2+1] counts synthesised labels from 1"
    );
    let f = ini.constant("fused").unwrap();
    assert_eq!(f.offset, 16);
    assert!(matches!(f.kind, Kind::Bits { lo: 1, hi: 1, .. }));
    let gl = ini.constant("glued").unwrap();
    assert_eq!((gl.units.as_str(), gl.scale.lit()), ("sparks", Some(2.0)));
    assert_eq!(ini.constant("alias").unwrap().offset, 17);
    let tc = ini.constant("tc").unwrap();
    assert_eq!((tc.offset, tc.units.as_str()), (19, "mins"));
    match &ini.constant("spaced").unwrap().kind {
        Kind::Bits { options, .. } => assert_eq!(options, &["x", "y"]),
        k => panic!("{k:?}"),
    }
    assert!(ini
        .warnings_of(WarnKind::Ignored)
        .any(|w| w.message.contains("glitch: tolerated")));
    let msgs =
        |k: WarnKind| -> Vec<String> { ini.warnings_of(k).map(|w| w.message.clone()).collect() };
    let invalid = msgs(WarnKind::Invalid);
    assert!(
        invalid.iter().any(|m| m.contains("bad: unknown type")),
        "{invalid:?}"
    );
    assert!(
        invalid.iter().any(|m| m.contains("badShape")),
        "{invalid:?}"
    );
    let refs = msgs(WarnKind::Reference);
    assert!(
        refs.iter().any(|m| m.contains("'past' ends at byte 22")),
        "{refs:?}"
    );
    assert!(refs.iter().any(|m| m.contains("duplicate 'x'")), "{refs:?}");
    assert!(
        refs.iter().any(|m| m.contains("bins 'nope' not defined")),
        "{refs:?}"
    );
    let ign = msgs(WarnKind::Ignored);
    assert!(ign.iter().any(|m| m.contains("somethingNew")), "{ign:?}");
    assert!(
        ign.iter().any(|m| m.contains("before the first section")),
        "{ign:?}"
    );
    assert!(!ini.is_clean());
    // Warnings carry line numbers.
    let bad = ini
        .warnings
        .iter()
        .find(|w| w.message.contains("bad: unknown type"))
        .unwrap();
    assert_eq!(bad.line, 35);
    assert_eq!(bad.section, "Constants");
}

#[test]
fn user_conditions_override_file() {
    let mut o = IniOptions::default();
    o.unset.insert("B".into());
    assert_eq!(snip(o).constant("x").unwrap().offset, 1, "A && !B");
    let mut o = IniOptions::default();
    o.unset.insert("A".into());
    o.set.insert("C".into());
    let ini = snip(o);
    assert_eq!(ini.constant("x").unwrap().offset, 2, "#elif C");
    assert!(ini.constant("onlyC").is_some());
    assert!(ini.constant("notC").is_none());
    let mut o = IniOptions::default();
    o.unset.insert("A".into());
    assert_eq!(snip(o).constant("x").unwrap().offset, 3, "#else");
}

#[test]
fn ms_like_definition() {
    let ini = load_ini(&sample("opentune-demo-ms.ini"), &IniOptions::default()).unwrap();
    assert!(ini.is_clean(), "{:#?}", ini.warnings);
    assert_eq!(ini.endianness, Endian::Big);
    assert!(!ini.comms.uses_envelope());
    assert_eq!(ini.pages.len(), 1);
    assert!(ini.pages[0].identifier.is_none());
    assert!(ini.pages[0].crc32.is_none());
    assert_eq!(ini.comms.och_get_command.as_ref().unwrap().raw, "A");
    assert_eq!(ini.tables[0].page, Some(0));
}

#[test]
fn not_an_ini_is_an_error() {
    let p = std::env::temp_dir().join(format!("tsini-not-{}.ini", std::process::id()));
    std::fs::write(&p, "hello world\n").unwrap();
    assert!(load_ini(&p, &IniOptions::default()).is_err());
    let _ = std::fs::remove_file(p);
}

#[test]
fn serialises_for_front_ends() {
    let ini = demo(&IniOptions::default());
    let j = serde_json::to_value(&ini).unwrap();
    assert_eq!(j["signature"], "opentune demo 1");
    assert_eq!(j["constants"][0]["kind"]["class"], "scalar");
}
