//! Controller definitions browser (`opentune-ctrldef`): drivetrain units
//! (TCU, AWD couplings, transfer cases, differentials). Port of the webview
//! app's Controllers panel (`feat_ctrl.rs`, `feat_ctrl.js`).
//!
//! The list ([`catalog`]) is loaded lazily the first time the view is shown:
//! the built-in `ctrldefs/` (first existing of `ctrldefs`, `../ctrldefs`,
//! `<exe dir>/ctrldefs`, ...), the `controllers/` of every bundled pack
//! (`packs/`), pack folders the user loads and single definition files the
//! user opens. Every definition is validated: structure, honesty, file name
//! and links (bundled ECU definitions, database units, checksum ids, the
//! pack's own ECU definitions); files that do not load are listed, never
//! dropped.
//! Left: search + kind filter + the definitions (name, maker / family,
//! protocols, source, check counts). Right: safety notes, identity and
//! identification keys, the diagnostic channels (transport and addressing),
//! CAN broadcast frames with their signals, live channels, adaptations,
//! tables, gear sets, an inline flash layout, the checks, the sources, and
//! an offline CAN-log decode with the definition's frame sets ([`decode`]).
//! Read-only: nothing here talks to a unit.

mod catalog;
mod decode;

use self::catalog::{Entry, FailedFile, Loaded, Sources};
use crate::icons;
use crate::theme::{self, font, Tokens, Tone, MONO_MEDIUM, UI_MEDIUM, UI_STRONG};
use crate::views::maps::{kv, two_pane};
use eframe::egui::{
    self, Align, CornerRadius, Label, Layout, Margin, Rect, RichText, Sense, Stroke, Ui, UiBuilder,
    Vec2,
};
use opentune_core::ecudef::{DataType, Endian, FullRead};
use opentune_core::{DiagLayer, Protocol};
use opentune_ctrldef::{
    AdaptationKind, ByteOrder, CanFrameDef, Confidence, ControllerDef, ControllerKind, DiagChannel,
    LiveSource, Severity, Signal, SignalKind, TableAxis, TableDef, TableRole, ValidationState,
};
use std::path::PathBuf;

/// Height of a definition row in the list.
const ROW_H: f32 = 46.0;

type State = Loaded;

/// The kind filter chips.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KindFilter {
    Tcu,
    Awd,
    /// Transfer case, differential, other.
    Other,
}

impl KindFilter {
    fn accepts(self, k: ControllerKind) -> bool {
        match self {
            KindFilter::Tcu => k == ControllerKind::Tcu,
            KindFilter::Awd => k == ControllerKind::Awd,
            KindFilter::Other => !matches!(k, ControllerKind::Tcu | ControllerKind::Awd),
        }
    }
}

#[derive(Default)]
pub struct ControllersView {
    state: Option<State>,
    /// Pack folders and definition files the user added (kept on reload).
    extra: Vec<String>,
    files: Vec<PathBuf>,
    search: String,
    kind: Option<KindFilter>,
    selected: Option<usize>,
    filter_key: Option<(String, Option<KindFilter>)>,
    filtered: Vec<usize>,
    decode: decode::Decode,
}

/// What the list pane asked for.
enum ListAction {
    None,
    Reload,
    AddPack(PathBuf),
    AddFile(PathBuf),
}

/// Transports and service layers of the diagnostic channels, plus CAN
/// broadcast when the definition decodes frames (`TP2.0 KWP2000 · CAN`).
fn protocols(d: &ControllerDef) -> String {
    let mut v: Vec<String> = Vec::new();
    for c in &d.diagnostics {
        let s = format!("{} {}", transport_short(c.transport), layer_label(c.layer));
        if !v.contains(&s) {
            v.push(s);
        }
    }
    if !d.buses.is_empty() {
        v.push("CAN broadcast".into());
    }
    if v.is_empty() {
        "no diagnostics defined".into()
    } else {
        v.join(" · ")
    }
}

/// Case-insensitive: every token of `q` occurs in the id, name, maker,
/// family, vehicles, identification keys or protocols.
fn def_matches(d: &ControllerDef, q: &str) -> bool {
    let mut hay = vec![
        d.id.clone(),
        d.name.clone(),
        d.maker.clone(),
        d.family.clone(),
        kind_label(d.kind).to_string(),
        protocols(d),
    ];
    hay.extend(d.vehicles.iter().cloned());
    hay.extend(d.identify.part_numbers.iter().cloned());
    hay.extend(d.identify.sw_numbers.iter().cloned());
    hay.extend(d.identify.gearbox_codes.iter().cloned());
    let hay = hay.join("\n").to_lowercase();
    q.split_whitespace()
        .all(|tok| hay.contains(&tok.to_lowercase()))
}

/// Indices of the definitions matching `kind` and the query, sorted by
/// maker then name.
fn filter_defs(defs: &[Entry], kind: Option<KindFilter>, q: &str) -> Vec<usize> {
    let mut v: Vec<usize> = defs
        .iter()
        .enumerate()
        .filter(|(_, e)| kind.is_none_or(|k| k.accepts(e.def.kind)) && def_matches(&e.def, q))
        .map(|(i, _)| i)
        .collect();
    v.sort_by_cached_key(|&i| {
        let d = &defs[i].def;
        (d.maker.to_lowercase(), d.name.to_lowercase())
    });
    v
}

fn kind_label(k: ControllerKind) -> &'static str {
    match k {
        ControllerKind::Tcu => "TCU",
        ControllerKind::Awd => "AWD",
        ControllerKind::TransferCase => "Transfer case",
        ControllerKind::Differential => "Differential",
        ControllerKind::Other => "Other",
    }
}

fn validation_tone(v: ValidationState) -> Tone {
    match v {
        ValidationState::Implemented => Tone::Muted,
        ValidationState::SyntheticTested => Tone::Accent,
        ValidationState::RealImageTested
        | ValidationState::BenchTested
        | ValidationState::VehicleTested => Tone::Ok,
    }
}

fn transport_short(p: Protocol) -> &'static str {
    match p {
        Protocol::IsoTp => "ISO-TP",
        Protocol::Vwtp20 => "TP2.0",
        Protocol::Iso14230Kline => "K-line",
        Protocol::Iso9141 => "ISO 9141",
        Protocol::RawCan => "raw CAN",
    }
}

fn transport_label(p: Protocol) -> &'static str {
    match p {
        Protocol::IsoTp => "ISO-TP (ISO 15765)",
        Protocol::Vwtp20 => "VW TP2.0",
        Protocol::Iso14230Kline => "K-line (ISO 14230)",
        Protocol::Iso9141 => "K-line (ISO 9141-2)",
        Protocol::RawCan => "raw CAN",
    }
}

fn layer_label(l: DiagLayer) -> &'static str {
    match l {
        DiagLayer::Uds => "UDS",
        DiagLayer::Kwp2000 => "KWP2000",
    }
}

fn is_kline(p: Protocol) -> bool {
    matches!(p, Protocol::Iso14230Kline | Protocol::Iso9141)
}

/// CAN bit rate (`500 kbit/s`) or K-line baud rate.
fn rate(bits: u32, kline: bool) -> String {
    if kline {
        format!("{bits} baud")
    } else if bits >= 1000 && bits.is_multiple_of(1000) {
        format!("{} kbit/s", bits / 1000)
    } else {
        format!("{bits} bit/s")
    }
}

/// `0x7E1`, or eight digits for a 29-bit id.
fn can_id(id: u32, extended: bool) -> String {
    if extended {
        format!("0x{id:08X}")
    } else {
        format!("0x{id:03X}")
    }
}

fn opt_hex8(v: Option<u8>) -> String {
    v.map(|b| format!("0x{b:02X}"))
        .unwrap_or_else(|| "–".into())
}

/// The unit's address on its channel: TP2.0 logical address or K-line target.
fn diag_address(c: &DiagChannel) -> String {
    match (c.tp20_address, c.kline_address) {
        (Some(a), _) => format!("TP2.0 0x{a:02X}"),
        (None, Some(a)) => format!("K 0x{a:02X}"),
        (None, None) => "–".into(),
    }
}

fn adaptation_label(k: AdaptationKind) -> &'static str {
    match k {
        AdaptationKind::Adaptation => "adaptation",
        AdaptationKind::BasicSetting => "basic setting",
        AdaptationKind::Routine => "routine",
        AdaptationKind::Reset => "reset",
    }
}

fn role_label(r: TableRole) -> &'static str {
    match r {
        TableRole::ShiftUp => "upshift",
        TableRole::ShiftDown => "downshift",
        TableRole::ShiftTime => "shift time",
        TableRole::ShiftPressure => "shift pressure",
        TableRole::FillTime => "fill time",
        TableRole::FillPressure => "fill pressure",
        TableRole::TccSlip => "TCC slip",
        TableRole::TccPressure => "TCC pressure",
        TableRole::TorqueLimit => "torque limit",
        TableRole::Adaptation => "adaptation",
        TableRole::Launch => "launch",
        TableRole::CouplingLock => "coupling lock",
        TableRole::Other => "other",
    }
}

fn confidence_label(c: Confidence) -> &'static str {
    match c {
        Confidence::High => "high",
        Confidence::Medium => "medium",
        Confidence::Low => "low",
        Confidence::StructureOnly => "structure only",
    }
}

fn byte_order_label(o: ByteOrder) -> &'static str {
    match o {
        ByteOrder::Little => "Intel",
        ByteOrder::Big => "Motorola",
    }
}

fn signal_kind_label(k: SignalKind) -> &'static str {
    match k {
        SignalKind::Number => "number",
        SignalKind::Bool => "bool",
        SignalKind::Enum => "enum",
        SignalKind::Ascii => "ascii",
    }
}

fn dtype(d: DataType) -> String {
    format!("{d:?}").to_lowercase()
}

fn endian(e: Endian) -> &'static str {
    match e {
        Endian::Big => "BE",
        Endian::Little => "LE",
    }
}

fn full_read_label(f: FullRead) -> &'static str {
    match f {
        FullRead::Obd => "over OBD",
        FullRead::BenchBoot => "bench / boot mode",
    }
}

/// A number without a pointless `.0`.
fn num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// `× factor + offset`, or the curve's point count.
fn linear(factor: f64, offset: f64) -> String {
    let mut s = format!("× {}", num(factor));
    if offset > 0.0 {
        s.push_str(&format!(" + {}", num(offset)));
    } else if offset < 0.0 {
        s.push_str(&format!(" − {}", num(-offset)));
    }
    s
}

fn signal_scaling(s: &Signal) -> String {
    if !s.curve.is_empty() {
        format!("curve, {} points", s.curve.len())
    } else {
        linear(s.factor, s.offset)
    }
}

/// Bits as `start|len` plus the byte order, DBC style.
fn signal_bits(s: &Signal) -> String {
    format!("{}|{} {}", s.start, s.len, byte_order_label(s.byte_order))
}

fn signal_type(s: &Signal) -> String {
    let mut k = signal_kind_label(s.kind).to_string();
    if s.signed {
        k.push_str(", signed");
    }
    k
}

/// Named values, invalid markers and the valid raw range, compactly.
fn signal_values(s: &Signal) -> String {
    let mut v = Vec::new();
    if !s.values.is_empty() {
        let shown: Vec<String> = s
            .values
            .iter()
            .take(4)
            .map(|e| format!("{}={}", e.raw, e.name))
            .collect();
        let more = s.values.len().saturating_sub(4);
        v.push(if more > 0 {
            format!("{} +{more}", shown.join(", "))
        } else {
            shown.join(", ")
        });
    }
    if !s.invalid.is_empty() {
        let inv: Vec<String> = s.invalid.iter().map(|x| format!("0x{x:X}")).collect();
        v.push(format!("invalid {}", inv.join("/")));
    }
    if let Some([a, b]) = s.raw_range {
        v.push(format!("raw {a}…{b}"));
    }
    if v.is_empty() {
        "–".into()
    } else {
        v.join(" · ")
    }
}

fn live_source(s: &LiveSource) -> String {
    match s {
        LiveSource::CanSignal { bus, frame, signal } => format!("CAN {bus} / {frame} / {signal}"),
        LiveSource::KwpMeasuringBlock { group, cell } => {
            format!("KWP block {group}, cell {cell}")
        }
        LiveSource::UdsDid {
            did,
            byte,
            len,
            signed,
            factor,
            offset,
        } => format!(
            "UDS DID 0x{did:04X} · byte {byte}, {len} B{} · {}",
            if *signed { " signed" } else { "" },
            linear(*factor, *offset)
        ),
    }
}

fn axis_text(a: &TableAxis) -> String {
    let mut s = if a.name.is_empty() {
        "unnamed".to_string()
    } else {
        a.name.clone()
    };
    if !a.unit.is_empty() {
        s.push_str(&format!(" [{}]", a.unit));
    }
    if !a.values.is_empty() {
        let shown: Vec<String> = a.values.iter().take(6).map(|v| num(*v)).collect();
        let more = if a.values.len() > 6 { " …" } else { "" };
        s.push_str(&format!(": {}{more}", shown.join(", ")));
    } else if !a.labels.is_empty() {
        let shown: Vec<&str> = a.labels.iter().take(6).map(String::as_str).collect();
        let more = if a.labels.len() > 6 { " …" } else { "" };
        s.push_str(&format!(": {}{more}", shown.join(", ")));
    } else if !a.derived.is_empty() {
        s.push_str(&format!(": {} ({} points)", a.derived, a.len()));
    }
    s
}

fn table_storage(t: &TableDef) -> String {
    if let Some(s) = &t.storage {
        format!(
            "0x{:06X} · {} {} · {}",
            s.address,
            dtype(s.data_type),
            endian(s.endian),
            linear(s.factor, s.offset)
        )
    } else if let Some(r) = &t.ecudef_map {
        format!("ecudef {} / {}", r.ecudef, r.map)
    } else {
        "not located".into()
    }
}

fn frame_title(f: &CanFrameDef) -> String {
    let mut s = format!("{}  {}", can_id(f.id, f.extended), f.name);
    if !f.sender.is_empty() {
        s.push_str(&format!(" · from {}", f.sender));
    }
    if let Some(ms) = f.cycle_ms {
        s.push_str(&format!(" · {ms} ms"));
    }
    s.push_str(&format!(
        " · {} signal{}",
        f.signals.len(),
        if f.signals.len() == 1 { "" } else { "s" }
    ));
    s
}

/// The URL when it is a web link.
fn web_link(url: &str) -> Option<&str> {
    let u = url.trim();
    (u.starts_with("https://") || u.starts_with("http://")).then_some(u)
}

impl ControllersView {
    pub fn ui(&mut self, ui: &mut Ui, t: &Tokens) {
        if self.state.is_none() {
            self.reload();
        }
        let mut retry = false;
        match self.state.as_ref() {
            Some(Loaded::Missing { tried }) => {
                let list = tried
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                theme::empty_state(
                    ui,
                    t,
                    icons::FOLDER_DASHED,
                    "No controller definitions found",
                    &format!(
                        "Looked for a ctrldefs folder in: {list}. Start OpenTune from the \
                         repository, or put ctrldefs/ next to the executable."
                    ),
                );
                retry = retry_button(ui, t);
            }
            Some(Loaded::Failed { dir, err }) => {
                theme::empty_state(
                    ui,
                    t,
                    icons::WARNING,
                    "The controller definitions did not load",
                    &format!("{}: {err}", dir.display()),
                );
                retry = retry_button(ui, t);
            }
            Some(Loaded::Ok { .. }) | None => {}
        }
        if retry {
            self.reload();
        }
        let Some(Loaded::Ok { defs, failed, dir }) = &self.state else {
            return;
        };
        let key = (self.search.clone(), self.kind);
        if self.filter_key.as_ref() != Some(&key) {
            self.filtered = filter_defs(defs, self.kind, &self.search);
            self.filter_key = Some(key);
        }
        if self.selected.is_none_or(|i| i >= defs.len()) {
            self.selected = self.filtered.first().copied();
        }

        let (left, right) = two_pane(ui);
        let dir = dir.display().to_string();
        let mut action = ListAction::None;
        let mut pick = None;
        ui.scope_builder(UiBuilder::new().max_rect(left), |ui| {
            let (p, a) = list_pane(
                ui,
                t,
                defs,
                failed,
                &self.filtered,
                self.selected,
                &mut self.search,
                &mut self.kind,
                &dir,
            );
            pick = p;
            action = a;
        });
        if pick.is_some() {
            self.selected = pick;
        }
        let decode = &mut self.decode;
        ui.scope_builder(UiBuilder::new().max_rect(right), |ui| {
            match self.selected.and_then(|i| defs.get(i)) {
                Some(e) => detail_pane(ui, t, e, decode),
                None => {
                    theme::panel(
                        ui,
                        t,
                        "Controller",
                        |_| {},
                        |ui| {
                            theme::empty_state(
                                ui,
                                t,
                                icons::CIRCUITRY,
                                "Pick a controller",
                                "Select a controller definition on the left.",
                            )
                        },
                    );
                }
            }
        });
        match action {
            ListAction::None => {}
            ListAction::Reload => self.reload(),
            ListAction::AddPack(p) => {
                let p = p.display().to_string();
                if !self.extra.contains(&p) {
                    self.extra.push(p);
                }
                self.reload();
            }
            ListAction::AddFile(p) => {
                if !self.files.contains(&p) {
                    self.files.push(p.clone());
                }
                self.reload();
                self.select_path(&p);
            }
        }
    }

    fn sources(&self) -> Sources {
        Sources {
            extra: self.extra.clone(),
            files: self.files.clone(),
            ..Sources::bundled()
        }
    }

    fn reload(&mut self) {
        self.state = Some(catalog::load(&self.sources()));
        self.filter_key = None;
        self.selected = None;
    }

    /// Select the definition loaded from `p` (clearing the filter so it is
    /// visible).
    fn select_path(&mut self, p: &std::path::Path) {
        if let Some(Loaded::Ok { defs, .. }) = &self.state {
            if let Some(i) = defs.iter().position(|e| e.path == p) {
                self.search.clear();
                self.kind = None;
                self.selected = Some(i);
            }
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

/// Returns (clicked definition, what else the pane asked for).
#[allow(clippy::too_many_arguments)]
fn list_pane(
    ui: &mut Ui,
    t: &Tokens,
    defs: &[Entry],
    failed: &[FailedFile],
    filtered: &[usize],
    selected: Option<usize>,
    search: &mut String,
    kind: &mut Option<KindFilter>,
    dir: &str,
) -> (Option<usize>, ListAction) {
    let mut pick = None;
    let mut action = ListAction::None;
    theme::panel(
        ui,
        t,
        "Controllers",
        |ui| {
            if theme::icon_button(ui, t, icons::ARROWS_CLOCKWISE, &format!("Reload {dir}"))
                .clicked()
            {
                action = ListAction::Reload;
            }
            if theme::icon_button(ui, t, icons::FILE_PLUS, "Open a definition file…").clicked() {
                if let Some(p) = rfd::FileDialog::new()
                    .set_title("Controller definition")
                    .add_filter("Controller definition", &["json"])
                    .pick_file()
                {
                    action = ListAction::AddFile(p);
                }
            }
            if theme::icon_button(ui, t, icons::FOLDER_PLUS, "Load a definition pack…").clicked()
            {
                if let Some(p) = rfd::FileDialog::new()
                    .set_title("Definition pack folder")
                    .pick_folder()
                {
                    action = ListAction::AddPack(p);
                }
            }
            let txt = if filtered.len() == defs.len() {
                defs.len().to_string()
            } else {
                format!("{} / {}", filtered.len(), defs.len())
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
                                .hint_text("Name, maker, part no., gearbox code")
                                .desired_width(f32::INFINITY),
                        );
                    });
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        for (k, l) in [
                            (None, "All"),
                            (Some(KindFilter::Tcu), "TCU"),
                            (Some(KindFilter::Awd), "AWD"),
                            (Some(KindFilter::Other), "Other"),
                        ] {
                            if ui.selectable_label(*kind == k, l).clicked() {
                                *kind = k;
                            }
                        }
                    });
                    if !failed.is_empty() {
                        ui.add_space(4.0);
                        egui::CollapsingHeader::new(
                            RichText::new(format!(
                                "{} {} file{} did not load",
                                icons::WARNING,
                                failed.len(),
                                if failed.len() == 1 { "" } else { "s" }
                            ))
                            .color(t.warn),
                        )
                        .id_salt("ctrl-failed")
                        .show(ui, |ui| {
                            for f in failed {
                                let name = std::path::Path::new(&f.file)
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| f.file.clone());
                                ui.add(
                                    Label::new(
                                        RichText::new(format!("{name} — {}", f.message))
                                            .small()
                                            .color(t.fg_dim),
                                    )
                                    .wrap(),
                                )
                                .on_hover_text(&f.file);
                            }
                        });
                    }
                });
            theme::hline(ui, t.border_soft);
            if filtered.is_empty() {
                ui.add_space(18.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new("No controller definition matches the filter.")
                            .color(t.fg_dim),
                    );
                });
                return;
            }
            egui::ScrollArea::vertical()
                .id_salt("ctrl-list")
                .auto_shrink([false, false])
                .show_rows(ui, ROW_H, filtered.len(), |ui, range| {
                    for &i in &filtered[range] {
                        let e = &defs[i];
                        let w = ui.available_width();
                        let (r, resp) = ui.allocate_exact_size(Vec2::new(w, ROW_H), Sense::click());
                        def_row(ui, t, r, &resp, e, selected == Some(i));
                        if resp.clicked() {
                            pick = Some(i);
                        }
                    }
                });
        },
    );
    (pick, action)
}

fn def_row(ui: &mut Ui, t: &Tokens, r: Rect, resp: &egui::Response, e: &Entry, selected: bool) {
    let d = &e.def;
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
    ui.painter().text(
        egui::pos2(inner.right(), inner.top() + 8.0),
        egui::Align2::RIGHT_CENTER,
        kind_label(d.kind).to_uppercase(),
        font(UI_STRONG, 10.0),
        t.fg_faint,
    );
    let (errs, warns) = (e.errors(), e.warnings());
    if errs + warns > 0 {
        ui.painter().text(
            egui::pos2(inner.right(), inner.bottom() - 8.0),
            egui::Align2::RIGHT_CENTER,
            if errs > 0 {
                format!("{errs} ERR")
            } else {
                format!("{warns} WARN")
            },
            font(UI_STRONG, 9.5),
            if errs > 0 { t.crit } else { t.warn },
        );
    }
    let text = Rect::from_min_max(inner.min, egui::pos2(inner.right() - 40.0, inner.bottom()));
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(text)
            .layout(Layout::top_down(Align::Min)),
        |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add(
                Label::new(
                    RichText::new(&d.name)
                        .font(font(if selected { UI_STRONG } else { UI_MEDIUM }, 13.0))
                        .color(t.fg),
                )
                .truncate()
                .selectable(false),
            );
            ui.add(
                Label::new(
                    RichText::new(match e.source {
                        "builtin" => format!("{} · {} · {}", d.maker, d.family, protocols(d)),
                        _ => format!("{} · {} · {}", e.source_label(), d.maker, d.family),
                    })
                    .font(font(MONO_MEDIUM, 11.0))
                    .color(t.fg_faint),
                )
                .truncate()
                .selectable(false),
            );
        },
    );
}

fn detail_pane(ui: &mut Ui, t: &Tokens, e: &Entry, dec: &mut decode::Decode) {
    let d = &e.def;
    theme::panel(
        ui,
        t,
        &d.name,
        |ui| {
            theme::pill(
                ui,
                t,
                d.validation.as_str(),
                validation_tone(d.validation),
                false,
            )
            .on_hover_text("evidence level of this definition");
            theme::pill(ui, t, kind_label(d.kind), Tone::Muted, false);
            if e.source != "builtin" {
                theme::pill(ui, t, &e.source_label(), Tone::Accent, false);
            }
        },
        |ui| {
            egui::ScrollArea::vertical()
                .id_salt(("ctrl-detail", &d.id))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Frame::new()
                        .inner_margin(Margin::symmetric(16, 14))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            body(ui, t, e);
                            if !d.buses.is_empty() {
                                section(ui, t, "Decode a CAN log", 0);
                                dec.ui(ui, t, &e.path, d);
                            }
                        });
                });
        },
    );
}

fn body(ui: &mut Ui, t: &Tokens, e: &Entry) {
    let (path, d) = (&e.path, &e.def);
    if !d.safety.is_empty() {
        callout(ui, t, &d.safety);
        ui.add_space(12.0);
    }

    theme::section_label(ui, t, "Identity");
    ui.add_space(4.0);
    egui::Grid::new("ctrl-identity")
        .num_columns(2)
        .spacing(Vec2::new(18.0, 7.0))
        .show(ui, |ui| {
            kv(ui, t, "ID", &d.id);
            kv(ui, t, "Kind", kind_label(d.kind));
            kv(ui, t, "Maker", &d.maker);
            kv(ui, t, "Family", &d.family);
            kv(ui, t, "Validation", d.validation.as_str());
            if !d.vehicles.is_empty() {
                kv(ui, t, "Vehicles", &d.vehicles.join("\n"));
            }
            let id = &d.identify;
            for (k, v) in [
                ("Part numbers", &id.part_numbers),
                ("Software numbers", &id.sw_numbers),
                ("Gearbox codes", &id.gearbox_codes),
            ] {
                if !v.is_empty() {
                    kv(ui, t, k, &v.join(", "));
                }
            }
            if !d.ecudefs.is_empty() {
                kv(ui, t, "ECU definitions", &d.ecudefs.join(", "));
            }
            if !d.units.is_empty() {
                kv(ui, t, "Database units", &d.units.join(", "));
            }
            kv(ui, t, "Checksum", d.checksum.as_deref().unwrap_or("–"));
            kv(ui, t, "Source", &e.source_label());
            kv(ui, t, "File", &path.display().to_string());
        });
    if !d.notes.is_empty() {
        ui.add_space(8.0);
        dim_text(ui, t, &d.notes);
    }

    section(ui, t, "Diagnostics", d.diagnostics.len());
    if d.diagnostics.is_empty() {
        dim_text(ui, t, "No diagnostic channel is defined for this unit.");
    } else {
        table(
            ui,
            t,
            "ctrl-diag",
            &[
                "Name",
                "Transport",
                "Layer",
                "Rate",
                "Request",
                "Response",
                "Address",
                "Session",
                "Ident.",
                "Validation",
            ],
            d.diagnostics
                .iter()
                .map(|c| Row {
                    cells: vec![
                        c.name.clone(),
                        transport_label(c.transport).into(),
                        layer_label(c.layer).into(),
                        rate(c.bitrate, is_kline(c.transport)),
                        c.tx_id
                            .map(|i| can_id(i, c.extended_ids))
                            .unwrap_or_else(|| "–".into()),
                        c.rx_id
                            .map(|i| can_id(i, c.extended_ids))
                            .unwrap_or_else(|| "–".into()),
                        diag_address(c),
                        opt_hex8(c.session),
                        c.identification.clone().unwrap_or_else(|| "–".into()),
                        c.validation.as_str().into(),
                    ],
                    notes: c.notes.clone(),
                })
                .collect(),
        );
    }

    if !d.buses.is_empty() {
        let frames: usize = d.buses.iter().map(|b| b.frames.len()).sum();
        section(ui, t, "CAN broadcast", frames);
        for b in &d.buses {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(&b.name)
                        .font(font(MONO_MEDIUM, 12.5))
                        .color(t.fg),
                );
                if !b.description.is_empty() {
                    ui.label(RichText::new(&b.description).color(t.fg_dim));
                }
                theme::pill(ui, t, &rate(b.bitrate, false), Tone::Muted, true);
            });
            if !b.notes.is_empty() {
                dim_text(ui, t, &b.notes);
            }
            for f in &b.frames {
                egui::CollapsingHeader::new(
                    RichText::new(frame_title(f))
                        .font(font(MONO_MEDIUM, 12.0))
                        .color(t.fg),
                )
                .id_salt(("ctrl-frame", &d.id, &b.name, f.id, &f.name))
                .show(ui, |ui| {
                    if !f.notes.is_empty() {
                        dim_text(ui, t, &f.notes);
                    }
                    let mut facts = Vec::new();
                    if let Some(n) = f.dlc {
                        facts.push(format!("DLC {n}"));
                    }
                    if let Some(n) = f.min_len {
                        facts.push(format!("decoded from {n} bytes"));
                    }
                    if let Some(v) = f.validation {
                        facts.push(v.as_str().to_string());
                    }
                    if !facts.is_empty() {
                        ui.label(
                            RichText::new(facts.join(" · "))
                                .font(font(MONO_MEDIUM, 11.0))
                                .color(t.fg_faint),
                        );
                    }
                    table(
                        ui,
                        t,
                        ("ctrl-sig", &d.id, &b.name, f.id),
                        &["Signal", "Bits", "Type", "Scaling", "Unit", "Values"],
                        f.signals
                            .iter()
                            .map(|s| Row {
                                cells: vec![
                                    s.name.clone(),
                                    signal_bits(s),
                                    signal_type(s),
                                    signal_scaling(s),
                                    if s.unit.is_empty() {
                                        "–".into()
                                    } else {
                                        s.unit.clone()
                                    },
                                    signal_values(s),
                                ],
                                notes: s.notes.clone(),
                            })
                            .collect(),
                    );
                });
            }
        }
    }

    if !d.live.is_empty() {
        section(ui, t, "Live channels", d.live.len());
        table(
            ui,
            t,
            "ctrl-live",
            &["Channel", "Unit", "Source"],
            d.live
                .iter()
                .map(|l| Row {
                    cells: vec![
                        l.name.clone(),
                        if l.unit.is_empty() {
                            "–".into()
                        } else {
                            l.unit.clone()
                        },
                        live_source(&l.source),
                    ],
                    notes: l.notes.clone(),
                })
                .collect(),
        );
    }

    if !d.adaptations.is_empty() {
        section(ui, t, "Adaptations and routines", d.adaptations.len());
        let yn = |b: bool| if b { "yes" } else { "no" }.to_string();
        table(
            ui,
            t,
            "ctrl-adapt",
            &[
                "Name",
                "Kind",
                "Service",
                "Writes",
                "Security",
                "Table",
                "Validation",
            ],
            d.adaptations
                .iter()
                .map(|a| Row {
                    cells: vec![
                        a.name.clone(),
                        adaptation_label(a.kind).into(),
                        if a.service.is_empty() {
                            "not public".into()
                        } else {
                            a.service.clone()
                        },
                        yn(a.writes),
                        yn(a.requires_security),
                        a.table.clone().unwrap_or_else(|| "–".into()),
                        a.validation.as_str().into(),
                    ],
                    notes: a.notes.clone(),
                })
                .collect(),
        );
    }

    if !d.tables.is_empty() {
        section(ui, t, "Tables", d.tables.len());
        table(
            ui,
            t,
            "ctrl-tables",
            &[
                "Table",
                "Role",
                "Size",
                "Unit",
                "X axis",
                "Y axis",
                "Storage",
                "Confidence",
            ],
            d.tables
                .iter()
                .map(|x| {
                    let mut name = x.name.clone();
                    if !x.profiles.is_empty() {
                        name.push_str(&format!(" ({})", x.profiles.join(", ")));
                    }
                    let mut notes = x.notes.clone();
                    if !x.source.is_empty() {
                        if !notes.is_empty() {
                            notes.push('\n');
                        }
                        notes.push_str(&format!("Source: {}", x.source));
                    }
                    Row {
                        cells: vec![
                            name,
                            role_label(x.role).into(),
                            format!("{} × {}", x.cols, x.rows),
                            if x.unit.is_empty() {
                                "–".into()
                            } else {
                                x.unit.clone()
                            },
                            axis_text(&x.x_axis),
                            x.y_axis
                                .as_ref()
                                .map(axis_text)
                                .unwrap_or_else(|| "–".into()),
                            table_storage(x),
                            confidence_label(x.confidence).into(),
                        ],
                        notes,
                    }
                })
                .collect(),
        );
    }

    if !d.gearsets.is_empty() {
        section(ui, t, "Gear ratios", d.gearsets.len());
        table(
            ui,
            t,
            "ctrl-gears",
            &["Variant", "Applies to", "Ratios"],
            d.gearsets
                .iter()
                .map(|g| {
                    let mut notes = g.notes.clone();
                    if !g.source.is_empty() {
                        if !notes.is_empty() {
                            notes.push('\n');
                        }
                        notes.push_str(&format!("Source: {}", g.source));
                    }
                    Row {
                        cells: vec![
                            g.name.clone(),
                            if g.applies_to.is_empty() {
                                "–".into()
                            } else {
                                g.applies_to.join(", ")
                            },
                            g.gears
                                .iter()
                                .map(|x| format!("{}: {}", x.gear, num(x.ratio)))
                                .collect::<Vec<_>>()
                                .join(" · "),
                        ],
                        notes,
                    }
                })
                .collect(),
        );
    }

    if let Some(f) = &d.flash {
        section(ui, t, "Flash layout", f.regions.len());
        egui::Grid::new("ctrl-flash")
            .num_columns(2)
            .spacing(Vec2::new(18.0, 7.0))
            .show(ui, |ui| {
                let size = if f.size == 0 {
                    "unknown".to_string()
                } else {
                    format!("{} bytes (0x{:X})", f.size, f.size)
                };
                kv(ui, t, "Image size", &size);
                kv(ui, t, "Full read", full_read_label(f.full_read));
            });
        if !f.regions.is_empty() {
            ui.add_space(6.0);
            table(
                ui,
                t,
                "ctrl-regions",
                &["Region", "Start", "End", "Length", "Writable"],
                f.regions
                    .iter()
                    .map(|r| Row {
                        cells: vec![
                            r.name.clone(),
                            format!("0x{:06X}", r.region.address),
                            format!("0x{:06X}", r.region.end().wrapping_sub(1)),
                            format!("0x{:X}", r.region.length),
                            if r.writable { "yes" } else { "no" }.into(),
                        ],
                        notes: String::new(),
                    })
                    .collect(),
            );
        }
        if !f.notes.is_empty() {
            ui.add_space(4.0);
            dim_text(ui, t, &f.notes);
        }
    }

    let issues = &e.issues;
    section(ui, t, "Definition checks", issues.len());
    if issues.is_empty() {
        ui.horizontal_wrapped(|ui| {
            theme::pill(ui, t, "checks OK", Tone::Ok, false);
            ui.label(
                RichText::new(
                    "opentune-ctrldef's structure, honesty and file-name rules pass, and every \
                     link (ECU definitions, database units, checksum ids) resolves.",
                )
                .color(t.fg_dim),
            );
        });
    }
    for i in issues {
        ui.horizontal_wrapped(|ui| {
            let tone = match i.severity {
                Severity::Error => Tone::Crit,
                Severity::Warn => Tone::Warn,
            };
            theme::pill(ui, t, i.severity.tag(), tone, true);
            ui.label(
                RichText::new(i.code)
                    .font(font(MONO_MEDIUM, 11.5))
                    .color(t.fg_dim),
            );
            let msg = if i.location.is_empty() {
                i.message.clone()
            } else {
                format!("{}: {}", i.location, i.message)
            };
            ui.add(Label::new(RichText::new(msg).color(t.fg)).wrap());
        });
    }

    section(ui, t, "Sources", d.sources.len());
    if d.sources.is_empty() {
        dim_text(ui, t, "No sources cited.");
    }
    for (i, s) in d.sources.iter().enumerate() {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(
                RichText::new(format!("[{}]", i + 1))
                    .font(font(MONO_MEDIUM, 11.5))
                    .color(t.fg_faint),
            );
            ui.label(RichText::new(&s.title).color(t.fg));
            theme::pill(ui, t, &s.license, Tone::Muted, true);
            match web_link(&s.url) {
                Some(url) => {
                    ui.hyperlink_to(RichText::new(url).font(font(MONO_MEDIUM, 11.5)), url);
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
        if !s.used_for.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(30.0);
                dim_text(ui, t, &s.used_for);
            });
        }
    }
}

/// A section rule and heading with a count.
fn section(ui: &mut Ui, t: &Tokens, title: &str, count: usize) {
    ui.add_space(16.0);
    theme::hline(ui, t.border_soft);
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        theme::section_label(ui, t, title);
        if count > 0 {
            ui.label(
                RichText::new(count.to_string())
                    .font(font(MONO_MEDIUM, 10.5))
                    .color(t.fg_faint),
            );
        }
    });
    ui.add_space(4.0);
}

fn dim_text(ui: &mut Ui, t: &Tokens, s: &str) {
    ui.add(Label::new(RichText::new(s).color(t.fg_dim)).wrap());
}

/// The safety notes, shown first.
fn callout(ui: &mut Ui, t: &Tokens, notes: &[String]) {
    egui::Frame::new()
        .fill(t.crit_soft)
        .stroke(Stroke::new(1.0, t.warn.gamma_multiply(0.4)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.label(RichText::new(icons::WARNING).size(18.0).color(t.warn));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    for n in notes {
                        ui.add(Label::new(RichText::new(n).color(t.fg)).wrap());
                    }
                });
            });
        });
}

/// One table row: the cells and free-text notes (hover on the first cell).
struct Row {
    cells: Vec<String>,
    notes: String,
}

/// A striped table with a header row; wide tables scroll horizontally.
fn table(
    ui: &mut Ui,
    t: &Tokens,
    id: impl std::hash::Hash + std::fmt::Debug,
    head: &[&str],
    rows: Vec<Row>,
) {
    let id = ui.make_persistent_id(id);
    egui::Frame::new()
        .stroke(Stroke::new(1.0, t.border_soft))
        .corner_radius(CornerRadius::same(theme::RADIUS_SM))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            egui::ScrollArea::horizontal()
                .id_salt(id.with("scroll"))
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    egui::Grid::new(id.with("grid"))
                        .num_columns(head.len())
                        .striped(true)
                        .min_row_height(22.0)
                        .spacing(Vec2::new(16.0, 6.0))
                        .show(ui, |ui| {
                            for h in head {
                                ui.label(
                                    RichText::new(h.to_uppercase())
                                        .font(font(UI_STRONG, 10.0))
                                        .color(t.fg_faint),
                                );
                            }
                            ui.end_row();
                            for r in &rows {
                                for (i, c) in r.cells.iter().enumerate() {
                                    if i == 0 {
                                        let txt = if r.notes.is_empty() {
                                            c.clone()
                                        } else {
                                            format!("{c}  {}", icons::INFO)
                                        };
                                        let resp = ui.label(
                                            RichText::new(txt)
                                                .font(font(UI_MEDIUM, 12.5))
                                                .color(t.fg),
                                        );
                                        if !r.notes.is_empty() {
                                            resp.on_hover_text(&r.notes);
                                        }
                                    } else {
                                        ui.label(
                                            RichText::new(c)
                                                .font(font(MONO_MEDIUM, 11.5))
                                                .color(t.fg_dim),
                                        );
                                    }
                                }
                                ui.end_row();
                            }
                        });
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn def(json: serde_json::Value) -> ControllerDef {
        serde_json::from_value(json).unwrap_or_else(|e| panic!("ctrldef: {e}"))
    }

    fn entry(path: &str, d: ControllerDef) -> Entry {
        Entry {
            path: PathBuf::from(path),
            def: d,
            source: "builtin",
            pack: None,
            issues: Vec::new(),
        }
    }

    fn sample() -> Vec<Entry> {
        let base = |id: &str, name: &str, kind: &str, maker: &str, family: &str| {
            serde_json::json!({
                "id": id, "name": name, "kind": kind, "maker": maker, "family": family,
                "validation": "implemented", "safety": ["s"],
                "sources": [{"title": "t", "license": "facts", "used_for": "u"}]
            })
        };
        let mut a = base("zf-test", "ZF eight-speed", "tcu", "ZF", "8HP");
        a["identify"] = serde_json::json!({"gearbox_codes": ["0BK"]});
        a["diagnostics"] = serde_json::json!([{
            "name": "uds", "transport": "iso_tp", "layer": "uds", "bitrate": 500000,
            "tx_id": "0x7E1", "rx_id": "0x7E9", "validation": "implemented"
        }]);
        let mut b = base("bw-test", "Haldex coupling", "awd", "BorgWarner", "Gen4");
        b["vehicles"] = serde_json::json!(["Passat CC 4Motion"]);
        b["diagnostics"] = serde_json::json!([{
            "name": "tp20", "transport": "vwtp20", "layer": "kwp2000", "bitrate": 500000,
            "tp20_address": "0x0A", "validation": "synthetic-tested"
        }]);
        b["buses"] = serde_json::json!([{"name": "pq", "bitrate": 500000, "frames": []}]);
        let c = base("x-diff", "Active diff", "differential", "Acme", "AD1");
        vec![
            entry("a.json", def(a)),
            entry("b.json", def(b)),
            entry("c.json", def(c)),
        ]
    }

    #[test]
    fn search_and_kind_filter_sorted_by_maker() {
        let d = sample();
        // Sorted by maker: Acme, BorgWarner, ZF.
        assert_eq!(filter_defs(&d, None, ""), vec![2, 1, 0]);
        assert_eq!(filter_defs(&d, Some(KindFilter::Tcu), ""), vec![0]);
        assert_eq!(filter_defs(&d, Some(KindFilter::Awd), ""), vec![1]);
        assert_eq!(filter_defs(&d, Some(KindFilter::Other), ""), vec![2]);
        assert_eq!(filter_defs(&d, None, "0bk"), vec![0], "gearbox code");
        assert_eq!(filter_defs(&d, None, "passat"), vec![1], "vehicle");
        assert_eq!(
            filter_defs(&d, None, "tp2.0 kwp"),
            vec![1],
            "protocol tokens"
        );
        assert_eq!(filter_defs(&d, None, "haldex gen4"), vec![1]);
        assert!(filter_defs(&d, None, "haldex uds").is_empty(), "all tokens");
        assert!(filter_defs(&d, Some(KindFilter::Tcu), "haldex").is_empty());
    }

    #[test]
    fn labels() {
        let d = sample();
        assert_eq!(protocols(&d[0].def), "ISO-TP UDS");
        assert_eq!(protocols(&d[1].def), "TP2.0 KWP2000 · CAN broadcast");
        assert_eq!(protocols(&d[2].def), "no diagnostics defined");
        assert_eq!(rate(500_000, false), "500 kbit/s");
        assert_eq!(rate(10_400, true), "10400 baud");
        assert_eq!(rate(33_333, false), "33333 bit/s");
        assert_eq!(can_id(0x7E1, false), "0x7E1");
        assert_eq!(can_id(0x18DA10F1, true), "0x18DA10F1");
        assert_eq!(diag_address(&d[1].def.diagnostics[0]), "TP2.0 0x0A");
        assert_eq!(diag_address(&d[0].def.diagnostics[0]), "–");
        assert_eq!(linear(0.75, -48.0), "× 0.75 − 48");
        assert_eq!(linear(1.0, 0.0), "× 1");
        assert_eq!(web_link(" https://x.org/a "), Some("https://x.org/a"));
        assert_eq!(web_link("local notes"), None);
        let s: Signal = serde_json::from_value(serde_json::json!({
            "name": "gear", "start": 12, "len": 4, "byte_order": "big", "kind": "enum",
            "values": [{"raw": 0, "name": "P"}, {"raw": 1, "name": "R"}],
            "invalid": [15], "raw_range": [0, 9]
        }))
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(signal_bits(&s), "12|4 Motorola");
        assert_eq!(signal_type(&s), "enum");
        assert_eq!(signal_values(&s), "0=P, 1=R · invalid 0xF · raw 0…9");
        assert_eq!(signal_scaling(&s), "× 1");
        let a = TableAxis {
            name: "pedal".into(),
            unit: "%".into(),
            values: vec![0.0, 12.5, 25.0],
            ..Default::default()
        };
        assert_eq!(axis_text(&a), "pedal [%]: 0, 12.5, 25");
        assert_eq!(
            live_source(&LiveSource::KwpMeasuringBlock { group: 1, cell: 2 }),
            "KWP block 1, cell 2"
        );
    }

    fn builtin_only(dirs: Vec<PathBuf>) -> Sources {
        Sources {
            builtin: dirs,
            ..Default::default()
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ot-native-ctrl-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap_or_else(|e| panic!("{e}"));
        d
    }

    /// A synthetic definition named after its file.
    fn synthetic(id: &str, extra: serde_json::Value) -> String {
        let mut v = serde_json::json!({
            "id": id, "name": "Synthetic", "kind": "awd", "maker": "Acme", "family": "SYN",
            "validation": "implemented", "safety": ["s"],
            "sources": [{"title": "t", "license": "facts", "used_for": "u"}]
        });
        if let (Some(o), Some(x)) = (v.as_object_mut(), extra.as_object()) {
            o.extend(x.clone());
        }
        v.to_string()
    }

    #[test]
    fn missing_directory_and_first_existing_wins() {
        let base = tmp("missing");
        let none = base.join("none");
        assert!(matches!(
            catalog::load(&builtin_only(vec![none.clone()])),
            Loaded::Missing { .. }
        ));
        let empty = base.join("empty");
        std::fs::create_dir_all(&empty).unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(
            catalog::load(&builtin_only(vec![none.clone(), empty.clone()])),
            Loaded::Failed { .. }
        ));
        let full = base.join("full");
        std::fs::create_dir_all(&full).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(
            full.join("syn-a.json"),
            synthetic("syn-a", serde_json::json!({})),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let Loaded::Ok { defs, dir, .. } = catalog::load(&builtin_only(vec![none, full.clone()]))
        else {
            panic!("did not load");
        };
        assert_eq!((defs.len(), dir), (1, full));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Packs and opened files join the list with their source; broken files
    /// are listed, not dropped; links are checked.
    #[test]
    fn packs_files_failures_and_link_checks() {
        let base = tmp("packs");
        let builtin = base.join("ctrldefs");
        std::fs::create_dir_all(&builtin).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(
            builtin.join("syn-a.json"),
            synthetic("syn-a", serde_json::json!({})),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        // A link to an ECU definition that does not exist.
        std::fs::write(
            builtin.join("syn-link.json"),
            synthetic(
                "syn-link",
                serde_json::json!({"ecudefs": ["no-such-ecudef"]}),
            ),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(builtin.join("broken.json"), "{ not json").unwrap_or_else(|e| panic!("{e}"));
        let pack = base.join("my-pack");
        std::fs::create_dir_all(pack.join("controllers")).unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(
            pack.join("pack.json"),
            r#"{"id":"ctrl-test","name":"Controller test pack","version":"0.1.0","license":"GPL-3.0-or-later"}"#,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(
            pack.join("controllers/syn-p.json"),
            synthetic("syn-p", serde_json::json!({})),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let loose = base.join("renamed.json");
        std::fs::write(&loose, synthetic("syn-loose", serde_json::json!({})))
            .unwrap_or_else(|e| panic!("{e}"));
        let src = Sources {
            builtin: vec![builtin.clone()],
            extra: vec![pack.display().to_string(), pack.display().to_string()],
            files: vec![
                loose.clone(),
                builtin.join("syn-a.json"),
                base.join("missing.json"),
            ],
            ..Default::default()
        };
        let Loaded::Ok { defs, failed, .. } = catalog::load(&src) else {
            panic!("did not load");
        };
        let by = |id: &str| {
            defs.iter()
                .find(|e| e.def.id == id)
                .unwrap_or_else(|| panic!("{id}"))
        };
        assert_eq!(
            defs.iter().filter(|e| e.def.id == "syn-a").count(),
            1,
            "no duplicates"
        );
        assert_eq!(
            defs.iter().filter(|e| e.def.id == "syn-p").count(),
            1,
            "pack listed once"
        );
        assert_eq!(by("syn-a").source_label(), "built-in");
        assert_eq!(by("syn-a").errors(), 0);
        assert_eq!(by("syn-p").source_label(), "pack ctrl-test v0.1.0");
        let l = by("syn-link");
        assert!(
            l.issues.iter().any(|i| i.code == "link.ecudef"),
            "{:?}",
            l.issues
        );
        assert!(l.errors() > 0);
        let f = by("syn-loose");
        assert_eq!(f.source_label(), "file");
        assert!(f.issues.iter().any(|i| i.code == "id.filename"));
        assert!(failed.iter().any(|x| x.file.ends_with("broken.json")));
        assert!(failed.iter().any(|x| x.file.ends_with("missing.json")));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The repository's own definitions (and bundled packs) load, every one
    /// renders its labels, and the shipped set passes every check incl. links.
    #[test]
    fn repository_ctrldefs_load() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        if !root.join("ctrldefs").is_dir() {
            return;
        }
        let src = Sources {
            builtin: vec![root.join("ctrldefs")],
            packs: Some(root.join("packs")).filter(|p| p.is_dir()),
            ecudefs: Some(root.join("ecudefs")),
            database: Some(root.join("database")),
            ..Default::default()
        };
        let Loaded::Ok { defs, .. } = catalog::load(&src) else {
            panic!("ctrldefs did not load");
        };
        assert_eq!(filter_defs(&defs, None, "").len(), defs.len());
        for e in &defs {
            let d = &e.def;
            assert!(!protocols(d).is_empty());
            assert!(d.tables.iter().all(|x| !table_storage(x).is_empty()));
            assert!(d.live.iter().all(|l| !live_source(&l.source).is_empty()));
            if e.source == "builtin" {
                assert_eq!(e.errors(), 0, "{}: {:?}", d.id, e.issues);
            }
        }
    }
}
