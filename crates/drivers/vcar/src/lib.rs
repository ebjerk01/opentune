//! # opentune-driver-vcar — the virtual car
//!
//! A whole multi-module vehicle simulated in memory, so every OpenTune panel and
//! engine (auto-scan, DTCs, coding, adaptation, service functions, OBD
//! dashboard, logging, CAN capture) can be used and tested end to end with no
//! hardware. Driver id: `vcar`.
//!
//! * Each module is an [`opentune_udssim::UdsServer`] behind its own
//!   server-side ISO-TP endpoint on a virtual **11-bit CAN** bus. The driver's
//!   [`DiagTransport`] is the core's own [`IsoTp`] over a [`VcarLink`], bound to
//!   the module selected by [`ConnectConfig::tx_id`] / [`rx_id`] (the engine,
//!   `0x7E0`/`0x7E8`, by default) — exactly like a real adapter. A request to an
//!   address no module listens on gets **no answer** (a timeout), so an
//!   auto-scan sees an absent module as absent.
//! * The OBD-capable module (the engine) answers functional OBD-II on `0x7DF`
//!   and on its physical id: mode 01 (incl. the supported-PID bitmaps) and
//!   mode 09 VIN via [`opentune_core::obd::sim::SimEcu`], modes 03 / 07 / 04 on
//!   the module's own DTC store.
//! * [`Interface::can_monitor`] produces synthetic periodic bus traffic from the
//!   same engine model that feeds the OBD data and the battery voltage
//!   (~14.1 V running, ~12.6 V engine off).
//! * SecurityAccess (0x27) uses a **TEST-ONLY** seed→key: `key = seed XOR
//!   mask` ([`test_security_key`]). It is deliberately trivial and is not, nor
//!   derived from, any manufacturer's algorithm.
//!
//! The vehicle is **data**: a profile JSON ([`Profile`], the built-in
//! `profiles/demo.json`) defines modules, addresses, identity DIDs, DTCs, coding
//! and adaptation DIDs, routines and IO channels. The addresses and values are
//! the virtual car's own and make no claim about any real vehicle.
//!
//! ## Shared state
//!
//! Apps re-open a driver for every module address (the desktop's adapter hub,
//! the per-module auto-scan). So that a cleared DTC or a coding write survives
//! that, the vehicle lives in a process-wide table keyed by its *spec*, and
//! every `vcar` driver opened with the same spec talks to the same car until the
//! process ends (or `reset` is given).
//!
//! ## Options ([`ConnectConfig::options`])
//!
//! - `profile` (or `port`, so the desktop's Port field works): `demo` (default)
//!   or a path to a profile JSON. Append `@name` for an independent instance of
//!   the same profile (`demo@bench`).
//! - `engine`: `on` / `off` — start or stop the engine when opening.
//! - `reset`: `1` / `true` — rebuild the car from its profile (forget writes and
//!   clears) when opening.
//!
//! [`rx_id`]: ConnectConfig::rx_id
//! [`IsoTp`]: opentune_core::protocol::IsoTp

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod link;
pub mod monitor;
pub mod profile;
pub mod vehicle;

pub use link::VcarLink;
pub use monitor::VcarMonitor;
pub use profile::Profile;
pub use vehicle::{engine_state, EngineState, Module, Vehicle};

use opentune_core::protocol::{DiagTransport, IsoTp};
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Progress,
    Result,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// The OBD-II functional (broadcast) request id.
pub const FUNCTIONAL_ID: u32 = 0x7DF;
/// The engine module's physical request id (the default binding).
pub const ENGINE_TX: u32 = 0x7E0;
/// The engine module's response id.
pub const ENGINE_RX: u32 = 0x7E8;
/// The built-in profile's spec.
pub const DEMO_SPEC: &str = "demo";

/// A vehicle shared by every driver (and monitor) opened on the same spec.
pub type SharedVehicle = Arc<Mutex<Vehicle>>;

/// The **test-only** SecurityAccess key for `seed`: every byte XOR `xor_mask`
/// (the virtual car's documented test algorithm — not any manufacturer's).
pub fn test_security_key(seed: &[u8], xor_mask: u8) -> Vec<u8> {
    seed.iter().map(|b| b ^ xor_mask).collect()
}

fn vehicles() -> &'static Mutex<HashMap<String, SharedVehicle>> {
    static VEHICLES: OnceLock<Mutex<HashMap<String, SharedVehicle>>> = OnceLock::new();
    VEHICLES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Normalise a spec: blank → `demo`.
fn norm(spec: &str) -> &str {
    let s = spec.trim();
    if s.is_empty() {
        DEMO_SPEC
    } else {
        s
    }
}

/// Build a fresh vehicle for `spec` (`demo`, or a profile path; `@name` suffix
/// ignored here).
fn build(spec: &str) -> Result<Vehicle> {
    let profile_part = spec.rsplit_once('@').map_or(spec, |(p, _)| p).trim();
    let profile = if profile_part.is_empty() || profile_part == DEMO_SPEC {
        Profile::demo()
    } else {
        Profile::load(std::path::Path::new(profile_part))?
    };
    Vehicle::from_profile(&profile)
}

/// The shared vehicle for `spec`, creating it on first use. See the crate docs
/// for the spec syntax.
pub fn shared_vehicle(spec: &str) -> Result<SharedVehicle> {
    let spec = norm(spec);
    let mut map = vehicles()
        .lock()
        .map_err(|_| Error::Protocol("virtual car table poisoned".into()))?;
    if let Some(v) = map.get(spec) {
        return Ok(v.clone());
    }
    let v = Arc::new(Mutex::new(build(spec)?));
    map.insert(spec.to_string(), v.clone());
    Ok(v)
}

/// Rebuild the shared vehicle for `spec` from its profile, discarding every
/// write and clear. Drivers already holding it see the fresh car.
pub fn reset_vehicle(spec: &str) -> Result<SharedVehicle> {
    let spec = norm(spec);
    let fresh = build(spec)?;
    let v = shared_vehicle(spec)?;
    *v.lock()
        .map_err(|_| Error::Protocol("virtual car state poisoned".into()))? = fresh;
    Ok(v)
}

/// The spec named by an option lookup: `profile`, else `port`, else `demo`.
pub(crate) fn spec_from<'a>(get: impl Fn(&str) -> Option<&'a str>) -> &'a str {
    get("profile")
        .or_else(|| get("port"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEMO_SPEC)
}

fn truthy(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "running" => Some(true),
        "0" | "false" | "no" | "off" | "stopped" => Some(false),
        _ => None,
    }
}

/// The virtual-car interface driver. See the crate docs.
#[derive(Default)]
pub struct VcarDriver {
    /// A vehicle fixed at construction (tests, embedders); `None` resolves the
    /// shared vehicle from the open options.
    attached: Option<SharedVehicle>,
    vehicle: Option<SharedVehicle>,
    diag: Option<IsoTp>,
    monitor: VcarMonitor,
    target: (u32, u32),
}

impl VcarDriver {
    /// A driver that resolves its vehicle from the `profile`/`port` option.
    pub fn new() -> Self {
        Self::default()
    }

    /// A driver bound to one specific vehicle (ignores `profile`/`port`), e.g.
    /// a private car built from a test profile.
    pub fn with_vehicle(vehicle: SharedVehicle) -> Self {
        Self {
            monitor: VcarMonitor::attached(vehicle.clone()),
            attached: Some(vehicle),
            ..Self::default()
        }
    }

    /// The vehicle this driver is talking to (after `open`).
    pub fn vehicle(&self) -> Option<&SharedVehicle> {
        self.vehicle.as_ref()
    }

    /// The `(tx_id, rx_id)` module binding of the open session.
    pub fn target(&self) -> Option<(u32, u32)> {
        self.diag.as_ref().map(|_| self.target)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Vehicle>> {
        self.vehicle
            .as_ref()
            .ok_or(Error::NotConnected)?
            .lock()
            .map_err(|_| Error::Protocol("virtual car state poisoned".into()))
    }
}

/// `22 <did>` over `t`; the value bytes, or `None` on any refusal / timeout.
fn read_did(t: &mut dyn DiagTransport, did: u16) -> Option<Vec<u8>> {
    let [hi, lo] = did.to_be_bytes();
    match t.request(&[0x22, hi, lo], 200).ok()?.as_slice() {
        [0x62, h, l, data @ ..] if *h == hi && *l == lo => Some(data.to_vec()),
        _ => None,
    }
}

impl Interface for VcarDriver {
    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: "vcar".into(),
            display_name: "Virtual car (offline simulator)".into(),
            firmware: Some("offline".into()),
            serial: None,
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            can: true,
            battery_voltage: true,
            ..Default::default()
        }
    }

    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        let opt = |k: &str| cfg.options.get(k).map(String::as_str);
        let vehicle = match &self.attached {
            Some(v) => v.clone(),
            None => {
                let spec = spec_from(opt);
                if opt("reset").and_then(truthy) == Some(true) {
                    reset_vehicle(spec)?
                } else {
                    shared_vehicle(spec)?
                }
            }
        };
        if let Some(e) = opt("engine") {
            let on = truthy(e)
                .ok_or_else(|| Error::Config(format!("vcar: engine must be on/off, got '{e}'")))?;
            vehicle
                .lock()
                .map_err(|_| Error::Protocol("virtual car state poisoned".into()))?
                .set_engine_running(on);
        }
        let tx = if cfg.tx_id == 0 { ENGINE_TX } else { cfg.tx_id };
        let rx = if cfg.rx_id == 0 { ENGINE_RX } else { cfg.rx_id };
        if tx > 0x7FF || rx > 0x7FF {
            return Err(Error::Config(format!(
                "vcar: the virtual car uses 11-bit CAN ids (got {tx:X}/{rx:X})"
            )));
        }
        self.diag = Some(IsoTp::new(Box::new(VcarLink::new(vehicle.clone())), tx, rx));
        self.vehicle = Some(vehicle);
        self.target = (tx, rx);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.diag = None;
        self.vehicle = None;
        Ok(())
    }

    fn battery_millivolts(&mut self) -> Result<Option<u32>> {
        let v = match &self.vehicle {
            Some(_) => self.lock()?.engine_now().battery_mv,
            // Not opened: the default car's battery, like a voltmeter on the plug.
            None => {
                shared_vehicle(DEMO_SPEC)?
                    .lock()
                    .map_err(|_| Error::Protocol("virtual car state poisoned".into()))?
                    .engine_now()
                    .battery_mv
            }
        };
        Ok(Some(v))
    }

    /// Identity of the bound module over UDS (`22 F190/F18A/F188/F191/F197`, …).
    fn read_ecu_id(&mut self) -> Result<EcuId> {
        let target = self.target;
        let t = self.diag.as_mut().ok_or(Error::NotConnected)?;
        // Presence first, so an absent module is a clean timeout.
        t.request(&[0x3E, 0x00], 200)?;
        let text = |v: Option<Vec<u8>>| {
            v.map(|b| String::from_utf8_lossy(&b).trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let mut id = EcuId::default();
        for (did, key) in [
            (0xF187u16, "part_number"),
            (0xF188, "sw_number"),
            (0xF189, "sw_version"),
            (0xF18A, "supplier"),
            (0xF190, "vin"),
            (0xF191, "hw_number"),
            (0xF195, "supplier_sw_version"),
            (0xF197, "system_name"),
        ] {
            if let Some(v) = text(read_did(t, did)) {
                id.raw.insert(key.into(), v);
            }
        }
        id.vin = id.raw.get("vin").cloned();
        id.hw_number = id.raw.get("hw_number").cloned();
        id.sw_number = id.raw.get("sw_number").cloned();
        id.supplier = id.raw.get("supplier").cloned();
        let name = self
            .lock()?
            .module(target.0)
            .map(|m| m.name.clone())
            .or_else(|| id.raw.get("system_name").cloned());
        id.ecu_name = name;
        Ok(id)
    }

    fn read_memory(
        &mut self,
        _region: MemoryRegion,
        _progress: &mut dyn FnMut(Progress),
    ) -> Result<Vec<u8>> {
        Err(Error::Unsupported(
            "the virtual car has no flash memory; use the mock driver for read/flash".into(),
        ))
    }

    fn write_memory(
        &mut self,
        _region: MemoryRegion,
        _data: &[u8],
        _progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        Err(Error::Unsupported(
            "the virtual car has no flash memory; use the mock driver for read/flash".into(),
        ))
    }

    fn diag(&mut self) -> Option<&mut dyn DiagTransport> {
        self.diag.as_mut().map(|d| d as &mut dyn DiagTransport)
    }

    /// Synthetic bus traffic (see [`monitor`]).
    fn can_monitor(&mut self) -> Option<&mut dyn opentune_core::CanMonitor> {
        Some(&mut self.monitor)
    }
}
