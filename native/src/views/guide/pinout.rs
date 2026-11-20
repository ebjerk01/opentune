//! The guide's ```` ```pinout <unit-id>``` ```` embeds: a data-driven bench
//! WIRING diagram of a database unit (`database/units`, via `opentune-db`),
//! drawn with the egui painter as the webview app's `pinout-svg.js` draws its
//! SVG: connector pins on the left, wired to the bench targets they need on
//! the right. It is not a picture of the connector face (the sources give pin
//! numbers and functions, not positions).
//!
//! A pin's confidence is its own, else the unit's `pinout_confidence`;
//! `unverified` pins are drawn dashed with a `?` (confirm by continuity).

use crate::theme::{font, Tokens, MONO_MEDIUM, UI_MEDIUM, UI_STRONG};
use eframe::egui::{
    self, epaint::CubicBezierShape, Align2, Color32, CornerRadius, Rect, Sense, Shape, Stroke,
    StrokeKind, Ui, Vec2,
};
use opentune_db::{Confidence, Signal, Unit};

/// Diagram width the geometry is laid out for (scaled down to fit).
const W: f32 = 760.0;
const LX: f32 = 12.0;
const LW: f32 = 340.0;
const RX: f32 = 540.0;
const RW: f32 = 208.0;
const ROW: f32 = 24.0;
const HEAD: f32 = 22.0;
const GAP: f32 = 14.0;
const TOP: f32 = 12.0;
const TH: f32 = 40.0;

/// Bench targets, top to bottom. `Other` gets no wire.
const TARGETS: [(Signal, &str, &str); 7] = [
    (
        Signal::V12Perm,
        "+12 V permanent (KL30)",
        "fused, current-limited supply",
    ),
    (
        Signal::V12Ign,
        "+12 V ignition (KL15)",
        "same supply, switched",
    ),
    (Signal::Gnd, "Ground (−)", "supply negative"),
    (Signal::Kline, "K-line", "to the interface"),
    (Signal::CanH, "CAN high", "to the interface"),
    (Signal::CanL, "CAN low", "to the interface"),
    (Signal::Boot, "Boot strap", "only for boot-mode reads"),
];

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PinRow {
    pub label: String,
    pub function: String,
    pub note: Option<String>,
    pub signal: Signal,
    pub unverified: bool,
    /// Distinct sources stating this pin.
    pub sources: usize,
    /// Row centre.
    pub y: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Block {
    pub title: String,
    pub y: f32,
    pub h: f32,
    pub pins: Vec<PinRow>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Target {
    pub signal: Signal,
    pub name: &'static str,
    pub sub: &'static str,
    /// Box top.
    pub y: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Diagram {
    pub blocks: Vec<Block>,
    pub targets: Vec<Target>,
    /// Where the legend starts.
    pub legend_y: f32,
    pub confidence: Confidence,
    pub any_unverified: bool,
}

impl Diagram {
    pub fn height(&self) -> f32 {
        self.legend_y + 50.0
    }
    fn target_centre(&self, s: Signal) -> Option<f32> {
        self.targets
            .iter()
            .find(|t| t.signal == s)
            .map(|t| t.y + TH / 2.0)
    }
}

/// The numeric part of a pin label (`S1-12` -> 12; none sorts last).
fn pin_num(s: &str) -> u32 {
    let digits: String = s
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().unwrap_or(u32::MAX)
}

/// Lay out the diagram for `unit`; `None` when it has no pins.
pub(crate) fn layout(unit: &Unit) -> Option<Diagram> {
    let conns: Vec<_> = unit
        .connectors
        .iter()
        .filter(|c| !c.pins.is_empty())
        .collect();
    if conns.is_empty() {
        return None;
    }
    let mut y = TOP;
    let mut blocks = Vec::new();
    for c in conns {
        let mut pins: Vec<_> = c.pins.iter().collect();
        pins.sort_by_key(|p| pin_num(&p.pin));
        let h = HEAD + pins.len() as f32 * ROW + 6.0;
        let rows = pins
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let mut src = p.sources.clone();
                src.sort_unstable();
                src.dedup();
                PinRow {
                    label: format!("{}-{}", c.name, p.pin),
                    function: p.function.clone(),
                    note: p.note.clone(),
                    signal: p.signal,
                    unverified: unit.pin_confidence(p) == Confidence::Unverified,
                    sources: src.len(),
                    y: y + HEAD + i as f32 * ROW + ROW / 2.0,
                }
            })
            .collect();
        let title = match &c.description {
            Some(d) => format!("Connector {} — {d}", c.name),
            None => format!("Connector {}", c.name),
        };
        blocks.push(Block {
            title,
            y,
            h,
            pins: rows,
        });
        y += h + GAP;
    }
    let left_h = y - GAP;
    let used: Vec<_> = TARGETS
        .iter()
        .filter(|(s, _, _)| blocks.iter().flat_map(|b| &b.pins).any(|p| p.signal == *s))
        .collect();
    let span = (left_h - TOP).max(used.len() as f32 * (TH + 10.0));
    let step = if used.is_empty() {
        0.0
    } else {
        span / used.len() as f32
    };
    let targets = used
        .iter()
        .enumerate()
        .map(|(i, (s, name, sub))| Target {
            signal: *s,
            name,
            sub,
            y: TOP + i as f32 * step + (step - TH) / 2.0,
        })
        .collect();
    let any_unverified = blocks.iter().flat_map(|b| &b.pins).any(|p| p.unverified);
    Some(Diagram {
        blocks,
        targets,
        legend_y: left_h.max(TOP + span) + 18.0,
        confidence: unit.pinout_confidence.unwrap_or(Confidence::Unverified),
        any_unverified,
    })
}

fn signal_color(t: &Tokens, s: Signal) -> Color32 {
    match s {
        Signal::V12Perm => t.crit,
        Signal::V12Ign => t.warn,
        Signal::Gnd => t.fg_dim,
        Signal::Kline => t.ok,
        Signal::CanH => t.accent,
        Signal::CanL => Color32::from_rgb(0x3b, 0x6f, 0xd8),
        Signal::Boot => Color32::from_rgb(0x8a, 0x4f, 0xd1),
        Signal::Other => t.fg_faint,
    }
}

fn conf_word(c: Confidence) -> &'static str {
    match c {
        Confidence::Verified => "sources agree",
        Confidence::Community => "one source",
        Confidence::Unverified => "disputed — confirm by continuity",
    }
}

/// Shorten `s` to at most `max` characters with an ellipsis.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut o: String = s.chars().take(max.saturating_sub(1)).collect();
        o.push('…');
        o
    }
}

/// Draw the wiring diagram of `unit` (or say why there is none).
pub(crate) fn show(ui: &mut Ui, t: &Tokens, unit: &Unit) {
    let Some(d) = layout(unit) else {
        ui.label(
            egui::RichText::new(format!("{} has no sourced pinout.", unit.id)).color(t.fg_dim),
        );
        return;
    };
    let scale = (ui.available_width() / W).clamp(0.5, 1.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(W, d.height()) * scale, Sense::hover());
    let p = ui.painter_at(rect);
    let at = |x: f32, y: f32| rect.min + Vec2::new(x, y) * scale;
    let r = |x: f32, y: f32, w: f32, h: f32| Rect::from_min_size(at(x, y), Vec2::new(w, h) * scale);
    let fs = |s: f32| s * scale;
    let box_fill = t.surface2;
    let box_line = Stroke::new(1.0, t.border);
    let label_cols = d
        .blocks
        .iter()
        .flat_map(|b| &b.pins)
        .map(|p| p.label.chars().count())
        .max()
        .unwrap_or(4) as f32;
    let fx = LX + 10.0 + label_cols * 7.0 + 14.0;
    let fn_max = (((LX + LW - 64.0 - fx) / 6.1).floor() as usize).max(12);
    let mut hover: Option<String> = None;
    let pointer = resp.hover_pos();

    // Wires first, under the boxes' end dots.
    for pr in d.blocks.iter().flat_map(|b| &b.pins) {
        let Some(y2) = d.target_centre(pr.signal) else {
            continue;
        };
        let (x1, x2) = (LX + LW + 4.0, RX);
        let mx = (x1 + x2) / 2.0;
        let stroke = Stroke::new(2.2 * scale, signal_color(t, pr.signal));
        let bez = CubicBezierShape::from_points_stroke(
            [at(x1, pr.y), at(mx, pr.y), at(mx, y2), at(x2, y2)],
            false,
            Color32::TRANSPARENT,
            stroke,
        );
        if pr.unverified {
            let pts = bez.flatten(Some(0.5));
            p.extend(Shape::dashed_line(&pts, stroke, 6.0 * scale, 4.0 * scale));
        } else {
            p.add(bez);
        }
    }
    for b in &d.blocks {
        p.rect(
            r(LX, b.y, LW, b.h),
            CornerRadius::same(7),
            box_fill,
            box_line,
            StrokeKind::Inside,
        );
        p.text(
            at(LX + 10.0, b.y + 11.0),
            Align2::LEFT_CENTER,
            clip(&b.title, 52),
            font(UI_STRONG, fs(12.0)),
            t.fg,
        );
        for (i, pr) in b.pins.iter().enumerate() {
            let row = r(LX + 1.0, pr.y - ROW / 2.0 + 1.0, LW - 2.0, ROW - 2.0);
            if pr.unverified {
                p.rect_filled(row, CornerRadius::same(4), t.crit_soft);
            } else if i % 2 == 1 {
                p.rect_filled(row, CornerRadius::same(4), t.surface3);
            }
            p.text(
                at(LX + 10.0, pr.y),
                Align2::LEFT_CENTER,
                &pr.label,
                font(MONO_MEDIUM, fs(11.0)),
                t.fg,
            );
            p.text(
                at(fx, pr.y),
                Align2::LEFT_CENTER,
                clip(&pr.function, fn_max),
                font(UI_MEDIUM, fs(11.0)),
                t.fg_dim,
            );
            if pr.unverified {
                p.text(
                    at(LX + LW - 18.0, pr.y),
                    Align2::LEFT_CENTER,
                    "?",
                    font(UI_STRONG, fs(11.0)),
                    t.crit,
                );
            } else if pr.sources >= 2 {
                p.text(
                    at(LX + LW - 14.0, pr.y),
                    Align2::RIGHT_CENTER,
                    format!("{} src ✓", pr.sources),
                    font(UI_MEDIUM, fs(11.0)),
                    t.fg_dim,
                );
            }
            p.circle_filled(at(LX + LW, pr.y), 4.0 * scale, signal_color(t, pr.signal));
            if pointer.is_some_and(|q| row.contains(q)) {
                let mut h = format!("{}: {}", pr.label, pr.function);
                if let Some(n) = &pr.note {
                    h.push_str(&format!(" — {n}"));
                }
                if pr.unverified {
                    h.push_str(" (disputed, confirm by continuity)");
                }
                hover = Some(h);
            }
        }
    }
    for tg in &d.targets {
        p.rect(
            r(RX, tg.y, RW, TH),
            CornerRadius::same(7),
            box_fill,
            box_line,
            StrokeKind::Inside,
        );
        p.rect_filled(
            r(RX, tg.y, 5.0, TH),
            CornerRadius::same(2),
            signal_color(t, tg.signal),
        );
        p.text(
            at(RX + 14.0, tg.y + 13.0),
            Align2::LEFT_CENTER,
            tg.name,
            font(UI_STRONG, fs(12.0)),
            t.fg,
        );
        p.text(
            at(RX + 14.0, tg.y + 29.0),
            Align2::LEFT_CENTER,
            tg.sub,
            font(UI_MEDIUM, fs(11.0)),
            t.fg_dim,
        );
    }
    // Legend.
    let ly = d.legend_y;
    p.line_segment(
        [at(LX, ly), at(LX + 30.0, ly)],
        Stroke::new(2.2 * scale, t.fg_dim),
    );
    p.text(
        at(LX + 38.0, ly),
        Align2::LEFT_CENTER,
        format!(
            "pinout {} ({})",
            d.confidence.as_str(),
            conf_word(d.confidence)
        ),
        font(UI_MEDIUM, fs(11.0)),
        t.fg_dim,
    );
    if d.any_unverified {
        p.extend(Shape::dashed_line(
            &[at(LX + 300.0, ly), at(LX + 330.0, ly)],
            Stroke::new(2.2 * scale, t.fg_dim),
            6.0 * scale,
            4.0 * scale,
        ));
        p.text(
            at(LX + 338.0, ly),
            Align2::LEFT_CENTER,
            "? disputed — confirm by continuity before wiring",
            font(UI_MEDIUM, fs(11.0)),
            t.crit,
        );
    }
    p.text(
        at(LX, ly + 20.0),
        Align2::LEFT_CENTER,
        "\"2 src ✓\" = two sources agree on that pin; \"verified\" means sources agree, not that this unit was measured.",
        font(UI_MEDIUM, fs(11.0)),
        t.fg_dim,
    );
    p.text(
        at(LX, ly + 36.0),
        Align2::LEFT_CENTER,
        "Check every pin with a multimeter on your unit before applying power.",
        font(UI_MEDIUM, fs(11.0)),
        t.fg_dim,
    );
    if let Some(h) = hover {
        resp.on_hover_text(h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(pins: serde_json::Value, conf: &str) -> Unit {
        serde_json::from_value(serde_json::json!({
            "id": "synthetic-unit", "kind": "ecu", "maker": "Acme", "family": "SYN1",
            "name": "Synthetic", "cpu": null, "flash_kib": null, "eeprom": null,
            "bus": ["can"], "protocols": ["uds"],
            "read_methods": {"obd": "read", "bench": false, "boot": false, "notes": ""},
            "checksum": null, "ecudef": null, "vehicles": [],
            "part_number_examples": [], "notes": "",
            "connectors": [{"name": "A", "pins": pins}],
            "bench": null, "pinout_confidence": conf, "safety_note": null,
            "sources": [{"title": "s1", "url": ""}, {"title": "s2", "url": ""}]
        }))
        .unwrap_or_else(|e| panic!("unit: {e}"))
    }

    #[test]
    fn pins_sort_numerically_and_only_used_targets_are_drawn() {
        let u = unit(
            serde_json::json!([
                {"pin": "12", "function": "CAN high", "signal": "can_h", "sources": [0, 1]},
                {"pin": "2", "function": "Ground", "signal": "gnd", "sources": [0]},
                {"pin": "5", "function": "spare", "signal": "other", "sources": [0],
                 "confidence": "unverified"}
            ]),
            "community",
        );
        let d = layout(&u).unwrap();
        let labels: Vec<&str> = d.blocks[0].pins.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels, ["A-2", "A-5", "A-12"]);
        let sig: Vec<Signal> = d.targets.iter().map(|t| t.signal).collect();
        assert_eq!(sig, [Signal::Gnd, Signal::CanH], "schema order, no `other`");
        assert_eq!(d.blocks[0].pins[2].sources, 2);
        assert!(d.blocks[0].pins[1].unverified && d.any_unverified);
        assert!(!d.blocks[0].pins[0].unverified, "unit confidence applies");
        assert!(d.target_centre(Signal::Gnd).is_some());
        assert!(d.target_centre(Signal::Other).is_none());
        assert!(d.height() > d.blocks[0].y + d.blocks[0].h);
        assert_eq!(pin_num("S1-12"), 1);
        assert_eq!(pin_num("X"), u32::MAX);
        assert_eq!(clip("abcdef", 4), "abc…");
    }

    #[test]
    fn no_pins_no_diagram() {
        assert!(layout(&unit(serde_json::json!([]), "verified")).is_none());
    }
}
