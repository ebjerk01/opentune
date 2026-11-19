//! Tuning projects (.otproj): a dump, its definition and notes, saved and
//! reopened together.
//!
//! The file format is the Tauri app's (`desktop/src-tauri/src/project.rs`),
//! implemented locally so `.otproj` files move freely between the two apps:
//! a small JSON side-file that references the base image by path plus a
//! fingerprint (size + 64-bit FNV-1a, hex) and records the definition,
//! checksum algorithm, notes, map edits and created / saved times (ISO 8601
//! UTC). It never holds a copy of the image.
//!
//! The native app has no map editor yet, so it does not create map edits;
//! edits (and any field it does not know) of a project made in the webview
//! app are kept verbatim when the project is saved again for the same image.

use crate::icons;
use crate::theme::{self, font, Tokens, Tone, UI_STRONG};
use crate::views::maps::{kv, two_pane};
use crate::views::vault::{self, callout, list_row, show_msg, Msg, ROW_H};
use eframe::egui::{self, Label, Margin, RichText, Sense, Ui, UiBuilder, Vec2};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// What the Project view asks the app to do.
pub enum ProjectAction {
    /// Open this dump with this definition id in the Flash view.
    Open {
        image: PathBuf,
        def_id: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// Format (pure, unit-tested; mirrors the Tauri app)
// ---------------------------------------------------------------------------

pub(crate) const PROJECT_FORMAT: &str = "opentune-project";
pub(crate) const PROJECT_VERSION: u32 = 1;
/// File extension of a project.
pub(crate) const PROJECT_EXT: &str = "otproj";
/// Recent projects kept in memory.
const RECENT_MAX: usize = 12;

/// 64-bit FNV-1a over the whole file: a fast change detector, not a
/// cryptographic hash (same as the Tauri app).
pub(crate) fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// `(size, 16-digit hex FNV-1a)` of a file.
pub(crate) fn file_fingerprint(path: &Path) -> std::io::Result<(u64, String)> {
    let data = std::fs::read(path)?;
    Ok((data.len() as u64, format!("{:016x}", fnv1a64(&data))))
}

/// Base-image reference: path + the fingerprint taken when it was saved.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct BaseImageRef {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub size: u64,
    /// Hex FNV-1a of the image at save time; empty when unknown.
    #[serde(default)]
    pub hash: String,
}

/// A project as stored on disk. Field names and order are the Tauri app's;
/// every field defaults so older or hand-written projects load. Map edits
/// and unknown fields are carried as raw JSON so nothing is lost.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Project {
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub ecudef_id: Option<String>,
    #[serde(default)]
    pub checksum_algo: Option<String>,
    #[serde(default)]
    pub base_image: BaseImageRef,
    /// One record per edited map (`kind`, `def_id`, `map_name`, `mapdef`,
    /// `cells`, `baseline`, `notes`), kept verbatim.
    #[serde(default)]
    pub edits: Vec<Value>,
    #[serde(default)]
    pub created: Option<String>,
    #[serde(default)]
    pub saved: Option<String>,
    /// Fields a newer app version wrote; written back unchanged.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// Whether the referenced image is still the one the project was saved for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ImageStatus {
    /// Present and the fingerprint matches.
    Match,
    /// Present but changed since the project was saved.
    Mismatch { size: u64, hash: String },
    /// The referenced path cannot be read.
    Missing,
    /// The project records no base image.
    NoImage,
}

/// Re-fingerprint the referenced image (the Tauri app's `project_open` rule:
/// hash compare, or a size check when no hash was stored).
pub(crate) fn check_image(p: &Project) -> ImageStatus {
    let path = p.base_image.path.trim();
    if path.is_empty() {
        return ImageStatus::NoImage;
    }
    match file_fingerprint(Path::new(path)) {
        Ok((size, hash)) => {
            let ok = if p.base_image.hash.is_empty() {
                p.base_image.size == 0 || p.base_image.size == size
            } else {
                hash == p.base_image.hash
            };
            if ok {
                ImageStatus::Match
            } else {
                ImageStatus::Mismatch { size, hash }
            }
        }
        Err(_) => ImageStatus::Missing,
    }
}

/// Parse a project file's text. A top-level JSON object is required and a
/// non-empty `format` must be OpenTune's.
pub(crate) fn parse_project(txt: &str) -> Result<Project, String> {
    let v: Value = serde_json::from_str(txt).map_err(|e| format!("not valid JSON: {e}"))?;
    if !v.is_object() {
        return Err("not an OpenTune project (expected a JSON object)".into());
    }
    let p: Project = serde_json::from_value(v).map_err(|e| format!("unexpected field: {e}"))?;
    if !p.format.is_empty() && p.format != PROJECT_FORMAT {
        return Err(format!("not an OpenTune project (format \"{}\")", p.format));
    }
    Ok(p)
}

pub(crate) fn load_project(path: &Path) -> Result<Project, String> {
    let txt = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_project(&txt).map_err(|e| format!("{}: {e}", path.display()))
}

/// Why a project may not be written to `dest` (`Ok` = allowed): it must be a
/// `.otproj` file and never the image itself, so a slip in the save dialog
/// cannot replace a dump.
pub(crate) fn check_save_target(dest: &Path, image: Option<&Path>) -> Result<(), String> {
    let ext_ok = dest
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(PROJECT_EXT));
    if !ext_ok {
        return Err(format!(
            "A project is saved as a .{PROJECT_EXT} file; {} was not written.",
            dest.display()
        ));
    }
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if image.is_some_and(|i| canon(i) == canon(dest)) {
        return Err("That is the dump itself; choose another name.".into());
    }
    Ok(())
}

/// Stamp the format, (re)fingerprint the base image when it can be read, and
/// write pretty JSON to `dest`. Returns whether the image was fingerprinted.
pub(crate) fn save_project(dest: &Path, p: &mut Project) -> Result<bool, String> {
    p.format = PROJECT_FORMAT.to_string();
    if p.version == 0 {
        p.version = PROJECT_VERSION;
    }
    let mut fingerprinted = false;
    let img = p.base_image.path.trim().to_string();
    if !img.is_empty() {
        if let Ok((size, hash)) = file_fingerprint(Path::new(&img)) {
            p.base_image.size = size;
            p.base_image.hash = hash;
            fingerprinted = true;
        }
    }
    let json = serde_json::to_string_pretty(p).map_err(|e| e.to_string())?;
    std::fs::write(dest, json).map_err(|e| format!("Could not write {}: {e}", dest.display()))?;
    Ok(fingerprinted)
}

/// ISO 8601 UTC with milliseconds, as the webview app writes
/// (`new Date().toISOString()`).
pub(crate) fn iso_utc(ms: u128) -> String {
    let (y, mo, d, h, mi, s) = vault::utc_parts((ms / 1000) as u64);
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:03}Z",
        ms % 1000
    )
}

fn now_iso() -> String {
    iso_utc(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
}

/// `2026-11-01T09:00:00.000Z` → `2026-11-01 09:00 UTC` (other text as is).
fn short_time(iso: &str) -> String {
    let b = iso.as_bytes();
    if iso.len() >= 16 && b.get(10) == Some(&b'T') && iso.is_char_boundary(16) {
        format!("{} {} UTC", &iso[..10], &iso[11..16])
    } else {
        iso.to_string()
    }
}

/// The project to write when saving `draft` from the current Flash state.
/// Map edits and the checksum choice only make sense for the image they were
/// recorded against: they are carried when the draft's base image is the
/// current dump, else dropped (the count is returned so the UI can say so).
pub(crate) fn build_for_save(
    draft: &Project,
    image: &Path,
    def_id: Option<&str>,
    now: String,
) -> (Project, usize) {
    let same = !draft.base_image.path.is_empty() && Path::new(&draft.base_image.path) == image;
    let mut p = draft.clone();
    let mut dropped = 0;
    if !same {
        dropped = p.edits.len();
        p.edits.clear();
        p.checksum_algo = None;
    }
    p.base_image = BaseImageRef {
        path: image.display().to_string(),
        ..Default::default()
    };
    p.ecudef_id = def_id.map(str::to_string);
    if p.name.trim().is_empty() {
        p.name = image
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    if p.created.is_none() {
        p.created = Some(now.clone());
    }
    p.saved = Some(now);
    (p, dropped)
}

/// A file-name-safe default: `[A-Za-z0-9_.-]`, others become `_`.
fn file_safe(s: &str) -> String {
    let s: String = s
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "_.-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "tune".into()
    } else {
        s
    }
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

struct Recent {
    path: PathBuf,
    name: String,
}

#[derive(Default)]
pub struct ProjectView {
    /// The project being viewed / edited (a new one until saved or opened).
    draft: Project,
    /// Where `draft` was loaded from / saved to.
    draft_path: Option<PathBuf>,
    /// Image check of the draft as of its last open / save.
    status: Option<ImageStatus>,
    recent: Vec<Recent>,
    msg: Option<Msg>,
}

enum Act {
    OpenDialog,
    OpenPath(PathBuf),
    New,
    Save,
    OpenInFlash,
    Recheck,
}

impl ProjectView {
    /// `image` / `def_id`: what the Flash view has open now (to save).
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        t: &Tokens,
        image: Option<&Path>,
        def_id: Option<&str>,
    ) -> Option<ProjectAction> {
        let (left, right) = two_pane(ui);
        let mut act = None;
        ui.scope_builder(UiBuilder::new().max_rect(left), |ui| {
            if let Some(a) = self.list_pane(ui, t) {
                act = Some(a);
            }
        });
        ui.scope_builder(UiBuilder::new().max_rect(right), |ui| {
            if let Some(a) = self.detail_pane(ui, t, image, def_id) {
                act = Some(a);
            }
        });
        self.apply(act?, image, def_id)
    }

    fn apply(
        &mut self,
        act: Act,
        image: Option<&Path>,
        def_id: Option<&str>,
    ) -> Option<ProjectAction> {
        match act {
            Act::OpenDialog => {
                let mut d = rfd::FileDialog::new()
                    .set_title("Open a tuning project")
                    .add_filter("OpenTune project", &[PROJECT_EXT]);
                if let Some(dir) = self.start_dir(image) {
                    d = d.set_directory(dir);
                }
                let p = d.pick_file()?;
                self.open_path(p)
            }
            Act::OpenPath(p) => self.open_path(p),
            Act::New => {
                *self = ProjectView {
                    recent: std::mem::take(&mut self.recent),
                    ..Default::default()
                };
                None
            }
            Act::Save => {
                let image = image?;
                self.save(image, def_id);
                None
            }
            Act::Recheck => {
                self.status = Some(check_image(&self.draft));
                None
            }
            Act::OpenInFlash => self.open_action(),
        }
    }

    fn start_dir(&self, image: Option<&Path>) -> Option<PathBuf> {
        self.draft_path
            .as_deref()
            .and_then(Path::parent)
            .or_else(|| image.and_then(Path::parent))
            .filter(|d| d.is_dir())
            .map(Path::to_path_buf)
    }

    fn open_action(&self) -> Option<ProjectAction> {
        let img = self.draft.base_image.path.trim();
        (!img.is_empty()).then(|| ProjectAction::Open {
            image: PathBuf::from(img),
            def_id: self.draft.ecudef_id.clone(),
        })
    }

    /// Load and check a project. A matching image opens straight away in the
    /// Flash view; otherwise the project is shown with a warning.
    fn open_path(&mut self, path: PathBuf) -> Option<ProjectAction> {
        let p = match load_project(&path) {
            Ok(p) => p,
            Err(e) => {
                self.msg = Some(Msg::Err(format!("Could not open the project. {e}")));
                return None;
            }
        };
        let status = check_image(&p);
        self.push_recent(&path, &p.name);
        self.draft = p;
        self.draft_path = Some(path);
        self.status = Some(status.clone());
        self.msg = None;
        match status {
            ImageStatus::Match => self.open_action(),
            _ => None,
        }
    }

    fn save(&mut self, image: &Path, def_id: Option<&str>) {
        let (mut p, dropped) = build_for_save(&self.draft, image, def_id, now_iso());
        let file_name = self
            .draft_path
            .as_deref()
            .and_then(Path::file_name)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("{}.{PROJECT_EXT}", file_safe(&p.name)));
        let mut d = rfd::FileDialog::new()
            .set_title("Save tuning project as")
            .set_file_name(file_name)
            .add_filter("OpenTune project", &[PROJECT_EXT]);
        if let Some(dir) = self.start_dir(Some(image)) {
            d = d.set_directory(dir);
        }
        // The dialog itself asks before replacing an existing file.
        let Some(mut dest) = d.save_file() else {
            return;
        };
        if dest.extension().is_none() {
            dest.set_extension(PROJECT_EXT);
            if dest.exists() {
                self.msg = Some(Msg::Warn(format!(
                    "{} already exists; save again and pick it in the dialog to replace it.",
                    dest.display()
                )));
                return;
            }
        }
        if let Err(e) = check_save_target(&dest, Some(image)) {
            self.msg = Some(Msg::Warn(e));
            return;
        }
        match save_project(&dest, &mut p) {
            Ok(fingerprinted) => {
                let mut s = format!("Saved {}.", dest.display());
                if !fingerprinted {
                    s.push_str(" The dump could not be read, so no fingerprint was recorded.");
                }
                if dropped > 0 {
                    s.push_str(&format!(
                        " {dropped} map edit(s) recorded against the previous image were not \
                         carried over (the file they came from is unchanged)."
                    ));
                }
                self.msg = Some(if dropped > 0 || !fingerprinted {
                    Msg::Warn(s)
                } else {
                    Msg::Ok(s)
                });
                self.push_recent(&dest, &p.name);
                self.status = Some(check_image(&p));
                self.draft = p;
                self.draft_path = Some(dest);
            }
            Err(e) => self.msg = Some(Msg::Err(e)),
        }
    }

    fn push_recent(&mut self, path: &Path, name: &str) {
        self.recent.retain(|r| r.path != path);
        self.recent.insert(
            0,
            Recent {
                path: path.to_path_buf(),
                name: name.to_string(),
            },
        );
        self.recent.truncate(RECENT_MAX);
    }

    fn list_pane(&mut self, ui: &mut Ui, t: &Tokens) -> Option<Act> {
        let mut act = None;
        theme::panel(
            ui,
            t,
            "Projects",
            |ui| {
                theme::pill(ui, t, &self.recent.len().to_string(), Tone::Muted, true);
            },
            |ui| {
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal_wrapped(|ui| {
                            if theme::primary_button(
                                ui,
                                t,
                                Some(icons::FOLDER_OPEN),
                                "Open project…",
                            )
                            .clicked()
                            {
                                act = Some(Act::OpenDialog);
                            }
                            if theme::button(ui, t, Some(icons::FILE_PLUS), "New")
                                .on_hover_text("Start a new project from the open dump")
                                .clicked()
                            {
                                act = Some(Act::New);
                            }
                        });
                    });
                theme::hline(ui, t.border_soft);
                egui::Frame::new()
                    .inner_margin(Margin {
                        left: 14,
                        right: 14,
                        top: 10,
                        bottom: 4,
                    })
                    .show(ui, |ui| theme::section_label(ui, t, "Recent"));
                if self.recent.is_empty() {
                    egui::Frame::new()
                        .inner_margin(Margin::symmetric(14, 6))
                        .show(ui, |ui| {
                            ui.add(
                                Label::new(
                                    RichText::new(
                                        "Projects you open or save in this session appear here.",
                                    )
                                    .color(t.fg_dim),
                                )
                                .wrap(),
                            );
                        });
                    return;
                }
                let current = self.draft_path.clone();
                egui::ScrollArea::vertical()
                    .id_salt("project-recent")
                    .auto_shrink([false, false])
                    .show_rows(ui, ROW_H, self.recent.len(), |ui, range| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for i in range {
                            let r = &self.recent[i];
                            let (rect, _) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), ROW_H),
                                Sense::hover(),
                            );
                            let file = r
                                .path
                                .file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let title = if r.name.is_empty() { &file } else { &r.name };
                            let resp = list_row(
                                ui,
                                t,
                                rect,
                                ui.make_persistent_id(("project-row", &r.path)),
                                title,
                                &r.path.display().to_string(),
                                current.as_ref() == Some(&r.path),
                            )
                            .on_hover_text("Click to open again");
                            if resp.clicked() {
                                act = Some(Act::OpenPath(r.path.clone()));
                            }
                        }
                    });
            },
        );
        act
    }

    fn detail_pane(
        &mut self,
        ui: &mut Ui,
        t: &Tokens,
        image: Option<&Path>,
        def_id: Option<&str>,
    ) -> Option<Act> {
        let mut act = None;
        let title = match (&self.draft_path, self.draft.name.trim()) {
            (_, n) if !n.is_empty() => n.to_string(),
            (Some(p), _) => p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            (None, _) => "New project".into(),
        };
        let can_open = !self.draft.base_image.path.trim().is_empty()
            && !matches!(self.status, Some(ImageStatus::Missing));
        let mismatch = matches!(self.status, Some(ImageStatus::Mismatch { .. }));
        let mut head = None;
        theme::panel(
            ui,
            t,
            &title,
            |ui| {
                let save = ui
                    .add_enabled_ui(image.is_some(), |ui| {
                        theme::primary_button(ui, t, Some(icons::FLOPPY_DISK), "Save project…")
                    })
                    .inner
                    .on_hover_text("Save the open dump, its definition and these notes")
                    .on_disabled_hover_text("Open a dump in Flash dump first");
                if save.clicked() {
                    head = Some(Act::Save);
                }
                if can_open {
                    let label = if mismatch {
                        "Open anyway"
                    } else {
                        "Open in Flash dump"
                    };
                    if theme::button(ui, t, Some(icons::ARROW_SQUARE_OUT), label)
                        .on_hover_text("Open the project's dump and definition in Flash dump")
                        .clicked()
                    {
                        head = Some(Act::OpenInFlash);
                    }
                }
            },
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("project-detail")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .inner_margin(Margin::symmetric(16, 14))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                if let Some(a) = self.body(ui, t, image, def_id) {
                                    act = Some(a);
                                }
                            });
                    });
            },
        );
        act.or(head)
    }

    fn body(
        &mut self,
        ui: &mut Ui,
        t: &Tokens,
        image: Option<&Path>,
        def_id: Option<&str>,
    ) -> Option<Act> {
        let mut act = None;
        if let Some(m) = &self.msg {
            show_msg(ui, t, m);
            ui.add_space(10.0);
        }
        if let Some(s) = &self.status {
            if status_callout(ui, t, s, &self.draft) {
                act = Some(Act::Recheck);
            }
            ui.add_space(14.0);
        }

        theme::section_label(ui, t, "Project");
        ui.add_space(4.0);
        egui::Grid::new("project-meta")
            .num_columns(2)
            .spacing(Vec2::new(18.0, 7.0))
            .show(ui, |ui| {
                ui.label(
                    RichText::new("NAME")
                        .font(font(UI_STRONG, 10.5))
                        .color(t.fg_faint),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft.name)
                        .hint_text("Named after the dump when empty")
                        .desired_width(ui.available_width().min(420.0)),
                );
                ui.end_row();
                let file = self
                    .draft_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "not saved yet".into());
                kv(ui, t, "File", &file);
                let d = self.draft.ecudef_id.as_deref().unwrap_or("–");
                kv(ui, t, "Definition", d);
                if let Some(c) = &self.draft.checksum_algo {
                    kv(ui, t, "Checksum", c);
                }
                let tm =
                    |s: &Option<String>| s.as_deref().map(short_time).unwrap_or_else(|| "–".into());
                kv(ui, t, "Created", &tm(&self.draft.created));
                kv(ui, t, "Saved", &tm(&self.draft.saved));
                if !self.draft.edits.is_empty() {
                    kv(
                        ui,
                        t,
                        "Map edits",
                        &format!(
                            "{} (made in the webview app; kept when saved for the same dump)",
                            self.draft.edits.len()
                        ),
                    );
                }
            });

        let bi = &self.draft.base_image;
        if !bi.path.is_empty() {
            ui.add_space(14.0);
            theme::section_label(ui, t, "Base image");
            ui.add_space(4.0);
            egui::Grid::new("project-image")
                .num_columns(2)
                .spacing(Vec2::new(18.0, 7.0))
                .show(ui, |ui| {
                    kv(ui, t, "Path", &bi.path);
                    kv(ui, t, "Recorded", &fingerprint_text(bi.size, &bi.hash));
                    if let Some(ImageStatus::Mismatch { size, hash }) = &self.status {
                        kv(ui, t, "Now", &fingerprint_text(*size, hash));
                    }
                });
        }

        ui.add_space(14.0);
        theme::section_label(ui, t, "Notes");
        ui.add_space(4.0);
        ui.add(
            egui::TextEdit::multiline(&mut self.draft.notes)
                .hint_text("What was changed, why, test-drive results…")
                .desired_rows(6)
                .desired_width(f32::INFINITY),
        );

        ui.add_space(14.0);
        theme::hline(ui, t.border_soft);
        ui.add_space(12.0);
        theme::section_label(ui, t, "Save records");
        ui.add_space(4.0);
        let Some(img) = image else {
            ui.label(
                RichText::new(format!(
                    "{}  Open a dump in Flash dump to save a project for it.",
                    icons::INFO
                ))
                .color(t.fg_dim),
            );
            return act;
        };
        egui::Grid::new("project-save")
            .num_columns(2)
            .spacing(Vec2::new(18.0, 7.0))
            .show(ui, |ui| {
                kv(ui, t, "Dump", &img.display().to_string());
                kv(ui, t, "Definition", def_id.unwrap_or("none selected"));
            });
        let same =
            !self.draft.base_image.path.is_empty() && Path::new(&self.draft.base_image.path) == img;
        if !same && !self.draft.edits.is_empty() {
            ui.add_space(10.0);
            callout(
                ui,
                t,
                icons::WARNING,
                t.warn,
                t.crit_soft,
                &format!(
                    "This project's {} map edit(s) were recorded against {}. Saving for the \
                     dump open now does not carry them over; the original project file keeps \
                     them.",
                    self.draft.edits.len(),
                    self.draft.base_image.path
                ),
            );
        }
        act
    }
}

/// `2.0 MiB · fnv 0123456789abcdef`.
fn fingerprint_text(size: u64, hash: &str) -> String {
    let mut s = if size > 0 {
        format!(
            "{} ({size} bytes)",
            crate::views::flash::size_label(size as usize)
        )
    } else {
        "size unknown".into()
    };
    if !hash.is_empty() {
        s.push_str(&format!(" · fnv {hash}"));
    }
    s
}

/// The image-check banner. Returns true when "Check again" was clicked.
fn status_callout(ui: &mut Ui, t: &Tokens, s: &ImageStatus, p: &Project) -> bool {
    let mut again = false;
    match s {
        ImageStatus::Match => callout(
            ui,
            t,
            icons::CHECK_CIRCLE,
            t.ok,
            t.ok_soft,
            "The referenced dump is present and unchanged since the project was saved.",
        ),
        ImageStatus::Mismatch { .. } => callout(
            ui,
            t,
            icons::WARNING,
            t.crit,
            t.crit_soft,
            &format!(
                "The dump at {} has changed since this project was saved (its fingerprint \
                 no longer matches). It may be a different or already-modified file: check \
                 it before you continue. \"Open anyway\" loads it as it is now.",
                p.base_image.path
            ),
        ),
        ImageStatus::Missing => callout(
            ui,
            t,
            icons::X_CIRCLE,
            t.crit,
            t.crit_soft,
            &format!(
                "The referenced dump cannot be read: {}. Move it back (or restore it from the \
                 vault) and check again.",
                p.base_image.path
            ),
        ),
        ImageStatus::NoImage => callout(
            ui,
            t,
            icons::INFO,
            t.warn,
            t.crit_soft,
            "This project records no dump. Open one in Flash dump and save the project to \
             attach it.",
        ),
    }
    if matches!(s, ImageStatus::Mismatch { .. } | ImageStatus::Missing) {
        ui.add_space(6.0);
        if theme::button(ui, t, Some(icons::ARROWS_CLOCKWISE), "Check again").clicked() {
            again = true;
        }
    }
    again
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::vault::TmpDir;

    /// A project exactly as the Tauri app writes it (`serde_json::to_string_pretty`
    /// of its `Project`), plus a field from some future version.
    fn tauri_json(img: &str, size: u64, hash: &str) -> String {
        format!(
            r#"{{
  "format": "opentune-project",
  "version": 1,
  "name": "demo tune",
  "notes": "stage 1",
  "ecudef_id": "edc16-demo",
  "checksum_algo": "edc16",
  "base_image": {{
    "path": {img},
    "size": {size},
    "hash": "{hash}"
  }},
  "edits": [
    {{
      "kind": "def",
      "def_id": "edc16-demo",
      "map_name": "injection limiter",
      "mapdef": null,
      "cells": [[10.0, 12.5], [14.0, 16.5]],
      "baseline": [[10.0, 10.0], [10.0, 10.0]],
      "notes": "+25% top end"
    }}
  ],
  "created": "2026-11-01T09:00:00.000Z",
  "saved": "2026-11-01T09:30:00.000Z",
  "future_field": {{"x": 1}}
}}"#,
            img = serde_json::to_string(img).unwrap()
        )
    }

    #[test]
    fn fnv_matches_the_tauri_app() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        // Published FNV-1a 64 test vector.
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_ne!(fnv1a64(b"abc"), fnv1a64(b"abd"));
    }

    #[test]
    fn reads_a_tauri_project_and_writes_it_back_losslessly() {
        let d = TmpDir::new("proj-compat");
        let img = d.0.join("stock.bin");
        std::fs::write(&img, vec![0xAAu8; 2048]).unwrap();
        let (size, hash) = file_fingerprint(&img).unwrap();
        let src = tauri_json(&img.to_string_lossy(), size, &hash);
        let p = parse_project(&src).unwrap();
        assert_eq!(p.name, "demo tune");
        assert_eq!(p.ecudef_id.as_deref(), Some("edc16-demo"));
        assert_eq!(p.checksum_algo.as_deref(), Some("edc16"));
        assert_eq!(p.edits.len(), 1);
        assert_eq!(p.created.as_deref(), Some("2026-11-01T09:00:00.000Z"));
        assert_eq!(check_image(&p), ImageStatus::Match);

        // Saved again by the native app for the same dump: edits, checksum
        // and unknown fields survive; the fingerprint is refreshed.
        let (mut out, dropped) = build_for_save(&p, &img, Some("edc16-demo"), iso_utc(0));
        assert_eq!(dropped, 0);
        let dest = d.0.join("again.otproj");
        assert!(save_project(&dest, &mut out).unwrap());
        let before: Value = serde_json::from_str(&src).unwrap();
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        for k in [
            "format",
            "version",
            "name",
            "notes",
            "ecudef_id",
            "checksum_algo",
            "base_image",
            "edits",
            "created",
            "future_field",
        ] {
            assert_eq!(before[k], after[k], "field {k}");
        }
        assert_eq!(after["saved"], "1970-01-01T00:00:00.000Z");
        // Exactly the Tauri field set (plus the carried unknown field).
        let mut keys: Vec<&str> = after
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "base_image",
                "checksum_algo",
                "created",
                "ecudef_id",
                "edits",
                "format",
                "future_field",
                "name",
                "notes",
                "saved",
                "version"
            ]
        );
    }

    #[test]
    fn new_project_has_the_tauri_shape() {
        let d = TmpDir::new("proj-new");
        let img = d.0.join("my dump.bin");
        std::fs::write(&img, [1u8, 2, 3]).unwrap();
        let (mut p, _) =
            build_for_save(&Project::default(), &img, None, iso_utc(1_700_000_000_123));
        assert_eq!(p.name, "my dump");
        let dest = d.0.join("t.otproj");
        save_project(&dest, &mut p).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        assert_eq!(v["format"], PROJECT_FORMAT);
        assert_eq!(v["version"], PROJECT_VERSION);
        assert_eq!(v["ecudef_id"], Value::Null);
        assert_eq!(v["checksum_algo"], Value::Null);
        assert_eq!(v["edits"], serde_json::json!([]));
        assert_eq!(v["base_image"]["size"], 3);
        assert_eq!(v["base_image"]["hash"].as_str().unwrap().len(), 16);
        assert_eq!(v["created"], "2023-11-14T22:13:20.123Z");
        assert_eq!(v["created"], v["saved"]);
        // Round trip.
        assert_eq!(load_project(&dest).unwrap(), p);
    }

    #[test]
    fn mismatch_missing_and_no_image() {
        let d = TmpDir::new("proj-status");
        let img = d.0.join("stock.bin");
        std::fs::write(&img, vec![0u8; 1024]).unwrap();
        let (mut p, _) = build_for_save(&Project::default(), &img, Some("x"), iso_utc(0));
        save_project(&d.0.join("a.otproj"), &mut p).unwrap();
        assert_eq!(check_image(&p), ImageStatus::Match);

        // Same size, different content.
        std::fs::write(&img, vec![0xFFu8; 1024]).unwrap();
        match check_image(&p) {
            ImageStatus::Mismatch { size, hash } => {
                assert_eq!(size, 1024);
                assert_ne!(hash, p.base_image.hash);
            }
            other => panic!("{other:?}"),
        }
        // No stored hash: falls back to a size check (Tauri rule).
        let mut q = p.clone();
        q.base_image.hash.clear();
        assert_eq!(check_image(&q), ImageStatus::Match);
        q.base_image.size = 4;
        assert!(matches!(check_image(&q), ImageStatus::Mismatch { .. }));

        std::fs::remove_file(&img).unwrap();
        assert_eq!(check_image(&p), ImageStatus::Missing);
        assert_eq!(check_image(&Project::default()), ImageStatus::NoImage);
    }

    #[test]
    fn edits_are_not_carried_to_another_image() {
        let d = TmpDir::new("proj-other");
        let a = d.0.join("a.bin");
        let b = d.0.join("b.bin");
        let src = tauri_json(&a.to_string_lossy(), 0, "");
        let p = parse_project(&src).unwrap();
        let (out, dropped) = build_for_save(&p, &b, None, iso_utc(0));
        assert_eq!(dropped, 1);
        assert!(out.edits.is_empty());
        assert_eq!(out.checksum_algo, None);
        assert_eq!(out.base_image.path, b.display().to_string());
        assert_eq!(out.notes, "stage 1", "notes stay");
        assert_eq!(out.created, p.created, "created stays");
    }

    #[test]
    fn rejects_non_projects() {
        assert!(parse_project("this is not json").is_err());
        assert!(parse_project("[]").is_err());
        assert!(parse_project(r#"{"format":"something-else"}"#).is_err());
        // A minimal / older hand-written project still loads.
        let p = parse_project(r#"{"name":"x"}"#).unwrap();
        assert_eq!(p.name, "x");
        assert_eq!(check_image(&p), ImageStatus::NoImage);
    }

    #[test]
    fn save_target_rules() {
        let d = TmpDir::new("proj-target");
        let img = d.0.join("stock.bin");
        std::fs::write(&img, [0u8; 4]).unwrap();
        assert!(
            check_save_target(&img, Some(&img)).is_err(),
            "never the dump"
        );
        assert!(check_save_target(&d.0.join("x.bin"), Some(&img)).is_err());
        assert!(check_save_target(&d.0.join("x.OTPROJ"), Some(&img)).is_ok());
        assert!(check_save_target(&d.0.join("x.otproj"), None).is_ok());
    }

    #[test]
    fn time_helpers() {
        assert_eq!(iso_utc(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(
            short_time("2026-11-01T09:00:00.000Z"),
            "2026-11-01 09:00 UTC"
        );
        assert_eq!(short_time("yesterday"), "yesterday");
        assert_eq!(file_safe(" my tune/v2 "), "my_tune_v2");
        assert_eq!(file_safe(""), "tune");
    }
}
