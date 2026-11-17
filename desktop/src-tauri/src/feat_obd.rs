//! Live OBD-II dashboard feature: poll the ELM327 serial read-diagnostics driver
//! for the handful of real-time values a running engine exposes over generic
//! OBD-II mode 0x01 (RPM, coolant temperature, vehicle speed) plus the adapter's
//! battery voltage.
//!
//! The `live_*` helpers used here are **inherent** methods on the concrete
//! [`Elm327Obd`](opentune_driver_elm327_obd::Elm327Obd) driver (not on the
//! `dyn Interface` that `debug_open_diag` hands back), so this constructs the
//! driver directly and opens it with [`Interface::open`]. Battery voltage is read
//! through the [`Interface`] trait's `battery_millivolts`.
//!
//! Each value is read independently and wrapped with `.ok()`: one PID failing
//! (e.g. an ECU that does not report speed) leaves the others intact and reports
//! `null` for the missing value rather than failing the whole poll. With no car
//! connected the adapter answers `NO DATA` and every live value comes back
//! `null`; the battery voltage still reads because `ATRV` works with no bus.

use opentune_core::Interface;
use serde_json::{json, Value};

/// Open the ELM327 OBD driver and read one snapshot of the live dashboard
/// values. Returns `{rpm, coolant_c, speed_kph, battery_mv}` with `null` for any
/// value the vehicle/adapter did not provide. The frontend polls this on an
/// interval.
///
/// `port`/`baud`/`bus` mirror the global connection options (empty strings are
/// ignored so the driver defaults apply: `/dev/ttyUSB0`, `38400`, HS-CAN).
#[tauri::command]
pub async fn obd_live(
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
) -> Result<Value, String> {
    let mut d = opentune_driver_elm327_obd::Elm327Obd::new();
    let mut cfg = opentune_core::ConnectConfig::default();
    if let Some(p) = port {
        if !p.trim().is_empty() {
            cfg.options.insert("port".into(), p);
        }
    }
    if let Some(b) = baud {
        if !b.trim().is_empty() {
            cfg.options.insert("baud".into(), b);
        }
    }
    if let Some(bus) = bus {
        if !bus.trim().is_empty() {
            cfg.options.insert("bus".into(), bus);
        }
    }

    // Opening runs the ELM327 AT handshake; a failure here (no adapter, bad port)
    // is a real error worth surfacing to the panel.
    Interface::open(&mut d, &cfg).map_err(|e| e.to_string())?;

    // Each live value is read independently: a single failing/unsupported PID
    // reports null instead of aborting the others. `live_rpm` -> f32 (rpm),
    // `live_coolant_celsius` -> i16 (°C), `live_speed_kph` -> u8 (km/h).
    let rpm = d.live_rpm().ok();
    let coolant_c = d.live_coolant_celsius().ok();
    let speed_kph = d.live_speed_kph().ok();
    // Battery voltage via the Interface trait (ATRV); Result<Option<u32>>.
    let battery_mv = Interface::battery_millivolts(&mut d).ok().flatten();

    Ok(json!({
        "rpm": rpm,
        "coolant_c": coolant_c,
        "speed_kph": speed_kph,
        "battery_mv": battery_mv,
    }))
}
