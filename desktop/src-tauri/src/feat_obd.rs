//! Live OBD-II dashboard feature: poll the handful of real-time values a running
//! engine exposes over generic OBD-II mode 0x01 (RPM, coolant temperature,
//! vehicle speed) plus the adapter's battery voltage.
//!
//! Each poll goes through the app-wide adapter hub ([`crate::adapter`]): the
//! opened adapter is reused across ticks (no full ELM327 `ATZ` re-init per
//! tick), and a tick is rejected with "adapter busy" while another panel is
//! using the adapter. The PIDs are read over the driver's generic diagnostic
//! transport (`01 <pid>` → `41 <pid> <data>`), addressed to the engine ECU
//! (0x7E0/0x7E8); battery voltage comes from [`Interface::battery_millivolts`].
//!
//! Each value is read independently and reported `null` when missing: one PID
//! failing (e.g. an ECU that does not report speed) leaves the others intact.
//! With no car connected every live value comes back `null`; the battery
//! voltage still reads because `ATRV` works with no bus.

use crate::adapter::{AdapterHub, Addr, ConnKey};
use opentune_core::{Interface, RawDiag};
use serde_json::{json, Value};

/// Per-PID request timeout.
const PID_TIMEOUT_MS: u64 = 1000;

/// Read one mode-01 PID and return its data bytes (after `41 <pid>`).
fn read_pid(d: &mut dyn RawDiag, pid: u8) -> Option<Vec<u8>> {
    match d.request(&[0x01, pid]).ok()?.as_slice() {
        [0x41, p, data @ ..] if *p == pid && !data.is_empty() => Some(data.to_vec()),
        _ => None,
    }
}

/// `((A*256)+B)/4` rpm.
fn rpm(d: &[u8]) -> Option<f32> {
    (d.len() >= 2).then(|| f32::from((u16::from(d[0]) << 8) | u16::from(d[1])) / 4.0)
}

/// Read one snapshot of the live dashboard values. Returns
/// `{rpm, coolant_c, speed_kph, battery_mv}` with `null` for any value the
/// vehicle/adapter did not provide. The frontend polls this on an interval.
///
/// `driver` defaults to `elm327obd`; `port`/`baud`/`bus` mirror the global
/// connection options (empty strings are ignored so the driver defaults apply).
#[tauri::command]
pub async fn obd_live(
    driver: Option<String>,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    hub: tauri::State<'_, AdapterHub>,
) -> Result<Value, String> {
    let driver = driver
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| "elm327obd".to_string());
    let key = ConnKey::new(
        &driver,
        port.as_deref(),
        baud.as_deref(),
        bus.as_deref(),
        Addr::ENGINE,
    );
    let (rpm, coolant_c, speed_kph, battery_mv) = hub.run("OBD live", move |s| {
        // Opening runs the adapter handshake; a failure here (no adapter, bad
        // port) is a real error worth surfacing to the panel.
        let pids = s.with_diag(&key, PID_TIMEOUT_MS, |d| {
            Ok((read_pid(d, 0x0C), read_pid(d, 0x05), read_pid(d, 0x0D)))
        })?;
        let battery_mv = s
            .interface(&key)
            .ok()
            .and_then(|i: &mut dyn Interface| i.battery_millivolts().ok().flatten());
        let rpm = pids.0.as_deref().and_then(rpm);
        let coolant = pids.1.and_then(|d| d.first().map(|a| i16::from(*a) - 40));
        let speed = pids.2.and_then(|d| d.first().copied());
        if rpm.is_none() && coolant.is_none() && speed.is_none() && battery_mv.is_none() {
            // Nothing answered at all: drop the cached adapter so the next tick
            // re-opens it (recovers from an unplugged / reset adapter).
            s.invalidate();
        }
        Ok((rpm, coolant, speed, battery_mv))
    })?;

    Ok(json!({
        "rpm": rpm,
        "coolant_c": coolant_c,
        "speed_kph": speed_kph,
        "battery_mv": battery_mv,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_mode01_replies() {
        let mut ecu = |r: &[u8]| -> Result<Vec<u8>, String> {
            match r {
                [0x01, 0x0C] => Ok(vec![0x41, 0x0C, 0x1A, 0xF8]),
                [0x01, 0x05] => Ok(vec![0x41, 0x05, 0x7B]),
                _ => Err("NO DATA".into()),
            }
        };
        assert_eq!(
            read_pid(&mut ecu, 0x0C).as_deref().and_then(rpm),
            Some(1726.0)
        );
        assert_eq!(read_pid(&mut ecu, 0x05), Some(vec![0x7B]));
        assert_eq!(read_pid(&mut ecu, 0x0D), None);
        // A reply echoing a different PID is rejected.
        let mut wrong = |_r: &[u8]| -> Result<Vec<u8>, String> { Ok(vec![0x41, 0x0D, 0x00]) };
        assert_eq!(read_pid(&mut wrong, 0x0C), None);
    }
}
