//! Map canvas: a calibration table painted directly (rounded cells + text),
//! heat-coloured between the map's own min and max with the classic
//! blue→red ramp of the design spec. Cells outside the clip rectangle are
//! skipped, so a long stack of maps costs only what is visible.

use crate::image::{axis_values, fmt_val, map_values};
use crate::theme::{font, Tokens, MONO_MEDIUM};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2,
};
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

/// Paint one map. Returns the value range, or `None` when the map runs past
/// the end of the image.
pub fn draw_map(ui: &mut egui::Ui, t: &Tokens, img: &[u8], m: &MapDef) -> Option<(f64, f64)> {
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

    let (resp, painter) = ui.allocate_painter(size, Sense::hover());
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
    let hover_cell = resp.hover_pos().and_then(|p| {
        let c = ((p.x - o.x - AXIS_W) / (cw + GAP)).floor();
        let r = ((p.y - o.y) / (CELL_H + GAP)).floor() - 1.0;
        (c >= 0.0 && r >= 0.0 && (c as usize) < cols && (r as usize) < rows)
            .then_some((c as usize, r as usize))
    });
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
            let rr = if hot { r.expand(2.0) } else { r };
            painter.rect_filled(rr, CornerRadius::same(4), heat(k));
            if hot {
                painter.rect_stroke(
                    rr,
                    CornerRadius::same(4),
                    Stroke::new(2.0, t.fg),
                    egui::StrokeKind::Inside,
                );
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
    Some((lo, hi))
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
}
