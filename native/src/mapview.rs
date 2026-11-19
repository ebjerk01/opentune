//! Map canvas: a calibration table painted directly (cell rectangles + text),
//! heat-coloured between the map's own min and max. Cells outside the clip
//! rectangle are skipped, so a long stack of maps costs only what is visible.

use crate::image::{axis_values, fmt_val, map_values};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use opentune_core::ecudef::MapDef;

pub const CELL_W: f32 = 54.0;
pub const CELL_H: f32 = 20.0;

/// Blue (low) → cyan → yellow → red (high), for `t` in 0..=1.
pub fn heat(t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let stops = [
        (40.0, 90.0, 200.0),
        (40.0, 180.0, 190.0),
        (230.0, 210.0, 60.0),
        (215.0, 60.0, 50.0),
    ];
    let x = t * (stops.len() - 1) as f32;
    let i = (x.floor() as usize).min(stops.len() - 2);
    let f = x - i as f32;
    let (a, b) = (stops[i], stops[i + 1]);
    let l = |p: f32, q: f32| (p + (q - p) * f).round() as u8;
    Color32::from_rgb(l(a.0, b.0), l(a.1, b.1), l(a.2, b.2))
}

fn text_on(bg: Color32) -> Color32 {
    let lum = 0.299 * bg.r() as f32 + 0.587 * bg.g() as f32 + 0.114 * bg.b() as f32;
    if lum > 140.0 {
        Color32::BLACK
    } else {
        Color32::WHITE
    }
}

/// Size the canvas of `m` needs (axis header row + column included).
pub fn canvas_size(m: &MapDef) -> Vec2 {
    Vec2::new(
        (m.cols.max(1) + 1) as f32 * CELL_W,
        (m.rows.max(1) + 1) as f32 * CELL_H,
    )
}

/// Paint one map. Returns false when the map runs past the end of the image.
pub fn draw_map(ui: &mut egui::Ui, img: &[u8], m: &MapDef) -> bool {
    let Some(vals) = map_values(img, m) else {
        ui.colored_label(Color32::LIGHT_RED, "map runs past the end of this image");
        return false;
    };
    let (rows, cols) = (m.rows.max(1) as usize, m.cols.max(1) as usize);
    let xs = axis_values(img, m.x_axis.as_ref(), cols);
    let ys = axis_values(img, m.y_axis.as_ref(), rows);
    let (lo, hi) = vals
        .iter()
        .fold((f64::MAX, f64::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    let span = if hi > lo { hi - lo } else { 1.0 };

    let (resp, painter) = ui.allocate_painter(canvas_size(m), Sense::hover());
    let origin = resp.rect.min;
    let clip = ui.clip_rect();
    let font = FontId::monospace(11.0);
    let axis_bg = ui.visuals().faint_bg_color;
    let axis_fg = ui.visuals().strong_text_color();
    let grid = Stroke::new(1.0, ui.visuals().window_stroke.color);
    let cell = |c: usize, r: usize| {
        Rect::from_min_size(
            Pos2::new(origin.x + c as f32 * CELL_W, origin.y + r as f32 * CELL_H),
            Vec2::new(CELL_W, CELL_H),
        )
    };
    // Header row (x axis) and column (y axis).
    for (c, x) in xs.iter().enumerate() {
        let r = cell(c + 1, 0);
        if clip.intersects(r) {
            painter.rect_filled(r, 0.0, axis_bg);
            painter.text(
                r.center(),
                Align2::CENTER_CENTER,
                fmt_val(*x),
                font.clone(),
                axis_fg,
            );
        }
    }
    for (rw, y) in ys.iter().enumerate() {
        let r = cell(0, rw + 1);
        if clip.intersects(r) {
            painter.rect_filled(r, 0.0, axis_bg);
            painter.text(
                r.center(),
                Align2::CENTER_CENTER,
                fmt_val(*y),
                font.clone(),
                axis_fg,
            );
        }
    }
    for rw in 0..rows {
        for c in 0..cols {
            let r = cell(c + 1, rw + 1);
            if !clip.intersects(r) {
                continue;
            }
            let v = vals[rw * cols + c];
            let bg = heat(((v - lo) / span) as f32);
            painter.rect_filled(r, 0.0, bg);
            painter.rect_stroke(r, 0.0, grid, egui::StrokeKind::Inside);
            painter.text(
                r.center(),
                Align2::CENTER_CENTER,
                fmt_val(v),
                font.clone(),
                text_on(bg),
            );
        }
    }
    if let Some(p) = resp.hover_pos() {
        let c = ((p.x - origin.x) / CELL_W) as usize;
        let rw = ((p.y - origin.y) / CELL_H) as usize;
        if c >= 1 && rw >= 1 && c <= cols && rw <= rows {
            let i = (rw - 1) * cols + (c - 1);
            let addr = m.address as usize + i * m.data_type.byte_len() as usize;
            resp.on_hover_text(format!(
                "row {} col {}: {} {}\nx {} / y {}\naddress 0x{addr:06X}",
                rw - 1,
                c - 1,
                fmt_val(vals[i]),
                m.unit,
                fmt_val(xs[c - 1]),
                fmt_val(ys[rw - 1])
            ));
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heat_ends_and_text_contrast() {
        assert_eq!(heat(0.0), Color32::from_rgb(40, 90, 200));
        assert_eq!(heat(1.0), Color32::from_rgb(215, 60, 50));
        assert_eq!(heat(-3.0), heat(0.0));
        assert_eq!(text_on(Color32::from_rgb(230, 210, 60)), Color32::BLACK);
        assert_eq!(text_on(Color32::from_rgb(40, 90, 200)), Color32::WHITE);
    }
}
