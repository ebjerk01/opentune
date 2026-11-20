//! Haldex panel operations (no UI): the OpenHaldex status read, its two
//! mode-change shapes, the Gen4 read and the offline CAN-log decode. Every
//! adapter access goes through the typed `AdapterAccess` wrappers; the
//! classification of each operation is the table in `views/haldex.rs`.

use super::super::coding::plumbing as pl;
use crate::host::{Ack, Actuation, AdapterAccess, BackupReceipt, DiagTarget, RawTransport};
use opentune_core::RawDiag;
use opentune_haldex::canlog::{parse_log, summarize, HaldexSummary};
use opentune_haldex::gen4::{
    read_gen4, tp20_refusal, Gen4ReadOptions, Gen4Report, TP20_LOGICAL_ADDRESS, TP20_TESTER_RX_ID,
};
use opentune_haldex::openhaldex::{
    LockStatus, Mode, OpenHaldexUds, Slip, DID_LOCK, DID_MODE, UDS_REQ_ID, UDS_RESP_ID,
};
use opentune_haldex::series::{build_series, series_csv, Series, MAX_POINTS, MAX_ROWS};
use opentune_haldex::Bus;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The OpenHaldex's diagnostic address (the Haldex physical pair 0x70F/0x779).
pub(crate) const OH: DiagTarget = DiagTarget::can(UDS_REQ_ID, UDS_RESP_ID);
/// The Haldex Gen4 controller: KWP2000 over a VW TP2.0 channel to 0x0A.
pub(crate) const GEN4: DiagTarget = DiagTarget::tp20(TP20_LOGICAL_ADDRESS);
/// The module name the confirmations show.
pub(crate) const OH_NAME: &str = "OpenHaldex";
/// Longest hold the panel offers.
pub(crate) const MAX_HOLD_S: u32 = 600;
/// How often a hold polls the unit's status.
const HOLD_POLL: Duration = Duration::from_millis(250);
/// Attempts the hand-back makes to set the previous mode again.
const RESTORE_ATTEMPTS: usize = 3;

/// The label the panel shows for a mode.
pub(crate) fn mode_label(m: Mode) -> &'static str {
    match m {
        Mode::Stock => "Stock (OEM behaviour)",
        Mode::Fwd => "FWD (no lock)",
        Mode::Split5050 => "50:50 (100 % lock)",
        Mode::Split6040 => "60:40 (40 % lock)",
        Mode::Split7525 => "75:25 (25 % lock)",
        Mode::Expert => "Expert (unit's own map)",
    }
}

/// The exact request a mode change sends (`2E FD A2 <mode>`).
pub(crate) fn mode_request_hex(mode: Mode) -> String {
    let [h, l] = DID_MODE.to_be_bytes();
    format!("2E {h:02X} {l:02X} {:02X}", mode.index())
}

// ---------------------------------------------------------------------------
// OpenHaldex: reads
// ---------------------------------------------------------------------------

/// Lock status (`22 FD A0`) and wheel slip (`22 FD A1`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OhStatus {
    pub lock: LockStatus,
    pub slip: Option<Slip>,
    pub slip_error: Option<String>,
}

/// Read the status (RDBI only). A unit that does not answer the slip DID
/// still returns its lock status.
pub(crate) fn read_status_on(d: &mut dyn RawDiag) -> Result<OhStatus, String> {
    let mut oh = OpenHaldexUds::new(d);
    let lock = oh
        .read_lock()
        .map_err(|e| format!("OpenHaldex lock status (22 FD A0): {e}"))?;
    let (slip, slip_error) = match oh.read_slip() {
        Ok(s) => (Some(s), None),
        Err(e) => (None, Some(e.to_string())),
    };
    Ok(OhStatus {
        lock,
        slip,
        slip_error,
    })
}

/// The status through the adapter (`read`: READ_SERVICES only).
pub(crate) fn status_op(a: &Arc<dyn AdapterAccess>) -> Result<OhStatus, String> {
    a.read("OpenHaldex status", OH, read_status_on)
}

/// The whole lock-status record (`22 FD A0`): what a mode change backs up
/// and announces as the mode to return to.
pub(crate) fn pre_read_op(a: &Arc<dyn AdapterAccess>) -> Result<Vec<u8>, String> {
    a.read("OpenHaldex pre-read", OH, |d| pl::read_did(d, DID_LOCK))
}

/// The mode a lock-status record names.
pub(crate) fn record_mode(rec: &[u8]) -> Result<Mode, String> {
    match rec {
        [_, _, m, ..] => Mode::from_index(*m)
            .ok_or_else(|| format!("the unit reports an unknown mode index {m}")),
        _ => Err(format!("0xFDA0 carries {} byte(s), need 3", rec.len())),
    }
}

fn check_ack(ack: &Ack) -> Result<(), String> {
    if ack.target() != Some(OH) {
        return Err(format!(
            "the confirmation does not name the OpenHaldex ({OH}); nothing was sent"
        ));
    }
    Ok(())
}

/// The unit still reports `prev` (re-read on the same link right before the
/// write). Anything else refuses the write: nothing has been sent but reads.
fn still_in(d: &mut dyn RawDiag, prev: Mode) -> Result<(), String> {
    let now = OpenHaldexUds::new(d)
        .read_lock()
        .map_err(|e| format!("OpenHaldex re-read (22 FD A0): {e}"))?;
    if now.mode != Some(prev) {
        return Err(format!(
            "the unit's mode changed since it was read (now index {}, expected {prev}); \
             nothing was written",
            now.mode_index
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// OpenHaldex: Set & keep (MUTATE)
// ---------------------------------------------------------------------------

/// Set the mode and leave it (the unit may keep it after a power cycle):
/// `mutate` with the vault backup of the lock-status record read before the
/// confirmation. Inside the write: re-read `22 FD A0` (still the backed-up
/// mode, else NOTHING is written), `2E FD A2 <mode>`, `22 FD A0` read-back
/// (must report the new mode).
pub(crate) fn set_keep_op(
    a: &Arc<dyn AdapterAccess>,
    ack: Ack,
    receipt: &BackupReceipt,
    record: Vec<u8>,
    target: Mode,
) -> Result<LockStatus, String> {
    check_ack(&ack)?;
    if receipt.len != record.len() || receipt.crc32 != crate::views::vault::crc32(&record) {
        return Err(
            "mode change refused: the backup receipt is not the backup of the unit's current \
             record; nothing was sent"
                .into(),
        );
    }
    let prev = record_mode(&record)?;
    a.mutate("OpenHaldex mode set", ack, receipt, move |d| {
        still_in(d, prev)?;
        OpenHaldexUds::new(d)
            .set_mode(target)
            .map_err(|e| format!("set OpenHaldex mode {target}: {e}"))
    })
}

// ---------------------------------------------------------------------------
// OpenHaldex: Hold (ACTUATE)
// ---------------------------------------------------------------------------

/// What a hold and the panel share: the stop flag, the panel's heartbeat,
/// whether the mode write went out, and the latest status.
pub(crate) struct HoldShared {
    stop: AtomicBool,
    epoch: Instant,
    /// ms since `epoch` of the panel's last frame.
    beat_ms: AtomicU64,
    /// The panel counts as left after this long without a frame.
    grace_ms: u64,
    /// Set just BEFORE `2E FD A2` is sent: from then on the hand-back must
    /// set the previous mode again (the write may have landed even if its
    /// reply was lost).
    written: AtomicBool,
    live: Mutex<Option<OhStatus>>,
}

impl HoldShared {
    pub(crate) fn new(grace: Duration) -> Arc<Self> {
        Arc::new(Self {
            stop: AtomicBool::new(false),
            epoch: Instant::now(),
            beat_ms: AtomicU64::new(0),
            grace_ms: grace.as_millis() as u64,
            written: AtomicBool::new(false),
            live: Mutex::new(None),
        })
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// The panel is on screen (called every frame it is drawn).
    pub(crate) fn beat(&self) {
        self.beat_ms.store(self.now_ms(), Ordering::SeqCst);
    }

    fn panel_left(&self) -> bool {
        self.now_ms()
            .saturating_sub(self.beat_ms.load(Ordering::SeqCst))
            > self.grace_ms
    }

    /// End the hold at its next poll (STOP, view dropped).
    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub(crate) fn written(&self) -> bool {
        self.written.load(Ordering::SeqCst)
    }

    pub(crate) fn live(&self) -> Option<OhStatus> {
        self.live.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Why a hold ended normally (every other ending is an error).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HoldEnd {
    /// STOP (or the panel was closed).
    Stopped,
    /// The chosen hold time ran out.
    TimeUp,
    /// No frame of the panel for the grace time (another view, a minimised
    /// window).
    PanelLeft,
}

#[derive(Clone, Debug)]
pub(crate) struct HoldReport {
    pub end: HoldEnd,
    pub held_ms: u64,
    pub polls: u32,
}

/// The run half of a hold: re-read (still `prev`, else nothing is written),
/// mark `written`, `2E FD A2 <target>` + read-back, then poll the status
/// every [`HOLD_POLL`] until STOP / the time / the panel leaves. A failed
/// poll (link loss) or the unit leaving `target` ends the hold with an error.
/// The hand-back ([`hold_restore`]) runs after it in every case.
pub(crate) fn hold_run(
    d: &mut dyn RawDiag,
    prev: Mode,
    target: Mode,
    hold: Duration,
    sh: &HoldShared,
) -> Result<HoldReport, String> {
    still_in(d, prev)?;
    sh.written.store(true, Ordering::SeqCst);
    let st = OpenHaldexUds::new(d)
        .set_mode(target)
        .map_err(|e| format!("set OpenHaldex mode {target}: {e}"))?;
    *sh.live.lock().unwrap_or_else(|e| e.into_inner()) = Some(OhStatus {
        lock: st,
        slip: None,
        slip_error: None,
    });
    let t0 = Instant::now();
    let mut polls = 0;
    loop {
        let mut slept = Duration::ZERO;
        while slept < HOLD_POLL {
            let end = if sh.stop.load(Ordering::SeqCst) {
                Some(HoldEnd::Stopped)
            } else if t0.elapsed() >= hold {
                Some(HoldEnd::TimeUp)
            } else if sh.panel_left() {
                Some(HoldEnd::PanelLeft)
            } else {
                None
            };
            if let Some(end) = end {
                return Ok(HoldReport {
                    end,
                    held_ms: t0.elapsed().as_millis() as u64,
                    polls,
                });
            }
            std::thread::sleep(Duration::from_millis(20));
            slept += Duration::from_millis(20);
        }
        let s = read_status_on(d).map_err(|e| format!("link lost while holding: {e}"))?;
        polls += 1;
        let left = s.lock.mode != Some(target);
        let idx = s.lock.mode_index;
        *sh.live.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
        if left {
            return Err(format!(
                "the unit left mode {target} by itself (now index {idx})"
            ));
        }
    }
}

/// The hand-back half of a hold: when the mode write went out, set `prev`
/// again (`2E FD A2 <prev>` + read-back), up to [`RESTORE_ATTEMPTS`] times.
/// When it did not (refused before the write), nothing is sent.
pub(crate) fn hold_restore(d: &mut dyn RawDiag, prev: Mode, sh: &HoldShared) -> Result<(), String> {
    if !sh.written() {
        return Ok(());
    }
    let mut last = String::new();
    for _ in 0..RESTORE_ATTEMPTS {
        match OpenHaldexUds::new(d).set_mode(prev) {
            Ok(_) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("set the OpenHaldex back to {prev}: {last}"))
}

/// Hold `target` while the panel stays open (at most `hold`), then hand the
/// unit back to `prev`: `actuate` on the confirmed module, so the hand-back
/// ALWAYS runs on the same link after the run returns, fails or panics.
pub(crate) fn hold_op(
    a: &Arc<dyn AdapterAccess>,
    ack: Ack,
    prev: Mode,
    target: Mode,
    hold: Duration,
    sh: Arc<HoldShared>,
) -> Result<Actuation<HoldReport>, String> {
    check_ack(&ack)?;
    if prev == target {
        return Err(format!(
            "the unit is already in mode {target}; nothing was sent"
        ));
    }
    let sh2 = sh.clone();
    a.actuate(
        "OpenHaldex mode hold",
        ack,
        move |d| hold_run(d, prev, target, hold, &sh),
        move |d: &mut dyn RawDiag| hold_restore(d, prev, &sh2),
    )
}

// ---------------------------------------------------------------------------
// Gen4 (read only)
// ---------------------------------------------------------------------------

/// Gen4 read options from the panel: groups (decimal, separated by commas or
/// spaces; empty = the default set) and whether to read the stored codes.
pub(crate) fn gen4_options(groups: &str, read_dtcs: bool) -> Result<Gen4ReadOptions, String> {
    let g = groups
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .map(|t| {
            t.parse::<u8>()
                .map_err(|_| format!("group '{t}': expected 0..=255"))
        })
        .collect::<Result<Vec<u8>, String>>()?;
    let mut o = Gen4ReadOptions {
        read_dtcs,
        ..Gen4ReadOptions::default()
    };
    if !g.is_empty() {
        o.groups = g;
    }
    Ok(o)
}

/// Why a real Gen4 read is refused with the connected `driver` (`None`: it
/// can open TP2.0).
pub(crate) fn gen4_refusal(driver: &str) -> Option<String> {
    tp20_refusal(driver).map(|why| {
        format!(
            "{why} In this panel: use the simulated unit to see the read, or capture the \
             drivetrain CAN (CAN capture) and decode it under CAN log."
        )
    })
}

/// The read-only Gen4 pass on the real unit: `read` on the TP2.0 target, so
/// only READ_SERVICES go out (`10 89`, `1A 9B`, `18 02 FF 00`, `21 <group>`).
pub(crate) fn gen4_read_op(
    a: &Arc<dyn AdapterAccess>,
    driver: &str,
    opts: Gen4ReadOptions,
) -> Result<Gen4Report, String> {
    if let Some(why) = gen4_refusal(driver) {
        return Err(why);
    }
    a.read("Haldex Gen4 read", GEN4, move |d| {
        let mut t = RawTransport::new(d);
        Ok(read_gen4(&mut t, &opts))
    })
}

/// The simulated Gen4 read: the real `VwTp20` channel and KWP2000 client
/// against the crate's synthetic Gen4 unit. No adapter, nothing on a bus.
pub(crate) fn gen4_sim(opts: &Gen4ReadOptions) -> Gen4Report {
    use opentune_core::protocol::vwtp20::VwTp20;
    use opentune_haldex::sim::{SimGen4, SimGen4Link};
    let link = SimGen4Link::new(SimGen4::default());
    let mut tp = VwTp20::new(Box::new(link), TP20_LOGICAL_ADDRESS, TP20_TESTER_RX_ID);
    let r = read_gen4(&mut tp, opts);
    tp.disconnect();
    r
}

// ---------------------------------------------------------------------------
// CAN-log decode (offline)
// ---------------------------------------------------------------------------

/// A decoded log: the crate's summary, chart series (decimated), the first
/// decoded frames and the full-resolution Live-log CSV.
pub(crate) struct Decoded {
    pub summary: HaldexSummary,
    pub series: Vec<Series>,
    /// `(seconds from the first frame, id, decoded frame as JSON)`.
    pub rows: Vec<(Option<f64>, u32, String)>,
    pub rows_total: usize,
    pub bad_lines: usize,
    pub csv: String,
}

/// Decode a candump / OpenTune CSV / SavvyCAN / ASC log text (offline).
pub(crate) fn decode_text(text: &str, bus: Bus) -> Decoded {
    let log = parse_log(text);
    let (summary, decoded) = summarize(&log, bus);
    let t0 = decoded.iter().find_map(|(f, _)| f.ts);
    let rows = decoded
        .iter()
        .take(MAX_ROWS)
        .map(|(f, d)| {
            (
                f.ts.zip(t0).map(|(a, b)| a - b),
                f.id,
                serde_json::to_string(d).unwrap_or_default(),
            )
        })
        .collect();
    Decoded {
        series: build_series(&decoded, MAX_POINTS),
        csv: series_csv(&build_series(&decoded, usize::MAX)),
        rows,
        rows_total: decoded.len(),
        bad_lines: log.bad_lines,
        summary,
    }
}

/// Read and decode a log file.
pub(crate) fn decode_file(path: &Path, bus: Bus) -> Result<Decoded, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(decode_text(&String::from_utf8_lossy(&bytes), bus))
}
