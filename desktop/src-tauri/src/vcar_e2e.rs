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
