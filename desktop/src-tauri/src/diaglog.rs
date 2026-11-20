//! Diagnostics log: a small local record of what went wrong on this machine.
//!
//! `<app data dir>/logs/opentune.log`, one JSON object per line, capped and
//! rotated, masked at write time, local only. The log itself (records,
//! bounding and masking, the rotating writer, the process-wide instance, the
//! panic hook, rate-limited frontend reports and read-back) lives in the
//! workspace crate [`opentune_diaglog`], shared with other front ends; this
//! module re-exports it so the `opentune_desktop::diaglog::*` paths stay
//! stable, and adds the Tauri side: [`init`] (called from the setup plugin)
//! and the `diag_log*` commands.
//!
//! Why command errors are logged in the frontend: Tauri 2's generated
//! dispatch (`generate_handler!` → each command's own `__cmd__*` macro)
//! resolves a command's `Result` inside that macro and hands the response to
//! a responder that is crate-private (`InvokeResolver::new` is `pub(crate)`,
//! and `Builder` has no invoke-response hook), so a wrapper around the
//! registered handler only ever sees "handled: true". The one place every
//! command's `Err` does pass is the frontend's `window.__TAURI__.core.invoke`
//! promise, which `diaglog.js` wraps once for every panel and plugin.

pub use opentune_diaglog::*;

use crate::state::{BuildChannel, DebugLog};
use serde_json::{json, Value};
use std::fs;
use std::sync::Mutex;
use std::time::Instant;

/// The session header: the first record of every start, with this app's
/// version as `core` (see [`opentune_diaglog::session_header_with_core`]).
pub fn session_header(info: &SessionInfo) -> Record {
    session_header_with_core(info, env!("CARGO_PKG_VERSION"))
}

/// Open the process-wide log under the app data dir and write the session
/// header (called from the setup plugin). Without a data dir, or when the
/// file cannot be opened, logging is off for the session and the session
/// log says why.
pub(crate) fn init<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    channel: &BuildChannel,
    debug: &DebugLog,
) {
    use tauri::Manager;
    let note = debug.clone();
    set_failure_note(move |e| note.push(e));
    let info = SessionInfo {
        app: app.package_info().name.clone(),
        version: app.package_info().version.to_string(),
        channel: channel.as_str().to_string(),
        webkit: tauri::webview_version().ok(),
        render: crate::startup::render_summary(),
    };
    let res = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("diagnostics log off: no app data directory ({e})"))
        .and_then(|d| global().open(&d.join(DIR_NAME), session_header(&info)));
    if let Err(e) = res {
        global().disable();
        debug.push(e);
    }
}

static FRONTEND_LIMIT: Mutex<RateLimiter> = Mutex::new(RateLimiter::new(FRONTEND_PER_SEC));

// ---------------------------------------------------------------------------
// Commands (every channel: the log is local and public)
// ---------------------------------------------------------------------------

/// One frontend report (`ui/src/diaglog.js`): an uncaught error, an
/// unhandled rejection or a command that failed. Rate-limited to
/// [`FRONTEND_PER_SEC`] per second.
#[tauri::command]
pub async fn diag_log(level: Option<String>, src: Option<String>, msg: String, ctx: Option<Value>) {
    for r in accept_frontend(
        &FRONTEND_LIMIT,
        Instant::now(),
        level.as_deref(),
        src.as_deref(),
        &msg,
        ctx,
    ) {
        write_global(r);
    }
}

/// The last `limit` records (default 100, at most 2000) at `level` or
/// above (default: all), oldest first.
#[tauri::command]
pub async fn diag_log_tail(limit: Option<usize>, level: Option<String>) -> Vec<Record> {
    let Some(dir) = log_dir() else {
        return Vec::new();
    };
    let min = level
        .as_deref()
        .and_then(Level::parse)
        .unwrap_or(Level::Debug);
    read_tail(&dir, limit.unwrap_or(100).min(2000), min)
}

/// Where the log lives and how big it is.
#[tauri::command]
pub async fn diag_log_info() -> Value {
    let dir = log_dir();
    let files = dir.as_deref().map(log_files).unwrap_or_default();
    let bytes: u64 = files
        .iter()
        .filter_map(|p| fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    json!({
        "state": global().state_name(),
        "dir": dir.as_ref().map(|d| d.display().to_string()),
        "file": dir.as_ref().map(|d| file_path(d, 0).display().to_string()),
        "files": files.len(),
        "bytes": bytes,
        "max_file_bytes": MAX_FILE_BYTES,
        "rotated_files": ROTATED_FILES,
    })
}

/// Open the log folder in the system file manager; returns its path.
#[tauri::command]
pub async fn diag_log_open_folder() -> Result<String, String> {
    let dir = log_dir().ok_or("the diagnostics log is off for this session")?;
    #[cfg(target_os = "windows")]
    let mut c = std::process::Command::new("explorer");
    #[cfg(target_os = "macos")]
    let mut c = std::process::Command::new("open");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut c = std::process::Command::new("xdg-open");
    use std::process::Stdio;
    let mut child = c
        .arg(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot open {}: {e}", dir.display()))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(dir.display().to_string())
}

/// Delete the log's contents (the live file restarts with this session's
/// header).
#[tauri::command]
pub async fn diag_log_clear() -> Result<(), String> {
    global().clear()
}
