//! Debug panel (alpha builds): raw requests to the adapter, the offline UDS
//! simulator and the session log.
//!
//! These commands drive real adapters and the service engines. They refuse to run
//! unless this is an alpha (non-stable) build, and every exchange is appended to the
//! session log so a run can be saved and handed back for debugging.

use crate::{
    adapter,
    common::{hexs, parse_hex},
    state::{BuildChannel, DebugLog},
};

/// Gate: debug commands run only on a pre-release (non-stable) channel.
pub(crate) fn alpha_only(channel: &BuildChannel) -> Result<(), String> {
    if channel.debug_enabled() {
        Ok(())
    } else {
        Err("debug commands are available only in alpha builds".into())
    }
}

/// Send one raw UDS/OBD request to a live (or mock) adapter and return the hex
/// response. The exchange is appended to the session log. `tx_id`/`rx_id`
/// select the target module (hex; default: the engine ECU 7E0/7E8). Async so
/// the blocking serial I/O never runs on the UI thread.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn debug_raw_request(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    tx_id: Option<String>,
    rx_id: Option<String>,
    request: String,
    timeout_ms: u64,
    log: tauri::State<'_, DebugLog>,
    hub: tauri::State<'_, adapter::AdapterHub>,
    channel: tauri::State<'_, BuildChannel>,
) -> Result<String, String> {
    alpha_only(&channel)?;
    let req = parse_hex(&request)?;
    let (addr, _) = adapter::read_addr(adapter::resolve(tx_id.as_deref(), rx_id.as_deref(), None)?);
    let key = adapter::ConnKey::new(
        &driver,
        port.as_deref(),
        baud.as_deref(),
        bus.as_deref(),
        addr,
    );
    let req2 = req.clone();
    let t0 = std::time::Instant::now();
    let res = hub.run("raw request", move |s| {
        s.with_diag(&key, timeout_ms, |d| {
            opentune_core::RawDiag::request(d, &req2)
        })
    });
    let to = format!("{:03X}", addr.tx_id);
    match res {
        Ok(resp) => {
            let ms = t0.elapsed().as_millis();
            let hex = hexs(&resp);
            log.push(format!("[{to}] -> {}  <- {hex}  ({ms} ms)", hexs(&req)));
            Ok(hex)
        }
        Err(m) => {
            log.push(format!("[{to}] -> {}  ERR {m}", hexs(&req)));
            Err(m)
        }
    }
}

/// Run the in-memory UDS conformance server offline: seed DIDs, handle raw
/// requests, return the hex responses. No hardware.
#[tauri::command]
pub fn debug_simulate(
    dids: Vec<String>,
    requests: Vec<String>,
    require_security_write: bool,
    log: tauri::State<DebugLog>,
    channel: tauri::State<BuildChannel>,
) -> Result<Vec<String>, String> {
    use opentune_udssim::server::{ServerConfig, UdsServer};
    alpha_only(&channel)?;
    let cfg = ServerConfig {
        require_security_for_write: require_security_write,
        ..Default::default()
    };
    let mut server = UdsServer::with_config(cfg);
    for d in &dids {
        let (id, hx) = d
            .split_once('=')
            .ok_or_else(|| format!("expected ID=HEX, got '{d}'"))?;
        let did = u16::from_str_radix(id.trim().trim_start_matches("0x"), 16)
            .map_err(|_| format!("invalid DID '{id}'"))?;
        server.set_did(did, parse_hex(hx)?);
    }
    let mut out = Vec::new();
    for r in &requests {
        let req = parse_hex(r)?;
        let resp = server.handle(&req);
        let hex = hexs(&resp);
        log.push(format!("sim -> {}  <- {hex}", hexs(&req)));
        out.push(hex);
    }
    Ok(out)
}

/// Return the current session log.
#[tauri::command]
pub fn debug_log_dump(log: tauri::State<DebugLog>) -> Vec<String> {
    log.0.lock().map(|v| v.clone()).unwrap_or_default()
}

/// Clear the session log.
#[tauri::command]
pub fn debug_log_clear(log: tauri::State<DebugLog>) {
    if let Ok(mut v) = log.0.lock() {
        v.clear();
    }
}
