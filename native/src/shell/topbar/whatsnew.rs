//! One-time "What's new" after an update (row 35), the native side of the
//! desktop's `whatsnew.rs`: the changelog parsing, the version comparison
//! and the seen-state check are [`opentune_hub::whatsnew`]'s; this module
//! embeds the repository's CHANGELOG.md (so the notes work offline and match
//! the build), finds the state file and draws the modal.
//!
//! * The state file is `<app data dir>/whatsnew.json`, the directory eframe
//!   keeps the settings in (`eframe::storage_dir(app id)`), outside the app
//!   bundle, so it survives an update replacing the bundle.
//! * No state file: a fresh install (nothing shown, the version recorded),
//!   unless the app's saved settings (`app.ron`) already exist: then it is an
//!   update from a build before this file, and the notes are shown.
//! * Same version: nothing. A newer stored version (a downgrade): recorded
//!   silently. An older one: the notes, once; closing records the version.
//!   When the changelog has no section for this version (the native app
//!   has its own version line), the latest OpenTune release's notes are
//!   shown, labelled as such.
//! * Scripted runs (`--screenshot`, `--bench`) never record anything: they
//!   show the notes only when a state file already asks for them.
//!
//! Nothing is fetched: the notes are compiled in.

use crate::theme::{self, font, Tokens, DISPLAY, MONO_MEDIUM, UI_STRONG};
use eframe::egui::{self, Margin, RichText, Ui};
use opentune_hub::whatsnew::{check_at, notes_for, write_seen, Notes, WhatsNew, FILE};
use std::path::{Path, PathBuf};

/// The repository changelog, compiled in.
pub const CHANGELOG: &str = include_str!("../../../../CHANGELOG.md");
/// The running app version (this crate's).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Highlights shown before "Show all".
const SHOW_FIRST: usize = 10;

/// The app's data directory (where eframe keeps `app.ron`).
pub fn data_dir(app_id: &str) -> Option<PathBuf> {
    eframe::storage_dir(app_id)
}

/// Is this a scripted run (screenshot / bench) that must not touch state?
pub fn scripted_run() -> bool {
    std::env::args().any(|a| a == "--screenshot" || a == "--bench")
}

/// The newest released section of `changelog` (not `[Unreleased]`).
fn latest_release(changelog: &str) -> Option<Notes> {
    changelog
        .lines()
        .filter_map(|l| l.strip_prefix("## ["))
        .filter_map(|l| l.split_once(']').map(|(v, _)| v))
        .filter(|v| !v.eq_ignore_ascii_case("unreleased"))
        .find_map(|v| notes_for(changelog, v))
}

/// The startup decision for state `file`: what to show, if anything.
/// `prior`: the app's saved settings exist (an earlier install). A scripted
/// run only reads (and only an existing file).
fn startup_check(file: &Path, prior: bool, scripted: bool) -> Option<WhatsNew> {
    if scripted && !file.is_file() {
        return None;
    }
    check_at(file, VERSION, CHANGELOG, prior && !scripted)
}

/// What the modal shows.
struct Shown {
    info: WhatsNew,
    /// Opened from the startup check (closing records the version).
    record: bool,
    /// No notes for this version: the latest release's, labelled as such.
    latest: Option<Notes>,
    all: bool,
}

#[derive(Default)]
pub struct WhatsNewUi {
    checked: bool,
    file: Option<PathBuf>,
    scripted: bool,
    shown: Option<Shown>,
}

impl WhatsNewUi {
    /// Once per run (the first frame): show this version's notes after an
    /// update.
    pub fn startup(&mut self, app_id: &str) {
        if self.checked {
            return;
        }
        self.checked = true;
        self.scripted = scripted_run();
        let Some(dir) = data_dir(app_id) else {
            return;
        };
        let file = dir.join(FILE);
        let prior = dir.join("app.ron").is_file();
        if let Some(info) = startup_check(&file, prior, self.scripted) {
            // The native line's own version may have no changelog section
            // yet: then the latest OpenTune release's, labelled as such.
            let latest = info
                .notes
                .is_none()
                .then(|| latest_release(CHANGELOG))
                .flatten();
            self.shown = Some(Shown {
                info,
                record: true,
                latest,
                all: false,
            });
        }
        self.file = Some(file);
    }

    /// Open from the menu: this version's notes (or the latest release's).
    pub fn open(&mut self) {
        let notes = notes_for(CHANGELOG, VERSION);
        let latest = notes.is_none().then(|| latest_release(CHANGELOG)).flatten();
        self.shown = Some(Shown {
            info: WhatsNew {
                version: VERSION.to_string(),
                previous: String::new(),
                notes,
            },
            record: false,
            latest,
            all: false,
        });
    }

    pub fn ui(&mut self, ctx: &egui::Context, t: &Tokens) {
        let Some(s) = &mut self.shown else {
            return;
        };
        let mut done = false;
        let r = egui::Modal::new(egui::Id::unique("whats-new"))
            .frame(theme::panel_frame(t).inner_margin(Margin::same(18)))
            .show(ctx, |ui| {
                ui.set_width(520.0);
                ui.label(
                    RichText::new(format!("What's new in v{}", s.info.version))
                        .font(font(DISPLAY, 17.0))
                        .color(t.fg),
                );
                ui.label(
                    RichText::new(subtitle(&s.info))
                        .size(12.0)
                        .color(t.fg_faint),
                );
                ui.add_space(8.0);
                let notes = s.info.notes.as_ref().or(s.latest.as_ref());
                egui::ScrollArea::vertical()
                    .max_height(380.0)
                    .show(ui, |ui| match notes {
                        Some(n) => {
                            if s.info.notes.is_none() {
                                ui.label(
                                    RichText::new(format!(
                                        "The changelog has no section for v{} yet. The latest \
                                         OpenTune release, v{}:",
                                        s.info.version, n.version
                                    ))
                                    .size(12.5)
                                    .color(t.fg_dim),
                                );
                            }
                            let limit = if s.all { usize::MAX } else { SHOW_FIRST };
                            notes_body(ui, t, n, limit);
                        }
                        None => {
                            ui.label(
                                RichText::new(format!("Updated to v{}.", s.info.version))
                                    .color(t.fg),
                            );
                        }
                    });
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::primary_button(ui, t, None, "Got it").clicked() {
                        done = true;
                    }
                    let total = notes.map_or(0, item_count);
                    if !s.all && total > SHOW_FIRST {
                        let more = format!("Show all ({} more)", total - SHOW_FIRST);
                        if theme::button(ui, t, None, &more).clicked() {
                            s.all = true;
                        }
                    }
                });
            });
        if done || r.should_close() {
            if s.record && !self.scripted {
                if let Some(f) = &self.file {
                    // Best effort: an unwritable file only means it shows again.
                    let _ = write_seen(f, VERSION);
                }
            }
            self.shown = None;
        }
    }
}

fn subtitle(w: &WhatsNew) -> String {
    let from = if w.previous.is_empty() {
        "Updated".to_string()
    } else {
        format!("Updated from v{}", w.previous)
    };
    match w.notes.as_ref().and_then(|n| n.date.as_deref()) {
        Some(d) => format!("{from} · released {d}"),
        None => from,
    }
}

fn item_count(n: &Notes) -> usize {
    n.groups.iter().map(|g| g.items.len()).sum()
}

/// The first `limit` highlights, grouped (`code` spans in mono).
fn notes_body(ui: &mut Ui, t: &Tokens, n: &Notes, limit: usize) {
    let mut left = limit;
    for g in &n.groups {
        if left == 0 {
            break;
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new(&g.heading)
                .font(font(UI_STRONG, 13.0))
                .color(t.accent),
        );
        for item in g.items.iter().take(left) {
            ui.horizontal_top(|ui| {
                ui.label(RichText::new("•").color(t.fg_faint));
                ui.add(egui::Label::new(inline_job(t, item)).wrap());
            });
        }
        left = left.saturating_sub(g.items.len());
    }
}

/// Text with `inline code` in the mono font.
fn inline_job(t: &Tokens, s: &str) -> egui::text::LayoutJob {
    use egui::text::{LayoutJob, TextFormat};
    let mut job = LayoutJob::default();
    for (i, part) in s.split('`').enumerate() {
        if part.is_empty() {
            continue;
        }
        let (font_id, color) = if i % 2 == 1 {
            (font(MONO_MEDIUM, 12.0), t.fg)
        } else {
            (egui::FontId::proportional(13.0), t.fg)
        };
        job.append(
            part,
            0.0,
            TextFormat {
                font_id,
                color,
                ..Default::default()
            },
        );
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ot-whatsnew-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn fresh_install_update_and_scripted_runs() {
        let d = tmp("flow");
        let f = d.join(FILE);
        // Fresh install: nothing shown, the version recorded.
        assert!(startup_check(&f, false, false).is_none());
        assert!(f.is_file());
        assert!(startup_check(&f, false, false).is_none(), "same version");
        // An update from an older version: shown until recorded.
        write_seen(&f, "0.0.1").unwrap();
        let w = startup_check(&f, false, false).expect("shown after an update");
        assert_eq!(
            (w.version.as_str(), w.previous.as_str()),
            (VERSION, "0.0.1")
        );
        write_seen(&f, VERSION).unwrap();
        assert!(startup_check(&f, false, false).is_none());
        // No state file but saved settings: an earlier install.
        std::fs::remove_file(&f).unwrap();
        assert!(startup_check(&f, true, false).is_some());
        assert!(!f.exists(), "not recorded until dismissed");
        // Scripted: never writes, never decides on its own.
        assert!(startup_check(&f, true, true).is_none());
        assert!(!f.exists());
        write_seen(&f, "0.0.1").unwrap();
        assert!(startup_check(&f, false, true).is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn changelog_is_bundled_and_has_a_release() {
        assert!(CHANGELOG.starts_with("# Changelog"));
        let n = latest_release(CHANGELOG).expect("a released section");
        assert!(item_count(&n) > 0);
        assert_ne!(n.version.to_ascii_lowercase(), "unreleased");
        let log = "## [Unreleased]\n### Added\n- New.\n\n## [1.2.0] - 2026-01-02\n\n### Fixed\n- A `fix`.\n";
        let n = latest_release(log).unwrap();
        assert_eq!(n.version, "1.2.0");
        assert_eq!(
            subtitle(&WhatsNew {
                version: "1.2.0".into(),
                previous: "1.1.0".into(),
                notes: Some(n),
            }),
            "Updated from v1.1.0 · released 2026-01-02"
        );
        assert_eq!(
            inline_job(&crate::theme::LIGHT, "a `b` c").sections.len(),
            3
        );
    }
}
