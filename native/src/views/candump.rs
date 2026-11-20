//! CAN capture: passive raw CAN capture to a candump log, CSV or Vector ASC
//! file (port of the webview app's CAN capture panel, `feat_candump.rs`).
//!
//! The capture runs in the adapter backend
//! ([`AdapterAccess::capture_start`] / `capture_status` / `capture_stop`,
//! the hub's `ops::candump` job): it opens its own driver for the link's
//! cable through the passive `CanMonitor`, listen-only where the adapter
//! supports it, and never opens a diagnostic session, so it SENDS NOTHING.
//! It is Bench tier in the backend (refused on other channels) and holds the
//! adapter until it is stopped or its duration ends; every other panel gets
//! "adapter busy" meanwhile.
//!
//! ## Per-op table
//!
//! | Operation | Sent on the bus | Path | Files |
//! |---|---|---|---|
//! | Start | nothing (listen-only monitor) | `capture_start` from a job | the output file the user picked; an EXISTING file is replaced only after `Host::confirm` + `Host::write_file` (its old contents archived in the vault) |
//! | Status | nothing (counters only, never the adapter) | `capture_status` | — |
//! | Stop | nothing | `capture_stop` from a job (finishes the file, frees the adapter) | — |
//!
//! Leaving the panel does not stop a capture (it is passive and keeps
//! recording); closing the app stops it ([`Drop`]), so the file is finished.

use super::coding::plumbing as pl;
use super::ts::save::Saver;
use crate::host::{AdapterAccess, CaptureSpec, Host, JobHandle, NativeView, Tier, ViewGroup};
use crate::icons;
use crate::theme::{self, Tone};
use eframe::egui::{self, RichText};
use opentune_candump::Format;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Bench;

pub fn create() -> Box<dyn NativeView> {
    Box::new(CandumpView::default())
}

/// The bus bitrates offered (0 = the driver's default).
const BITRATES: [u32; 6] = [0, 33_333, 100_000, 125_000, 250_000, 500_000];
/// How often the counters are refreshed while capturing.
const STATUS_EVERY: Duration = Duration::from_millis(250);
/// Most identifiers listed.
const MAX_ID_ROWS: usize = 64;

/// Parse the identifier filter: hex ids (optional `0x`), separated by commas
/// or whitespace; each at most 29 bits. Empty = every frame.
pub(crate) fn parse_filter(s: &str) -> Result<Vec<u32>, String> {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .map(|t| {
            let h = t.trim_start_matches("0x").trim_start_matches("0X");
            let id =
                u32::from_str_radix(h, 16).map_err(|_| format!("'{t}' is not a hex CAN id"))?;
            if id > 0x1FFF_FFFF {
                return Err(format!("filter id {id:X} is larger than 29 bits"));
            }
            Ok(id)
        })
        .collect()
}

pub(crate) enum Step {
    Idle,
    /// The output file exists: the replace confirmation is open.
    Replace(CaptureSpec),
    Starting(JobHandle<Value>),
    Running,
    Stopping(JobHandle<Value>),
}

pub struct CandumpView {
    pub(crate) format: Format,
    pub(crate) filter: String,
    /// Seconds (0 = until stopped).
    pub(crate) duration_s: u32,
    pub(crate) bitrate: u32,
    pub(crate) path: Option<PathBuf>,
    pub(crate) step: Step,
    saver: Saver,
    /// The latest counters (also of the last capture once it ended).
    pub(crate) status: Option<Value>,
    last_status: Option<Instant>,
    pub(crate) msg: Option<(Tone, String)>,
    /// The adapter of the running capture (stopped in `Drop`).
    adapter: Option<Arc<dyn AdapterAccess>>,
}

impl Default for CandumpView {
    fn default() -> Self {
        Self {
            format: Format::Candump,
            filter: String::new(),
            duration_s: 0,
            bitrate: 0,
            path: None,
            step: Step::Idle,
            saver: Saver::default(),
            status: None,
            last_status: None,
            msg: None,
            adapter: None,
        }
    }
}

impl CandumpView {
    pub(crate) fn busy(&self) -> bool {
        !matches!(self.step, Step::Idle)
    }

    fn spec(&self) -> Result<CaptureSpec, String> {
        let path = self
            .path
            .as_ref()
            .ok_or("choose the output file first (Save as…)")?;
        Ok(CaptureSpec {
            path: path.display().to_string(),
            format: self.format.as_str().into(),
            filter_ids: parse_filter(&self.filter)?,
            duration_s: (self.duration_s > 0).then_some(self.duration_s),
            bitrate: (self.bitrate > 0).then_some(self.bitrate),
        })
    }

    /// Start a capture (asks first when the output file exists).
    pub(crate) fn request_start(&mut self, host: &mut Host<'_>) {
        if self.busy() {
            return;
        }
        let spec = match self.spec() {
            Ok(s) => s,
            Err(e) => return self.msg = Some((Tone::Crit, e)),
        };
        if host.adapter().is_none() {
            return self.msg = Some((Tone::Crit, "no adapter layer in this build".into()));
        }
        let p = PathBuf::from(&spec.path);
        if p.exists() {
            // Archive and empty it through the host's guarded write first.
            self.saver.save("candump-replace", p, Vec::new());
            self.step = Step::Replace(spec);
        } else {
            self.begin(host, spec);
        }
    }

    fn begin(&mut self, host: &Host<'_>, spec: CaptureSpec) {
        let Some(a) = host.adapter() else {
            self.step = Step::Idle;
            return;
        };
        self.msg = None;
        self.status = None;
        self.adapter = Some(a.clone());
        self.step =
            Step::Starting(host.spawn_job("CAN capture start", move |_| a.capture_start(spec)));
    }

    pub(crate) fn request_stop(&mut self, host: &Host<'_>) {
        if !matches!(self.step, Step::Running) {
            return;
        }
        let Some(a) = self.adapter.clone() else {
            self.step = Step::Idle;
            return;
        };
        self.step = Step::Stopping(host.spawn_job("CAN capture stop", move |_| a.capture_stop()));
    }

    fn refresh(&mut self, force: bool) {
        if !force && self.last_status.is_some_and(|t| t.elapsed() < STATUS_EVERY) {
            return;
        }
        if let Some(a) = &self.adapter {
            if let Ok(s) = a.capture_status() {
                self.status = Some(s);
                self.last_status = Some(Instant::now());
            }
        }
    }

    pub(crate) fn tick(&mut self, host: &mut Host<'_>) {
        self.step = match std::mem::replace(&mut self.step, Step::Idle) {
            Step::Idle => Step::Idle,
            Step::Replace(spec) => match self.saver.tick(host) {
                None => Step::Replace(spec),
                Some(Ok(_)) => {
                    self.begin(host, spec);
                    return;
                }
                Some(Err(e)) => {
                    self.msg = Some((Tone::Muted, format!("{e}; nothing was captured")));
                    Step::Idle
                }
            },
            Step::Starting(mut job) => match job.take() {
                None => Step::Starting(job),
                Some(Ok(v)) => {
                    let ad = v["adapter"].as_str().unwrap_or_default().to_string();
                    self.msg = Some((Tone::Ok, format!("capturing ({ad})")));
                    self.refresh(true);
                    Step::Running
                }
                Some(Err(e)) => {
                    self.adapter = None;
                    self.msg = Some((Tone::Crit, format!("capture not started: {e}")));
                    Step::Idle
                }
            },
            Step::Running => {
                self.refresh(false);
                let ended = self
                    .status
                    .as_ref()
                    .is_some_and(|s| s["running"] == Value::Bool(false));
                match (ended, self.adapter.clone()) {
                    (false, _) => Step::Running,
                    // Ended by itself (its duration, or an error): finish the
                    // file and free the adapter through capture_stop.
                    (true, Some(a)) => Step::Stopping(
                        host.spawn_job("CAN capture stop", move |_| a.capture_stop()),
                    ),
                    (true, None) => Step::Idle,
                }
            }
            Step::Stopping(mut job) => match job.take() {
                None => Step::Stopping(job),
                Some(r) => {
                    self.refresh(true);
                    self.adapter = None;
                    let err = self
                        .status
                        .as_ref()
                        .and_then(|s| s["error"].as_str().map(str::to_string));
                    self.msg = Some(match (r, err) {
                        (Ok(v), None) => (
                            Tone::Ok,
                            format!(
                                "saved {}: {} frames in {:.1} s",
                                v["path"].as_str().unwrap_or("?"),
                                v["frames"].as_u64().unwrap_or(0),
                                v["elapsed_s"].as_f64().unwrap_or(0.0)
                            ),
                        ),
                        (Ok(_), Some(e)) => (Tone::Crit, format!("capture ended: {e}")),
                        (Err(e), _) => (Tone::Crit, format!("stop: {e}")),
                    });
                    Step::Idle
                }
            },
        };
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        let t = *host.tokens();
        let locked = self.busy();
        ui.add_enabled_ui(!locked, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Format").color(t.fg_dim));
                egui::ComboBox::from_id_salt("candump-format")
                    .selected_text(self.format.as_str())
                    .show_ui(ui, |ui| {
                        for f in [Format::Candump, Format::Csv, Format::Asc] {
                            ui.selectable_value(&mut self.format, f, f.as_str());
                        }
                    });
                ui.label(RichText::new("Bitrate").color(t.fg_dim));
                let br = |b: u32| {
                    if b == 0 {
                        "driver default".to_string()
                    } else {
                        format!("{} kbit/s", b as f64 / 1000.0)
                    }
                };
                egui::ComboBox::from_id_salt("candump-bitrate")
                    .selected_text(br(self.bitrate))
                    .show_ui(ui, |ui| {
                        for b in BITRATES {
                            ui.selectable_value(&mut self.bitrate, b, br(b));
                        }
                    });
                ui.label(RichText::new("Stop after (s, 0 = never)").color(t.fg_dim));
                ui.add(egui::DragValue::new(&mut self.duration_s).range(0..=86_400));
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("Only ids").color(t.fg_dim));
                ui.add(
                    egui::TextEdit::singleline(&mut self.filter)
                        .desired_width(260.0)
                        .hint_text("all (e.g. 2C0, 4A0)"),
                );
            });
            ui.horizontal(|ui| {
                if theme::button(ui, &t, Some(icons::FLOPPY_DISK), "Save as…").clicked() {
                    let name = format!("capture.{}", self.format.extension());
                    if let Some(p) = host.pick_save("CAN capture output file", &name) {
                        self.path = Some(p);
                    }
                }
                match &self.path {
                    Some(p) => pl::mono(ui, host, &p.display().to_string()),
                    None => pl::note(ui, host, Tone::Muted, "no output file chosen"),
                }
            });
        });
    }

    fn status_ui(&self, ui: &mut egui::Ui, host: &Host<'_>) {
        let t = *host.tokens();
        let Some(s) = &self.status else { return };
        ui.horizontal_wrapped(|ui| {
            let n = |k: &str| s[k].as_u64().unwrap_or(0);
            let tone = if s["running"] == Value::Bool(true) {
                t.ok
            } else {
                t.fg
            };
            let frames = n("frames").to_string();
            theme::stat(ui, &t, icons::WAVEFORM, "frames", &frames, tone);
            let fps = format!("{:.0}", s["fps"].as_f64().unwrap_or(0.0));
            theme::stat(ui, &t, icons::PULSE, "fps", &fps, t.fg);
            let el = format!("{:.1} s", s["elapsed_s"].as_f64().unwrap_or(0.0));
            theme::stat(ui, &t, icons::CLOCK, "elapsed", &el, t.fg);
            theme::stat(ui, &t, icons::FILE, "bytes", &n("bytes").to_string(), t.fg);
            let dropped = n("dropped");
            let dc = if dropped > 0 { t.warn } else { t.fg };
            theme::stat(ui, &t, icons::WARNING, "dropped", &dropped.to_string(), dc);
            let ids = n("unique_ids").to_string();
            theme::stat(ui, &t, icons::LIST_DASHES, "ids", &ids, t.fg);
        });
        if let Some(a) = s["adapter"].as_str().filter(|a| !a.is_empty()) {
            let lo = match s["listen_only"].as_bool() {
                Some(true) => "listen-only",
                Some(false) => "NOT listen-only (the adapter may acknowledge frames)",
                None => "listen-only: unknown",
            };
            pl::note(ui, host, Tone::Muted, &format!("{a} · {lo}"));
        }
        if let Some(e) = s["error"].as_str() {
            pl::note(ui, host, Tone::Crit, e);
        }
        let ids = s["ids"].as_array().cloned().unwrap_or_default();
        if ids.is_empty() {
            return;
        }
        theme::section_label(ui, &t, "Identifiers");
        egui::Grid::new("candump-ids")
            .striped(true)
            .num_columns(5)
            .show(ui, |ui| {
                for h in ["id", "count", "Hz", "dlc", "last data"] {
                    ui.label(RichText::new(h).color(t.fg_dim));
                }
                ui.end_row();
                for r in ids.iter().take(MAX_ID_ROWS) {
                    let id = r["id"].as_u64().unwrap_or(0);
                    let ext = r["ext"].as_bool().unwrap_or(false);
                    let ids = if ext {
                        format!("{id:08X}")
                    } else {
                        format!("{id:03X}")
                    };
                    pl::mono(ui, host, &ids);
                    pl::mono(ui, host, &r["count"].as_u64().unwrap_or(0).to_string());
                    pl::mono(ui, host, &format!("{:.1}", r["hz"].as_f64().unwrap_or(0.0)));
                    pl::mono(ui, host, &r["dlc"].as_u64().unwrap_or(0).to_string());
                    pl::mono(ui, host, r["last"].as_str().unwrap_or(""));
                    ui.end_row();
                }
            });
    }
}

impl Drop for CandumpView {
    fn drop(&mut self) {
        // Closing the app: finish the file and free the adapter.
        if matches!(self.step, Step::Running | Step::Starting(_)) {
            if let Some(a) = &self.adapter {
                let _ = a.capture_stop();
            }
        }
    }
}

impl NativeView for CandumpView {
    fn id(&self) -> &str {
        "candump"
    }
    fn label(&self) -> &str {
        "CAN capture"
    }
    fn icon(&self) -> &str {
        icons::WAVEFORM
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Vehicle
    }
    fn tier(&self) -> Tier {
        TIER
    }
    fn ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        self.tick(host);
        if self.busy() {
            ui.ctx().request_repaint_after(STATUS_EVERY);
        }
        let t = *host.tokens();
        theme::panel(
            ui,
            &t,
            "CAN capture",
            |_| {},
            |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_space(6.0);
                    pl::note(
                        ui,
                        host,
                        Tone::Muted,
                        "Records every frame on the bus through the adapter's passive monitor \
                         (listen-only where supported). Nothing is sent. The capture holds the \
                         adapter until it stops.",
                    );
                    self.settings_ui(ui, host);
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let can = crate::widgets::caps::gate(
                            ui,
                            host,
                            &[crate::widgets::caps::Need::Can],
                        );
                        let idle = matches!(self.step, Step::Idle);
                        if ui
                            .add_enabled_ui(can && idle && self.path.is_some(), |ui| {
                                theme::primary_button(ui, &t, Some(icons::RECORD), "Start")
                            })
                            .inner
                            .clicked()
                        {
                            self.request_start(host);
                        }
                        if ui
                            .add_enabled_ui(matches!(self.step, Step::Running), |ui| {
                                theme::primary_button(ui, &t, Some(icons::STOP), "Stop")
                            })
                            .inner
                            .clicked()
                        {
                            self.request_stop(host);
                        }
                        if matches!(self.step, Step::Starting(_) | Step::Stopping(_)) {
                            ui.spinner();
                        }
                    });
                    if let Some((tone, m)) = &self.msg {
                        pl::note(ui, host, *tone, m);
                    }
                    self.status_ui(ui, host);
                });
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{BuildChannel, DiagTarget, HostShared};
    use crate::hublink::{HubAdapter, LinkCfg};
    use crate::views::coding::simkit::Fixture;

    fn mock(channel: &str) -> Arc<dyn AdapterAccess> {
        let a = HubAdapter::with_registry().with_channel(BuildChannel::new(channel));
        a.set_link(Some(LinkCfg {
            driver: "mock".into(),
            port: String::new(),
            baud: 0,
            bus: String::new(),
        }));
        Arc::new(a)
    }

    fn run(v: &mut CandumpView, fx: &mut Fixture, until: impl Fn(&CandumpView) -> bool) {
        let t0 = Instant::now();
        while !until(v) {
            assert!(t0.elapsed() < Duration::from_secs(10), "stuck");
            fx.host(DiagTarget::ENGINE, |h| v.tick(h));
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn filters_parse_as_hex_ids_of_at_most_29_bits() {
        assert_eq!(parse_filter("").unwrap(), Vec::<u32>::new());
        assert_eq!(
            parse_filter("2C0, 0x4a0 1FFFFFFF").unwrap(),
            vec![0x2C0, 0x4A0, 0x1FFF_FFFF]
        );
        assert!(parse_filter("20000000").unwrap_err().contains("29 bits"));
        assert!(parse_filter("xyz").is_err());
    }

    #[test]
    fn a_capture_starts_reports_counters_and_stops_into_the_chosen_file() {
        let mut fx = Fixture::new("n7-cap");
        fx.adapter = Some(mock(BuildChannel::BENCH));
        let mut v = CandumpView::default();
        v.path = Some(fx.dir.join("cap.log"));
        fx.host(DiagTarget::ENGINE, |h| v.request_start(h));
        run(&mut v, &mut fx, |v| !matches!(v.step, Step::Starting(_)));
        assert!(matches!(v.step, Step::Running), "{:?}", v.msg);
        assert_eq!(v.status.as_ref().unwrap()["running"], Value::Bool(true));
        fx.host(DiagTarget::ENGINE, |h| v.request_stop(h));
        run(&mut v, &mut fx, |v| matches!(v.step, Step::Idle));
        assert_eq!(v.msg.as_ref().unwrap().0, Tone::Ok, "{:?}", v.msg);
        assert!(fx.dir.join("cap.log").exists());
        assert_eq!(v.status.as_ref().unwrap()["running"], Value::Bool(false));
    }

    #[test]
    fn a_timed_capture_ends_by_itself_and_frees_the_adapter() {
        let mut fx = Fixture::new("n7-cap-timed");
        let a = mock(BuildChannel::BENCH);
        fx.adapter = Some(a.clone());
        let mut v = CandumpView::default();
        v.path = Some(fx.dir.join("timed.csv"));
        v.format = Format::Csv;
        v.duration_s = 1;
        fx.host(DiagTarget::ENGINE, |h| v.request_start(h));
        run(&mut v, &mut fx, |v| matches!(v.step, Step::Idle));
        assert!(fx.dir.join("timed.csv").exists(), "{:?}", v.msg);
        assert_eq!(a.capture_status().unwrap()["running"], Value::Bool(false));
    }

    #[test]
    fn an_existing_output_file_is_archived_before_it_is_replaced() {
        let mut fx = Fixture::new("n7-cap-replace");
        fx.adapter = Some(mock(BuildChannel::BENCH));
        let p = fx.dir.join("old.log");
        std::fs::write(&p, b"earlier capture").unwrap();
        let mut v = CandumpView::default();
        v.path = Some(p.clone());
        // Declined: the file is untouched and nothing starts.
        fx.host(DiagTarget::ENGINE, |h| v.request_start(h));
        fx.host(DiagTarget::ENGINE, |h| v.tick(h));
        fx.confirms.answer("candump-replace", false);
        fx.host(DiagTarget::ENGINE, |h| v.tick(h));
        assert!(matches!(v.step, Step::Idle));
        assert_eq!(std::fs::read(&p).unwrap(), b"earlier capture");
        // Accepted: archived in the vault, then captured.
        fx.host(DiagTarget::ENGINE, |h| v.request_start(h));
        fx.host(DiagTarget::ENGINE, |h| v.tick(h));
        fx.confirms.answer("candump-replace", true);
        run(&mut v, &mut fx, |v| {
            matches!(v.step, Step::Running | Step::Idle)
        });
        assert!(matches!(v.step, Step::Running), "{:?}", v.msg);
        let vault = pl::vault_dir(&p);
        let kept = std::fs::read_dir(&vault)
            .unwrap()
            .filter_map(|e| std::fs::read(e.ok()?.path()).ok())
            .any(|d| d == b"earlier capture");
        assert!(kept, "the earlier capture is in the vault");
        fx.host(DiagTarget::ENGINE, |h| v.request_stop(h));
        run(&mut v, &mut fx, |v| matches!(v.step, Step::Idle));
    }

    #[test]
    fn the_backend_refuses_off_bench_and_without_a_file_nothing_starts() {
        let mut fx = Fixture::new("n7-cap-stable");
        fx.shared = HostShared::new(BuildChannel::new(BuildChannel::STABLE), None);
        fx.adapter = Some(mock(BuildChannel::STABLE));
        let mut v = CandumpView::default();
        fx.host(DiagTarget::ENGINE, |h| v.request_start(h));
        assert!(matches!(v.step, Step::Idle));
        assert!(v.msg.as_ref().unwrap().1.contains("output file"));
        v.path = Some(fx.dir.join("stable.log"));
        v.filter = "20000000".into();
        fx.host(DiagTarget::ENGINE, |h| v.request_start(h));
        assert!(v.msg.as_ref().unwrap().1.contains("29 bits"));
        v.filter.clear();
        fx.host(DiagTarget::ENGINE, |h| v.request_start(h));
        run(&mut v, &mut fx, |v| matches!(v.step, Step::Idle));
        let m = &v.msg.as_ref().unwrap().1;
        assert!(m.contains("Bench"), "{m}");
        assert!(!fx.dir.join("stable.log").exists());
    }
}
