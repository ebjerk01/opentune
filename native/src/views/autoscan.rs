//! Auto-scan: presence, identification and stored fault codes of every
//! module in a vehicle's module list, over [`opentune_autoscan`] (port of the
//! webview app's Auto-scan panel, `feat_scan.rs`), plus clearing fault codes
//! of one module or of all modules that reported codes.
//!
//! The scan is one read-only `adapter.scan` that binds each module's own
//! `req_id` / `resp_id` before probing it. Clearing is a separate, confirmed
//! `actuate` PER MODULE, confirmed with `Host::confirm_ecu_for` for that
//! module by name. "Clear all" never fans out: one summary lists every module
//! first (it authorises nothing), then each module gets its own read, DTC
//! report and typed confirmation; a declined module is skipped.
//!
//! ## Per-op table (reviewed before merge)
//!
//! | Operation | SIDs sent (in order) | Path | Backup source |
//! |---|---|---|---|
//! | Scan all modules | per module, in list order, after `bind(req_id/resp_id)`: UDS `3E 00`, `22 F190`, `22 F187`, `22 F189`, `22 F191`, `22 F195`, `19 02 FF`; OBD `01 00`, `03` (an absent module gets only the ping) | `scan` (`ModuleSession::bind` per module; read-only) | n/a (read only) |
//! | Clear one module's DTCs — 1. read | UDS `3E 00`, the identity `22`s, `19 02 FF`; OBD `01 00`, `03` | `read` (that module) | — |
//! | Clear — 2. DTC report | none | a NEW file `<vault>/dtc-reports/<time>-<module>.txt` (panel record, not a `BackupReceipt`); no report, no clear | the codes read in step 1 |
//! | Clear — 3. confirm | none | `Host::confirm_ecu_for(req, module, "<module name> (<ids>)")`, per-module key, typed `CLEAR`, text "Clears stored fault codes — not reversible …" | — |
//! | Clear — 4. clear | (`ack.target() == module` re-checked first) run: UDS `3E 00`, identity `22`s, `19 02 FF` (the codes must equal the report, else NOTHING is cleared), `3E 00`, `14 FF FF FF`; OBD `01 00`, `03`, `01 00`, `04`. restore: sends nothing (a clear holds no session or output to hand back) | `actuate` (the `Ack`'s module) | n/a (not reversible; the step-2 report is the record) |
//! | Clear all — summary | none | a plain `Host::confirm` listing EVERY module that will be touched; it names no module and authorises none | n/a |
//! | Clear all — per module | steps 1–4 above for each listed module in turn, each with its own report and its own `confirm_ecu_for` (declined → skipped, nothing sent to it) | `read` → `actuate` per module | n/a (per-module report) |

use super::coding::plumbing as pl;
use crate::host::{
    Ack, Actuation, AdapterAccess, ConfirmRequest, ConfirmResult, DiagTarget, Host, JobHandle,
    NativeView, Tier, ViewGroup,
};
use crate::icons;
use crate::theme::{self, Tone};
use crate::widgets::packs::{PackKind, PackPicker};
use eframe::egui::{self, RichText};
use opentune_autoscan::{
    clear_module, scan_module, ClearResult, Dtc, ModuleAddr, ModuleResult, Protocol, VehicleModules,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Bench;

pub fn create() -> Box<dyn NativeView> {
    Box::new(AutoscanView::default())
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

pub(crate) fn load(path: &Path) -> Result<VehicleModules, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read module list {}: {e}", path.display()))?;
    VehicleModules::from_json(&text).map_err(|e| format!("parse module list: {e}"))
}

/// A module's own CAN address.
pub(crate) fn target_of(m: &ModuleAddr) -> DiagTarget {
    DiagTarget::can(m.req_id, m.resp_id)
}

/// Scan every module, each on its own address (read-only).
pub(crate) fn scan_op(
    a: &Arc<dyn AdapterAccess>,
    modules: VehicleModules,
) -> Result<Vec<ModuleResult>, String> {
    a.scan("auto-scan", move |s| {
        Ok(modules
            .modules
            .iter()
            .map(|m| match s.bind(target_of(m)) {
                Ok(d) => scan_module(d, m),
                Err(e) => ModuleResult {
                    name: m.name.clone(),
                    present: false,
                    identity: None,
                    dtcs: Vec::new(),
                    error: Some(format!("adapter: {e}")),
                },
            })
            .collect())
    })
}

/// Read one module's codes for the report (read path).
pub(crate) fn read_codes_op(
    a: &Arc<dyn AdapterAccess>,
    m: ModuleAddr,
) -> Result<ModuleResult, String> {
    a.read("read DTCs", target_of(&m), move |d| {
        let r = scan_module(d, &m);
        if !r.present {
            return Err(format!("module {} does not answer", m.name));
        }
        if let Some(e) = r.error.as_ref().filter(|_| r.dtcs.is_empty()) {
            return Err(format!("the fault codes could not be read: {e}"));
        }
        Ok(r)
    })
}

/// The DTC report text of `r` (module `m`).
pub(crate) fn report_text(m: &ModuleAddr, r: &ModuleResult) -> String {
    let mut s = pl::report_header("saved before clearing", &m.name, target_of(m));
    s.push_str(&format!(
        "protocol: {}\nread: {}\ncodes: {}\n",
        match m.protocol {
            Protocol::Uds => "UDS",
            Protocol::Obd => "OBD-II",
        },
        match m.protocol {
            Protocol::Uds => "19 02 FF",
            Protocol::Obd => "03",
        },
        r.dtcs.len()
    ));
    for d in &r.dtcs {
        s.push_str(&format!(
            "{}  {}  {}\n",
            d.code,
            d.status
                .map(|x| format!("status 0x{x:02X}"))
                .unwrap_or_else(|| "-".into()),
            d.description
        ));
    }
    s
}

/// Clear module `m`'s codes on the module the user confirmed (`actuate`).
/// The codes are read again first and must equal `reported` (the saved
/// report), else nothing is cleared.
pub(crate) fn clear_op(
    a: &Arc<dyn AdapterAccess>,
    ack: Ack,
    m: ModuleAddr,
    reported: Vec<Dtc>,
) -> Result<Actuation<ClearResult>, String> {
    if ack.target() != Some(target_of(&m)) {
        return Err(format!(
            "clear refused: the confirmed module is not {} ({}); nothing was sent",
            m.name,
            target_of(&m)
        ));
    }
    a.actuate(
        "clear DTCs",
        ack,
        move |d| {
            let now = scan_module(d, &m);
            if !now.present {
                return Err(format!("{} does not answer; nothing cleared", m.name));
            }
            if now.dtcs != reported {
                return Err(
                    "the stored codes changed since the report was saved; nothing was cleared \
                     (read them again)"
                        .into(),
                );
            }
            let r = clear_module(d, &m);
            match &r.error {
                Some(e) if !r.cleared => Err(format!("clear failed: {e}")),
                _ => Ok(r),
            }
        },
        // A clear holds no session or output: there is nothing to hand back.
        |_d| Ok(()),
    )
}

// ---------------------------------------------------------------------------
// Clear pipeline: (summary →) per module: read → report → confirm → actuate
// ---------------------------------------------------------------------------

/// The confirmation key of clearing module `m` (one per module: an answer
/// for one module can never be consumed for another).
pub(crate) fn clear_key(m: &ModuleAddr) -> String {
    pl::confirm_key("autoscan-clear", target_of(m))
}

/// The one summary shown before "Clear all" starts: every module that will
/// be touched. Accepting it authorises NO module; each is confirmed alone.
pub(crate) fn summary_request(mods: &[(ModuleAddr, usize)]) -> ConfirmRequest {
    let list: Vec<String> = mods
        .iter()
        .map(|(m, n)| format!("• {}: {n} code(s)", pl::module_label(&m.name, target_of(m))))
        .collect();
    ConfirmRequest {
        key: "autoscan-clear-all".into(),
        title: format!("Clear fault codes on {} modules", mods.len()),
        body: format!(
            "These modules will be touched, one at a time:\n{}\n\nNothing is cleared by this \
             step. For EACH module, OpenTune first reads its codes again and saves them to a \
             DTC report, then asks you to confirm that module on its own (typed CLEAR). A \
             module you decline is skipped; the others still need their own confirmation.",
            list.join("\n")
        ),
        typed: None,
        confirm_label: "Review modules one by one".into(),
    }
}

/// The confirmation of clearing ONE module (`index` of `total`).
pub(crate) fn module_request(
    m: &ModuleAddr,
    codes: usize,
    report: &Path,
    index: usize,
    total: usize,
) -> ConfirmRequest {
    let label = pl::module_label(&m.name, target_of(m));
    ConfirmRequest {
        key: clear_key(m),
        title: format!("Clear fault codes: {}", m.name),
        body: format!(
            "Clears stored fault codes — not reversible. {codes} code(s) on {label}; the list \
             was saved to {}. Freeze-frame data and readiness monitors may be reset too. This \
             confirmation covers {label} only (module {index} of {total}).",
            report.display()
        ),
        typed: Some("CLEAR".into()),
        confirm_label: "Clear codes".into(),
    }
}

pub(crate) enum Step {
    Idle,
    /// The clear-all summary (a plain confirmation: it authorises nothing).
    Summary(ConfirmRequest, Vec<ModuleAddr>),
    Read(JobHandle<ModuleResult>, ModuleAddr),
    Confirm(ConfirmRequest, ModuleAddr, Vec<Dtc>),
    Clear(JobHandle<Actuation<ClearResult>>, String),
}

pub struct AutoscanView {
    picker: PackPicker,
    pub(crate) modules: Option<VehicleModules>,
    scan_job: Option<JobHandle<Vec<ModuleResult>>>,
    pub(crate) results: Vec<ModuleResult>,
    pub(crate) step: Step,
    /// Modules still to clear (each with its own read, report and confirm).
    pub(crate) queue: std::collections::VecDeque<ModuleAddr>,
    /// How many modules the current clear run covers.
    total: usize,
    /// `(module, outcome)` of the current / last clear run.
    pub(crate) outcomes: Vec<(String, String)>,
    pub(crate) records: pl::Records,
    pub(crate) last_report: Option<PathBuf>,
    pub(crate) reports: Vec<PathBuf>,
    pub(crate) msg: Option<(Tone, String)>,
}

impl Default for AutoscanView {
    fn default() -> Self {
        Self {
            picker: PackPicker::new(PackKind::Modules),
            modules: None,
            scan_job: None,
            results: Vec::new(),
            step: Step::Idle,
            queue: Default::default(),
            total: 0,
            outcomes: Vec::new(),
            records: Default::default(),
            last_report: None,
            reports: Vec::new(),
            msg: None,
        }
    }
}

impl AutoscanView {
    fn set_list(&mut self, path: &Path) {
        match load(path) {
            Ok(m) => {
                self.modules = Some(m);
                self.results.clear();
                self.msg = None;
            }
            Err(e) => self.msg = Some((Tone::Crit, e)),
        }
    }

    fn busy(&self) -> bool {
        self.scan_job.is_some() || !matches!(self.step, Step::Idle)
    }

    fn start_scan(&mut self, host: &Host<'_>) {
        let (Some(m), Some(a)) = (self.modules.clone(), host.adapter()) else {
            return;
        };
        self.scan_job = Some(host.spawn_job("auto-scan", move |_| scan_op(&a, m)));
    }

    fn can_clear(&mut self, host: &Host<'_>) -> bool {
        if self.records.near(host).is_none() {
            self.msg = Some((
                Tone::Crit,
                "choose a folder first: the codes are saved in a report before clearing".into(),
            ));
            return false;
        }
        host.adapter().is_some()
    }

    /// Clear module `m` (any module of the list): its codes are read and
    /// saved first, then it gets its own confirmation.
    pub(crate) fn request_clear(&mut self, host: &mut Host<'_>, m: ModuleAddr) {
        if !self.can_clear(host) {
            return;
        }
        self.outcomes.clear();
        self.total = 1;
        self.queue = [m].into();
        self.step = self.next_module(host);
    }

    /// Clear every scanned module that reported codes: one summary listing
    /// them all first, then module by module, each confirmed on its own.
    pub(crate) fn request_clear_all(&mut self, host: &mut Host<'_>) {
        let Some(mods) = self.modules.clone() else {
            return;
        };
        let list: Vec<(ModuleAddr, usize)> = self
            .results
            .iter()
            .zip(&mods.modules)
            .filter(|(r, _)| r.present && !r.dtcs.is_empty())
            .map(|(r, m)| (m.clone(), r.dtcs.len()))
            .collect();
        if list.is_empty() {
            return self.msg = Some((Tone::Muted, "no module reported codes".into()));
        }
        if !self.can_clear(host) {
            return;
        }
        let req = summary_request(&list);
        self.step = Step::Summary(req, list.into_iter().map(|(m, _)| m).collect());
    }

    /// The next module's read (or the end of the run).
    fn next_module(&mut self, host: &Host<'_>) -> Step {
        let (Some(m), Some(a)) = (self.queue.pop_front(), host.adapter()) else {
            if self.total > 1 {
                let done: Vec<String> = self
                    .outcomes
                    .iter()
                    .map(|(m, o)| format!("{m}: {o}"))
                    .collect();
                self.msg = Some((
                    Tone::Muted,
                    format!("clear all finished. {}", done.join("; ")),
                ));
            }
            self.queue.clear();
            return Step::Idle;
        };
        let mm = m.clone();
        Step::Read(
            host.spawn_job("read DTCs", move |_| read_codes_op(&a, mm)),
            m,
        )
    }

    /// Record how module `name` ended and go on with the next one.
    fn finish(&mut self, host: &Host<'_>, name: &str, tone: Tone, outcome: String) -> Step {
        self.msg = Some((tone, format!("{name}: {outcome}")));
        self.outcomes.push((name.to_string(), outcome));
        self.next_module(host)
    }

    pub(crate) fn tick(&mut self, host: &mut Host<'_>) {
        if let Some(r) = pl::take(&mut self.scan_job) {
            match r {
                Ok(v) => {
                    self.results = v;
                    self.msg = None;
                }
                Err(e) => self.msg = Some((Tone::Crit, format!("scan failed: {e}"))),
            }
        }
        self.step = match std::mem::replace(&mut self.step, Step::Idle) {
            Step::Idle => Step::Idle,
            Step::Summary(req, list) => match host.confirm(req.clone()) {
                ConfirmResult::Pending => Step::Summary(req, list),
                ConfirmResult::Declined => {
                    self.msg = Some((Tone::Muted, "clear all cancelled: nothing was sent".into()));
                    Step::Idle
                }
                // A plain confirmation: it names no module and authorises
                // nothing; each module is confirmed on its own below.
                ConfirmResult::Accepted(_plain) => {
                    self.outcomes.clear();
                    self.total = list.len();
                    self.queue = list.into();
                    self.next_module(host)
                }
            },
            Step::Read(mut job, m) => match job.take() {
                None => Step::Read(job, m),
                Some(Err(e)) => self.finish(host, &m.name, Tone::Crit, format!("not cleared: {e}")),
                Some(Ok(r)) => self.save_report(host, m, r),
            },
            Step::Confirm(req, m, dtcs) => {
                let label = pl::module_label(&m.name, target_of(&m));
                match host.confirm_ecu_for(req.clone(), target_of(&m), &label) {
                    ConfirmResult::Pending => Step::Confirm(req, m, dtcs),
                    ConfirmResult::Declined => {
                        let n = m.name.clone();
                        self.finish(
                            host,
                            &n,
                            Tone::Muted,
                            "skipped (declined): nothing sent".into(),
                        )
                    }
                    ConfirmResult::Accepted(ack) => match host.adapter() {
                        Some(a) => {
                            let name = m.name.clone();
                            let job =
                                host.spawn_job("clear DTCs", move |_| clear_op(&a, ack, m, dtcs));
                            Step::Clear(job, name)
                        }
                        None => Step::Idle,
                    },
                }
            }
            Step::Clear(mut job, name) => match job.take() {
                None => Step::Clear(job, name),
                Some(r) => match r.and_then(Actuation::into_result) {
                    Ok(_) => self.finish(host, &name, Tone::Ok, "fault codes cleared".into()),
                    Err(e) => self.finish(host, &name, Tone::Crit, e),
                },
            },
        };
    }

    fn save_report(&mut self, host: &Host<'_>, m: ModuleAddr, r: ModuleResult) -> Step {
        let Some(near) = self.records.near(host) else {
            return self.finish(host, &m.name, Tone::Crit, "no report folder".into());
        };
        if r.dtcs.is_empty() {
            return self.finish(
                host,
                &m.name,
                Tone::Muted,
                "no stored codes to clear".into(),
            );
        }
        let path = match pl::save_dtc_report(&near, &m.name, &report_text(&m, &r)) {
            Ok(p) => p,
            Err(e) => {
                return self.finish(
                    host,
                    &m.name,
                    Tone::Crit,
                    format!("report not saved, nothing cleared: {e}"),
                )
            }
        };
        let index = self.total - self.queue.len();
        let req = module_request(&m, r.dtcs.len(), &path, index, self.total);
        self.last_report = Some(path.clone());
        self.reports.push(path);
        Step::Confirm(req, m, r.dtcs)
    }
}

impl NativeView for AutoscanView {
    fn id(&self) -> &str {
        "autoscan"
    }
    fn label(&self) -> &str {
        "Auto-scan"
    }
    fn icon(&self) -> &str {
        icons::SCAN
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Vehicle
    }
    fn tier(&self) -> Tier {
        TIER
    }
    fn count(&self) -> Option<String> {
        let n: usize = self.results.iter().map(|r| r.dtcs.len()).sum();
        (n > 0).then(|| n.to_string())
    }
    fn ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        self.tick(host);
        let t = *host.tokens();
        if self.busy() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
        theme::panel(
            ui,
            &t,
            "Auto-scan",
            |_| {},
            |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_space(6.0);
                    if let Some(d) = self.picker.ui(ui, host) {
                        self.set_list(&d.path);
                    }
                    if theme::button(ui, &t, Some(icons::FOLDER_OPEN), "Open module list…")
                        .clicked()
                    {
                        if let Some(p) = host.pick_file("Module list", &[("JSON", &["json"])]) {
                            self.set_list(&p);
                        }
                    }
                    let Some(mods) = self.modules.clone() else {
                        pl::note(ui, host, Tone::Muted, "Choose a vehicle module list.");
                        return;
                    };
                    pl::mono(
                        ui,
                        host,
                        &format!("{}  ·  {} modules", mods.vehicle, mods.modules.len()),
                    );
                    let can =
                        crate::widgets::caps::gate(ui, host, &[crate::widgets::caps::Need::Diag]);
                    if ui
                        .add_enabled_ui(can && !self.busy(), |ui| {
                            theme::primary_button(ui, &t, Some(icons::SCAN), "Scan all modules")
                        })
                        .inner
                        .clicked()
                    {
                        self.start_scan(host);
                    }
                    if self.scan_job.is_some() {
                        pl::note(ui, host, Tone::Muted, "scanning…");
                    }
                    if let Some((tone, m)) = &self.msg {
                        pl::note(ui, host, *tone, m);
                    }
                    if let Some(p) = &self.last_report {
                        pl::mono(ui, host, &format!("last DTC report: {}", p.display()));
                    }
                    self.records.ui(ui, host);
                    ui.add_space(6.0);
                    let any_codes = self.results.iter().any(|r| r.present && !r.dtcs.is_empty());
                    if ui
                        .add_enabled_ui(can && any_codes && !self.busy(), |ui| {
                            theme::button(ui, &t, Some(icons::ERASER), "Clear all…")
                        })
                        .inner
                        .on_hover_text(
                            "Lists every module first, then reads, reports and confirms each \
                             module on its own.",
                        )
                        .clicked()
                    {
                        self.request_clear_all(host);
                    }
                    for (m, o) in &self.outcomes {
                        pl::mono(ui, host, &format!("{m}: {o}"));
                    }
                    let mut clear = None;
                    for (r, m) in self.results.iter().zip(&mods.modules) {
                        ui.separator();
                        ui.horizontal(|ui| {
                            let (tone, txt) = if r.present {
                                (Tone::Ok, "present")
                            } else {
                                (Tone::Muted, "absent")
                            };
                            theme::pill(ui, &t, txt, tone, false);
                            ui.label(RichText::new(&r.name).strong());
                            ui.label(RichText::new(target_of(m).to_string()).color(t.fg_faint));
                            if !r.dtcs.is_empty() {
                                theme::pill(
                                    ui,
                                    &t,
                                    &format!("{} DTC", r.dtcs.len()),
                                    Tone::Warn,
                                    false,
                                );
                                let ok = !self.busy() && can;
                                if ui
                                    .add_enabled_ui(ok, |ui| {
                                        theme::button(ui, &t, Some(icons::ERASER), "Clear…")
                                    })
                                    .inner
                                    .clicked()
                                {
                                    clear = Some(m.clone());
                                }
                            }
                        });
                        if let Some(id) = &r.identity {
                            if let Some(v) = &id.vin {
                                pl::mono(ui, host, &format!("VIN {v}"));
                            }
                            for d in &id.dids {
                                pl::mono(
                                    ui,
                                    host,
                                    &format!(
                                        "{:04X} {}: {}",
                                        d.did,
                                        d.label,
                                        d.text.clone().unwrap_or_else(|| pl::hex(&d.raw))
                                    ),
                                );
                            }
                        }
                        for d in &r.dtcs {
                            pl::mono(ui, host, &format!("{}  {}", d.code, d.description));
                        }
                        if let Some(e) = &r.error {
                            pl::note(ui, host, Tone::Warn, e);
                        }
                    }
                    if let Some(m) = clear {
                        self.request_clear(host, m);
                    }
                });
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::coding::simkit::{Bus, Fixture};
    use super::*;
    use opentune_udssim::server::{DtcRecord, UdsServer};

    const ABS: DiagTarget = DiagTarget::can(0x760, 0x768);

    fn m(name: &str, tx: u32, rx: u32) -> ModuleAddr {
        ModuleAddr {
            name: name.into(),
            addr: 0,
            req_id: tx,
            resp_id: rx,
            protocol: Protocol::Uds,
        }
    }

    fn vehicle() -> (Bus, VehicleModules) {
        let mut engine = UdsServer::new();
        engine.set_did(0xF190, b"WVWZZZ1KZAW000001".to_vec());
        engine.add_dtc(DtcRecord::new([0x01, 0x01, 0x00], 0x24));
        let mut abs = UdsServer::new();
        abs.add_dtc(DtcRecord::new([0xC1, 0x00, 0x00], 0x2F));
        let bus = Bus::default()
            .uds(0x7E0, 0x7E8, engine)
            .uds(0x760, 0x768, abs);
        let mods = VehicleModules {
            vehicle: "synthetic".into(),
            modules: vec![
                m("Engine", 0x7E0, 0x7E8),
                m("ABS", 0x760, 0x768),
                m("Airbag", 0x715, 0x77F),
            ],
        };
        (bus, mods)
    }

    fn dtcs(bus: &Bus, t: DiagTarget) -> usize {
        bus.with_uds(t.tx, t.rx, |s| s.dtcs().len())
    }

    fn run(v: &mut AutoscanView, fx: &mut Fixture, t: DiagTarget) {
        let t0 = std::time::Instant::now();
        loop {
            fx.host(t, |h| v.tick(h));
            if matches!(v.step, Step::Idle | Step::Confirm(..) | Step::Summary(..)) {
                break;
            }
            assert!(t0.elapsed() < std::time::Duration::from_secs(10), "stuck");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn the_scan_binds_each_module_and_sends_only_reads() {
        let (bus, mods) = vehicle();
        let r = scan_op(&bus.access("sim"), mods).unwrap();
        assert!(r[0].present && r[1].present && !r[2].present);
        assert_eq!(r[0].dtcs[0].code, "P0101");
        assert_eq!(r[1].dtcs[0].code, "U0100");
        assert_eq!(bus.lock().opens, vec![0x7E0, 0x760, 0x715]);
        for (_, req) in bus.wire() {
            assert!(
                crate::ops::refuse_ecu_changing(&req).is_ok(),
                "{req:02X?} is not a read"
            );
        }
    }

    #[test]
    fn clearing_saves_a_report_first_and_clears_only_the_confirmed_module() {
        let (bus, mods) = vehicle();
        let mut fx = Fixture::new("scan-clear");
        fx.adapter = Some(bus.access("sim"));
        let mut v = AutoscanView {
            records: pl::Records::in_dir(&fx.dir),
            ..Default::default()
        };
        // The ABS is not the app target (the engine is): it is confirmed by
        // name and only it is cleared.
        const APP: DiagTarget = DiagTarget::ENGINE;
        fx.host(APP, |h| v.request_clear(h, mods.modules[1].clone()));
        run(&mut v, &mut fx, APP);
        // The report is on disk before anything changing is sent.
        let report = v.last_report.clone().expect("report saved");
        let text = std::fs::read_to_string(&report).unwrap();
        assert!(text.contains("U0100") && text.contains("0x760"), "{text}");
        assert!(bus.with_sid(0x14).is_empty());
        let Step::Confirm(req, ..) = &v.step else {
            panic!("expected the confirmation")
        };
        assert!(req.body.contains("not reversible"));
        assert!(req.body.contains("covers ABS (tx 0x760 / rx 0x768) only"));
        fx.host(APP, |h| v.tick(h)); // the dialog opens
        assert!(matches!(v.step, Step::Confirm(..)));
        assert!(bus.with_sid(0x14).is_empty(), "nothing while pending");
        fx.confirms.answer(&clear_key(&mods.modules[1]), true);
        run(&mut v, &mut fx, APP);
        assert_eq!(v.msg.as_ref().unwrap().0, Tone::Ok, "{:?}", v.msg);
        assert_eq!(
            bus.with_sid(0x14),
            vec![(0x760, vec![0x14, 0xFF, 0xFF, 0xFF])]
        );
        assert_eq!(dtcs(&bus, ABS), 0);
        assert_eq!(
            dtcs(&bus, DiagTarget::ENGINE),
            1,
            "the engine keeps its code"
        );
    }

    #[test]
    fn declined_plain_or_foreign_confirmations_clear_nothing() {
        let (bus, mods) = vehicle();
        let a = bus.access("sim");
        let mut fx = Fixture::new("scan-decline");
        fx.adapter = Some(a.clone());
        let mut v = AutoscanView {
            records: pl::Records::in_dir(&fx.dir),
            ..Default::default()
        };
        // Declined.
        fx.host(ABS, |h| v.request_clear(h, mods.modules[1].clone()));
        run(&mut v, &mut fx, ABS);
        fx.host(ABS, |h| v.tick(h)); // the dialog opens
        fx.confirms.answer(&clear_key(&mods.modules[1]), false);
        fx.host(ABS, |h| v.tick(h));
        assert!(matches!(v.step, Step::Idle));
        assert!(bus.with_sid(0x14).is_empty());
        // A plain (file) confirmation, or one for another module.
        let reported = read_codes_op(&a, mods.modules[1].clone()).unwrap().dtcs;
        let req = ConfirmRequest {
            key: "plain".into(),
            title: "t".into(),
            body: "b".into(),
            typed: None,
            confirm_label: "ok".into(),
        };
        fx.host(ABS, |h| h.confirm(req.clone()));
        fx.confirms.answer("plain", true);
        let ConfirmResult::Accepted(plain) = fx.host(ABS, |h| h.confirm(req)) else {
            panic!()
        };
        assert!(clear_op(&a, plain, mods.modules[1].clone(), reported.clone()).is_err());
        let engine_ack = fx.ecu_ack(DiagTarget::ENGINE);
        assert!(clear_op(&a, engine_ack, mods.modules[1].clone(), reported).is_err());
        assert!(bus.with_sid(0x14).is_empty());
        assert_eq!(dtcs(&bus, ABS), 1);
    }

    /// A clear-all run up to its summary, after a scan.
    fn clear_all_view(bus: &Bus, mods: &VehicleModules, fx: &mut Fixture) -> AutoscanView {
        let mut v = AutoscanView {
            records: pl::Records::in_dir(&fx.dir),
            modules: Some(mods.clone()),
            results: scan_op(&bus.access("sim"), mods.clone()).unwrap(),
            ..Default::default()
        };
        bus.clear_wire();
        fx.host(DiagTarget::ENGINE, |h| v.request_clear_all(h));
        v
    }

    #[test]
    fn clear_all_lists_every_module_first_and_sends_nothing_for_it() {
        let (bus, mods) = vehicle();
        let mut fx = Fixture::new("scan-all-summary");
        fx.adapter = Some(bus.access("sim"));
        let mut v = clear_all_view(&bus, &mods, &mut fx);
        let Step::Summary(req, list) = &v.step else {
            panic!("expected the summary")
        };
        // Every module that will be touched (the absent airbag is not).
        assert!(req
            .body
            .contains("• Engine (tx 0x7E0 / rx 0x7E8): 1 code(s)"));
        assert!(req.body.contains("• ABS (tx 0x760 / rx 0x768): 1 code(s)"));
        assert!(!req.body.contains("Airbag"));
        assert_eq!(list.len(), 2);
        // Declining the summary sends nothing at all.
        fx.host(DiagTarget::ENGINE, |h| v.tick(h));
        fx.confirms.answer("autoscan-clear-all", false);
        fx.host(DiagTarget::ENGINE, |h| v.tick(h));
        assert!(matches!(v.step, Step::Idle));
        assert!(bus.wire().is_empty());
    }

    #[test]
    fn one_confirmation_never_authorises_more_than_one_module() {
        let (bus, mods) = vehicle();
        let mut fx = Fixture::new("scan-all");
        fx.adapter = Some(bus.access("sim"));
        const APP: DiagTarget = DiagTarget::ENGINE;
        let mut v = clear_all_view(&bus, &mods, &mut fx);
        // Accepting the summary authorises no module: nothing is cleared.
        fx.host(APP, |h| v.tick(h));
        fx.confirms.answer("autoscan-clear-all", true);
        run(&mut v, &mut fx, APP);
        assert!(bus.with_sid(0x14).is_empty());
        let Step::Confirm(req, m, _) = &v.step else {
            panic!("expected the engine's own confirmation")
        };
        assert_eq!(m.name, "Engine");
        assert!(req.body.contains("(module 1 of 2)"), "{}", req.body);
        assert_eq!(v.reports.len(), 1, "the engine's report, before its clear");
        // An answer for the ABS given now (its dialog is not open) is no
        // answer at all; the engine's acceptance covers the engine only.
        fx.confirms.answer(&clear_key(&mods.modules[1]), true);
        fx.host(APP, |h| v.tick(h)); // the engine dialog opens
        fx.confirms.answer(&clear_key(&mods.modules[0]), true);
        run(&mut v, &mut fx, APP);
        assert_eq!(
            bus.with_sid(0x14),
            vec![(0x7E0, vec![0x14, 0xFF, 0xFF, 0xFF])]
        );
        // The ABS now has its own report and its own pending confirmation.
        let Step::Confirm(req, m, _) = &v.step else {
            panic!("expected the ABS's own confirmation")
        };
        assert_eq!(m.name, "ABS");
        assert!(req.body.contains("(module 2 of 2)"));
        assert_eq!(v.reports.len(), 2);
        assert_ne!(v.reports[0], v.reports[1]);
        for _ in 0..5 {
            fx.host(APP, |h| v.tick(h));
        }
        assert!(matches!(v.step, Step::Confirm(..)), "still pending");
        assert_eq!(
            dtcs(&bus, ABS),
            1,
            "not cleared without its own confirmation"
        );
        // Declined: skipped, nothing sent to it.
        fx.confirms.answer(&clear_key(&mods.modules[1]), false);
        run(&mut v, &mut fx, APP);
        assert!(matches!(v.step, Step::Idle));
        assert_eq!(bus.with_sid(0x14).len(), 1);
        assert_eq!(dtcs(&bus, ABS), 1);
        assert_eq!(dtcs(&bus, DiagTarget::ENGINE), 0);
        assert_eq!(v.outcomes.len(), 2);
        assert!(v.outcomes[1].1.contains("skipped"));
    }

    #[test]
    fn changed_codes_clear_nothing_and_the_hand_back_still_runs() {
        let (bus, mods) = vehicle();
        let a = bus.access("sim");
        let mut fx = Fixture::new("scan-changed");
        let ack = fx.ecu_ack(ABS);
        // The report said "no codes", the module has one now.
        let act = clear_op(&a, ack, mods.modules[1].clone(), Vec::new()).unwrap();
        assert!(act.result.unwrap_err().contains("changed"));
        assert!(act.restored.is_ok(), "restore ran after the failed run");
        assert!(bus.with_sid(0x14).is_empty());
        assert_eq!(dtcs(&bus, ABS), 1);
    }
}
