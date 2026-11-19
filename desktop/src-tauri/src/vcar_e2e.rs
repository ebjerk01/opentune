//! End to end over the real adapter hub with the `vcar` (virtual car) driver
//! from the registry: the same worker thread, per-module re-binding and panel
//! helpers the desktop commands use, against the bundled `packs/vcar-demo`.

use crate::adapter::{self, AdapterHub, Addr, ConnKey};
use crate::{feat_adaptation, feat_coding, feat_scan};
use opentune_adaptation::{AdaptationDef, RoutineOp};
use opentune_autoscan::VehicleModules;
use opentune_coding::{get, read_coding, CodingDef};
use opentune_core::obd::ObdSession;

fn pack(rel: &str) -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs/vcar-demo");
    std::fs::read_to_string(p.join(rel)).unwrap()
}

/// A connection to a private virtual car (its own instance in the vcar table).
fn conn(addr: Addr) -> ConnKey {
    ConnKey::new("vcar", Some("demo@desktop-e2e"), None, Some("hs"), addr)
}

#[test]
fn adapter_hub_drives_the_virtual_car() {
    let hub = AdapterHub::with_registry();
    let mods = VehicleModules::from_json(&pack("modules/vcar-demo.json")).unwrap();

    // Auto-scan: every module on its own binding; the unfitted one is absent.
    let m = mods.clone();
    let scan = hub
        .run("auto-scan", move |s| {
            Ok(feat_scan::scan_modules(s, &conn(Addr::ENGINE), &m, 100))
        })
        .unwrap();
    let present: Vec<&str> = scan
        .iter()
        .filter(|r| r.present)
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(present.len(), 7, "6 modules + the OBD view: {present:?}");
    let parking = scan.iter().find(|r| r.name.starts_with("Parking")).unwrap();
    assert!(!parking.present);
    let abs = scan.iter().find(|r| r.name == "ABS / ESC").unwrap();
    assert_eq!(abs.dtcs[0].code, "C0035");

    // Clear only the ABS; a fresh scan still sees the engine's codes.
    let only_abs = VehicleModules {
        vehicle: mods.vehicle.clone(),
        modules: mods
            .modules
            .iter()
            .filter(|m| m.name == "ABS / ESC")
            .cloned()
            .collect(),
    };
    let cleared = hub
        .run("clear DTCs", move |s| {
            Ok(feat_scan::clear_modules(
                s,
                &conn(Addr::ENGINE),
                &only_abs,
                100,
            ))
        })
        .unwrap();
    assert!(cleared[0].cleared, "{:?}", cleared[0].error);
    let m = mods.clone();
    let rescan = hub
        .run("auto-scan", move |s| {
            Ok(feat_scan::scan_modules(s, &conn(Addr::ENGINE), &m, 100))
        })
        .unwrap();
    let dtcs = |n: &str| rescan.iter().find(|r| r.name == n).unwrap().dtcs.len();
    assert_eq!(dtcs("ABS / ESC"), 0);
    assert_eq!(dtcs("Engine"), 3);

    // Coding write on the body module, addressed by the definition's req_id.
    let text = pack("coding/body.json");
    let def = CodingDef::from_json(&text).unwrap();
    let body = adapter::write_addr(
        adapter::resolve(None, None, Some(&text)).unwrap(),
        "coding write",
    )
    .unwrap();
    assert_eq!((body.tx_id, body.rx_id), (0x70E, 0x778));
    let d = def.clone();
    let report = hub
        .run("coding write", move |s| {
            feat_coding::write_one(s, &conn(body), &d, 100, "Rear fog lamp", 0)
        })
        .unwrap();
    assert!(report.is_verified(), "{:?}", report.outcome);
    let back = hub
        .run("coding read", move |s| {
            s.with_diag(&conn(body), 100, |d| {
                read_coding(d, &def).map_err(|e| e.to_string())
            })
            .map(|c| {
                (
                    get(&def, &c, "Rear fog lamp"),
                    get(&def, &c, "Daytime running lights"),
                )
            })
        })
        .unwrap();
    assert_eq!((back.0.unwrap(), back.1.unwrap()), (0, 2));

    // An adaptation routine needs its target: without req_id/resp_id the
    // mutating op is refused; with the def's address it runs on that module.
    let text = pack("adaptation/abs.json");
    let def = AdaptationDef::from_json(&text).unwrap();
    let mut stripped: serde_json::Value = serde_json::from_str(&text).unwrap();
    stripped.as_object_mut().unwrap().remove("req_id");
    stripped.as_object_mut().unwrap().remove("resp_id");
    let none = adapter::resolve(None, None, Some(&stripped.to_string())).unwrap();
    assert!(adapter::write_addr(none, "routine start/stop").is_err());
    let target = adapter::write_addr(
        adapter::resolve(None, None, Some(&text)).unwrap(),
        "routine",
    )
    .unwrap();
    let r = hub
        .run("adaptation routine", move |s| {
            feat_adaptation::routine_once(
                s,
                &conn(target),
                &def,
                "Brake bleed sequence",
                RoutineOp::Start,
                &[],
                100,
            )
        })
        .unwrap();
    assert_eq!(r.value, Some(0.0));

    // Dashboard: OBD live data + battery voltage on the engine binding.
    let (rpm, mv) = hub
        .run("obd live", move |s| {
            let key = conn(Addr::ENGINE);
            let ro = s.with_diag(&key, 100, |d| Ok(ObdSession::new().read(d, &[0x0C])))?;
            let mv = s
                .interface(&key)?
                .battery_millivolts()
                .map_err(|e| e.to_string())?;
            Ok((ro.values["rpm"].as_ref().and_then(|v| v.first()), mv))
        })
        .unwrap();
    assert!(rpm.unwrap() >= 800.0);
    assert_eq!(mv, Some(14_100));
}

/// The `Check my car` wizard (ui/src/feat_wizard.js) end to end on the virtual
/// car: the same commands, in the same order, with the same arguments the
/// panel sends — probe, identify, scan (OBD view + module list), the DTC
/// database lookups behind Explain, and the live snapshot. The whole flow is
/// read-only: afterwards the car still holds every code it started with.
#[test]
fn check_my_car_wizard_flow_on_the_virtual_car() {
    use crate::{drivers, feat_dash, feat_obdinfo, utilities};
    use opentune_core::obd::inspect::Sections;

    let hub = AdapterHub::with_registry();
    const PORT: &str = "demo@wizard-e2e";
    let dash = || feat_dash::Conn {
        driver: "vcar".into(),
        port: Some(PORT.into()),
        bus: Some("hs".into()),
        ..Default::default()
    };
    let insp = || feat_obdinfo::Conn {
        driver: "vcar".into(),
        port: Some(PORT.into()),
        bus: Some("hs".into()),
        ..Default::default()
    };
    let req = |sections: &[&str], all_ecus: bool| feat_obdinfo::Request {
        sections: Sections::parse(sections).unwrap(),
        all_ecus,
        frame: 0,
    };
    let key = ConnKey::new("vcar", Some(PORT), None, Some("hs"), Addr::ENGINE);

    // 1. Connect: probe_adapter — a diagnostic link and a battery reading.
    let (probe, line) = drivers::probe_json(&hub, key.clone(), "vcar");
    assert_eq!(probe["connected"], true, "{probe}");
    assert!(line.unwrap().contains("FOUND"));
    assert_eq!(probe["capabilities"]["can"], true);
    assert_eq!(probe["capabilities"]["battery_voltage"], true);
    assert_eq!(probe["battery_mv"], 14_100, "engine running by default");

    // 2. Identify: obd_inspect(info) on the engine, obd_supported for the VIN
    //    fallback, vin_decode for the plain-language VIN breakdown.
    let id = feat_obdinfo::inspect_impl(&hub, insp(), req(&["info"], false)).unwrap();
    let engine = &id["ecus"][0];
    assert_eq!(engine["responded"], true, "{engine}");
    assert_eq!(engine["addr"]["tx"], "7E0");
    let vin = engine["info"]["vin"].as_str().unwrap().to_string();
    assert_eq!(vin.len(), 17);
    let sup = feat_dash::supported_impl(&hub, dash()).unwrap();
    assert_eq!(sup["vin"], vin.as_str(), "mode 09 and the fallback agree");
    assert!(!sup["pids"].as_array().unwrap().is_empty());
    // The decoder answers for any 17-character VIN (flags explain oddities).
    let _ = utilities::vin_decode(vin.clone());

    // 3. Scan: the OBD view of every OBD address (readiness + DTCs) …
    let obd = feat_obdinfo::inspect_impl(&hub, insp(), req(&["readiness", "dtcs"], true)).unwrap();
    let ecus = obd["ecus"].as_array().unwrap();
    assert!(!ecus.is_empty(), "{obd}");
    let engine = ecus.iter().find(|e| e["addr"]["tx"] == "7E0").unwrap();
    let status = &engine["readiness"]["status"];
    assert!(status["mil"].is_boolean(), "{engine}");
    assert!(!status["monitors"].as_array().unwrap().is_empty());
    let stored: Vec<&str> = engine["dtcs"]["stored"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    let pending: Vec<&str> = engine["dtcs"]["pending"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    assert!(
        stored.contains(&"P0101") && stored.contains(&"P0420"),
        "{stored:?}"
    );
    assert!(pending.contains(&"P0171"), "{pending:?}");
    // … then the vehicle's module list (autoscan_run's per-module scan).
    let mods = VehicleModules::from_json(&pack("modules/vcar-demo.json")).unwrap();
    let m = mods.clone();
    let k = key.clone();
    let scan = hub
        .run("auto-scan", move |s| {
            Ok(feat_scan::scan_modules(s, &k, &m, 100))
        })
        .unwrap();
    assert_eq!(scan.iter().filter(|r| r.present).count(), 7);
    let codes_before: usize = scan.iter().map(|r| r.dtcs.len()).sum();
    assert!(codes_before > 0);

    // 4. Explain: every description the wizard shows is the DTC database's
    //    own text for that code — the scan never made one up.
    for d in engine["dtcs"]["stored"].as_array().unwrap() {
        let code = d["code"].as_str().unwrap().to_string();
        let db = utilities::dtc_lookup(code.clone()).unwrap();
        assert_eq!(db["description"], d["description"], "{code}");
        assert!(db["system"].as_str().unwrap().starts_with("Powertrain"));
    }
    for r in scan.iter().filter(|r| r.present) {
        for d in &r.dtcs {
            let db = utilities::dtc_lookup(d.code.clone()).unwrap();
            assert_eq!(db["description"], d.description.as_str(), "{}", d.code);
        }
    }

    // 5. Live check: the wizard's mode-01 set (coolant, trims, rpm, speed,
    //    module voltage) plus the adapter's battery reading.
    let live =
        feat_dash::read_impl(&hub, dash(), vec![0x05, 0x06, 0x07, 0x0C, 0x0D, 0x42]).unwrap();
    assert_eq!(live["battery_mv"], 14_100);
    let v = &live["values"];
    assert!(v["rpm"].as_f64().unwrap() >= 600.0, "{live}");
    assert!(v["coolant_temp"].is_number(), "{live}");
    assert_eq!(v["speed"].as_f64(), Some(0.0), "idling, not moving");
    // A PID the ECU does not report is null with the reason, never a guess.
    for k in ["stft_b1", "ltft_b1"] {
        if v[k].is_null() {
            assert!(live["errors"][k].is_string(), "{k}: {live}");
        }
    }
    // The warn/crit thresholds the wizard rates coolant with.
    let rows = feat_dash::pid_table_rows();
    let coolant = rows.iter().find(|r| r["key"] == "coolant_temp").unwrap();
    assert!(coolant["warn"].is_number() && coolant["crit"].is_number());

    // Read-only: a fresh scan finds every code the car started with.
    let m = mods.clone();
    let rescan = hub
        .run("auto-scan", move |s| {
            Ok(feat_scan::scan_modules(s, &key, &m, 100))
        })
        .unwrap();
    assert_eq!(
        rescan.iter().map(|r| r.dtcs.len()).sum::<usize>(),
        codes_before
    );
}

/// The same wizard flow on the built-in OBD simulator (`obdsim`): no adapter,
/// no module list — identify, the OBD scan and the live check all answer.
#[test]
fn check_my_car_wizard_flow_on_the_obd_simulator() {
    use crate::{feat_dash, feat_obdinfo};
    use opentune_core::obd::inspect::Sections;

    let hub = AdapterHub::with_registry();
    let dash = || feat_dash::Conn {
        driver: "obdsim".into(),
        ..Default::default()
    };
    let insp = || feat_obdinfo::Conn {
        driver: "obdsim".into(),
        ..Default::default()
    };
    let req = |sections: &[&str], all_ecus: bool| feat_obdinfo::Request {
        sections: Sections::parse(sections).unwrap(),
        all_ecus,
        frame: 0,
    };
    let id = feat_obdinfo::inspect_impl(&hub, insp(), req(&["info"], false)).unwrap();
    assert!(id["ecus"][0]["info"]["vin"].is_string(), "{id}");
    let scan = feat_obdinfo::inspect_impl(&hub, insp(), req(&["readiness", "dtcs"], true)).unwrap();
    assert_eq!(scan["ecus"].as_array().unwrap().len(), 2, "{scan}");
    assert_eq!(scan["ecus"][0]["readiness"]["status"]["mil"], true);
    feat_dash::supported_impl(&hub, dash()).unwrap();
    let live = feat_dash::read_impl(&hub, dash(), vec![0x05, 0x0C, 0x0D, 0x42]).unwrap();
    assert!(live["values"]["rpm"].is_number(), "{live}");
    assert_eq!(live["battery_mv"], 14_100);
}
