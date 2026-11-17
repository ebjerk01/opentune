// OpenTune — full-vehicle Auto-scan + DTCs feature (backend).
//
// Thin Tauri command layer over the public `opentune-autoscan` engine: load a
// vehicle's module list (data) and, for EACH module, bind the adapter to that
// module's own CAN request/response ids (`req_id`/`resp_id`) before running the
// engine's per-module scan / clear. Runs through the app-wide adapter hub. All decoding/validation lives in the engine; this
// file only moves bytes in (connection + module JSON) and JSON out.
//
// Part of the public OpenTune desktop app; uses only the public OpenTune crates.

use crate::adapter::{AdapterHub, Addr, ConnKey, Session};
use opentune_autoscan::{ClearResult, ModuleAddr, ModuleResult, VehicleModules};
use serde_json::{json, Value};

/// Serialise one scanned module into the shape the Auto-scan panel renders:
/// name / present / identity (VIN + DIDs) / DTC list + count / non-fatal error.
fn module_to_json(m: &opentune_autoscan::ModuleResult) -> Value {
    let identity = m.identity.as_ref().map(|id| {
        let dids: Vec<Value> = id
            .dids
            .iter()
            .map(|d| {
                json!({
                    "did": format!("0x{:04X}", d.did),
                    "label": d.label,
                    "text": d.text,
                    "raw": d
                        .raw
                        .iter()
                        .map(|b| format!("{b:02X}"))
                        .collect::<Vec<_>>()
                        .join(" "),
                })
            })
            .collect();
        json!({ "vin": id.vin, "dids": dids })
    });

    let dtcs: Vec<Value> = m
        .dtcs
        .iter()
        .map(|d| {
            json!({
                "code": d.code,
                "description": d.description,
                "system": d.system.letter().to_string(),
                "status": d.status.map(|s| format!("0x{s:02X}")),
            })
        })
        .collect();

    json!({
        "name": m.name,
        "present": m.present,
        "identity": identity,
        "dtcs": dtcs,
        "dtcCount": m.dtcs.len(),
        "error": m.error,
    })
}

fn load_modules(modules_path: &str) -> Result<opentune_autoscan::VehicleModules, String> {
    let text = std::fs::read_to_string(modules_path)
        .map_err(|e| format!("read module list '{modules_path}': {e}"))?;
    opentune_autoscan::VehicleModules::from_json(&text)
        .map_err(|e| format!("parse module list: {e}"))
}

/// A module's own CAN address, as the adapter binding for it.
fn module_addr(m: &ModuleAddr) -> Addr {
    Addr {
        tx_id: m.req_id,
        rx_id: m.resp_id,
    }
}

/// Scan every module, re-binding the adapter to **that module's** `req_id` /
/// `resp_id` before probing it. (A single binding would send every module's
/// requests to one ECU and report it as present N times.) A module whose
/// binding cannot be opened is reported absent with the reason.
pub(crate) fn scan_modules(
    s: &mut Session,
    conn: &ConnKey,
    modules: &VehicleModules,
    timeout_ms: u64,
) -> Vec<ModuleResult> {
    modules
        .modules
        .iter()
        .map(|m| {
            let key = conn.with_addr(module_addr(m));
            s.with_diag(&key, timeout_ms, |d| {
                Ok(opentune_autoscan::scan_module(d, m))
            })
            .unwrap_or_else(|e| ModuleResult {
                name: m.name.clone(),
                present: false,
                identity: None,
                dtcs: Vec::new(),
                error: Some(format!("adapter: {e}")),
            })
        })
        .collect()
}

/// Clear DTCs module by module, each over a binding to that module's own
/// address, so `cleared` is only ever reported for the module that was
/// actually addressed and acknowledged the clear.
pub(crate) fn clear_modules(
    s: &mut Session,
    conn: &ConnKey,
    modules: &VehicleModules,
    timeout_ms: u64,
) -> Vec<ClearResult> {
    modules
        .modules
        .iter()
        .map(|m| {
            let key = conn.with_addr(module_addr(m));
            s.with_diag(&key, timeout_ms, |d| {
                Ok(opentune_autoscan::clear_module(d, m))
            })
            .unwrap_or_else(|e| ClearResult {
                name: m.name.clone(),
                cleared: false,
                error: Some(format!("adapter: {e}")),
            })
        })
        .collect()
}

/// Run a read-only full-vehicle auto-scan.
///
/// Loads a `VehicleModules` JSON list from `modules_path` and probes every
/// module for presence, identity (UDS) and stored DTCs, each on its own CAN
/// address. Never writes to a module.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn autoscan_run(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    modules_path: String,
    timeout_ms: u64,
    hub: tauri::State<'_, AdapterHub>,
) -> Result<Value, String> {
    let modules = load_modules(&modules_path)?;
    let conn = ConnKey::new(
        &driver,
        port.as_deref(),
        baud.as_deref(),
        bus.as_deref(),
        Addr::ENGINE,
    );
    let mods = modules.clone();
    let results = hub.run("auto-scan", move |s| {
        Ok(scan_modules(s, &conn, &mods, timeout_ms))
    })?;
    let report = opentune_autoscan::VehicleScan { modules: results };

    let present_count = report.present().count();
    let dtc_total = report.dtc_count();
    let modules_json: Vec<Value> = report
        .modules
        .iter()
        .zip(modules.modules.iter())
        .map(|(r, m)| {
            let mut v = module_to_json(r);
            v["target"] = module_addr(m).json(false);
            v
        })
        .collect();

    Ok(json!({
        "vehicle": modules.vehicle,
        "moduleCount": report.modules.len(),
        "presentCount": present_count,
        "dtcTotal": dtc_total,
        "modules": modules_json,
    }))
}

/// Clear stored DTCs on every present module in the list.
///
/// The engine's only mutating operation, kept out of [`autoscan_run`]: the UI
/// invokes it explicitly from the "Clear all DTCs" button. Each module is
/// cleared over a binding to its own CAN address.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn autoscan_clear(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    modules_path: String,
    timeout_ms: u64,
    hub: tauri::State<'_, AdapterHub>,
) -> Result<Value, String> {
    let modules = load_modules(&modules_path)?;
    let conn = ConnKey::new(
        &driver,
        port.as_deref(),
        baud.as_deref(),
        bus.as_deref(),
        Addr::ENGINE,
    );
    let mods = modules.clone();
    let results = hub.run("clear DTCs", move |s| {
        Ok(clear_modules(s, &conn, &mods, timeout_ms))
    })?;

    let cleared_count = results.iter().filter(|r| r.cleared).count();
    let results_json: Vec<Value> = results
        .iter()
        .zip(modules.modules.iter())
        .map(|(r, m)| {
            json!({
                "name": r.name,
                "cleared": r.cleared,
                "error": r.error,
                "target": module_addr(m).json(false),
            })
        })
        .collect();

    Ok(json!({
        "vehicle": modules.vehicle,
        "clearedCount": cleared_count,
        "results": results_json,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::testkit::SimBus;
    use opentune_autoscan::Protocol;
    use opentune_udssim::server::{DtcRecord, UdsServer};

    fn m(name: &str, tx: u32, rx: u32) -> ModuleAddr {
        ModuleAddr {
            name: name.into(),
            addr: 0,
            req_id: tx,
            resp_id: rx,
            protocol: Protocol::Uds,
        }
    }

    /// Engine at 7E0/7E8 with a VIN + a DTC, ABS at 760/768 with its own DTC,
    /// and an airbag at 715/77F that is not fitted.
    fn vehicle() -> (SimBus, VehicleModules) {
        let bus = SimBus::default();
        let mut engine = UdsServer::new();
        engine.set_did(0xF190, b"WVWZZZ1KZAW000001".to_vec());
        engine.add_dtc(DtcRecord::new([0x01, 0x01, 0x00], 0x24));
        bus.add(0x7E0, 0x7E8, engine);
        let mut abs = UdsServer::new();
        abs.set_did(0xF187, b"1K0907379".to_vec());
        abs.add_dtc(DtcRecord::new([0xC1, 0x00, 0x00], 0x2F));
        bus.add(0x760, 0x768, abs);
        let mods = VehicleModules {
            vehicle: "sim".into(),
            modules: vec![
                m("Engine", 0x7E0, 0x7E8),
                m("ABS", 0x760, 0x768),
                m("Airbag", 0x715, 0x77F),
            ],
        };
        (bus, mods)
    }

    fn conn() -> ConnKey {
        ConnKey::new("sim", None, None, Some("hs"), Addr::ENGINE)
    }

    #[test]
    fn scan_binds_each_module_to_its_own_address() {
        let (bus, mods) = vehicle();
        let mut s = bus.session();
        let r = scan_modules(&mut s, &conn(), &mods, 100);

        // One open per module, each on that module's ids.
        let opens: Vec<(u32, u32)> = bus
            .opens
            .borrow()
            .iter()
            .map(|a| (a.tx_id, a.rx_id))
            .collect();
        assert_eq!(opens, vec![(0x7E0, 0x7E8), (0x760, 0x768), (0x715, 0x77F)]);

        assert!(r[0].present && r[1].present);
        assert!(
            !r[2].present,
            "absent airbag must not borrow the engine's answer"
        );
        assert_eq!(
            r[0].identity.as_ref().unwrap().vin.as_deref(),
            Some("WVWZZZ1KZAW000001")
        );
        // The ABS reports its own identity/DTC, not the engine's.
        assert_eq!(r[1].identity.as_ref().unwrap().vin, None);
        assert_eq!(r[0].dtcs[0].code, "P0101");
        assert_eq!(r[1].dtcs[0].code, "U0100");
        assert!(!bus.sent_to(0x760).is_empty());
    }

    #[test]
    fn clear_reports_only_modules_actually_addressed() {
        let (bus, mods) = vehicle();
        let mut s = bus.session();
        let r = clear_modules(&mut s, &conn(), &mods, 100);
        assert!(r[0].cleared && r[1].cleared);
        assert!(!r[2].cleared, "no module at 715 → not cleared");

        // Exactly one 14 FF FF FF went to each present module, none twice to the engine.
        let clear = vec![0x14, 0xFF, 0xFF, 0xFF];
        let n = |tx| bus.sent_to(tx).iter().filter(|q| **q == clear).count();
        assert_eq!((n(0x7E0), n(0x760), n(0x715)), (1, 1, 0));
        assert_eq!(
            *bus.cleared.borrow(),
            vec![(0x7E0, 0x7E8), (0x760, 0x768)],
            "each clear acknowledged by the module it was addressed to"
        );
    }
}
