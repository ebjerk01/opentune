//! Maps browser: every map of every ECU definition, searchable; picking one
//! opens it in the Flash dump view.
//!
//! Left: a search box and the maps grouped by definition (virtualised: only
//! the rows in the viewport are laid out). Right: the selected map's
//! definition (address, storage, scaling, axes, bounds, notes) and, when the
//! open dump holds it, a live heat-grid preview.

use crate::icons;
use crate::image::{fmt_val, Image};
use crate::mapview;
use crate::theme::{self, font, Tokens, Tone, MONO_MEDIUM, UI_MEDIUM, UI_STRONG};
use eframe::egui::{
    self, Align, CornerRadius, Label, Layout, Margin, Rect, RichText, Sense, Ui, UiBuilder, Vec2,
};
use opentune_core::ecudef::{AxisDef, DataType, EcuDef, Endian, MapDef, Scaling};

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

/// Size of the map's data in bytes, without overflow.
fn data_len(m: &MapDef) -> u64 {
    m.cell_count() as u64 * m.data_type.byte_len() as u64
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
    /// Returns `(definition index, map index)` when the user opens a map.
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        t: &Tokens,
        defs: &[EcuDef],
        image: Option<&Image>,
    ) -> Option<(usize, usize)> {
        let total: usize = defs.iter().map(|d| d.maps.len()).sum();
        if total == 0 {
            theme::empty_state(
                ui,
                t,
                icons::STACK,
                "No maps",
                "None of the loaded ECU definitions carries a map library.",
            );
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
        ui.scope_builder(UiBuilder::new().max_rect(right), |ui| {
            if let Some(hit) = self.detail_pane(ui, t, defs, image) {
                open = Some(hit);
            }
        });
        open
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
                }
            },
        );
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
        let empty = MapDef { rows: 0, ..m };
        assert!(!fits(1 << 20, &empty));
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
}
