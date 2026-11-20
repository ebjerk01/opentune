//! Inspection tests on SYNTHETIC OBD ECUs ([`SimVehicle::demo`]: an engine
//! on 7E0 with codes and a freeze frame, a gearbox on 7E1) behind the real
//! hub adapter.

use super::model::{self, Scope};
use super::InspectionView;
use crate::host::{DiagTarget, NativeView};
use crate::views::coding::simkit::{Bus, Fixture};
use opentune_core::obd::sim::{SimEcu, SimVehicle};
use opentune_core::obd::Sections;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn ecu(e: SimEcu) -> crate::views::coding::simkit::Script {
    let e = Arc::new(Mutex::new(e));
    Box::new(move |req| {
        use opentune_core::RawDiag;
        e.lock().unwrap().request(req)
    })
}

fn vehicle() -> Bus {
    let mut v = SimVehicle::demo(3);
    let mut engine = v.ecus.remove(&0x7E0).unwrap();
    engine.vin = None;
    let tcm = v.ecus.remove(&0x7E1).unwrap();
    Bus::default()
        .script(0x7E0, 0x7E8, ecu(engine))
        .script(0x7E1, 0x7E9, ecu(tcm))
}

#[test]
fn scopes_name_their_modules() {
    let m = model::modules(Scope::Target, DiagTarget::can(0x7E1, 0x7E9));
    assert_eq!(m.len(), 1);
    assert!(
        m[0].label.contains("7E1") || m[0].label.contains("0x7E1"),
        "{}",
        m[0].label
    );
    let all = model::modules(Scope::AllObd, DiagTarget::ENGINE);
    assert_eq!(all.len(), 8);
    assert_eq!(all[0].target, DiagTarget::can(0x7E0, 0x7E8));
    assert_eq!(all[7].target, DiagTarget::can(0x7E7, 0x7EF));
    let other = model::modules(Scope::Target, DiagTarget::can(0x710, 0x77A));
    assert!(other[0].label.starts_with("Module"));
}

#[test]
fn all_obd_ecus_keeps_the_answering_ones_and_only_reads() {
    let bus = vehicle();
    let a = bus.access("sim");
    let mods = model::modules(Scope::AllObd, DiagTarget::ENGINE);
    let rs = model::read(&a, mods, false, Sections::ALL, None).unwrap();
    assert_eq!(rs.len(), 2, "engine + gearbox");
    assert_eq!(rs[0].module.target.tx, 0x7E0);
    assert_eq!(rs[1].module.target.tx, 0x7E1);
    let s = model::summary(&rs);
    assert_eq!(s.modules, 2);
    assert!(s.mil_on);
    assert_eq!((s.stored, s.pending, s.permanent), (2, 1, 1));
    assert!(s.incomplete > 0);
    let engine = &rs[0].ins;
    assert!(engine.freeze_frame.as_ref().is_some_and(|f| f.stored));
    assert_eq!(
        engine.dtcs.as_ref().unwrap().stored[0].code,
        "P0401",
        "the demo's first stored code"
    );
    // Every address was tried; nothing but read services went out, and no
    // clear (mode 04) ever.
    let wire = bus.wire();
    for tx in 0x7E0..=0x7E7u32 {
        assert!(wire.iter().any(|(t, _)| *t == tx), "{tx:X} not tried");
    }
    for (_, req) in &wire {
        assert!(crate::ops::refuse_ecu_changing(req).is_ok(), "{req:02X?}");
        assert_ne!(req[0], 0x04);
    }
    let text = model::report_text(&rs);
    assert!(text.contains("P0401") && text.contains("MIL ON"), "{text}");
    assert!(text.contains("CALID OT-DEMO-TCU-0001"), "{text}");
}

#[test]
fn the_target_scope_keeps_a_silent_module_and_sections_limit_the_requests() {
    let bus = vehicle();
    let a = bus.access("sim");
    let silent = model::modules(Scope::Target, DiagTarget::can(0x7E5, 0x7ED));
    let rs = model::read(&a, silent, true, Sections::ALL, None).unwrap();
    assert_eq!(rs.len(), 1);
    assert!(!rs[0].ins.responded);
    assert!(model::report_text(&rs).contains("no answer"));
    bus.clear_wire();
    let only_dtcs = Sections {
        readiness: false,
        dtcs: true,
        freeze: false,
        info: false,
    };
    let rs = model::read(
        &a,
        model::modules(Scope::Target, DiagTarget::ENGINE),
        true,
        only_dtcs,
        None,
    )
    .unwrap();
    assert!(rs[0].ins.readiness.is_none() && rs[0].ins.dtcs.is_some());
    for (_, req) in bus.wire() {
        assert!(matches!(req[0], 0x03 | 0x07 | 0x0A), "{req:02X?}");
    }
}

#[test]
fn the_view_reads_in_a_job_and_counts_codes() {
    let bus = vehicle();
    let mut fx = Fixture::new("inspect-view");
    let mut v = InspectionView::default();
    assert!(
        fx.host(DiagTarget::ENGINE, |h| v.start(h)).is_err(),
        "no adapter"
    );
    fx.adapter = Some(bus.access("sim"));
    v.sections = Sections {
        readiness: false,
        dtcs: false,
        freeze: false,
        info: false,
    };
    assert!(
        fx.host(DiagTarget::ENGINE, |h| v.start(h)).is_err(),
        "no section"
    );
    assert!(bus.wire().is_empty());
    v.sections = Sections::ALL;
    v.scope = Scope::AllObd;
    fx.host(DiagTarget::ENGINE, |h| v.start(h)).unwrap();
    let t0 = Instant::now();
    while v.running() {
        assert!(t0.elapsed() < Duration::from_secs(20), "stuck");
        std::thread::sleep(Duration::from_millis(5));
    }
    v.poll();
    assert_eq!(v.results.as_ref().map(Vec::len), Some(2));
    assert_eq!(v.count().as_deref(), Some("4"));
}
