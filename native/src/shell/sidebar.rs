//! Left navigation: grouped destinations with counts, version footer.

use crate::theme::{self, font, Tokens, MONO_MEDIUM, UI_MEDIUM, UI_STRONG};
use crate::views::View;
use eframe::egui::{self, CornerRadius, Margin, RichText, Sense, Stroke, Ui, Vec2};

/// One row: a destination with its label, icon and right-aligned count.
pub struct NavItem {
    pub view: View,
    pub label: String,
    pub icon: String,
    pub count: Option<String>,
}

/// A section of rows under an uppercase header.
pub struct NavGroup {
    pub label: String,
    pub items: Vec<NavItem>,
}

fn nav_item(ui: &mut Ui, t: &Tokens, item: &NavItem, active: bool, reveal: bool) -> bool {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 34.0), Sense::click());
    if active && reveal {
        // The view changed (a jump, `--view`, the last session's view):
        // scroll its row into the list's visible part.
        resp.scroll_to_me(None);
    }
    let hovered = resp.hovered();
    let (bg, fg, stroke) = if active {
        (t.accent_soft, t.accent, Stroke::new(1.0, t.accent_line))
    } else if hovered {
        (t.surface2, t.fg, Stroke::NONE)
    } else {
        (egui::Color32::TRANSPARENT, t.fg_dim, Stroke::NONE)
    };
    let p = ui.painter();
    p.rect(
        rect,
        CornerRadius::same(theme::RADIUS_SM),
        bg,
        stroke,
        egui::StrokeKind::Inside,
    );
    let left = rect.left() + 10.0;
    p.text(
        egui::pos2(left, rect.center().y),
        egui::Align2::LEFT_CENTER,
        &item.icon,
        egui::FontId::proportional(17.0),
        fg,
    );
    let f = if active {
        font(UI_STRONG, 13.5)
    } else {
        font(UI_MEDIUM, 13.5)
    };
    p.text(
        egui::pos2(left + 28.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        &item.label,
        f,
        fg,
    );
    if let Some(c) = &item.count {
        let cc = if active { t.accent } else { t.fg_faint };
        p.text(
            egui::pos2(rect.right() - 10.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            c,
            font(MONO_MEDIUM, 11.0),
            cc,
        );
    }
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

/// Draw the sidebar contents; returns the newly picked view, if any.
pub fn ui(
    ui: &mut Ui,
    t: &Tokens,
    current: View,
    groups: &[NavGroup],
    footer: &str,
) -> Option<View> {
    let mut picked = None;
    let shown = egui::Id::unique("opentune-nav-shown-view");
    let reveal = ui.data(|d| d.get_temp::<View>(shown)) != Some(current);
    ui.data_mut(|d| d.insert_temp(shown, current));
    let footer_h = 38.0;
    let nav_h = (ui.available_height() - footer_h).max(0.0);
    egui::ScrollArea::vertical()
        .id_salt("nav")
        .max_height(nav_h)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            egui::Frame::new()
                .inner_margin(Margin::symmetric(10, 12))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for g in groups.iter().filter(|g| !g.items.is_empty()) {
                        let label = g.label.as_str();
                        ui.add_space(8.0);
                        egui::Frame::new()
                            .inner_margin(Margin {
                                left: 10,
                                right: 0,
                                top: 0,
                                bottom: 4,
                            })
                            .show(ui, |ui| {
                                theme::section_label(ui, t, label);
                            });
                        for item in &g.items {
                            if nav_item(ui, t, item, item.view == current, reveal) {
                                picked = Some(item.view);
                            }
                        }
                    }
                });
        });
    ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
        egui::Frame::new()
            .inner_margin(Margin::symmetric(16, 10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    theme::dot(ui, t.ok, t.ok_soft);
                    ui.label(
                        RichText::new(footer)
                            .font(font(MONO_MEDIUM, 11.0))
                            .color(t.fg_faint),
                    );
                });
            });
        theme::hline(ui, t.border_soft);
    });
    picked
}
