//! Aftermarket-ECU panel tests: the repo's SYNTHETIC demo INIs
//! (`samples/ts`, describing no real ECU) and the in-memory simulated ECU
//! (`tsproto::SimEcu`). No hardware.

use super::*;
use crate::host::DiagTarget;
use crate::views::coding::simkit::{Bus, Fixture};
use opentune_tsproto::{Port, SimEcu, SimEvent, SimOptions};
use std::sync::Mutex;

const DEMO: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../samples/ts/opentune-demo.ini"
);
const DEMO_MS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../samples/ts/opentune-demo-ms.ini"
);

fn view() -> TsView {
    let mut v = TsView::default();
    v.load_ini(Path::new(DEMO));
    assert_eq!(
        v.offline_msg.as_ref().unwrap().0,
        Tone::Ok,
        "{:?}",
        v.offline_msg
    );
    v
}

fn display(v: &TsView, name: &str) -> String {
    v.model.get(name).unwrap().display.unwrap()
}

/// A simulated ECU the test can inspect after the panel used it.
struct Shared(Arc<Mutex<SimEcu>>);

impl Port for Shared {
    fn send(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.0.lock().unwrap().send(data)
    }
    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> std::io::Result<usize> {
        self.0.lock().unwrap().recv(buf, timeout)
    }
    fn discard_input(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().discard_input()
    }
}

type Opened = Result<(online::Conn, online::ConnectInfo), String>;

fn shared_conn(v: &TsView, opts: SimOptions, force: bool) -> (Arc<Mutex<SimEcu>>, Opened) {
    let ini = v.model.ini().unwrap();
    let mut ecu = SimEcu::new(&ini, opts);
    if let Some(t) = &v.model.working {
        ecu.load_tune(t);
    }
    let ecu = Arc::new(Mutex::new(ecu));
    let r = online::attach(
        ini,
        Box::new(Shared(ecu.clone())),
        "shared sim".into(),
        true,
        None,
        force,
    );
    (ecu, r)
}

fn idle(v: &mut TsView, fx: &mut Fixture) {
    let t0 = Instant::now();
    while v.op.is_some() {
        assert!(t0.elapsed() < Duration::from_secs(10), "stuck");
        fx.host(DiagTarget::ENGINE, |h| v.tick(h));
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn no_writes(ecu: &Arc<Mutex<SimEcu>>) {
    let ev = ecu.lock().unwrap().events().to_vec();
    assert!(
        !ev.iter()
            .any(|e| matches!(e, SimEvent::Write { .. } | SimEvent::Burn { .. })),
        "{ev:?}"
    );
}

#[test]
fn offline_edits_validate_against_the_ini() {
    let mut v = view();
    v.apply_value("reqFuel", "12.3").unwrap();
    assert_eq!(display(&v, "reqFuel"), "12.3 ms");
    // Outside the INI's limits (0..25.5): refused, value unchanged.
    let e = v.apply_value("reqFuel", "30").unwrap_err();
    assert!(e.contains("refused") && e.contains("25.5"), "{e}");
    assert_eq!(display(&v, "reqFuel"), "12.3 ms");
    assert!(v.apply_value("reqFuel", "abc").is_err());
    // Bits by index; INVALID and out-of-range options refused.
    v.apply_value("injLayout", "3").unwrap();
    assert!(v
        .apply_value("injLayout", "2")
        .unwrap_err()
        .contains("INVALID"));
    assert!(v.apply_value("injLayout", "4").is_err());
    // Strings fit their length; arrays are edited per cell.
    assert!(v.apply_value("tuneName", &"x".repeat(17)).is_err());
    v.apply_value("tuneName", "street").unwrap();
    assert!(v
        .apply_value("wueRates", "1")
        .unwrap_err()
        .contains("array"));
    v.apply_cell("wueRates", 0, "180").unwrap();
    assert!(v.apply_cell("wueRates", 99, "180").is_err());
    // A cell outside the limits (100..255) is refused, not clamped.
    let e = v.apply_cell("wueRates", 1, "300").unwrap_err();
    assert!(e.contains("refused"), "{e}");
    match v.model.get("wueRates").unwrap().value {
        Some(TsValue::Array(a)) => assert_eq!(&a[..2], &[180.0, 0.0]),
        other => panic!("{other:?}"),
    }
    assert!(v.apply_value("nope", "1").is_err());
    let rows = v.model.values().unwrap();
    assert!(rows.iter().all(|r| !matches!(r.class, Class::Array { .. })));
    assert!(rows.iter().any(|r| r.name == "crankRPM"));
}

#[test]
fn tables_curves_and_a_msq_round_trip_with_diff() {
    let mut fx = Fixture::new("n7-ts-msq");
    let mut v = view();
    v.apply_cell("veTable", 9, "77").unwrap(); // row 1, col 1
    for (i, x) in [500, 1000, 2000, 3000, 4000, 5000, 6000, 7000]
        .iter()
        .enumerate()
    {
        v.apply_cell("rpmBins", i, &x.to_string()).unwrap();
    }
    let t = v.model.table("veTbl").unwrap();
    assert_eq!((t.rows, t.cols), (6, 8));
    assert_eq!(t.cells[1][1], 77.0);
    let x = t.x.unwrap();
    assert_eq!(
        (x.name.as_str(), x.channel.as_deref()),
        ("rpmBins", Some("rpm"))
    );
    assert_eq!(x.values[2], 2000.0);
    let c = v.model.curve("wueCurve").unwrap();
    assert_eq!(c.x.unwrap().name, "wueBins");
    assert_eq!(c.ys[0].name, "wueRates");

    let path = fx.dir.join("roundtrip.msq");
    v.save_msq(path.clone());
    assert!(
        v.offline_msg.as_ref().unwrap().1.contains("saved"),
        "{:?}",
        v.offline_msg
    );
    v.diff_msq(&path);
    assert!(v.offline_msg.as_ref().unwrap().1.starts_with("0 constant"));
    v.apply_value("crankRPM", "400").unwrap();
    v.diff_msq(&path);
    assert!(v.offline_msg.as_ref().unwrap().1.starts_with("1 constant"));
    assert!(v.report[0].starts_with("crankRPM"));
    // Loading it back restores the saved value.
    v.load_msq(&path);
    assert_eq!(v.model.table("veTbl").unwrap().cells[1][1], 77.0);
    assert_ne!(display(&v, "crankRPM"), "400 rpm");
    // Saving over the file asks first and archives the old one.
    v.save_msq(path.clone());
    assert!(v.saver.busy());
    fx.host(DiagTarget::ENGINE, |h| v.tick(h));
    fx.confirms.answer("ts-save-msq", true);
    fx.host(DiagTarget::ENGINE, |h| v.tick(h));
    assert!(!v.saver.busy());
    assert!(pl::vault_dir(&path).is_dir());
}

#[test]
fn the_simulated_ecu_connects_reads_backs_up_plans_and_polls_without_writing() {
    let mut fx = Fixture::new("n7-ts-online");
    let mut v = view();
    // Offline only until connected.
    fx.host(DiagTarget::ENGINE, |h| v.request_read(h));
    assert!(v.online_msg.as_ref().unwrap().1.contains("not connected"));
    v.apply_value("reqFuel", "8.0").unwrap();
    let (ecu, opened) = shared_conn(&v, SimOptions::default(), false);
    let (conn, info) = opened.unwrap();
    assert!(info.matches);
    v.conn = Some(conn);
    // The plan needs an ECU read first.
    v.request_plan();
    assert!(v.plan.as_ref().unwrap().is_err());
    fx.host(DiagTarget::ENGINE, |h| v.request_read(h));
    idle(&mut v, &mut fx);
    assert_eq!(display(&v, "reqFuel"), "8.0 ms");
    assert_eq!(v.model.working_src, "ECU");
    v.request_plan();
    assert!(v.plan.as_ref().unwrap().as_ref().unwrap().is_empty());
    // Edits make a plan (offline), naming the constants.
    v.apply_value("reqFuel", "9.5").unwrap();
    v.apply_cell("veTable", 0, "61").unwrap();
    v.request_plan();
    let p = v.plan.clone().unwrap().unwrap();
    let names: Vec<&str> = p.constants.iter().map(|(n, _)| n.as_str()).collect();
    assert!(
        names.contains(&"reqFuel") && names.contains(&"veTable"),
        "{names:?}"
    );
    assert_eq!(p.pages.len(), 2);
    // Backup: refused without a folder, then the whole tune into the vault.
    fx.host(DiagTarget::ENGINE, |h| v.request_backup(h));
    assert!(v.op.is_none() && v.online_msg.as_ref().unwrap().1.contains("folder"));
    v.records = pl::Records::in_dir(&fx.dir);
    fx.host(DiagTarget::ENGINE, |h| v.request_backup(h));
    idle(&mut v, &mut fx);
    assert_eq!(
        v.online_msg.as_ref().unwrap().0,
        Tone::Ok,
        "{:?}",
        v.online_msg
    );
    let pages: Vec<u8> = ecu.lock().unwrap().ram().concat();
    assert_eq!(std::fs::read(&v.last_backup[0]).unwrap(), pages);
    let xml = std::fs::read_to_string(&v.last_backup[1]).unwrap();
    assert!(xml.contains("reqFuel"));
    assert_eq!(display(&v, "reqFuel"), "9.5 ms", "the edits are kept");
    // Live channels.
    fx.host(DiagTarget::ENGINE, |h| {
        v.channels = "rpm, coolant".into();
        v.request_poll(h)
    });
    idle(&mut v, &mut fx);
    assert_eq!(v.samples.len(), 2, "{:?}", v.online_msg);
    assert_eq!(v.samples[0].name, "rpm");
    assert!(v.samples.iter().all(|s| s.value.is_some()));
    // Nothing was ever written or burned.
    no_writes(&ecu);
    v.disconnect();
    assert!(v.conn.is_none());
}

#[test]
fn connect_through_the_panel_checks_the_signature_and_reload_drops_it() {
    let mut fx = Fixture::new("n7-ts-connect");
    let mut v = view();
    v.port = "sim".into();
    fx.host(DiagTarget::ENGINE, |h| v.request_connect(h));
    idle(&mut v, &mut fx);
    assert!(v.conn.is_some(), "{:?}", v.online_msg);
    assert!(v.conn_info.as_ref().unwrap().matches);
    assert!(v.conn_info.as_ref().unwrap().envelope);
    // Reloading the INI drops the connection (its check was against the old one).
    v.load_ini(Path::new(DEMO));
    assert!(v.conn.is_none());
    // A wrong framing name or an empty port is refused early.
    v.framing = "bogus".into();
    fx.host(DiagTarget::ENGINE, |h| v.request_connect(h));
    assert!(v.op.is_none());
    v.framing = "auto".into();
    v.port = " ".into();
    fx.host(DiagTarget::ENGINE, |h| v.request_connect(h));
    idle(&mut v, &mut fx);
    assert!(v.conn.is_none());
    // The MegaSquirt-like demo uses plain framing.
    v.load_ini(Path::new(DEMO_MS));
    v.port = "sim".into();
    fx.host(DiagTarget::ENGINE, |h| v.request_connect(h));
    idle(&mut v, &mut fx);
    assert!(
        !v.conn_info.as_ref().unwrap().envelope,
        "{:?}",
        v.online_msg
    );
}

#[test]
fn a_signature_mismatch_is_refused_unless_forced() {
    let v = view();
    let other = || SimOptions {
        signature: Some("another firmware 1".into()),
        ..SimOptions::default()
    };
    let (_ecu, r) = shared_conn(&v, other(), false);
    assert!(r.err().unwrap().contains("does not match"));
    let (_ecu, r) = shared_conn(&v, other(), true);
    let (_conn, i) = r.unwrap();
    assert!(!i.matches);
    assert_eq!(i.ini_signature.as_deref(), Some("opentune demo 1"));
}

#[test]
fn the_adapter_links_port_is_not_opened_as_an_ecu_port() {
    let bus = Bus::default();
    let a = bus.adapter("sim");
    a.poll_link().unwrap();
    let mut fx = Fixture::new("n7-ts-port");
    fx.adapter = Some(a as Arc<dyn crate::host::AdapterAccess>);
    let mut v = view();
    v.port = "sim0".into(); // the simkit link's port
    fx.host(DiagTarget::ENGINE, |h| v.request_connect(h));
    assert!(v.op.is_none());
    assert!(
        v.online_msg.as_ref().unwrap().1.contains("adapter"),
        "{:?}",
        v.online_msg
    );
}
