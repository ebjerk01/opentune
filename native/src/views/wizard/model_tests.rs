//! The wizard's pure logic on SYNTHETIC data (the webview app's
//! `wizard.test.mjs` cases, ported): the step machine, plain-language
//! errors, the voltage bands, the scan summary, explanations that only
//! repeat the data, the live rating and the report.

use super::model::*;
use opentune_autoscan::{
    Dtc as ModDtc, Identity as ModIdentity, ModuleResult as ModResult, System,
};
use opentune_core::obd::inspect::{
    DtcEntry, DtcReport, Ignition, Monitor, MonitorState, MonitorStatus, Readiness, VehicleInfo,
};
use opentune_core::obd::Inspection;
use opentune_core::Capabilities;
use std::collections::{BTreeMap, BTreeSet};

const TEST_VIN: &str = "TESTVCAR0D0000001";

fn mon(key: &'static str, name: &'static str, state: MonitorState) -> Monitor {
    Monitor {
        key,
        name,
        continuous: false,
        state,
    }
}

fn dtc(code: &str, desc: &str) -> DtcEntry {
    DtcEntry {
        code: code.into(),
        description: desc.into(),
        system: "powertrain",
    }
}

const CAT: &str = "Catalyst System Efficiency Below Threshold Bank 1";

fn readiness(mil: bool, monitors: Vec<Monitor>) -> Option<Readiness> {
    Some(Readiness {
        status: MonitorStatus {
            mil,
            dtc_count: 0,
            ignition: Ignition::Spark,
            monitors,
        },
        this_cycle: None,
        this_cycle_error: None,
    })
}

/// Engine with the MIL on and codes in all three OBD lists, plus a gearbox
/// whose mode 0A failed.
fn obd_scan() -> ObdScan {
    let engine = Inspection {
        responded: true,
        readiness: readiness(
            true,
            vec![
                mon("misfire", "Misfire", MonitorState::Complete),
                mon("catalyst", "Catalyst", MonitorState::Incomplete),
                mon(
                    "secondary_air",
                    "Secondary air system",
                    MonitorState::NotSupported,
                ),
            ],
        ),
        dtcs: Some(DtcReport {
            stored: vec![dtc("P0420", CAT)],
            pending: vec![dtc("P0171", "System Too Lean Bank 1")],
            permanent: vec![dtc("P0420", CAT)],
            errors: BTreeMap::new(),
        }),
        ..Default::default()
    };
    let tcm = Inspection {
        responded: true,
        readiness: readiness(false, vec![]),
        dtcs: Some(DtcReport {
            errors: [("permanent".to_string(), "mode 0A not supported".to_string())].into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    ObdScan {
        ecus: vec![
            ObdEcu {
                tx: 0x7E0,
                rx: 0x7E8,
                ins: engine,
            },
            ObdEcu {
                tx: 0x7E1,
                rx: 0x7E9,
                ins: tcm,
            },
        ],
        silent: vec!["7E2".into(), "7E3".into()],
    }
}

fn mod_dtc(code: &str, desc: &str, sys: System, status: u8) -> ModDtc {
    ModDtc {
        code: code.into(),
        description: desc.into(),
        system: sys,
        status: Some(status),
    }
}

fn module_scan() -> ModuleScan {
    ModuleScan {
        vehicle: "Demo car".into(),
        modules: vec![
            ModResult {
                name: "Engine".into(),
                present: true,
                identity: Some(ModIdentity {
                    vin: Some(TEST_VIN.into()),
                    dids: vec![],
                }),
                dtcs: vec![mod_dtc("P0420", CAT, System::Powertrain, 0x28)],
                error: None,
            },
            ModResult {
                name: "ABS".into(),
                present: true,
                identity: None,
                dtcs: vec![mod_dtc(
                    "C1234",
                    "Manufacturer-specific / unknown code",
                    System::Chassis,
                    0x2F,
                )],
                error: None,
            },
            ModResult {
                name: "Seat memory".into(),
                present: false,
                identity: None,
                dtcs: vec![],
                error: None,
            },
        ],
    }
}

// ---- state machine ---------------------------------------------------------

#[test]
fn steps_run_fail_retry_skip_and_next_only_when_resolved() {
    let mut r = Run::new(0);
    let ids: Vec<&str> = StepId::ALL.iter().map(|s| s.title()).collect();
    assert_eq!(
        ids,
        [
            "Connect",
            "Identify",
            "Scan",
            "Explain",
            "Live check",
            "Report"
        ]
    );
    assert!(!r.can_next(), "a pending step blocks Next");
    r.begin(StepId::Connect);
    assert!(r.busy());
    r.skip(StepId::Connect);
    assert_eq!(
        r.status(StepId::Connect),
        Status::Running,
        "a running step cannot be skipped"
    );
    r.fail(StepId::Connect, "adapter busy: Dashboard");
    assert_eq!(r.status(StepId::Connect), Status::Failed);
    let e = r.step(StepId::Connect).error.clone().unwrap();
    assert!(e.text.contains("Another panel is using the adapter"));
    assert_eq!(e.raw, "adapter busy: Dashboard");
    assert!(!r.can_next(), "a failed step blocks Next");
    r.begin(StepId::Connect);
    r.succeed(StepId::Connect, StepData::Report);
    assert_eq!(r.step(StepId::Connect).attempts, 2);
    assert!(r.step(StepId::Connect).error.is_none());
    assert!(r.can_next());
    r.next();
    assert_eq!(r.current, 1);
    assert!(!r.reachable(3), "cannot jump past an unresolved step");
    r.skip(StepId::Identify);
    r.next();
    r.go_to(0);
    assert_eq!(r.current, 0);
    r.go_to(2);
    assert_eq!(r.current, 2);
    r.back();
    assert_eq!(r.current, 1);
    // Busy: no navigation.
    r.begin(StepId::Scan);
    r.back();
    r.go_to(0);
    assert_eq!(r.current, 1);
}

#[test]
fn rerunning_or_skipping_the_scan_makes_explain_stale() {
    let mut r = Run::new(0);
    r.succeed(StepId::Scan, StepData::Report);
    r.succeed(StepId::Explain, StepData::Explain(vec![]));
    r.succeed(StepId::Scan, StepData::Report);
    assert_eq!(r.status(StepId::Explain), Status::Pending);
    r.succeed(StepId::Explain, StepData::Explain(vec![]));
    r.skip(StepId::Scan);
    assert_eq!(r.status(StepId::Explain), Status::Pending);
    assert!(r.step(StepId::Explain).data.is_none());
    r.succeed(StepId::Explain, StepData::Explain(vec![]));
    r.fail(StepId::Scan, "x");
    assert_eq!(r.status(StepId::Explain), Status::Pending);
}

// ---- errors ----------------------------------------------------------------

#[test]
fn errors_are_plain_language_with_the_raw_text_kept() {
    let cases = [
        ("adapter busy: Inspection is running", "Another panel"),
        (
            "open /dev/ttyUSB0: No such file or directory (os error 2)",
            "could not be opened",
        ),
        (
            "no adapter connected: pick a cable and connect it in the top bar",
            "could not be opened",
        ),
        (
            "no response from 7E0/7E8 within 1000 ms (ISO-TP timeout)",
            "car did not answer",
        ),
        ("UNABLE TO CONNECT", "could not talk to the car"),
        (
            "parse module list: missing field",
            "module list could not be read",
        ),
        (
            "K-line cannot carry diagnostics: no diagnostic link (CAN or DoIP)",
            "cannot carry diagnostic requests",
        ),
        ("weird failure 42", "Something went wrong"),
    ];
    for (raw, want) in cases {
        let e = error_info(raw);
        assert!(e.text.contains(want), "{raw} -> {}", e.text);
        assert_eq!(e.raw, raw);
    }
    assert!(
        !error_info("PID not supported by ECU")
            .text
            .contains("could not be opened"),
        "'supported' is not a port"
    );
    assert!(error_info("bad port name")
        .text
        .contains("could not be opened"));
}

// ---- voltage / connect -------------------------------------------------------

#[test]
fn voltage_bands_and_unknown_without_a_reading() {
    assert_eq!(voltage_verdict(None).level, Level::Unknown);
    assert_eq!(voltage_verdict(Some(10_900)).level, Level::Bad);
    assert_eq!(voltage_verdict(Some(11_000)).level, Level::Warn);
    assert_eq!(voltage_verdict(Some(11_999)).level, Level::Warn);
    assert_eq!(voltage_verdict(Some(12_000)).level, Level::Ok);
    let v = voltage_verdict(Some(12_600)).text;
    assert!(v.starts_with("12.6 V ") && v.contains("engine off"), "{v}");
    let v = voltage_verdict(Some(14_100)).text;
    assert!(
        v.starts_with("14.1 V ") && v.contains("charging range"),
        "{v}"
    );
    assert_eq!(voltage_verdict(Some(15_000)).level, Level::Ok);
    assert_eq!(voltage_verdict(Some(15_100)).level, Level::Warn);
    assert_eq!(
        (VOLT_NO_POWER, VOLT_MIN_WRITE, VOLT_RUNNING, VOLT_HIGH),
        (11_000, 12_000, 13_000, 15_000)
    );
}

fn probe(caps: Option<Capabilities>, mv: Option<u32>) -> ConnectProbe {
    ConnectProbe {
        driver: "vcar".into(),
        name: "Virtual car".into(),
        port: "demo".into(),
        opened: Ok(()),
        battery_mv: mv,
        caps,
    }
}

#[test]
fn connect_turns_the_probe_into_a_result_or_a_plain_failure() {
    let can = Capabilities {
        can: true,
        battery_voltage: true,
        ..Default::default()
    };
    let ok = connect_result(probe(Some(can), Some(14_100))).unwrap();
    assert_eq!(ok.voltage.level, Level::Ok);
    let unknown = connect_result(probe(None, None)).unwrap();
    assert_eq!(unknown.voltage.level, Level::Unknown);
    let doip = Capabilities {
        doip: true,
        ..Default::default()
    };
    assert!(connect_result(probe(Some(doip), None)).is_ok());
    let mut p = probe(Some(can), None);
    p.opened = Err("open COM9: Access is denied".into());
    assert_eq!(
        connect_result(p).unwrap_err(),
        "open COM9: Access is denied"
    );
    let kline = Capabilities {
        kline: true,
        ..Default::default()
    };
    let e = connect_result(probe(Some(kline), None)).unwrap_err();
    assert!(e.contains("cannot carry diagnostics"), "{e}");
    assert!(error_info(&e)
        .text
        .contains("cannot carry diagnostic requests"));
}

// ---- identify / scan / explain -----------------------------------------------

#[test]
fn identify_takes_mode_09_else_the_fallback_and_carries_the_decode() {
    let ins = Inspection {
        responded: true,
        info: Some(VehicleInfo {
            vin: Some(TEST_VIN.into()),
            ecu_name: Some("ECM-Demo".into()),
            calids: vec!["CAL-1".into()],
            cvns: vec!["0000BEEF".into()],
            ..Default::default()
        }),
        ..Default::default()
    };
    let dec = VinDecoded {
        manufacturer: Some("Demo".into()),
        model_year: Some("2013".into()),
        ..Default::default()
    };
    let a = identity(Some(&ins), Some("7E0/7E8".into()), None, Some(dec));
    assert_eq!(a.vin.as_deref(), Some(TEST_VIN));
    assert_eq!(a.vin_source, Some(VIN_MODE09));
    assert_eq!(a.target.as_deref(), Some("7E0/7E8"));
    assert_eq!(a.decoded.unwrap().model_year.as_deref(), Some("2013"));
    let ins = Inspection {
        responded: true,
        info: Some(VehicleInfo {
            errors: [("vin".to_string(), "NRC 0x31".to_string())].into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let b = identity(Some(&ins), None, Some("TESTVCAR0D0000002".into()), None);
    assert_eq!(b.vin.as_deref(), Some("TESTVCAR0D0000002"));
    assert!(b.vin_source.unwrap().contains("F190"));
    assert_eq!(b.errors.get("vin").map(String::as_str), Some("NRC 0x31"));
    // The VIN decoder runs on any 17-character VIN.
    assert!(decode_vin(TEST_VIN).is_some());
}

#[test]
fn the_scan_summary_merges_mil_readiness_and_codes_per_code() {
    let s = scan_summary(Some(&obd_scan()), Some(&module_scan()));
    assert_eq!(s.mil, Some(true));
    assert_eq!(s.mil_by, ["Engine (OBD 7E0)"]);
    assert_eq!(
        s.obd_modules,
        ["Engine (OBD 7E0)", "Transmission (OBD 7E1)"]
    );
    let r = s.readiness.as_ref().unwrap();
    assert_eq!(r.incomplete, ["Catalyst"]);
    assert_eq!(r.unsupported, ["Secondary air system"]);
    let codes: Vec<&str> = s.dtcs.iter().map(|d| d.code.as_str()).collect();
    assert_eq!(codes, ["C1234", "P0171", "P0420"]);
    let p0420 = s.dtcs.iter().find(|d| d.code == "P0420").unwrap();
    let kinds: Vec<SeenKind> = p0420.seen_as.iter().map(|x| x.kind).collect();
    assert_eq!(
        kinds,
        [SeenKind::Stored, SeenKind::Permanent, SeenKind::Module]
    );
    assert_eq!(s.modules.as_ref().unwrap().absent, ["Seat memory"]);
    assert!(
        s.notes[0].contains("permanent codes not read"),
        "{:?}",
        s.notes
    );
    assert_eq!(answered(&s), 4);
    let empty = scan_summary(Some(&ObdScan::default()), None);
    assert_eq!(empty.mil, None, "no readiness: MIL unknown");
    assert_eq!(answered(&empty), 0);
}

#[test]
fn explanations_say_only_what_the_data_says() {
    let s = scan_summary(Some(&obd_scan()), Some(&module_scan()));
    let lookups: BTreeMap<String, DtcLookup> =
        [("P0420".to_string(), lookup("P0420").unwrap())].into();
    let items = explain(Some(&s), &lookups, None);
    let mil = items.iter().find(|i| i.kind == ItemKind::Mil).unwrap();
    assert!(mil.title.contains("ON"));
    let p = items
        .iter()
        .find(|i| i.code.as_deref() == Some("P0420"))
        .unwrap();
    assert!(p.title.starts_with("P0420 — "), "{}", p.title);
    assert!(!p.title.contains("no description"));
    assert_eq!(
        p.text, CODE_TEXT,
        "no cause beyond the code's own description"
    );
    assert_eq!(
        p.system.as_deref(),
        Some("Powertrain (engine / transmission / emissions)")
    );
    assert!(
        p.seen.iter().any(|x| x.starts_with(
            "Engine: stored in the module's fault memory — status 0x28: confirmed and stored, \
             has failed since"
        )),
        "{:?}",
        p.seen
    );
    let pend = items
        .iter()
        .find(|i| i.code.as_deref() == Some("P0171"))
        .unwrap();
    assert_eq!(pend.level, Level::Warn, "pending only");
    let unk = items
        .iter()
        .find(|i| i.code.as_deref() == Some("C1234"))
        .unwrap();
    assert!(unk
        .title
        .contains("no description in OpenTune's code table"));
    assert!(unk.text.contains("manufacturer-specific or unknown"));
    assert!(unk.seen[0].contains("failing at the moment of the scan"));
    let rd = items
        .iter()
        .find(|i| i.kind == ItemKind::Readiness && i.level == Level::Warn)
        .unwrap();
    assert_eq!(rd.text, INCOMPLETE_TEXT);
    assert_eq!(rd.list, ["Catalyst"]);
    assert!(items
        .iter()
        .any(|i| i.kind == ItemKind::Modules && i.text.contains("Seat memory")));
    // No word hints at a cause the data does not give.
    let all: String = items
        .iter()
        .map(|i| format!("{} {} {}", i.title, i.text, i.seen.join(" ")))
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    for w in [
        "likely",
        "probably",
        "caused by",
        "replace",
        "faulty",
        "bad sensor",
    ] {
        assert!(!all.contains(w), "{w}");
    }
    // A clean car, a skipped scan.
    let mut clean = obd_scan();
    clean.ecus.remove(0);
    let c = explain(
        Some(&scan_summary(Some(&clean), None)),
        &BTreeMap::new(),
        Some("n"),
    );
    assert!(c
        .iter()
        .any(|i| i.title == "No trouble codes" && i.level == Level::Ok));
    assert!(c
        .iter()
        .find(|i| i.kind == ItemKind::Mil)
        .unwrap()
        .title
        .contains("off"));
    assert!(c
        .iter()
        .any(|i| i.kind == ItemKind::Modules && i.title == "Full module scan: not run"));
    assert_eq!(
        explain(None, &BTreeMap::new(), None)[0].kind,
        ItemKind::NoData
    );
}

#[test]
fn uds_status_bits_in_plain_words() {
    assert_eq!(uds_status_text(Some(0x08)), ["confirmed and stored"]);
    assert_eq!(uds_status_text(Some(0x89)).len(), 3);
    assert!(uds_status_text(None).is_empty());
}

// ---- live --------------------------------------------------------------------

fn sample(rpm: f64, cool: f64, speed: f64, mv: u32) -> Sample {
    Sample {
        values: [
            ("rpm".to_string(), Some(rpm)),
            ("coolant_temp".into(), Some(cool)),
            ("speed".into(), Some(speed)),
            ("stft_b1".into(), None),
            ("ltft_b1".into(), Some(2.3)),
            ("module_voltage".into(), Some(14.1)),
        ]
        .into(),
        errors: [("stft_b1".to_string(), "not supported by ECU".to_string())].into(),
        battery_mv: Some(mv),
    }
}

#[test]
fn live_pids_and_ratings_from_the_channel_table_only() {
    assert_eq!(live_pids(None), [0x05, 0x06, 0x07, 0x0C, 0x0D, 0x42]);
    let sup: BTreeSet<u8> = [0x05, 0x0C, 0x0D, 0x33].into();
    assert_eq!(live_pids(Some(&sup)), [0x05, 0x0C, 0x0D]);
    let s = live_summary(&[
        sample(820.0, 90.0, 0.0, 14_100),
        sample(800.0, 106.0, 0.0, 14_050),
    ]);
    let row = |k: &str| s.rows.iter().find(|r| r.key == k).unwrap().clone();
    assert_eq!(row("coolant_temp").level, Level::Warn);
    assert_eq!(row("coolant_temp").max, Some(106.0));
    assert!(row("coolant_temp").note.contains("warning mark of 105"));
    assert_eq!(row("stft_b1").last, None);
    assert!(row("stft_b1").note.contains("not supported by ECU"));
    assert_eq!(
        row("ltft_b1").level,
        Level::Unrated,
        "no threshold, no rating"
    );
    assert_eq!(row("ltft_b1").last, Some(2.3));
    assert!(row("ltft_b1").note.contains("no threshold"));
    assert_eq!(s.idle.state, IdleState::Idle);
    assert!(s.idle.text.contains("800–820 rpm"), "{}", s.idle.text);
    assert_eq!(s.battery.mv, Some(14_050));
    let crit = live_summary(&[sample(820.0, 116.0, 0.0, 14_100)]);
    assert_eq!(
        crit.rows
            .iter()
            .find(|r| r.key == "coolant_temp")
            .unwrap()
            .level,
        Level::Crit
    );
    assert_eq!(
        live_summary(&[sample(0.0, 20.0, 0.0, 12_500)]).idle.state,
        IdleState::Off
    );
    assert_eq!(
        live_summary(&[sample(1900.0, 80.0, 50.0, 14_000)])
            .idle
            .state,
        IdleState::Moving
    );
    let none = live_summary(&[]);
    assert_eq!(none.battery.level, Level::Unknown);
    assert_eq!(none.idle.state, IdleState::Unknown);
    assert_eq!(fmt_val(Some(-0.004), "V"), "0.00");
    assert_eq!(fmt_val(Some(2.26), "%"), "2.3");
    assert_eq!(fmt_val(None, "V"), "\u{2014}");
}

// ---- report ------------------------------------------------------------------

fn full_run() -> Run {
    let mut r = Run::new(0);
    let mut p = probe(
        Some(Capabilities {
            can: true,
            ..Default::default()
        }),
        Some(14_100),
    );
    p.name = "Virtual <car>".into();
    r.succeed(
        StepId::Connect,
        StepData::Connect(connect_result(p).unwrap()),
    );
    let ins = Inspection {
        responded: true,
        info: Some(VehicleInfo {
            vin: Some(TEST_VIN.into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    r.succeed(
        StepId::Identify,
        StepData::Identify(identity(Some(&ins), Some("7E0/7E8".into()), None, None)),
    );
    let summary = scan_summary(Some(&obd_scan()), Some(&module_scan()));
    let items = explain(Some(&summary), &BTreeMap::new(), None);
    r.succeed(
        StepId::Scan,
        StepData::Scan(ScanData {
            summary,
            problems: vec![],
            modules_note: None,
        }),
    );
    r.succeed(StepId::Explain, StepData::Explain(items));
    r.fail(StepId::Live, "adapter busy: Dashboard");
    r
}

fn meta(include_vin: bool) -> ReportMeta {
    ReportMeta {
        // 2026-01-02 12:00 UTC.
        now_ms: 1_767_355_200_000,
        app: "OpenTune".into(),
        include_vin,
    }
}

#[test]
fn utc_dates() {
    assert_eq!(utc(0), (1970, 1, 1, 0, 0));
    assert_eq!(utc(1_767_355_200_000), (2026, 1, 2, 12, 0));
    assert_eq!(utc(951_782_400_000), (2000, 2, 29, 0, 0));
}

#[test]
fn the_report_carries_every_step_escapes_data_and_says_it_was_read_only() {
    let mut r = full_run();
    let html = report_html(&r, &meta(false), true);
    assert!(html.starts_with("<!doctype html>"));
    assert!(
        html.contains("<title>OpenTune car check</title>"),
        "no VIN by default"
    );
    assert!(html.contains("Virtual &lt;car&gt;"), "data is escaped");
    assert!(!html.contains("Virtual <car>"));
    assert!(html.contains("P0420"));
    assert!(html.contains("Live check<span class=\"st failed\">failed"));
    assert!(html.contains("Another panel is using the adapter"));
    assert!(!html.to_lowercase().contains("<script"), "a static page");
    assert!(html.contains(&DISCLAIMER.replace('\'', "&#39;")));
    assert!(html.contains("2026-01-02 12:00 UTC"));
    let md = report_markdown(&r, &meta(false));
    assert!(md.starts_with("# OpenTune car check"));
    assert!(md.contains("## Scan — done"));
    assert!(md.contains(&format!("| P0420 | {CAT} |")), "{md}");
    assert!(md.contains("| --- | --- | --- |"));
    assert!(md.contains("## Live check — failed"));
    assert!(md.contains("Detail: `adapter busy: Dashboard`"));
    assert!(md.contains("Read-only check"));
    assert_eq!(
        report_file_name(&r, "html", &meta(false)),
        "opentune-car-check-2026-01-02.html"
    );
    r.skip(StepId::Live);
    assert!(report_markdown(&r, &meta(false))
        .contains("## Live check — skipped\n\nThis step was skipped."));
}

#[test]
fn the_vin_is_left_out_unless_ticked() {
    let r = full_run();
    for text in [
        report_markdown(&r, &meta(false)),
        report_html(&r, &meta(false), true),
        report_html(&r, &meta(false), false),
    ] {
        assert!(!text.contains(TEST_VIN), "{text}");
        assert!(!text.contains("000001"));
        assert!(text.contains("left out of this report"));
    }
    assert!(!report_file_name(&r, "md", &meta(false)).contains("000001"));
    // The module scan's identities never reach the report either.
    assert!(!report_markdown(&r, &meta(false)).contains("TESTVCAR"));
    let md = report_markdown(&r, &meta(true));
    assert!(md.contains(&format!("· VIN {TEST_VIN}")));
    assert!(md.contains(&format!("{TEST_VIN} (OBD mode 09)")));
    let html = report_html(&r, &meta(true), true);
    assert!(html.contains(&format!("<title>OpenTune car check — {TEST_VIN}</title>")));
    assert_eq!(
        report_file_name(&r, "md", &meta(true)),
        "opentune-car-check-2026-01-02-000001.md"
    );
}

#[test]
fn markdown_escapes_its_specials() {
    let mut r = Run::new(0);
    r.fail(StepId::Connect, "bad `tick` and *star* | pipe");
    let md = report_markdown(&r, &meta(false));
    assert!(
        md.contains("Detail: `bad 'tick' and *star* | pipe`"),
        "{md}"
    );
    r.skip(StepId::Connect);
    let mut p = probe(None, None);
    p.name = "a_b|c".into();
    r.succeed(
        StepId::Connect,
        StepData::Connect(connect_result(p).unwrap()),
    );
    let md = report_markdown(&r, &meta(false));
    assert!(md.contains("- **Adapter:** a\\_b\\|c"), "{md}");
}
