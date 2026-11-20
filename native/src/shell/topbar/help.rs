//! Panel help: a short "what this does / what you need / how to use it" per
//! view, from `assets/panel-help.json` (the desktop's `panel-help.json`
//! content, keyed by the native view ids, with the three steps each panel
//! shows). Data-driven: a view with an entry gets help, one without simply
//! has none; nothing here knows a particular view.
//!
//! Shown two ways: the top bar's help menu opens [`HelpWindow`] (every
//! panel, pick one), and a view can put its own entry under its header with
//! [`strip`].

use crate::icons;
use crate::theme::{self, font, Tokens, Tone, DISPLAY, UI_STRONG};
use eframe::egui::{self, Margin, RichText, Ui};
use serde::Deserialize;
use std::sync::OnceLock;

/// One panel's help (an entry of `assets/panel-help.json`).
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Entry {
    /// The view id (`NativeView::id`, or a core view's `--view` name).
    pub id: String,
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub needs: Vec<String>,
    /// `read-only`, `writes-file`, `writes-ecu` or `none` (offline).
    pub risk: String,
    /// The guide page with the detail (file stem in `guide/`).
    #[serde(default)]
    pub guide: String,
    /// Three short steps.
    #[serde(default)]
    pub steps: Vec<String>,
}

impl Entry {
    /// The risk as a pill: text and tone.
    pub fn risk_pill(&self) -> (&'static str, Tone) {
        match self.risk.as_str() {
            "read-only" => ("Read-only", Tone::Ok),
            "writes-file" => ("Writes files", Tone::Warn),
            "writes-ecu" => ("Can write to the ECU", Tone::Crit),
            _ => ("Offline", Tone::Muted),
        }
    }
}

const DATA: &str = include_str!("../../../assets/panel-help.json");

fn parse(json: &str) -> Result<Vec<Entry>, String> {
    serde_json::from_str(json).map_err(|e| format!("panel help: {e}"))
}

/// Every help entry, in sidebar order.
pub fn entries() -> &'static [Entry] {
    static ALL: OnceLock<Vec<Entry>> = OnceLock::new();
    ALL.get_or_init(|| parse(DATA).unwrap_or_default())
}

/// The help for view `id`, if it has any.
pub fn entry(id: &str) -> Option<&'static Entry> {
    entries().iter().find(|e| e.id == id)
}

/// The body of one entry: risk pill, summary, needs, numbered steps.
pub fn body(ui: &mut Ui, t: &Tokens, e: &Entry) {
    ui.spacing_mut().item_spacing.y = 6.0;
    let (risk, tone) = e.risk_pill();
    ui.horizontal_wrapped(|ui| {
        theme::pill(ui, t, risk, tone, false);
        for n in &e.needs {
            theme::pill(ui, t, n, Tone::Muted, false);
        }
    });
    ui.add(egui::Label::new(RichText::new(&e.summary).size(13.0).color(t.fg)).wrap());
    if !e.steps.is_empty() {
        ui.add_space(2.0);
        for (i, s) in e.steps.iter().enumerate() {
            ui.horizontal_top(|ui| {
                ui.label(
                    RichText::new(format!("{}.", i + 1))
                        .font(font(UI_STRONG, 12.5))
                        .color(t.accent),
                );
                ui.add(egui::Label::new(RichText::new(s).size(12.5).color(t.fg_dim)).wrap());
            });
        }
    }
    if !e.guide.is_empty() {
        ui.label(
            RichText::new(format!("{} Guide page: {}", icons::BOOK, e.guide))
                .size(11.5)
                .color(t.fg_faint),
        );
    }
}

/// A view's help strip (collapsed to one line until opened). Draws nothing
/// for an id without help.
pub fn strip(ui: &mut Ui, t: &Tokens, id: &str) {
    let Some(e) = entry(id) else {
        return;
    };
    egui::Frame::new()
        .fill(t.surface2)
        .stroke(egui::Stroke::new(1.0, t.border_soft))
        .corner_radius(egui::CornerRadius::same(theme::RADIUS_SM))
        .inner_margin(Margin::symmetric(12, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::CollapsingHeader::new(
                RichText::new(format!(
                    "{}  {}",
                    icons::QUESTION,
                    first_sentence(&e.summary)
                ))
                .size(12.5)
                .color(t.fg_dim),
            )
            .id_salt(("panel-help", id))
            .default_open(false)
            .show(ui, |ui| body(ui, t, e));
        });
}

/// The summary's first sentence (the strip's one-line form).
fn first_sentence(s: &str) -> &str {
    match s.find(". ") {
        Some(i) => &s[..=i],
        None => s,
    }
}

/// The "Panel help" window of the top bar: every panel, one shown.
#[derive(Default)]
pub struct HelpWindow {
    pub open: bool,
    selected: Option<String>,
}

impl HelpWindow {
    /// Open on panel `id` (or the last one shown).
    pub fn show_for(&mut self, id: Option<&str>) {
        if let Some(id) = id.filter(|i| entry(i).is_some()) {
            self.selected = Some(id.to_string());
        }
        self.open = true;
    }

    pub fn ui(&mut self, ctx: &egui::Context, t: &Tokens) {
        if !self.open {
            return;
        }
        let all = entries();
        let cur = self
            .selected
            .as_deref()
            .and_then(entry)
            .or_else(|| all.first());
        let mut done = false;
        let r = egui::Modal::new(egui::Id::unique("panel-help-window"))
            .frame(theme::panel_frame(t).inner_margin(Margin::same(18)))
            .show(ctx, |ui| {
                ui.set_width(640.0);
                ui.label(
                    RichText::new("Panel help")
                        .font(font(DISPLAY, 16.0))
                        .color(t.fg),
                );
                ui.add_space(10.0);
                ui.horizontal_top(|ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("panel-help-list")
                        .max_height(360.0)
                        .max_width(180.0)
                        .show(ui, |ui| {
                            ui.set_width(170.0);
                            for e in all {
                                let on = cur.is_some_and(|c| c.id == e.id);
                                if ui.selectable_label(on, &e.title).clicked() {
                                    self.selected = Some(e.id.clone());
                                }
                            }
                        });
                    theme::vsep(ui, t, 360.0);
                    ui.vertical(|ui| {
                        ui.set_width(420.0);
                        match cur {
                            Some(e) => {
                                ui.label(
                                    RichText::new(&e.title)
                                        .font(font(DISPLAY, 15.0))
                                        .color(t.fg),
                                );
                                body(ui, t, e);
                            }
                            None => {
                                ui.label(RichText::new("No help is bundled.").color(t.fg_dim));
                            }
                        }
                    });
                });
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::button(ui, t, None, "Close").clicked() {
                        done = true;
                    }
                });
            });
        if done || r.should_close() {
            self.open = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_help_parses_and_covers_the_views() {
        let all = parse(DATA).expect("assets/panel-help.json parses");
        assert_eq!(all.len(), entries().len());
        let mut seen = std::collections::HashSet::new();
        for e in &all {
            assert!(seen.insert(e.id.as_str()), "duplicate help id {}", e.id);
            assert!(!e.title.is_empty() && !e.summary.is_empty(), "{}", e.id);
            assert!(
                matches!(
                    e.risk.as_str(),
                    "read-only" | "writes-file" | "writes-ecu" | "none"
                ),
                "{}: risk {}",
                e.id,
                e.risk
            );
            assert!(!e.steps.is_empty(), "{} has no steps", e.id);
        }
        // Every sidebar destination (core views and built-ins) has help.
        for id in crate::views::NAV_ORDER {
            assert!(entry(id).is_some(), "no panel help for '{id}'");
        }
        assert!(entry("about").is_some());
        assert!(entry("no-such-view").is_none());
    }

    #[test]
    fn risk_pills_and_first_sentence() {
        let mut e = entry("debug").unwrap().clone();
        assert_eq!(e.risk_pill().0, "Read-only");
        e.risk = "writes-ecu".into();
        assert_eq!(e.risk_pill(), ("Can write to the ECU", Tone::Crit));
        e.risk = "none".into();
        assert_eq!(e.risk_pill().1, Tone::Muted);
        assert_eq!(first_sentence("One. Two."), "One.");
        assert_eq!(first_sentence("Only one"), "Only one");
        assert!(parse("[{\"id\": 1}]").is_err());
    }
}
