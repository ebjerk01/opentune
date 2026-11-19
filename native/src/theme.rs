//! OpenTune design tokens, fonts and reusable widgets for the native app.
//!
//! Tokens are the design-target mockup's CSS custom properties (light and
//! dark). Views never hard-code colours: they call [`tokens`] and use the
//! helpers below, so a token change restyles the whole app.
//!
//! Fonts (bundled, SIL OFL 1.1, see assets/fonts/*-OFL.txt): Inter (UI),
//! Space Grotesk (display / headings), JetBrains Mono (data). Phosphor (MIT)
//! is a fallback in every family, so `icons::*` work inside any text.

use crate::icons;
use eframe::egui::{
    self, epaint::Shadow, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId,
    Margin, Response, RichText, Sense, Stroke, TextStyle, Ui, Vec2,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// User theme choice; `System` follows the OS light/dark setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    pub fn label(self) -> &'static str {
        match self {
            ThemeMode::System => "Follow system",
            ThemeMode::Light => "Light",
            ThemeMode::Dark => "Dark",
        }
    }
}

/// The colour tokens of one theme.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tokens {
    pub dark: bool,
    pub bg: Color32,
    pub surface: Color32,
    pub surface2: Color32,
    pub surface3: Color32,
    pub border: Color32,
    pub border_soft: Color32,
    pub fg: Color32,
    pub fg_dim: Color32,
    pub fg_faint: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub accent_line: Color32,
    /// Text on an accent fill (primary buttons, logo).
    pub on_accent: Color32,
    pub ok: Color32,
    pub ok_soft: Color32,
    pub warn: Color32,
    pub crit: Color32,
    pub crit_soft: Color32,
    pub sel: Color32,
    pub shadow: Color32,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}
const fn rgba(r: u8, g: u8, b: u8, a: f32) -> Color32 {
    // Premultiplied, as egui stores colours.
    let a8 = (a * 255.0) as u8;
    Color32::from_rgba_premultiplied(
        (r as f32 * a) as u8,
        (g as f32 * a) as u8,
        (b as f32 * a) as u8,
        a8,
    )
}

pub const LIGHT: Tokens = Tokens {
    dark: false,
    bg: rgb(0xf3, 0xf5, 0xfa),
    surface: rgb(0xff, 0xff, 0xff),
    surface2: rgb(0xee, 0xf1, 0xf7),
    surface3: rgb(0xe4, 0xe9, 0xf2),
    border: rgb(0xd7, 0xdd, 0xe8),
    border_soft: rgb(0xe7, 0xeb, 0xf3),
    fg: rgb(0x1a, 0x20, 0x30),
    fg_dim: rgb(0x58, 0x60, 0x74),
    fg_faint: rgb(0x92, 0x9c, 0xb0),
    accent: rgb(0xe4, 0x70, 0x1d),
    accent_soft: rgba(228, 112, 29, 0.12),
    accent_line: rgba(228, 112, 29, 0.32),
    on_accent: rgb(0x1a, 0x0e, 0x05),
    ok: rgb(0x17, 0xa5, 0x64),
    ok_soft: rgba(23, 165, 100, 0.14),
    warn: rgb(0xc3, 0x88, 0x1c),
    crit: rgb(0xd6, 0x45, 0x3c),
    crit_soft: rgba(214, 69, 60, 0.12),
    sel: rgba(228, 112, 29, 0.16),
    shadow: rgba(16, 22, 40, 0.13),
};

pub const DARK: Tokens = Tokens {
    dark: true,
    bg: rgb(0x0c, 0x0f, 0x16),
    surface: rgb(0x13, 0x18, 0x22),
    surface2: rgb(0x1a, 0x21, 0x30),
    surface3: rgb(0x23, 0x2c, 0x3e),
    border: rgb(0x26, 0x2f, 0x41),
    border_soft: rgb(0x1b, 0x22, 0x31),
    fg: rgb(0xe8, 0xeb, 0xf2),
    fg_dim: rgb(0x8b, 0x95, 0xa9),
    fg_faint: rgb(0x5a, 0x64, 0x7a),
    accent: rgb(0xff, 0x8d, 0x40),
    accent_soft: rgba(255, 141, 64, 0.14),
    accent_line: rgba(255, 141, 64, 0.34),
    on_accent: rgb(0x1a, 0x0e, 0x05),
    ok: rgb(0x45, 0xd4, 0x92),
    ok_soft: rgba(69, 212, 146, 0.14),
    warn: rgb(0xf3, 0xb4, 0x4e),
    crit: rgb(0xff, 0x5f, 0x5c),
    crit_soft: rgba(255, 95, 92, 0.14),
    sel: rgba(255, 141, 64, 0.20),
    shadow: rgba(0, 0, 0, 0.52),
};

/// The tokens of the theme egui is currently showing.
pub fn tokens(ctx: &egui::Context) -> Tokens {
    if ctx.global_style().visuals.dark_mode {
        DARK
    } else {
        LIGHT
    }
}

// ---------------------------------------------------------------------------
// Fonts
// ---------------------------------------------------------------------------

/// Display face (Space Grotesk Bold): titles, brand, big numbers.
pub const DISPLAY: &str = "display";
/// Display face, medium weight.
pub const DISPLAY_MEDIUM: &str = "display-medium";
/// Inter SemiBold: buttons, active nav items, emphasis.
pub const UI_STRONG: &str = "ui-strong";
/// Inter Medium.
pub const UI_MEDIUM: &str = "ui-medium";
/// JetBrains Mono Medium / SemiBold: emphasised data.
pub const MONO_MEDIUM: &str = "mono-medium";
pub const MONO_STRONG: &str = "mono-strong";

macro_rules! font {
    ($f:literal) => {
        Arc::new(FontData::from_static(include_bytes!(concat!(
            "../assets/fonts/",
            $f
        ))))
    };
}

/// Register the bundled fonts and the named families.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fd = FontDefinitions::default();
    let data = [
        ("inter", font!("Inter-Regular.ttf")),
        ("inter-medium", font!("Inter-Medium.ttf")),
        ("inter-semibold", font!("Inter-SemiBold.ttf")),
        ("grotesk-medium", font!("SpaceGrotesk-Medium.ttf")),
        ("grotesk-bold", font!("SpaceGrotesk-Bold.ttf")),
        ("jbmono", font!("JetBrainsMono-Regular.ttf")),
        ("jbmono-medium", font!("JetBrainsMono-Medium.ttf")),
        ("jbmono-semibold", font!("JetBrainsMono-SemiBold.ttf")),
        ("phosphor", font!("Phosphor.ttf")),
    ];
    for (k, v) in data {
        fd.font_data.insert(k.to_string(), v);
    }
    // egui's own fonts stay as last-resort fallbacks (symbols, emoji).
    let fallback: Vec<String> = fd
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let fam = |primary: &str| {
        let mut v = vec![primary.to_string(), "phosphor".to_string()];
        v.extend(fallback.iter().cloned());
        v
    };
    fd.families.insert(FontFamily::Proportional, fam("inter"));
    fd.families.insert(FontFamily::Monospace, fam("jbmono"));
    for (name, primary) in [
        (DISPLAY, "grotesk-bold"),
        (DISPLAY_MEDIUM, "grotesk-medium"),
        (UI_STRONG, "inter-semibold"),
        (UI_MEDIUM, "inter-medium"),
        (MONO_MEDIUM, "jbmono-medium"),
        (MONO_STRONG, "jbmono-semibold"),
    ] {
        fd.families
            .insert(FontFamily::Name(name.into()), fam(primary));
    }
    ctx.set_fonts(fd);
}

pub fn font(family: &str, size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(family.into()))
}
#[allow(dead_code)]
pub fn ui_font(size: f32) -> FontId {
    FontId::proportional(size)
}
#[allow(dead_code)]
pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

// ---------------------------------------------------------------------------
// Style
// ---------------------------------------------------------------------------

pub const RADIUS: u8 = 12;
pub const RADIUS_SM: u8 = 8;
/// Comfortable density: outer padding of the workspace and gaps between panels.
pub const GAP: f32 = 14.0;

fn visuals(t: &Tokens) -> egui::Visuals {
    let mut v = if t.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    v.dark_mode = t.dark;
    v.override_text_color = Some(t.fg);
    v.panel_fill = t.bg;
    v.window_fill = t.surface;
    v.window_stroke = Stroke::new(1.0, t.border);
    v.window_corner_radius = CornerRadius::same(RADIUS);
    v.window_shadow = Shadow {
        offset: [0, 10],
        blur: 30,
        spread: 0,
        color: t.shadow,
    };
    v.popup_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: t.shadow,
    };
    v.menu_corner_radius = CornerRadius::same(RADIUS);
    v.faint_bg_color = t.surface2;
    v.extreme_bg_color = t.surface2;
    v.code_bg_color = t.surface2;
    v.hyperlink_color = t.accent;
    v.warn_fg_color = t.warn;
    v.error_fg_color = t.crit;
    v.selection.bg_fill = t.sel;
    v.selection.stroke = Stroke::new(1.0, t.accent);
    let r = CornerRadius::same(RADIUS_SM);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = t.surface;
    w.noninteractive.weak_bg_fill = t.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, t.border_soft);
    w.noninteractive.fg_stroke = Stroke::new(1.0, t.fg_dim);
    w.noninteractive.corner_radius = r;
    for (wv, fill, stroke, fg) in [
        (&mut w.inactive, t.surface2, t.border, t.fg),
        (&mut w.hovered, t.surface3, t.border, t.fg),
        (&mut w.active, t.surface3, t.accent_line, t.fg),
        (&mut w.open, t.surface3, t.border, t.fg),
    ] {
        wv.bg_fill = fill;
        wv.weak_bg_fill = fill;
        wv.bg_stroke = Stroke::new(1.0, stroke);
        wv.fg_stroke = Stroke::new(1.5, fg);
        wv.corner_radius = r;
        wv.expansion = 0.0;
    }
    v
}

fn style_spacing(style: &mut egui::Style) {
    use TextStyle::*;
    style.text_styles = [
        (Small, FontId::proportional(11.0)),
        (Body, FontId::proportional(13.5)),
        (
            Button,
            FontId::new(13.0, FontFamily::Name(UI_STRONG.into())),
        ),
        (Heading, FontId::new(15.0, FontFamily::Name(DISPLAY.into()))),
        (Monospace, FontId::monospace(12.5)),
    ]
    .into();
    let s = &mut style.spacing;
    s.item_spacing = Vec2::new(8.0, 6.0);
    s.button_padding = Vec2::new(12.0, 7.0);
    s.interact_size.y = 30.0;
    s.menu_margin = Margin::same(7);
    s.window_margin = Margin::same(12);
    s.scroll.bar_width = 8.0;
    s.scroll.floating = true;
}

/// Install fonts, both themes' visuals and spacing. Call once at start-up.
pub fn setup(ctx: &egui::Context, mode: ThemeMode) {
    install_fonts(ctx);
    ctx.set_visuals_of(egui::Theme::Light, visuals(&LIGHT));
    ctx.set_visuals_of(egui::Theme::Dark, visuals(&DARK));
    ctx.all_styles_mut(style_spacing);
    apply_mode(ctx, mode);
}

pub fn apply_mode(ctx: &egui::Context, mode: ThemeMode) {
    ctx.set_theme(match mode {
        ThemeMode::System => egui::ThemePreference::System,
        ThemeMode::Light => egui::ThemePreference::Light,
        ThemeMode::Dark => egui::ThemePreference::Dark,
    });
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// An elevated panel: surface fill, 1 px border, 12 px radius, deep shadow.
pub fn panel_frame(t: &Tokens) -> egui::Frame {
    egui::Frame::new()
        .fill(t.surface)
        .stroke(Stroke::new(1.0, t.border))
        .corner_radius(CornerRadius::same(RADIUS))
        .shadow(Shadow {
            offset: [0, 8],
            blur: 26,
            spread: 0,
            color: t.shadow,
        })
}

/// A bar (global top bar, dump bar, status bar): surface fill, no radius.
pub fn bar_frame(t: &Tokens, margin: Margin) -> egui::Frame {
    egui::Frame::new().fill(t.surface).inner_margin(margin)
}

/// A panel with a header row (title + right-aligned extras) and a body.
/// The body gets the remaining height of `ui`.
pub fn panel<R>(
    ui: &mut Ui,
    t: &Tokens,
    title: &str,
    header_right: impl FnOnce(&mut Ui),
    body: impl FnOnce(&mut Ui) -> R,
) -> R {
    panel_frame(t)
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            egui::Frame::new()
                .inner_margin(Margin::symmetric(14, 9))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(title)
                                .font(font(DISPLAY_MEDIUM, 13.5))
                                .color(t.fg),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            header_right,
                        );
                    });
                });
            hline(ui, t.border_soft);
            body(ui)
        })
        .inner
}

/// A full-width 1 px horizontal rule.
pub fn hline(ui: &mut Ui, color: Color32) {
    let (r, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(r, 0.0, color);
}

/// A 1 px vertical separator of `height`.
pub fn vsep(ui: &mut Ui, t: &Tokens, height: f32) {
    let (r, _) = ui.allocate_exact_size(Vec2::new(1.0, height), Sense::hover());
    ui.painter().rect_filled(r, 0.0, t.border);
}

// ---------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------

fn icon_text(icon: Option<&str>, label: &str) -> String {
    match icon {
        Some(i) if !label.is_empty() => format!("{i}  {label}"),
        Some(i) => i.to_string(),
        None => label.to_string(),
    }
}

/// Secondary button (surface-2 fill, border).
pub fn button(ui: &mut Ui, t: &Tokens, icon: Option<&str>, label: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(icon_text(icon, label)).color(t.fg))
            .fill(t.surface2)
            .stroke(Stroke::new(1.0, t.border))
            .corner_radius(CornerRadius::same(RADIUS_SM)),
    )
}

/// Primary button (accent fill).
pub fn primary_button(ui: &mut Ui, t: &Tokens, icon: Option<&str>, label: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(icon_text(icon, label)).color(t.on_accent))
            .fill(t.accent)
            .stroke(Stroke::new(1.0, t.accent))
            .corner_radius(CornerRadius::same(RADIUS_SM)),
    )
}

/// Square 34 px icon button with a tooltip.
pub fn icon_button(ui: &mut Ui, t: &Tokens, icon: &str, tooltip: &str) -> Response {
    ui.add(
        egui::Button::new(RichText::new(icon).size(17.0).color(t.fg))
            .min_size(Vec2::splat(34.0))
            .fill(t.surface2)
            .stroke(Stroke::new(1.0, t.border))
            .corner_radius(CornerRadius::same(RADIUS_SM)),
    )
    .on_hover_text(tooltip)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Tone {
    Muted,
    Accent,
    Ok,
    Warn,
    Crit,
}

fn tone_colors(t: &Tokens, tone: Tone) -> (Color32, Color32) {
    match tone {
        Tone::Muted => (t.fg_dim, t.surface2),
        Tone::Accent => (t.accent, t.accent_soft),
        Tone::Ok => (t.ok, t.ok_soft),
        Tone::Warn => (t.warn, t.crit_soft),
        Tone::Crit => (t.crit, t.crit_soft),
    }
}

/// A rounded pill / badge.
pub fn pill(ui: &mut Ui, t: &Tokens, text: &str, tone: Tone, mono_text: bool) -> Response {
    let (fg, bg) = tone_colors(t, tone);
    let f = if mono_text {
        font(MONO_MEDIUM, 11.0)
    } else {
        font(UI_STRONG, 11.0)
    };
    egui::Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(255))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| ui.label(RichText::new(text).font(f).color(fg)))
        .response
}

/// Small uppercase section label (nav groups, menu headings).
pub fn section_label(ui: &mut Ui, t: &Tokens, text: &str) {
    ui.label(
        RichText::new(text.to_uppercase())
            .font(font(UI_STRONG, 10.5))
            .color(t.fg_faint),
    );
}

/// A boxed key/value stat (top bar): icon, KEY, value.
pub fn stat(
    ui: &mut Ui,
    t: &Tokens,
    icon: &str,
    key: &str,
    value: &str,
    value_color: Color32,
) -> Response {
    egui::Frame::new()
        .fill(t.surface2)
        .stroke(Stroke::new(1.0, t.border))
        .corner_radius(CornerRadius::same(RADIUS_SM))
        .inner_margin(Margin::symmetric(11, 6))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 7.0;
            ui.label(RichText::new(icon).size(15.0).color(t.fg_faint));
            ui.label(
                RichText::new(key.to_uppercase())
                    .font(font(UI_STRONG, 10.0))
                    .color(t.fg_faint),
            );
            ui.label(
                RichText::new(value)
                    .font(font(MONO_STRONG, 12.5))
                    .color(value_color),
            );
        })
        .response
}

/// A status dot with a soft halo.
pub fn dot(ui: &mut Ui, color: Color32, halo: Color32) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
    ui.painter().circle_filled(r.center(), 7.0, halo);
    ui.painter().circle_filled(r.center(), 4.0, color);
}

/// A large empty state for views that are not ported yet.
pub fn empty_state(ui: &mut Ui, t: &Tokens, icon: &str, title: &str, text: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.28).max(24.0));
        ui.label(RichText::new(icon).size(40.0).color(t.accent));
        ui.add_space(8.0);
        ui.label(RichText::new(title).font(font(DISPLAY, 20.0)).color(t.fg));
        ui.add_space(4.0);
        ui.label(RichText::new(text).color(t.fg_dim));
    });
}

/// The brand mark: accent tile with a gauge glyph.
pub fn logo(ui: &mut Ui, t: &Tokens, size: f32) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    ui.painter()
        .rect_filled(r, CornerRadius::same(RADIUS_SM), t.accent);
    ui.painter().text(
        r.center(),
        egui::Align2::CENTER_CENTER,
        icons::GAUGE,
        FontId::proportional(size * 0.6),
        t.on_accent,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_differ_and_are_opaque_where_needed() {
        assert!(DARK.dark && !LIGHT.dark);
        for t in [LIGHT, DARK] {
            for c in [t.bg, t.surface, t.fg, t.accent, t.border] {
                assert_eq!(c.a(), 255);
            }
            assert!(t.accent_soft.a() < 255);
        }
        assert_eq!(LIGHT.accent, Color32::from_rgb(0xe4, 0x70, 0x1d));
        assert_eq!(DARK.accent, Color32::from_rgb(0xff, 0x8d, 0x40));
    }
}
