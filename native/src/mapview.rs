//! Map canvas: a calibration table painted directly (rounded cells + text),
//! heat-coloured between the map's own min and max with the classic
//! blue→red ramp of the design spec. Cells outside the clip rectangle are
//! skipped, so a long stack of maps costs only what is visible.
//!
//! An editable map has a cursor (the active cell) and a rectangular range
//! (shift-click, drag, Shift+arrows, Ctrl+A); the range operations live in
//! [`mapops`].

#[path = "mapops.rs"]
pub mod mapops;

use crate::image::{axis_values, fmt_val, map_values};
use crate::theme::{font, Tokens, MONO_MEDIUM};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Margin, Pos2, Rect, RichText, Sense, Stroke, Vec2,
};
use mapops::Sel;
use opentune_core::ecudef::MapDef;

pub const CELL_H: f32 = 22.0;
pub const GAP: f32 = 2.0;
/// Axis label column width.
pub const AXIS_W: f32 = 52.0;

/// The spec's six-stop ramp (deep blue → teal → green → yellow → orange → red).
pub const STOPS: [(u8, u8, u8); 6] = [
    (43, 58, 103),
    (47, 125, 122),
    (79, 174, 90),
    (215, 198, 74),
    (232, 145, 59),
    (215, 72, 63),
];

pub fn heat(t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let x = t * (STOPS.len() - 1) as f32;
    let i = (x.floor() as usize).min(STOPS.len() - 2);
    let f = x - i as f32;
    let (a, b) = (STOPS[i], STOPS[i + 1]);
    let l = |p: u8, q: u8| (p as f32 + (q as f32 - p as f32) * f).round() as u8;
    Color32::from_rgb(l(a.0, b.0), l(a.1, b.1), l(a.2, b.2))
}

/// Dark text on the light half of the ramp, light text on the dark half.
pub fn text_on(t: f32) -> Color32 {
    if t > 0.52 {
        Color32::from_rgb(0x0c, 0x0f, 0x16)
    } else {
        Color32::from_rgb(0xee, 0xf1, 0xf7)
    }
}

/// Cell width that fits the widest value of the map.
fn cell_w(vals: &[f64], xs: &[f64]) -> f32 {
    let chars = vals
        .iter()
        .chain(xs)
        .map(|v| fmt_val(*v).len())
        .max()
        .unwrap_or(3);
    (chars as f32 * 6.6 + 12.0).max(34.0)
}

/// The low→high legend ramp, `w` wide.
pub fn legend(ui: &mut egui::Ui, t: &Tokens, w: f32, lo: &str, hi: &str) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(lo)
                .font(font(MONO_MEDIUM, 11.0))
                .color(t.fg_dim),
        );
        let (r, _) = ui.allocate_exact_size(Vec2::new(w, 10.0), Sense::hover());
        let n = 48;
        for k in 0..n {
            let x0 = r.left() + r.width() * k as f32 / n as f32;
            let x1 = r.left() + r.width() * (k + 1) as f32 / n as f32 + 0.5;
            let c = heat(k as f32 / (n - 1) as f32);
            let cr = if k == 0 {
                CornerRadius {
                    nw: 5,
                    sw: 5,
                    ne: 0,
                    se: 0,
                }
            } else if k == n - 1 {
                CornerRadius {
                    nw: 0,
                    sw: 0,
                    ne: 5,
                    se: 5,
                }
            } else {
                CornerRadius::ZERO
            };
            ui.painter()
                .rect_filled(Rect::from_x_y_ranges(x0..=x1, r.y_range()), cr, c);
        }
        ui.label(
            RichText::new(hi)
                .font(font(MONO_MEDIUM, 11.0))
                .color(t.fg_dim),
        );
    });
}

/// Selection and in-place text entry of an editable map.
#[derive(Default)]
pub struct CellCursor {
    /// Address of the map the cursor belongs to (reset on another map).
    pub map_addr: Option<u32>,
    /// Selected (active) cell (column, row).
    pub sel: Option<(usize, usize)>,
    /// The other corner of the selected range (`None`: just `sel`).
    pub anchor: Option<(usize, usize)>,
    /// Text being typed into the selected cell.
    pub text: Option<String>,
    focus_text: bool,
    /// On focus: select the whole text (else the caret goes to the end).
    select_all: bool,
    scroll_to_sel: bool,
}

impl CellCursor {
    /// Forget the selection when `m` is another map.
    pub fn follow(&mut self, m: &MapDef) {
        if self.map_addr != Some(m.address) {
            *self = Self {
                map_addr: Some(m.address),
                ..Default::default()
            };
        }
    }

    /// Select `idx` (row-major) of a map with `cols` columns.
    pub fn select(&mut self, idx: usize, cols: usize) {
        let cols = cols.max(1);
        self.sel = Some((idx % cols, idx / cols));
        self.anchor = None;
        self.text = None;
        self.scroll_to_sel = true;
    }

    /// The selected range: from the anchor to the active cell.
    pub fn range(&self) -> Option<Sel> {
        let s = self.sel?;
        Some(Sel::span(self.anchor.unwrap_or(s), s))
    }

    /// Select a whole `cols × rows` map (the active cell at the top left).
    pub fn select_whole(&mut self, cols: usize, rows: usize) {
        self.sel = Some((0, 0));
        self.anchor = Some((cols.max(1) - 1, rows.max(1) - 1));
        self.text = None;
    }

    /// Select the range `s` (active cell at its top left).
    pub fn select_range(&mut self, s: Sel) {
        self.sel = Some((s.c0, s.r0));
        self.anchor = Some((s.c1, s.r1));
        self.text = None;
    }
}

/// What the user asked for on an editable map this frame.
#[derive(Clone, Debug, PartialEq)]
pub enum CellCommand {
    /// Cell index (row-major) and the edit.
    Edit(usize, crate::image::CellEdit),
    /// `+` / `-` on a range of more than one cell: raw steps for every cell.
    Step(Sel, i32),
    /// Copy (or cut) the range.
    Copy(Sel),
    /// Paste text at the range.
    Paste(Sel, String),
    /// Typed text that is not a number.
    Invalid(String),
}

/// The result of drawing an editable map.
pub struct MapOut {
    pub lo: f64,
    pub hi: f64,
    pub cmd: Option<CellCommand>,
}

/// Paint one map. Returns the value range, or `None` when the map runs past
/// the end of the image.
pub fn draw_map(ui: &mut egui::Ui, t: &Tokens, img: &[u8], m: &MapDef) -> Option<(f64, f64)> {
    draw_map_ex(ui, t, img, m, None, &|_| false).map(|o| (o.lo, o.hi))
}

/// Parse a typed cell value (`,` accepted as the decimal separator).
pub fn parse_value(s: &str) -> Option<f64> {
    s.trim()
        .replace(',', ".")
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

/// Keys of a focused, editable map that are not text entry.
#[derive(Default)]
struct NavKeys {
    dx: i32,
    dy: i32,
    /// Shift held with the arrows: extend the range instead of moving it.
    extend: bool,
    step: i32,
    edit: bool,
    cancel: bool,
    ch: Option<char>,
    select_all: bool,
    copy: bool,
    paste: Option<String>,
}

fn nav_keys(ui: &egui::Ui) -> NavKeys {
    use egui::{Event, Key};
    ui.input(|i| {
        let mut k = NavKeys::default();
        let big = if i.modifiers.shift { 10 } else { 1 };
        for e in &i.events {
            match e {
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } if !modifiers.command => match key {
                    Key::ArrowLeft => k.dx -= 1,
                    Key::ArrowRight => k.dx += 1,
                    Key::ArrowUp => k.dy -= 1,
                    Key::ArrowDown => k.dy += 1,
                    Key::Enter | Key::F2 => k.edit = true,
                    Key::Escape => k.cancel = true,
                    _ => {}
                },
                Event::Key {
                    key: Key::A,
                    pressed: true,
                    modifiers,
                    ..
                } if modifiers.command => k.select_all = true,
                Event::Copy | Event::Cut => k.copy = true,
                Event::Paste(s) => k.paste = Some(s.clone()),
                Event::Text(s) if !i.modifiers.command => match s.as_str() {
                    "+" | "=" => k.step += big,
                    "-" => k.step -= big,
                    _ => {
                        k.ch = s
                            .chars()
                            .find(|c| c.is_ascii_digit() || *c == '.' || *c == ',');
                    }
                },
                _ => {}
            }
        }
        k.extend = i.modifiers.shift;
        k
    })
}

/// Paint one map; with a `cursor` it is editable: click selects, arrows move,
/// Enter/F2 or typing a digit edits, `+`/`-` step one raw LSB (Shift: 10),
/// Esc cancels. `changed(idx)` marks cells that differ from the saved file.
pub fn draw_map_ex(
    ui: &mut egui::Ui,
    t: &Tokens,
    img: &[u8],
    m: &MapDef,
    cursor: Option<&mut CellCursor>,
    changed: &dyn Fn(usize) -> bool,
) -> Option<MapOut> {
    draw_map_edit(ui, t, img, m, cursor, changed, &|_| None)
}

/// [`draw_map_ex`] plus ranges: shift-click / drag / Shift+arrows select a
/// rectangle, Ctrl+A all, Ctrl+C / Ctrl+V copy and paste it, `+`/`-` step
/// every selected cell. `was(idx)` is a changed cell's value in the saved
/// file (shown with the difference when hovering it).
pub fn draw_map_edit(
    ui: &mut egui::Ui,
    t: &Tokens,
    img: &[u8],
    m: &MapDef,
    mut cursor: Option<&mut CellCursor>,
    changed: &dyn Fn(usize) -> bool,
    was: &dyn Fn(usize) -> Option<f64>,
) -> Option<MapOut> {
    let Some(vals) = map_values(img, m) else {
        ui.colored_label(t.crit, "This map runs past the end of the image.");
        return None;
    };
    let (rows, cols) = (m.rows.max(1) as usize, m.cols.max(1) as usize);
    let xs = axis_values(img, m.x_axis.as_ref(), cols);
    let ys = axis_values(img, m.y_axis.as_ref(), rows);
    let (lo, hi) = vals
        .iter()
        .fold((f64::MAX, f64::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    let span = if hi > lo { hi - lo } else { 1.0 };
    let cw = cell_w(&vals, &xs);
    let size = Vec2::new(
        AXIS_W + cols as f32 * (cw + GAP),
        (rows + 1) as f32 * (CELL_H + GAP),
    );

    let editable = cursor.is_some();
    let (resp, painter) = ui.allocate_painter(
        size,
        if editable {
            Sense::click_and_drag()
        } else {
            Sense::hover()
        },
    );
    let o = resp.rect.min;
    let clip = ui.clip_rect();
    let f_cell = font(MONO_MEDIUM, 10.5);
    let f_axis = FontId::monospace(10.0);
    let cell = |c: usize, r: usize| {
        Rect::from_min_size(
            Pos2::new(
                o.x + AXIS_W + c as f32 * (cw + GAP),
                o.y + (r + 1) as f32 * (CELL_H + GAP),
            ),
            Vec2::new(cw, CELL_H),
        )
    };
    // X axis header.
    for (c, x) in xs.iter().enumerate() {
        let r = Rect::from_min_size(
            Pos2::new(o.x + AXIS_W + c as f32 * (cw + GAP), o.y),
            Vec2::new(cw, CELL_H),
        );
        if clip.intersects(r) {
            painter.text(
                r.center(),
                Align2::CENTER_CENTER,
                fmt_val(*x),
                f_axis.clone(),
                t.fg_faint,
            );
        }
    }
    let at = |p: Pos2| {
        let c = ((p.x - o.x - AXIS_W) / (cw + GAP)).floor();
        let r = ((p.y - o.y) / (CELL_H + GAP)).floor() - 1.0;
        (c >= 0.0 && r >= 0.0 && (c as usize) < cols && (r as usize) < rows)
            .then_some((c as usize, r as usize))
    };
    let hover_cell = resp.hover_pos().and_then(at);

    // Editing input (before painting, so the frame shows the new state).
    let mut cmd = None;
    let mut sel = None;
    let mut range: Option<Sel> = None;
    if let Some(cur) = cursor.as_deref_mut() {
        let shift = ui.input(|i| i.modifiers.shift);
        if resp.drag_started() {
            let origin = ui.input(|i| i.pointer.press_origin());
            if let Some(p) = origin.and_then(at) {
                if !shift || cur.sel.is_none() {
                    cur.anchor = Some(p);
                } else if cur.anchor.is_none() {
                    cur.anchor = cur.sel;
                }
                cur.sel = Some(p);
                cur.text = None;
                resp.request_focus();
            }
        }
        if resp.dragged() {
            if let Some(p) = resp.interact_pointer_pos().and_then(at) {
                cur.sel = Some(p);
            }
        }
        if resp.clicked() || resp.double_clicked() {
            if let Some(p) = resp.interact_pointer_pos().and_then(at) {
                if cur.sel != Some(p) {
                    cur.text = None;
                }
                if shift && cur.sel.is_some() && !resp.double_clicked() {
                    // Extend the range from its anchor (or the active cell).
                    if cur.anchor.is_none() {
                        cur.anchor = cur.sel;
                    }
                } else {
                    cur.anchor = None;
                }
                cur.sel = Some(p);
                resp.request_focus();
                if resp.double_clicked() {
                    cur.text = Some(fmt_val(vals[p.1 * cols + p.0]));
                    cur.focus_text = true;
                    cur.select_all = true;
                }
            }
        }
        if let Some((c, r)) = cur.sel {
            if c >= cols || r >= rows {
                cur.sel = None;
            }
        }
        if let Some((c, r)) = cur.anchor {
            if c >= cols || r >= rows || cur.sel.is_none() {
                cur.anchor = None;
            }
        }
        if resp.has_focus() && cur.text.is_none() {
            ui.memory_mut(|mem| {
                mem.set_focus_lock_filter(
                    resp.id,
                    egui::EventFilter {
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                        ..Default::default()
                    },
                )
            });
            let k = nav_keys(ui);
            if k.select_all {
                cur.select_whole(cols, rows);
            }
            if let Some((c, r)) = cur.sel {
                let range = cur.range().unwrap_or(Sel::cell(c, r));
                if k.dx != 0 || k.dy != 0 {
                    let nc = (c as i32 + k.dx).clamp(0, cols as i32 - 1) as usize;
                    let nr = (r as i32 + k.dy).clamp(0, rows as i32 - 1) as usize;
                    if k.extend {
                        cur.anchor = cur.anchor.or(Some((c, r)));
                    } else {
                        cur.anchor = None;
                    }
                    cur.sel = Some((nc, nr));
                    cur.scroll_to_sel = true;
                }
                let i = r * cols + c;
                if k.copy {
                    cmd = Some(CellCommand::Copy(range));
                } else if let Some(text) = k.paste {
                    cmd = Some(CellCommand::Paste(range, text));
                } else if k.step != 0 && range.count() > 1 {
                    cmd = Some(CellCommand::Step(range, k.step));
                } else if k.step != 0 {
                    cmd = Some(CellCommand::Edit(i, crate::image::CellEdit::Step(k.step)));
                } else if k.edit {
                    cur.text = Some(fmt_val(vals[i]));
                    cur.focus_text = true;
                    cur.select_all = true;
                } else if let Some(ch) = k.ch {
                    cur.text = Some(ch.to_string());
                    cur.focus_text = true;
                    cur.select_all = false;
                }
                if k.cancel {
                    cur.sel = None;
                    cur.anchor = None;
                }
            } else if k.dx != 0 || k.dy != 0 {
                cur.sel = Some((0, 0));
            }
        }
        sel = cur.sel;
        range = cur.range().filter(|r| r.count() > 1);
    }

    for (rw, y) in ys.iter().enumerate() {
        let ar = Rect::from_min_size(
            Pos2::new(o.x, o.y + (rw + 1) as f32 * (CELL_H + GAP)),
            Vec2::new(AXIS_W - 6.0, CELL_H),
        );
        if clip.intersects(ar) {
            painter.text(
                ar.right_center(),
                Align2::RIGHT_CENTER,
                fmt_val(*y),
                f_axis.clone(),
                t.fg_dim,
            );
        }
        for c in 0..cols {
            let r = cell(c, rw);
            if !clip.intersects(r) {
                continue;
            }
            let v = vals[rw * cols + c];
            let k = ((v - lo) / span) as f32;
            let hot = hover_cell == Some((c, rw));
            let picked = sel == Some((c, rw));
            let rr = if hot || picked { r.expand(2.0) } else { r };
            painter.rect_filled(rr, CornerRadius::same(4), heat(k));
            if range.is_some_and(|s| s.contains(c, rw)) {
                // In the selected range: a tint and a thin accent frame.
                painter.rect(
                    rr,
                    CornerRadius::same(4),
                    t.accent.gamma_multiply(0.30),
                    Stroke::new(1.5, t.accent),
                    egui::StrokeKind::Inside,
                );
            }
            if hot || picked {
                painter.rect_stroke(
                    rr,
                    CornerRadius::same(4),
                    Stroke::new(2.0, if picked { t.accent } else { t.fg }),
                    egui::StrokeKind::Inside,
                );
            }
            if changed(rw * cols + c) {
                // Unsaved edit: a small marker in the top-right corner.
                let p = rr.right_top() + Vec2::new(-5.0, 5.0);
                painter.circle(p, 3.0, t.fg, Stroke::new(1.0, heat(k)));
            }
            painter.text(
                rr.center(),
                Align2::CENTER_CENTER,
                fmt_val(v),
                f_cell.clone(),
                text_on(k),
            );
        }
    }
    // The text box over the selected cell.
    if let Some(cur) = cursor {
        if let (Some((c, rw)), true) = (cur.sel, cur.scroll_to_sel) {
            ui.scroll_to_rect(cell(c, rw).expand(CELL_H), None);
            cur.scroll_to_sel = false;
        }
        if let (Some((c, rw)), Some(text)) = (cur.sel, cur.text.as_mut()) {
            let r = cell(c, rw).expand(2.0);
            let te = ui.put(
                r,
                egui::TextEdit::singleline(text)
                    .font(font(MONO_MEDIUM, 10.5))
                    .align(Align2::CENTER_CENTER)
                    .margin(Margin::symmetric(2, 2))
                    .desired_width(r.width()),
            );
            if std::mem::take(&mut cur.focus_text) {
                te.request_focus();
                if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), te.id) {
                    let n = text.chars().count();
                    let from = if cur.select_all { 0 } else { n };
                    let all = egui::text_selection::CCursorRange::two(
                        egui::text::CCursor::new(from),
                        egui::text::CCursor::new(n),
                    );
                    st.cursor.set_char_range(Some(all));
                    st.store(ui.ctx(), te.id);
                }
            }
            if te.lost_focus() {
                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                if enter {
                    let i = rw * cols + c;
                    cmd = Some(match parse_value(text) {
                        Some(v) => CellCommand::Edit(i, crate::image::CellEdit::Set(v)),
                        None => CellCommand::Invalid(text.clone()),
                    });
                    // Excel-like: move down after Enter.
                    if rw + 1 < rows {
                        cur.sel = Some((c, rw + 1));
                    }
                    cur.anchor = None;
                }
                cur.text = None;
                // Keep the keyboard on the grid.
                resp.request_focus();
            }
        }
    }
    let range = Some(MapOut { lo, hi, cmd });
    if let Some((c, rw)) = hover_cell {
        let i = rw * cols + c;
        let addr = m.address as usize + i * m.data_type.byte_len() as usize;
        let x_name = m
            .x_axis
            .as_ref()
            .map(|a| a.name.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("x");
        let y_name = m
            .y_axis
            .as_ref()
            .map(|a| a.name.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("y");
        resp.on_hover_ui_at_pointer(|ui| {
            ui.label(
                RichText::new(format!("{} {}", fmt_val(vals[i]), m.unit))
                    .font(font(crate::theme::DISPLAY, 15.0))
                    .color(t.accent),
            );
            if let Some(w) = was(i).filter(|w| *w != vals[i]) {
                let d = vals[i] - w;
                ui.label(
                    RichText::new(format!(
                        "was {} · {}{}",
                        fmt_val(w),
                        if d > 0.0 { "+" } else { "−" },
                        fmt_val(d.abs())
                    ))
                    .font(FontId::monospace(11.5))
                    .color(t.warn),
                );
            }
            ui.label(
                RichText::new(format!(
                    "{x_name} {} · {y_name} {}",
                    fmt_val(xs[c]),
                    fmt_val(ys[rw])
                ))
                .font(FontId::monospace(11.5))
                .color(t.fg_dim),
            );
            ui.label(
                RichText::new(format!("row {rw} col {c} · 0x{addr:06X}"))
                    .font(FontId::monospace(11.0))
                    .color(t.fg_faint),
            );
        });
    }
    range
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramp_ends_and_text_contrast() {
        assert_eq!(heat(0.0), Color32::from_rgb(43, 58, 103));
        assert_eq!(heat(1.0), Color32::from_rgb(215, 72, 63));
        assert_eq!(heat(-3.0), heat(0.0));
        assert_eq!(text_on(0.9), Color32::from_rgb(0x0c, 0x0f, 0x16));
        assert_eq!(text_on(0.1), Color32::from_rgb(0xee, 0xf1, 0xf7));
        assert!(cell_w(&[1234.56], &[]) > cell_w(&[1.0], &[]));
    }

    /// Drive the editable grid headless: click a cell, type, Enter, step.
    #[test]
    fn click_type_enter_and_step_produce_edits() {
        use crate::image::CellEdit;
        use egui::{Event, Key, Modifiers, PointerButton, RawInput};
        let ctx = egui::Context::default();
        crate::theme::setup(&ctx, crate::theme::ThemeMode::Dark);
        let m = MapDef {
            name: "m".into(),
            rows: 2,
            cols: 2,
            data_type: opentune_core::ecudef::DataType::U8,
            ..Default::default()
        };
        let img = [1u8, 2, 3, 4];
        let mut cur = CellCursor::default();
        cur.follow(&m);
        let origin = std::cell::Cell::new(Pos2::ZERO);
        let frame = |events: Vec<Event>, cur: &mut CellCursor| {
            let mut cmd = None;
            let input = RawInput {
                events,
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                origin.set(ui.next_widget_position());
                cmd = draw_map_ex(ui, &crate::theme::DARK, &img, &m, Some(cur), &|_| false)
                    .and_then(|o| o.cmd);
            });
            out.textures_delta.clear();
            cmd
        };
        frame(vec![], &mut cur);
        // Cell (1, 0): second column, first row.
        let p = origin.get() + Vec2::new(AXIS_W + 34.0 + GAP + 8.0, CELL_H + GAP + 8.0);
        let btn = |pressed| Event::PointerButton {
            pos: p,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        frame(vec![Event::PointerMoved(p), btn(true)], &mut cur);
        frame(vec![btn(false)], &mut cur);
        assert_eq!(cur.sel, Some((1, 0)));
        let key = |k| Event::Key {
            key: k,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        // `+` steps the selected cell.
        assert_eq!(
            frame(vec![Event::Text("+".into())], &mut cur),
            Some(CellCommand::Edit(1, CellEdit::Step(1)))
        );
        // A digit opens the text box with it; more typing appends; Enter commits.
        frame(vec![Event::Text("4".into())], &mut cur);
        assert_eq!(cur.text.as_deref(), Some("4"));
        frame(vec![], &mut cur);
        frame(vec![Event::Text("2".into())], &mut cur);
        assert_eq!(cur.text.as_deref(), Some("42"));
        let cmd = frame(vec![key(Key::Enter)], &mut cur);
        assert_eq!(cmd, Some(CellCommand::Edit(1, CellEdit::Set(42.0))));
        assert_eq!(cur.text, None);
        assert_eq!(cur.sel, Some((1, 1)), "Enter moves down");
        // Esc in the text box cancels without an edit.
        frame(vec![key(Key::F2)], &mut cur);
        assert!(cur.text.is_some());
        frame(vec![], &mut cur);
        assert_eq!(frame(vec![key(Key::Escape)], &mut cur), None);
        assert_eq!(cur.text, None);
    }

    /// Ranges: shift-click, drag, Shift+arrows, Ctrl+A, and the range
    /// commands (step all, copy, paste).
    #[test]
    fn ranges_by_shift_click_drag_keys_and_clipboard() {
        use egui::{Event, Key, Modifiers, PointerButton, RawInput};
        let ctx = egui::Context::default();
        crate::theme::setup(&ctx, crate::theme::ThemeMode::Dark);
        let m = MapDef {
            name: "m".into(),
            rows: 3,
            cols: 3,
            data_type: opentune_core::ecudef::DataType::U8,
            ..Default::default()
        };
        let img = [1u8, 2, 3, 4, 5, 6, 7, 8, 9];
        let mut cur = CellCursor::default();
        cur.follow(&m);
        let origin = std::cell::Cell::new(Pos2::ZERO);
        let frame = |events: Vec<Event>, mods: Modifiers, cur: &mut CellCursor| {
            let mut cmd = None;
            let mut events = events;
            events.insert(0, Event::ModifiersChanged(mods));
            let input = RawInput {
                events,
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                origin.set(ui.next_widget_position());
                cmd = draw_map_ex(ui, &crate::theme::DARK, &img, &m, Some(cur), &|_| false)
                    .and_then(|o| o.cmd);
            });
            out.textures_delta.clear();
            cmd
        };
        frame(vec![], Modifiers::NONE, &mut cur);
        let cw = 34.0;
        let at = |c: usize, r: usize| {
            origin.get()
                + Vec2::new(
                    AXIS_W + c as f32 * (cw + GAP) + 8.0,
                    (r + 1) as f32 * (CELL_H + GAP) + 8.0,
                )
        };
        let btn = |p, pressed, modifiers| Event::PointerButton {
            pos: p,
            button: PointerButton::Primary,
            pressed,
            modifiers,
        };
        let click = |p: Pos2, mods: Modifiers, cur: &mut CellCursor| {
            frame(vec![Event::PointerMoved(p), btn(p, true, mods)], mods, cur);
            frame(vec![btn(p, false, mods)], mods, cur);
        };
        // Click (0,0), shift-click (1,2): a 2 × 3 range, active cell (1,2).
        click(at(0, 0), Modifiers::NONE, &mut cur);
        assert_eq!(cur.range(), Some(Sel::cell(0, 0)));
        click(at(1, 2), Modifiers::SHIFT, &mut cur);
        assert_eq!(cur.sel, Some((1, 2)));
        assert_eq!(cur.range(), Some(Sel::span((0, 0), (1, 2))));
        // `+` steps every cell of the range at once.
        assert_eq!(
            frame(vec![Event::Text("+".into())], Modifiers::NONE, &mut cur),
            Some(CellCommand::Step(Sel::span((0, 0), (1, 2)), 1))
        );
        // Copy and paste name the range.
        assert_eq!(
            frame(vec![Event::Copy], Modifiers::NONE, &mut cur),
            Some(CellCommand::Copy(Sel::span((0, 0), (1, 2))))
        );
        assert_eq!(
            frame(vec![Event::Paste("1\t2".into())], Modifiers::NONE, &mut cur),
            Some(CellCommand::Paste(Sel::span((0, 0), (1, 2)), "1\t2".into()))
        );
        // A plain arrow collapses the range; Shift+arrow grows it again.
        let key = |k, modifiers| Event::Key {
            key: k,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        frame(
            vec![key(Key::ArrowUp, Modifiers::NONE)],
            Modifiers::NONE,
            &mut cur,
        );
        assert_eq!(cur.range(), Some(Sel::cell(1, 1)));
        frame(
            vec![key(Key::ArrowRight, Modifiers::SHIFT)],
            Modifiers::SHIFT,
            &mut cur,
        );
        assert_eq!(cur.range(), Some(Sel::span((1, 1), (2, 1))));
        // Ctrl+A: the whole map.
        frame(
            vec![key(Key::A, Modifiers::COMMAND)],
            Modifiers::COMMAND,
            &mut cur,
        );
        assert_eq!(cur.range(), Some(Sel::all(3, 3)));
        // Drag from (2,0) to (0,1).
        let (a, b) = (at(2, 0), at(0, 1));
        frame(
            vec![Event::PointerMoved(a), btn(a, true, Modifiers::NONE)],
            Modifiers::NONE,
            &mut cur,
        );
        for k in 1..=6 {
            let p = a + (b - a) * (k as f32 / 6.0);
            frame(vec![Event::PointerMoved(p)], Modifiers::NONE, &mut cur);
        }
        frame(
            vec![btn(b, false, Modifiers::NONE)],
            Modifiers::NONE,
            &mut cur,
        );
        assert_eq!(cur.range(), Some(Sel::span((0, 0), (2, 1))));
        assert_eq!(cur.sel, Some((0, 1)), "the active cell follows the pointer");
        // Esc clears the selection.
        frame(
            vec![key(Key::Escape, Modifiers::NONE)],
            Modifiers::NONE,
            &mut cur,
        );
        assert_eq!(cur.range(), None);
    }

    #[test]
    fn typed_values_and_cursor_reset() {
        assert_eq!(parse_value(" 12,5 "), Some(12.5));
        assert_eq!(parse_value("-3"), Some(-3.0));
        assert_eq!(parse_value("abc"), None);
        assert_eq!(parse_value("inf"), None);
        let mut cur = CellCursor::default();
        let m = MapDef {
            address: 0x10,
            ..Default::default()
        };
        cur.follow(&m);
        cur.select(5, 4);
        assert_eq!(cur.sel, Some((1, 1)));
        cur.follow(&m);
        assert_eq!(cur.sel, Some((1, 1)), "same map keeps the cursor");
        cur.follow(&MapDef {
            address: 0x20,
            ..Default::default()
        });
        assert_eq!(cur.sel, None);
    }
}
