//! Global top bar (persistent across views): brand, cable select +
//! auto-detect, battery voltage + link status, settings menu, theme toggle.

use crate::icons;
use crate::settings::{Settings, Units};
use crate::theme::{self, font, ThemeMode, Tokens, DISPLAY, MONO_MEDIUM};
use eframe::egui::{self, RichText, Ui};

/// Live adapter state shown in the bar. Filled by the adapter layer when one
/// is connected; `None` values render as "n/a".
#[derive(Clone, Debug, Default)]
pub struct LinkState {
    pub connected: bool,
    pub battery_v: Option<f32>,
    /// Last auto-detect / connect message.
    pub note: Option<String>,
}

#[derive(Default)]
pub struct TopBar {
    pub link: LinkState,
}

/// Known driver ids and their labels for the cable select.
pub const DRIVERS: &[(&str, &str)] = &[
    ("elm327-obd", "ELM327 OBD"),
    ("kkl", "KKL (K-line)"),
    ("j2534", "J2534 PassThru"),
    ("openport2", "OpenPort 2.0"),
    ("socketcan", "SocketCAN"),
    ("slcan", "slcan (serial CAN)"),
    ("gsusb", "gs_usb (candleLight)"),
    ("vcar", "Virtual car (simulator)"),
];

pub fn driver_label(id: &str) -> &str {
    DRIVERS
        .iter()
        .find(|(d, _)| *d == id)
        .map(|(_, l)| *l)
        .unwrap_or("No cable")
}

impl TopBar {
    pub fn ui(&mut self, ui: &mut Ui, t: &Tokens, settings: &mut Settings) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            // Brand.
            theme::logo(ui, t, 30.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(
                    RichText::new("OpenTune")
                        .font(font(DISPLAY, 15.0))
                        .color(t.fg),
                );
                ui.horizontal(|ui| {
                    theme::pill(ui, t, "native", theme::Tone::Accent, false);
                });
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                // Theme toggle (cycles to the opposite of what is shown).
                let showing_dark = t.dark;
                let icon = if showing_dark {
                    icons::SUN
                } else {
                    icons::MOON
                };
                if theme::icon_button(ui, t, icon, "Toggle light / dark").clicked() {
                    settings.theme = if showing_dark {
                        ThemeMode::Light
                    } else {
                        ThemeMode::Dark
                    };
                    theme::apply_mode(ui.ctx(), settings.theme);
                }
                self.settings_menu(ui, t, settings);
                theme::vsep(ui, t, 26.0);

                // Link + battery.
                let (link_txt, link_col) = if self.link.connected {
                    ("Connected", t.ok)
                } else {
                    ("Not connected", t.fg_dim)
                };
                theme::stat(ui, t, icons::PLUGS_CONNECTED, "Link", link_txt, link_col);
                let (v_txt, v_col) = match self.link.battery_v {
                    Some(v) => (format!("{v:.1} V"), if v >= 12.0 { t.ok } else { t.warn }),
                    None => ("n/a".to_string(), t.fg_dim),
                };
                theme::stat(ui, t, icons::LIGHTNING, "Batt", &v_txt, v_col);
                theme::vsep(ui, t, 26.0);

                // Cable select + auto-detect (adapter layer: see adapters.rs once wired).
                if theme::button(ui, t, Some(icons::BROADCAST), "Auto-detect").clicked() {
                    self.link.note = Some("auto-detect: no adapter layer yet".into());
                }
                let label = driver_label(&settings.adapter.driver).to_string();
                egui::ComboBox::from_id_salt("cable")
                    .selected_text(RichText::new(label).font(font(theme::UI_STRONG, 13.0)))
                    .width(170.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut settings.adapter.driver,
                            String::new(),
                            "No cable",
                        );
                        for (id, l) in DRIVERS {
                            ui.selectable_value(&mut settings.adapter.driver, id.to_string(), *l);
                        }
                    });
            });
        });
    }

    fn settings_menu(&mut self, ui: &mut Ui, t: &Tokens, settings: &mut Settings) {
        let r = theme::icon_button(ui, t, icons::GEAR, "Settings");
        egui::Popup::menu(&r).width(250.0).show(|ui| {
            theme::section_label(ui, t, "Appearance");
            ui.horizontal(|ui| {
                ui.label("Theme");
                for m in [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark] {
                    if ui
                        .selectable_label(settings.theme == m, m.label())
                        .clicked()
                    {
                        settings.theme = m;
                        theme::apply_mode(ui.ctx(), m);
                    }
                }
            });
            ui.separator();
            theme::section_label(ui, t, "Bench");
            ui.horizontal(|ui| {
                ui.label("Units");
                for u in [Units::Metric, Units::Imperial] {
                    ui.selectable_value(&mut settings.units, u, u.label());
                }
            });
            ui.horizontal(|ui| {
                ui.label("BSL baud");
                egui::ComboBox::from_id_salt("bsl_baud")
                    .selected_text(settings.bsl_baud.to_string())
                    .show_ui(ui, |ui| {
                        for b in [9600u32, 19200, 38400, 57600, 115200] {
                            ui.selectable_value(&mut settings.bsl_baud, b, b.to_string());
                        }
                    });
            });
            ui.checkbox(&mut settings.telemetry, "Send diagnostics (telemetry)")
                .on_hover_text("Off by default. The public build has no upload target.");
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Port");
                ui.add(
                    egui::TextEdit::singleline(&mut settings.adapter.port)
                        .hint_text("/dev/ttyUSB0")
                        .desired_width(140.0),
                );
            });
            ui.separator();
            ui.label(
                RichText::new(format!(
                    "OpenTune native {} · GPL-3.0-or-later",
                    env!("CARGO_PKG_VERSION")
                ))
                .font(font(MONO_MEDIUM, 11.5))
                .color(t.fg_dim),
            );
        });
    }
}
