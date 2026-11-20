//! # Live datalogging panel (backend)
//!
//! Two Tauri commands that back the "Live log" panel. The live loop is driven
//! from the frontend, which calls [`log_poll_once`] on an interval. Each poll
//! goes through the app-wide adapter hub ([`crate::adapter`]), which reuses the
//! already-opened adapter between ticks (no full ELM327 re-init per tick) and
//! rejects the tick with "adapter busy" while another panel owns the adapter.
//! A poll in which nothing answered at the transport (or the adapter reset)
//! drops the opened adapter so the next poll re-opens it; a poll in which the
//! ECU only sent negative responses is alive ([`crate::adapter::PollLink`]).
//!
//! Parameter definitions are RomRaider-style `logger.xml` files parsed by
//! [`opentune_logdef`], or a definition pack's `loggers/*.json` (the same
//! [`opentune_logdef::LogDef`] model, serialised as JSON). UDS `0x22` DIDs,
//! OBD-II mode-01 PIDs and UDS `0x23` memory reads are driven (see
//! [`opentune_logdef::poll_once`]); anything else comes back with an
//! explanatory `note` rather than a value, which the UI shows verbatim.
//!
//! ## Live session (UDS `0x2C` fast mode, Subaru SSM)
//!
//! [`log_start`] / [`log_tick`] / [`log_stop`] drive an
//! [`opentune_logdef::LiveSession`] (docs/LIVE-LOGGING.md): the channels are
//! packed into dynamically defined DIDs where the ECU allows it, with a
//! per-channel polling fallback and the reason; or, with an SSM transport,
//! the module is identified (`BF`), the definition reduced to what it
//! supports and the channels read in batched `A8` requests
//! ([`opentune_logdef::ssm`], the same code as `opentune log --transport
//! ssm|ssm-can`). Because the dynamic DIDs live in the ECU's diagnostic
//! session, the session is **one long-running adapter operation**
//! ([`AdapterHub::start_op`]): it holds the adapter until it stops, so another
//! panel cannot change the session underneath it. The job owns the
//! `LiveSession` on the adapter worker and serves ticks one at a time over a
//! channel (ticks can never overlap).
//!
//! The job runs on the core resilient loop
//! ([`opentune_core::resilient::run_resilient`]), one round per panel tick,
//! so it has the CLI's link semantics: a tick in which requests were sent and
//! none got any response is *dead* (a tick of negative responses is alive);
//! after [`DEAD_TICKS`](opentune_hub::livelog::DEAD_TICKS) dead ticks in a
//! row, or at once on an adapter reset, the link is **lost**. Without
//! reconnect the session then stops, as it always did. With reconnect
//! (opt-in, see [`live_policy`]) the adapter is closed and re-opened
//! **through this session's own hub `Session`** with a growing backoff, and
//! `LiveSession::start` runs again so the dynamic DIDs are re-defined. Every
//! link event becomes a marker sample (`param_id` `link`,
//! [`opentune_logdef::link_event_sample`]) in the next tick's samples, and
//! each tick reports the link state
//! ([`LinkView`](opentune_hub::livelog::LinkView)). The session stops —
//! clearing the dynamic DIDs while the link is alive — on [`log_stop`] and
//! when no tick arrives for [`IDLE_STOP`] (panel closed, window gone).
//!
//! The session engine itself (and the stateless poll) lives in
//! [`opentune_hub::livelog`], shared with other front ends; the commands here
//! are thin glue over it.

use crate::adapter::AdapterHub;
pub use opentune_hub::livelog::LiveLogState;
use opentune_hub::livelog::{
    await_live_start, begin_live, chrono_millis, live_request, load_def, poll_on, sample_json,
    stop_live, tick_live, LiveStart,
};
use serde_json::{json, Value};

/// Parse a `logger.xml` definition and return the list of available parameters.
///
/// Each entry is `{ id, name, units }`. A definition can list the same parameter
/// id under several ECU variants; the UI only needs one checkbox per id, so
/// duplicates are collapsed to the first occurrence while preserving order.
#[tauri::command]
pub fn log_params(def_path: String) -> Result<Value, String> {
    let def = load_def(&def_path)?;

    let mut seen = std::collections::HashSet::new();
    let mut params = Vec::new();
    for (_proto, _ecu, p) in def.params() {
        if !seen.insert(p.id.clone()) {
            continue;
        }
        params.push(json!({
            "id": p.id,
            "name": p.name,
            "units": p.units,
        }));
    }

    Ok(json!({
        "path": def_path,
        "version": def.version,
        "count": params.len(),
        "params": params,
    }))
}

/// Poll the selected parameters **once** and return the samples. An empty
/// `ids` selects every enabled parameter in the definition.
///
/// This is the per-tick worker for the live loop. Reads go to the target module
/// (UI `tx_id`/`rx_id`, else the engine ECU). Every parameter always yields a
/// sample; a failed or unsupported read is carried in `note`, never silently
/// dropped. When nothing answered at the transport, or the adapter reset, the
/// cached adapter is dropped so the next tick re-opens it (recovers from an
/// unplugged / reset adapter); `link` says which (`up`, `silent`, `lost`).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn log_poll_once(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    tx_id: Option<String>,
    rx_id: Option<String>,
    def_path: String,
    timeout_ms: u64,
    ids: Vec<String>,
    hub: tauri::State<'_, crate::adapter::AdapterHub>,
) -> Result<Value, String> {
    let def = load_def(&def_path)?;
    let (addr, assumed) = crate::adapter::read_addr(crate::adapter::resolve(
        tx_id.as_deref(),
        rx_id.as_deref(),
        None,
    )?);
    let key = crate::adapter::ConnKey::new(
        &driver,
        port.as_deref(),
        baud.as_deref(),
        bus.as_deref(),
        addr,
    );
    let (samples, link) = hub.run("live log", move |s| {
        poll_on(s, &key, timeout_ms, &def, &ids)
    })?;
    let rows: Vec<Value> = samples.iter().map(sample_json).collect();

    Ok(json!({
        "ts": chrono_millis(),
        "count": rows.len(),
        "samples": rows,
        "target": addr.json(assumed),
        "link": link,
    }))
}

/// Start a live session over the selected parameters: fast (`0x2C`) when
/// `fast` and the ECU allows it, else per-channel polling (with the reason);
/// with `transport` `ssm`/`ssm-can`, Subaru SSM to `ssm_target`. `reconnect`
/// re-opens a lost link ([`live_policy`]). Holds the adapter until
/// [`log_stop`]. Returns `{ status, channels, params, ssm, target, link }`.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn log_start(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    tx_id: Option<String>,
    rx_id: Option<String>,
    def_path: String,
    timeout_ms: u64,
    ids: Vec<String>,
    fast: bool,
    dynamic_did: Option<String>,
    max_record: Option<usize>,
    fast_session: Option<String>,
    transport: Option<String>,
    ssm_target: Option<String>,
    ssm_max_packet: Option<usize>,
    reconnect: Option<bool>,
    hub: tauri::State<'_, AdapterHub>,
    state: tauri::State<'_, LiveLogState>,
) -> Result<Value, String> {
    let req = live_request(LiveStart {
        driver: &driver,
        port: port.as_deref(),
        baud: baud.as_deref(),
        bus: bus.as_deref(),
        tx_id: tx_id.as_deref(),
        rx_id: rx_id.as_deref(),
        def_path: &def_path,
        timeout_ms,
        ids,
        fast,
        dynamic_did: dynamic_did.as_deref(),
        max_record,
        fast_session: fast_session.as_deref(),
        transport: transport.as_deref(),
        ssm_target: ssm_target.as_deref(),
        ssm_max_packet,
        reconnect: reconnect.unwrap_or(false),
    })?;
    let target = req.target.clone();
    let rx = begin_live(&hub, &state, req)?;
    let st = tauri::async_runtime::spawn_blocking(move || await_live_start(rx))
        .await
        .map_err(|e| format!("live start task failed: {e}"))??;
    let link = state.current().and_then(|r| r.link());
    Ok(json!({
        "status": st.status,
        "channels": st.params.len(),
        "params": st.params,
        "ssm": st.ssm,
        "target": target,
        "link": link,
    }))
}

/// One sample of every channel from the running session:
/// `{ ts, samples, status, rate, running, ended, target, link }`. Link
/// events arrive as marker samples (`paramId` `link`).
#[tauri::command]
pub async fn log_tick(state: tauri::State<'_, LiveLogState>) -> Result<Value, String> {
    tick_live(&state)
}

/// Stop the running session, clear its dynamic DIDs and release the adapter:
/// `{ running, status, ended, rate, target, link, events, ssm }` (`events`:
/// link markers no tick picked up).
#[tauri::command]
pub async fn log_stop(
    hub: tauri::State<'_, AdapterHub>,
    state: tauri::State<'_, LiveLogState>,
) -> Result<Value, String> {
    stop_live(&hub, &state)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A definition pack's JSON logger loads through the same path as an XML
    /// `logger.xml`, so the Live log panel can open `packs/*/loggers/*.json`.
    #[test]
    fn loads_a_pack_json_logger() {
        let p = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../packs/generic-obd2/loggers/obd2-mode01.json"
        );
        let def = load_def(p).unwrap();
        assert!(def.param_count() > 20);
        let v = log_params(p.to_string()).unwrap();
        assert!(v.to_string().contains("obd01_0c_rpm"));
    }
}
