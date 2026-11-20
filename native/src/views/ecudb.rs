//! ECU database browser (database/units via opentune-db): units, pinouts,
//! access profiles.
//!
//! The database is loaded lazily the first time the view is shown, from the
//! first existing of `database`, `../database` or `<exe dir>/database`. Left:
//! search + kind filter + the units. Right: identity, read methods, the access
//! profile (OBD / bench / boot / security cards), pinout, bench wiring and the
//! cited sources as links. Data only: the database holds no key material and
//! this view adds none.

use crate::icons;
use crate::theme::{self, font, Tokens, Tone, DISPLAY_MEDIUM, MONO_MEDIUM, UI_MEDIUM, UI_STRONG};
use crate::views::maps::{kv, two_pane};
use eframe::egui::{
    self, Align, CornerRadius, Label, Layout, Margin, Rect, RichText, Sense, Stroke, Ui, UiBuilder,
    Vec2,
};
use opentune_db::{
    Access, BootEntry, Bus, Confidence, Database, Entry, Kind, ObdAccess, Possible, Protocol,
    Reachable, SecurityNeed, Signal, StrapAction, Unit,
};
use std::path::PathBuf;

/// Height of a unit row in the list.
const ROW_H: f32 = 46.0;

enum State {
    Loaded { db: Database, dir: PathBuf },
    Missing { tried: Vec<PathBuf> },
    Failed { dir: PathBuf, err: String },
}

#[derive(Default)]
pub struct EcuDbView {
    state: Option<State>,
    search: String,
    kind: Option<Kind>,
    selected: Option<usize>,
    filter_key: Option<(String, Option<Kind>)>,
    filtered: Vec<usize>,
}

/// Where the database folder is looked for, in order.
fn candidates() -> Vec<PathBuf> {
    crate::paths::candidates("database")
}

/// The first candidate that is a directory.
fn first_dir(cands: &[PathBuf]) -> Option<PathBuf> {
    cands.iter().find(|p| p.is_dir()).cloned()
}

fn load() -> State {
    let tried = candidates();
    let Some(dir) = first_dir(&tried) else {
        return State::Missing { tried };
    };
    match Database::load_dir(&dir) {
        Ok(db) if db.entries().is_empty() => State::Failed {
            err: format!("no unit files in {}", dir.display()),
            dir,
        },
        Ok(db) => State::Loaded { db, dir },
        Err(e) => State::Failed {
            err: e.to_string(),
            dir,
        },
    }
}

/// Indices of the entries matching `kind` (None = all) and the free-text
/// query (`Unit::matches`: every token, case-insensitive), sorted by maker
/// then name.
fn filter_units(entries: &[Entry], kind: Option<Kind>, q: &str) -> Vec<usize> {
    let mut v: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| kind.is_none_or(|k| e.unit.kind == k) && e.unit.matches(q))
        .map(|(i, _)| i)
        .collect();
    v.sort_by_cached_key(|&i| {
        let u = &entries[i].unit;
        (u.maker.to_lowercase(), u.name.to_lowercase())
    });
    v
}

fn confidence_tone(c: Confidence) -> Tone {
    match c {
        Confidence::Verified => Tone::Ok,
        Confidence::Community => Tone::Accent,
        Confidence::Unverified => Tone::Warn,
    }
}

fn possible_label(p: Possible) -> &'static str {
    match p {
        Possible::Yes => "possible",
        Possible::No => "not possible",
        Possible::Unknown => "unknown",
    }
}

fn possible_tone(p: Possible) -> Tone {
    match p {
        Possible::Yes => Tone::Ok,
        Possible::No => Tone::Muted,
        Possible::Unknown => Tone::Warn,
    }
}

fn obd_tone(o: ObdAccess) -> Tone {
    match o {
        ObdAccess::ReadWrite | ObdAccess::Read => Tone::Ok,
        ObdAccess::WriteOnly => Tone::Accent,
        ObdAccess::None => Tone::Muted,
        ObdAccess::Unknown => Tone::Warn,
    }
}

fn security_label(s: SecurityNeed) -> &'static str {
    match s {
        SecurityNeed::Required => "required",
        SecurityNeed::NotRequired => "not required",
        SecurityNeed::Unknown => "unknown",
    }
}

fn security_tone(s: SecurityNeed) -> Tone {
    match s {
        SecurityNeed::Required => Tone::Accent,
        SecurityNeed::NotRequired => Tone::Ok,
        SecurityNeed::Unknown => Tone::Muted,
    }
}

fn kind_label(k: Kind) -> &'static str {
    match k {
        Kind::Ecu => "ECU",
        Kind::Tcu => "TCU",
        Kind::Awd => "AWD",
    }
}

fn bus_label(b: Bus) -> &'static str {
    match b {
        Bus::Can => "CAN",
        Bus::Kline => "K-line",
    }
}

fn protocol_label(p: Protocol) -> &'static str {
    match p {
        Protocol::Kwp2000 => "KWP2000",
        Protocol::Uds => "UDS",
        Protocol::Tp20 => "TP2.0",
        Protocol::Kwp1281 => "KWP1281",
    }
}

fn signal_label(s: Signal) -> &'static str {
    match s {
        Signal::V12Perm => "+12 V permanent",
        Signal::V12Ign => "+12 V ignition",
        Signal::Gnd => "ground",
        Signal::CanH => "CAN high",
        Signal::CanL => "CAN low",
        Signal::Kline => "K-line",
        Signal::Boot => "boot",
        Signal::Other => "other",
    }
}

fn entry_label(e: BootEntry) -> &'static str {
    match e {
        BootEntry::ConnectorPin => "connector pin",
        BootEntry::BoardPad => "board pad / test point",
        BootEntry::BdmJtag => "BDM / JTAG header",
        BootEntry::None => "none",
        BootEntry::Unknown => "unknown",
    }
}

fn reachable_label(r: Reachable) -> &'static str {
    match r {
        Reachable::ExternalConnector => "external connector (case closed)",
        Reachable::OpenCase => "open case",
        Reachable::Unknown => "unknown",
    }
}

fn strap_label(a: StrapAction) -> &'static str {
    match a {
        StrapAction::PullToGnd => "pull to GND",
        StrapAction::PullToVcc => "pull to VCC",
        StrapAction::Bridge => "bridge",
        StrapAction::Connect => "connect probe",
    }
}

/// Flash size: `512 KiB`, `2 MiB (2048 KiB)`, or `unknown`.
fn flash_text(kib: Option<u32>) -> String {
    match kib {
        None => "unknown".into(),
        Some(k) if k >= 1024 && k.is_multiple_of(1024) => format!("{} MiB ({k} KiB)", k / 1024),
        Some(k) => format!("{k} KiB"),
    }
}

/// Source references as the numbers the Sources list shows (1-based).
fn refs(idx: &[usize]) -> String {
    idx.iter()
        .map(|i| format!("[{}]", i + 1))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The URL to open for a source, or `None` when it is not a plain http(s)
/// link (shown as text then).
fn link_target(url: &str) -> Option<&str> {
    let u = url.trim();
    let lower = u.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))?;
    (!rest.is_empty() && !u.chars().any(char::is_whitespace)).then_some(u)
}

/// A compact display form: no scheme, no `www.`, no trailing slash, at most
/// `max` characters (ellipsised).
fn short_url(url: &str, max: usize) -> String {
    let u = url.trim();
    let u = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
        .unwrap_or(u);
    let u = u.strip_prefix("www.").unwrap_or(u);
    let u = u.strip_suffix('/').unwrap_or(u);
    if u.chars().count() <= max {
        u.to_string()
    } else {
        let keep: String = u.chars().take(max.saturating_sub(1)).collect();
        format!("{keep}…")
    }
}

/// The vehicle line: `make model · engine · years`.
fn vehicle_text(v: &opentune_db::Vehicle) -> String {
    let mut s = format!("{} {}", v.make, v.model);
    for x in [&v.engine, &v.years].into_iter().flatten() {
        s.push_str(" · ");
        s.push_str(x);
    }
    s
}

impl EcuDbView {
    pub fn ui(&mut self, ui: &mut Ui, t: &Tokens) {
        if self.state.is_none() {
            self.reload();
        }
        let mut retry = false;
        match self.state.as_ref() {
            Some(State::Loaded { .. }) => {}
            Some(State::Missing { tried }) => {
                let list = tried
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                theme::empty_state(
                    ui,
                    t,
                    icons::FOLDER_DASHED,
                    "No ECU database found",
                    &format!(
                        "Looked for a database folder in: {list}. Start OpenTune from the \
                         repository, or put database/ next to the executable."
                    ),
                );
                retry = retry_button(ui, t);
            }
            Some(State::Failed { dir, err }) => {
                theme::empty_state(
                    ui,
                    t,
                    icons::WARNING,
                    "The ECU database did not load",
                    &format!("{}: {err}", dir.display()),
                );
                retry = retry_button(ui, t);
            }
            None => {}
        }
        if retry {
            self.reload();
        }
        let Some(State::Loaded { db, dir }) = &self.state else {
            return;
        };
        let entries = db.entries();
        let key = (self.search.clone(), self.kind);
        if self.filter_key.as_ref() != Some(&key) {
            self.filtered = filter_units(entries, self.kind, &self.search);
            self.filter_key = Some(key);
        }
        if self.selected.is_none_or(|i| i >= entries.len()) {
            self.selected = self.filtered.first().copied();
        }

        let (left, right) = two_pane(ui);
        let dir = dir.display().to_string();
        let mut reload = false;
        let mut pick = None;
        ui.scope_builder(UiBuilder::new().max_rect(left), |ui| {
            let (p, r) = list_pane(
                ui,
                t,
                entries,
                &self.filtered,
                self.selected,
                &mut self.search,
                &mut self.kind,
                &dir,
            );
            pick = p;
            reload = r;
        });
        if pick.is_some() {
            self.selected = pick;
        }
        ui.scope_builder(UiBuilder::new().max_rect(right), |ui| {
            match self.selected.and_then(|i| entries.get(i)) {
                Some(e) => detail_pane(ui, t, &e.unit),
                None => {
                    theme::panel(
                        ui,
                        t,
                        "Unit",
                        |_| {},
                        |ui| {
                            theme::empty_state(
                                ui,
                                t,
                                icons::CPU,
                                "Pick a unit",
                                "Select a control unit on the left.",
                            )
                        },
                    );
                }
            }
        });
        if reload {
            self.reload();
        }
    }

    fn reload(&mut self) {
        self.state = Some(load());
        self.filter_key = None;
        self.selected = None;
    }

    /// Number of units, once loaded (sidebar count).
    pub fn count(&self) -> Option<usize> {
        match &self.state {
            Some(State::Loaded { db, .. }) => Some(db.entries().len()),
            _ => None,
        }
    }
}

fn retry_button(ui: &mut Ui, t: &Tokens) -> bool {
    ui.add_space(12.0);
    ui.vertical_centered(|ui| {
        theme::button(ui, t, Some(icons::ARROWS_CLOCKWISE), "Try again").clicked()
    })
    .inner
}

/// Returns (clicked unit, reload requested).
#[allow(clippy::too_many_arguments)]
fn list_pane(
    ui: &mut Ui,
    t: &Tokens,
    entries: &[Entry],
    filtered: &[usize],
    selected: Option<usize>,
    search: &mut String,
    kind: &mut Option<Kind>,
    dir: &str,
) -> (Option<usize>, bool) {
    let mut pick = None;
    let mut reload = false;
    theme::panel(
        ui,
        t,
        "Control units",
        |ui| {
            if theme::icon_button(ui, t, icons::ARROWS_CLOCKWISE, &format!("Reload {dir}"))
                .clicked()
            {
                reload = true;
            }
            let txt = if filtered.len() == entries.len() {
                entries.len().to_string()
            } else {
                format!("{} / {}", filtered.len(), entries.len())
            };
            theme::pill(ui, t, &txt, Tone::Muted, true);
        },
        |ui| {
            egui::Frame::new()
                .inner_margin(Margin::symmetric(12, 10))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(icons::MAGNIFYING_GLASS).color(t.fg_faint));
                        ui.add(
                            egui::TextEdit::singleline(search)
                                .hint_text("Name, maker, part no., vehicle")
                                .desired_width(f32::INFINITY),
                        );
                    });
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        for (k, l) in [
                            (None, "All"),
                            (Some(Kind::Ecu), "ECU"),
                            (Some(Kind::Tcu), "TCU"),
                            (Some(Kind::Awd), "AWD"),
                        ] {
                            if ui.selectable_label(*kind == k, l).clicked() {
                                *kind = k;
                            }
                        }
                    });
                });
            theme::hline(ui, t.border_soft);
            if filtered.is_empty() {
                ui.add_space(18.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("No unit matches the filter.").color(t.fg_dim));
                });
                return;
            }
            egui::ScrollArea::vertical()
                .id_salt("ecudb-list")
                .auto_shrink([false, false])
                .show_rows(ui, ROW_H, filtered.len(), |ui, range| {
                    for &i in &filtered[range] {
                        let u = &entries[i].unit;
                        let w = ui.available_width();
                        let (r, resp) = ui.allocate_exact_size(Vec2::new(w, ROW_H), Sense::click());
                        unit_row(ui, t, r, &resp, u, selected == Some(i));
                        if resp.clicked() {
                            pick = Some(i);
                        }
                    }
                });
        },
    );
    (pick, reload)
}

fn unit_row(ui: &mut Ui, t: &Tokens, r: Rect, resp: &egui::Response, u: &Unit, selected: bool) {
    let bg = r.shrink2(Vec2::new(6.0, 1.0));
    if selected {
        ui.painter()
            .rect_filled(bg, CornerRadius::same(theme::RADIUS_SM), t.sel);
        ui.painter().rect_filled(
            Rect::from_min_size(
                bg.min + Vec2::new(0.0, 9.0),
                Vec2::new(3.0, bg.height() - 18.0),
            ),
            CornerRadius::same(2),
            t.accent,
        );
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(bg, CornerRadius::same(theme::RADIUS_SM), t.surface2);
    }
    let inner = r.shrink2(Vec2::new(16.0, 6.0));
    // Kind tag, top right.
    ui.painter().text(
        egui::pos2(inner.right(), inner.top() + 8.0),
        egui::Align2::RIGHT_CENTER,
        kind_label(u.kind),
        font(UI_STRONG, 10.0),
        t.fg_faint,
    );
    let text = Rect::from_min_max(inner.min, egui::pos2(inner.right() - 34.0, inner.bottom()));
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(text)
            .layout(Layout::top_down(Align::Min)),
        |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add(
                Label::new(
                    RichText::new(&u.name)
                        .font(font(if selected { UI_STRONG } else { UI_MEDIUM }, 13.0))
                        .color(t.fg),
                )
                .truncate()
                .selectable(false),
            );
            ui.add(
                Label::new(
                    RichText::new(format!("{} · {} · {}", u.maker, u.family, u.id))
                        .font(font(MONO_MEDIUM, 11.0))
                        .color(t.fg_faint),
                )
                .truncate()
                .selectable(false),
            );
        },
    );
}

fn detail_pane(ui: &mut Ui, t: &Tokens, u: &Unit) {
    theme::panel(
        ui,
        t,
        &u.name,
        |ui| {
            if let Some(c) = u.pinout_confidence {
                theme::pill(
                    ui,
                    t,
                    &format!("pinout {}", c.as_str()),
                    confidence_tone(c),
                    false,
                );
            }
            theme::pill(ui, t, kind_label(u.kind), Tone::Muted, false);
        },
        |ui| {
            egui::ScrollArea::vertical()
                .id_salt(("ecudb-detail", &u.id))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Frame::new()
                        .inner_margin(Margin::symmetric(16, 14))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            unit_body(ui, t, u);
                        });
                });
        },
    );
}

fn unit_body(ui: &mut Ui, t: &Tokens, u: &Unit) {
    if let Some(s) = &u.safety_note {
        callout(ui, t, icons::WARNING, t.warn, t.crit_soft, s);
        ui.add_space(12.0);
    }

    theme::section_label(ui, t, "Identity");
    ui.add_space(4.0);
    egui::Grid::new("db-identity")
        .num_columns(2)
        .spacing(Vec2::new(18.0, 7.0))
        .show(ui, |ui| {
            kv(ui, t, "ID", &u.id);
            kv(ui, t, "Maker", &u.maker);
            kv(ui, t, "Family", &u.family);
            if !u.aliases.is_empty() {
                kv(ui, t, "Aliases", &u.aliases.join(", "));
            }
            kv(ui, t, "Processor", u.cpu.as_deref().unwrap_or("–"));
            let mut flash = flash_text(u.flash_kib);
            if let Some(n) = &u.flash_note {
                flash.push_str(&format!(" — {n}"));
            }
            kv(ui, t, "Flash", &flash);
            if let Some(e) = &u.eeprom {
                kv(ui, t, "EEPROM", e);
            }
            let bus: Vec<&str> = u.bus.iter().map(|b| bus_label(*b)).collect();
            kv(ui, t, "Bus", &bus.join(", "));
            let proto: Vec<&str> = u.protocols.iter().map(|p| protocol_label(*p)).collect();
            kv(ui, t, "Protocols", &proto.join(", "));
            kv(ui, t, "Checksum", u.checksum.as_deref().unwrap_or("–"));
            kv(ui, t, "ECU definition", u.ecudef.as_deref().unwrap_or("–"));
            if !u.part_number_examples.is_empty() {
                kv(ui, t, "Part numbers", &u.part_number_examples.join(", "));
            }
            if !u.vehicles.is_empty() {
                let v: Vec<String> = u.vehicles.iter().map(vehicle_text).collect();
                kv(ui, t, "Vehicles", &v.join("\n"));
            }
        });
    if !u.notes.is_empty() {
        ui.add_space(8.0);
        dim_text(ui, t, &u.notes);
    }

    section(ui, t, "Read methods");
    let rm = &u.read_methods;
    ui.horizontal_wrapped(|ui| {
        theme::pill(
            ui,
            t,
            &format!("OBD {}", rm.obd.as_str()),
            obd_tone(rm.obd),
            false,
        );
        let yn = |b: bool| if b { Tone::Ok } else { Tone::Muted };
        theme::pill(
            ui,
            t,
            if rm.bench { "bench: yes" } else { "bench: no" },
            yn(rm.bench),
            false,
        );
        theme::pill(
            ui,
            t,
            if rm.boot { "boot: yes" } else { "boot: no" },
            yn(rm.boot),
            false,
        );
    });
    if !rm.notes.is_empty() {
        ui.add_space(4.0);
        dim_text(ui, t, &rm.notes);
    }

    section(ui, t, "Access profile");
    match &u.access {
        Some(a) => access_cards(ui, t, a),
        None => {
            dim_text(
                ui,
                t,
                "This record has no access profile yet (OBD, bench, boot and security \
                 access are not filled in).",
            );
        }
    }

    section(ui, t, "Pinout");
    pinout(ui, t, u);

    if let Some(b) = &u.bench {
        section(ui, t, "Bench wiring");
        egui::Grid::new("db-bench")
            .num_columns(2)
            .spacing(Vec2::new(18.0, 7.0))
            .show(ui, |ui| {
                for (k, v) in [
                    ("Power", &b.power),
                    ("Ground", &b.ground),
                    ("Ignition", &b.ign),
                    ("CAN high", &b.can_h),
                    ("CAN low", &b.can_l),
                    ("K-line", &b.kline),
                    ("Boot", &b.boot),
                ] {
                    if !v.is_empty() {
                        kv(ui, t, k, &v.join(", "));
                    }
                }
            });
        if !b.notes.is_empty() {
            ui.add_space(6.0);
            dim_text(ui, t, &b.notes);
        }
        if let Some(r) = &b.resistor_notes {
            ui.add_space(4.0);
            dim_text(ui, t, r);
        }
    }

    section(ui, t, "Sources");
    if u.sources.is_empty() {
        dim_text(ui, t, "No sources cited.");
    }
    for (i, s) in u.sources.iter().enumerate() {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(
                RichText::new(format!("[{}]", i + 1))
                    .font(font(MONO_MEDIUM, 11.5))
                    .color(t.fg_faint),
            );
            ui.label(RichText::new(&s.title).color(t.fg));
            match link_target(&s.url) {
                Some(url) => {
                    ui.hyperlink_to(
                        RichText::new(short_url(url, 64)).font(font(MONO_MEDIUM, 11.5)),
                        url,
                    );
                }
                None if !s.url.trim().is_empty() => {
                    ui.label(
                        RichText::new(s.url.trim())
                            .font(font(MONO_MEDIUM, 11.5))
                            .color(t.fg_dim),
                    );
                }
                None => {}
            }
        });
    }
}

fn section(ui: &mut Ui, t: &Tokens, title: &str) {
    ui.add_space(16.0);
    theme::hline(ui, t.border_soft);
    ui.add_space(10.0);
    theme::section_label(ui, t, title);
    ui.add_space(4.0);
}

fn dim_text(ui: &mut Ui, t: &Tokens, s: &str) {
    ui.add(Label::new(RichText::new(s).color(t.fg_dim)).wrap());
}

fn callout(ui: &mut Ui, t: &Tokens, icon: &str, fg: egui::Color32, bg: egui::Color32, s: &str) {
    egui::Frame::new()
        .fill(bg)
        .stroke(Stroke::new(1.0, fg.gamma_multiply(0.4)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.label(RichText::new(icon).size(18.0).color(fg));
                ui.add(Label::new(RichText::new(s).color(t.fg)).wrap());
            });
        });
}

/// One labelled fact inside an access card.
fn fact(ui: &mut Ui, t: &Tokens, key: &str, value: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(
            RichText::new(key.to_uppercase())
                .font(font(UI_STRONG, 10.0))
                .color(t.fg_faint),
        );
        ui.label(
            RichText::new(value)
                .font(font(MONO_MEDIUM, 11.5))
                .color(t.fg),
        );
    });
}

fn pill_fact(ui: &mut Ui, t: &Tokens, key: &str, value: &str, tone: Tone) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(
            RichText::new(key.to_uppercase())
                .font(font(UI_STRONG, 10.0))
                .color(t.fg_faint),
        );
        theme::pill(ui, t, value, tone, false);
    });
}

fn sources_line(ui: &mut Ui, t: &Tokens, idx: &[usize]) {
    if idx.is_empty() {
        return;
    }
    ui.add_space(2.0);
    ui.label(
        RichText::new(format!("sources {}", refs(idx)))
            .font(font(MONO_MEDIUM, 10.5))
            .color(t.fg_faint),
    );
}

/// A card of the access profile: icon, title, confidence pill, body.
fn card(
    ui: &mut Ui,
    t: &Tokens,
    w: f32,
    icon: &str,
    title: &str,
    conf: Confidence,
    body: impl FnOnce(&mut Ui),
) {
    ui.allocate_ui_with_layout(Vec2::new(w, 0.0), Layout::top_down(Align::Min), |ui| {
        egui::Frame::new()
            .fill(t.surface2)
            .stroke(Stroke::new(1.0, t.border_soft))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(w - 24.0);
                ui.spacing_mut().item_spacing.y = 5.0;
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon).size(16.0).color(t.accent));
                    ui.label(
                        RichText::new(title)
                            .font(font(DISPLAY_MEDIUM, 13.5))
                            .color(t.fg),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        theme::pill(ui, t, conf.as_str(), confidence_tone(conf), false)
                            .on_hover_text(match conf {
                                Confidence::Verified => "two or more independent sources agree",
                                Confidence::Community => "one credible public source",
                                Confidence::Unverified => "no usable source, or sources disagree",
                            });
                    });
                });
                body(ui);
            });
    });
}

fn access_cards(ui: &mut Ui, t: &Tokens, a: &Access) {
    let gap = 12.0;
    let avail = ui.available_width();
    let two = avail >= 560.0;
    let w = if two { (avail - gap) / 2.0 } else { avail };

    let obd = |ui: &mut Ui| {
        card(ui, t, w, icons::CAR, "OBD", a.obd.confidence, |ui| {
            pill_fact(ui, t, "Mode", a.obd.mode.as_str(), obd_tone(a.obd.mode));
            fact(
                ui,
                t,
                "Protocol",
                a.obd.protocol.as_deref().unwrap_or("unknown"),
            );
            dim_text(ui, t, &a.obd.notes);
            sources_line(ui, t, &a.obd.sources);
        })
    };
    let bench = |ui: &mut Ui| {
        card(ui, t, w, icons::PLUGS, "Bench", a.bench.confidence, |ui| {
            pill_fact(
                ui,
                t,
                "Closed case",
                possible_label(a.bench.possible),
                possible_tone(a.bench.possible),
            );
            dim_text(ui, t, &a.bench.how);
            sources_line(ui, t, &a.bench.sources);
        })
    };
    let boot = |ui: &mut Ui| {
        let b = &a.boot;
        card(
            ui,
            t,
            w,
            icons::CIRCUITRY,
            "Boot mode",
            b.confidence,
            |ui| {
                pill_fact(
                    ui,
                    t,
                    "Boot",
                    possible_label(b.possible),
                    possible_tone(b.possible),
                );
                fact(ui, t, "Entry", entry_label(b.entry));
                fact(ui, t, "Reach", reachable_label(b.reachable));
                if let Some(m) = &b.mcu_mode {
                    fact(ui, t, "Mode", m);
                }
                for p in &b.points {
                    let mut s = format!("{} — {} · {}", p.name, p.location, strap_label(p.action));
                    if let Some(r) = p.resistor_ohms {
                        s.push_str(&format!(" ({r} Ω)"));
                    }
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.label(RichText::new(icons::DOT_OUTLINE).color(t.fg_faint));
                        ui.label(RichText::new(s).font(font(MONO_MEDIUM, 11.5)).color(t.fg));
                        theme::pill(
                            ui,
                            t,
                            p.confidence.as_str(),
                            confidence_tone(p.confidence),
                            false,
                        );
                    })
                    .response
                    .on_hover_text(p.note.as_deref().unwrap_or(""));
                }
                if let Some(r) = &b.reset_timing {
                    fact(ui, t, "Timing", r);
                }
                dim_text(ui, t, &b.notes);
                sources_line(ui, t, &b.sources);
            },
        )
    };
    let security = |ui: &mut Ui| {
        let s = &a.security;
        card(ui, t, w, icons::SHIELD, "Security", s.confidence, |ui| {
            pill_fact(
                ui,
                t,
                "Diagnostic read",
                security_label(s.diag_read),
                security_tone(s.diag_read),
            );
            pill_fact(
                ui,
                t,
                "Flash write",
                security_label(s.flash_write),
                security_tone(s.flash_write),
            );
            dim_text(ui, t, &s.notes);
            sources_line(ui, t, &s.sources);
        })
    };

    if two {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            obd(ui);
            bench(ui);
        });
        ui.add_space(gap - 6.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            boot(ui);
            security(ui);
        });
    } else {
        for f in [&obd as &dyn Fn(&mut Ui), &bench, &boot, &security] {
            f(ui);
            ui.add_space(gap - 6.0);
        }
    }
}

fn pinout(ui: &mut Ui, t: &Tokens, u: &Unit) {
    if !u.has_pinout() {
        dim_text(ui, t, "No sourced pinout in this record.");
        return;
    }
    egui::ScrollArea::horizontal()
        .id_salt("db-pinout")
        .auto_shrink([false, true])
        .show(ui, |ui| {
            egui::Grid::new("db-pins")
                .striped(true)
                .num_columns(6)
                .spacing(Vec2::new(18.0, 6.0))
                .show(ui, |ui| {
                    for h in [
                        "Connector",
                        "Pin",
                        "Function",
                        "Signal",
                        "Confidence",
                        "Src",
                    ] {
                        ui.label(
                            RichText::new(h.to_uppercase())
                                .font(font(UI_STRONG, 10.5))
                                .color(t.fg_faint),
                        );
                    }
                    ui.end_row();
                    let m = |s: &str, c| RichText::new(s).font(font(MONO_MEDIUM, 11.5)).color(c);
                    for c in &u.connectors {
                        for (k, p) in c.pins.iter().enumerate() {
                            if k == 0 {
                                let r = ui.label(m(&c.name, t.fg));
                                if let Some(d) = &c.description {
                                    r.on_hover_text(d);
                                }
                            } else {
                                ui.label("");
                            }
                            ui.label(m(&p.pin, t.fg));
                            let f = ui.label(RichText::new(&p.function).color(t.fg));
                            if let Some(n) = &p.note {
                                f.on_hover_text(n);
                            }
                            ui.label(m(signal_label(p.signal), t.fg_dim));
                            let conf = u.pin_confidence(p);
                            theme::pill(ui, t, conf.as_str(), confidence_tone(conf), false);
                            ui.label(m(&refs(&p.sources), t.fg_faint));
                            ui.end_row();
                        }
                    }
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn unit(id: &str, kind: &str, maker: &str, name: &str) -> Entry {
        let raw = serde_json::json!({
            "id": id, "kind": kind, "maker": maker, "family": "FAM", "name": name,
            "cpu": null, "flash_kib": null, "eeprom": null, "bus": ["can"],
            "protocols": ["uds"],
            "read_methods": {"obd": "read", "bench": false, "boot": false, "notes": ""},
            "checksum": null, "ecudef": null,
            "vehicles": [{"make": "VW", "model": "Passat", "engine": "2.0 TDI"}],
            "part_number_examples": ["03L 906 018"], "notes": "", "connectors": [],
            "bench": null, "pinout_confidence": null, "safety_note": null, "sources": []
        });
        opentune_db::parse_entry(&raw.to_string(), PathBuf::from(format!("{id}.json")))
            .unwrap_or_else(|e| panic!("test unit: {e}"))
    }

    fn sample() -> Vec<Entry> {
        vec![
            unit("z-tcu", "tcu", "ZF", "8HP"),
            unit("b-ecu", "ecu", "Bosch", "EDC17"),
            unit("a-ecu", "ecu", "Bosch", "EDC15"),
            unit("h-awd", "awd", "BorgWarner", "Haldex"),
        ]
    }

    #[test]
    fn filter_by_kind_and_text_sorted_by_maker_then_name() {
        let e = sample();
        // Sorted: BorgWarner Haldex, Bosch EDC15, Bosch EDC17, ZF 8HP.
        assert_eq!(filter_units(&e, None, ""), vec![3, 2, 1, 0]);
        assert_eq!(filter_units(&e, Some(Kind::Ecu), ""), vec![2, 1]);
        assert_eq!(filter_units(&e, Some(Kind::Awd), ""), vec![3]);
        assert_eq!(filter_units(&e, None, "edc17"), vec![1]);
        assert_eq!(
            filter_units(&e, Some(Kind::Tcu), "bosch"),
            Vec::<usize>::new()
        );
        // Vehicles and part numbers are searchable too (Unit::matches).
        assert_eq!(filter_units(&e, None, "passat").len(), 4);
        assert_eq!(filter_units(&e, Some(Kind::Ecu), "03L906018"), vec![2, 1]);
    }

    #[test]
    fn confidence_and_state_tones() {
        assert_eq!(confidence_tone(Confidence::Verified), Tone::Ok);
        assert_eq!(confidence_tone(Confidence::Community), Tone::Accent);
        assert_eq!(confidence_tone(Confidence::Unverified), Tone::Warn);
        assert_eq!(possible_label(Possible::Yes), "possible");
        assert_eq!(possible_label(Possible::No), "not possible");
        assert_eq!(possible_label(Possible::Unknown), "unknown");
        assert_eq!(possible_tone(Possible::Unknown), Tone::Warn);
        assert_eq!(obd_tone(ObdAccess::Unknown), Tone::Warn);
        assert_eq!(obd_tone(ObdAccess::ReadWrite), Tone::Ok);
        assert_eq!(security_tone(SecurityNeed::NotRequired), Tone::Ok);
        assert_eq!(security_label(SecurityNeed::NotRequired), "not required");
    }

    #[test]
    fn urls_and_references() {
        assert_eq!(
            link_target("https://example.org/a"),
            Some("https://example.org/a")
        );
        assert_eq!(
            link_target("  HTTP://example.org "),
            Some("HTTP://example.org")
        );
        assert_eq!(link_target("https://"), None);
        assert_eq!(link_target("javascript:alert(1)"), None);
        assert_eq!(link_target("file:///etc/passwd"), None);
        assert_eq!(link_target("https://a b"), None);
        assert_eq!(link_target(""), None);
        assert_eq!(short_url("https://www.example.org/x/", 40), "example.org/x");
        assert_eq!(short_url("http://example.org", 40), "example.org");
        assert_eq!(short_url("https://example.org/abcdef", 8), "example…");
        assert_eq!(
            short_url("https://example.org/abcdef", 8).chars().count(),
            8
        );
        assert_eq!(refs(&[0, 2]), "[1] [3]");
        assert_eq!(refs(&[]), "");
    }

    #[test]
    fn sizes_and_labels() {
        assert_eq!(flash_text(None), "unknown");
        assert_eq!(flash_text(Some(512)), "512 KiB");
        assert_eq!(flash_text(Some(2048)), "2 MiB (2048 KiB)");
        assert_eq!(flash_text(Some(1568)), "1568 KiB");
        assert_eq!(signal_label(Signal::CanH), "CAN high");
        assert_eq!(protocol_label(Protocol::Tp20), "TP2.0");
        assert_eq!(
            vehicle_text(&opentune_db::Vehicle {
                make: "VW".into(),
                model: "Golf".into(),
                engine: None,
                years: Some("1998-2005".into()),
            }),
            "VW Golf · 1998-2005"
        );
    }

    #[test]
    fn first_existing_directory_wins() {
        let base = std::env::temp_dir().join(format!("ot-native-ecudb-{}", std::process::id()));
        let b = base.join("b");
        std::fs::create_dir_all(&b).unwrap_or_else(|e| panic!("{e}"));
        let c = vec![base.join("a"), b.clone(), base.clone()];
        assert_eq!(first_dir(&c), Some(b));
        assert_eq!(first_dir(&[base.join("none")]), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The repository's own database loads and every unit renders its
    /// labels (catches a schema enum the view does not map).
    #[test]
    fn repository_database_loads() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../database");
        if !dir.is_dir() {
            return;
        }
        let db = Database::load_dir(&dir).unwrap_or_else(|e| panic!("{e}"));
        assert!(!db.entries().is_empty());
        assert_eq!(
            filter_units(db.entries(), None, "").len(),
            db.entries().len()
        );
        for e in db.entries() {
            for s in &e.unit.sources {
                assert!(link_target(&s.url).is_some(), "{}: {}", e.unit.id, s.url);
            }
        }
    }
}
