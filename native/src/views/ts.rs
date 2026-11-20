//! Aftermarket ECU (TunerStudio-compatible: Speeduino, rusEFI, MegaSquirt):
//! INI definitions with `#set` conditions, `.msq` tunes (load / save / diff),
//! validated offline edits of constants, tables and curves, and the serial
//! ECU online: connect + signature check, read the tune, back it up, plan a
//! write, poll realtime channels (port of the webview app's Aftermarket ECU
//! panel, `feat_ts.rs`, over `opentune-tsini` / `opentune-tsproto`).
//!
//! ## Per-op table (reviewed before merge)
//!
//! | Operation | Kind | Sent to the ECU | Confirm | Backup / files |
//! |---|---|---|---|---|
//! | Load INI / `.msq`, edit values, diff with a `.msq` | offline | nothing | — | — |
//! | Save `.msq` | offline | nothing | replacing a file: `Host::confirm` + `Host::write_file` | the replaced file is archived in the vault |
//! | Connect (port or `sim`) | read | `queryCommand` (signature) | — | refused when the port is the adapter link's |
//! | Read from ECU | read | page reads (`pageReadCommand`) | — | — (becomes the ECU tune and the working tune) |
//! | Back up ECU tune | read | page reads | — | `Host::backup(folder, "ts ECU tune pages", <all pages>)` and `Host::backup(folder, "ts ECU tune msq", <.msq XML>)`; refused without a folder |
//! | Plan write | offline | nothing | — | compares the working tune with the last ECU read |
//! | Live channels | read | `ochGetCommand` | — | — |
//! | Write / burn | mutate | **DISABLED** in this panel: the serial ECU is not on the vehicle adapter, and the host has no confirm + backup-gated write path for a non-adapter ECU yet (see the note in the panel) | — | — |
//!
//! Online code (`ts/online.rs`) only calls `tsproto::Client` methods that
//! take no `WriteAuth`, so nothing here can change the ECU.

mod model;
mod online;
pub(crate) mod save;
#[cfg(test)]
mod tests;

use super::coding::plumbing as pl;
use crate::host::{Host, JobHandle, NativeView, Tier, ViewGroup};
use crate::icons;
use crate::theme::{self, Tone};
use eframe::egui::{self, RichText};
use model::{Class, TsModel};
use online::{Conn, ConnectInfo};
use opentune_tsini::{ChannelSample, Tune, Value as TsValue};
use save::Saver;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Bench;

pub fn create() -> Box<dyn NativeView> {
    Box::new(TsView::default())
}

/// How often live channels are polled.
const POLL_EVERY: Duration = Duration::from_millis(200);

/// Why writing is off in this panel (shown next to the plan).
pub(crate) const WRITE_DISABLED: &str = "Writing and burning a tune is not available in the \
     native app yet: this serial ECU is not on the vehicle adapter, and the host has no \
     confirmed, backup-first write path for it. Review the plan, back up the ECU's tune, and \
     write with `opentune ts write` meanwhile.";

/// What the Edit card shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EditSel {
    Constants,
    Table(String),
    Curve(String),
}

type ConnResult<T> = (Conn, Result<T, String>);

/// One online operation in flight (it holds the connection).
pub(crate) enum Online {
    Connect(JobHandle<(Conn, ConnectInfo)>),
    Read(JobHandle<ConnResult<Tune>>),
    Backup(JobHandle<ConnResult<Tune>>),
    Poll(JobHandle<ConnResult<Vec<ChannelSample>>>),
}

pub struct TsView {
    pub(crate) model: TsModel,
    pub(crate) set_text: String,
    pub(crate) unset_text: String,
    pub(crate) force_msq: bool,
    pub(crate) offline_msg: Option<(Tone, String)>,
    /// Detail lines of the last `.msq` load or diff.
    pub(crate) report: Vec<String>,
    pub(crate) edit: EditSel,
    pub(crate) filter: String,
    /// The selected constant and its edit text.
    pub(crate) sel: Option<String>,
    pub(crate) edit_text: String,
    /// The selected array cell (constant, row-major index) and its text.
    pub(crate) cell: Option<(String, usize)>,
    pub(crate) cell_text: String,
    saver: Saver,
    // Online
    pub(crate) port: String,
    pub(crate) baud: u32,
    pub(crate) framing: String,
    pub(crate) force_sig: bool,
    pub(crate) conn: Option<Conn>,
    pub(crate) conn_info: Option<ConnectInfo>,
    pub(crate) op: Option<Online>,
    pub(crate) online_msg: Option<(Tone, String)>,
    pub(crate) plan: Option<Result<model::PlanView, String>>,
    pub(crate) records: pl::Records,
    pub(crate) last_backup: Vec<PathBuf>,
    pub(crate) live: bool,
    pub(crate) channels: String,
    pub(crate) samples: Vec<ChannelSample>,
    last_poll: Option<Instant>,
}

impl Default for TsView {
    fn default() -> Self {
        Self {
            model: TsModel::default(),
            set_text: String::new(),
            unset_text: String::new(),
            force_msq: false,
            offline_msg: None,
            report: Vec::new(),
            edit: EditSel::Constants,
            filter: String::new(),
            sel: None,
            edit_text: String::new(),
            cell: None,
            cell_text: String::new(),
            saver: Saver::default(),
            port: "sim".into(),
            baud: online::DEFAULT_BAUD,
            framing: "auto".into(),
            force_sig: false,
            conn: None,
            conn_info: None,
            op: None,
            online_msg: None,
            plan: None,
            records: pl::Records::default(),
            last_backup: Vec::new(),
            live: false,
            channels: String::new(),
            samples: Vec::new(),
            last_poll: None,
        }
    }
}

fn names(s: &str) -> BTreeSet<String> {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

impl TsView {
    // -- offline ------------------------------------------------------------

    /// Drop the connection if it was checked against another INI than the
    /// one loaded now (a reload or `.msq` conditions changed it).
    fn drop_stale_conn(&mut self) {
        let stale = match (&self.conn, &self.model.ini) {
            (Some(c), Some(i)) => !Arc::ptr_eq(&c.ini, i),
            (Some(_), None) => true,
            _ => false,
        };
        if stale {
            self.conn = None;
            self.conn_info = None;
            self.live = false;
            self.online_msg = Some((
                Tone::Muted,
                "the INI changed: the connection was closed (connect again)".into(),
            ));
        }
    }

    pub(crate) fn load_ini(&mut self, path: &Path) {
        if self.op.is_some() {
            return self.offline_msg = Some((Tone::Warn, "wait for the online operation".into()));
        }
        match self
            .model
            .load_ini(path, names(&self.set_text), names(&self.unset_text))
        {
            Ok(i) => {
                self.offline_msg = Some((
                    Tone::Ok,
                    format!(
                        "loaded {} ({}), working tune blank",
                        i.path.display(),
                        i.signature.as_deref().unwrap_or("no signature")
                    ),
                ));
                self.report.clear();
                self.plan = None;
                self.sel = None;
                self.cell = None;
                self.edit = EditSel::Constants;
            }
            Err(e) => self.offline_msg = Some((Tone::Crit, e)),
        }
        self.drop_stale_conn();
    }

    pub(crate) fn load_msq(&mut self, path: &Path) {
        match self.model.load_msq(path, self.force_msq) {
            Ok(l) => {
                let r = &l.report;
                self.offline_msg = Some((
                    Tone::Ok,
                    format!(
                        "loaded {} entries ({} applied) on a {} base",
                        l.entries,
                        r.applied,
                        if l.base_ecu { "ECU" } else { "blank" }
                    ),
                ));
                let mut lines = Vec::new();
                if !l.conditions_added.is_empty() {
                    lines.push(format!(
                        "INI re-read with the tune's settings: {}",
                        l.conditions_added.join(", ")
                    ));
                }
                lines.extend(r.notes.iter().cloned());
                lines.extend(r.unknown.iter().map(|n| format!("unknown: {n}")));
                lines.extend(r.skipped.iter().map(|(n, w)| format!("skipped {n}: {w}")));
                lines.extend(r.out_of_range.iter().map(|n| format!("out of range: {n}")));
                if let Some(s) = l.signature {
                    lines.push(format!("tune signature: {s}"));
                }
                self.report = lines;
                self.plan = None;
            }
            Err(e) => self.offline_msg = Some((Tone::Crit, e)),
        }
        self.drop_stale_conn();
    }

    pub(crate) fn save_msq(&mut self, path: PathBuf) {
        match self.model.working_xml() {
            Ok((xml, entries, skipped)) => {
                if !skipped.is_empty() {
                    self.report = skipped
                        .iter()
                        .map(|(n, w)| format!("not saved {n}: {w}"))
                        .collect();
                }
                if let Some(r) = self.saver.save("ts-save-msq", path, xml.into_bytes()) {
                    self.offline_msg = Some(saved(r, entries));
                }
            }
            Err(e) => self.offline_msg = Some((Tone::Crit, e)),
        }
    }

    pub(crate) fn diff_msq(&mut self, path: &Path) {
        match self.model.diff_msq(path, self.force_msq) {
            Ok((rows, _rep)) => {
                self.offline_msg = Some((
                    Tone::Ok,
                    format!("{} constant(s) differ (file → working tune)", rows.len()),
                ));
                self.report = rows.iter().map(|(n, c)| format!("{n}: {c}")).collect();
            }
            Err(e) => self.offline_msg = Some((Tone::Crit, e)),
        }
    }

    /// Apply the edit text to the selected (non-array) constant.
    pub(crate) fn apply_value(&mut self, name: &str, text: &str) -> Result<(), String> {
        let row = self.model.get(name)?;
        let v = match &row.class {
            Class::Scalar => {
                let x: f64 = text
                    .trim()
                    .parse()
                    .map_err(|_| format!("{name} needs a number"))?;
                serde_json::json!(x)
            }
            Class::String(_) => serde_json::json!(text),
            Class::Bits(_) => {
                let i: u64 = text
                    .trim()
                    .parse()
                    .map_err(|_| format!("{name} needs an option index"))?;
                serde_json::json!(i)
            }
            Class::Array { .. } => return Err(format!("{name} is an array: edit its cells")),
        };
        self.model.set_value(name, &v).map(|_| ())
    }

    /// Apply the cell text to the selected array cell.
    pub(crate) fn apply_cell(
        &mut self,
        name: &str,
        index: usize,
        text: &str,
    ) -> Result<(), String> {
        let x: f64 = text
            .trim()
            .parse()
            .map_err(|_| "a cell needs a number".to_string())?;
        self.model.set_cell(name, index, x)
    }

    pub(crate) fn request_plan(&mut self) {
        self.plan = Some(self.model.plan());
    }

    // -- online -------------------------------------------------------------

    pub(crate) fn request_connect(&mut self, host: &Host<'_>) {
        if self.op.is_some() {
            return;
        }
        let ini = match self.model.ini() {
            Ok(i) => i,
            Err(e) => return self.online_msg = Some((Tone::Crit, e)),
        };
        let framing = match online::parse_framing(&self.framing) {
            Ok(f) => f,
            Err(e) => return self.online_msg = Some((Tone::Crit, e)),
        };
        let port = self.port.trim().to_string();
        let link = host.link();
        if !port.eq_ignore_ascii_case("sim") && link.connected && link.port.trim() == port {
            return self.online_msg = Some((
                Tone::Crit,
                format!(
                    "{port} is the vehicle adapter's port: disconnect the adapter in the top bar \
                     first"
                ),
            ));
        }
        self.conn = None; // close a previous connection first
        self.conn_info = None;
        let working = self.model.working.clone();
        let (baud, force) = (self.baud, self.force_sig);
        self.online_msg = None;
        self.op = Some(Online::Connect(
            host.spawn_job("aftermarket ECU: connect", move |_| {
                online::connect(ini, working, &port, baud, framing, force)
            }),
        ));
    }

    pub(crate) fn disconnect(&mut self) {
        if self.op.is_none() {
            let was = self.conn.take().map(|c| c.label);
            self.conn_info = None;
            self.live = false;
            self.online_msg = Some((
                Tone::Muted,
                match was {
                    Some(l) => format!("disconnected {l}; the port is released"),
                    None => "not connected".into(),
                },
            ));
        }
    }

    fn take_conn(&mut self) -> Option<Conn> {
        if self.op.is_some() {
            return None;
        }
        let c = self.conn.take();
        if c.is_none() {
            self.online_msg = Some((Tone::Crit, "not connected: press Connect first".into()));
        }
        c
    }

    pub(crate) fn request_read(&mut self, host: &Host<'_>) {
        let Some(mut c) = self.take_conn() else {
            return;
        };
        self.op = Some(Online::Read(host.spawn_job(
            "aftermarket ECU: read tune",
            move |_| {
                let r = online::read_tune(&mut c);
                Ok((c, r))
            },
        )));
    }

    /// Read the ECU's whole tune and keep it in the vault (read-only).
    pub(crate) fn request_backup(&mut self, host: &Host<'_>) {
        if self.records.near(host).is_none() {
            return self.online_msg =
                Some((Tone::Crit, "choose a backup folder first (Folder…)".into()));
        }
        let Some(mut c) = self.take_conn() else {
            return;
        };
        self.op = Some(Online::Backup(host.spawn_job(
            "aftermarket ECU: back up tune",
            move |_| {
                let r = online::read_tune(&mut c);
                Ok((c, r))
            },
        )));
    }

    pub(crate) fn request_poll(&mut self, host: &Host<'_>) {
        let Some(mut c) = self.take_conn() else {
            return;
        };
        let tune = self.model.working.clone();
        let wanted: Vec<String> = self
            .channels
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .collect();
        self.last_poll = Some(Instant::now());
        self.op = Some(Online::Poll(host.spawn_job(
            "aftermarket ECU: poll",
            move |_| {
                let r = online::poll(&mut c, tune.as_ref(), &wanted);
                Ok((c, r))
            },
        )));
    }

    /// The connection is back from a job: keep it unless the INI changed.
    fn put_back(&mut self, c: Conn) {
        self.conn = Some(c);
        self.drop_stale_conn();
    }

    fn backed_up(&mut self, host: &Host<'_>, tune: Tune) {
        let (Some(near), Ok(ini)) = (self.records.near(host), self.model.ini()) else {
            self.online_msg = Some((Tone::Crit, "no backup folder; nothing kept".into()));
            return;
        };
        let pages = tune.pages.concat();
        let (xml, _, _) = model::tune_xml(&ini, &tune, "backup of the ECU's tune");
        let a = host.backup(&near, "ts ECU tune pages", &pages);
        let b = host.backup(&near, "ts ECU tune msq", xml.as_bytes());
        match (a, b) {
            (Ok(a), Ok(b)) => {
                self.online_msg = Some((
                    Tone::Ok,
                    format!(
                        "the ECU's tune ({} bytes) is in the vault: {} and {}",
                        a.len,
                        a.path.display(),
                        b.path.display()
                    ),
                ));
                self.last_backup = vec![a.path, b.path];
                // The fresh read is the ECU tune a plan compares against; the
                // working tune (the user's edits) is kept.
                self.model.ecu = Some(tune);
                self.plan = None;
            }
            (Err(e), _) | (_, Err(e)) => self.online_msg = Some((Tone::Crit, e)),
        }
    }

    pub(crate) fn tick(&mut self, host: &mut Host<'_>) {
        if let Some(r) = self.saver.tick(host) {
            self.offline_msg = Some(saved(r, 0));
        }
        let Some(op) = self.op.take() else {
            if self.live
                && self.conn.is_some()
                && self.last_poll.is_none_or(|t| t.elapsed() >= POLL_EVERY)
            {
                self.request_poll(host);
            }
            return;
        };
        match op {
            Online::Connect(mut j) => match j.take() {
                None => self.op = Some(Online::Connect(j)),
                Some(Ok((c, info))) => {
                    self.online_msg = Some((
                        if info.matches { Tone::Ok } else { Tone::Warn },
                        format!(
                            "connected: {} ({}), signature {:?}{}",
                            info.label,
                            if info.envelope { "msEnvelope" } else { "plain" },
                            info.ecu_signature,
                            if info.matches {
                                String::new()
                            } else {
                                format!(
                                    " does NOT match the INI's {:?} (forced)",
                                    info.ini_signature.as_deref().unwrap_or("")
                                )
                            }
                        ),
                    ));
                    self.conn_info = Some(info);
                    self.put_back(c);
                }
                Some(Err(e)) => self.online_msg = Some((Tone::Crit, e)),
            },
            Online::Read(mut j) => match j.take() {
                None => self.op = Some(Online::Read(j)),
                Some(Ok((c, r))) => {
                    match r {
                        Ok(t) => {
                            let n = t.pages.iter().map(Vec::len).sum::<usize>();
                            self.model.set_ecu_tune(t);
                            self.plan = None;
                            self.online_msg = Some((
                                Tone::Ok,
                                format!("read {n} bytes; it is now the working tune"),
                            ));
                        }
                        Err(e) => self.online_msg = Some((Tone::Crit, format!("read: {e}"))),
                    }
                    self.put_back(c);
                }
                Some(Err(e)) => self.online_msg = Some((Tone::Crit, e)),
            },
            Online::Backup(mut j) => match j.take() {
                None => self.op = Some(Online::Backup(j)),
                Some(Ok((c, r))) => {
                    match r {
                        Ok(t) => self.backed_up(host, t),
                        Err(e) => {
                            self.online_msg = Some((Tone::Crit, format!("backup read failed: {e}")))
                        }
                    }
                    self.put_back(c);
                }
                Some(Err(e)) => self.online_msg = Some((Tone::Crit, e)),
            },
            Online::Poll(mut j) => match j.take() {
                None => self.op = Some(Online::Poll(j)),
                Some(Ok((c, r))) => {
                    match r {
                        Ok(s) => self.samples = s,
                        Err(e) => {
                            self.live = false;
                            self.online_msg = Some((Tone::Crit, format!("poll: {e}")));
                        }
                    }
                    self.put_back(c);
                }
                Some(Err(e)) => self.online_msg = Some((Tone::Crit, e)),
            },
        }
    }

    // -- UI -----------------------------------------------------------------

    fn offline_ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        let t = *host.tokens();
        theme::section_label(ui, &t, "Definition and tune (offline)");
        ui.horizontal_wrapped(|ui| {
            if theme::button(ui, &t, Some(icons::FOLDER_OPEN), "Open INI…").clicked() {
                if let Some(p) = host.pick_file("TunerStudio INI", &[("INI", &["ini"])]) {
                    self.load_ini(&p);
                }
            }
            ui.label(RichText::new("#set").color(t.fg_dim));
            ui.add(egui::TextEdit::singleline(&mut self.set_text).desired_width(120.0));
            ui.label(RichText::new("#unset").color(t.fg_dim));
            ui.add(egui::TextEdit::singleline(&mut self.unset_text).desired_width(120.0));
            let reload = self.model.ini_path.clone();
            if ui
                .add_enabled_ui(reload.is_some(), |ui| {
                    theme::button(ui, &t, Some(icons::ARROWS_CLOCKWISE), "Reload")
                })
                .inner
                .clicked()
            {
                if let Some(p) = reload {
                    self.load_ini(&p);
                }
            }
        });
        let Some(info) = self.model.info() else {
            pl::note(ui, host, Tone::Muted, "Open the ECU's INI definition.");
            return;
        };
        pl::mono(
            ui,
            host,
            &format!(
                "{} · signature {:?} · {} · {} page(s) · {} constants · {} channels · {} \
                 tables · {} curves",
                info.path.display(),
                info.signature.as_deref().unwrap_or(""),
                if info.envelope { "msEnvelope" } else { "plain" },
                info.pages.len(),
                info.constants + info.pc_variables,
                info.channels,
                info.tables.len(),
                info.curves.len()
            ),
        );
        if !info.conditions.is_empty() {
            pl::mono(
                ui,
                host,
                &format!("conditions: {}", info.conditions.join(", ")),
            );
        }
        let (inv, refs, ign) = info.warnings;
        if inv + refs > 0 {
            pl::note(
                ui,
                host,
                Tone::Warn,
                &format!("{inv} invalid line(s), {refs} bad reference(s), {ign} ignored"),
            );
            for w in info.first_invalid.iter().take(5) {
                pl::mono(ui, host, w);
            }
        }
        ui.horizontal_wrapped(|ui| {
            if theme::button(ui, &t, Some(icons::FOLDER_OPEN), "Load .msq…").clicked() {
                if let Some(p) = host.pick_file("Tune", &[("Tune", &["msq"])]) {
                    self.load_msq(&p);
                }
            }
            if ui
                .add_enabled_ui(!self.saver.busy(), |ui| {
                    theme::button(ui, &t, Some(icons::FLOPPY_DISK), "Save .msq…")
                })
                .inner
                .clicked()
            {
                if let Some(p) = host.pick_save("Save the working tune", "tune.msq") {
                    self.save_msq(p);
                }
            }
            if theme::button(ui, &t, Some(icons::GIT_DIFF), "Diff with .msq…").clicked() {
                if let Some(p) = host.pick_file("Tune to compare", &[("Tune", &["msq"])]) {
                    self.diff_msq(&p);
                }
            }
            ui.checkbox(&mut self.force_msq, "ignore tune signature")
                .on_hover_text("Load a tune whose signature differs from the INI's");
            pl::mono(
                ui,
                host,
                &format!("working tune: {}", self.model.working_src),
            );
        });
        if let Some((tone, m)) = &self.offline_msg {
            pl::note(ui, host, *tone, m);
        }
        for l in self.report.iter().take(40) {
            pl::mono(ui, host, l);
        }
        self.edit_ui(ui, host, &info);
    }

    fn edit_ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>, info: &model::IniInfo) {
        let t = *host.tokens();
        theme::section_label(ui, &t, "Edit (working tune)");
        let label = |s: &EditSel| match s {
            EditSel::Constants => "Constants".to_string(),
            EditSel::Table(id) => format!(
                "Table: {}",
                info.tables
                    .iter()
                    .find(|(i, _)| i == id)
                    .map(|(_, t)| t.as_str())
                    .unwrap_or(id)
            ),
            EditSel::Curve(id) => format!(
                "Curve: {}",
                info.curves
                    .iter()
                    .find(|(i, _)| i == id)
                    .map(|(_, t)| t.as_str())
                    .unwrap_or(id)
            ),
        };
        let before = self.edit.clone();
        egui::ComboBox::from_id_salt("ts-edit")
            .width(320.0)
            .selected_text(label(&self.edit))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.edit, EditSel::Constants, "Constants");
                for (id, title) in &info.tables {
                    let s = EditSel::Table(id.clone());
                    ui.selectable_value(&mut self.edit, s, format!("Table: {title}"));
                }
                for (id, title) in &info.curves {
                    let s = EditSel::Curve(id.clone());
                    ui.selectable_value(&mut self.edit, s, format!("Curve: {title}"));
                }
            });
        if self.edit != before {
            self.cell = None;
            self.sel = None;
        }
        match self.edit.clone() {
            EditSel::Constants => self.constants_ui(ui, host),
            EditSel::Table(id) => self.table_ui(ui, host, &id),
            EditSel::Curve(id) => self.curve_ui(ui, host, &id),
        }
        self.cell_editor_ui(ui, host);
    }

    fn constants_ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        let t = *host.tokens();
        let rows = match self.model.values() {
            Ok(r) => r,
            Err(e) => return pl::note(ui, host, Tone::Crit, &e),
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new("filter").color(t.fg_dim));
            ui.add(egui::TextEdit::singleline(&mut self.filter).desired_width(180.0));
        });
        let f = self.filter.to_ascii_lowercase();
        egui::ScrollArea::vertical()
            .id_salt("ts-constants")
            .max_height(260.0)
            .show(ui, |ui| {
                for r in rows
                    .iter()
                    .filter(|r| f.is_empty() || r.name.to_ascii_lowercase().contains(&f))
                {
                    let text = match &r.display {
                        Ok(d) => format!("{:<24} {d}", r.name),
                        Err(e) => format!("{:<24} ({e})", r.name),
                    };
                    let on = self.sel.as_deref() == Some(r.name.as_str());
                    if ui
                        .selectable_label(
                            on,
                            RichText::new(text).font(theme::mono(11.5)).color(t.fg),
                        )
                        .clicked()
                    {
                        self.sel = Some(r.name.clone());
                        self.edit_text = match &r.value {
                            Some(TsValue::Scalar(x)) => x.to_string(),
                            Some(TsValue::Bits(i)) => i.to_string(),
                            Some(TsValue::Text(s)) => s.clone(),
                            _ => String::new(),
                        };
                    }
                }
            });
        let Some(name) = self.sel.clone() else { return };
        let Some(r) = rows.into_iter().find(|r| r.name == name) else {
            return;
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&r.name).color(t.accent));
            match &r.class {
                Class::Bits(opts) => {
                    let mut i: usize = self.edit_text.parse().unwrap_or(0);
                    let cur = opts.get(i).cloned().unwrap_or_default();
                    egui::ComboBox::from_id_salt("ts-bits")
                        .selected_text(cur)
                        .show_ui(ui, |ui| {
                            for (k, o) in opts.iter().enumerate() {
                                ui.selectable_value(&mut i, k, format!("{k}: {o}"));
                            }
                        });
                    self.edit_text = i.to_string();
                }
                _ => {
                    ui.add(egui::TextEdit::singleline(&mut self.edit_text).desired_width(140.0));
                }
            }
            ui.label(RichText::new(&r.units).color(t.fg_dim));
            let page = match r.page {
                Some(p) => format!("page {}", p + 1),
                None => "PC variable".into(),
            };
            ui.label(RichText::new(page).color(t.fg_dim));
            if let (Some(lo), Some(hi)) = (r.min, r.max) {
                ui.label(RichText::new(format!("{lo} … {hi}")).color(t.fg_dim));
            }
            if theme::button(ui, &t, Some(icons::CHECK), "Apply").clicked() {
                let text = self.edit_text.clone();
                self.offline_msg = Some(match self.apply_value(&name, &text) {
                    Ok(()) => (Tone::Ok, format!("{name} set (working tune; not sent)")),
                    Err(e) => (Tone::Crit, e),
                });
            }
        });
    }

    fn table_ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>, id: &str) {
        let t = *host.tokens();
        let tv = match self.model.table(id) {
            Ok(v) => v,
            Err(e) => return pl::note(ui, host, Tone::Crit, &e),
        };
        pl::mono(
            ui,
            host,
            &format!(
                "{} ({}, {}×{} {})",
                tv.title, tv.z, tv.rows, tv.cols, tv.units
            ),
        );
        for (k, a) in [("x", &tv.x), ("y", &tv.y)] {
            if let Some(a) = a {
                let ch = a.channel.as_deref().unwrap_or("no channel");
                pl::mono(ui, host, &format!("{k}: {} [{}] ({ch})", a.name, a.units));
            }
        }
        if let Some(h) = &tv.help {
            pl::note(ui, host, Tone::Muted, h);
        }
        let d = tv.digits.unwrap_or(1).min(4);
        let ax = |a: &Option<model::Axis>, i: usize| {
            a.as_ref()
                .and_then(|a| a.values.get(i))
                .map(|v| format!("{v}"))
                .unwrap_or_default()
        };
        egui::ScrollArea::both()
            .id_salt("ts-table")
            .max_height(320.0)
            .show(ui, |ui| {
                egui::Grid::new("ts-table-grid")
                    .spacing([4.0, 2.0])
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(
                                tv.y.as_ref()
                                    .map(|a| {
                                        format!(
                                            "{} \\ {}",
                                            a.name,
                                            tv.x.as_ref().map(|x| x.name.as_str()).unwrap_or("")
                                        )
                                    })
                                    .unwrap_or_default(),
                            )
                            .color(t.fg_dim),
                        );
                        for c in 0..tv.cols {
                            ui.label(RichText::new(ax(&tv.x, c)).color(t.fg_dim));
                        }
                        ui.end_row();
                        // Highest y bin at the top, as tuning tools draw it.
                        for r in (0..tv.rows).rev() {
                            ui.label(RichText::new(ax(&tv.y, r)).color(t.fg_dim));
                            for c in 0..tv.cols {
                                let v = tv.cells.get(r).and_then(|row| row.get(c)).copied();
                                let idx = r * tv.cols + c;
                                let on = self.cell == Some((tv.z.clone(), idx));
                                let txt = v.map(|v| format!("{v:.d$}")).unwrap_or_default();
                                if ui
                                    .selectable_label(
                                        on,
                                        RichText::new(txt).font(theme::mono(11.0)),
                                    )
                                    .clicked()
                                {
                                    self.cell = Some((tv.z.clone(), idx));
                                    self.cell_text = v.map(|v| v.to_string()).unwrap_or_default();
                                }
                            }
                            ui.end_row();
                        }
                    });
            });
        for a in [&tv.x, &tv.y].into_iter().flatten() {
            if let Some(e) = &a.error {
                pl::note(ui, host, Tone::Warn, &format!("axis {}: {e}", a.name));
            }
        }
    }

    fn curve_ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>, id: &str) {
        let t = *host.tokens();
        let cv = match self.model.curve(id) {
            Ok(v) => v,
            Err(e) => return pl::note(ui, host, Tone::Crit, &e),
        };
        pl::mono(ui, host, &cv.title);
        let n =
            cv.x.as_ref()
                .map(|x| x.values.len())
                .unwrap_or_else(|| cv.ys.iter().map(|y| y.values.len()).max().unwrap_or(0));
        egui::Grid::new("ts-curve-grid")
            .spacing([6.0, 2.0])
            .show(ui, |ui| {
                ui.label(
                    RichText::new(cv.x.as_ref().map(|x| x.name.as_str()).unwrap_or("x"))
                        .color(t.fg_dim),
                );
                for y in &cv.ys {
                    ui.label(RichText::new(format!("{} [{}]", y.name, y.units)).color(t.fg_dim));
                }
                ui.end_row();
                for i in 0..n {
                    let xv =
                        cv.x.as_ref()
                            .and_then(|x| x.values.get(i))
                            .map(|v| v.to_string())
                            .unwrap_or_default();
                    ui.label(RichText::new(xv).color(t.fg_dim));
                    for y in &cv.ys {
                        let v = y.values.get(i).copied();
                        let on = self.cell == Some((y.name.clone(), i));
                        let txt = v.map(|v| v.to_string()).unwrap_or_default();
                        if ui
                            .selectable_label(on, RichText::new(txt).font(theme::mono(11.0)))
                            .clicked()
                        {
                            self.cell = Some((y.name.clone(), i));
                            self.cell_text = v.map(|v| v.to_string()).unwrap_or_default();
                        }
                    }
                    ui.end_row();
                }
            });
    }

    fn cell_editor_ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        let t = *host.tokens();
        let Some((name, idx)) = self.cell.clone() else {
            return;
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{name}[{idx}]")).color(t.accent));
            ui.add(egui::TextEdit::singleline(&mut self.cell_text).desired_width(100.0));
            if theme::button(ui, &t, Some(icons::CHECK), "Apply").clicked() {
                let text = self.cell_text.clone();
                self.offline_msg = Some(match self.apply_cell(&name, idx, &text) {
                    Ok(()) => (
                        Tone::Ok,
                        format!("{name}[{idx}] set (working tune; not sent)"),
                    ),
                    Err(e) => (Tone::Crit, e),
                });
            }
        });
    }

    fn online_ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        let t = *host.tokens();
        theme::section_label(ui, &t, "ECU (online)");
        let busy = self.op.is_some();
        ui.add_enabled_ui(!busy && self.conn.is_none(), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("port").color(t.fg_dim));
                ui.add(
                    egui::TextEdit::singleline(&mut self.port)
                        .desired_width(140.0)
                        .hint_text("sim, /dev/ttyACM0, COM3"),
                );
                ui.label(RichText::new("baud").color(t.fg_dim));
                ui.add(egui::DragValue::new(&mut self.baud).range(1200..=2_000_000));
                egui::ComboBox::from_id_salt("ts-framing")
                    .selected_text(format!("framing: {}", self.framing))
                    .show_ui(ui, |ui| {
                        for f in ["auto", "envelope", "plain"] {
                            ui.selectable_value(&mut self.framing, f.to_string(), f);
                        }
                    });
                ui.checkbox(&mut self.force_sig, "ignore signature mismatch");
            });
        });
        ui.horizontal_wrapped(|ui| {
            let has_ini = self.model.ini.is_some();
            if self.conn.is_none() {
                if ui
                    .add_enabled_ui(!busy && has_ini, |ui| {
                        theme::primary_button(ui, &t, Some(icons::PLUGS), "Connect")
                    })
                    .inner
                    .clicked()
                {
                    self.request_connect(host);
                }
            } else if ui
                .add_enabled_ui(!busy, |ui| {
                    theme::button(ui, &t, Some(icons::X), "Disconnect")
                })
                .inner
                .clicked()
            {
                self.disconnect();
            }
            let on = self.conn.is_some() && !busy;
            if ui
                .add_enabled_ui(on, |ui| {
                    theme::button(ui, &t, Some(icons::DOWNLOAD), "Read from ECU")
                })
                .inner
                .on_hover_text("Reads every page; it becomes the working tune")
                .clicked()
            {
                self.request_read(host);
            }
            if ui
                .add_enabled_ui(on, |ui| {
                    theme::button(ui, &t, Some(icons::SHIELD_CHECK), "Back up ECU tune")
                })
                .inner
                .on_hover_text("Reads every page and keeps it (raw pages + .msq) in the vault")
                .clicked()
            {
                self.request_backup(host);
            }
            if theme::button(ui, &t, Some(icons::LIST_CHECKS), "Plan write").clicked() {
                self.request_plan();
            }
            ui.add_enabled_ui(false, |ui| {
                theme::primary_button(ui, &t, Some(icons::UPLOAD), "Write to ECU")
            })
            .response
            .on_disabled_hover_text(WRITE_DISABLED);
            if busy {
                ui.spinner();
            }
        });
        self.records.ui(ui, host);
        for b in &self.last_backup {
            pl::mono(ui, host, &format!("backup: {}", b.display()));
        }
        if let Some((tone, m)) = &self.online_msg {
            pl::note(ui, host, *tone, m);
        }
        match &self.plan {
            Some(Ok(p)) => {
                if p.is_empty() {
                    pl::note(ui, host, Tone::Ok, "the ECU already holds the working tune");
                } else {
                    pl::mono(
                        ui,
                        host,
                        &format!("{} byte(s) would change:", p.changed_bytes),
                    );
                    for pp in &p.pages {
                        pl::mono(
                            ui,
                            host,
                            &format!(
                                "page {}: {} byte(s) in {} range(s){}",
                                pp.page,
                                pp.changed,
                                pp.ranges.len(),
                                if pp.burnable {
                                    ""
                                } else {
                                    " (not burnable: RAM only)"
                                }
                            ),
                        );
                    }
                    for (n, c) in p.constants.iter().take(60) {
                        pl::mono(ui, host, &format!("  {n}: {c}"));
                    }
                }
                pl::note(ui, host, Tone::Warn, WRITE_DISABLED);
            }
            Some(Err(e)) => pl::note(ui, host, Tone::Crit, e),
            None => {}
        }
        theme::section_label(ui, &t, "Live channels");
        ui.horizontal(|ui| {
            ui.add_enabled_ui(self.conn.is_some() || self.live, |ui| {
                ui.checkbox(&mut self.live, "poll");
            });
            ui.add(
                egui::TextEdit::singleline(&mut self.channels)
                    .desired_width(260.0)
                    .hint_text("all (e.g. rpm, coolant)"),
            );
        });
        ui.horizontal_wrapped(|ui| {
            for s in &self.samples {
                let v = match s.value {
                    Some(v) => format!("{v:.2} {}", s.units),
                    None => "—".into(),
                };
                theme::stat(
                    ui,
                    &t,
                    icons::PULSE,
                    &s.name,
                    &v,
                    if s.value.is_some() { t.fg } else { t.fg_dim },
                )
                .on_hover_text(&s.note);
            }
        });
    }
}

fn saved(r: Result<PathBuf, String>, entries: usize) -> (Tone, String) {
    match r {
        Ok(p) if entries > 0 => (
            Tone::Ok,
            format!("saved {} ({entries} entries)", p.display()),
        ),
        Ok(p) => (Tone::Ok, format!("saved {}", p.display())),
        Err(e) => (Tone::Crit, e),
    }
}

impl NativeView for TsView {
    fn id(&self) -> &str {
        "ts"
    }
    fn label(&self) -> &str {
        "Aftermarket ECU"
    }
    fn icon(&self) -> &str {
        icons::ENGINE
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Vehicle
    }
    fn tier(&self) -> Tier {
        TIER
    }
    fn ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        self.tick(host);
        if self.op.is_some() || self.live {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        let t = *host.tokens();
        theme::panel(
            ui,
            &t,
            "Aftermarket ECU",
            |_| {},
            |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_space(6.0);
                    self.offline_ui(ui, host);
                    ui.add_space(8.0);
                    self.online_ui(ui, host);
                });
            },
        );
    }
}
