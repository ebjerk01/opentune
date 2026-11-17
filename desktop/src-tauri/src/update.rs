//! Software update (Tauri updater plugin).

use crate::state::OpGuard;
use serde_json::{json, Value};
use tauri::Emitter;
use tauri_plugin_updater::UpdaterExt;

// The updater PLUGIN is registered in `main()` and its endpoint + minisign
// public key live in `tauri.conf.json`. These commands are the missing invocation
// layer: check → (user confirms) → download+install → relaunch. Signature
// verification is NOT done here by hand — the plugin verifies every downloaded
// bundle against the configured `pubkey` inside `download()` before it is
// installed, so a tampered or wrongly-signed artifact fails the download step and
// never reaches disk. We only surface that failure to the UI.
//
// The whole install is gated behind [`OpGuard`]: it refuses to start while a
// flash or map write-back is in flight, and while it runs those refuse to start,
// so an in-place binary replacement can never race a destructive operation.

/// Event channel the install flow emits download progress on. The frontend
/// listens for `{ downloaded, total }` (total is `null` until the server sends a
/// content-length).
const UPDATE_PROGRESS_EVENT: &str = "update://progress";

/// Report the running version and whether a long operation currently blocks an
/// update. Lets the UI disable the "Install update" affordance proactively
/// instead of only learning on refusal.
#[tauri::command]
pub fn update_status(app: tauri::AppHandle, guard: tauri::State<OpGuard>) -> Value {
    let busy = guard.current();
    json!({
        "current_version": app.package_info().version.to_string(),
        "busy": busy.is_some(),
        "busy_op": busy,
    })
}

/// Check the configured endpoint for a newer release. Does NOT download or
/// install — it only tells the UI what is available so it can ask the user.
/// Offline (no network) this returns an error the UI shows as "check failed",
/// which is the honest outcome.
#[tauri::command]
pub async fn update_check(app: tauri::AppHandle) -> Result<Value, String> {
    let current = app.package_info().version.to_string();
    let updater = app
        .updater()
        .map_err(|e| format!("updater unavailable: {e}"))?;
    match updater.check().await {
        Ok(Some(update)) => Ok(json!({
            "available": true,
            "current_version": update.current_version,
            "version": update.version,
            "notes": update.body,
            "date_unix": update.date.map(|d| d.unix_timestamp()),
        })),
        Ok(None) => Ok(json!({
            "available": false,
            "current_version": current,
        })),
        Err(e) => Err(format!("update check failed: {e}")),
    }
}

/// Download and install the available update, emitting [`UPDATE_PROGRESS_EVENT`]
/// as bytes arrive. Refuses while a flash / map write-back is running, and claims
/// the same guard for its own duration. On success the app is NOT auto-restarted;
/// the UI confirms and calls [`update_relaunch`] so the user is never surprised by
/// a restart. Signature verification is enforced by the plugin's `download()`; a
/// bad signature or an interrupted download surfaces here as a hard error and
/// nothing is installed.
#[tauri::command]
pub async fn update_install(
    app: tauri::AppHandle,
    guard: tauri::State<'_, OpGuard>,
) -> Result<Value, String> {
    // Claim the long-operation slot for the whole install (freed on drop).
    let _op = guard.begin("software update")?;

    let updater = app
        .updater()
        .map_err(|e| format!("updater unavailable: {e}"))?;
    let update = match updater
        .check()
        .await
        .map_err(|e| format!("update check failed: {e}"))?
    {
        Some(u) => u,
        None => {
            return Ok(json!({ "installed": false, "up_to_date": true }));
        }
    };

    let version = update.version.clone();
    let app_ev = app.clone();
    let mut downloaded: u64 = 0;

    // `download_and_install` verifies the minisign signature against the bundled
    // pubkey inside `download()`; a mismatch or a truncated/interrupted download
    // returns Err here and nothing is written over the installed app.
    update
        .download_and_install(
            move |chunk_len, content_len| {
                downloaded += chunk_len as u64;
                let _ = app_ev.emit(
                    UPDATE_PROGRESS_EVENT,
                    json!({ "downloaded": downloaded, "total": content_len }),
                );
            },
            || {},
        )
        .await
        .map_err(|e| format!("update download/install failed (signature or network): {e}"))?;

    Ok(json!({ "installed": true, "up_to_date": false, "version": version }))
}

/// Relaunch the app so a freshly-installed update takes effect. Called only after
/// [`update_install`] reports success and the user confirms. Never returns.
#[tauri::command]
pub fn update_relaunch(app: tauri::AppHandle) {
    app.restart();
}
