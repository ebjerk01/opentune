//! Global top bar (persistent across views): brand, cable select +
//! auto-detect, battery voltage + link status, settings menu, theme toggle.
//!
//! The adapter work (port discovery, ELM327 probing, the battery link) lives
//! in `crate::adapters` and runs on background threads; this file only polls
//! its results and draws them.

use crate::adapters::{self, BusyPort, Candidate, DetectReport, Link, LinkState, LinkStatus};
use crate::icons;
use crate::settings::{Settings, Units};
use crate::theme::{
    self, font, ThemeMode, Tokens, Tone, DISPLAY, MONO_MEDIUM, MONO_STRONG, RADIUS_SM, UI_STRONG,
};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontId, Margin, Response, RichText, Sense,
    Stroke, Ui, Vec2,
};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// Known driver ids and their labels for the cable select.
pub const DRIVERS: &[(&str, &str)] = &[
    ("elm327obd", "ELM327 OBD"),
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

const BSL_BAUDS: [u32; 5] = [9600, 19200, 38400, 57600, 115200];

struct Toast {
    text: String,
    tone: Tone,
    until: Instant,
}

#[derive(Default)]
pub struct TopBar {
    link: Link,
    /// Status seen last frame (to toast transitions such as a lost link).
    last_status: LinkStatus,
    /// Running scan and whether it was the Auto-detect button (pick + toast).
    scan: Option<(Receiver<DetectReport>, bool)>,
    candidates: Vec<Candidate>,
    /// Outcome of the last auto-detect, shown as the cable select's tooltip.
    detect_note: Option<String>,
    toast: Option<Toast>,
    /// Width of the right-hand group last frame (it is right-aligned but laid
    /// out left to right, so its inner widgets keep their reading order).
    right_w: f32,
    adapter_window: bool,
    about_window: bool,
}

impl TopBar {
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        t: &Tokens,
        settings: &mut Settings,
        branding: &crate::host::Branding,
    ) {
        let ctx = ui.ctx().clone();
        self.poll(settings);
        let link = self.link.snapshot();
        // Any change of driver / port (select, auto-detect, settings window)
        // drops a live link that no longer matches it.
        if link.status != LinkStatus::Idle
            && (settings.adapter.driver != adapters::ELM_DRIVER
                || settings.adapter.port.trim() != link.port)
        {
            self.link.disconnect();
            self.last_status = LinkStatus::Idle;
            self.notify("Adapter changed: disconnected", Tone::Muted);
        }
        let link = self.link.snapshot();

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            // Brand.
            theme::logo(ui, t, 30.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(
                    RichText::new(&branding.name)
                        .font(font(DISPLAY, 15.0))
                        .color(t.fg),
                );
                if !branding.pill.is_empty() {
                    ui.horizontal(|ui| {
                        theme::pill(ui, t, &branding.pill, Tone::Accent, false);
                    });
                }
            });

            ui.add_space((ui.available_width() - self.right_w).max(0.0));
            let group = ui
                .horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    self.cable_select(ui, t, settings, &link);
                    self.detect_button(ui, t, settings, &link);
                    theme::vsep(ui, t, 26.0);
                    self.battery_stat(ui, t, &link);
                    self.link_stat(ui, t, settings, &link);
                    theme::vsep(ui, t, 26.0);
                    self.settings_menu(ui, t, settings);
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
                })
                .response;
            let w = group.rect.width();
            if (w - self.right_w).abs() > 0.5 {
                self.right_w = w;
                ui.ctx().request_discard("top bar group width");
            }
        });

        let toast_y = ui.min_rect().bottom() + 17.0;
        self.show_toast(&ctx, t, toast_y);
        self.adapter_modal(&ctx, t, settings);
        self.about_modal(&ctx, t);
    }

    // -----------------------------------------------------------------------
    // Background results
    // -----------------------------------------------------------------------

    fn poll(&mut self, settings: &mut Settings) {
        if let Some((rx, pick)) = &self.scan {
            match rx.try_recv() {
                Ok(rep) => {
                    let pick = *pick;
                    self.scan = None;
                    self.apply_report(rep, pick, settings);
                }
                Err(TryRecvError::Disconnected) => {
                    self.scan = None;
                    self.notify("Adapter scan failed", Tone::Crit);
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        let st = self.link.snapshot();
        if st.status != self.last_status {
            match st.status {
                LinkStatus::Connected => {
                    if st.baud != 0 {
                        settings.adapter.baud = st.baud;
                    }
                    let id = st.ident.clone().unwrap_or_else(|| "ELM327".into());
                    self.notify(
                        &format!("Connected: {id} on {} at {} baud", st.port, st.baud),
                        Tone::Ok,
                    );
                }
                LinkStatus::Idle => {
                    if let Some(e) = &st.error {
                        self.notify(&format!("Not connected: {e}"), Tone::Crit);
                    }
                }
                LinkStatus::Connecting => {}
            }
            self.last_status = st.status;
        }
    }

    fn apply_report(&mut self, rep: DetectReport, pick: bool, settings: &mut Settings) {
        self.candidates = rep.candidates;
        if !pick {
            return;
        }
        let n = self.candidates.len();
        let (mut note, tone) = match adapters::best(&self.candidates, rep.probed) {
            Some(c) => {
                settings.adapter.driver = c.driver.unwrap_or_default().to_string();
                settings.adapter.port = c.port.clone();
                if let Some(b) = c.elm_baud {
                    settings.adapter.baud = b;
                }
                let label = driver_label(c.driver.unwrap_or_default());
                match (&c.elm, c.driver) {
                    (Some(id), _) => (
                        format!(
                            "Found {id} on {} at {} baud",
                            c.port,
                            c.elm_baud.unwrap_or(0)
                        ),
                        Tone::Ok,
                    ),
                    (None, Some(adapters::ELM_DRIVER)) => (
                        format!(
                            "Picked {label} on {} by USB id (no ELM327 reply to ATI)",
                            c.port
                        ),
                        Tone::Warn,
                    ),
                    (None, Some("kkl")) => (
                        format!(
                            "Picked {label} on {} by USB id (K-line ports are never probed)",
                            c.port
                        ),
                        Tone::Ok,
                    ),
                    _ => (format!("Picked {label} on {}", c.port), Tone::Ok),
                }
            }
            None => (
                format!(
                    "No known adapter found ({n} port{} listed)",
                    if n == 1 { "" } else { "s" }
                ),
                Tone::Warn,
            ),
        };
        self.notify(&note, tone);
        for e in &rep.errors {
            note.push('\n');
            note.push_str(e);
        }
        self.detect_note = Some(note);
    }

    fn start_scan(&mut self, ctx: &egui::Context, probe: bool, s: &Settings, link: &LinkState) {
        if self.scan.is_some() {
            return;
        }
        let busy = (link.status == LinkStatus::Connected).then(|| BusyPort {
            port: link.port.clone(),
            ident: link.ident.clone().unwrap_or_else(|| "ELM327".into()),
            baud: link.baud,
        });
        let rx = adapters::spawn_scan(ctx.clone(), probe, busy, s.adapter.baud);
        self.scan = Some((rx, probe));
    }

    fn notify(&mut self, text: &str, tone: Tone) {
        let secs = if tone == Tone::Crit { 10 } else { 6 };
        self.toast = Some(Toast {
            text: text.to_string(),
            tone,
            until: Instant::now() + Duration::from_secs(secs),
        });
    }

    fn toggle_link(&mut self, ctx: &egui::Context, settings: &Settings, link: &LinkState) {
        if link.status != LinkStatus::Idle {
            self.link.disconnect();
            self.last_status = LinkStatus::Idle;
            self.notify("Disconnected", Tone::Muted);
            return;
        }
        let driver = settings.adapter.driver.as_str();
        let port = settings.adapter.port.trim();
        if driver.is_empty() {
            self.notify("Pick a cable first (or run Auto-detect)", Tone::Warn);
        } else if driver != adapters::ELM_DRIVER {
            self.notify(
                &format!(
                    "{}: connect is not available in the native app yet",
                    driver_label(driver)
                ),
                Tone::Warn,
            );
        } else if port.is_empty() {
            self.notify(
                "No port set: run Auto-detect or pick a port in the cable list",
                Tone::Warn,
            );
        } else {
            self.link
                .connect(ctx.clone(), port.to_string(), settings.adapter.baud);
            self.last_status = LinkStatus::Connecting;
        }
    }

    // -----------------------------------------------------------------------
    // Widgets
    // -----------------------------------------------------------------------

    /// The `.select` of the mockup: status dot, driver label, port, caret.
    fn cable_select(&mut self, ui: &mut Ui, t: &Tokens, settings: &mut Settings, link: &LinkState) {
        let (dot, halo) = match link.status {
            LinkStatus::Connected => (t.ok, t.ok_soft),
            LinkStatus::Connecting => (t.warn, t.accent_soft),
            LinkStatus::Idle if link.error.is_some() => (t.crit, t.crit_soft),
            LinkStatus::Idle => (t.fg_faint, t.surface3),
        };
        let driver = settings.adapter.driver.clone();
        let port = settings.adapter.port.trim().to_string();
        let mut prepared = egui::Frame::new()
            .fill(t.surface2)
            .stroke(Stroke::new(1.0, t.border))
            .corner_radius(CornerRadius::same(RADIUS_SM))
            .inner_margin(Margin::symmetric(11, 7))
            .begin(ui);
        {
            let ui = &mut prepared.content_ui;
            ui.spacing_mut().item_spacing.x = 8.0;
            small_dot(ui, dot, halo);
            ui.label(
                RichText::new(driver_label(&driver))
                    .font(font(UI_STRONG, 13.0))
                    .color(t.fg),
            );
            if !driver.is_empty() && !port.is_empty() {
                ui.label(
                    RichText::new(short_port(&port))
                        .font(font(MONO_MEDIUM, 11.5))
                        .color(t.fg_faint),
                );
            }
            ui.label(
                RichText::new(icons::CARET_DOWN)
                    .size(13.0)
                    .color(t.fg_faint),
            );
        }
        let rect = prepared.allocate_space(ui).rect;
        let resp = ui
            .interact(rect, ui.make_persistent_id("cable-select"), Sense::click())
            .on_hover_cursor(CursorIcon::PointingHand);
        if resp.hovered() {
            prepared.frame.fill = t.surface3;
        }
        prepared.paint(ui);
        let tip = match (&self.detect_note, port.is_empty()) {
            (Some(n), _) => n.clone(),
            (None, false) => format!("Port: {port}"),
            (None, true) => "Choose the adapter cable".to_string(),
        };
        let resp = resp.on_hover_text(tip);
        if resp.clicked() {
            // Refresh the port list (enumeration only, nothing is opened).
            self.start_scan(ui.ctx(), false, settings, link);
        }

        let mut pick: Option<(String, Option<String>)> = None;
        egui::Popup::menu(&resp).width(300.0).show(|ui| {
            if ui.selectable_label(driver.is_empty(), "No cable").clicked() {
                pick = Some((String::new(), None));
            }
            menu_heading(ui, t, "Drivers");
            for (id, label) in DRIVERS {
                if ui.selectable_label(driver == *id, *label).clicked() {
                    pick = Some((id.to_string(), None));
                }
            }
            menu_heading(ui, t, "Detected");
            if self.scan.is_some() {
                ui.label(RichText::new("Scanning…").color(t.fg_faint));
            } else if self.candidates.is_empty() {
                ui.label(
                    RichText::new("No serial ports or CAN interfaces found").color(t.fg_faint),
                );
            }
            for c in &self.candidates {
                let name = match (&c.elm, c.driver) {
                    (Some(_), _) => "ELM327 OBD",
                    (None, Some(d)) => driver_label(d),
                    (None, None) => "Unknown serial",
                };
                let selected = port == c.port && c.driver.is_none_or(|d| d == driver);
                let mut tip = c.detail.clone();
                if let Some(id) = &c.elm {
                    tip.push_str(&format!("\n{id}"));
                }
                if ui
                    .selectable_label(selected, format!("{name} — {}", c.port))
                    .on_hover_text(tip)
                    .clicked()
                {
                    // An unrecognised port keeps the chosen driver.
                    let d = c
                        .driver
                        .map(str::to_string)
                        .unwrap_or_else(|| driver.clone());
                    pick = Some((d, Some(c.port.clone())));
                    if let Some(b) = c.elm_baud {
                        settings.adapter.baud = b;
                    }
                }
            }
        });
        if let Some((d, p)) = pick {
            settings.adapter.driver = d;
            if let Some(p) = p {
                settings.adapter.port = p;
            }
        }
    }

    fn detect_button(&mut self, ui: &mut Ui, t: &Tokens, settings: &Settings, link: &LinkState) {
        let scanning = self.scan.as_ref().is_some_and(|(_, probe)| *probe);
        let idle = link.status == LinkStatus::Idle;
        let (icon, label) = if scanning {
            (icons::CIRCLE_NOTCH, "Detecting…")
        } else {
            (icons::BROADCAST, "Auto-detect")
        };
        let r = ui
            .add_enabled_ui(idle && !scanning, |ui| {
                theme::button(ui, t, Some(icon), label)
            })
            .inner;
        let r = if idle {
            r.on_hover_text(
                "List serial ports and CAN interfaces, and confirm ELM327 adapters with \
                 AT identification commands only (ATI). K-line cables are never written to.",
            )
        } else {
            r.on_disabled_hover_text("Disconnect first")
        };
        if r.clicked() {
            self.start_scan(ui.ctx(), true, settings, link);
        }
    }

    fn battery_stat(&mut self, ui: &mut Ui, t: &Tokens, link: &LinkState) {
        let (v_txt, v_col) = match link.battery_v {
            Some(v) => (format!("{v:.1} V"), if v >= 12.0 { t.ok } else { t.warn }),
            None => ("n/a".to_string(), t.fg_dim),
        };
        theme::stat(ui, t, icons::LIGHTNING, "Batt", &v_txt, v_col).on_hover_text(
            "Battery voltage at the OBD socket, read by the ELM327 (ATRV) every 2 s while connected",
        );
    }

    /// The Link stat doubles as the connect / disconnect button.
    fn link_stat(&mut self, ui: &mut Ui, t: &Tokens, settings: &Settings, link: &LinkState) {
        let (icon, txt, col) = match link.status {
            LinkStatus::Connected => (icons::PLUGS_CONNECTED, "Connected", t.ok),
            LinkStatus::Connecting => (icons::PLUGS, "Connecting…", t.warn),
            LinkStatus::Idle => (icons::PLUGS, "Not connected", t.fg_dim),
        };
        let r = theme::stat(ui, t, icon, "Link", txt, col);
        let r = ui
            .interact(r.rect, ui.make_persistent_id("link-stat"), Sense::click())
            .on_hover_cursor(CursorIcon::PointingHand);
        let tip = match link.status {
            LinkStatus::Connected => format!(
                "{} on {} at {} baud\nClick to disconnect",
                link.ident.as_deref().unwrap_or("ELM327"),
                link.port,
                link.baud
            ),
            LinkStatus::Connecting => format!("Opening {}…\nClick to cancel", link.port),
            LinkStatus::Idle => match &link.error {
                Some(e) => format!("Click to connect\nLast attempt: {e}"),
                None => "Click to connect (ELM327 only in the native app)".to_string(),
            },
        };
        if r.on_hover_text(tip).clicked() {
            self.toggle_link(ui.ctx(), settings, link);
        }
    }

    /// The gear menu (mockup `.menu`).
    fn settings_menu(&mut self, ui: &mut Ui, t: &Tokens, settings: &mut Settings) {
        let r = theme::icon_button(ui, t, icons::GEAR, "Settings");
        egui::Popup::menu(&r)
            .width(244.0)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                menu_heading(ui, t, "Appearance");
                if menu_row(ui, t, "Theme", RowValue::Text(settings.theme.label()))
                    .on_hover_text("Click to change")
                    .clicked()
                {
                    settings.theme = match settings.theme {
                        ThemeMode::System => ThemeMode::Light,
                        ThemeMode::Light => ThemeMode::Dark,
                        ThemeMode::Dark => ThemeMode::System,
                    };
                    theme::apply_mode(ui.ctx(), settings.theme);
                }
                menu_sep(ui, t);
                menu_heading(ui, t, "Bench");
                if menu_row(ui, t, "Units", RowValue::Text(settings.units.label()))
                    .on_hover_text("Click to change")
                    .clicked()
                {
                    settings.units = match settings.units {
                        Units::Metric => Units::Imperial,
                        Units::Imperial => Units::Metric,
                    };
                }
                let baud = settings.bsl_baud.to_string();
                if menu_row(ui, t, "BSL baud", RowValue::Text(&baud))
                    .on_hover_text(
                        "Bootstrap-loader baud for BSL reads (38400 works at 16/20/32 MHz). \
                         Click to change",
                    )
                    .clicked()
                {
                    settings.bsl_baud = next_baud(settings.bsl_baud);
                }
                if menu_row(ui, t, "Telemetry", RowValue::Switch(settings.telemetry))
                    .on_hover_text("Off by default. The public build has no upload target.")
                    .clicked()
                {
                    settings.telemetry = !settings.telemetry;
                }
                menu_sep(ui, t);
                if menu_row(ui, t, "Adapter settings…", RowValue::None).clicked() {
                    self.adapter_window = true;
                    ui.close();
                }
                let ver = env!("CARGO_PKG_VERSION");
                if menu_row(ui, t, "About OpenTune", RowValue::Text(ver)).clicked() {
                    self.about_window = true;
                    ui.close();
                }
            });
    }

    // -----------------------------------------------------------------------
    // Overlays
    // -----------------------------------------------------------------------

    fn show_toast(&mut self, ctx: &egui::Context, t: &Tokens, y: f32) {
        let Some(toast) = &self.toast else {
            return;
        };
        let now = Instant::now();
        if now >= toast.until {
            self.toast = None;
            return;
        }
        ctx.request_repaint_after(toast.until - now);
        let (icon, col) = match toast.tone {
            Tone::Ok => (icons::CHECK_CIRCLE, t.ok),
            Tone::Warn => (icons::WARNING, t.warn),
            Tone::Crit => (icons::WARNING_CIRCLE, t.crit),
            Tone::Accent | Tone::Muted => (icons::INFO, t.accent),
        };
        let r = egui::Area::new(egui::Id::unique("topbar-toast"))
            .order(egui::Order::Foreground)
            .anchor(Align2::RIGHT_TOP, [-14.0, y])
            .show(ctx, |ui| {
                theme::panel_frame(t)
                    .inner_margin(Margin::symmetric(12, 9))
                    .show(ui, |ui| {
                        ui.set_max_width(440.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(icon).size(16.0).color(col));
                            ui.add(egui::Label::new(RichText::new(&toast.text).color(t.fg)).wrap());
                        });
                    })
            });
        if r.response
            .interact(Sense::click())
            .on_hover_text("Dismiss")
            .clicked()
        {
            self.toast = None;
        }
    }

    fn adapter_modal(&mut self, ctx: &egui::Context, t: &Tokens, settings: &mut Settings) {
        if !self.adapter_window {
            return;
        }
        let mut done = false;
        let cands = &self.candidates;
        let r = egui::Modal::new(egui::Id::unique("adapter-settings"))
            .frame(theme::panel_frame(t).inner_margin(Margin::same(18)))
            .show(ctx, |ui| {
                ui.set_width(400.0);
                modal_title(ui, t, "Adapter settings");
                let a = &mut settings.adapter;
                egui::Grid::new("adapter-grid")
                    .num_columns(2)
                    .spacing([14.0, 10.0])
                    .show(ui, |ui| {
                        ui.label(RichText::new("Driver").color(t.fg_dim));
                        egui::ComboBox::from_id_salt("adapter-driver")
                            .selected_text(driver_label(&a.driver))
                            .width(220.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut a.driver, String::new(), "No cable");
                                for (id, l) in DRIVERS {
                                    ui.selectable_value(&mut a.driver, id.to_string(), *l);
                                }
                            });
                        ui.end_row();

                        ui.label(RichText::new("Port").color(t.fg_dim));
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut a.port)
                                    .hint_text("/dev/ttyUSB0 · COM3 · can0")
                                    .font(font(MONO_MEDIUM, 12.5))
                                    .desired_width(190.0),
                            );
                            ui.add_enabled_ui(!cands.is_empty(), |ui| {
                                ui.menu_button(icons::LIST, |ui| {
                                    for c in cands {
                                        if ui.button(format!("{} — {}", c.port, c.detail)).clicked()
                                        {
                                            a.port = c.port.clone();
                                        }
                                    }
                                })
                                .response
                                .on_hover_text("Detected ports")
                                .on_disabled_hover_text(
                                    "Open the cable list or run Auto-detect to find ports",
                                );
                            });
                        });
                        ui.end_row();

                        ui.label(RichText::new("CAN bus").color(t.fg_dim));
                        ui.horizontal(|ui| {
                            let ms = a.can_bus == "ms";
                            if ui.selectable_label(!ms, "HS-CAN").clicked() {
                                a.can_bus = "hs".into();
                            }
                            if ui.selectable_label(ms, "MS-CAN").clicked() {
                                a.can_bus = "ms".into();
                            }
                        });
                        ui.end_row();

                        ui.label(RichText::new("Request id (tx)").color(t.fg_dim));
                        hex_field(ui, t, &mut a.tx, "7E0");
                        ui.end_row();
                        ui.label(RichText::new("Response id (rx)").color(t.fg_dim));
                        hex_field(ui, t, &mut a.rx, "7E8");
                        ui.end_row();

                        ui.label(RichText::new("Target module").color(t.fg_dim));
                        let typed = !a.tx.trim().is_empty() || !a.rx.trim().is_empty();
                        match crate::host::resolve_target(&a.tx, &a.rx, None) {
                            Ok(tg) if typed => ui.label(
                                RichText::new(tg.to_string())
                                    .font(theme::font(theme::MONO_STRONG, 12.5))
                                    .color(t.fg),
                            ),
                            Ok(_) => ui.label(
                                RichText::new("definition's ids, else engine (7E0 / 7E8)")
                                    .color(t.fg),
                            ),
                            Err(e) => ui.label(
                                RichText::new(format!("{} {e}", icons::WARNING))
                                    .size(12.0)
                                    .color(t.crit),
                            ),
                        };
                        ui.end_row();
                    });
                ui.add_space(8.0);
                ui.label(
                    RichText::new(
                        "One target for the whole app: every diagnostic view and plugin talks \
                         to this module, and every ECU write names it in its confirmation. \
                         Blank ids use the ECU definition's default. The CAN bus choice \
                         applies to dual-bus (HS/MS switch) ELM327 adapters.",
                    )
                    .size(12.0)
                    .color(t.fg_faint),
                );
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::primary_button(ui, t, None, "Done").clicked() {
                        done = true;
                    }
                });
            });
        if done || r.should_close() {
            self.adapter_window = false;
        }
    }

    fn about_modal(&mut self, ctx: &egui::Context, t: &Tokens) {
        if !self.about_window {
            return;
        }
        let mut done = false;
        let r = egui::Modal::new(egui::Id::unique("about-opentune"))
            .frame(theme::panel_frame(t).inner_margin(Margin::same(18)))
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.horizontal(|ui| {
                    theme::logo(ui, t, 34.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("OpenTune")
                                .font(font(DISPLAY, 18.0))
                                .color(t.fg),
                        );
                        ui.label(
                            RichText::new(format!("native {}", env!("CARGO_PKG_VERSION")))
                                .font(font(MONO_STRONG, 12.0))
                                .color(t.accent),
                        );
                    });
                });
                ui.add_space(10.0);
                let line = |ui: &mut Ui, k: &str, v: &str| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(k).font(font(UI_STRONG, 12.5)).color(t.fg_dim));
                        ui.label(RichText::new(v).size(12.5).color(t.fg));
                    });
                };
                line(ui, "Licence", "GPL-3.0-or-later");
                line(ui, "UI", "egui / eframe on wgpu");
                line(
                    ui,
                    "Credits",
                    "fonts: Inter, Space Grotesk, JetBrains Mono (SIL OFL 1.1); \
                     icons: Phosphor (MIT)",
                );
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::button(ui, t, None, "Close").clicked() {
                        done = true;
                    }
                });
            });
        if done || r.should_close() {
            self.about_window = false;
        }
    }
}

// ---------------------------------------------------------------------------
// Small private helpers (menu rows, switch, dot)
// ---------------------------------------------------------------------------

fn next_baud(b: u32) -> u32 {
    let i = BSL_BAUDS.iter().position(|x| *x == b);
    match i {
        Some(i) => BSL_BAUDS[(i + 1) % BSL_BAUDS.len()],
        None => 38400,
    }
}

/// "/dev/ttyUSB0" -> "ttyUSB0"; other names unchanged.
fn short_port(p: &str) -> &str {
    p.strip_prefix("/dev/").unwrap_or(p)
}

/// The select's 8 px status dot with a 3 px halo.
fn small_dot(ui: &mut Ui, color: Color32, halo: Color32) {
    let (r, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
    ui.painter().circle_filled(r.center(), 7.0, halo);
    ui.painter().circle_filled(r.center(), 4.0, color);
}

fn menu_heading(ui: &mut Ui, t: &Tokens, text: &str) {
    ui.add_space(7.0);
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        theme::section_label(ui, t, text);
    });
    ui.add_space(3.0);
}

fn menu_sep(ui: &mut Ui, t: &Tokens) {
    ui.add_space(6.0);
    theme::hline(ui, t.border_soft);
    ui.add_space(6.0);
}

fn modal_title(ui: &mut Ui, t: &Tokens, text: &str) {
    ui.label(RichText::new(text).font(font(DISPLAY, 16.0)).color(t.fg));
    ui.add_space(12.0);
}

fn hex_field(ui: &mut Ui, t: &Tokens, value: &mut String, hint: &str) {
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(value)
                .hint_text(hint)
                .font(font(MONO_MEDIUM, 12.5))
                .desired_width(110.0),
        );
        if !adapters::valid_can_id(value) {
            ui.label(
                RichText::new(format!("{} not a hex CAN id", icons::WARNING))
                    .size(12.0)
                    .color(t.crit),
            );
        }
    });
}

enum RowValue<'a> {
    None,
    Text(&'a str),
    Switch(bool),
}

/// A full-width menu item (mockup `.menu-item`): label left, value or switch
/// right, surface-2 hover.
fn menu_row(ui: &mut Ui, t: &Tokens, label: &str, value: RowValue) -> Response {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
    if ui.is_rect_visible(rect) {
        if resp.hovered() {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(7), t.surface2);
        }
        ui.painter().text(
            rect.left_center() + Vec2::new(10.0, 0.0),
            Align2::LEFT_CENTER,
            label,
            FontId::proportional(13.0),
            t.fg,
        );
        let right = rect.right_center() - Vec2::new(10.0, 0.0);
        match value {
            RowValue::None => {}
            RowValue::Text(v) => {
                ui.painter().text(
                    right,
                    Align2::RIGHT_CENTER,
                    v,
                    font(MONO_MEDIUM, 12.0),
                    t.fg_dim,
                );
            }
            RowValue::Switch(on) => switch(ui, t, resp.id, on, right),
        }
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

/// The mockup's `.sw` toggle (32 x 18), painted right-aligned at `right`.
fn switch(ui: &Ui, t: &Tokens, id: egui::Id, on: bool, right: egui::Pos2) {
    let how = ui.ctx().animate_bool(id.with("sw"), on);
    let r = egui::Rect::from_min_size(right - Vec2::new(32.0, 9.0), Vec2::new(32.0, 18.0));
    let bg = t.border.lerp_to_gamma(t.ok, how);
    ui.painter().rect_filled(r, CornerRadius::same(9), bg);
    let knob = if t.dark && how < 0.5 { t.fg } else { t.surface };
    let x = egui::lerp((r.left() + 9.0)..=(r.right() - 9.0), how);
    ui.painter()
        .circle_filled(egui::pos2(x, r.center().y), 7.0, knob);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bsl_baud_cycles_and_labels() {
        assert_eq!(next_baud(38400), 57600);
        assert_eq!(next_baud(115200), 9600);
        assert_eq!(next_baud(1234), 38400);
        assert_eq!(short_port("/dev/ttyUSB0"), "ttyUSB0");
        assert_eq!(short_port("COM3"), "COM3");
        assert_eq!(driver_label("kkl"), "KKL (K-line)");
        assert_eq!(driver_label(""), "No cable");
    }
}
