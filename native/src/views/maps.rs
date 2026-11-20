//! Maps browser: every map of every ECU definition, searchable; picking one
//! opens it in the Flash dump view.
//!
//! Left: a search box and the maps grouped by definition (virtualised: only
//! the rows in the viewport are laid out). Right: the selected map's
//! definition (address, storage, scaling, axes, bounds, notes) and, when the
//! open dump holds it, a live heat-grid preview.
//!
//! Raw bin → named maps (the desktop's `defs.rs`): **Import** reads a
//! TunerPro XDF or an ASAM A2L with the shared `opentune-xdf` /
//! `opentune-a2l` parsers, and **Promote** turns an address range of the open
//! dump into a map. Both become a session definition (id `session-…`, never
//! written to disk) handed to the app as `HostAction::AddDefinition`
//! ([`MapsView::take_actions`]); its maps then open and edit in Flash dump
//! like any other, and a project records them as `adhoc` edits.

use crate::host::HostAction;
use crate::icons;
use crate::image::{fmt_val, Image};
use crate::mapview;
use crate::theme::{self, font, Tokens, Tone, MONO_MEDIUM, UI_MEDIUM, UI_STRONG};
use crate::views::project::SESSION_DEF_PREFIX;
use crate::views::vault::{callout, show_msg, Msg};
use eframe::egui::{
    self, Align, CornerRadius, Label, Layout, Margin, Rect, RichText, Sense, Ui, UiBuilder, Vec2,
};
use opentune_core::ecudef::{AxisDef, DataType, EcuDef, Endian, MapDef, Scaling};
use std::path::{Path, PathBuf};

/// Height of a definition (group) header row in the list.
const HEADER_H: f32 = 30.0;
/// Height of a map row in the list.
const ROW_H: f32 = 42.0;

/// One row of the grouped list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    /// Definition `def` with `matches` maps under it.
    Header {
        def: usize,
        matches: usize,
    },
    Map {
        def: usize,
        map: usize,
    },
}

impl Row {
    fn height(self) -> f32 {
        match self {
            Row::Header { .. } => HEADER_H,
            Row::Map { .. } => ROW_H,
        }
    }
}

/// The filtered list plus each row's top offset (prefix sum of heights).
#[derive(Default)]
struct RowCache {
    key: Option<(String, usize, usize)>,
    rows: Vec<Row>,
    tops: Vec<f32>,
    total_h: f32,
    map_count: usize,
}

#[derive(Default)]
pub struct MapsView {
    pub search: String,
    selected: Option<(usize, usize)>,
    cache: RowCache,
    /// What the detail pane shows.
    detail: Detail,
    /// The last import (file, result).
    import: Option<(PathBuf, Result<Imported, String>)>,
    /// A2L only: CPU base address subtracted from every address (hex).
    a2l_base: String,
    promote: PromoteForm,
    msg: Option<Msg>,
    /// Actions for the app (definitions to add); see [`MapsView::take_actions`].
    actions: Vec<HostAction>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Detail {
    #[default]
    Map,
    Import,
    Promote,
}

// ---------------------------------------------------------------------------
// Import (XDF / A2L) and promote (pure, unit-tested; mirrors desktop defs.rs)
// ---------------------------------------------------------------------------

/// A parsed definition file.
#[derive(Debug, Clone)]
pub(crate) struct Imported {
    /// `xdf` or `a2l`.
    pub kind: &'static str,
    /// XDF title, or the A2L project / module.
    pub title: String,
    pub maps: Vec<MapDef>,
    /// Import-loss warnings (XDF) and notes such as the applied base.
    pub warnings: Vec<String>,
}

/// `0x…` or plain hex.
pub(crate) fn parse_hex(s: &str) -> Result<u32, String> {
    let h = s.trim();
    let h = h
        .strip_prefix("0x")
        .or_else(|| h.strip_prefix("0X"))
        .unwrap_or(h);
    u32::from_str_radix(h, 16).map_err(|_| format!("'{}' is not a hex address", s.trim()))
}

/// Parse an XDF or A2L text (the format sniffed as the desktop does: the
/// extension, then the content). For A2L, `base` is subtracted from every
/// map and axis address (CPU address → file offset).
pub(crate) fn import_text(ext: &str, src: &str, base: Option<u32>) -> Result<Imported, String> {
    let ext = ext.to_ascii_lowercase();
    let head = src.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']);
    let looks_a2l = ext == "a2l"
        || head.contains("ASAP2")
        || head.contains("/begin PROJECT")
        || head.contains("/begin MODULE");
    let looks_xdf = ext == "xdf"
        || head.starts_with("XDF")
        || head.contains("<XDFFORMAT")
        || head.contains("%%TABLE%%");
    if looks_a2l && !looks_xdf {
        let imp = opentune_a2l::parse_a2l(src).map_err(|e| e.to_string())?;
        let mut maps = imp.maps;
        let mut warnings = Vec::new();
        if let Some(b) = base {
            let mut under = 0;
            for m in &mut maps {
                if m.address < b {
                    under += 1;
                }
                m.address = m.address.wrapping_sub(b);
                for ax in [m.x_axis.as_mut(), m.y_axis.as_mut()].into_iter().flatten() {
                    if let Some(a) = ax.address.as_mut() {
                        *a = a.wrapping_sub(b);
                    }
                }
            }
            warnings.push(format!("CPU base 0x{b:08X} subtracted from every address"));
            if under > 0 {
                warnings.push(format!(
                    "{under} map(s) lie below the base; their addresses wrapped and they will \
                     not fit a dump"
                ));
            }
        }
        let title = [imp.project, imp.module]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" / ");
        Ok(Imported {
            kind: "a2l",
            title,
            maps,
            warnings,
        })
    } else if looks_xdf {
        let imp = opentune_xdf::parse_xdf(src).map_err(|e| e.to_string())?;
        let mut warnings = imp.warnings;
        if imp.base_offset != 0 {
            warnings.insert(
                0,
                format!(
                    "XDF base offset 0x{:X} ({}) already applied",
                    imp.base_offset,
                    if imp.base_subtract {
                        "subtracted"
                    } else {
                        "added"
                    }
                ),
            );
        }
        Ok(Imported {
            kind: "xdf",
            title: imp.title.unwrap_or_default(),
            maps: imp.maps,
            warnings,
        })
    } else {
        Err("unrecognised definition file: expected a TunerPro XDF or an ASAM A2L".into())
    }
}

/// Read and parse a definition file.
pub(crate) fn import_file(path: &Path, base: Option<u32>) -> Result<Imported, String> {
    let raw = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    import_text(ext, &String::from_utf8_lossy(&raw), base)
}

/// `[a-z0-9-]`, others become `-`.
fn slug(s: &str) -> String {
    let s: String = s
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "imported".into()
    } else {
        s
    }
}

/// A session definition holding `maps`: no diagnostic ids (the app's target
/// stays the Adapter settings / the engine default), no checksum algorithm
/// (the Flash view keeps the one chosen there).
pub(crate) fn session_def(
    id: &str,
    name: &str,
    family: &str,
    notes: String,
    maps: Vec<MapDef>,
) -> EcuDef {
    let mut d: EcuDef = serde_json::from_value(serde_json::json!({
        "id": format!("{SESSION_DEF_PREFIX}{id}"),
        "name": name,
        "maker": "Imported",
        "family": family,
        "notes": notes,
        "protocol": "iso_tp",
        "diag_layer": "uds",
        "bitrate": 500000,
    }))
    .expect("a session definition is valid");
    d.maps = maps;
    d
}

/// The definition an import becomes (re-importing the same file replaces it).
pub(crate) fn imported_def(path: &Path, imp: &Imported) -> EcuDef {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let label = if imp.title.trim().is_empty() {
        stem.clone()
    } else {
        imp.title.trim().to_string()
    };
    session_def(
        &format!("{}-{}", imp.kind, slug(&stem)),
        &format!("{label} ({}, session)", imp.kind.to_uppercase()),
        "imported",
        format!("Imported from {} for this session.", path.display()),
        imp.maps.clone(),
    )
}

/// The promote form (the desktop's `promote_region` fields).
#[derive(Clone, Debug)]
pub(crate) struct PromoteForm {
    pub name: String,
    pub address: String,
    pub rows: u32,
    pub cols: u32,
    pub data_type: DataType,
    pub endian: Endian,
    pub factor: f64,
    pub offset: f64,
    pub unit: String,
}

impl Default for PromoteForm {
    fn default() -> Self {
        Self {
            name: String::new(),
            address: String::new(),
            rows: 1,
            cols: 16,
            data_type: DataType::U16,
            endian: Endian::Big,
            factor: 1.0,
            offset: 0.0,
            unit: String::new(),
        }
    }
}

/// Id of the session definition that collects promoted maps.
pub(crate) const PROMOTED_ID: &str = "promoted";

/// Validate the form into a map (the desktop's checks: a name, a non-zero
/// factor, and the whole grid inside the open dump).
pub(crate) fn promote_map(f: &PromoteForm, image_len: usize) -> Result<MapDef, String> {
    let name = f.name.trim();
    if name.is_empty() {
        return Err("give the map a name".into());
    }
    let address = parse_hex(&f.address)?;
    if f.factor == 0.0 || !f.factor.is_finite() || !f.offset.is_finite() {
        return Err("the scaling factor must be a non-zero number".into());
    }
    let (rows, cols) = (f.rows.max(1), f.cols.max(1));
    let span = rows as u64 * cols as u64 * f.data_type.byte_len() as u64;
    let need = address as u64 + span;
    if need > image_len as u64 {
        return Err(format!(
            "a {cols} × {rows} {} map at 0x{address:08X} needs 0x{need:08X} bytes but the \
             dump is only 0x{image_len:08X}",
            format!("{:?}", f.data_type).to_lowercase()
        ));
    }
    Ok(MapDef {
        name: name.to_string(),
        address,
        data_type: f.data_type,
        endian: f.endian,
        rows,
        cols,
        scaling: Scaling {
            factor: f.factor,
            offset: f.offset,
        },
        unit: f.unit.trim().to_string(),
        notes: "promoted from a region of the open dump".into(),
        ..Default::default()
    })
}

/// The promoted-maps definition with `m` added (a map of the same name is
/// replaced).
pub(crate) fn with_promoted(defs: &[EcuDef], m: MapDef) -> EcuDef {
    let id = format!("{SESSION_DEF_PREFIX}{PROMOTED_ID}");
    let mut maps = defs
        .iter()
        .find(|d| d.id == id)
        .map(|d| d.maps.clone())
        .unwrap_or_default();
    match maps.iter().position(|x| x.name == m.name) {
        Some(i) => maps[i] = m,
        None => maps.push(m),
    }
    session_def(
        PROMOTED_ID,
        "Promoted maps (session)",
        "promoted",
        "Maps promoted from regions of an open dump in this session.".into(),
        maps,
    )
}

/// Case-insensitive: every whitespace-separated token of `q` must occur in the
/// map name, its unit, or the definition's name / id / family / maker.
fn map_matches(def: &EcuDef, m: &MapDef, q: &str) -> bool {
    let hay = [
        m.name.as_str(),
        m.unit.as_str(),
        def.name.as_str(),
        def.id.as_str(),
        def.family.as_str(),
        def.maker.as_str(),
    ]
    .join("\n")
    .to_lowercase();
    q.split_whitespace()
        .all(|tok| hay.contains(&tok.to_lowercase()))
}

/// The grouped rows for query `q`: a header per definition that has at least
/// one matching map, followed by those maps in definition order. Definitions
/// without matches (or without maps) are left out.
fn build_rows(defs: &[EcuDef], q: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    for (d, def) in defs.iter().enumerate() {
        let hits: Vec<usize> = def
            .maps
            .iter()
            .enumerate()
            .filter(|(_, m)| map_matches(def, m, q))
            .map(|(i, _)| i)
            .collect();
        if hits.is_empty() {
            continue;
        }
        rows.push(Row::Header {
            def: d,
            matches: hits.len(),
        });
        rows.extend(hits.into_iter().map(|map| Row::Map { def: d, map }));
    }
    rows
}

/// Top offset of every row and the total height.
fn row_tops(rows: &[Row]) -> (Vec<f32>, f32) {
    let mut y = 0.0;
    let tops = rows
        .iter()
        .map(|r| {
            let top = y;
            y += r.height();
            top
        })
        .collect();
    (tops, y)
}

/// Index range of the rows that intersect `[y0, y1)` (content coordinates).
fn visible_range(tops: &[f32], total_h: f32, y0: f32, y1: f32) -> std::ops::Range<usize> {
    if tops.is_empty() || y1 <= 0.0 || y0 >= total_h {
        return 0..0;
    }
    // The row containing y0 is the one before the first top past y0.
    let first = tops.partition_point(|&t| t <= y0).saturating_sub(1);
    let end = tops.partition_point(|&t| t < y1);
    first..end.max(first)
}

/// `cols × rows`, the way the Flash view writes dimensions.
fn dims(m: &MapDef) -> String {
    format!("{} × {}", m.cols, m.rows)
}

/// The list row's second line: dimensions, unit, address.
fn row_meta(m: &MapDef) -> String {
    let mut s = dims(m);
    if !m.unit.is_empty() {
        s.push_str(" · ");
        s.push_str(&m.unit);
    }
    s.push_str(&format!(" · 0x{:06X}", m.address));
    s
}

/// Size of the map's data in bytes, without overflow (the shared codec's
/// `rows.max(1) × cols.max(1)` shape).
fn data_len(m: &MapDef) -> u64 {
    crate::image::data_len(m)
}

/// True when the whole map lies inside an image of `len` bytes.
fn fits(len: usize, m: &MapDef) -> bool {
    let bytes = data_len(m);
    bytes > 0 && m.address as u64 + bytes <= len as u64
}

fn dtype_label(dt: DataType) -> String {
    let n = dt.byte_len();
    format!(
        "{} ({n} byte{})",
        format!("{dt:?}").to_lowercase(),
        if n == 1 { "" } else { "s" }
    )
}

fn endian_label(e: Endian) -> &'static str {
    match e {
        Endian::Big => "big-endian (Motorola)",
        Endian::Little => "little-endian (Intel)",
    }
}

/// `raw × factor + offset` with the sign folded into the operator.
fn scaling_text(s: &Scaling) -> String {
    let mut out = format!("raw × {}", s.factor);
    if s.offset > 0.0 {
        out.push_str(&format!(" + {}", s.offset));
    } else if s.offset < 0.0 {
        out.push_str(&format!(" − {}", -s.offset));
    }
    out
}

/// One line describing an axis: name, unit, breakpoints and where they live.
fn axis_text(a: Option<&AxisDef>, n: u32) -> String {
    let Some(a) = a else {
        return format!("{n} points · index only");
    };
    let mut s = if a.name.is_empty() {
        "unnamed".to_string()
    } else {
        a.name.clone()
    };
    if !a.unit.is_empty() {
        s.push_str(&format!(" [{}]", a.unit));
    }
    let count = if a.count > 0 { a.count } else { n };
    s.push_str(&format!(" · {count} points · "));
    if !a.values.is_empty() {
        s.push_str("inline values");
    } else if let Some(addr) = a.address {
        s.push_str(&format!(
            "stored at 0x{addr:06X}, {}",
            format!("{:?}", a.data_type).to_lowercase()
        ));
    } else if let Some(r) = &a.axis_ref {
        s.push_str(&format!("shared axis '{r}'"));
    } else {
        s.push_str("index only");
    }
    s
}

/// The two panes of a browser view: a list column on the left and a detail
/// pane filling the rest, `theme::GAP` apart and from the edges.
pub(crate) fn two_pane(ui: &mut Ui) -> (Rect, Rect) {
    let outer = ui.available_rect_before_wrap().shrink(theme::GAP);
    ui.allocate_rect(ui.available_rect_before_wrap(), Sense::hover());
    let w = (outer.width() * 0.32)
        .clamp(260.0, 380.0)
        .min((outer.width() - theme::GAP) * 0.5)
        .max(0.0);
    let left = Rect::from_min_size(outer.min, Vec2::new(w, outer.height()));
    let right = Rect::from_min_max(
        egui::pos2(left.right() + theme::GAP, outer.top()),
        outer.max,
    );
    (left, right)
}

/// One key / value row of a two-column detail grid: uppercase faint key,
/// mono value.
pub(crate) fn kv(ui: &mut Ui, t: &Tokens, key: &str, value: &str) {
    ui.label(
        RichText::new(key.to_uppercase())
            .font(font(UI_STRONG, 10.5))
            .color(t.fg_faint),
    );
    ui.add(
        Label::new(
            RichText::new(value)
                .font(font(MONO_MEDIUM, 12.0))
                .color(t.fg),
        )
        .wrap(),
    );
    ui.end_row();
}

impl MapsView {
    /// Definitions imported / promoted since the last call, as
    /// `HostAction::AddDefinition`, for the app to apply (`app.rs` drains
    /// this after every `ui`).
    pub fn take_actions(&mut self) -> Vec<HostAction> {
        std::mem::take(&mut self.actions)
    }

    /// Returns `(definition index, map index)` when the user opens a map.
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        t: &Tokens,
        defs: &[EcuDef],
        image: Option<&Image>,
    ) -> Option<(usize, usize)> {
        let total: usize = defs.iter().map(|d| d.maps.len()).sum();
        if total == 0 && self.detail == Detail::Map {
            theme::empty_state(
                ui,
                t,
                icons::STACK,
                "No maps",
                "None of the loaded ECU definitions carries a map library. Import an XDF / \
                 A2L or promote a region of the open dump.",
            );
            ui.vertical_centered(|ui| {
                if theme::button(ui, t, Some(icons::TRAY_ARROW_DOWN), "Import XDF / A2L…").clicked()
                {
                    self.detail = Detail::Import;
                }
                if theme::button(ui, t, Some(icons::STACK_PLUS), "Promote a region…").clicked() {
                    self.detail = Detail::Promote;
                }
            });
            return None;
        }
        // A stale selection (definitions reloaded) is dropped.
        if let Some((d, m)) = self.selected {
            if defs.get(d).and_then(|x| x.maps.get(m)).is_none() {
                self.selected = None;
            }
        }
        self.refresh(defs, total);

        let (left, right) = two_pane(ui);
        let mut open = None;
        ui.scope_builder(UiBuilder::new().max_rect(left), |ui| {
            if let Some(hit) = self.list_pane(ui, t, defs, total) {
                open = Some(hit);
            }
        });
        ui.scope_builder(UiBuilder::new().max_rect(right), |ui| match self.detail {
            Detail::Import => self.import_pane(ui, t, image),
            Detail::Promote => self.promote_pane(ui, t, defs, image),
            Detail::Map => {
                if let Some(hit) = self.detail_pane(ui, t, defs, image) {
                    open = Some(hit);
                }
            }
        });
        open
    }

    fn import_pane(&mut self, ui: &mut Ui, t: &Tokens, image: Option<&Image>) {
        let mut pick = false;
        let mut add = false;
        let mut close = false;
        theme::panel(
            ui,
            t,
            "Import XDF / A2L",
            |ui| {
                if theme::primary_button(ui, t, Some(icons::FOLDER_OPEN), "Choose file…").clicked()
                {
                    pick = true;
                }
                if theme::button(ui, t, Some(icons::X), "Close").clicked() {
                    close = true;
                }
            },
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("import-detail")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .inner_margin(Margin::symmetric(16, 14))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                add = self.import_body(ui, t, image);
                            });
                    });
            },
        );
        if close {
            self.detail = Detail::Map;
        }
        if pick {
            if let Some(p) = rfd::FileDialog::new()
                .set_title("Import a map definition")
                .add_filter("XDF / A2L", &["xdf", "XDF", "a2l", "A2L"])
                .pick_file()
            {
                self.run_import(p);
            }
        }
        if add {
            if let Some((p, Ok(imp))) = &self.import {
                let d = imported_def(p, imp);
                self.msg = Some(Msg::Ok(format!(
                    "Added '{}' ({} maps) for this session. Pick it in Flash dump's \
                     definition list to view and edit its maps; nothing is written to disk.",
                    d.name,
                    d.maps.len()
                )));
                self.actions.push(HostAction::AddDefinition(Box::new(d)));
            }
        }
    }

    fn run_import(&mut self, p: PathBuf) {
        let base = match self.a2l_base.trim() {
            "" => None,
            s => match parse_hex(s) {
                Ok(b) => Some(b),
                Err(e) => {
                    self.import = Some((p, Err(e)));
                    return;
                }
            },
        };
        self.msg = None;
        let r = import_file(&p, base);
        self.import = Some((p, r));
    }

    /// Returns true when "Add as definition" was clicked.
    fn import_body(&mut self, ui: &mut Ui, t: &Tokens, image: Option<&Image>) -> bool {
        let mut add = false;
        ui.add(
            Label::new(
                RichText::new(
                    "A TunerPro XDF or an ASAM A2L becomes a definition for this session: its \
                     maps open in Flash dump against the dump you have open. Nothing is \
                     written to disk.",
                )
                .color(t.fg_dim),
            )
            .wrap(),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("A2L CPU BASE")
                    .font(font(UI_STRONG, 10.5))
                    .color(t.fg_faint),
            );
            ui.add(
                egui::TextEdit::singleline(&mut self.a2l_base)
                    .desired_width(110.0)
                    .hint_text("e.g. 0x80000000"),
            )
            .on_hover_text(
                "Subtracted from every A2L address (CPU address → file offset). Empty: none.",
            );
        });
        if let Some(m) = &self.msg {
            ui.add_space(10.0);
            show_msg(ui, t, m);
        }
        let Some((path, res)) = &self.import else {
            return false;
        };
        ui.add_space(12.0);
        let imp = match res {
            Ok(i) => i,
            Err(e) => {
                callout(
                    ui,
                    t,
                    icons::X_CIRCLE,
                    t.crit,
                    t.crit_soft,
                    &format!("Not imported: {e}"),
                );
                return false;
            }
        };
        egui::Grid::new("import-meta")
            .num_columns(2)
            .spacing(Vec2::new(18.0, 7.0))
            .show(ui, |ui| {
                kv(ui, t, "File", &path.display().to_string());
                kv(ui, t, "Format", &imp.kind.to_uppercase());
                if !imp.title.is_empty() {
                    kv(ui, t, "Title", &imp.title);
                }
                kv(ui, t, "Maps", &imp.maps.len().to_string());
                if let Some(img) = image {
                    let n = imp.maps.iter().filter(|m| fits(img.bytes.len(), m)).count();
                    kv(
                        ui,
                        t,
                        "Fit the open dump",
                        &format!("{n} of {}", imp.maps.len()),
                    );
                }
            });
        for w in imp.warnings.iter().take(8) {
            ui.label(
                RichText::new(format!("{}  {w}", icons::WARNING))
                    .small()
                    .color(t.warn),
            );
        }
        if imp.warnings.len() > 8 {
            ui.label(
                RichText::new(format!("… and {} more warnings", imp.warnings.len() - 8))
                    .small()
                    .color(t.fg_dim),
            );
        }
        ui.add_space(10.0);
        let enabled = !imp.maps.is_empty();
        if ui
            .add_enabled_ui(enabled, |ui| {
                theme::primary_button(ui, t, Some(icons::PLUS), "Add as definition")
            })
            .inner
            .on_hover_text("Add it to the definitions of this session")
            .clicked()
        {
            add = true;
        }
        ui.add_space(10.0);
        let len = image.map(|i| i.bytes.len());
        egui::ScrollArea::vertical()
            .id_salt("import-maps")
            .max_height(360.0)
            .show_rows(ui, 20.0, imp.maps.len(), |ui, range| {
                egui::Grid::new("import-map-rows")
                    .striped(true)
                    .num_columns(4)
                    .min_row_height(20.0)
                    .show(ui, |ui| {
                        for m in &imp.maps[range] {
                            ui.label(RichText::new(&m.name).color(t.fg));
                            ui.label(
                                RichText::new(row_meta(m))
                                    .font(font(MONO_MEDIUM, 11.0))
                                    .color(t.fg_dim),
                            );
                            ui.label(
                                RichText::new(format!("{:?}", m.data_type).to_lowercase())
                                    .color(t.fg_dim),
                            );
                            match len {
                                Some(l) if fits(l, m) => {
                                    ui.label(RichText::new(icons::CHECK).color(t.ok))
                                }
                                Some(_) => ui
                                    .label(RichText::new(icons::X).color(t.crit))
                                    .on_hover_text("outside the open dump"),
                                None => ui.label(""),
                            };
                            ui.end_row();
                        }
                    });
            });
        add
    }

    fn promote_pane(&mut self, ui: &mut Ui, t: &Tokens, defs: &[EcuDef], image: Option<&Image>) {
        let mut make = false;
        let mut close = false;
        theme::panel(
            ui,
            t,
            "Promote a region",
            |ui| {
                if theme::button(ui, t, Some(icons::X), "Close").clicked() {
                    close = true;
                }
            },
            |ui| {
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(16, 14))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(
                            Label::new(
                                RichText::new(
                                    "Name an address range of the open dump as a map. It joins \
                                     the session definition \"Promoted maps\"; select that in \
                                     Flash dump to view and edit it.",
                                )
                                .color(t.fg_dim),
                            )
                            .wrap(),
                        );
                        ui.add_space(10.0);
                        let f = &mut self.promote;
                        egui::Grid::new("promote-form")
                            .num_columns(2)
                            .spacing(Vec2::new(18.0, 7.0))
                            .show(ui, |ui| {
                                let key = |ui: &mut Ui, k: &str| {
                                    ui.label(
                                        RichText::new(k.to_uppercase())
                                            .font(font(UI_STRONG, 10.5))
                                            .color(t.fg_faint),
                                    );
                                };
                                key(ui, "Name");
                                ui.add(
                                    egui::TextEdit::singleline(&mut f.name).desired_width(240.0),
                                );
                                ui.end_row();
                                key(ui, "Address");
                                ui.add(
                                    egui::TextEdit::singleline(&mut f.address)
                                        .desired_width(120.0)
                                        .hint_text("0x…"),
                                );
                                ui.end_row();
                                key(ui, "Columns × rows");
                                ui.horizontal(|ui| {
                                    ui.add(egui::DragValue::new(&mut f.cols).range(1..=4096));
                                    ui.label("×");
                                    ui.add(egui::DragValue::new(&mut f.rows).range(1..=4096));
                                });
                                ui.end_row();
                                key(ui, "Storage");
                                ui.horizontal(|ui| {
                                    egui::ComboBox::from_id_salt("promote-type")
                                        .selected_text(dtype_label(f.data_type))
                                        .show_ui(ui, |ui| {
                                            for dt in [
                                                DataType::U8,
                                                DataType::S8,
                                                DataType::U16,
                                                DataType::S16,
                                                DataType::U32,
                                                DataType::S32,
                                                DataType::F32,
                                            ] {
                                                ui.selectable_value(
                                                    &mut f.data_type,
                                                    dt,
                                                    dtype_label(dt),
                                                );
                                            }
                                        });
                                    egui::ComboBox::from_id_salt("promote-endian")
                                        .selected_text(endian_label(f.endian))
                                        .show_ui(ui, |ui| {
                                            for e in [Endian::Big, Endian::Little] {
                                                ui.selectable_value(
                                                    &mut f.endian,
                                                    e,
                                                    endian_label(e),
                                                );
                                            }
                                        });
                                });
                                ui.end_row();
                                key(ui, "Scaling");
                                ui.horizontal(|ui| {
                                    ui.label("raw ×");
                                    ui.add(egui::DragValue::new(&mut f.factor).speed(0.01));
                                    ui.label("+");
                                    ui.add(egui::DragValue::new(&mut f.offset).speed(0.1));
                                });
                                ui.end_row();
                                key(ui, "Unit");
                                ui.add(
                                    egui::TextEdit::singleline(&mut f.unit).desired_width(120.0),
                                );
                                ui.end_row();
                            });
                        ui.add_space(12.0);
                        if ui
                            .add_enabled_ui(image.is_some(), |ui| {
                                theme::primary_button(ui, t, Some(icons::PLUS), "Make map")
                            })
                            .inner
                            .on_disabled_hover_text("Open a dump in Flash dump first")
                            .clicked()
                        {
                            make = true;
                        }
                        if let Some(m) = &self.msg {
                            ui.add_space(10.0);
                            show_msg(ui, t, m);
                        }
                    });
            },
        );
        if close {
            self.detail = Detail::Map;
        }
        if make {
            let len = image.map_or(0, |i| i.bytes.len());
            match promote_map(&self.promote, len) {
                Ok(m) => {
                    let name = m.name.clone();
                    let d = with_promoted(defs, m);
                    self.msg = Some(Msg::Ok(format!(
                        "'{name}' added to \"{}\" ({} map(s)) for this session.",
                        d.name,
                        d.maps.len()
                    )));
                    self.actions.push(HostAction::AddDefinition(Box::new(d)));
                }
                Err(e) => self.msg = Some(Msg::Warn(format!("Not promoted: {e}"))),
            }
        }
    }

    fn refresh(&mut self, defs: &[EcuDef], total: usize) {
        let key = (self.search.clone(), defs.len(), total);
        if self.cache.key.as_ref() == Some(&key) {
            return;
        }
        let rows = build_rows(defs, &self.search);
        let (tops, total_h) = row_tops(&rows);
        self.cache = RowCache {
            map_count: rows.iter().filter(|r| matches!(r, Row::Map { .. })).count(),
            key: Some(key),
            rows,
            tops,
            total_h,
        };
    }

    fn list_pane(
        &mut self,
        ui: &mut Ui,
        t: &Tokens,
        defs: &[EcuDef],
        total: usize,
    ) -> Option<(usize, usize)> {
        let shown = self.cache.map_count;
        let mut open = None;
        let mut tool = None;
        theme::panel(
            ui,
            t,
            "Maps",
            |ui| {
                let txt = if shown == total {
                    format!("{total}")
                } else {
                    format!("{shown} / {total}")
                };
                theme::pill(ui, t, &txt, Tone::Muted, true);
                if theme::icon_button(
                    ui,
                    t,
                    icons::TRAY_ARROW_DOWN,
                    "Import an XDF or A2L definition",
                )
                .clicked()
                {
                    tool = Some(Detail::Import);
                }
                if theme::icon_button(
                    ui,
                    t,
                    icons::STACK_PLUS,
                    "Promote a region of the dump to a map",
                )
                .clicked()
                {
                    tool = Some(Detail::Promote);
                }
            },
            |ui| {
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(icons::MAGNIFYING_GLASS).color(t.fg_faint));
                            ui.add(
                                egui::TextEdit::singleline(&mut self.search)
                                    .hint_text("Search name, unit or definition")
                                    .desired_width(f32::INFINITY),
                            );
                        });
                    });
                theme::hline(ui, t.border_soft);
                if self.cache.rows.is_empty() {
                    ui.add_space(18.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "No map matches \u{201c}{}\u{201d}.",
                                self.search.trim()
                            ))
                            .color(t.fg_dim),
                        );
                    });
                    return;
                }
                let rows = &self.cache.rows;
                let tops = &self.cache.tops;
                let total_h = self.cache.total_h;
                let selected = self.selected;
                let mut clicked = None;
                egui::ScrollArea::vertical()
                    .id_salt("maps-list")
                    .auto_shrink([false, false])
                    .show_viewport(ui, |ui, viewport| {
                        let w = ui.available_width();
                        let (full, _) =
                            ui.allocate_exact_size(Vec2::new(w, total_h), Sense::hover());
                        for i in visible_range(tops, total_h, viewport.min.y, viewport.max.y) {
                            let row = rows[i];
                            let r = Rect::from_min_size(
                                full.min + Vec2::new(0.0, tops[i]),
                                Vec2::new(w, row.height()),
                            );
                            match row {
                                Row::Header { def, matches } => {
                                    header_row(ui, t, r, &defs[def], matches)
                                }
                                Row::Map { def, map } => {
                                    let sel = selected == Some((def, map));
                                    let resp =
                                        map_row(ui, t, r, (def, map), &defs[def].maps[map], sel);
                                    if resp.double_clicked() {
                                        open = Some((def, map));
                                    }
                                    if resp.clicked() || resp.double_clicked() {
                                        clicked = Some((def, map));
                                    }
                                }
                            }
                        }
                    });
                if clicked.is_some() {
                    self.selected = clicked;
                    self.detail = Detail::Map;
                }
            },
        );
        if let Some(d) = tool {
            self.detail = d;
            self.msg = None;
        }
        open
    }

    fn detail_pane(
        &mut self,
        ui: &mut Ui,
        t: &Tokens,
        defs: &[EcuDef],
        image: Option<&Image>,
    ) -> Option<(usize, usize)> {
        let Some((d, mi)) = self.selected else {
            theme::panel(
                ui,
                t,
                "Map",
                |_| {},
                |ui| {
                    theme::empty_state(
                        ui,
                        t,
                        icons::STACK,
                        "Pick a map",
                        "Select a map on the left to see its definition and a preview. \
                         Double-click opens it in Flash dump.",
                    )
                },
            );
            return None;
        };
        let def = &defs[d];
        let m = &def.maps[mi];
        let mut open = false;
        theme::panel(
            ui,
            t,
            &m.name,
            |ui| {
                if theme::primary_button(ui, t, Some(icons::ARROW_SQUARE_OUT), "Open in Flash dump")
                    .on_hover_text("Select this definition and map in the Flash dump view")
                    .clicked()
                {
                    open = true;
                }
                if !m.unit.is_empty() {
                    theme::pill(ui, t, &m.unit, Tone::Muted, true);
                }
                theme::pill(ui, t, &dims(m), Tone::Muted, true);
            },
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("map-detail")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .inner_margin(Margin::symmetric(16, 14))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                details(ui, t, def, m);
                                ui.add_space(14.0);
                                preview(ui, t, m, image);
                            });
                    });
            },
        );
        open.then_some((d, mi))
    }
}

fn header_row(ui: &mut Ui, t: &Tokens, r: Rect, def: &EcuDef, matches: usize) {
    let inner = Rect::from_min_max(r.min + Vec2::new(14.0, 10.0), r.max - Vec2::new(14.0, 2.0));
    ui.painter().text(
        egui::pos2(inner.right(), inner.center().y),
        egui::Align2::RIGHT_CENTER,
        matches.to_string(),
        font(MONO_MEDIUM, 10.5),
        t.fg_faint,
    );
    let label = Rect::from_min_max(inner.min, egui::pos2(inner.right() - 36.0, inner.bottom()));
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(label)
            .layout(Layout::left_to_right(Align::Center)),
        |ui| {
            ui.add(
                Label::new(
                    RichText::new(def.name.to_uppercase())
                        .font(font(UI_STRONG, 10.5))
                        .color(t.fg_faint),
                )
                .truncate()
                .selectable(false),
            )
            .on_hover_text(&def.id);
        },
    );
}

fn map_row(
    ui: &mut Ui,
    t: &Tokens,
    r: Rect,
    key: (usize, usize),
    m: &MapDef,
    selected: bool,
) -> egui::Response {
    let id = ui.make_persistent_id(("map-row", key));
    let resp = ui.interact(r, id, Sense::click());
    let bg = r.shrink2(Vec2::new(6.0, 1.0));
    if selected {
        ui.painter()
            .rect_filled(bg, CornerRadius::same(theme::RADIUS_SM), t.sel);
        ui.painter().rect_filled(
            Rect::from_min_size(
                bg.min + Vec2::new(0.0, 8.0),
                Vec2::new(3.0, bg.height() - 16.0),
            ),
            CornerRadius::same(2),
            t.accent,
        );
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(bg, CornerRadius::same(theme::RADIUS_SM), t.surface2);
    }
    let inner = r.shrink2(Vec2::new(16.0, 5.0));
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::top_down(Align::Min)),
        |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.add(
                Label::new(
                    RichText::new(&m.name)
                        .font(font(if selected { UI_STRONG } else { UI_MEDIUM }, 13.0))
                        .color(t.fg),
                )
                .truncate()
                .selectable(false),
            );
            ui.add(
                Label::new(
                    RichText::new(row_meta(m))
                        .font(font(MONO_MEDIUM, 11.0))
                        .color(t.fg_faint),
                )
                .truncate()
                .selectable(false),
            );
        },
    );
    resp
}

fn details(ui: &mut Ui, t: &Tokens, def: &EcuDef, m: &MapDef) {
    theme::section_label(ui, t, "Definition");
    ui.add_space(4.0);
    egui::Grid::new("map-def")
        .num_columns(2)
        .spacing(Vec2::new(18.0, 7.0))
        .show(ui, |ui| {
            kv(ui, t, "Definition", &format!("{} ({})", def.name, def.id));
            let len = data_len(m);
            kv(
                ui,
                t,
                "Address",
                &format!(
                    "0x{:06X} – 0x{:06X} · {len} bytes",
                    m.address,
                    (m.address as u64 + len).saturating_sub(1),
                ),
            );
            kv(
                ui,
                t,
                "Size",
                &format!(
                    "{} cols × {} rows · {} cells",
                    m.cols,
                    m.rows,
                    m.cell_count()
                ),
            );
            kv(ui, t, "Data type", &dtype_label(m.data_type));
            kv(ui, t, "Byte order", endian_label(m.endian));
            kv(ui, t, "Scaling", &scaling_text(&m.scaling));
            kv(ui, t, "Unit", if m.unit.is_empty() { "–" } else { &m.unit });
            if m.min.is_some() || m.max.is_some() {
                let b = |v: Option<f64>| v.map(fmt_val).unwrap_or_else(|| "–".into());
                kv(
                    ui,
                    t,
                    "Bounds",
                    &format!("min {} · max {}", b(m.min), b(m.max)),
                );
            }
            kv(ui, t, "X axis", &axis_text(m.x_axis.as_ref(), m.cols));
            kv(ui, t, "Y axis", &axis_text(m.y_axis.as_ref(), m.rows));
        });
    if !m.notes.is_empty() {
        ui.add_space(12.0);
        theme::section_label(ui, t, "Notes");
        ui.add_space(2.0);
        ui.add(Label::new(RichText::new(&m.notes).color(t.fg_dim)).wrap());
    }
}

fn preview(ui: &mut Ui, t: &Tokens, m: &MapDef, image: Option<&Image>) {
    theme::hline(ui, t.border_soft);
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        theme::section_label(ui, t, "Preview");
        if let Some(img) = image {
            theme::pill(ui, t, &img.name(), Tone::Muted, true);
        }
    });
    ui.add_space(6.0);
    let Some(img) = image else {
        ui.label(
            RichText::new(format!(
                "{}  Open a dump in Flash dump to preview this map's values.",
                icons::INFO
            ))
            .color(t.fg_dim),
        );
        return;
    };
    if !fits(img.bytes.len(), m) {
        ui.label(
            RichText::new(format!(
                "{}  The open dump ({} bytes) does not contain this map (it ends at 0x{:X}).",
                icons::WARNING,
                img.bytes.len(),
                m.address as u64 + data_len(m)
            ))
            .color(t.warn),
        );
        return;
    }
    ui.label(
        RichText::new(
            "Values are read from the open dump at this address; they only mean something \
             when the dump matches this definition.",
        )
        .small()
        .color(t.fg_faint),
    );
    ui.add_space(6.0);
    let range = egui::ScrollArea::horizontal()
        .id_salt("map-preview")
        .auto_shrink([false, true])
        .show(ui, |ui| mapview::draw_map(ui, t, &img.bytes, m))
        .inner;
    if let Some((lo, hi)) = range {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            mapview::legend(ui, t, 160.0, &fmt_val(lo), &fmt_val(hi));
            ui.label(
                RichText::new("hover a cell for its value")
                    .small()
                    .color(t.fg_faint),
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(name: &str, unit: &str, address: u32) -> MapDef {
        MapDef {
            name: name.into(),
            unit: unit.into(),
            address,
            rows: 2,
            cols: 3,
            data_type: DataType::U16,
            ..Default::default()
        }
    }

    fn def(id: &str, name: &str, maps: Vec<MapDef>) -> EcuDef {
        let mut d: EcuDef = serde_json::from_value(serde_json::json!({
            "id": id, "name": name, "maker": "Bosch", "family": "EDC15",
            "protocol": "iso_tp", "diag_layer": "uds", "bitrate": 500000
        }))
        .unwrap_or_else(|e| panic!("minimal EcuDef: {e}"));
        d.maps = maps;
        d
    }

    fn sample() -> Vec<EcuDef> {
        vec![
            def(
                "a",
                "Alpha EDC",
                vec![
                    map("Boost target", "hPa", 0x100),
                    map("Torque limiter", "Nm", 0x200),
                ],
            ),
            def("empty", "No maps", vec![]),
            def("b", "Beta ME7", vec![map("Ignition base", "deg", 0x10)]),
        ]
    }

    #[test]
    fn search_matches_name_unit_and_definition_tokens() {
        let d = sample();
        let m = &d[0].maps[0];
        assert!(map_matches(&d[0], m, ""));
        assert!(map_matches(&d[0], m, "   "));
        assert!(map_matches(&d[0], m, "boost"));
        assert!(map_matches(&d[0], m, "HPA"));
        assert!(map_matches(&d[0], m, "alpha target"));
        assert!(map_matches(&d[0], m, "edc15"), "family");
        assert!(!map_matches(&d[0], m, "boost deg"), "every token must hit");
        assert!(!map_matches(&d[0], m, "beta"));
    }

    #[test]
    fn rows_are_grouped_by_definition_and_skip_empty_groups() {
        let d = sample();
        assert_eq!(
            build_rows(&d, ""),
            vec![
                Row::Header { def: 0, matches: 2 },
                Row::Map { def: 0, map: 0 },
                Row::Map { def: 0, map: 1 },
                Row::Header { def: 2, matches: 1 },
                Row::Map { def: 2, map: 0 },
            ]
        );
        assert_eq!(
            build_rows(&d, "nm"),
            vec![
                Row::Header { def: 0, matches: 1 },
                Row::Map { def: 0, map: 1 }
            ]
        );
        assert!(build_rows(&d, "nothing-like-this").is_empty());
    }

    #[test]
    fn row_offsets_and_viewport_culling() {
        let rows = build_rows(&sample(), "");
        let (tops, h) = row_tops(&rows);
        assert_eq!(tops[0], 0.0);
        assert_eq!(tops[1], HEADER_H);
        assert_eq!(h, 2.0 * HEADER_H + 3.0 * ROW_H);
        assert_eq!(visible_range(&tops, h, 0.0, h), 0..rows.len());
        // A viewport inside the second map row only.
        let y = tops[2] + 1.0;
        assert_eq!(visible_range(&tops, h, y, y + 2.0), 2..3);
        // Straddling the boundary between rows 1 and 2.
        assert_eq!(visible_range(&tops, h, tops[2] - 1.0, tops[2] + 1.0), 1..3);
        assert_eq!(visible_range(&tops, h, h + 10.0, h + 20.0), 0..0);
        assert_eq!(visible_range(&[], 0.0, 0.0, 100.0), 0..0);
    }

    #[test]
    fn map_fits_inside_image() {
        let m = map("m", "", 0x10); // 6 cells × 2 bytes = 12 bytes
        assert!(fits(0x10 + 12, &m));
        assert!(!fits(0x10 + 11, &m));
        let far = MapDef {
            address: u32::MAX - 2,
            ..m.clone()
        };
        assert!(!fits(u32::MAX as usize, &far), "no u32 overflow");
        // rows 0 is one row (the shared codec's shape rule): 3 cells, 6 bytes.
        let one_row = MapDef { rows: 0, ..m };
        assert!(fits(0x10 + 6, &one_row) && !fits(0x10 + 5, &one_row));
    }

    #[test]
    fn labels() {
        assert_eq!(dims(&map("m", "", 0)), "3 × 2");
        assert_eq!(row_meta(&map("m", "Nm", 0x1234)), "3 × 2 · Nm · 0x001234");
        assert_eq!(row_meta(&map("m", "", 0x10)), "3 × 2 · 0x000010");
        assert_eq!(dtype_label(DataType::U8), "u8 (1 byte)");
        assert_eq!(dtype_label(DataType::S16), "s16 (2 bytes)");
        assert_eq!(endian_label(Endian::Little), "little-endian (Intel)");
        assert_eq!(
            scaling_text(&Scaling {
                factor: 0.75,
                offset: -48.0
            }),
            "raw × 0.75 − 48"
        );
        assert_eq!(
            scaling_text(&Scaling {
                factor: 1.0,
                offset: 0.0
            }),
            "raw × 1"
        );
        assert_eq!(axis_text(None, 8), "8 points · index only");
        let a = AxisDef {
            name: "rpm".into(),
            unit: "1/min".into(),
            address: Some(0x40),
            data_type: DataType::U16,
            ..Default::default()
        };
        assert_eq!(
            axis_text(Some(&a), 16),
            "rpm [1/min] · 16 points · stored at 0x000040, u16"
        );
        let r = AxisDef {
            axis_ref: Some("rpm_axis".into()),
            count: 4,
            ..Default::default()
        };
        assert_eq!(
            axis_text(Some(&r), 9),
            "unnamed · 4 points · shared axis 'rpm_axis'"
        );
    }

    const XDF: &str = r#"<XDFFORMAT version="1.60"><XDFHEADER><deftitle>Demo XDF</deftitle></XDFHEADER>
<XDFTABLE><title>AFR</title><XDFAXIS id="z">
<EMBEDDEDDATA mmedaddress="0x10" mmedelementsizebits="8"/>
<MATH equation="X/128"><VAR id="X"/></MATH>
</XDFAXIS></XDFTABLE></XDFFORMAT>"#;

    const A2L: &str = r#"ASAP2_VERSION 1 71
/begin PROJECT DemoProj "demo project"
  /begin HEADER "h" VERSION "1" /end HEADER
  /begin MODULE DemoMod "the module"

    /begin MOD_COMMON ""
      BYTE_ORDER MSB_FIRST
    /end MOD_COMMON

    /begin RECORD_LAYOUT RL_MAP
      FNC_VALUES 1 SWORD COLUMN_DIR DIRECT
    /end RECORD_LAYOUT

    /begin RECORD_LAYOUT RL_AXIS
      AXIS_PTS_X 1 UWORD INDEX_INCR DIRECT
    /end RECORD_LAYOUT

    /begin COMPU_METHOD CM_TORQUE
      "torque scaling"
      RAT_FUNC "%8.4" "Nm"
      COEFFS 0 655.350000 0.000000 0 0.000000 10.240000
    /end COMPU_METHOD

    /begin COMPU_METHOD CM_RPM
      "rpm scaling"
      RAT_FUNC "%8.0" "rpm"
      COEFFS 0 1 0 0 0 1
    /end COMPU_METHOD

    /begin COMPU_METHOD CM_TAB
      "nonlinear sensor"
      TAB_INTP "%8.2" "V"
      COMPU_TAB_REF TBL_SENS
    /end COMPU_METHOD

    /begin AXIS_PTS AX_RPM
      "rpm breakpoints"
      0x8000
      no_input
      RL_AXIS
      0
      CM_RPM
      16
      0.0
      7000.0
    /end AXIS_PTS

    /begin CHARACTERISTIC map0001
      "boost target"
      MAP
      0x1000
      RL_MAP
      0
      CM_TORQUE
      -10.0
      320.0
      /begin AXIS_DESCR
        COM_AXIS rpm_in CM_RPM 16 0.0 7000.0
        AXIS_PTS_REF AX_RPM
      /end AXIS_DESCR
      /begin AXIS_DESCR
        STD_AXIS load_in CM_RPM 8 0.0 100.0
      /end AXIS_DESCR
    /end CHARACTERISTIC
  /end MODULE
/end PROJECT
"#;

    #[test]
    fn imports_xdf_and_a2l_like_the_desktop() {
        let x = import_text("xdf", XDF, None).unwrap();
        assert_eq!((x.kind, x.title.as_str()), ("xdf", "Demo XDF"));
        assert_eq!(x.maps.len(), 1);
        assert_eq!(x.maps[0].address, 0x10);
        // Sniffed from the content without an extension too.
        assert_eq!(import_text("", XDF, None).unwrap().kind, "xdf");
        let a = import_text("A2L", A2L, None).unwrap();
        assert_eq!(a.kind, "a2l");
        assert!(a.title.contains("DemoProj"), "{}", a.title);
        let m = a
            .maps
            .iter()
            .find(|m| m.address == 0x1000)
            .expect("map0001");
        // A CPU base is subtracted from map and stored-axis addresses.
        let b = import_text("a2l", A2L, Some(0x800)).unwrap();
        let mb = b.maps.iter().find(|x| x.name == m.name).unwrap();
        assert_eq!(mb.address, 0x800);
        assert!(b.warnings.iter().any(|w| w.contains("0x00000800")));
        let under = import_text("a2l", A2L, Some(0x2000)).unwrap();
        assert!(under.warnings.iter().any(|w| w.contains("below the base")));
        assert!(import_text("txt", "hello", None).is_err());
        assert_eq!(parse_hex("0x1F").unwrap(), 0x1F);
        assert_eq!(parse_hex("ff").unwrap(), 0xFF);
        assert!(parse_hex("zz").is_err());
    }

    #[test]
    fn imported_and_promoted_definitions_are_session_only() {
        let x = import_text("xdf", XDF, None).unwrap();
        let d = imported_def(Path::new("/tmp/My Tune.xdf"), &x);
        assert_eq!(d.id, "session-xdf-my-tune");
        assert!(d.name.contains("Demo XDF") && d.name.contains("session"));
        assert_eq!((d.tx_id, d.rx_id, d.checksum.clone()), (0, 0, None));
        assert_eq!(d.maps.len(), 1);

        let mut f = PromoteForm {
            name: "found".into(),
            address: "0x10".into(),
            rows: 2,
            cols: 4,
            ..Default::default()
        };
        // u16, 2 × 4 = 16 bytes from 0x10: needs 0x20.
        assert!(promote_map(&f, 0x1F).is_err());
        let m = promote_map(&f, 0x20).unwrap();
        assert_eq!((m.address, m.rows, m.cols), (0x10, 2, 4));
        f.factor = 0.0;
        assert!(promote_map(&f, 0x100).is_err());
        f.factor = 1.0;
        f.name = " ".into();
        assert!(promote_map(&f, 0x100).is_err());
        // Promoted maps collect in one definition; the same name replaces.
        let d1 = with_promoted(&[], m.clone());
        assert_eq!(d1.id, "session-promoted");
        let other = MapDef {
            name: "other".into(),
            ..m.clone()
        };
        let d2 = with_promoted(std::slice::from_ref(&d1), other);
        assert_eq!(d2.maps.len(), 2);
        let again = MapDef { address: 0x40, ..m };
        let d3 = with_promoted(&[d2], again);
        assert_eq!(d3.maps.len(), 2);
        assert_eq!(d3.maps[0].address, 0x40);
    }

    #[test]
    fn adding_a_definition_is_an_action_for_the_app() {
        let mut v = MapsView::default();
        let x = import_text("xdf", XDF, None).unwrap();
        v.import = Some((PathBuf::from("a.xdf"), Ok(x.clone())));
        v.actions
            .push(HostAction::AddDefinition(Box::new(imported_def(
                Path::new("a.xdf"),
                &x,
            ))));
        let a = v.take_actions();
        assert!(matches!(&a[..], [HostAction::AddDefinition(d)] if d.id == "session-xdf-a"));
        assert!(v.take_actions().is_empty());
    }
}
