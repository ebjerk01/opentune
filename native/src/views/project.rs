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
//! Map edits use the Tauri `MapEdit` record (`kind`, `def_id`, `map_name`,
//! `mapdef`, `cells` = the edited physical grid, `baseline` = the file's
//! grid, `notes`). Saving captures every map of the selected definition whose
//! bytes differ from the file on disk ([`capture_edits`]); records of other
//! maps (and unknown fields) are kept verbatim. Maps of a session definition
//! (an imported XDF / A2L, a promoted region) are stored as `adhoc` records
//! with their `mapdef`, as the webview app does, so they reopen without it.
//!
//! Applying a project ([`apply_project_edits`]) lays the recorded cells onto
//! the open dump through `Image::edit_maps`: every cell is checked like a
//! typed one (refused, never clamped) and the whole project is ONE undo step,
//! ALL OR NOTHING: if any record cannot be resolved or any cell is refused,
//! nothing changes and every problem is listed (a partly applied tune could
//! leave related maps inconsistent). Only cells whose recorded value differs
//! from the dump are written, so a stock value outside a definition's limits
//! does not block a project that did not touch it.
//!
//! The Flash view owns the dump and the Project view the project. They meet
//! through two small hand-offs in the egui context's temporary data (no app
//! state, nothing global): the Flash view publishes an [`EditSnapshot`] of
//! its unsaved edits whenever they change, and the Project view posts an
//! [`ApplyRequest`] that the Flash view carries out once the project's dump is
//! the one open there (and has no unsaved edits of its own).

use crate::icons;
use crate::image::{fmt_val, shape, CellEdit, Image};
use crate::theme::{self, font, Tokens, Tone, UI_STRONG};
use crate::views::maps::{kv, two_pane};
use crate::views::vault::{self, callout, list_row, show_msg, Msg, ROW_H};
use eframe::egui::{self, Label, Margin, RichText, Sense, Ui, UiBuilder, Vec2};
use opentune_core::ecudef::{EcuDef, MapDef};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
// Map edits: capture and apply (pure, unit-tested)
// ---------------------------------------------------------------------------

/// Id prefix of a definition that exists only in this session (an imported
/// XDF / A2L or promoted maps); its maps are recorded as `adhoc` edits.
pub(crate) const SESSION_DEF_PREFIX: &str = "session-";

/// One edited map, the Tauri app's `MapEdit` record.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct MapEdit {
    /// `def` (a map of a definition) or `adhoc` (`mapdef` carries the map).
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub def_id: Option<String>,
    #[serde(default)]
    pub map_name: String,
    #[serde(default)]
    pub mapdef: Option<MapDef>,
    /// The edited physical values, row by row.
    #[serde(default)]
    pub cells: Vec<Vec<f64>>,
    /// The file's values when the edit was captured.
    #[serde(default)]
    pub baseline: Vec<Vec<f64>>,
    #[serde(default)]
    pub notes: String,
}

impl MapEdit {
    fn adhoc(&self) -> bool {
        self.kind == "adhoc" || (self.kind.is_empty() && self.mapdef.is_some())
    }

    /// Identity of the edited map (the webview's `editKey`): a record with
    /// the same key replaces the earlier one.
    pub fn key(&self) -> String {
        if self.adhoc() {
            let addr = self
                .mapdef
                .as_ref()
                .map(|m| m.address.to_string())
                .unwrap_or_default();
            format!("adhoc:{}:{addr}", self.map_name)
        } else {
            format!(
                "def:{}:{}",
                self.def_id.as_deref().unwrap_or("null"),
                self.map_name
            )
        }
    }

    /// Cells that differ from the baseline (the webview's `cellsChanged`).
    pub fn changed_cells(&self) -> usize {
        self.cells
            .iter()
            .enumerate()
            .map(|(r, row)| {
                row.iter()
                    .enumerate()
                    .filter(|(c, v)| {
                        let b = self.baseline.get(r).and_then(|b| b.get(*c));
                        b.is_some_and(|b| (*v - b).abs() > 1e-9)
                    })
                    .count()
            })
            .sum()
    }

    /// `rows × cols` of the recorded grid.
    pub fn dims(&self) -> (usize, usize) {
        (self.cells.len(), self.cells.first().map_or(0, Vec::len))
    }
}

/// A stored record as a [`MapEdit`], or `None` when it is not one.
pub(crate) fn parse_edit(v: &Value) -> Option<MapEdit> {
    v.is_object()
        .then(|| serde_json::from_value(v.clone()).ok())
        .flatten()
}

/// Every map of `def` whose bytes in the working image differ from the file
/// on disk, as edit records (edited grid + the file's grid).
pub(crate) fn capture_edits(img: &Image, def: &EcuDef) -> Vec<MapEdit> {
    let adhoc = def.id.starts_with(SESSION_DEF_PREFIX);
    let grid = |b: &[u8], m: &MapDef| opentune_hub::maps::decode_cells(b, m);
    def.maps
        .iter()
        .filter(|m| img.map_changed(m))
        .filter_map(|m| {
            Some(MapEdit {
                kind: if adhoc { "adhoc" } else { "def" }.into(),
                def_id: (!adhoc).then(|| def.id.clone()),
                map_name: m.name.clone(),
                mapdef: adhoc.then(|| m.clone()),
                cells: grid(&img.bytes, m)?,
                baseline: grid(img.saved_bytes(), m)?,
                notes: String::new(),
            })
        })
        .collect()
}

/// The project's records with `captured` folded in: a captured map replaces
/// the record with the same key (keeping its notes), new ones are appended,
/// every other record (and anything that is not a map edit) stays verbatim.
pub(crate) fn merge_edits(existing: &[Value], captured: &[MapEdit]) -> Vec<Value> {
    let mut out: Vec<Value> = existing.to_vec();
    for e in captured {
        let mut e = e.clone();
        let at = out
            .iter()
            .position(|v| parse_edit(v).is_some_and(|x| x.key() == e.key()));
        match at {
            Some(i) => {
                if let Some(old) = parse_edit(&out[i]) {
                    e.notes = old.notes;
                }
                out[i] = serde_json::to_value(&e).unwrap_or(Value::Null);
            }
            None => out.push(serde_json::to_value(&e).unwrap_or(Value::Null)),
        }
    }
    out
}

/// The map a record edits: its own `mapdef` (adhoc), else the named map of
/// the named definition.
fn resolve<'a>(e: &'a MapEdit, defs: &'a [EcuDef]) -> Result<&'a MapDef, String> {
    if e.adhoc() {
        return e
            .mapdef
            .as_ref()
            .ok_or_else(|| "an ad-hoc record without its map definition".to_string());
    }
    let id = e.def_id.as_deref().unwrap_or("");
    let d = defs
        .iter()
        .find(|d| d.id == id)
        .ok_or_else(|| format!("definition '{id}' is not loaded"))?;
    d.maps
        .iter()
        .find(|m| m.name == e.map_name)
        .ok_or_else(|| format!("'{id}' has no map named '{}'", e.map_name))
}

/// Lay a project's map edits onto `img`: all or nothing, one undo step.
/// Returns `(map name, cells changed)` per record, or every problem (a record
/// that cannot be resolved, a size mismatch, each refused cell); then the
/// image is unchanged.
pub(crate) fn apply_project_edits(
    img: &mut Image,
    defs: &[EcuDef],
    edits: &[Value],
) -> Result<Vec<(String, usize)>, Vec<String>> {
    let mut problems = Vec::new();
    let mut plan: Vec<(MapDef, Vec<(usize, CellEdit)>)> = Vec::new();
    for (k, v) in edits.iter().enumerate() {
        let Some(e) = parse_edit(v) else {
            problems.push(format!("record {}: not a map edit", k + 1));
            continue;
        };
        let m = match resolve(&e, defs) {
            Ok(m) => m,
            Err(why) => {
                problems.push(format!("'{}': {why}", e.map_name));
                continue;
            }
        };
        let (rows, cols) = shape(m);
        if e.dims() != (rows, cols) || e.cells.iter().any(|r| r.len() != cols) {
            let (r, c) = e.dims();
            problems.push(format!(
                "'{}': the record is {c} × {r} but the map is {cols} × {rows}",
                e.map_name
            ));
            continue;
        }
        let Some(now) = crate::image::map_values(&img.bytes, m) else {
            problems.push(format!(
                "'{}' runs past the end of the open dump",
                e.map_name
            ));
            continue;
        };
        // Only cells the record changes: an untouched stock value outside the
        // definition's limits must not block the project.
        let cells = e
            .cells
            .iter()
            .flatten()
            .enumerate()
            .filter(|(i, v)| (**v - now[*i]).abs() > 1e-9 * now[*i].abs().max(1.0))
            .map(|(i, v)| (i, CellEdit::Set(*v)))
            .collect();
        plan.push((m.clone(), cells));
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let ops: Vec<(&MapDef, &[(usize, CellEdit)])> =
        plan.iter().map(|(m, c)| (m, c.as_slice())).collect();
    match img.edit_maps(&ops) {
        Ok(n) => Ok(plan.iter().map(|(m, _)| m.name.clone()).zip(n).collect()),
        Err(refused) => Err(refused
            .into_iter()
            .map(|(k, r)| {
                let m = &plan[k].0;
                let cols = shape(m).1;
                format!(
                    "'{}' row {} col {}: {}",
                    m.name,
                    r.idx / cols,
                    r.idx % cols,
                    r.reason
                )
            })
            .collect()),
    }
}

/// What the Flash view last published about its unsaved edits.
#[derive(Debug, Clone)]
pub(crate) struct EditSnapshot {
    pub image: PathBuf,
    pub algo: Option<String>,
    pub edits: Vec<MapEdit>,
}

/// A project's edits to lay onto `image` once it is open in the Flash view.
#[derive(Debug, Clone)]
pub(crate) struct ApplyRequest {
    pub image: PathBuf,
    pub project: String,
    pub edits: Vec<Value>,
}

fn snapshot_id() -> egui::Id {
    egui::Id::unique("opentune-native/project/edit-snapshot")
}

fn apply_id() -> egui::Id {
    egui::Id::unique("opentune-native/project/apply-request")
}

/// Flash view → Project view: the current unsaved edits.
pub(crate) fn publish_snapshot(ctx: &egui::Context, s: EditSnapshot) {
    ctx.data_mut(|d| d.insert_temp(snapshot_id(), Arc::new(s)));
}

pub(crate) fn snapshot(ctx: &egui::Context) -> Option<Arc<EditSnapshot>> {
    ctx.data(|d| d.get_temp::<Arc<EditSnapshot>>(snapshot_id()))
}

/// Project view → Flash view: apply these edits (replaces an earlier request).
pub(crate) fn request_apply(ctx: &egui::Context, r: ApplyRequest) {
    ctx.data_mut(|d| d.insert_temp(apply_id(), Some(Arc::new(r))));
}

/// The waiting request, if any (not removed).
pub(crate) fn pending_apply(ctx: &egui::Context) -> Option<Arc<ApplyRequest>> {
    ctx.data(|d| d.get_temp::<Option<Arc<ApplyRequest>>>(apply_id()))
        .flatten()
}

/// Remove and return the waiting request.
pub(crate) fn take_apply(ctx: &egui::Context) -> Option<Arc<ApplyRequest>> {
    ctx.data_mut(|d| d.remove_temp::<Option<Arc<ApplyRequest>>>(apply_id()))
        .flatten()
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
        let ctx = ui.ctx().clone();
        self.apply(act?, image, def_id, &ctx)
    }

    fn apply(
        &mut self,
        act: Act,
        image: Option<&Path>,
        def_id: Option<&str>,
        ctx: &egui::Context,
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
                self.open_path(p, ctx)
            }
            Act::OpenPath(p) => self.open_path(p, ctx),
            Act::New => {
                *self = ProjectView {
                    recent: std::mem::take(&mut self.recent),
                    ..Default::default()
                };
                None
            }
            Act::Save => {
                let image = image?;
                let snap = snapshot(ctx).filter(|s| s.image == image);
                self.save(image, def_id, snap.as_deref());
                None
            }
            Act::Recheck => {
                self.status = Some(check_image(&self.draft));
                None
            }
            Act::OpenInFlash => self.open_action(ctx),
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

    /// Open the project's dump (and definition) in the Flash view; its map
    /// edits follow as an [`ApplyRequest`] the Flash view carries out there.
    fn open_action(&self, ctx: &egui::Context) -> Option<ProjectAction> {
        let img = self.draft.base_image.path.trim();
        if img.is_empty() {
            return None;
        }
        if !self.draft.edits.is_empty() {
            request_apply(
                ctx,
                ApplyRequest {
                    image: PathBuf::from(img),
                    project: self.draft.name.clone(),
                    edits: self.draft.edits.clone(),
                },
            );
        }
        Some(ProjectAction::Open {
            image: PathBuf::from(img),
            def_id: self.draft.ecudef_id.clone(),
        })
    }

    /// Load and check a project. A matching image opens straight away in the
    /// Flash view; otherwise the project is shown with a warning.
    fn open_path(&mut self, path: PathBuf, ctx: &egui::Context) -> Option<ProjectAction> {
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
            ImageStatus::Match => self.open_action(ctx),
            _ => None,
        }
    }

    /// `snap`: the Flash view's unsaved edits of this dump (captured into the
    /// project).
    fn save(&mut self, image: &Path, def_id: Option<&str>, snap: Option<&EditSnapshot>) {
        let (mut p, dropped) = build_for_save(&self.draft, image, def_id, now_iso());
        let captured = snap.map_or(0, |s| s.edits.len());
        if let Some(s) = snap {
            p.edits = merge_edits(&p.edits, &s.edits);
            if s.algo.is_some() {
                p.checksum_algo.clone_from(&s.algo);
            }
        }
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
                if captured > 0 {
                    s.push_str(&format!(
                        " {captured} edited map(s) captured from Flash dump."
                    ));
                }
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
        let n = self.draft.edits.len();
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
                    let label = match (mismatch, n) {
                        (true, 0) => "Open anyway".to_string(),
                        (true, n) => format!("Open anyway & apply {n} edit(s)"),
                        (false, 0) => "Open in Flash dump".to_string(),
                        (false, n) => format!("Open & apply {n} edit(s)"),
                    };
                    let label = label.as_str();
                    if theme::button(ui, t, Some(icons::ARROW_SQUARE_OUT), label)
                        .on_hover_text(
                            "Open the project's dump and definition in Flash dump; its map \
                             edits are applied there as one undo step (all or nothing)",
                        )
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
                kv(ui, t, "Map edits", &self.draft.edits.len().to_string());
            });
        if !self.draft.edits.is_empty() {
            ui.add_space(14.0);
            theme::section_label(ui, t, "Map edits");
            ui.add_space(4.0);
            edits_table(ui, t, &mut self.draft.edits);
        }

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
        let snap = snapshot(ui.ctx()).filter(|s| s.image == img);
        egui::Grid::new("project-save")
            .num_columns(2)
            .spacing(Vec2::new(18.0, 7.0))
            .show(ui, |ui| {
                kv(ui, t, "Dump", &img.display().to_string());
                kv(ui, t, "Definition", def_id.unwrap_or("none selected"));
                let edited = match &snap {
                    Some(s) if !s.edits.is_empty() => s
                        .edits
                        .iter()
                        .map(|e| format!("{} ({} cells)", e.map_name, e.changed_cells()))
                        .collect::<Vec<_>>()
                        .join(", "),
                    _ => "none".into(),
                };
                kv(ui, t, "Unsaved map edits", &edited);
            });
        if let Some(r) = pending_apply(ui.ctx()) {
            ui.add_space(8.0);
            callout(
                ui,
                t,
                icons::INFO,
                t.accent,
                t.accent_soft,
                &format!(
                    "{} map edit(s) of {} are waiting to be applied in Flash dump.",
                    r.edits.len(),
                    if r.project.is_empty() {
                        "the project"
                    } else {
                        &r.project
                    }
                ),
            );
        }
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

/// The project's map edits: map, size, source, changed cells, notes, remove.
/// Notes are written back into the stored record (other fields untouched).
fn edits_table(ui: &mut Ui, t: &Tokens, edits: &mut Vec<Value>) {
    let mut remove = None;
    egui::Grid::new("project-edits")
        .striped(true)
        .num_columns(6)
        .spacing(Vec2::new(14.0, 6.0))
        .show(ui, |ui| {
            for h in ["Map", "Size", "From", "Changed", "Notes", ""] {
                ui.label(
                    RichText::new(h.to_uppercase())
                        .font(font(UI_STRONG, 10.5))
                        .color(t.fg_faint),
                );
            }
            ui.end_row();
            for (i, v) in edits.iter_mut().enumerate() {
                let Some(e) = parse_edit(v) else {
                    ui.label(RichText::new("(not a map edit)").color(t.fg_dim));
                    for _ in 0..4 {
                        ui.label("");
                    }
                    if ui.small_button(icons::X).on_hover_text("Remove").clicked() {
                        remove = Some(i);
                    }
                    ui.end_row();
                    continue;
                };
                ui.label(RichText::new(&e.map_name).color(t.fg));
                let (r, c) = e.dims();
                ui.label(RichText::new(format!("{c} × {r}")).color(t.fg_dim));
                let from = if e.kind == "adhoc" {
                    "ad-hoc".to_string()
                } else {
                    e.def_id.clone().unwrap_or_else(|| "–".into())
                };
                ui.label(RichText::new(from).color(t.fg_dim));
                let n = e.changed_cells();
                let max_delta = e
                    .cells
                    .iter()
                    .flatten()
                    .zip(e.baseline.iter().flatten())
                    .map(|(a, b)| (a - b).abs())
                    .fold(0.0, f64::max);
                theme::pill(
                    ui,
                    t,
                    &n.to_string(),
                    if n > 0 { Tone::Warn } else { Tone::Muted },
                    true,
                )
                .on_hover_text(format!("largest change {}", fmt_val(max_delta)));
                let mut notes = e.notes.clone();
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut notes)
                            .hint_text("notes")
                            .desired_width(160.0),
                    )
                    .changed()
                {
                    if let Some(o) = v.as_object_mut() {
                        o.insert("notes".into(), Value::String(notes));
                    }
                }
                if ui
                    .small_button(icons::X)
                    .on_hover_text("Remove this edit from the project")
                    .clicked()
                {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
    if let Some(i) = remove {
        edits.remove(i);
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

    // -------------------------------------------------------------------
    // Map edits: capture, merge, apply
    // -------------------------------------------------------------------

    fn edit_def() -> EcuDef {
        use opentune_core::ecudef::{DataType, Endian, Scaling};
        let mut d: EcuDef = serde_json::from_value(serde_json::json!({
            "id": "demo", "name": "Demo", "maker": "Bosch", "family": "EDC16",
            "protocol": "iso_tp", "diag_layer": "uds", "bitrate": 500000
        }))
        .unwrap();
        let m = |name: &str, address| MapDef {
            name: name.into(),
            address,
            rows: 2,
            cols: 2,
            data_type: DataType::U8,
            endian: Endian::Big,
            scaling: Scaling {
                factor: 0.5,
                offset: 0.0,
            },
            max: Some(100.0),
            ..Default::default()
        };
        d.maps = vec![m("injection limiter", 0), m("boost", 8)];
        d
    }

    fn img() -> Image {
        Image::from_bytes(PathBuf::from("stock.bin"), vec![20u8; 16])
    }

    #[test]
    fn tauri_records_parse_and_keep_their_key() {
        let p = parse_project(&tauri_json("/x.bin", 0, "")).unwrap();
        let e = parse_edit(&p.edits[0]).unwrap();
        assert_eq!(e.key(), "def:edc16-demo:injection limiter");
        assert_eq!(e.dims(), (2, 2));
        assert_eq!(e.changed_cells(), 3);
        let adhoc = MapEdit {
            kind: "adhoc".into(),
            map_name: "x".into(),
            mapdef: Some(MapDef {
                address: 16,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(adhoc.key(), "adhoc:x:16");
        assert!(parse_edit(&Value::String("no".into())).is_none());
    }

    #[test]
    fn capture_then_merge_keeps_other_records_and_notes() {
        let d = edit_def();
        let mut im = img();
        assert!(capture_edits(&im, &d).is_empty());
        im.edit_cells(
            &d.maps[1],
            &[(0, CellEdit::Set(15.0)), (3, CellEdit::Set(20.0))],
        )
        .unwrap();
        let cap = capture_edits(&im, &d);
        assert_eq!(cap.len(), 1);
        let e = &cap[0];
        assert_eq!(
            (e.kind.as_str(), e.def_id.as_deref()),
            ("def", Some("demo"))
        );
        assert_eq!(e.cells, vec![vec![15.0, 10.0], vec![10.0, 20.0]]);
        assert_eq!(e.baseline, vec![vec![10.0, 10.0], vec![10.0, 10.0]]);
        assert_eq!(e.changed_cells(), 2);
        assert!(e.mapdef.is_none());

        // An earlier record for the same map (with notes) and an unrelated
        // record / unknown value.
        let mut old = e.clone();
        old.cells[0][0] = 99.0;
        old.notes = "keep me".into();
        let other = serde_json::json!({"kind": "def", "def_id": "x", "map_name": "y",
            "cells": [[1.0]], "baseline": [[1.0]], "notes": "", "future": 1});
        let existing = vec![other.clone(), serde_json::to_value(&old).unwrap()];
        let merged = merge_edits(&existing, &cap);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0], other, "untouched records stay verbatim");
        let m = parse_edit(&merged[1]).unwrap();
        assert_eq!(m.cells[0][0], 15.0, "replaced by the capture");
        assert_eq!(m.notes, "keep me");

        // A session definition records ad-hoc edits with their map.
        let mut s = d.clone();
        s.id = format!("{SESSION_DEF_PREFIX}xdf-demo");
        let a = capture_edits(&im, &s);
        assert_eq!(a[0].kind, "adhoc");
        assert_eq!(a[0].def_id, None);
        assert_eq!(a[0].mapdef.as_ref().map(|m| m.address), Some(8));
    }

    /// The pinned apply contract: all or nothing, one undo step, refusals
    /// per cell, never clamped.
    #[test]
    fn applying_a_project_is_all_or_nothing_and_one_undo_step() {
        let d = edit_def();
        let defs = vec![d.clone()];
        let mut src = img();
        src.edit_cell(&d.maps[0], 1, CellEdit::Set(30.0)).unwrap();
        src.edit_cell(&d.maps[1], 2, CellEdit::Set(40.0)).unwrap();
        let edits: Vec<Value> = capture_edits(&src, &d)
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect();
        assert_eq!(edits.len(), 2);

        // Applied onto a fresh stock image: both maps, one undo step.
        let mut im = img();
        let r = apply_project_edits(&mut im, &defs, &edits).unwrap();
        assert_eq!(
            r,
            vec![
                ("injection limiter".to_string(), 1),
                ("boost".to_string(), 1)
            ]
        );
        assert_eq!(im.bytes, src.bytes);
        assert_eq!(im.undo(), Some(1));
        assert!(!im.is_dirty() && !im.can_undo(), "ONE undo step");

        // One refused cell (over max 100; also too big for u8): nothing
        // changes, and the refusal names the map, row and column.
        let mut bad = edits.clone();
        bad[1]["cells"][1][1] = serde_json::json!(200.0);
        let mut im = img();
        let e = apply_project_edits(&mut im, &defs, &bad).unwrap_err();
        assert_eq!(e.len(), 1);
        assert!(e[0].contains("'boost' row 1 col 1"), "{e:?}");
        assert_eq!(im.bytes, vec![20u8; 16], "not clamped, not partly applied");
        assert!(!im.can_undo());

        // An unknown definition / map or a size mismatch blocks it too.
        let mut unknown = edits.clone();
        unknown[0]["def_id"] = serde_json::json!("nope");
        unknown[1]["cells"] = serde_json::json!([[1.0]]);
        let e = apply_project_edits(&mut im, &defs, &unknown).unwrap_err();
        assert_eq!(e.len(), 2, "{e:?}");
        assert!(e[0].contains("not loaded") && e[1].contains("1 × 1"));
        assert_eq!(im.bytes, vec![20u8; 16]);

        // A stock value outside the limits that the project does not change
        // does not block it.
        let mut odd = Image::from_bytes(PathBuf::from("odd.bin"), vec![20u8; 16]);
        odd.bytes[0] = 250; // 125 > max 100, as found in the file
        odd.mark_saved(PathBuf::from("odd.bin"), odd.bytes.clone());
        let mut keep = edits.clone();
        keep[0]["cells"][0][0] = serde_json::json!(125.0);
        assert!(apply_project_edits(&mut odd, &defs, &keep).is_ok());
    }
}
