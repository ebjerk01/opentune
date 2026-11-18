//! The driver registry: every driver a release can talk to, opening one for an
//! adapter binding or an offline image, and the sidebar adapter probe.

use crate::{adapter, state::DebugLog};
use opentune_core::{ConnectConfig, Interface, Registry};
use serde::Serialize;
use serde_json::{json, Value};

/// Build the driver registry. The mock/file driver powers the offline workflow;
/// the hardware drivers are registered so the app can show what a release can
/// talk to (they are inert until a real interface is attached).
pub(crate) fn registry() -> Registry {
    let mut r = Registry::new();
    r.register("mock", "Mock / file driver (offline)", || {
        Box::new(opentune_driver_mock::MockDriver::new())
    });
    r.register("openport2", "OpenPort 2.0 (J2534/CDC-ACM)", || {
        Box::new(opentune_driver_openport2::OpenPort2Driver::new())
    });
    r.register("socketcan", "SocketCAN", || {
        Box::new(opentune_driver_socketcan::SocketCanDriver::new())
    });
    r.register("slcan", "SLCAN (serial CAN)", || {
        Box::new(opentune_driver_slcan::SlcanDriver::new())
    });
    r.register("elm327", "K-line (ELM327)", || {
        Box::new(opentune_driver_elm327::Elm327Driver::new())
    });
    r.register("kkl", "K-line (KKL 409.1 raw)", || {
        Box::new(opentune_driver_kkl::KklDriver::new())
    });
    r.register(
        "elm327obd",
        "ELM327 serial (OBD diagnostics + CAN memory read)",
        || Box::new(opentune_driver_elm327_obd::Elm327Obd::new()),
    );
    r.register("elm327net", "ELM327 over WiFi / Bluetooth", || {
        Box::new(opentune_driver_elm327net::Elm327NetDriver::new())
    });
    r.register("gsusb", "gs_usb / candleLight USB-CAN", || {
        Box::new(opentune_driver_gsusb::GsUsbDriver::new())
    });
    r.register("j2534", "J2534 PassThru (generic)", || {
        Box::new(opentune_driver_j2534::J2534Driver::new())
    });
    r.register("vcar", "Virtual car (offline simulator)", || {
        Box::new(opentune_driver_vcar::VcarDriver::new())
    });
    r.register(
        opentune_udssim::driver::DRIVER_ID,
        opentune_udssim::driver::DRIVER_NAME,
        || Box::new(opentune_udssim::UdssimDriver::new()),
    );
    // Bench tier until bench/vehicle-tested: only alpha builds list it.
    #[cfg(feature = "alpha")]
    r.register(
        opentune_driver_isotp_ble::DRIVER_ID,
        opentune_driver_isotp_ble::DRIVER_NAME,
        || Box::new(opentune_driver_isotp_ble::IsoTpBleDriver::new()),
    );
    r
}

/// Open a registry driver bound to `key`: its serial port/baud, CAN bus select
/// (`hs`/`ms` — for a switched ELM327 this must match the dongle's physical
/// HS/MS-CAN switch) and the target module's CAN request/response ids. Only the
/// adapter worker ([`adapter::AdapterHub`]) calls this; panels go through the hub.
pub fn open_registry_driver(key: &adapter::ConnKey) -> Result<Box<dyn Interface>, String> {
    let reg = registry();
    let driver = &key.driver;
    let mut drv = reg
        .make(driver)
        .ok_or_else(|| format!("unknown driver '{driver}'"))?;
    let cfg = key.config()?;
    drv.open(&cfg).map_err(|e| format!("open {driver}: {e}"))?;
    Ok(drv)
}

/// Probe for an adapter: open the selected driver fresh (which, for
/// `elm327obd`, runs the ELM327 handshake) and report whether it answered, its
/// firmware banner, battery voltage and capabilities. A general app-wide command
/// (not debug-only): the sidebar shows the adapter status everywhere. When
/// another adapter operation is running it reports `busy` instead of probing.
#[tauri::command]
pub async fn probe_adapter(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    log: tauri::State<'_, DebugLog>,
    hub: tauri::State<'_, adapter::AdapterHub>,
) -> Result<Value, String> {
    let key = adapter::ConnKey::new(
        &driver,
        port.as_deref(),
        baud.as_deref(),
        bus.as_deref(),
        adapter::Addr::ENGINE,
    );
    let res = hub.run("detect adapter", move |s| {
        // A probe is an explicit "is it there?" — always a fresh handshake.
        s.invalidate();
        let r = (|| {
            let drv = s.interface(&key)?;
            let info = drv.info();
            let caps = drv.capabilities();
            let battery_mv = drv.battery_millivolts().ok().flatten();
            Ok::<_, String>((info, caps, battery_mv))
        })();
        if r.is_err() {
            s.invalidate();
        }
        Ok(r)
    });
    match res {
        Ok(Ok((info, caps, battery_mv))) => {
            log.push(format!(
                "probe {driver}: FOUND {} {}",
                info.display_name,
                info.firmware.clone().unwrap_or_default()
            ));
            Ok(json!({
                "connected": true,
                "driver": info.id,
                "name": info.display_name,
                "firmware": info.firmware,
                "serial": info.serial,
                "battery_mv": battery_mv,
                "capabilities": {
                    "can": caps.can,
                    "can_fd": caps.can_fd,
                    "kline": caps.kline,
                    "read_memory": caps.read_memory,
                    "write_memory": caps.write_memory,
                    "bench_boot": caps.bench_boot,
                    "battery_voltage": caps.battery_voltage,
                    "programming_voltage": caps.programming_voltage,
                },
            }))
        }
        Ok(Err(e)) => {
            log.push(format!("probe {driver}: {e}"));
            Ok(json!({ "connected": false, "error": e }))
        }
        // The hub refused (another operation holds the adapter) — not "absent".
        Err(e) => Ok(json!({ "connected": false, "busy": true, "error": e })),
    }
}

/// Open the mock/file driver on `path` (the offline image).
pub(crate) fn open_file(path: &str) -> Result<Box<dyn Interface>, String> {
    let reg = registry();
    let mut drv = reg.make("mock").ok_or("mock driver missing")?;
    let mut cfg = ConnectConfig::default();
    cfg.options.insert("file".into(), path.to_string());
    drv.open(&cfg).map_err(|e| e.to_string())?;
    Ok(drv)
}

#[derive(Serialize)]
pub(crate) struct DriverInfo {
    id: String,
    name: String,
}

#[tauri::command]
pub fn list_drivers() -> Vec<DriverInfo> {
    registry()
        .list()
        .iter()
        .map(|e| DriverInfo {
            id: e.id.to_string(),
            name: e.display_name.to_string(),
        })
        .collect()
}
