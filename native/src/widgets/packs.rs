//! The definition pack picker: choose the definition file an engine runs on
//! (coding, adaptation, service, logger, modules, controller) from a loaded
//! [definition pack](opentune_packs) instead of browsing for it. It shows
//! which pack (id, version) a definition came from and whether the pack
//! loaded cleanly. As the webview app's `pack-picker.js` + `packs.rs`.
//!
//! Packs are read in a background job (the bundled `packs/` folder, plus any
//! pack the user loads from disk); nothing here writes.

use crate::host::{DataRoot, Host, JobHandle};
use crate::icons;
use crate::theme::{self, font, MONO_MEDIUM};
use eframe::egui::{self, RichText, Ui};
use opentune_packs::{discover, load_pack, Kind, Pack};
use std::path::{Path, PathBuf};

pub use opentune_packs::Kind as PackKind;

/// One definition offered by a pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackDef {
    pub pack_id: String,
    pub pack_name: String,
    pub pack_version: String,
    /// The pack loaded without skipping any file.
    pub pack_valid: bool,
    pub kind: Kind,
    pub id: String,
    pub name: String,
    /// The definition file (what the engine opens).
    pub path: PathBuf,
}

/// The definitions of `kind` in `pack`.
pub fn defs_of(pack: &Pack, kind: Kind) -> Vec<PackDef> {
    let m = &pack.manifest;
    pack.entries
        .iter()
        .filter(|e| e.kind == kind)
        .map(|e| PackDef {
            pack_id: m.id.clone(),
            pack_name: m.name.clone(),
            pack_version: m.version.clone(),
            pack_valid: pack.report.is_clean(),
            kind,
            id: e.id.clone(),
            name: e.name.clone(),
            path: pack.root.join(&e.file),
        })
        .collect()
}

/// Every definition of `kind` in the packs under `root` (or in the pack at
/// `root`), with the packs that failed to open as `(dir, why)`.
pub fn scan(root: &Path, kind: Kind) -> (Vec<PackDef>, Vec<(PathBuf, String)>) {
    let (mut defs, mut failed) = (Vec::new(), Vec::new());
    for dir in discover(root) {
        match load_pack(&dir) {
            Ok(p) => defs.extend(defs_of(&p, kind)),
            Err(e) => failed.push((dir, e.to_string())),
        }
    }
    (defs, failed)
}

type Scan = (Vec<PackDef>, Vec<(PathBuf, String)>);

/// The picker. Call [`ui`](Self::ui) every frame; it returns the definition
/// the user picked that frame.
pub struct PackPicker {
    kind: Kind,
    job: Option<JobHandle<Scan>>,
    started: bool,
    defs: Vec<PackDef>,
    failed: Vec<(PathBuf, String)>,
    selected: Option<usize>,
}

impl PackPicker {
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            job: None,
            started: false,
            defs: Vec::new(),
            failed: Vec::new(),
            selected: None,
        }
    }

    pub fn selected(&self) -> Option<&PackDef> {
        self.selected.and_then(|i| self.defs.get(i))
    }

    fn load(&mut self, host: &Host<'_>, root: PathBuf) {
        let kind = self.kind;
        self.job = Some(host.spawn_job("reading packs", move |_p| Ok(scan(&root, kind))));
    }

    pub fn ui(&mut self, ui: &mut Ui, host: &Host<'_>) -> Option<PackDef> {
        let t = *host.tokens();
        if !self.started {
            self.started = true;
            if let Some(root) = host.data_dir(DataRoot::Packs) {
                self.load(host, root.to_path_buf());
            }
        }
        if let Some(r) = self.job.as_mut().and_then(JobHandle::take) {
            self.job = None;
            if let Ok((defs, failed)) = r {
                for d in defs {
                    if !self.defs.contains(&d) {
                        self.defs.push(d);
                    }
                }
                self.failed.extend(failed);
            }
        }
        let mut picked = None;
        ui.horizontal(|ui| {
            let current = self
                .selected()
                .map(|d| format!("{} ({} {})", d.name, d.pack_id, d.pack_version))
                .unwrap_or_else(|| format!("Choose a {} definition", self.kind.name()));
            egui::ComboBox::from_id_salt(("pack-picker", self.kind.dir()))
                .selected_text(current)
                .width(320.0)
                .show_ui(ui, |ui| {
                    if self.defs.is_empty() {
                        ui.label(RichText::new("No pack offers one").color(t.fg_faint));
                    }
                    for (i, d) in self.defs.iter().enumerate() {
                        let mut label = format!("{}  ·  {} {}", d.name, d.pack_id, d.pack_version);
                        if !d.pack_valid {
                            label.push_str("  (pack has skipped files)");
                        }
                        if ui
                            .selectable_label(self.selected == Some(i), label)
                            .on_hover_text(d.path.display().to_string())
                            .clicked()
                        {
                            self.selected = Some(i);
                            picked = Some(d.clone());
                        }
                    }
                });
            if theme::button(ui, &t, Some(icons::FOLDER_OPEN), "Load pack…").clicked() {
                if let Some(dir) = host.pick_dir("Load a definition pack") {
                    self.load(host, dir);
                }
            }
            if self.job.is_some() {
                ui.label(RichText::new("reading…").color(t.fg_faint));
            }
        });
        if let Some(d) = self.selected().filter(|d| !d.pack_valid) {
            ui.label(
                RichText::new(format!(
                    "{}  pack {} skipped some files when it loaded",
                    icons::WARNING,
                    d.pack_id
                ))
                .font(font(MONO_MEDIUM, 11.5))
                .color(t.warn),
            );
        }
        for (dir, why) in &self.failed {
            ui.label(
                RichText::new(format!("{}: {why}", dir.display()))
                    .font(font(MONO_MEDIUM, 11.5))
                    .color(t.fg_faint),
            );
        }
        picked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_packs_offer_definitions_by_kind() {
        let root = Path::new("../packs");
        if !root.is_dir() {
            return; // not a repository checkout
        }
        let (coding, failed) = scan(root, Kind::Coding);
        assert!(failed.is_empty(), "{failed:?}");
        assert!(!coding.is_empty());
        assert!(coding
            .iter()
            .all(|d| d.kind == Kind::Coding && d.path.is_file()));
        // The `_template` scaffold is never offered.
        assert!(coding
            .iter()
            .all(|d| !d.path.to_string_lossy().contains("_template")));
    }
}
