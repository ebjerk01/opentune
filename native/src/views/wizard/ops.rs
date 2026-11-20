//! The wizard's adapter work: one short READ operation at a time on the
//! app's adapter (`AdapterAccess::read` / `scan`, the read-only allow-list
//! path), from a background job. Nothing here can change the car: no
//! `confirm_ecu`, no `mutate`, no `actuate`, no clearing of codes.
//!
//! | Step | Requests (in order) | Path |
//! |---|---|---|
//! | Connect | none (the cable is opened on the app target: adapter set-up only) | `read` |
//! | Identify | OBD `09 00`, `09 02` / `04` / `06` / `0A` (supported ones), then when no VIN: `09 02`, UDS `22 F190` | `read` (app target) |
//! | Scan | per OBD address `7E0..7E7`: `01 01`, `01 41`, `03`, `07`, `0A` (a silent address gets only the first); with a module list, Auto-scan's identity / `19 02 FF` reads per module | `read` per address; `scan` (re-binds per module) |
//! | Live check | `01 00` (+ further support ranges), then six mode-01 reads of the live PIDs | `read` per reading; with the adapter busy, the live feed instead (nothing sent) |

use super::model::{
    self, ConnectInfo, ConnectProbe, Identity, LiveData, LiveSource, ModuleScan, ObdEcu, ObdScan,
    Problem, Sample, ScanData,
};
use crate::host::{AdapterAccess, DiagTarget, FeedEvent, LiveFeed, ProgressSink};
use opentune_core::obd::{self, ObdSession, Sections, OBD_ECU_IDS};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Adapter operation name (shown in "adapter busy" messages).
pub const OP: &str = "Check my car";

const INFO: Sections = Sections {
    readiness: false,
    dtcs: false,
    freeze: false,
    info: true,
};
const SCAN: Sections = Sections {
    readiness: true,
    dtcs: true,
    freeze: false,
    info: false,
};

/// Cancellation and progress of a running step (a job's [`ProgressSink`], or
/// nothing in tests).
pub trait Ctl {
    fn stopped(&self) -> bool {
        false
    }
    fn progress(&self, _frac: f32, _msg: &str) {}
}

impl Ctl for ProgressSink {
    fn stopped(&self) -> bool {
        self.cancelled()
    }
    fn progress(&self, frac: f32, msg: &str) {
        self.set(frac, msg);
    }
}

/// No progress, never stopped (tests).
#[cfg(test)]
pub struct NoCtl;
#[cfg(test)]
impl Ctl for NoCtl {}

pub fn is_busy(e: &str) -> bool {
    e.contains("adapter busy")
}

fn battery_mv(a: &Arc<dyn AdapterAccess>) -> Option<u32> {
    a.link()
        .battery_v
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| (v * 1000.0).round() as u32)
}

/// The driver's display name from the registry.
pub fn driver_name(id: &str) -> String {
    opentune_hub::drivers::registry()
        .list()
        .iter()
        .find(|e| e.id == id)
        .map_or_else(|| id.to_string(), |e| e.display_name.to_string())
}

/// `7E0/7E8` for a CAN target, else its description.
pub fn target_text(t: DiagTarget) -> String {
    match t.transport {
        crate::host::Transport::IsoTp => format!("{:03X}/{:03X}", t.tx, t.rx),
        _ => t.to_string(),
    }
}

/// Connect: open the app's cable on the target (adapter set-up only, no
/// request goes to a control unit), then read the battery and capabilities
/// the adapter reports.
pub fn connect(a: &Arc<dyn AdapterAccess>, target: DiagTarget) -> Result<ConnectInfo, String> {
    let opened = a.read(OP, target, |_d| Ok(()));
    let link = a.link();
    model::connect_result(ConnectProbe {
        name: driver_name(&link.driver),
        driver: link.driver,
        port: link.port,
        opened,
        battery_mv: battery_mv(a),
        caps: a.capabilities(),
    })
}

/// Identify: VIN (mode 09, else the UDS `22 F190` fall-back), engine
/// computer name, calibration IDs and CVNs of the app's target.
pub fn identify(a: &Arc<dyn AdapterAccess>, target: DiagTarget) -> Result<Identity, String> {
    let (ins, fallback) = a.read(OP, target, |d| {
        let ins = obd::inspect(d, INFO, 0);
        let has_vin = ins.info.as_ref().is_some_and(|i| i.vin.is_some());
        let fallback = if has_vin { None } else { obd::read_vin(d) };
        Ok((ins, fallback))
    })?;
    let vin = ins
        .info
        .as_ref()
        .and_then(|i| i.vin.clone())
        .or_else(|| fallback.clone());
    let decoded = vin.as_deref().and_then(model::decode_vin);
    let id = model::identity(Some(&ins), Some(target_text(target)), fallback, decoded);
    if !id.responded && id.vin.is_none() {
        return Err(ins
            .errors
            .get("module")
            .or_else(|| ins.errors.values().next())
            .cloned()
            .unwrap_or_else(|| "no response: the engine computer did not answer".into()));
    }
    Ok(id)
}

pub const NO_LIST_NOTE: &str = "No module list was chosen, so only the OBD modules were asked.";

/// Scan: readiness, MIL and codes of every OBD address (one read each), then
/// with a module list the full module scan (Auto-scan, one `scan`).
pub fn scan(
    a: &Arc<dyn AdapterAccess>,
    modules: Option<PathBuf>,
    ctl: &dyn Ctl,
) -> Result<ScanData, String> {
    let mut problems = Vec::new();
    let mut obd_scan = ObdScan::default();
    let n = OBD_ECU_IDS.len() as f32 + if modules.is_some() { 2.0 } else { 0.0 };
    for (i, &(tx, rx)) in OBD_ECU_IDS.iter().enumerate() {
        if ctl.stopped() {
            return Err("cancelled".into());
        }
        ctl.progress(i as f32 / n, &format!("asking OBD address {tx:03X}"));
        match a.read(OP, DiagTarget::can(tx, rx), |d| {
            Ok(obd::inspect(d, SCAN, 0))
        }) {
            Ok(ins) if ins.responded => obd_scan.ecus.push(ObdEcu { tx, rx, ins }),
            Ok(_) => obd_scan.silent.push(format!("{tx:03X}")),
            // No link / busy: the rest of the OBD scan would fail the same way.
            Err(e) => {
                problems.push(Problem {
                    what: "OBD scan".into(),
                    info: model::error_info(&e),
                });
                break;
            }
        }
    }
    let mut modules_note = None;
    let mut mods = None;
    match modules {
        None => modules_note = Some(NO_LIST_NOTE.to_string()),
        Some(path) => {
            if ctl.stopped() {
                return Err("cancelled".into());
            }
            ctl.progress(OBD_ECU_IDS.len() as f32 / n, "scanning the module list");
            match crate::views::autoscan::load(&path).and_then(|list| {
                let vehicle = list.vehicle.clone();
                crate::views::autoscan::scan_op(a, list).map(|m| (vehicle, m))
            }) {
                Ok((vehicle, modules)) => mods = Some(ModuleScan { vehicle, modules }),
                Err(e) => problems.push(Problem {
                    what: "Module scan".into(),
                    info: model::error_info(&e),
                }),
            }
        }
    }
    let summary = model::scan_summary(Some(&obd_scan), mods.as_ref());
    if model::answered(&summary) == 0 {
        return Err(problems
            .first()
            .map(|p| p.info.raw.clone())
            .unwrap_or_else(|| {
                "no response: no module answered (OBD addresses 7E0–7E7 silent)".into()
            }));
    }
    Ok(ScanData {
        summary,
        problems,
        modules_note,
    })
}

/// Sleep `d` in short steps; false when stopped meanwhile.
fn nap(d: Duration, ctl: &dyn Ctl) -> bool {
    let until = Instant::now() + d;
    loop {
        if ctl.stopped() {
            return false;
        }
        let now = Instant::now();
        if now >= until {
            return true;
        }
        std::thread::sleep((until - now).min(Duration::from_millis(20)));
    }
}

/// Live check: the supported live PIDs, then [`model::LIVE_SAMPLES`] short
/// mode-01 reads `gap` apart (each its own read operation, so other panels
/// get the adapter in between). While another panel holds the adapter, the
/// readings come from the live feed it publishes instead (`feed`), so
/// nothing is sent at all.
pub fn live(
    a: &Arc<dyn AdapterAccess>,
    target: DiagTarget,
    feed: Option<&LiveFeed>,
    gap: Duration,
    ctl: &dyn Ctl,
) -> Result<LiveData, String> {
    // Subscribed first, so frames published while discovery waits count.
    let rx = feed.map(LiveFeed::subscribe);
    ctl.progress(0.0, "asking which values the engine computer reports");
    let disc = a.read(OP, target, |d| {
        let mut s = ObdSession::new();
        let sup = s.discover(d)?;
        Ok((s, sup))
    });
    let (mut session, sup) = match disc {
        Ok(x) => x,
        Err(e) if is_busy(&e) => match rx {
            Some(rx) => return live_from_feed(&rx, gap, ctl, e),
            None => return Err(e),
        },
        Err(e) => return Err(e),
    };
    drop(rx);
    let pids = model::live_pids(Some(&sup));
    if pids.is_empty() {
        return Err(model::NO_LIVE_VALUES.into());
    }
    let mut samples = Vec::new();
    let mut last = None;
    let mut stopped = false;
    for i in 0..model::LIVE_SAMPLES {
        if ctl.stopped() {
            stopped = true;
            break;
        }
        let (s, p) = (session.clone(), pids.clone());
        match a.read(OP, target, move |d| {
            let mut s = s;
            let ro = s.read(d, &p);
            Ok((s, ro))
        }) {
            Ok((s, ro)) => {
                session = s;
                samples.push(Sample::from_readout(&ro, battery_mv(a)));
            }
            Err(e) => last = Some(e),
        }
        ctl.progress(
            (i + 1) as f32 / model::LIVE_SAMPLES as f32,
            &format!("reading {} of {}", i + 1, model::LIVE_SAMPLES),
        );
        if i + 1 < model::LIVE_SAMPLES && !nap(gap, ctl) {
            stopped = true;
            break;
        }
    }
    if samples.is_empty() {
        return Err(last.unwrap_or_else(|| "no reading arrived".into()));
    }
    Ok(LiveData {
        pids,
        summary: model::live_summary(&samples),
        stopped,
        source: LiveSource::Reads,
    })
}

/// The live keys of one feed frame as a reading (`None` when it has none).
pub fn sample_from_feed(ev: &FeedEvent, battery_mv: Option<u32>) -> Option<(String, Sample)> {
    let FeedEvent::Frame { source, values, .. } = ev else {
        return None;
    };
    let values: std::collections::BTreeMap<String, Option<f64>> = values
        .iter()
        .filter(|v| model::LIVE_KEYS.contains(&v.key.as_str()))
        .map(|v| (v.key.clone(), v.value))
        .collect();
    (!values.is_empty()).then(|| {
        (
            source.clone(),
            Sample {
                values,
                errors: Default::default(),
                battery_mv,
            },
        )
    })
}

/// Up to [`model::LIVE_SAMPLES`] readings from the live feed, waiting at most
/// as long as the own reads would take (plus a second). `busy` is the
/// adapter's answer, returned when no frame arrives.
fn live_from_feed(
    rx: &Receiver<FeedEvent>,
    gap: Duration,
    ctl: &dyn Ctl,
    busy: String,
) -> Result<LiveData, String> {
    let until = Instant::now() + gap * model::LIVE_SAMPLES as u32 + Duration::from_secs(1);
    let mut samples = Vec::new();
    let mut source = String::new();
    let mut stopped = false;
    while samples.len() < model::LIVE_SAMPLES {
        if ctl.stopped() {
            stopped = true;
            break;
        }
        let now = Instant::now();
        if now >= until {
            break;
        }
        match rx.recv_timeout((until - now).min(Duration::from_millis(50))) {
            Ok(ev) => {
                if let Some((src, s)) = sample_from_feed(&ev, None) {
                    source = src;
                    samples.push(s);
                    ctl.progress(
                        samples.len() as f32 / model::LIVE_SAMPLES as f32,
                        &format!(
                            "reading {} of {} (live feed)",
                            samples.len(),
                            model::LIVE_SAMPLES
                        ),
                    );
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if samples.is_empty() {
        return Err(busy);
    }
    let pids = model::live_pids(None)
        .into_iter()
        .filter(|p| {
            model::LIVE_KEYS.iter().any(|k| {
                obd::channel(k).is_some_and(|c| c.pid == *p)
                    && samples.iter().any(|s| s.values.contains_key(*k))
            })
        })
        .collect();
    Ok(LiveData {
        pids,
        summary: model::live_summary(&samples),
        stopped,
        source: LiveSource::Feed(source),
    })
}
