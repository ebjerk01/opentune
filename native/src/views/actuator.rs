//! Actuator tests: timed output tests (UDS `0x2F`) on the actuator channels
//! an adaptation definition DECLARES, always handed back to the ECU (port of
//! the webview app's Actuator tests panel).
//!
//! The hub's actuator gates are enforced, not re-implemented:
//! [`opentune_hub::ops::actuator::plan_ask`] validates and plans (declared
//! channel / option / value range / duration cap; sends nothing) and
//! [`opentune_hub::ops::actuator::prepare`] checks Bench tier, the per-run
//! ack, the free-form refusal, a known target and SecurityAccess before the
//! adapter is touched. The run itself is the engine's
//! [`opentune_adaptation::actuator::run_test`] inside `adapter.actuate` (the
//! hub's `begin` needs the raw `AdapterHub`, which a native view never gets;
//! `actuate` gives the same single-operation hold plus a hand-back the
//! adapter guarantees).
//!
//! ## Per-op table (reviewed before merge)
//!
//! | Operation | SIDs sent (in order) | Path | Backup source |
//! |---|---|---|---|
//! | List channels / Plan | none (`actuators`, hub `plan_ask`) | offline | n/a |
//! | Run test — 1. confirm | none | `Host::confirm_ecu_for(req, module, "<definition name> (<ids>)")` (the definition's module, else the app target; per-module key; dialog shows the plan) | — |
//! | Run test — 2. gates | none | `ack.target() == module`, then hub `prepare` (Bench tier, ack, no free-form, known target = the confirmed module, SecurityAccess: a secured channel needs a seed/key id that resolves in the build's seed/key resolvers) | — |
//! | Run test — 3. run | run: `10 03`; `27 <lvl>`, `27 <lvl+1> <key>` (secured channels only; the handler is `Unlocker::seed_key`; a rejected key is never retried and drives nothing); `2F <did> <option> <record>`; `3E 00` every 2 s while held (until the duration ends or STOP); `2F <did> 00` (up to 2 attempts); `10 01`. restore (always): when the run did not report the output safe: `2F <did> 00`; when the run did not report the default session: `10 01` | `actuate` (the `Ack`'s module) | n/a (non-persistent) |
//! | Stop | none by itself: sets the stop flag; the running test sends its `2F <did> 00` + `10 01` | (inside the same `actuate`) | n/a |
//!
//! Not offered: free-form IO DIDs (the gate refuses them outside alpha
//! Debug, and this panel never asks).
//!
//! The gate's SecurityAccess check takes the build's seed/key resolvers
//! (`prepare(…, keys, …, seedkey)`); the panel hands it the host's own (the
//! same set `Host::unlocker_for` resolves through) and the run's handler
//! comes from `Unlocker::seed_key`.

use super::coding::plumbing as pl;
use crate::host::{
    Ack, Actuation, AdapterAccess, BuildChannel, ConfirmRequest, ConfirmResult, DiagTarget, Host,
    JobHandle, NativeView, Tier, Unlocker, ViewGroup,
};
use crate::icons;
use crate::theme::{self, Tone};
use crate::widgets::packs::{PackKind, PackPicker};
use eframe::egui::{self, RichText};
use opentune_adaptation::actuator::{
    actuators, run_test, ActuatorInfo, Exchange, RealClock, TestClock, TestLogin, TestOutcome,
    TestPlan, TestReport,
};
use opentune_adaptation::IoControl;
use opentune_core::RawDiag;
use opentune_hub::ops::actuator::{load_def, plan_ask, prepare, Ask};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Bench;

pub fn create() -> Box<dyn NativeView> {
    Box::new(ActuatorView::default())
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct Loaded {
    pub path: PathBuf,
    pub name: String,
    pub channels: Vec<ActuatorInfo>,
    /// The module the definition names (`req_id` / `resp_id`).
    pub module: Option<DiagTarget>,
}

pub(crate) fn load(path: &Path) -> Result<Loaded, String> {
    let (def, text) = load_def(&path.to_string_lossy())?;
    Ok(Loaded {
        path: path.to_path_buf(),
        name: def.name.clone(),
        channels: actuators(&def).map_err(|e| e.to_string())?,
        module: pl::def_module(&text)?,
    })
}

/// The hub's `Ask` for a declared channel (never free-form).
pub(crate) fn ask(
    def: &Path,
    channel: &str,
    option: IoControl,
    value: Option<f64>,
    duration_ms: u64,
) -> Ask {
    Ask {
        def_path: Some(def.to_string_lossy().into_owned()),
        channel: Some(channel.to_string()),
        option: option.name().to_string(),
        value,
        raw: None,
        duration_ms,
        freeform_did: None,
        state: None,
        freeform_security: false,
    }
}

/// The hub's gates for a run on the confirmed module: Bench tier, the ack,
/// no free-form, a known target, SecurityAccess (`keys` + `seedkey`: the
/// gate checks the id resolves; the run's handler comes from
/// `Unlocker::seed_key`). Sends nothing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn gate(
    ask: &Ask,
    channel: &BuildChannel,
    keys: &opentune_hub::seedkeys::SeedKeys,
    ack: &Ack,
    module: DiagTarget,
    driver: &str,
    seedkey: Option<&str>,
) -> Result<TestPlan, String> {
    let t = ack.ecu_target()?;
    if t != module {
        return Err(format!(
            "the confirmation does not name module {module}; nothing was sent"
        ));
    }
    if !pl::is_isotp(t) {
        return Err(format!(
            "actuator tests are UDS on CAN; the confirmed module is {t}"
        ));
    }
    let (tx, rx) = (format!("{:X}", t.tx), format!("{:X}", t.rx));
    let start = prepare(
        ask,
        channel,
        keys,
        (driver, None, None, None),
        Some(&tx),
        Some(&rx),
        1000,
        true,
        false,
        seedkey,
    )?;
    if (start.key.addr.tx_id, start.key.addr.rx_id) != (t.tx, t.rx) {
        return Err("the prepared target is not the confirmed module; nothing was sent".into());
    }
    Ok(start.planned.plan)
}

/// Feeds every exchange to the panel as it happens.
struct FeedClock {
    inner: RealClock,
    feed: Arc<Mutex<Vec<Exchange>>>,
}

impl TestClock for FeedClock {
    fn now_ms(&mut self) -> u64 {
        self.inner.now_ms()
    }
    fn wait(&mut self, ms: u64) {
        self.inner.wait(ms)
    }
    fn stop_requested(&mut self) -> bool {
        self.inner.stop_requested()
    }
    fn on_exchange(&mut self, ex: &Exchange) {
        self.feed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(ex.clone());
    }
}

/// Is the module's reply a positive response to `sid`?
fn positive(r: &Result<Vec<u8>, String>, sid: u8) -> bool {
    matches!(r, Ok(b) if b.first() == Some(&(sid + 0x40)))
}

/// Run a gated plan on the confirmed module (`actuate`). The result is `Ok`
/// for a completed or stopped test; the report of any other ending is in
/// `report` (and the error text). The hand-back returns control and the
/// default session unless the run's own report says it already did.
pub(crate) fn run_op(
    a: &Arc<dyn AdapterAccess>,
    ack: Ack,
    plan: TestPlan,
    stop: Arc<AtomicBool>,
    feed: Arc<Mutex<Vec<Exchange>>>,
    report: Arc<Mutex<Option<TestReport>>>,
    login: Option<Unlocker>,
) -> Result<Actuation<TestReport>, String> {
    if plan.requires_security && login.is_none() {
        return Err("this test needs SecurityAccess: enter a seed/key id; nothing was sent".into());
    }
    let did = plan.did;
    let r2 = report.clone();
    a.actuate(
        opentune_hub::ops::actuator::TEST_OP,
        ack,
        move |d| {
            // Keys only: the engine runs 27 lvl / 27 lvl+1 once and never
            // retries a rejected key.
            let handler = match &login {
                Some(u) if plan.requires_security => Some(u.seed_key()?),
                _ => None,
            };
            let login = TestLogin {
                confirmed: true,
                seed_key: handler.as_deref(),
            };
            let mut clock = FeedClock {
                inner: RealClock::new(stop),
                feed,
            };
            let rep = run_test(d, &plan, &login, &mut clock).map_err(|e| e.to_string())?;
            *report.lock().unwrap_or_else(|e| e.into_inner()) = Some(rep.clone());
            match &rep.outcome {
                TestOutcome::Completed | TestOutcome::Stopped => Ok(rep),
                o => Err(format!("actuator test did not complete: {o:?}")),
            }
        },
        move |d: &mut dyn RawDiag| {
            let rep = r2.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let (safe, default) = rep
                .as_ref()
                .map(|r| (r.safe, r.default_session))
                .unwrap_or((false, false));
            let mut returned = safe;
            let mut why = None;
            if !safe {
                let r = d.request(&[0x2F, (did >> 8) as u8, did as u8, 0x00]);
                returned = positive(&r, 0x2F);
                if !returned {
                    why = Some(format!("returnControlToECU: {r:02X?}"));
                }
            }
            let left = default || positive(&d.request(&[0x10, 0x01]), 0x10);
            // ISO 14229: leaving to the default session ends any IO control
            // the tester holds, so either confirmed step is a hand-back.
            if returned || left {
                Ok(())
            } else {
                Err(why.unwrap_or_else(|| "default session (10 01) refused".into()))
            }
        },
    )
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

pub(crate) enum Step {
    Idle,
    Confirm(ConfirmRequest, Ask, DiagTarget, String),
    Running(JobHandle<Actuation<TestReport>>),
}

pub struct ActuatorView {
    picker: PackPicker,
    pub(crate) loaded: Option<Loaded>,
    sel: Option<usize>,
    option: IoControl,
    value: f64,
    duration_ms: u64,
    plan: Option<Result<TestPlan, String>>,
    pub(crate) step: Step,
    stop: Arc<AtomicBool>,
    feed: Arc<Mutex<Vec<Exchange>>>,
    report: Arc<Mutex<Option<TestReport>>>,
    /// The seed/key id for a secured channel (blank: none).
    pub(crate) seedkey: String,
    pub(crate) msg: Option<(Tone, String)>,
}

impl Default for ActuatorView {
    fn default() -> Self {
        Self {
            picker: PackPicker::new(PackKind::Adaptation),
            loaded: None,
            sel: None,
            option: IoControl::ShortTermAdjustment,
            value: 0.0,
            duration_ms: 3000,
            plan: None,
            step: Step::Idle,
            stop: Arc::new(AtomicBool::new(false)),
            feed: Default::default(),
            report: Default::default(),
            seedkey: String::new(),
            msg: None,
        }
    }
}

impl ActuatorView {
    pub(crate) fn set_def(&mut self, path: &Path) {
        match load(path) {
            Ok(l) => {
                self.loaded = Some(l);
                self.sel = None;
                self.plan = None;
                self.msg = None;
            }
            Err(e) => self.msg = Some((Tone::Crit, e)),
        }
    }

    fn running(&self) -> bool {
        !matches!(self.step, Step::Idle)
    }

    fn current_ask(&self) -> Option<Ask> {
        let l = self.loaded.as_ref()?;
        let c = l.channels.get(self.sel?)?;
        let value = (c.value.is_some() && self.option == IoControl::ShortTermAdjustment)
            .then_some(self.value);
        Some(ask(&l.path, &c.name, self.option, value, self.duration_ms))
    }

    /// Ask to run the selected test (opens the confirmation for the
    /// definition's module, by name). Refused, sending nothing, when the plan
    /// does not validate.
    pub(crate) fn request_run(&mut self, host: &mut Host<'_>) {
        let (Some(l), Some(ask)) = (self.loaded.as_ref(), self.current_ask()) else {
            return;
        };
        let module = match pl::module_of(host, l.module) {
            Ok(m) => m,
            Err(e) => return self.msg = Some((Tone::Crit, e)),
        };
        let plan = match plan_ask(&ask, host.channel()) {
            Ok(p) => p.plan,
            Err(e) => return self.msg = Some((Tone::Crit, e)),
        };
        let req = ConfirmRequest {
            key: pl::confirm_key("actuator-run", module),
            title: format!("Actuator test: {}", plan.channel),
            body: format!(
                "Drives output DID 0x{:04X} ({}) on the module for {} ms, then returns control \
                 to the ECU (always, also after an error or STOP). Make sure moving or \
                 switching this output is safe now.",
                plan.did,
                plan.option.label(),
                plan.duration_ms
            ),
            typed: None,
            confirm_label: "Run test".into(),
        };
        let label = pl::module_label(&l.name, module);
        self.step = Step::Confirm(req, ask, module, label);
    }

    pub(crate) fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub(crate) fn tick(&mut self, host: &mut Host<'_>) {
        self.step = match std::mem::replace(&mut self.step, Step::Idle) {
            Step::Idle => Step::Idle,
            Step::Confirm(req, ask, module, label) => {
                match host.confirm_ecu_for(req.clone(), module, &label) {
                    ConfirmResult::Pending => Step::Confirm(req, ask, module, label),
                    ConfirmResult::Declined => {
                        self.msg = Some((Tone::Muted, "cancelled: nothing was sent".into()));
                        Step::Idle
                    }
                    ConfirmResult::Accepted(ack) => {
                        let driver = host.link().driver;
                        let id = self.seedkey.trim().to_string();
                        let sk = (!id.is_empty()).then_some(id.as_str());
                        let gated = gate(
                            &ask,
                            host.channel(),
                            host.seedkeys,
                            &ack,
                            module,
                            &driver,
                            sk,
                        )
                        .and_then(|plan| match sk {
                            Some(id) if plan.requires_security => {
                                host.unlocker_for(id).map(|u| (plan, Some(u)))
                            }
                            _ => Ok((plan, None)),
                        });
                        match (gated, host.adapter()) {
                            (Ok((plan, login)), Some(a)) => {
                                self.stop = Arc::new(AtomicBool::new(false));
                                self.feed = Default::default();
                                self.report = Default::default();
                                let (s, f, r) =
                                    (self.stop.clone(), self.feed.clone(), self.report.clone());
                                Step::Running(host.spawn_job("actuator test", move |_| {
                                    run_op(&a, ack, plan, s, f, r, login)
                                }))
                            }
                            (Err(e), _) => {
                                self.msg = Some((Tone::Crit, format!("refused: {e}")));
                                Step::Idle
                            }
                            (_, None) => Step::Idle,
                        }
                    }
                }
            }
            Step::Running(mut job) => match job.take() {
                None => Step::Running(job),
                Some(r) => {
                    self.msg = Some(match r.and_then(Actuation::into_result) {
                        Ok(rep) => (
                            Tone::Ok,
                            format!(
                                "held {} ms{}; control returned to the ECU",
                                rep.held_ms,
                                if rep.stopped { " (stopped)" } else { "" }
                            ),
                        ),
                        Err(e) => (Tone::Crit, e),
                    });
                    Step::Idle
                }
            },
        };
    }
}

impl NativeView for ActuatorView {
    fn id(&self) -> &str {
        "actuator"
    }
    fn label(&self) -> &str {
        "Actuator tests"
    }
    fn icon(&self) -> &str {
        icons::POWER
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Vehicle
    }
    fn tier(&self) -> Tier {
        TIER
    }
    fn ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        self.tick(host);
        let t = *host.tokens();
        if self.running() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
        theme::panel(
            ui,
            &t,
            "Actuator tests",
            |_| {},
            |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_space(6.0);
                    if let Some(d) = self.picker.ui(ui, host) {
                        self.set_def(&d.path);
                    }
                    if theme::button(ui, &t, Some(icons::FOLDER_OPEN), "Open definition…").clicked()
                    {
                        if let Some(p) =
                            host.pick_file("Adaptation definition", &[("JSON", &["json"])])
                        {
                            self.set_def(&p);
                        }
                    }
                    if let Some((tone, m)) = &self.msg {
                        pl::note(ui, host, *tone, m);
                    }
                    let Some(l) = self.loaded.clone() else {
                        pl::note(ui, host, Tone::Muted, "Choose an adaptation definition.");
                        return;
                    };
                    pl::mono(ui, host, &l.name);
                    if l.channels.is_empty() {
                        pl::note(ui, host, Tone::Muted, "It declares no actuator channels.");
                    }
                    for (i, c) in l.channels.iter().enumerate() {
                        if ui
                            .selectable_label(
                                self.sel == Some(i),
                                format!("{}  ·  DID 0x{:04X}", c.name, c.did),
                            )
                            .clicked()
                        {
                            self.sel = Some(i);
                            self.plan = None;
                            self.option = *c
                                .options
                                .iter()
                                .find(|o| o.takes_control())
                                .unwrap_or(&IoControl::ReturnControlToEcu);
                            self.duration_ms = self.duration_ms.min(c.max_duration_ms);
                        }
                    }
                    let Some(c) = self.sel.and_then(|i| l.channels.get(i)).cloned() else {
                        return;
                    };
                    ui.add_space(6.0);
                    if c.requires_security {
                        pl::note(
                            ui,
                            host,
                            Tone::Warn,
                            &format!(
                                "This module needs SecurityAccess (level 0x{:02X}) for IO \
                                 control: enter a seed/key id (OpenTune ships none).",
                                c.security_level
                            ),
                        );
                        ui.horizontal(|ui| {
                            ui.label("Seed/key id");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.seedkey)
                                    .desired_width(140.0)
                                    .hint_text("none"),
                            );
                        });
                    }
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("actuator-option")
                            .selected_text(self.option.label())
                            .show_ui(ui, |ui| {
                                for o in &c.options {
                                    ui.selectable_value(&mut self.option, *o, o.label());
                                }
                            });
                        if let Some(v) = &c.value {
                            ui.label("value");
                            ui.add(egui::DragValue::new(&mut self.value).range(v.min..=v.max));
                            ui.label(RichText::new(&v.unit).color(t.fg_dim));
                        }
                        ui.label("ms");
                        ui.add(
                            egui::DragValue::new(&mut self.duration_ms)
                                .range(1..=c.max_duration_ms),
                        );
                    });
                    let module = pl::module_of(host, l.module);
                    let blocker = module.as_ref().err().cloned();
                    let off = module
                        .as_ref()
                        .ok()
                        .and_then(|m| pl::off_target_note(host, *m));
                    ui.horizontal(|ui| {
                        if theme::button(ui, &t, Some(icons::LIST_CHECKS), "Plan").clicked() {
                            self.plan = self
                                .current_ask()
                                .map(|a| plan_ask(&a, host.channel()).map(|p| p.plan));
                        }
                        let can = crate::widgets::caps::gate(
                            ui,
                            host,
                            &[crate::widgets::caps::Need::Can],
                        );
                        let ok = can && blocker.is_none() && !self.running();
                        if ui
                            .add_enabled_ui(ok, |ui| {
                                theme::primary_button(ui, &t, Some(icons::PLAY), "Run…")
                            })
                            .inner
                            .clicked()
                        {
                            self.request_run(host);
                        }
                        if ui
                            .add_enabled_ui(matches!(self.step, Step::Running(_)), |ui| {
                                theme::primary_button(ui, &t, Some(icons::STOP), "STOP")
                            })
                            .inner
                            .clicked()
                        {
                            self.request_stop();
                        }
                    });
                    if let Some(b) = blocker.as_ref().or(off.as_ref()) {
                        pl::note(ui, host, Tone::Warn, b);
                    }
                    match &self.plan {
                        Some(Ok(p)) => {
                            for s in &p.steps {
                                pl::mono(
                                    ui,
                                    host,
                                    &format!("{:?}  {}  {}", s.phase, pl::hex(&s.request), s.note),
                                );
                            }
                        }
                        Some(Err(e)) => pl::note(ui, host, Tone::Crit, e),
                        None => {}
                    }
                    let feed = self.feed.lock().map(|f| f.clone()).unwrap_or_default();
                    if !feed.is_empty() {
                        theme::section_label(ui, &t, "Exchanges");
                        for ex in feed.iter().rev().take(40) {
                            pl::mono(
                                ui,
                                host,
                                &format!(
                                    "{:>6} ms  {:?}  → {}  ← {}",
                                    ex.t_ms,
                                    ex.phase,
                                    pl::hex(&ex.request),
                                    ex.response
                                        .as_deref()
                                        .map(pl::hex)
                                        .or_else(|| ex.error.clone())
                                        .unwrap_or_default()
                                ),
                            );
                        }
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
    use crate::host::HostShared;
    use opentune_udssim::server::UdsServer;

    const ECU: DiagTarget = DiagTarget::can(0x7E0, 0x7E8);
    const TCU: DiagTarget = DiagTarget::can(0x7E1, 0x7E9);

    fn def_file(dir: &Path) -> PathBuf {
        let p = dir.join("act.json");
        std::fs::write(
            &p,
            r#"{"id":"act","name":"Act","req_id":"7E0","resp_id":"7E8","channels":[
               {"name":"Fan","kind":"actuator","did":1280,"state":[1],
                "options":["short_term_adjustment"],"max_duration_ms":5000}]}"#,
        )
        .unwrap();
        p
    }

    fn bus() -> Bus {
        let s = || {
            let mut s = UdsServer::new();
            s.set_io(0x0500, vec![0x00]);
            s
        };
        Bus::default().uds(0x7E0, 0x7E8, s()).uds(0x7E1, 0x7E9, s())
    }

    fn run(v: &mut ActuatorView, fx: &mut Fixture, t: DiagTarget) {
        let t0 = std::time::Instant::now();
        while !matches!(v.step, Step::Idle) {
            assert!(t0.elapsed() < std::time::Duration::from_secs(10), "stuck");
            fx.host(t, |h| v.tick(h));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn view(fx: &Fixture) -> ActuatorView {
        let mut v = ActuatorView::default();
        v.set_def(&def_file(&fx.dir));
        v.sel = Some(0);
        v.duration_ms = 50;
        v
    }

    #[test]
    fn a_test_runs_on_the_confirmed_module_and_hands_control_back() {
        let bus = bus();
        let mut fx = Fixture::new("act-run");
        fx.adapter = Some(bus.access("sim"));
        let mut v = view(&fx);
        fx.host(ECU, |h| v.request_run(h));
        fx.host(ECU, |h| v.tick(h));
        assert!(bus.wire().is_empty(), "nothing before the confirmation");
        fx.confirms
            .answer(&pl::confirm_key("actuator-run", ECU), true);
        run(&mut v, &mut fx, ECU);
        assert_eq!(v.msg.as_ref().unwrap().0, Tone::Ok, "{:?}", v.msg);
        let sent = bus.sent_to(0x7E0);
        assert_eq!(sent[0], vec![0x10, 0x03]);
        assert_eq!(sent[1], vec![0x2F, 0x05, 0x00, 0x03, 0x01]);
        assert!(sent.contains(&vec![0x2F, 0x05, 0x00, 0x00]));
        assert_eq!(sent.last().unwrap(), &vec![0x10, 0x01]);
        assert!(!bus.with_uds(0x7E0, 0x7E8, |s| s.io_overridden(0x0500)));
        assert!(bus.sent_to(0x7E1).is_empty());
    }

    #[test]
    fn the_hub_gates_refuse_before_anything_is_sent() {
        let bus = bus();
        let mut fx = Fixture::new("act-gates");
        fx.adapter = Some(bus.access("sim"));
        let mut v = view(&fx);
        // Declined.
        fx.host(ECU, |h| v.request_run(h));
        fx.host(ECU, |h| v.tick(h));
        fx.confirms
            .answer(&pl::confirm_key("actuator-run", ECU), false);
        fx.host(ECU, |h| v.tick(h));
        assert!(matches!(v.step, Step::Idle));
        // A plan outside the declared cap.
        v.duration_ms = 60_000;
        fx.host(ECU, |h| v.request_run(h));
        assert!(matches!(v.step, Step::Idle));
        // The stable channel: the hub's Bench gate refuses the run.
        v.duration_ms = 50;
        fx.shared = HostShared::new(BuildChannel::new(BuildChannel::STABLE), None);
        fx.host(ECU, |h| v.request_run(h));
        fx.host(ECU, |h| v.tick(h));
        fx.confirms
            .answer(&pl::confirm_key("actuator-run", ECU), true);
        run(&mut v, &mut fx, ECU);
        assert!(v.msg.as_ref().unwrap().1.contains("Bench"), "{:?}", v.msg);
        // A plain (file) Ack fails the gate.
        let req = ConfirmRequest {
            key: "plain".into(),
            title: "t".into(),
            body: "b".into(),
            typed: None,
            confirm_label: "ok".into(),
        };
        fx.host(ECU, |h| h.confirm(req.clone()));
        fx.confirms.answer("plain", true);
        let ConfirmResult::Accepted(plain) = fx.host(ECU, |h| h.confirm(req)) else {
            panic!()
        };
        let a = v.current_ask().unwrap();
        assert!(gate(
            &a,
            &BuildChannel::new(BuildChannel::BENCH),
            &Default::default(),
            &plain,
            ECU,
            "sim",
            None
        )
        .is_err());
        // An ECU Ack for another module (the TCU) fails the module check.
        let tcu = fx.ecu_ack(TCU);
        let e = gate(
            &a,
            &BuildChannel::new(BuildChannel::BENCH),
            &Default::default(),
            &tcu,
            ECU,
            "sim",
            None,
        )
        .unwrap_err();
        assert!(e.contains("does not name module"), "{e}");
        assert!(bus.wire().is_empty(), "{:02X?}", bus.wire());
    }

    #[test]
    fn a_secured_channel_is_refused_by_the_gate() {
        let mut fx = Fixture::new("act-sec");
        let p = fx.dir.join("sec.json");
        std::fs::write(
            &p,
            r#"{"id":"s","name":"S","requires_security":true,"channels":[
               {"name":"Fan","kind":"actuator","did":1280,"state":[1]}]}"#,
        )
        .unwrap();
        let ack = fx.ecu_ack(ECU);
        let a = ask(&p, "Fan", IoControl::ShortTermAdjustment, None, 50);
        let e = gate(
            &a,
            &BuildChannel::new(BuildChannel::BENCH),
            &Default::default(),
            &ack,
            ECU,
            "sim",
            None,
        )
        .unwrap_err();
        assert!(e.contains("SecurityAccess"), "{e}");
    }

    #[test]
    fn the_hand_back_returns_control_when_the_run_could_not() {
        // The control reply is garbled (the output may be driven), and the
        // module refuses the run's two returnControlToECU; the hand-back's
        // third one is accepted.
        let mut returns = 0;
        let bus = Bus::default().script(
            0x7E0,
            0x7E8,
            Box::new(move |r: &[u8]| {
                Ok(match r {
                    [0x10, s] => vec![0x50, *s, 0x00, 0x32, 0x01, 0xF4],
                    [0x2F, _, _, 0x03, ..] => vec![0x6F, 0xFF, 0xFF, 0x03],
                    [0x2F, h, l, 0x00] => {
                        returns += 1;
                        if returns <= 2 {
                            vec![0x7F, 0x2F, 0x22]
                        } else {
                            vec![0x6F, *h, *l, 0x00]
                        }
                    }
                    _ => vec![0x7F, r[0], 0x11],
                })
            }),
        );
        let a = bus.access("sim");
        let mut fx = Fixture::new("act-fail");
        let ack = fx.ecu_ack(ECU);
        let p = def_file(&fx.dir);
        let plan = gate(
            &ask(&p, "Fan", IoControl::ShortTermAdjustment, None, 50),
            &BuildChannel::new(BuildChannel::BENCH),
            &Default::default(),
            &ack,
            ECU,
            "sim",
            None,
        )
        .unwrap();
        let report = Arc::new(Mutex::new(None));
        let act = run_op(
            &a,
            ack,
            plan,
            Arc::new(AtomicBool::new(false)),
            Default::default(),
            report.clone(),
            None,
        )
        .unwrap();
        assert!(act.result.is_err());
        assert!(act.restored.is_ok(), "{:?}", act.restored);
        let rep = report.lock().unwrap().clone().unwrap();
        assert!(!rep.safe);
        let returns: Vec<_> = bus
            .sent_to(0x7E0)
            .into_iter()
            .filter(|r| r == &vec![0x2F, 0x05, 0x00, 0x00])
            .collect();
        assert_eq!(returns.len(), 3, "two by the run, one by the hand-back");
    }

    #[test]
    fn stop_ends_the_hold_early_and_still_hands_back() {
        let bus = bus();
        let a = bus.access("sim");
        let mut fx = Fixture::new("act-stop");
        let ack = fx.ecu_ack(ECU);
        let p = def_file(&fx.dir);
        let plan = gate(
            &ask(&p, "Fan", IoControl::ShortTermAdjustment, None, 5000),
            &BuildChannel::new(BuildChannel::BENCH),
            &Default::default(),
            &ack,
            ECU,
            "sim",
            None,
        )
        .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            s2.store(true, Ordering::SeqCst);
        });
        let t0 = std::time::Instant::now();
        let act = run_op(
            &a,
            ack,
            plan,
            stop,
            Default::default(),
            Default::default(),
            None,
        )
        .unwrap();
        assert!(t0.elapsed() < std::time::Duration::from_secs(3));
        let rep = act.into_result().unwrap();
        assert!(rep.stopped && rep.control_returned);
        assert!(!bus.with_uds(0x7E0, 0x7E8, |s| s.io_overridden(0x0500)));
    }

    fn secured(
        fx: &Fixture,
        checker: Box<dyn opentune_udssim::security::SecurityChecker + Send>,
    ) -> (Bus, PathBuf) {
        let mut s = UdsServer::new();
        s.set_io(0x0500, vec![0x00]);
        s.set_security_checker(checker);
        let bus = Bus::default().uds(0x7E0, 0x7E8, s);
        let p = fx.dir.join("sec.json");
        std::fs::write(
            &p,
            r#"{"id":"s","name":"S","req_id":"7E0","resp_id":"7E8","requires_security":true,
               "channels":[{"name":"Fan","kind":"actuator","did":1280,"state":[1]}]}"#,
        )
        .unwrap();
        (bus, p)
    }

    fn secured_run(fx: &mut Fixture, bus: &Bus, p: &Path) -> Result<Actuation<TestReport>, String> {
        fx.keys = super::super::coding::simkit::xor_keys();
        let ack = fx.ecu_ack(ECU);
        let keys = fx.keys.clone();
        let plan = gate(
            &ask(p, "Fan", IoControl::ShortTermAdjustment, None, 50),
            &BuildChannel::new(BuildChannel::BENCH),
            &keys,
            &ack,
            ECU,
            "sim",
            Some("xor-5a"),
        )?;
        assert!(plan.requires_security);
        let u = fx.host(ECU, |h| h.unlocker_for("xor-5a")).unwrap();
        run_op(
            &bus.access("sim"),
            ack,
            plan,
            Arc::new(AtomicBool::new(false)),
            Default::default(),
            Default::default(),
            Some(u),
        )
    }

    #[test]
    fn a_secured_channel_runs_with_the_resolved_key_once() {
        let mut fx = Fixture::new("act-sec-ok");
        let (bus, p) = secured(
            &fx,
            Box::new(opentune_udssim::security::XorSecurity::new(0x5A)),
        );
        let act = secured_run(&mut fx, &bus, &p).unwrap();
        assert!(act.restored.is_ok());
        let rep = act.result.unwrap();
        assert!(rep.security_unlocked && rep.control_returned);
        assert_eq!(bus.with_sid(0x27).len(), 2, "seed + key, once");
        assert!(!bus.with_uds(0x7E0, 0x7E8, |s| s.io_overridden(0x0500)));
    }

    #[test]
    fn a_rejected_key_drives_nothing_and_is_never_retried() {
        let mut fx = Fixture::new("act-sec-bad");
        let (bus, p) = secured(&fx, Box::new(opentune_udssim::security::AlwaysReject));
        let act = secured_run(&mut fx, &bus, &p).unwrap();
        assert!(act.result.is_err());
        assert!(act.restored.is_ok(), "{:?}", act.restored);
        let keys = bus
            .with_sid(0x27)
            .into_iter()
            .filter(|(_, r)| r.get(1) == Some(&0x02))
            .count();
        assert_eq!(keys, 1, "one key, never retried");
        assert!(
            !bus.sent_to(0x7E0)
                .iter()
                .any(|r| r.starts_with(&[0x2F, 0x05, 0x00, 0x03])),
            "nothing was driven"
        );
    }

    #[test]
    fn a_secured_plan_without_a_key_sends_nothing() {
        let mut fx = Fixture::new("act-sec-none");
        let (bus, p) = secured(
            &fx,
            Box::new(opentune_udssim::security::XorSecurity::new(0x5A)),
        );
        fx.keys = super::super::coding::simkit::xor_keys();
        let ack = fx.ecu_ack(ECU);
        let keys = fx.keys.clone();
        let plan = gate(
            &ask(&p, "Fan", IoControl::ShortTermAdjustment, None, 50),
            &BuildChannel::new(BuildChannel::BENCH),
            &keys,
            &ack,
            ECU,
            "sim",
            Some("xor-5a"),
        )
        .unwrap();
        let ack = fx.ecu_ack(ECU);
        let e = run_op(
            &bus.access("sim"),
            ack,
            plan,
            Default::default(),
            Default::default(),
            Default::default(),
            None,
        )
        .unwrap_err();
        assert!(e.contains("SecurityAccess"), "{e}");
        assert!(bus.wire().is_empty());
    }
}
