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

use serde::{Deserialize, Serialize};
use std::path::Path;

const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");
const FILE: &str = "whatsnew.json";
/// The running core version (`desktop/src-tauri/Cargo.toml`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// A highlight longer than this is cut (on a char boundary) with an ellipsis.
const MAX_ITEM_CHARS: usize = 180;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Group {
    /// "Added", "Changed", "Fixed", ...
    pub heading: String,
    /// One line per changelog entry: its bold title, else its first sentence.
    pub items: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notes {
    pub version: String,
    pub date: Option<String>,
    pub groups: Vec<Group>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WhatsNew {
    pub version: String,
    /// The version recorded before this one ("" when unknown: an install from
    /// before this file existed, or an unreadable file).
    pub previous: String,
    /// `None` when the changelog has no entries for this version.
    pub notes: Option<Notes>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Seen {
    last_seen_version: Option<String>,
}

// ---------------------------------------------------------------------------
// Changelog
// ---------------------------------------------------------------------------

/// The highlights of `version`'s section (`## [x.y.z] - date`), or `None` when
/// there is no such section or it has no entries.
pub fn notes_for(changelog: &str, version: &str) -> Option<Notes> {
    let head = format!("## [{version}]");
    let mut lines = changelog.lines();
    let header = lines.by_ref().find(|l| l.starts_with(&head))?;
    let date = header[head.len()..]
        .trim()
        .trim_start_matches(['-', '\u{2013}', '\u{2014}'])
        .trim();
    let mut groups: Vec<Group> = Vec::new();
    let mut item: Option<String> = None;
    let flush = |groups: &mut Vec<Group>, item: &mut Option<String>| {
        if let (Some(raw), Some(g)) = (item.take(), groups.last_mut()) {
            let h = highlight(&raw);
            if !h.is_empty() {
                g.items.push(h);
            }
        }
    };
    for line in lines {
        if line.starts_with("## ") {
            break;
        }
        if let Some(h) = line.strip_prefix("### ") {
            flush(&mut groups, &mut item);
            groups.push(Group {
                heading: h.trim().to_string(),
                items: Vec::new(),
            });
        } else if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            flush(&mut groups, &mut item);
            item = Some(rest.trim().to_string());
        } else if line.starts_with(char::is_whitespace) && !line.trim().is_empty() {
            // Continuation (or a nested bullet) of the current entry.
            if let Some(raw) = item.as_mut() {
                raw.push(' ');
                raw.push_str(line.trim());
            }
        } else if line.trim().is_empty() {
            // A blank line ends the entry; prose between entries is skipped.
            flush(&mut groups, &mut item);
        }
    }
    flush(&mut groups, &mut item);
    groups.retain(|g| !g.items.is_empty());
    if groups.is_empty() {
        return None;
    }
    Some(Notes {
        version: version.to_string(),
        date: (!date.is_empty()).then(|| date.to_string()),
        groups,
    })
}

/// One line for an entry: the leading `**bold title**`, else the first
/// sentence. Links keep their text; inline code keeps its backticks (the UI
/// renders them).
fn highlight(raw: &str) -> String {
    let raw = raw.trim();
    let text = match raw
        .strip_prefix("**")
        .and_then(|r| r.find("**").map(|e| &r[..e]))
    {
        Some(title) if !title.trim().is_empty() => title.trim().to_string(),
        _ => first_sentence(raw).to_string(),
    };
    clip(&strip_links(&text).replace("**", ""), MAX_ITEM_CHARS)
}

/// Up to and including the first ". " (or ".** ", a sentence ending inside
/// bold) outside inline code; the whole text if there is none.
fn first_sentence(s: &str) -> &str {
    let mut in_code = false;
    for (i, c) in s.char_indices() {
        match c {
            '`' => in_code = !in_code,
            '.' if !in_code => {
                let rest = &s[i + 1..];
                let after_bold = rest.strip_prefix("**").unwrap_or(rest);
                if after_bold.starts_with(' ') {
                    return &s[..s.len() - after_bold.len()];
                }
            }
            _ => {}
        }
    }
    s
}

/// `[text](url)` -> `text`.
fn strip_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        match after.find("](").and_then(|close| {
            after[close + 2..]
                .find(')')
                .map(|end| (close, close + 2 + end + 1))
        }) {
            Some((close, end)) => {
                out.push_str(&rest[..open]);
                out.push_str(&after[..close]);
                rest = &after[end..];
            }
            None => {
                out.push_str(&rest[..=open]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out = out.trim_end().to_string();
    out.push('\u{2026}');
    out
}

// ---------------------------------------------------------------------------
// Seen-version state
// ---------------------------------------------------------------------------

/// `Some(true)` when `a` is a newer `x.y.z` than `b`, `None` when either does
/// not parse (pre-release suffixes are ignored).
fn newer(a: &str, b: &str) -> Option<bool> {
    fn parse(v: &str) -> Option<[u64; 3]> {
        let core = v.trim().trim_start_matches('v');
        let core = core.split(['-', '+']).next()?;
        let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
        let out = [it.next()??, it.next()??, it.next()??];
        it.next().is_none().then_some(out)
    }
    Some(parse(a)? > parse(b)?)
}

fn write_seen(file: &Path, version: &str) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_vec_pretty(&Seen {
        last_seen_version: Some(version.to_string()),
    })?;
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, file)
}

/// The check against a state file: what to show, if anything. Records the
/// version silently on a fresh install or a downgrade. `prior_install`: the UI
/// found state an earlier session left behind (see the module docs).
pub fn check_at(
    file: &Path,
    current: &str,
    changelog: &str,
    prior_install: bool,
) -> Option<WhatsNew> {
    let previous = match std::fs::read(file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && prior_install => String::new(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = write_seen(file, current);
            return None;
        }
        // Unreadable or damaged: there was an install, so treat it as an update.
        Err(_) => String::new(),
        Ok(bytes) => match serde_json::from_slice::<Seen>(&bytes) {
            Ok(Seen {
                last_seen_version: Some(v),
            }) => v,
            Ok(Seen {
                last_seen_version: None,
            })
            | Err(_) => String::new(),
        },
    };
    if previous == current {
        return None;
    }
    if newer(current, &previous) == Some(false) {
        let _ = write_seen(file, current);
        return None;
    }
    Some(WhatsNew {
        version: current.to_string(),
        previous,
        notes: notes_for(changelog, current),
    })
}

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

    const LOG: &str =
        "# Changelog\n\nIntro prose.\n\n## [Unreleased]\n\n### Added\n- Not released yet.\n\n\
## [1.2.0] - 2026-01-02\n\nSome prose about the release.\n\n### Added\n\
- **Bold title** \u{2014} with detail that\n  wraps onto a second line.\n\
- Plain entry with `code. inside` first. Second sentence here.\n\
- See [the guide](docs/GUIDE.md) for more\n  - nested bullet\n\
- Desktop: **bold in the middle,\n  wrapped.** More text. Again.\n\n\
### Fixed\n- A fix.\n\n### Removed\n\n\
## [1.1.0] - 2025-12-01\n\n### Added\n- Older.\n\n## [1.0.0]\n\nOnly prose.\n";

    #[test]
    fn section_highlights() {
        let n = notes_for(LOG, "1.2.0").expect("section");
        assert_eq!(n.date.as_deref(), Some("2026-01-02"));
        let heads: Vec<_> = n.groups.iter().map(|g| g.heading.as_str()).collect();
        assert_eq!(heads, ["Added", "Fixed"], "empty groups are dropped");
        assert_eq!(
            n.groups[0].items,
            [
                "Bold title",
                "Plain entry with `code. inside` first.",
                "See the guide for more - nested bullet",
                "Desktop: bold in the middle, wrapped.",
            ]
        );
        assert_eq!(n.groups[1].items, ["A fix."]);
        assert_eq!(notes_for(LOG, "1.1.0").unwrap().groups[0].items, ["Older."]);
    }

    #[test]
    fn missing_or_empty_section_is_none() {
        assert_eq!(notes_for(LOG, "9.9.9"), None);
        assert_eq!(notes_for(LOG, "1.0.0"), None, "prose only");
        assert_eq!(notes_for(LOG, "1.2"), None, "no prefix match on 1.2");
    }

    #[test]
    fn long_items_are_clipped() {
        let long = format!("## [2.0.0]\n### Added\n- {}\n", "é".repeat(500));
        let item = &notes_for(&long, "2.0.0").unwrap().groups[0].items[0];
        assert_eq!(item.chars().count(), MAX_ITEM_CHARS);
        assert!(item.ends_with('\u{2026}'));
    }

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

    #[test]
    fn version_order() {
        assert_eq!(newer("0.8.0", "0.7.9"), Some(true));
        assert_eq!(newer("0.10.0", "0.9.0"), Some(true));
        assert_eq!(newer("0.7.0", "0.8.0"), Some(false));
        assert_eq!(newer("v1.0.0-rc1", "0.9.0"), Some(true));
        assert_eq!(newer("0.8.0", ""), None);
        assert_eq!(newer("0.8", "0.7.0"), None);
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ot-whatsnew-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn seen(file: &Path) -> Option<String> {
        serde_json::from_slice::<Seen>(&std::fs::read(file).ok()?)
            .ok()?
            .last_seen_version
    }

    #[test]
    fn once_per_update_never_on_first_run() {
        let dir = tmp("once");
        let file = dir.join("cfg").join(FILE);
        assert_eq!(
            check_at(&file, "1.1.0", LOG, false),
            None,
            "fresh install: silent"
        );
        assert_eq!(
            seen(&file).as_deref(),
            Some("1.1.0"),
            "recorded on first run"
        );
        assert_eq!(check_at(&file, "1.1.0", LOG, false), None, "same version");
        assert_eq!(
            check_at(&file, "1.1.0", LOG, true),
            None,
            "the UI-state hint only matters while there is no file"
        );

        let w = check_at(&file, "1.2.0", LOG, false).expect("update shows");
        assert_eq!(
            (w.version.as_str(), w.previous.as_str()),
            ("1.2.0", "1.1.0")
        );
        assert_eq!(w.notes.unwrap().groups[0].items[0], "Bold title");
        assert!(
            check_at(&file, "1.2.0", LOG, false).is_some(),
            "until dismissed"
        );
        write_seen(&file, "1.2.0").unwrap();
        assert_eq!(check_at(&file, "1.2.0", LOG, false), None, "once");

        let w = check_at(&file, "1.3.0", LOG, false).expect("no section still shows");
        assert_eq!(w.notes, None);

        assert_eq!(
            check_at(&file, "1.0.0", LOG, false),
            None,
            "downgrade: silent"
        );
        assert_eq!(seen(&file).as_deref(), Some("1.0.0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_from_before_the_state_file() {
        let dir = tmp("prior");
        let file = dir.join(FILE);
        let w = check_at(&file, "1.2.0", LOG, true).expect("0.7.x user updating shows");
        assert_eq!(w.previous, "");
        assert!(w.notes.is_some());
        assert!(!file.exists(), "written on dismiss, not on show");
        write_seen(&file, "1.2.0").unwrap();
        assert_eq!(check_at(&file, "1.2.0", LOG, true), None, "once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn damaged_state_counts_as_an_update() {
        let dir = tmp("damaged");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(FILE);
        std::fs::write(&file, b"{not json").unwrap();
        let w = check_at(&file, "1.2.0", LOG, false).expect("shows");
        assert_eq!(w.previous, "");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
