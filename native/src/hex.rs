//! Virtualised hex view: only the visible rows are laid out each frame, so a
//! 1 MiB (65 536-row) image costs the same as a 4 KiB one.

use crate::theme::Tokens;
use eframe::egui::{self, text::LayoutJob, Color32, FontId, TextFormat};
use std::fmt::Write;
use std::ops::Range;

pub const BYTES_PER_ROW: usize = 16;

pub struct HexView {
    /// Pending scroll target (points), applied on the next frame.
    pub scroll_to: Option<f32>,
    /// Pending jump target (byte address), converted with the measured row
    /// height inside `show`.
    jump_addr: Option<usize>,
    pub jump_text: String,
    pub row_height: f32,
}

impl Default for HexView {
    fn default() -> Self {
        Self {
            scroll_to: None,
            jump_addr: None,
            jump_text: String::new(),
            row_height: 19.0,
        }
    }
}

/// One row as plain text: offset, 16 hex bytes, ASCII (tests, copy).
#[allow(dead_code)]
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

/// One row as a styled layout job: faint offset, bytes, dim ASCII; bytes of
/// `hl` in accent on the selection colour.
fn row_job(
    t: &Tokens,
    bytes: &[u8],
    row: usize,
    hl: Option<&Range<usize>>,
    font: &FontId,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    let fmt = |c: Color32, bg: Color32| TextFormat {
        font_id: font.clone(),
        color: c,
        background: bg,
        ..Default::default()
    };
    let start = row * BYTES_PER_ROW;
    let chunk = bytes
        .get(start..(start + BYTES_PER_ROW).min(bytes.len()))
        .unwrap_or(&[]);
    job.append(
        &format!("{start:08X}"),
        0.0,
        fmt(t.fg_faint, Color32::TRANSPARENT),
    );
    job.append("   ", 0.0, fmt(t.fg, Color32::TRANSPARENT));
    let on = |i: usize| hl.is_some_and(|h| h.contains(&(start + i)));
    let mut s = String::with_capacity(3);
    for i in 0..BYTES_PER_ROW {
        s.clear();
        match chunk.get(i) {
            Some(b) => {
                let _ = write!(s, "{b:02X}");
            }
            None => s.push_str("  "),
        }
        let (c, bg) = if on(i) {
            (t.accent, t.sel)
        } else {
            (t.fg, Color32::TRANSPARENT)
        };
        job.append(&s, 0.0, fmt(c, bg));
        job.append(
            if i == 7 { "  " } else { " " },
            0.0,
            fmt(t.fg, Color32::TRANSPARENT),
        );
    }
    job.append("  ", 0.0, fmt(t.fg, Color32::TRANSPARENT));
    for (i, &b) in chunk.iter().enumerate() {
        let ch = if (0x20..0x7F).contains(&b) {
            b as char
        } else {
            '.'
        };
        let (c, bg) = if on(i) {
            (t.accent, t.sel)
        } else {
            (t.fg_dim, Color32::TRANSPARENT)
        };
        let mut tmp = [0u8; 4];
        job.append(ch.encode_utf8(&mut tmp), 0.0, fmt(c, bg));
    }
    job
}

impl HexView {
    /// Draw the rows (no header). `highlight` marks a byte range (the selected
    /// map). Returns the scroll offset after this frame.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        t: &Tokens,
        bytes: &[u8],
        highlight: Option<Range<usize>>,
    ) -> f32 {
        let rows = bytes.len().div_ceil(BYTES_PER_ROW);
        let font = FontId::monospace(12.5);
        let text_h = ui.fonts_mut(|f| f.row_height(&font)) + 3.0;
        self.row_height = text_h + ui.spacing().item_spacing.y;
        if let Some(a) = self.jump_addr.take() {
            self.scroll_to = Some((a / BYTES_PER_ROW) as f32 * self.row_height);
        }
        let mut area = egui::ScrollArea::vertical()
            .id_salt("hex")
            .auto_shrink([false, false]);
        if let Some(y) = self.scroll_to.take() {
            area = area.vertical_scroll_offset(y);
        }
        let out = area.show_rows(ui, text_h, rows, |ui, range| {
            for row in range {
                let job = row_job(t, bytes, row, highlight.as_ref(), &font);
                ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend));
            }
        });
        out.state.clamped_offset().y
    }

    /// Scroll so that `addr` is the top row.
    pub fn jump(&mut self, addr: usize) {
        self.jump_addr = Some(addr);
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

    #[test]
    fn highlighted_bytes_get_the_selection_background() {
        let t = crate::theme::DARK;
        let bytes = vec![0x41u8; 32];
        let job = row_job(&t, &bytes, 0, Some(&(2..4)), &FontId::monospace(12.0));
        // egui merges neighbouring sections with the same format, so count
        // highlighted characters: 2 hex bytes (4 chars) + 2 ASCII chars.
        let hl: usize = job
            .sections
            .iter()
            .filter(|s| s.format.background == t.sel)
            .map(|s| job.text[s.byte_range.start.0..s.byte_range.end.0].len())
            .sum();
        assert_eq!(hl, 6);
    }
}
