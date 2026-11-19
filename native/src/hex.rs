//! Virtualised hex view: only the visible rows are laid out each frame, so a
//! 1 MiB (65 536-row) image costs the same as a 4 KiB one.

use eframe::egui::{self, Color32, RichText};
use std::fmt::Write;
use std::ops::Range;

pub const BYTES_PER_ROW: usize = 16;

pub struct HexView {
    /// Pending scroll target (points), applied on the next frame.
    pub scroll_to: Option<f32>,
    pub jump_text: String,
    pub row_height: f32,
    line: String,
}

impl Default for HexView {
    fn default() -> Self {
        Self {
            scroll_to: None,
            jump_text: String::new(),
            row_height: 16.0,
            line: String::new(),
        }
    }
}

/// One row as text: offset, 16 hex bytes, ASCII.
pub fn format_row(out: &mut String, bytes: &[u8], row: usize) {
    out.clear();
    let start = row * BYTES_PER_ROW;
    let chunk = bytes
        .get(start..(start + BYTES_PER_ROW).min(bytes.len()))
        .unwrap_or(&[]);
    let _ = write!(out, "{start:08X}  ");
    for i in 0..BYTES_PER_ROW {
        match chunk.get(i) {
            Some(b) => {
                let _ = write!(out, "{b:02X} ");
            }
            None => out.push_str("   "),
        }
        if i == 7 {
            out.push(' ');
        }
    }
    out.push(' ');
    for &b in chunk {
        out.push(if (0x20..0x7F).contains(&b) {
            b as char
        } else {
            '.'
        });
    }
}

/// Parse a jump target: `0x1234`, `1234h` or plain hex.
pub fn parse_addr(s: &str) -> Option<usize> {
    let t = s.trim();
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    let t = t
        .strip_suffix('h')
        .or_else(|| t.strip_suffix('H'))
        .unwrap_or(t);
    usize::from_str_radix(t, 16).ok()
}

impl HexView {
    /// Draw the view. `highlight` marks a byte range (e.g. the selected map).
    /// Returns the scroll offset after this frame.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        bytes: &[u8],
        highlight: Option<Range<usize>>,
    ) -> f32 {
        ui.horizontal(|ui| {
            ui.label("Go to");
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.jump_text)
                    .desired_width(90.0)
                    .hint_text("0x1C000"),
            );
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                if let Some(a) = parse_addr(&self.jump_text) {
                    self.scroll_to = Some((a / BYTES_PER_ROW) as f32 * self.row_height);
                }
            }
            ui.label(format!(
                "{} bytes, {} rows",
                bytes.len(),
                bytes.len().div_ceil(BYTES_PER_ROW)
            ));
        });
        ui.separator();
        let rows = bytes.len().div_ceil(BYTES_PER_ROW);
        self.row_height =
            ui.text_style_height(&egui::TextStyle::Monospace) + ui.spacing().item_spacing.y;
        let mut area = egui::ScrollArea::vertical()
            .id_salt("hex")
            .auto_shrink([false, false]);
        if let Some(y) = self.scroll_to.take() {
            area = area.vertical_scroll_offset(y);
        }
        let line = &mut self.line;
        let out = area.show_rows(
            ui,
            ui.text_style_height(&egui::TextStyle::Monospace),
            rows,
            |ui, range| {
                for row in range {
                    format_row(line, bytes, row);
                    let start = row * BYTES_PER_ROW;
                    let hit = highlight
                        .as_ref()
                        .is_some_and(|h| h.start < start + BYTES_PER_ROW && start < h.end);
                    let mut text = RichText::new(line.as_str()).monospace();
                    if hit {
                        text =
                            text.background_color(Color32::from_rgba_unmultiplied(255, 170, 0, 60));
                    }
                    ui.label(text);
                }
            },
        );
        out.state.clamped_offset().y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_text_and_partial_last_row() {
        let bytes: Vec<u8> = (0u8..20).map(|b| b + 0x40).collect();
        let mut s = String::new();
        format_row(&mut s, &bytes, 0);
        assert!(s.starts_with("00000000  40 41 42 43 44 45 46 47  48 "));
        assert!(s.ends_with("@ABCDEFGHIJKLMNO"));
        format_row(&mut s, &bytes, 1);
        assert!(s.starts_with("00000010  50 51 52 53 "));
        assert!(s.ends_with("PQRS"));
    }

    #[test]
    fn jump_targets() {
        assert_eq!(parse_addr("0x1C000"), Some(0x1C000));
        assert_eq!(parse_addr("1c000h"), Some(0x1C000));
        assert_eq!(parse_addr(" ff "), Some(0xFF));
        assert_eq!(parse_addr("zz"), None);
    }
}
