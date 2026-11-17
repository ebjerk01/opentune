//! App-wide managed state: the resolved data directories, the build channel,
//! the long-operation guard and the debug session log.

use crate::startup;
use opentune_core::{catalog::CatalogEntry, ecudef::EcuDef};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::Manager;

// Destructive / long-running operations (a mock flash, a map write-back) must
// never overlap with a software update: an in-place binary replacement while a
// flash is mid-write could corrupt the running session. `OpGuard` is a single
// managed slot naming the one long operation in flight (or `None` when idle).
//
// `begin()` claims the slot and hands back an `OpToken`; dropping the token
// (normally, on panic, or via `?`) frees the slot, so a failed command can
// never leave the app wedged as "busy". The update flow claims the same slot,
// so a flash cannot start mid-update and an update cannot start mid-flash.
#[derive(Clone, Default)]
pub(crate) struct OpGuard(Arc<Mutex<Option<String>>>);

/// RAII release token: the long operation is in flight for as long as this is
/// alive, and the guard slot is cleared the instant it drops.
#[derive(Debug)]
pub(crate) struct OpToken(Arc<Mutex<Option<String>>>);

impl Drop for OpToken {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = None;
        }
    }
}

impl OpGuard {
    /// Claim the single long-operation slot for `name`, or return a clear error
    /// naming whatever is already running. The returned token frees the slot on
    /// drop.
    pub(crate) fn begin(&self, name: &str) -> Result<OpToken, String> {
        let mut slot = self
            .0
            .lock()
            .map_err(|_| "internal error: operation guard was poisoned".to_string())?;
        if let Some(cur) = slot.as_ref() {
            return Err(format!(
                "'{cur}' is already running — wait for it to finish before starting another operation"
            ));
        }
        *slot = Some(name.to_string());
        Ok(OpToken(self.0.clone()))
    }

    /// Name of the long operation currently in flight, if any.
    pub(crate) fn current(&self) -> Option<String> {
        self.0.lock().ok().and_then(|s| s.clone())
    }
}

/// Resolved data directories, found once at startup (managed state).
pub struct AppData {
    /// ECU definition directory.
    pub ecudefs: Option<PathBuf>,
    /// Vehicle catalog directory.
    pub catalog: Option<PathBuf>,
    /// In-app guide (Markdown) directory.
    pub guide: Option<PathBuf>,
    /// Control-unit reference database (`database/units/*.json`).
    pub database: Option<PathBuf>,
    /// Definition packs (`packs/<id>/pack.json` + module lists, loggers,
    /// service scripts…); the panels' file pickers open here.
    pub packs: Option<PathBuf>,
}

fn first_existing(cands: &[PathBuf]) -> Option<PathBuf> {
    cands
        .iter()
        .find(|p| !p.as_os_str().is_empty() && p.exists())
        .cloned()
}

/// Build channel reported to the UI (managed state).
///
/// The public binary uses [`BuildChannel::compiled`]: `"alpha"` when compiled
/// with the `alpha` feature (the Debug panel is available), else `"stable"`. An
/// embedder may set its own channel name through [`DesktopConfig::channel`].
/// Every channel other than `"stable"` enables the debug/live-engine commands,
/// and the UI shows its pre-release sections for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildChannel(String);

impl BuildChannel {
    /// The default release channel: debug commands are refused.
    pub const STABLE: &'static str = "stable";
    /// The public pre-release channel (`--features alpha`).
    pub const ALPHA: &'static str = "alpha";
    /// The channel of a plugin build that ships `tier: "bench"` panels (the UI
    /// shows its Bench section for `alpha` and `bench`).
    pub const BENCH: &'static str = "bench";

    /// A named channel (an embedder's own name, or one of the constants).
    pub fn new(name: impl Into<String>) -> Self {
        BuildChannel(name.into())
    }

    /// The channel this crate was compiled for: `alpha` with the `alpha`
    /// feature, else `stable`.
    pub fn compiled() -> Self {
        if cfg!(feature = "alpha") {
            Self::new(Self::ALPHA)
        } else {
            Self::new(Self::STABLE)
        }
    }

    /// The channel name as the UI sees it.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Debug / live-engine commands run on every channel except `stable`.
    pub fn debug_enabled(&self) -> bool {
        self.0 != Self::STABLE
    }
}

impl Default for BuildChannel {
    fn default() -> Self {
        Self::compiled()
    }
}

/// The build channel name (`"stable"`, `"alpha"`, or an embedder's own).
#[tauri::command]
pub fn build_channel(channel: tauri::State<'_, BuildChannel>) -> String {
    channel.as_str().to_string()
}

/// A shared, in-memory session log of diagnostic exchanges (managed state; an
/// embedder's commands may append to it too).
#[derive(Clone, Default)]
pub struct DebugLog(pub(crate) Arc<Mutex<Vec<String>>>);

impl DebugLog {
    /// Append one line to the session log.
    pub fn push(&self, line: impl Into<String>) {
        if let Ok(mut v) = self.0.lock() {
            v.push(line.into());
        }
    }
}

/// The ECU definitions in `dir`: parsed once, shared, and re-parsed only when a
/// file there changes (see [`startup::defs`]).
pub(crate) fn load_defs(dir: &Option<PathBuf>) -> Arc<Vec<EcuDef>> {
    startup::defs(dir)
}

/// The vehicle catalog in `dir`, cached like [`load_defs`].
pub(crate) fn load_cat(dir: &Option<PathBuf>) -> Arc<Vec<CatalogEntry>> {
    startup::catalog(dir)
}

#[tauri::command]
pub fn data_paths(state: tauri::State<AppData>) -> Value {
    json!({
        "ecudefs": state.ecudefs.as_ref().map(|p| p.display().to_string()),
        "catalog": state.catalog.as_ref().map(|p| p.display().to_string()),
        "guide": state.guide.as_ref().map(|p| p.display().to_string()),
        "database": state.database.as_ref().map(|p| p.display().to_string()),
        "packs": state.packs.as_ref().map(|p| p.display().to_string()),
    })
}

/// Resolve the data directories. `extra_roots` (an embedder's own data
/// locations) are searched after the override and the bundled resources but
/// before this crate's dev fallbacks.
pub(crate) fn resolve_data<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    extra_roots: &[PathBuf],
) -> AppData {
    let res = app.path().resource_dir().ok();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")); // desktop/src-tauri
    let repo = manifest.join("../.."); // repo root in dev
    let env_dir = std::env::var("OPENTUNE_DATA").ok().map(PathBuf::from);

    // Try, in order: OPENTUNE_DATA override, the bundled resource dir (several
    // layouts Tauri may produce), the embedder's roots, a dev copy next to
    // src-tauri, then the repo.
    let find = |name: &str| -> Option<PathBuf> {
        let mut cands: Vec<PathBuf> = Vec::new();
        if let Some(e) = &env_dir {
            cands.push(e.join(name));
        }
        if let Some(r) = &res {
            cands.push(r.join(name));
            cands.push(r.join("data").join(name));
            cands.push(r.join("_up_").join(name));
            cands.push(r.join("_up_").join("_up_").join(name)); // ../../<name> flattening
        }
        cands.extend(extra_roots.iter().map(|root| root.join(name)));
        cands.push(manifest.join(name));
        cands.push(repo.join(name));
        first_existing(&cands)
    };

    AppData {
        ecudefs: find("ecudefs"),
        catalog: find("catalog"),
        guide: find("guide"),
        database: find("database"),
        packs: find("packs"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_guard_blocks_second_operation() {
        let g = OpGuard::default();
        let tok = g.begin("flash").expect("first op claims the slot");
        assert_eq!(g.current().as_deref(), Some("flash"));
        // A software update must be refused while a flash holds the slot, and the
        // error must name what is running.
        let err = g.begin("software update").unwrap_err();
        assert!(
            err.contains("flash"),
            "error should name the running op: {err}"
        );
        drop(tok);
    }

    #[test]
    fn op_guard_releases_on_drop() {
        let g = OpGuard::default();
        {
            let _tok = g.begin("flash").unwrap();
            assert!(g.current().is_some());
        } // token dropped here (mirrors a command returning, via `?`, or panicking)
        assert!(g.current().is_none(), "slot must free when the token drops");
        // The slot is reusable afterwards (e.g. an update after the flash finished).
        let _tok2 = g
            .begin("software update")
            .expect("slot reusable after release");
        assert_eq!(g.current().as_deref(), Some("software update"));
    }

    #[test]
    fn op_guard_update_blocks_flash_symmetrically() {
        let g = OpGuard::default();
        let _upd = g.begin("software update").unwrap();
        // While an update holds the slot, a flash is refused too (no destructive
        // write can race an in-place binary replacement).
        assert!(g.begin("flash").is_err());
    }
}
