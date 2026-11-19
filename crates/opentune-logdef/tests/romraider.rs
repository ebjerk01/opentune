//! RomRaider logger-definition import / export, and the imported definitions
//! logged end to end: SSM against the simulated module
//! (`opentune_core::protocol::ssmsim`), OBD-II against a scripted transport.
//! Synthetic fixture only (tests/fixtures/romraider_logger_synthetic.xml);
//! no RomRaider definition data is committed.
//!
//! `OPENTUNE_ROMRAIDER_LOGGER=/path/to/logger.xml` additionally runs the
//! importer over a real, separately downloaded RomRaider file (see
//! docs/LIVE-LOGGING.md); unset, that test is a no-op.
use opentune_core::ecudef::{DataType, Endian};
use opentune_core::protocol::ssm::{SsmKLine, ECU};
use opentune_core::protocol::ssmsim::{demo, SsmSim, SsmSimConfig};
use opentune_core::{CoreDiag, RawDiag};
use opentune_logdef::{
    export_romraider_logger, import_romraider_logger, poll_once, ssm, Addressing, ConversionFit,
    FastLogConfig, LiveSession, LogDef, LogMode, ParamStatus, RomRaiderImport,
    RomRaiderImportOptions, Sample, SsmPollConfig, SsmTarget, ValueKind,
};

const XML: &str = include_str!("fixtures/romraider_logger_synthetic.xml");
const ROM: &str = "5A5A000001";

fn import(protocol: Option<&str>, ecu: Option<&str>) -> Result<RomRaiderImport, String> {
    import_romraider_logger(
        XML,
        &RomRaiderImportOptions {
            protocol: protocol.map(str::to_string),
            ecu: ecu.map(str::to_string),
        },
    )
}

fn status(imp: &RomRaiderImport, id: &str) -> (ParamStatus, String) {
    let p = imp
        .params
        .iter()
        .find(|p| p.id == id)
        .unwrap_or_else(|| panic!("{id} not imported"));
    (p.status, p.detail.clone())
}

fn param<'a>(def: &'a LogDef, id: &str) -> &'a opentune_logdef::LogParam {
    def.params()
        .find(|(_, _, p)| p.id == id)
        .unwrap_or_else(|| panic!("{id} missing"))
        .2
}

fn value(samples: &[Sample], id: &str) -> Option<f64> {
    samples
        .iter()
        .find(|s| s.param_id == id)
        .unwrap_or_else(|| panic!("no sample for {id}"))
        .value
}

fn close(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < 1e-9)
}

#[test]
fn ssm_standard_parameters_are_classified() {
    let imp = import(None, None).unwrap();
    assert_eq!(imp.protocol, "SSM");
    assert!(imp.def.validate().is_empty());
    use ParamStatus::*;
    for (id, want) in [
        ("P1", Linear),
        ("P2", Linear),
        ("P3", Linear),
        ("P4", Expression),
        ("P5", Refused),
        ("P6", Linear),
        ("P7", Refused),
        ("P8", Text),
        ("P9", Linear),
        ("P200", Calculated),
        ("P201", Refused),
        ("P202", Calculated),
        ("P203", Refused),
        ("P204", Refused),
        ("P205", Refused),
        ("P206", Refused),
        ("P207", Refused),
        ("S1", Switch),
    ] {
        assert_eq!(status(&imp, id).0, want, "{id}: {:?}", status(&imp, id));
    }
    // Reasons are specific.
    assert!(status(&imp, "P5").1.contains("unsupported expression"));
    assert!(status(&imp, "P7").1.contains("3-byte SSM range"));
    assert!(status(&imp, "P201").1.contains("per-ECU parameter"));
    assert!(status(&imp, "P201").1.contains("--ecu"));
    assert!(status(&imp, "P203").1.contains("does not reference"));
    assert!(status(&imp, "P204").1.contains("'knots'"));
    assert!(status(&imp, "P205").1.contains("circular"));
    assert!(status(&imp, "P207").1.contains("P5"));
    assert!(status(&imp, "P200").1.contains("from P3, P2"));
    assert!(status(&imp, "P6").1.contains("'state' dropped"));

    // Refused parameters are disabled and can never produce a number.
    for id in ["P5", "P201", "P205"] {
        let p = param(&imp.def, id);
        assert_eq!(p.enabled, Some(false));
        assert!(matches!(
            p.conversion.as_ref().unwrap().fit,
            ConversionFit::NonLinear { .. }
        ));
    }
    // Unusable alternative conversion dropped, usable one kept.
    let p6 = param(&imp.def, "P6");
    assert_eq!(p6.conversions.len(), 1);
    let p1 = param(&imp.def, "P1");
    assert_eq!(p1.conversions.len(), 2);
    assert_eq!(p1.min, Some(-40.0));
    // Fragments, storage and endian carried over.
    assert_eq!(
        param(&imp.def, "P2").addressing,
        Addressing::Memory {
            address: 0x1001,
            length: 2,
            bit: None
        }
    );
    let p9 = param(&imp.def, "P9");
    assert_eq!(
        (p9.storagetype, p9.endian),
        (Some(DataType::S16), Endian::Little)
    );
    assert_eq!(param(&imp.def, "P8").value_kind, ValueKind::Text);

    // File-level notes: other protocols, per-ECU params, replace labels, dtcodes.
    let notes = imp.notes.join("\n");
    for want in [
        "OBD, DS2",
        "2 per-ECU parameter(s) across 3 ECU id(s)",
        "<replace> value label",
        "dtcodes",
    ] {
        assert!(notes.contains(want), "missing '{want}' in:\n{notes}");
    }
    // No per-ECU group without --ecu.
    assert_eq!(imp.def.protocols[0].ecus.len(), 1);
}

#[test]
fn ecu_option_adds_that_ecus_parameters() {
    let imp = import(Some("ssm"), Some("5a5a0000aa")).unwrap();
    assert_eq!(status(&imp, "E1").0, ParamStatus::Linear);
    assert_eq!(status(&imp, "P201").0, ParamStatus::Calculated);
    let groups = &imp.def.protocols[0].ecus;
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[1].id.as_deref(), Some("5a5a0000aa"));
    assert_eq!(
        param(&imp.def, "E1").addressing,
        Addressing::Memory {
            address: 0xFF80,
            length: 4,
            bit: None
        }
    );
    // E2 is only defined for ...0001.
    assert!(imp.params.iter().all(|p| p.id != "E2"));
    let imp = import(None, Some(ROM)).unwrap();
    assert_eq!(status(&imp, "E2").0, ParamStatus::Switch);
    assert!(imp.def.validate().is_empty());
}

#[test]
fn doctype_declaration_is_accepted() {
    let xml = XML.replacen(
        "<logger version",
        "<!DOCTYPE logger SYSTEM \"logger.dtd\">\n<logger version",
        1,
    );
    assert!(xml.contains("<!DOCTYPE"));
    let imp = import_romraider_logger(&xml, &RomRaiderImportOptions::default()).unwrap();
    assert_eq!(imp.params.len(), import(None, None).unwrap().params.len());
    // Entity expansion is bounded, not followed forever.
    let bomb = r#"<?xml version="1.0"?><!DOCTYPE logger [
      <!ENTITY a "aaaaaaaaaa"><!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;">
      <!ENTITY c "&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;"><!ENTITY d "&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;">
      <!ENTITY e "&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;"><!ENTITY f "&e;&e;&e;&e;&e;&e;&e;&e;&e;&e;">
      <!ENTITY g "&f;&f;&f;&f;&f;&f;&f;&f;&f;&f;"><!ENTITY h "&g;&g;&g;&g;&g;&g;&g;&g;&g;&g;">
      ]><logger><protocols><protocol id="SSM"><parameters>
      <parameter id="P1" name="&h;" desc=""><address>0x1</address></parameter>
      </parameters></protocol></protocols></logger>"#;
    assert!(opentune_logdef::parse(bomb).is_err());
}

#[test]
fn bad_choices_are_errors() {
    let e = import(None, Some("0000000000")).unwrap_err();
    assert!(e.contains("0000000000") && e.contains("3 ECU id(s)"), "{e}");
    let e = import(Some("DS2"), None).unwrap_err();
    assert!(e.contains("DS2") && e.contains("no live transport"), "{e}");
    let e = import(Some("NOPE"), None).unwrap_err();
    assert!(e.contains("SSM, OBD, DS2"), "{e}");
    let ecus = r##"<ecus><logprotocols><logprotocol type="SSM"><ecu id="b">
        <parameter id="A" offset="#0001" expr="[value]" metric=""/></ecu></logprotocol></logprotocols></ecus>"##;
    let e = import_romraider_logger(ecus, &RomRaiderImportOptions::default()).unwrap_err();
    assert!(e.contains("not a RomRaider <logger>"), "{e}");
}

#[test]
fn obd_parameters_import_with_widths_and_calculations() {
    let imp = import(Some("OBD"), None).unwrap();
    assert!(imp.def.validate().is_empty());
    assert_eq!(
        param(&imp.def, "O12").addressing,
        Addressing::ObdPid { pid: 0x0C }
    );
    assert_eq!(param(&imp.def, "O20").storagetype, Some(DataType::U8));
    assert_eq!(status(&imp, "O1B").0, ParamStatus::Refused);
    assert_eq!(status(&imp, "O200").0, ParamStatus::Calculated);
    assert!(imp.notes.iter().any(|n| n.contains("capability bits")));
}

/// Import → export → import reproduces the definition and the report.
#[test]
fn export_round_trips() {
    for (proto, ecu) in [(None, None), (None, Some(ROM)), (Some("OBD"), None)] {
        let first = import(proto, ecu).unwrap();
        let (xml, notes) = export_romraider_logger(&first.def);
        assert!(notes.is_empty(), "{notes:?}");
        let again = import_romraider_logger(
            &xml,
            &RomRaiderImportOptions {
                protocol: proto.map(str::to_string),
                ecu: ecu.map(str::to_string),
            },
        )
        .unwrap_or_else(|e| panic!("{e}\n{xml}"));
        assert_eq!(again.def, first.def, "{xml}");
        // Same outcome per parameter (a dropped conversion is only reported
        // the first time: the export no longer contains it).
        let outcome = |i: &RomRaiderImport| -> Vec<(String, ParamStatus)> {
            i.params.iter().map(|p| (p.id.clone(), p.status)).collect()
        };
        assert_eq!(outcome(&again), outcome(&first));
        // And through the JSON a pack or `log --def` reads.
        let json = serde_json::to_string(&first.def).unwrap();
        assert_eq!(opentune_logdef::load(&json).unwrap(), first.def);
    }
}

#[test]
fn export_lists_what_it_leaves_out() {
    let ecus = r##"<ecus><ecu_tools><convert_factor type="t" name="F" metric="F" expr="[value]*2"/></ecu_tools>
      <logprotocols><logprotocol type="SSM"><ecu id="b">
        <parameter id="A" offset="#0001" storagetype="uint8" type="t" expr="[value]" metric=""><alt id="x" offset="#0002"/></parameter>
      </ecu></logprotocol></logprotocols></ecus>"##;
    let def = opentune_logdef::parse(ecus).unwrap();
    let (_xml, notes) = export_romraider_logger(&def);
    let notes = notes.join("\n");
    assert!(notes.contains("<convert_factor>"), "{notes}");
    assert!(notes.contains("<alt>"), "{notes}");
    assert!(notes.contains("unit-conversion group"), "{notes}");
}

/// Fixed demo-module RAM: coolant 90 °C, 800 rpm, 50 km/h, switch on, ROM
/// float 1.5, a 4-character tag and a signed little-endian trim of -2.00 %.
fn static_sim() -> SsmSim {
    let mut cfg = SsmSimConfig::demo();
    cfg.animate = false;
    let ram = &mut cfg.ram;
    ram.insert(demo::COOLANT, 130);
    ram.insert(demo::RPM, 0x0C);
    ram.insert(demo::RPM + 1, 0x80);
    ram.insert(demo::SPEED, 50);
    ram.insert(demo::SWITCHES, 0b0000_1000);
    for (i, b) in 1.5f32.to_be_bytes().into_iter().enumerate() {
        ram.insert(demo::ROM_FLOAT + i as u32, b);
    }
    for (i, b) in b"TAG1".iter().enumerate() {
        ram.insert(0x1040 + i as u32, *b);
    }
    ram.insert(0x1006, 0x38);
    ram.insert(0x1007, 0xFF);
    SsmSim::new(cfg)
}

fn ssm_log(def: &LogDef, ids: &[&str]) -> (Vec<String>, Vec<Sample>, Vec<String>) {
    let mut sim = static_sim();
    let mut kl = SsmKLine::open(&mut sim, ECU).unwrap();
    let mut diag = CoreDiag::new(&mut kl, 200);
    let init = ssm::identify(&mut diag).unwrap();
    let sel = ssm::select(def, &init, SsmTarget::Ecu);
    let cfg = FastLogConfig {
        ssm: Some(SsmPollConfig::default()),
        ..FastLogConfig::default()
    };
    let mut session = LiveSession::new(&sel.def, ids, cfg).unwrap();
    assert_eq!(session.start(&mut diag).mode, LogMode::Ssm);
    let samples = session.tick(&mut diag);
    let channels = session.params().iter().map(|p| p.id.clone()).collect();
    (channels, samples, sel.notes)
}

/// The imported definition logs against the SSM simulator: scaled,
/// expression, text, signed, switch, per-ECU and calculated channels.
#[test]
fn imported_ssm_definition_logs_against_the_simulator() {
    let imp = import(None, Some(ROM)).unwrap();
    let (channels, s, notes) = ssm_log(&imp.def, &[]);
    // Refused / disabled channels are not logged by default.
    for id in ["P5", "P7", "P205"] {
        assert!(!channels.iter().any(|c| c == id), "{id} logged");
    }
    assert!(close(value(&s, "P1"), 90.0));
    assert!(close(value(&s, "P2"), 800.0));
    assert!(close(value(&s, "P3"), 50.0));
    assert!(close(value(&s, "P4"), 14.7 / 51.0));
    assert!(close(value(&s, "P9"), -2.0));
    assert!(close(value(&s, "S1"), 1.0));
    assert!(close(value(&s, "E1"), 1.5));
    assert!(close(value(&s, "E2"), 1.0));
    // [P3:mph] = 25, * 800 rpm / 1000.
    assert!(close(value(&s, "P200"), 20.0));
    assert!(close(value(&s, "P202"), 40.0));
    assert!(close(value(&s, "P201"), 91.5));
    let p200 = s.iter().find(|x| x.param_id == "P200").unwrap();
    assert_eq!((p200.units.as_str(), p200.note.as_str()), ("x", ""));
    let tag = s.iter().find(|x| x.param_id == "P8").unwrap();
    assert_eq!(tag.text.as_deref(), Some("TAG1"));
    // The address past the SSM range is left out with a reason.
    assert!(notes.iter().any(|n| n.contains("P7")), "{notes:?}");

    // Selecting only a calculated channel brings its inputs along.
    let (channels, s, _) = ssm_log(&imp.def, &["P202"]);
    assert_eq!(channels, ["P3", "P2", "P200", "P202"]);
    assert!(close(value(&s, "P202"), 40.0));
}

/// Without its ROM's per-ECU input a calculated parameter is not selected
/// for an SSM module (and the importer already refused it).
#[test]
fn ssm_selection_drops_calculations_whose_inputs_are_missing() {
    let imp = import(None, Some("5A5A000002")).unwrap();
    // The simulator is ...0001: E1 for ...0002 is not this ROM.
    let (channels, _s, notes) = ssm_log(&imp.def, &[]);
    assert!(!channels.iter().any(|c| c == "E1" || c == "P201"));
    assert!(
        notes
            .iter()
            .any(|n| n.contains("does not log") && n.contains("P201")),
        "{notes:?}"
    );
}

/// OBD-II: answers mode-01 PIDs from a table.
struct Obd {
    pids: Vec<(u8, Vec<u8>)>,
}

impl RawDiag for Obd {
    fn request(&mut self, req: &[u8]) -> Result<Vec<u8>, String> {
        match req {
            [0x01, pid] => Ok(match self.pids.iter().find(|(p, _)| p == pid) {
                Some((_, data)) => [vec![0x41, *pid], data.clone()].concat(),
                None => vec![0x7F, 0x01, 0x12],
            }),
            _ => Err("unexpected request".into()),
        }
    }
}

#[test]
fn imported_obd_definition_polls_with_calculations() {
    let imp = import(Some("OBD"), None).unwrap();
    let mut ecu = Obd {
        pids: vec![
            (0x05, vec![130]),
            (0x0C, vec![0x1A, 0xF8]),
            (0x14, vec![0x5A, 0x80]),
        ],
    };
    let s = poll_once(&mut ecu, &imp.def, &[]);
    assert!(close(value(&s, "O5"), 90.0));
    assert!(close(value(&s, "O12"), 1726.0));
    assert!(close(value(&s, "O200"), 17.26));
    // One byte of the two-value PID 14, not 0x5A80.
    assert!(close(value(&s, "O20"), 0.45));
    assert!(s.iter().all(|x| x.param_id != "O1B"));

    // A failed input read leaves the calculation empty, with the reason.
    let mut ecu = Obd {
        pids: vec![(0x05, vec![130])],
    };
    let s = poll_once(&mut ecu, &imp.def, &["O200"]);
    assert_eq!(s.len(), 2);
    let o200 = s.iter().find(|x| x.param_id == "O200").unwrap();
    assert_eq!(o200.value, None);
    assert!(o200.note.contains("O12 has no value"), "{}", o200.note);
}

/// A real RomRaider `logger.xml`, downloaded separately and never committed
/// (its definitions carry no redistribution licence). Gated on
/// `OPENTUNE_ROMRAIDER_LOGGER`.
#[test]
fn real_romraider_logger_imports_and_round_trips() {
    let Some(path) = std::env::var_os("OPENTUNE_ROMRAIDER_LOGGER") else {
        eprintln!("OPENTUNE_ROMRAIDER_LOGGER not set; skipping the real-file import test");
        return;
    };
    let raw = std::fs::read(&path).expect("read OPENTUNE_ROMRAIDER_LOGGER");
    let xml = String::from_utf8_lossy(&raw);
    let src = opentune_logdef::parse(&xml).expect("parse");
    let ssm = src
        .protocols
        .iter()
        .find(|p| p.id == "SSM")
        .expect("an SSM protocol");
    let rom = ssm
        .ecus
        .iter()
        .find_map(|g| g.id.as_deref())
        .map(|ids| ids.split(',').next().unwrap_or(ids).trim().to_string());
    for ecu in [None, rom] {
        let opts = RomRaiderImportOptions {
            protocol: Some("SSM".into()),
            ecu: ecu.clone(),
        };
        let imp = import_romraider_logger(&xml, &opts).expect("import");
        assert!(imp.def.validate().is_empty());
        assert!(imp.params.len() > 50, "{} params", imp.params.len());
        assert!(imp.count(ParamStatus::Calculated) > 0);
        for p in &imp.params {
            if p.status == ParamStatus::Refused {
                assert!(!p.detail.is_empty(), "{} refused without a reason", p.id);
            }
        }
        let summary: Vec<String> = [
            ParamStatus::Linear,
            ParamStatus::Expression,
            ParamStatus::Calculated,
            ParamStatus::Switch,
            ParamStatus::Text,
            ParamStatus::Refused,
        ]
        .iter()
        .map(|s| format!("{} {}", imp.count(*s), s.as_str()))
        .collect();
        eprintln!("ecu {ecu:?}: {}", summary.join(", "));
        for p in imp
            .params
            .iter()
            .filter(|p| p.status == ParamStatus::Refused)
        {
            eprintln!("  refused {}: {}", p.id, p.detail);
        }
        let (out, _notes) = export_romraider_logger(&imp.def);
        let again = import_romraider_logger(&out, &opts).expect("re-import");
        assert_eq!(again.def, imp.def);
        let outcome = |i: &RomRaiderImport| -> Vec<(String, ParamStatus)> {
            i.params.iter().map(|p| (p.id.clone(), p.status)).collect()
        };
        assert_eq!(outcome(&again), outcome(&imp));
    }
}
