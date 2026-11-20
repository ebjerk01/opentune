//! # CAN capture panel (backend)
//!
//! Records every frame on the bus to a file (candump log / CSV / Vector ASC)
//! through the driver's passive [`CanMonitor`](opentune_core::CanMonitor),
//! listen-only where the adapter supports it.
//!
//! A capture is one long-running operation on the app-wide adapter hub
//! ([`AdapterHub::start_op`]): it holds the adapter for its whole duration, so
//! every other panel gets "adapter busy" until it ends. The driver is created
//! fresh from the registry and **never `open`ed** — opening a diagnostic
//! session could transmit; the monitor opens the hardware itself. Live
//! counters live in [`CandumpState`], outside the hub, so `candump_status` and
//! `candump_stop` never wait on the busy adapter; `candump_stop` ends the
//! capture through a stop flag the capture loop checks every 100 ms.
//!
//! Commands (all `async`):
//! - `candump_start({driver, port?, baud?, bus?, path, format, filterIds?,
//!   durationS?, bitrate?}) -> {ok: true}`
//! - `candump_status() -> {running, path, format, frames, bytes, fps, elapsed_s,
//!   dropped, ids: [{id, ext, count, hz, dlc, last}], error, adapter, …}`
//! - `candump_stop() -> {path, frames, bytes, elapsed_s, dropped}`
//!
//! The capture job, its counters and stop handling live in
//! [`opentune_hub::ops::candump`], shared with other front ends; this module
//! keeps the Tauri commands.

use crate::adapter::AdapterHub;
use opentune_candump::{monitor_opts, Format};
pub use opentune_hub::ops::candump::CandumpState;
use opentune_hub::ops::candump::{
    await_start, begin_capture, capture_status, registry_driver, stop_capture, StartRequest,
    STOP_TIMEOUT,
};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Start a capture to `path`. Refused while another capture (or any other
/// adapter operation) is running. Returns once the adapter is capturing.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn candump_start(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    path: String,
    format: String,
    filter_ids: Option<Vec<u32>>,
    duration_s: Option<u32>,
    bitrate: Option<u32>,
    hub: tauri::State<'_, AdapterHub>,
    state: tauri::State<'_, CandumpState>,
) -> Result<Value, String> {
    let format: Format = format.parse()?;
    let filter = filter_ids.unwrap_or_default();
    if let Some(bad) = filter.iter().find(|&&id| id > 0x1FFF_FFFF) {
        return Err(format!("filter id {bad:X} is larger than 29 bits"));
    }
    let req = StartRequest {
        opts: monitor_opts(
            port.as_deref(),
            baud.as_deref(),
            bus.as_deref(),
            bitrate.filter(|&b| b > 0),
            filter,
        ),
        driver: driver.trim().to_string(),
        path,
        format,
        duration: duration_s
            .filter(|&d| d > 0)
            .map(|d| Duration::from_secs(d.into())),
    };
    let rx = begin_capture(&hub, &state, req, registry_driver)?;
    let adapter = tauri::async_runtime::spawn_blocking(move || await_start(rx))
        .await
        .map_err(|e| format!("capture start task failed: {e}"))??;
    Ok(json!({ "ok": true, "adapter": adapter }))
}

/// Live counters of the current (or last) capture. Never touches the adapter.
#[tauri::command]
pub async fn candump_status(state: tauri::State<'_, CandumpState>) -> Result<Value, String> {
    Ok(capture_status(&state))
}

/// Stop the capture, finish the file and release the adapter.
#[tauri::command]
pub async fn candump_stop(
    hub: tauri::State<'_, AdapterHub>,
    state: tauri::State<'_, CandumpState>,
) -> Result<Value, String> {
    let cap = state
        .current()
        .ok_or_else(|| "no CAN capture has been started".to_string())?;
    cap.stop.store(true, Ordering::Relaxed);
    // Wait off the async runtime; status/stop never need the hub's busy slot.
    tauri::async_runtime::spawn_blocking(move || cap.wait_done(STOP_TIMEOUT))
        .await
        .map_err(|e| format!("capture stop task failed: {e}"))?;
    stop_capture(&hub, &state)
}
