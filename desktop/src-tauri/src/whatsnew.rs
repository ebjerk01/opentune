//! One-time "What's new" after an update.
//!
//! On start the UI asks [`whatsnew_check`]. The last version the user has seen
//! lives in `<app config dir>/whatsnew.json`, outside the app bundle, so it
//! survives an update replacing the bundle.
//!
//! * No file and no earlier install: a fresh install. The running version is
//!   recorded silently and nothing is shown (the popup is for updates, not the
//!   first run).
//! * No file but an earlier install (`prior_install`): an update from a release
//!   before this file existed (0.7.x). Releases up to 0.7 wrote nothing of
//!   their own outside the webview, so the UI decides: `whatsnew.js` loads
//!   first and snapshots whether OpenTune's own localStorage keys (`ot.*`,
//!   `opentune.*`, `ot_*`: sidebar groups, recent projects, dashboard, ...)
//!   already exist before any other script runs. Only an earlier session can
//!   have written them, so a fresh install never sees the popup; an old
//!   install that never saved any UI state does not see it either.
//! * Same version: nothing.
//! * Older version stored: the popup, with this version's highlights from the
//!   bundled CHANGELOG.md, or a plain "Updated to vX" when it has no section.
//!   [`whatsnew_seen`] (on dismiss) records the version so it shows only once.
//! * Newer version stored (a downgrade): recorded silently, nothing shown.
//!
//! The notes are the repository's CHANGELOG.md compiled in, so they work
//! offline and always match the build. The version is this crate's own
//! version, the same tree as the changelog, also when another app embeds the
//! core.
//!
//! The changelog parsing, the version comparison and the state-file check
//! live in [`opentune_hub::whatsnew`] (shared with other front ends); this
//! module embeds the changelog and the version and finds the state file.

// `whatsnew` is a private module: only what the commands use is re-exported
// (`Group` / `Notes` are reachable as `opentune_hub::whatsnew::*`).
pub use opentune_hub::whatsnew::{check_at, WhatsNew};
use opentune_hub::whatsnew::{write_seen, FILE};

const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");
/// The running core version (`desktop/src-tauri/Cargo.toml`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn state_file(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    use tauri::Manager;
    app.path()
        .app_config_dir()
        .map(|d| d.join(FILE))
        .map_err(|e| format!("no app config directory ({e})"))
}

/// What to show after an update, or `null`. `prior_install`: OpenTune UI
/// state from an earlier session existed before this one wrote any.
#[tauri::command]
pub fn whatsnew_check(app: tauri::AppHandle, prior_install: Option<bool>) -> Option<WhatsNew> {
    let file = state_file(&app).ok()?;
    check_at(&file, VERSION, CHANGELOG, prior_install.unwrap_or(false))
}

/// The popup was dismissed: never show it again for this version.
#[tauri::command]
pub fn whatsnew_seen(app: tauri::AppHandle) -> Result<(), String> {
    let file = state_file(&app)?;
    write_seen(&file, VERSION).map_err(|e| format!("could not record the version: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentune_hub::whatsnew::notes_for;

    #[test]
    fn bundled_changelog_covers_this_version() {
        // Release hygiene: bump-version + the CHANGELOG section go together.
        let n = notes_for(CHANGELOG, VERSION)
            .unwrap_or_else(|| panic!("CHANGELOG.md has no entries for {VERSION}"));
        assert!(n
            .groups
            .iter()
            .all(|g| g.items.iter().all(|i| !i.is_empty())));
    }
}
