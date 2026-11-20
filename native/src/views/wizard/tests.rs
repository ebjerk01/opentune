//! The wizard end to end on SYNTHETIC modules behind the real hub adapter
//! (simkit): an OBD engine ([`SimEcu::demo`]) and gearbox on 7E0 / 7E1, a
//! UDS ABS module for the full module scan. Every test proves the check is
//! read-only: only read services go out, never a clear (`04` / `14`), a
//! session change, a security access or a write, and no confirmation is ever
//! asked.

use super::model::{self, IdleState, Level, LiveSource, Status, StepId};
use super::ops::{self, NoCtl};
use super::WizardView;
use crate::host::{AdapterAccess, DiagTarget, FeedEvent, FeedValue, LiveFeed, NativeView};
use crate::views::coding::simkit::{Bus, Fixture, Script};
use opentune_core::obd::sim::{SimEcu, SimVehicle};
use opentune_udssim::server::{DtcRecord, UdsServer};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TEST_VIN: &str = "TESTVCAR0D0000001";
const ENGINE: DiagTarget = DiagTarget::ENGINE;

/// The services a read-only check may send.
const ALLOWED: [u8; 8] = [0x01, 0x03, 0x07, 0x09, 0x0A, 0x19, 0x22, 0x3E];

/// An OBD ECU; `f190`: it also answers UDS `22 F190` with this VIN.
fn ecu(e: SimEcu, f190: Option<&str>) -> Script {
    let e = Arc::new(Mutex::new(e));
    let f190 = f190.map(|v| v.as_bytes().to_vec());
    Box::new(move |req| {
        use opentune_core::RawDiag;
        if let (Some(v), [0x22, 0xF1, 0x90]) = (&f190, req) {
            let mut r = vec![0x62, 0xF1, 0x90];
            r.extend_from_slice(v);
            return Ok(r);
        }
        e.lock().unwrap().request(req)
    })
}

fn engine(vin: Option<&str>) -> SimEcu {
    let mut e = SimVehicle::demo(3).ecus.remove(&0x7E0).unwrap();
    e.vin = vin.map(str::to_string);
    e
}

/// Engine (with `vin` over mode 09) + gearbox + ABS (one code).
fn vehicle(vin: Option<&str>) -> Bus {
    let mut v = SimVehicle::demo(3);
    let tcm = v.ecus.remove(&0x7E1).unwrap();
    let mut abs = UdsServer::new();
    abs.add_dtc(DtcRecord::new([0xC1, 0x00, 0x00], 0x2F));
    Bus::default()
        .script(0x7E0, 0x7E8, ecu(engine(vin), None))
        .script(0x7E1, 0x7E9, ecu(tcm, None))
        .uds(0x760, 0x768, abs)
}

/// A module list: the ABS and an absent airbag module.
fn module_list(dir: &std::path::Path) -> PathBuf {
    let p = dir.join("modules.json");
    let json = r#"{"vehicle":"Synthetic car","modules":[
        {"name":"ABS","addr":3,"req_id":1888,"resp_id":1896,"protocol":"uds"},
        {"name":"Airbag","addr":21,"req_id":1813,"resp_id":1919,"protocol":"uds"}]}"#;
    std::fs::write(&p, json).unwrap();
    p
}

fn assert_read_only(bus: &Bus) {
    for (tx, req) in bus.wire() {
        assert!(
            crate::ops::refuse_ecu_changing(&req).is_ok(),
            "{tx:X}: {req:02X?}"
        );
        assert!(
            ALLOWED.contains(&req[0]),
            "{tx:X}: {req:02X?} is not a read"
        );
    }
    assert!(bus.with_sid(0x04).is_empty(), "no OBD clear");
    assert!(bus.with_sid(0x14).is_empty(), "no UDS clear");
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "ot-wizard-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn connect_opens_the_cable_without_sending_anything() {
    let bus = vehicle(None);
    let a = bus.access("vcar");
    let c = ops::connect(&a, ENGINE).unwrap();
    assert_eq!(c.driver, "vcar");
    assert!(c.name.to_lowercase().contains("virtual car"), "{}", c.name);
    assert_eq!(c.voltage.level, Level::Unknown, "no battery reading yet");
    assert_eq!(bus.lock().opens, vec![0x7E0], "opened on the target");
    assert!(bus.wire().is_empty(), "nothing sent to a control unit");
    assert_eq!(ops::target_text(ENGINE), "7E0/7E8");
}

#[test]
fn identify_reads_mode_09_or_falls_back_to_f190() {
    let bus = vehicle(Some(TEST_VIN));
    let a = bus.access("vcar");
    let id = ops::identify(&a, ENGINE).unwrap();
    assert_eq!(id.vin.as_deref(), Some(TEST_VIN));
    assert_eq!(id.vin_source, Some(model::VIN_MODE09));
    assert!(!id.calids.is_empty() && id.ecu_name.is_some());
    assert!(id.decoded.is_some());
    assert_eq!(id.target.as_deref(), Some("7E0/7E8"));
    assert!(bus.with_sid(0x22).is_empty(), "no fall-back needed");
    assert_read_only(&bus);

    // No VIN over mode 09: the UDS 22 F190 fall-back.
    let bus = Bus::default().script(0x7E0, 0x7E8, ecu(engine(None), Some(TEST_VIN)));
    let id = ops::identify(&bus.access("vcar"), ENGINE).unwrap();
    assert_eq!(id.vin.as_deref(), Some(TEST_VIN));
    assert_eq!(id.vin_source, Some(model::VIN_FALLBACK));
    assert_eq!(bus.with_sid(0x22).len(), 1);
    assert_read_only(&bus);

    // Nothing answers: a plain "did not answer".
    let bus = Bus::default();
    let e = ops::identify(&bus.access("vcar"), ENGINE).unwrap_err();
    assert!(model::error_info(&e).text.contains("did not answer"), "{e}");
}

#[test]
fn the_scan_asks_every_obd_address_and_the_module_list_read_only() {
    let dir = tmp("scan");
    let bus = vehicle(None);
    let a = bus.access("vcar");
    let d = ops::scan(&a, Some(module_list(&dir)), &NoCtl).unwrap();
    let s = &d.summary;
    assert_eq!(s.mil, Some(true));
    assert_eq!(
        s.obd_modules,
        ["Engine (OBD 7E0)", "Transmission (OBD 7E1)"]
    );
    assert_eq!(s.silent.len(), 6);
    assert!(s.dtcs.iter().any(|x| x.code == "P0401"), "{:?}", s.dtcs);
    let u = s.dtcs.iter().find(|x| x.code == "U0100").expect("ABS code");
    assert_eq!(u.seen_as[0].source, "ABS");
    assert_eq!(u.seen_as[0].status, Some(0x2F));
    let m = s.modules.as_ref().unwrap();
    assert_eq!(
        (m.present.clone(), m.absent.clone()),
        (vec!["ABS".to_string()], vec!["Airbag".to_string()])
    );
    assert!(s
        .readiness
        .as_ref()
        .is_some_and(|r| !r.incomplete.is_empty()));
    assert!(d.problems.is_empty(), "{:?}", d.problems);
    for tx in 0x7E0..=0x7E7u32 {
        assert!(!bus.sent_to(tx).is_empty(), "{tx:X} not asked");
    }
    assert_read_only(&bus);

    // The explanations of it: the DTC table's words, the status bits.
    let mut run = model::Run::new(0);
    run.succeed(StepId::Scan, model::StepData::Scan(d));
    let items = model::explain_run(&run);
    let p = items
        .iter()
        .find(|i| i.code.as_deref() == Some("P0401"))
        .unwrap();
    assert!(!p.title.contains("no description"), "{}", p.title);
    let u = items
        .iter()
        .find(|i| i.code.as_deref() == Some("U0100"))
        .unwrap();
    assert!(u.seen[0].contains("failing at the moment of the scan"));

    // No list: only OBD, said so. A bad list: a plain problem, OBD kept.
    let d = ops::scan(&a, None, &NoCtl).unwrap();
    assert_eq!(d.modules_note.as_deref(), Some(ops::NO_LIST_NOTE));
    let d = ops::scan(&a, Some(dir.join("missing.json")), &NoCtl).unwrap();
    assert_eq!(d.problems.len(), 1);
    assert_eq!(d.problems[0].what, "Module scan");
    assert_eq!(d.summary.obd_modules.len(), 2);
    // Nothing answers at all: the step fails.
    let e = ops::scan(&Bus::default().access("vcar"), None, &NoCtl).unwrap_err();
    assert!(e.contains("no module answered"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_live_check_takes_six_mode_01_readings() {
    let bus = vehicle(None);
    let a = bus.access("vcar");
    let d = ops::live(&a, ENGINE, None, Duration::from_millis(1), &NoCtl).unwrap();
    assert_eq!(d.source, LiveSource::Reads);
    assert_eq!(d.summary.samples, model::LIVE_SAMPLES);
    assert!(!d.stopped);
    let cool = d
        .summary
        .rows
        .iter()
        .find(|r| r.key == "coolant_temp")
        .unwrap();
    assert_eq!(cool.n, model::LIVE_SAMPLES);
    assert_ne!(cool.level, Level::Unrated, "coolant has warn / crit marks");
    assert_eq!(
        d.summary.idle.state,
        IdleState::Moving,
        "the demo car drives"
    );
    assert!(d.pids.contains(&0x05) && d.pids.contains(&0x0C));
    for (_, req) in bus.wire() {
        assert_eq!(req[0], 0x01, "{req:02X?}");
    }
    assert_read_only(&bus);
}

#[test]
fn the_live_check_uses_the_live_feed_while_another_panel_holds_the_adapter() {
    let bus = vehicle(None);
    let a: Arc<dyn AdapterAccess> = bus.access("vcar");
    let hold = a
        .start("Live log", ENGINE, |_d, stop| {
            while !stop.is_stopped() {
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok(())
        })
        .unwrap();
    // Busy and no feed: the plain busy message.
    let e = ops::live(&a, ENGINE, None, Duration::from_millis(1), &NoCtl).unwrap_err();
    assert_eq!(model::error_info(&e).text, model::BUSY_TEXT);
    // Busy with a publisher on the feed: its frames are the readings.
    let feed = LiveFeed::default();
    let done = Arc::new(AtomicBool::new(false));
    let (f2, d2) = (feed.clone(), done.clone());
    let publisher = std::thread::spawn(move || {
        while !d2.load(Ordering::SeqCst) {
            f2.publish(FeedEvent::Frame {
                source: "dashboard".into(),
                t_ms: 1,
                values: vec![
                    FeedValue {
                        key: "rpm".into(),
                        name: "Engine speed".into(),
                        unit: "rpm".into(),
                        value: Some(810.0),
                    },
                    FeedValue {
                        key: "speed".into(),
                        name: "Vehicle speed".into(),
                        unit: "km/h".into(),
                        value: Some(0.0),
                    },
                    FeedValue {
                        key: "egt_b1#0".into(),
                        name: "x".into(),
                        unit: "°C".into(),
                        value: Some(300.0),
                    },
                ],
            });
            std::thread::sleep(Duration::from_millis(3));
        }
    });
    let d = ops::live(&a, ENGINE, Some(&feed), Duration::from_millis(20), &NoCtl).unwrap();
    done.store(true, Ordering::SeqCst);
    publisher.join().unwrap();
    assert_eq!(d.source, LiveSource::Feed("dashboard".into()));
    assert_eq!(d.summary.samples, model::LIVE_SAMPLES);
    assert_eq!(d.summary.idle.state, IdleState::Idle);
    assert_eq!(d.pids, vec![0x0C, 0x0D]);
    assert!(bus.wire().is_empty(), "nothing sent: the adapter was busy");
    hold.stop();
}

fn wait(v: &mut WizardView) {
    let t0 = Instant::now();
    while v.running() {
        assert!(t0.elapsed() < Duration::from_secs(20), "stuck");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Run the current step `id` through the view, then Next.
fn step(v: &mut WizardView, fx: &mut Fixture, id: StepId) {
    assert_eq!(v.run.cur(), id);
    fx.host(ENGINE, |h| v.run_step(h, id));
    wait(v);
    assert_eq!(
        v.run.status(id),
        Status::Done,
        "{id:?}: {:?}",
        v.run.step(id).error
    );
    fx.host(ENGINE, |h| v.next(h));
}

#[test]
fn the_view_walks_every_step_read_only_and_saves_a_report_without_the_vin() {
    let bus = vehicle(Some(TEST_VIN));
    let mut fx = Fixture::new("wizard-view");
    let mut v = WizardView {
        gap: Duration::from_millis(1),
        ..Default::default()
    };
    // No adapter: the step fails in plain words, nothing is sent.
    fx.host(ENGINE, |h| v.run_step(h, StepId::Connect));
    assert_eq!(v.run.status(StepId::Connect), Status::Failed);
    let e = v.run.step(StepId::Connect).error.clone().unwrap();
    assert!(e.text.contains("could not be opened"), "{}", e.text);
    fx.adapter = Some(bus.access("vcar"));

    step(&mut v, &mut fx, StepId::Connect);
    step(&mut v, &mut fx, StepId::Identify);
    v.modules = Some(module_list(&fx.dir));
    step(&mut v, &mut fx, StepId::Scan);
    // Explain needs no car: it filled in on arrival.
    assert_eq!(v.run.cur(), StepId::Explain);
    assert_eq!(v.run.status(StepId::Explain), Status::Done);
    assert!(v.run.explain().unwrap().iter().any(|i| i.code.is_some()));
    assert!(v.count().is_some_and(|n| n != "0"));
    fx.host(ENGINE, |h| v.next(h));
    step(&mut v, &mut fx, StepId::Live);
    assert_eq!(v.run.cur(), StepId::Report);
    assert_eq!(v.run.status(StepId::Report), Status::Done);

    // Read-only from start to report: no confirmation, no view switch.
    assert_read_only(&bus);
    assert!(fx.actions.is_empty());

    // The report leaves the VIN out unless ticked.
    let md = v.report_text(true);
    assert!(md.contains("## Live check — done"), "{md}");
    assert!(!md.contains(TEST_VIN), "{md}");
    let path = fx.dir.join("check.md");
    v.save_to(path.clone(), md.clone().into_bytes());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), md);
    assert!(matches!(v.msg, Some((crate::theme::Tone::Ok, _))));
    // An existing file is not replaced without a confirmation.
    v.save_to(path.clone(), b"other".to_vec());
    assert!(v.save.is_some());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), md);
    v.include_vin = true;
    assert!(v.report_text(false).contains(TEST_VIN));

    // A new check starts from Connect.
    v.restart();
    assert_eq!(v.run.cur(), StepId::Connect);
    assert_eq!(v.run.status(StepId::Scan), Status::Pending);
}

#[test]
fn skipping_steps_reaches_a_report_of_what_was_done() {
    let mut fx = Fixture::new("wizard-skip");
    let mut v = WizardView::default();
    for _ in 0..3 {
        fx.host(ENGINE, |h| v.skip(h));
    }
    // The scan was skipped: Explain says so.
    assert_eq!(v.run.cur(), StepId::Explain);
    assert_eq!(v.run.explain().unwrap()[0].kind, model::ItemKind::NoData);
    fx.host(ENGINE, |h| v.next(h));
    fx.host(ENGINE, |h| v.skip(h));
    assert_eq!(v.run.cur(), StepId::Report);
    let md = v.report_text(true);
    assert!(md.contains("## Scan — skipped"), "{md}");
    assert!(md.contains("## Explain — done"), "{md}");
}
