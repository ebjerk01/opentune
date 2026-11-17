//! End to end: the real OpenTune engines (auto-scan, coding, adaptation,
//! service functions, OBD, CAN capture) driven over the `vcar` driver exactly
//! the way the apps drive a real adapter — one driver open per module address,
//! the core ISO-TP stack in between.
use opentune_autoscan::{ModuleAddr, Protocol, VehicleModules};
use opentune_core::obd::{self, ObdSession};
use opentune_core::{ConnectConfig, CoreDiag, Interface, MonitorOpts, RawDiag};
use opentune_driver_vcar::{
    engine_state, monitor, shared_vehicle, test_security_key, Profile, SharedVehicle, VcarDriver,
    Vehicle,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const ENGINE: (u32, u32) = (0x7E0, 0x7E8);
const GEARBOX: (u32, u32) = (0x7E1, 0x7E9);
const ABS: (u32, u32) = (0x713, 0x77D);
const AIRBAG: (u32, u32) = (0x715, 0x77F);
const FUNCTIONAL: (u32, u32) = (0x7DF, 0x7E8);

fn pack_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../packs/vcar-demo")
}

fn pack_file(rel: &str) -> String {
    std::fs::read_to_string(pack_dir().join(rel)).unwrap()
}

/// A private demo car (not shared with other tests).
fn private_car() -> SharedVehicle {
    Arc::new(Mutex::new(Vehicle::from_profile(&Profile::demo()).unwrap()))
}

fn open(car: &SharedVehicle, (tx, rx): (u32, u32)) -> VcarDriver {
    let mut d = VcarDriver::with_vehicle(car.clone());
    let cfg = ConnectConfig {
        tx_id: tx,
        rx_id: rx,
        ..ConnectConfig::default()
    };
    d.open(&cfg).unwrap();
    d
}

/// Run `f` over a fresh driver bound to `addr`, like the desktop hub does.
fn with_diag<R>(car: &SharedVehicle, addr: (u32, u32), f: impl FnOnce(&mut CoreDiag) -> R) -> R {
    let mut d = open(car, addr);
    let t = d.diag().expect("vcar exposes a diagnostic transport");
    let mut diag = CoreDiag::new(t, 100);
    f(&mut diag)
}

fn demo_modules() -> VehicleModules {
    VehicleModules::from_json(&pack_file("modules/vcar-demo.json")).unwrap()
}

fn scan_all(car: &SharedVehicle, mods: &VehicleModules) -> Vec<opentune_autoscan::ModuleResult> {
    mods.modules
        .iter()
        .map(|m| {
            with_diag(car, (m.req_id, m.resp_id), |d| {
                opentune_autoscan::scan_module(d, m)
            })
        })
        .collect()
}

#[test]
fn autoscan_finds_all_six_modules_with_identity_and_dtcs() {
    let car = private_car();
    let profile = Profile::demo();
    let mods = demo_modules();
    let r = scan_all(&car, &mods);

    for pm in &profile.modules {
        let res = r
            .iter()
            .zip(&mods.modules)
            .find(|(_, m)| m.req_id == pm.req_id && m.protocol == Protocol::Uds)
            .map(|(r, _)| r)
            .unwrap_or_else(|| panic!("{} missing from the pack module list", pm.name));
        assert!(res.present, "{} must be present", pm.name);
        assert!(res.error.is_none(), "{}: {:?}", pm.name, res.error);
        let id = res.identity.as_ref().unwrap();
        let part = id.dids.iter().find(|d| d.did == 0xF187).unwrap();
        let want = pm.dids.iter().find(|d| d.did == 0xF187).unwrap();
        assert_eq!(part.text.as_deref(), want.ascii.as_deref(), "{}", pm.name);
        // Only the engine serves the VIN.
        let want_vin = (pm.req_id == 0x7E0).then(|| profile.vin.clone());
        assert_eq!(id.vin, want_vin, "{}", pm.name);
        // Exactly the profile's DTCs, with their status bytes.
        let got: Vec<(String, Option<u8>)> = res
            .dtcs
            .iter()
            .map(|d| (d.code.clone(), d.status))
            .collect();
        let want: Vec<(String, Option<u8>)> = pm
            .dtcs
            .iter()
            .map(|d| (d.code.clone(), Some(d.status)))
            .collect();
        assert_eq!(got, want, "{}", pm.name);
    }

    // OBD view of the engine over the functional id: mode 03 = confirmed codes.
    let obd = r.iter().find(|m| m.name == "Engine (OBD-II)").unwrap();
    assert!(obd.present);
    let codes: Vec<&str> = obd.dtcs.iter().map(|d| d.code.as_str()).collect();
    assert_eq!(codes, ["P0101", "P0420"]);

    // The module that is not fitted answers nothing -> absent, no error.
    let absent = r.iter().find(|m| m.name.starts_with("Parking")).unwrap();
    assert!(!absent.present);
    assert!(absent.error.is_none());
    assert!(absent.identity.is_none());
}

#[test]
fn unknown_address_times_out() {
    let car = private_car();
    let mut d = open(&car, (0x7A0, 0x7A8));
    let err = d.diag().unwrap().request(&[0x3E, 0x00], 50).unwrap_err();
    assert!(matches!(err, opentune_core::Error::Timeout(_)), "{err}");
    assert!(d.read_ecu_id().is_err());
    // A real module identifies itself.
    let mut g = open(&car, GEARBOX);
    let id = g.read_ecu_id().unwrap();
    assert_eq!(id.ecu_name.as_deref(), Some("Gearbox"));
    assert_eq!(id.hw_number.as_deref(), Some("VCT-HW-02"));
    assert_eq!(id.supplier.as_deref(), Some("VCAR-SUP-02"));
    assert_eq!(id.vin, None);
}

#[test]
fn clearing_one_module_clears_only_that_module() {
    // Through the shared table, like separate app operations: every open is a
    // new driver instance, the car persists.
    let spec = "demo@test-clear-one";
    let opener = |addr: (u32, u32)| {
        let mut d = VcarDriver::new();
        let mut cfg = ConnectConfig {
            tx_id: addr.0,
            rx_id: addr.1,
            ..ConnectConfig::default()
        };
        cfg.options.insert("profile".into(), spec.into());
        d.open(&cfg).unwrap();
        d
    };
    let abs = ModuleAddr {
        name: "ABS / ESC".into(),
        addr: 3,
        req_id: ABS.0,
        resp_id: ABS.1,
        protocol: Protocol::Uds,
    };
    let mut d = opener(ABS);
    let r = opentune_autoscan::clear_module(&mut CoreDiag::new(d.diag().unwrap(), 100), &abs);
    assert!(r.cleared, "{:?}", r.error);
    drop(d);

    let car = shared_vehicle(spec).unwrap();
    let mods = demo_modules();
    let after = scan_all(&car, &mods);
    for (res, m) in after.iter().zip(&mods.modules) {
        if !res.present {
            continue;
        }
        if m.req_id == ABS.0 {
            assert!(res.dtcs.is_empty(), "ABS cleared");
        } else {
            assert!(!res.dtcs.is_empty(), "{} keeps its codes", m.name);
        }
    }
    // OBD mode 04 on the engine clears the engine's store (UDS view too).
    let mut d = opener(FUNCTIONAL);
    let mut diag = CoreDiag::new(d.diag().unwrap(), 100);
    assert_eq!(diag.request(&[0x04]).unwrap(), vec![0x44]);
    assert!(car
        .lock()
        .unwrap()
        .module(0x7E0)
        .unwrap()
        .server()
        .dtcs()
        .is_empty());
}

#[test]
fn gearbox_coding_read_write_read_back() {
    use opentune_coding::{get, read_coding, set, write_coding, CodingDef, WriteOptions};
    let car = private_car();
    let def = CodingDef::from_json(&pack_file("coding/gearbox.json")).unwrap();
    def.validate().unwrap();

    let before = with_diag(&car, GEARBOX, |d| read_coding(d, &def).unwrap());
    assert_eq!(get(&def, &before, "Drive layout").unwrap(), 1);
    assert_eq!(get(&def, &before, "Tyre circumference").unwrap(), 200);
    assert_eq!(get(&def, &before, "Tow mode").unwrap(), 0);

    let desired = set(&def, &before, "Tow mode", 1).unwrap();
    let report = with_diag(&car, GEARBOX, |d| {
        assert_eq!(d.request(&[0x10, 0x03]).unwrap()[0], 0x50);
        write_coding(d, &def, &before, &desired, &WriteOptions::default()).unwrap()
    });
    assert!(report.is_verified(), "{:?}", report.outcome);
    assert_eq!(report.written, vec![0x0600]);

    // A new binding (new driver) reads the new value back.
    let after = with_diag(&car, GEARBOX, |d| read_coding(d, &def).unwrap());
    assert_eq!(get(&def, &after, "Tow mode").unwrap(), 1);
    // Only the gearbox changed: the engine's 0x0600 is untouched.
    let v = car.lock().unwrap();
    assert_eq!(
        v.module(0x7E1).unwrap().server().did(0x0600),
        Some(&[0x09, 0xC8, 0, 0][..])
    );
    assert_eq!(
        v.module(0x7E0).unwrap().server().did(0x0600),
        Some(&[3, 3, 0, 0, 0, 0][..])
    );
}

#[test]
fn adaptation_routine_needs_the_right_target() {
    use opentune_adaptation::{
        read_value, run_actuator_test, run_routine, write_value, AdaptationDef, RoutineOp,
        WriteOptions,
    };
    let car = private_car();
    let def = AdaptationDef::from_json(&pack_file("adaptation/gearbox.json")).unwrap();
    def.validate().unwrap();

    // The gearbox's routine does not exist on the engine: refused there.
    let wrong = with_diag(&car, ENGINE, |d| {
        run_routine(d, &def, "Reset clutch adaptation", RoutineOp::Start, &[])
    });
    assert!(wrong.is_err(), "routine must not run on the engine");
    // ... and on an address nobody answers, it fails at the transport.
    let absent = with_diag(&car, (0x7A0, 0x7A8), |d| {
        run_routine(d, &def, "Reset clutch adaptation", RoutineOp::Start, &[])
    });
    assert!(absent.is_err());
    // On its own module it runs and reports the result.
    let ok = with_diag(&car, GEARBOX, |d| {
        run_routine(d, &def, "Reset clutch adaptation", RoutineOp::Start, &[]).unwrap()
    });
    assert_eq!(ok.value, Some(0.0));

    // Value channel: read, write in range, verify; out of range refused.
    let r = with_diag(&car, GEARBOX, |d| {
        read_value(d, &def, "Shift firmness").unwrap()
    });
    assert_eq!(r.raw, 5);
    let w = with_diag(&car, GEARBOX, |d| {
        write_value(d, &def, "Shift firmness", 7, &WriteOptions::default()).unwrap()
    });
    assert!(w.is_verified());
    assert!(with_diag(&car, GEARBOX, |d| write_value(
        d,
        &def,
        "Shift firmness",
        11,
        &WriteOptions::default()
    ))
    .is_err());

    // Actuator: driven, then control handed back to the ECU.
    let a = with_diag(&car, GEARBOX, |d| {
        run_actuator_test(
            d,
            &def,
            "Shift-lock solenoid",
            &[],
            &WriteOptions::default(),
        )
        .unwrap()
    });
    assert!(a.is_completed(), "{:?}", a.outcome);
    assert_eq!(a.response, vec![0x01]);
    let v = car.lock().unwrap();
    let tcu = v.module(0x7E1).unwrap().server();
    assert!(!tcu.io_overridden(0x0510));
    assert_eq!(tcu.io_state(0x0510), Some(&[0x00][..]));
}

#[test]
fn airbag_writes_need_the_test_security_key() {
    use opentune_servicefn::{run_script, RunOptions, ServiceScript};
    let car = private_car();
    let mask = Profile::demo().security.xor_mask;

    // Locked: the write is refused with securityAccessDenied.
    let resp = with_diag(&car, AIRBAG, |d| {
        d.request(&[0x10, 0x03]).unwrap();
        d.request(&[0x2E, 0x06, 0x00, 0x07, 0x00]).unwrap()
    });
    assert_eq!(resp, vec![0x7F, 0x2E, 0x33]);

    // Unlocked with the documented TEST-ONLY key: accepted.
    let resp = with_diag(&car, AIRBAG, |d| {
        d.request(&[0x10, 0x03]).unwrap();
        let seed = d.request(&[0x27, 0x01]).unwrap();
        assert_eq!(&seed[..2], &[0x67, 0x01]);
        let mut key = vec![0x27, 0x02];
        key.extend(test_security_key(&seed[2..], mask));
        assert_eq!(d.request(&key).unwrap(), vec![0x67, 0x02]);
        d.request(&[0x2E, 0x06, 0x00, 0x07, 0x00]).unwrap()
    });
    assert_eq!(resp, vec![0x6E, 0x06, 0x00]);

    // The secured service script runs with a key provider for the test algorithm.
    let script = ServiceScript::from_json(&pack_file("services/airbag-lamp-test.json")).unwrap();
    let key = move |seed: &[u8], _level: u8| -> Result<Vec<u8>, String> {
        Ok(test_security_key(seed, mask))
    };
    let opts = RunOptions {
        dry_run: false,
        skip_waits: true,
    };
    let report = with_diag(&car, AIRBAG, |d| {
        run_script(d, &script, Some(&key), &opts).unwrap()
    });
    assert!(report.succeeded(), "{:?}", report.outcome);
    assert!(report.security_unlocked);
    // Without a key provider it stops before touching the lamp.
    let refused = with_diag(&car, AIRBAG, |d| run_script(d, &script, None, &opts));
    assert!(refused.map(|r| !r.succeeded()).unwrap_or(true));
}

#[test]
fn unsecured_service_scripts_run() {
    use opentune_servicefn::{run_script, RunOptions, ServiceScript};
    let car = private_car();
    let opts = RunOptions {
        dry_run: false,
        skip_waits: true,
    };
    for (file, addr) in [
        ("services/cluster-service-reset.json", (0x714, 0x77E)),
        ("services/engine-fan-test.json", ENGINE),
    ] {
        let script = ServiceScript::from_json(&pack_file(file)).unwrap();
        let r = with_diag(&car, addr, |d| run_script(d, &script, None, &opts).unwrap());
        assert!(r.succeeded(), "{file}: {:?}", r.outcome);
    }
    assert!(!car
        .lock()
        .unwrap()
        .module(0x7E0)
        .unwrap()
        .server()
        .io_overridden(0x0500));
}

#[test]
fn dashboard_obd_read_follows_the_engine() {
    let car = private_car();
    for addr in [FUNCTIONAL, ENGINE] {
        let (pids, ro, vin) = with_diag(&car, addr, |d| {
            let mut s = ObdSession::new();
            let pids = s.discover(d).unwrap();
            let ro = s.read(d, &[0x0C, 0x0D, 0x05, 0x42, 0x0B]);
            (pids, ro, obd::read_vin(d))
        });
        assert!(pids.contains(&0x0C) && pids.contains(&0x42) && pids.contains(&0x87));
        assert_eq!(vin.as_deref(), Some("TESTVCAR0D0000001"));
        let num = |k: &str| ro.values[k].as_ref().unwrap().first().unwrap();
        let rpm = num("rpm");
        assert!((800.0..=3100.0).contains(&rpm), "rpm {rpm}");
        assert!((num("module_voltage") - 14.1).abs() < 0.01);
        assert_eq!(num("coolant_temp"), 90.0);
        assert!(
            ro.values["boost_kpa"].is_some(),
            "derived boost from MAP+baro"
        );
    }
    let mut d = open(&car, ENGINE);
    assert_eq!(d.battery_millivolts().unwrap(), Some(14_100));

    // Engine off: rpm 0 and the battery at rest.
    car.lock().unwrap().set_engine_running(false);
    let ro = with_diag(&car, ENGINE, |d| ObdSession::new().read(d, &[0x0C, 0x42]));
    assert_eq!(ro.values["rpm"].as_ref().unwrap().first(), Some(0.0));
    assert_eq!(d.battery_millivolts().unwrap(), Some(12_600));

    // A non-OBD module refuses OBD, an absent one does not answer.
    let r = with_diag(&car, ABS, |d| d.request(&[0x01, 0x00]).unwrap());
    assert_eq!(r, vec![0x7F, 0x01, 0x11]);
    assert!(with_diag(&car, (0x7A0, 0x7A8), |d| d.request(&[0x01, 0x00])).is_err());
}

#[test]
fn engine_option_and_capabilities() {
    let mut d = VcarDriver::new();
    let caps = d.capabilities();
    assert!(caps.can && caps.battery_voltage && !caps.read_memory && !caps.write_memory);
    assert_eq!(d.info().id, "vcar");
    let mut cfg = ConnectConfig::default();
    cfg.options
        .insert("port".into(), "demo@test-engine-off".into());
    cfg.options.insert("engine".into(), "off".into());
    d.open(&cfg).unwrap();
    assert_eq!(d.battery_millivolts().unwrap(), Some(12_600));
    assert_eq!(d.target(), Some(ENGINE));
    cfg.options.insert("engine".into(), "sideways".into());
    assert!(VcarDriver::new().open(&cfg).is_err());
    let mut bad = ConnectConfig::default();
    bad.options
        .insert("profile".into(), "/nonexistent/vcar.json".into());
    assert!(VcarDriver::new().open(&bad).is_err());
}

#[test]
fn candump_capture_produces_engine_consistent_frames() {
    use opentune_candump::{run_capture, CaptureParams, CaptureStats, Format, FrameWriter};
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    let car = private_car();
    let mut drv = VcarDriver::with_vehicle(car.clone());
    let mon = drv.can_monitor().expect("vcar exposes a monitor");
    let mut opts = MonitorOpts::default();
    opts.options.insert("vcar_realtime".into(), "0".into());
    opts.options.insert("vcar_frames".into(), "400".into());
    mon.start(&opts).unwrap();
    let mut w = FrameWriter::new(
        Vec::new(),
        Format::Candump,
        &mon.channel(),
        mon.wall_base_us(),
    );
    let stats = Mutex::new(CaptureStats::new());
    let stop = AtomicBool::new(false);
    let params = CaptureParams {
        opts,
        duration: Some(Duration::from_millis(300)),
    };
    run_capture(mon, &mut w, &stats, &stop, &params, &mut |_| {}).unwrap();
    let text = String::from_utf8(w.into_inner().unwrap()).unwrap();
    let stats = stats.into_inner().unwrap();
    assert_eq!(stats.frames(), 400);
    assert!(text.lines().next().unwrap().contains(") vcar0 "));
    let snap = stats.snapshot(1_000_000, 256);
    let ids: Vec<u32> = snap.ids.iter().map(|r| r.id).collect();
    for id in [
        monitor::ID_ENGINE,
        monitor::ID_SPEED,
        monitor::ID_CLUSTER,
        monitor::ID_BODY,
    ] {
        assert!(ids.contains(&id), "{id:X} in {ids:X?}");
    }

    // Each engine frame's rpm is the engine model's rpm at that timestamp.
    let mut m = opentune_driver_vcar::VcarMonitor::attached(car.clone());
    let mut o = MonitorOpts::default();
    o.options.insert("vcar_realtime".into(), "0".into());
    o.filter_ids = vec![monitor::ID_ENGINE];
    use opentune_core::CanMonitor;
    m.start(&o).unwrap();
    let epoch_offset = car.lock().unwrap().epoch().elapsed().as_secs_f64();
    for _ in 0..50 {
        let f = m.next_frame(10).unwrap().unwrap();
        let rpm = u16::from_be_bytes([f.data()[0], f.data()[1]]) as f64 / 4.0;
        let model = engine_state(true, epoch_offset + f.ts_us as f64 / 1e6).rpm;
        // The monitor's offset was taken a moment before ours.
        assert!((rpm - model).abs() < 25.0, "frame {rpm} vs model {model}");
        assert_eq!(f.data()[5], 1, "running flag");
    }
}

#[test]
fn demo_pack_matches_the_virtual_car() {
    let pack = opentune_packs::load_pack(&pack_dir()).unwrap();
    assert!(pack.report.is_clean(), "{:?}", pack.report.notes);
    assert_eq!(pack.coding.len(), 6);
    assert_eq!(pack.adaptation.len(), 6);
    assert_eq!(pack.services.len(), 3);
    assert_eq!(pack.modules.len(), 1);

    let car = private_car();
    let v = car.lock().unwrap();
    let hex = |s: &serde_json::Value| u32::from_str_radix(s.as_str().unwrap(), 16).unwrap();
    for kind in ["coding", "adaptation", "services"] {
        for entry in std::fs::read_dir(pack_dir().join(kind)).unwrap() {
            let path = entry.unwrap().path();
            let j: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let (rq, rs) = (hex(&j["req_id"]), hex(&j["resp_id"]));
            let m = v
                .module(rq)
                .unwrap_or_else(|| panic!("{}: no module at {rq:X}", path.display()));
            assert_eq!(m.resp_id, rs, "{}", path.display());
            // Every DID the definition reads exists on its module.
            for ch in j["channels"].as_array().into_iter().flatten() {
                if ch["kind"] == "value" {
                    let did = ch["did"].as_u64().unwrap() as u16;
                    assert!(
                        m.server().did(did).is_some(),
                        "{} {did:04X}",
                        path.display()
                    );
                }
                if ch["kind"] == "actuator" {
                    let did = ch["did"].as_u64().unwrap() as u16;
                    assert!(
                        m.server().io_state(did).is_some(),
                        "{} io {did:04X}",
                        path.display()
                    );
                }
            }
            for did in j["addressing"]["dids"].as_array().into_iter().flatten() {
                let did = did.as_u64().unwrap() as u16;
                assert!(
                    m.server().did(did).is_some(),
                    "{} {did:04X}",
                    path.display()
                );
            }
        }
    }
    // Every coding definition decodes its module's current bytes.
    drop(v);
    for def in &pack.coding {
        let j: serde_json::Value = serde_json::from_str(&pack_file(&format!(
            "coding/{}.json",
            def.id.trim_start_matches("vcar-")
        )))
        .unwrap();
        let addr = (hex(&j["req_id"]), hex(&j["resp_id"]));
        let c = with_diag(&car, addr, |d| {
            opentune_coding::read_coding(d, def).unwrap()
        });
        opentune_coding::decode(def, &c).unwrap();
    }
}
