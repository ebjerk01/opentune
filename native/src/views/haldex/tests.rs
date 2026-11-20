//! Haldex panel tests: a SYNTHETIC OpenHaldex (the crate's `SimOpenHaldex`)
//! and Gen4 (`SimGen4`) behind the real hub adapter (`coding::simkit`), and
//! a generated CAN log. No hardware, no real vehicle data.

use super::ops::{self, HoldShared, OH};
use super::*;
use crate::host::{AdapterAccess, DiagTarget};
use crate::views::coding::simkit::{Bus as SimBus, Fixture};
use opentune_haldex::sim::{SimGen4, SimOpenHaldex};
use std::sync::Mutex;

const ECU: DiagTarget = DiagTarget::ENGINE;

/// A bus with a simulated OpenHaldex on 0x70F/0x779 (shared, inspectable).
fn oh_bus() -> (SimBus, Arc<Mutex<SimOpenHaldex>>) {
    let unit = Arc::new(Mutex::new(SimOpenHaldex::default()));
    let u = unit.clone();
    let bus = SimBus::default().script(
        OH.tx,
        OH.rx,
        Box::new(move |r: &[u8]| {
            use opentune_core::RawDiag;
            u.lock().unwrap().request(r)
        }),
    );
    (bus, unit)
}

fn mode(unit: &Arc<Mutex<SimOpenHaldex>>) -> Mode {
    unit.lock().unwrap().mode
}

fn writes(bus: &SimBus) -> Vec<Vec<u8>> {
    bus.sent_to(OH.tx)
        .into_iter()
        .filter(|r| r.first() == Some(&0x2E))
        .collect()
}

fn run_until(v: &mut HaldexView, fx: &mut Fixture, until: impl Fn(&HaldexView) -> bool) {
    let t0 = Instant::now();
    while !until(v) {
        assert!(t0.elapsed() < Duration::from_secs(15), "stuck");
        fx.host(ECU, |h| v.tick(h));
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn holding(v: &HaldexView) -> bool {
    matches!(v.step, Step::Holding { .. })
}

/// Request a hold of `target` and accept the confirmation; returns once the
/// mode is set on the unit.
fn start_hold(
    v: &mut HaldexView,
    fx: &mut Fixture,
    target: Mode,
    unit: &Arc<Mutex<SimOpenHaldex>>,
) {
    fx.host(ECU, |h| v.request(h, Intent::Hold, target));
    run_until(v, fx, |v| matches!(v.step, Step::Confirm { .. }));
    fx.host(ECU, |h| v.tick(h));
    fx.confirms
        .answer(&pl::confirm_key("openhaldex-hold", OH), true);
    run_until(v, fx, holding);
    let t0 = Instant::now();
    while mode(unit) != target {
        assert!(t0.elapsed() < Duration::from_secs(5), "mode not set");
        fx.host(ECU, |h| v.tick(h));
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn status_is_read_only_on_the_openhaldex_address() {
    let (bus, unit) = oh_bus();
    unit.lock().unwrap().commanded_pct = 25;
    let mut fx = Fixture::new("n7-oh-status");
    fx.adapter = Some(bus.access("sim"));
    let mut v = HaldexView::default();
    fx.host(ECU, |h| v.request_status(h));
    run_until(&mut v, &mut fx, |v| v.status.is_some());
    let s = v.status.clone().unwrap().unwrap();
    assert_eq!(s.lock.commanded_pct, 25);
    assert_eq!(s.lock.mode, Some(Mode::Stock));
    assert_eq!(s.slip.unwrap().rl, None, "-128 = no data");
    assert!(bus
        .wire()
        .iter()
        .all(|(tx, r)| *tx == OH.tx && r[0] == 0x22));
}

#[test]
fn a_hold_sets_the_mode_and_stop_hands_the_unit_back() {
    let (bus, unit) = oh_bus();
    let mut fx = Fixture::new("n7-oh-hold");
    fx.adapter = Some(bus.access("sim"));
    let mut v = HaldexView::default();
    // Nothing but the pre-read before the confirmation.
    fx.host(ECU, |h| v.request(h, Intent::Hold, Mode::Split6040));
    run_until(&mut v, &mut fx, |v| matches!(v.step, Step::Confirm { .. }));
    assert!(writes(&bus).is_empty());
    let Step::Confirm { req, prev, .. } = &v.step else {
        unreachable!()
    };
    assert_eq!(*prev, Mode::Stock);
    assert!(req.body.contains("2E FD A2 03") && req.body.contains("2E FD A2 00"));
    bus.clear_wire();
    fx.host(ECU, |h| v.tick(h));
    fx.confirms
        .answer(&pl::confirm_key("openhaldex-hold", OH), true);
    run_until(&mut v, &mut fx, holding);
    let t0 = Instant::now();
    while mode(&unit) != Mode::Split6040 {
        assert!(t0.elapsed() < Duration::from_secs(5));
        fx.host(ECU, |h| v.tick(h));
    }
    std::thread::sleep(Duration::from_millis(600));
    fx.host(ECU, |h| v.tick(h));
    assert!(v.hold.live().is_some());
    v.request_stop();
    run_until(&mut v, &mut fx, |v| !v.busy());
    assert_eq!(v.msg.as_ref().unwrap().0, Tone::Ok, "{:?}", v.msg);
    assert!(v.msg.as_ref().unwrap().1.contains("stopped"));
    assert_eq!(mode(&unit), Mode::Stock, "handed back");
    let sent = bus.sent_to(OH.tx);
    assert_eq!(sent[0], vec![0x22, 0xFD, 0xA0], "re-read first");
    assert_eq!(
        writes(&bus),
        vec![vec![0x2E, 0xFD, 0xA2, 0x03], vec![0x2E, 0xFD, 0xA2, 0x00]]
    );
    assert!(v.unrestored.is_none());
}

#[test]
fn leaving_the_panel_or_the_time_running_out_hands_back() {
    // The panel is left: no frame for the grace time.
    let (bus, unit) = oh_bus();
    let mut fx = Fixture::new("n7-oh-left");
    fx.adapter = Some(bus.access("sim"));
    let mut v = HaldexView::default();
    v.grace = Duration::from_millis(300);
    start_hold(&mut v, &mut fx, Mode::Fwd, &unit);
    std::thread::sleep(Duration::from_millis(900)); // no tick = no frame
    let t0 = Instant::now();
    while mode(&unit) != Mode::Stock {
        assert!(t0.elapsed() < Duration::from_secs(5), "not handed back");
        std::thread::sleep(Duration::from_millis(20));
    }
    run_until(&mut v, &mut fx, |v| !v.busy());
    assert!(
        v.msg.as_ref().unwrap().1.contains("panel was left"),
        "{:?}",
        v.msg
    );
    // The hold time runs out.
    let (bus, unit) = oh_bus();
    fx.adapter = Some(bus.access("sim"));
    let mut v = HaldexView::default();
    v.hold_s = 1;
    start_hold(&mut v, &mut fx, Mode::Split5050, &unit);
    run_until(&mut v, &mut fx, |v| !v.busy());
    assert!(v.msg.as_ref().unwrap().1.contains("time up"), "{:?}", v.msg);
    assert_eq!(mode(&unit), Mode::Stock);
}

#[test]
fn closing_the_panel_during_a_hold_hands_back_before_it_returns() {
    let (bus, unit) = oh_bus();
    let mut fx = Fixture::new("n7-oh-drop");
    fx.adapter = Some(bus.access("sim"));
    let mut v = HaldexView::default();
    start_hold(&mut v, &mut fx, Mode::Expert, &unit);
    drop(v);
    assert_eq!(mode(&unit), Mode::Stock, "Drop waited for the hand-back");
    assert_eq!(writes(&bus).last().unwrap(), &vec![0x2E, 0xFD, 0xA2, 0x00]);
}

#[test]
fn a_lost_link_ends_the_hold_and_a_failed_hand_back_is_offered_again() {
    let (bus, unit) = oh_bus();
    let mut fx = Fixture::new("n7-oh-lost");
    fx.adapter = Some(bus.access("sim"));
    let mut v = HaldexView::default();
    v.records = pl::Records::in_dir(&fx.dir);
    start_hold(&mut v, &mut fx, Mode::Split7525, &unit);
    bus.dead.store(true, std::sync::atomic::Ordering::SeqCst);
    run_until(&mut v, &mut fx, |v| !v.busy());
    let (tone, m) = v.msg.clone().unwrap();
    assert_eq!(tone, Tone::Crit);
    assert!(m.contains("link lost") && m.contains("FAILED"), "{m}");
    assert_eq!(v.unrestored, Some(Mode::Stock));
    assert_eq!(
        mode(&unit),
        Mode::Split7525,
        "the unit could not be reached"
    );
    // The link is back: "Set Stock again" is the confirmed, backed-up path.
    bus.dead.store(false, std::sync::atomic::Ordering::SeqCst);
    fx.host(ECU, |h| v.request(h, Intent::Keep, Mode::Stock));
    run_until(&mut v, &mut fx, |v| matches!(v.step, Step::Confirm { .. }));
    fx.host(ECU, |h| v.tick(h));
    fx.confirms
        .answer(&pl::confirm_key("openhaldex-set", OH), true);
    run_until(&mut v, &mut fx, |v| !v.busy());
    assert_eq!(mode(&unit), Mode::Stock, "{:?}", v.msg);
    assert!(v.unrestored.is_none());
}

#[test]
fn set_and_keep_backs_up_the_record_first_and_refuses_a_changed_unit() {
    let (bus, unit) = oh_bus();
    let mut fx = Fixture::new("n7-oh-keep");
    fx.adapter = Some(bus.access("sim"));
    let mut v = HaldexView::default();
    // No backup folder: refused, nothing sent.
    fx.host(ECU, |h| v.request(h, Intent::Keep, Mode::Fwd));
    assert!(!v.busy());
    assert!(bus.wire().is_empty());
    v.records = pl::Records::in_dir(&fx.dir);
    // Declined: only the pre-read went out.
    fx.host(ECU, |h| v.request(h, Intent::Keep, Mode::Fwd));
    run_until(&mut v, &mut fx, |v| matches!(v.step, Step::Confirm { .. }));
    let Step::Confirm { req, .. } = &v.step else {
        unreachable!()
    };
    assert_eq!(req.typed.as_deref(), Some("SET"));
    fx.host(ECU, |h| v.tick(h));
    fx.confirms
        .answer(&pl::confirm_key("openhaldex-set", OH), false);
    fx.host(ECU, |h| v.tick(h));
    assert!(!v.busy());
    assert!(writes(&bus).is_empty());
    // Accepted: the backup holds the pre-read record, then the write.
    fx.host(ECU, |h| v.request(h, Intent::Keep, Mode::Fwd));
    run_until(&mut v, &mut fx, |v| matches!(v.step, Step::Confirm { .. }));
    fx.host(ECU, |h| v.tick(h));
    fx.confirms
        .answer(&pl::confirm_key("openhaldex-set", OH), true);
    run_until(&mut v, &mut fx, |v| !v.busy());
    assert_eq!(v.msg.as_ref().unwrap().0, Tone::Ok, "{:?}", v.msg);
    assert_eq!(mode(&unit), Mode::Fwd);
    let b = std::fs::read(v.last_backup.as_ref().unwrap()).unwrap();
    assert_eq!(b, vec![0, 0x80, Mode::Stock.index()], "the FDA0 record");
    assert_eq!(writes(&bus), vec![vec![0x2E, 0xFD, 0xA2, 0x01]]);
    // The unit changes between the pre-read and the write: nothing written.
    bus.clear_wire();
    fx.host(ECU, |h| v.request(h, Intent::Keep, Mode::Split5050));
    run_until(&mut v, &mut fx, |v| matches!(v.step, Step::Confirm { .. }));
    unit.lock().unwrap().mode = Mode::Expert;
    fx.host(ECU, |h| v.tick(h));
    fx.confirms
        .answer(&pl::confirm_key("openhaldex-set", OH), true);
    run_until(&mut v, &mut fx, |v| !v.busy());
    assert!(v.msg.as_ref().unwrap().1.contains("changed"), "{:?}", v.msg);
    assert!(writes(&bus).is_empty());
    assert_eq!(mode(&unit), Mode::Expert);
}

#[test]
fn the_ops_refuse_a_wrong_ack_or_receipt_and_an_unwritten_hold_restores_nothing() {
    let (bus, unit) = oh_bus();
    let a = bus.access("sim");
    let mut fx = Fixture::new("n7-oh-guards");
    // An Ack for the engine is not one for the OpenHaldex.
    let ack = fx.ecu_ack(ECU);
    let e = ops::hold_op(
        &a,
        ack,
        Mode::Stock,
        Mode::Fwd,
        Duration::from_secs(1),
        HoldShared::new(GRACE),
    )
    .unwrap_err();
    assert!(e.contains("does not name"), "{e}");
    let ack = fx.ecu_ack(OH);
    let other = fx.backup(&[9, 9, 9]);
    let e = ops::set_keep_op(&a, ack, &other, vec![0, 0x80, 0], Mode::Fwd).unwrap_err();
    assert!(e.contains("receipt"), "{e}");
    assert!(bus.wire().is_empty());
    // The hold refused before its write: the hand-back sends nothing.
    unit.lock().unwrap().mode = Mode::Fwd;
    let ack = fx.ecu_ack(OH);
    let act = ops::hold_op(
        &a,
        ack,
        Mode::Stock,
        Mode::Split5050,
        Duration::from_secs(1),
        HoldShared::new(GRACE),
    )
    .unwrap();
    assert!(act.result.unwrap_err().contains("changed"));
    assert!(act.restored.is_ok());
    assert!(writes(&bus).is_empty(), "{:02X?}", bus.wire());
    assert_eq!(mode(&unit), Mode::Fwd);
    // A record that names no mode.
    assert!(ops::record_mode(&[0, 0, 9]).is_err());
    assert!(ops::record_mode(&[0, 0]).is_err());
}

#[test]
fn gen4_reads_through_tp20_read_only_and_refuses_an_elm() {
    // Real path: a raw-CAN cable, the hub's TP2.0 target, a synthetic unit.
    let g = Arc::new(Mutex::new(SimGen4::default()));
    let g2 = g.clone();
    let bus = SimBus::default().script(
        ops::GEN4.tx,
        ops::GEN4.rx,
        Box::new(move |r: &[u8]| {
            use opentune_core::protocol::DiagTransport;
            g2.lock().unwrap().request(r, 0).map_err(|e| e.to_string())
        }),
    );
    let a: Arc<dyn AdapterAccess> = bus.access("slcan");
    let opts = ops::gen4_options("", true).unwrap();
    let r = ops::gen4_read_op(&a, "slcan", opts.clone()).unwrap();
    assert!(r.responded(), "{r:?}");
    assert!(r.identity.unwrap().part_number.starts_with("0AY"));
    assert!(g
        .lock()
        .unwrap()
        .services()
        .iter()
        .all(|s| [0x10, 0x1A, 0x18, 0x21].contains(s)));
    // An ELM327 cannot carry TP2.0: refused before anything is sent.
    bus.clear_wire();
    let e = ops::gen4_read_op(&a, "elm327obd", opts.clone()).unwrap_err();
    assert!(e.contains("TP2.0") && e.contains("CAN log"), "{e}");
    assert!(bus.wire().is_empty());
    // The simulated read needs no adapter.
    let s = ops::gen4_sim(&opts);
    assert!(s.responded() && !s.groups.is_empty());
    // Group parsing.
    assert_eq!(ops::gen4_options("1, 3", false).unwrap().groups, vec![1, 3]);
    assert!(ops::gen4_options("300", true).is_err());
}

#[test]
fn gen4_sim_runs_from_the_panel() {
    let mut fx = Fixture::new("n7-gen4-panel");
    let mut v = HaldexView::default();
    v.gen4_sim = true;
    fx.host(ECU, |h| v.request_gen4(h));
    run_until(&mut v, &mut fx, |v| v.gen4.is_some());
    assert!(v.gen4.as_ref().unwrap().as_ref().unwrap().responded());
}

const LOG: &str = "\
(1.0) can0 2A0#7F0000
(1.1) can0 2A0#FE0000
(1.2) can0 2C0#1880000054C0FE73
(1.3) can0 2C0#01BF0080560000F3
(1.4) can0 4A0#88130000A00F0000
(1.5) can0 6B0#0000C0280000020A
(1.6) can0 7E8#0102
";

#[test]
fn a_log_decodes_offline_and_exports_a_live_log_csv() {
    let mut fx = Fixture::new("n7-haldex-log");
    let p = fx.dir.join("drive.log");
    std::fs::write(&p, LOG).unwrap();
    let mut v = HaldexView::default();
    fx.host(ECU, |h| v.request_decode(h, p.clone()));
    run_until(&mut v, &mut fx, |v| v.log.is_some());
    let d = v.log.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(d.summary.frames, 7);
    assert_eq!(d.rows_total, 6);
    assert_eq!(d.rows[0].1, 0x2A0);
    let req = d.series.iter().find(|s| s.key == "lock_request").unwrap();
    assert_eq!(req.v, vec![0.0, 100.0]);
    assert!(d.csv.starts_with("ts,param_id,name,raw,value,units,note\n"));
    let out = fx.dir.join("series.csv");
    v.export_csv(out.clone());
    assert!(v.log_msg.as_ref().unwrap().1.contains("saved"));
    let csv = std::fs::read_to_string(&out).unwrap();
    assert!(csv.contains("0,lock_request,Lock request,,0,%,"));
    // A missing file is an error, not a panic.
    fx.host(ECU, |h| v.request_decode(h, fx_missing()));
    run_until(&mut v, &mut fx, |v| v.log_job.is_none());
    assert!(v.log.as_ref().unwrap().is_err());
}

fn fx_missing() -> PathBuf {
    std::env::temp_dir().join("ot-n7-no-such-log.log")
}
