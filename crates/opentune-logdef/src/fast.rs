//! **Fast logging** over UDS `0x2C` DynamicallyDefineDataIdentifier.
//!
//! [`poll_once`](crate::live::poll_once) sends one request per channel, so a
//! 20-channel log costs 20 round trips per sample. ISO 14229-1 §10.6 lets a
//! tester ask the ECU to *compose* a new data identifier — a dynamically
//! defined DID (DDDID) in `0xF200..=0xF3FF` — out of slices of existing DIDs
//! (`defineByIdentifier`, sub-function `0x01`) and/or memory ranges
//! (`defineByMemoryAddress`, `0x02`). One `0x22` read of the DDDID then
//! returns every channel's bytes concatenated, and the tester slices them back
//! apart. [`LiveSession`] does exactly that:
//!
//! 1. **Plan.** UDS-DID channels are probed once with `0x22` to learn their
//!    record length (so the slice is exactly what per-channel polling would
//!    decode); memory channels use their declared length. Channels are packed
//!    in order into records of at most [`FastLogConfig::max_record_len`]
//!    bytes, opening the next DDDID (`first_did`, `first_did + 1`, …) when one
//!    is full, up to [`FastLogConfig::max_dids`]. OBD-II PIDs, unknown
//!    addressing, failed probes and anything that does not fit stay
//!    **polled** individually alongside the dynamic reads.
//! 2. **Define.** Each planned DDDID is cleared (`2C 03`) and then defined:
//!    its identifier elements first (`2C 01`), then its memory elements
//!    (`2C 02`, `addressAndLengthFormatIdentifier` `0x14`), at most
//!    [`FastLogConfig::max_elements_per_request`] per request — ISO 14229-1
//!    appends successive defines of one DDDID in order.
//! 3. **Tick.** One `22 <DDDID>` per record; the response is checked for the
//!    planned length and sliced into per-channel values with the *same*
//!    decoding and conversion as polling.
//! 4. **Recover.** If a DDDID read comes back `requestOutOfRange` (0x31) or
//!    the wrong length, the definition was lost (session change, S3 timeout,
//!    ECU reset — ISO 14229-1 clears DDDIDs on session transitions): the
//!    session re-enters [`FastLogConfig::session`] if one is set, re-defines
//!    once and re-reads. If that fails it falls back to polling.
//! 5. **Stop.** [`LiveSession::stop`] clears every DDDID it defined.
//!
//! If the ECU refuses any part of the setup — `serviceNotSupported` (0x11),
//! `subFunctionNotSupported` (0x12), `requestOutOfRange` (0x31),
//! `conditionsNotCorrect` (0x22), `securityAccessDenied` (0x33) or anything
//! else — the session clears what it defined, switches to
//! [`LogMode::Polling`] and records why in [`LiveStatus::reason`]; logging
//! carries on, just slower. Dynamic DIDs outside `0xF200..=0xF3FF` are never
//! used ([`FastLogConfig::validate`]).
//!
//! The periodic transmission service `0x2A` (ReadDataByPeriodicIdentifier) is
//! out of scope: every read here is tester-initiated.

use crate::live::{
    blank_sample, decode_did_record, decode_memory, poll_param, selected_params, Sample, MEM_ALFID,
};
use crate::model::{Addressing, LogDef, LogParam};
use opentune_core::RawDiag;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::ops::RangeInclusive;

/// The ISO 14229-1 range reserved for dynamically defined data identifiers.
pub const DYNAMIC_DID_RANGE: RangeInclusive<u16> = 0xF200..=0xF3FF;

/// The first dynamic DID used unless configured otherwise.
pub const DEFAULT_DYNAMIC_DID: u16 = 0xF200;

/// UDS service ids used here.
const SID_SESSION: u8 = 0x10;
const SID_READ_DID: u8 = 0x22;
const SID_DDDID: u8 = 0x2C;

/// How a [`LiveSession`] is reading its channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogMode {
    /// Channels packed into dynamically defined DIDs (UDS `0x2C`).
    #[serde(rename = "fast-2c")]
    Fast2c,
    /// One request per channel (the fallback, and the default without
    /// [`FastLogConfig::enabled`]).
    #[serde(rename = "polling")]
    Polling,
    /// Subaru SSM2: batched `A8` address reads ([`FastLogConfig::ssm`]).
    #[serde(rename = "ssm-a8")]
    Ssm,
}

/// Fast-logging policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FastLogConfig {
    /// Try UDS `0x2C`. `false` = plain per-channel polling.
    pub enabled: bool,
    /// First dynamic DID to define (must lie in [`DYNAMIC_DID_RANGE`]).
    pub first_did: u16,
    /// How many consecutive dynamic DIDs (from `first_did`) may be used.
    pub max_dids: u16,
    /// Largest record (bytes) per dynamic DID. ECUs limit this; a define past
    /// the ECU's own limit is refused (0x31) and the session falls back, so a
    /// conservative value is the safer default.
    pub max_record_len: usize,
    /// Most source elements carried by one `0x2C` define request.
    pub max_elements_per_request: usize,
    /// Diagnostic session to (re-)enter with `0x10` before defining, e.g.
    /// `0x03` for an ECU that only accepts `0x2C` in the extended session.
    /// `None` leaves the session alone.
    pub session: Option<u8>,
    /// Read over Subaru SSM2 instead of UDS: every channel's byte addresses
    /// in batched `A8` requests ([`LogMode::Ssm`]). The transport must carry
    /// SSM payloads; the UDS settings above are then unused. Select the
    /// channels with [`crate::ssm::select`] first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssm: Option<crate::ssm::SsmPollConfig>,
}

impl Default for FastLogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            first_did: DEFAULT_DYNAMIC_DID,
            max_dids: 4,
            max_record_len: 32,
            max_elements_per_request: 8,
            session: None,
            ssm: None,
        }
    }
}

impl FastLogConfig {
    /// Fast mode on, everything else default.
    pub fn fast() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    /// Check the policy: every dynamic DID it may use lies inside
    /// [`DYNAMIC_DID_RANGE`], and the limits are non-zero.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(ssm) = &self.ssm {
            if self.enabled {
                return Err("UDS 0x2C fast mode and SSM are exclusive".into());
            }
            ssm.validate()?;
        }
        if !DYNAMIC_DID_RANGE.contains(&self.first_did) {
            return Err(format!(
                "dynamic DID 0x{:04X} is outside the ISO 14229-1 range 0xF200..=0xF3FF",
                self.first_did
            ));
        }
        if self.max_dids == 0 || self.max_record_len == 0 || self.max_elements_per_request == 0 {
            return Err("max_dids, max_record_len and max_elements_per_request must be ≥ 1".into());
        }
        let last = u32::from(self.first_did) + u32::from(self.max_dids) - 1;
        if last > u32::from(*DYNAMIC_DID_RANGE.end()) {
            return Err(format!(
                "0x{:04X} + {} dynamic DIDs runs past 0xF3FF",
                self.first_did, self.max_dids
            ));
        }
        Ok(())
    }
}

/// One dynamic DID as planned and defined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DynamicDidInfo {
    /// The dynamic DID.
    pub did: u16,
    /// Bytes in its record.
    pub record_len: usize,
    /// Channel ids packed into it, in record order.
    pub channels: Vec<String>,
}

/// What a [`LiveSession`] is doing and how well — the shape a UI shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveStatus {
    /// Current read mode.
    pub mode: LogMode,
    /// Why the session is polling (fast mode not requested, or what the ECU
    /// refused). `None` while fast mode is active.
    pub reason: Option<String>,
    /// The dynamic DIDs in use (empty while polling).
    pub dynamic_dids: Vec<DynamicDidInfo>,
    /// Channel ids read one request each, every tick.
    pub polled: Vec<String>,
    /// Per-channel caveats from planning (e.g. why a channel was not packed).
    pub notes: Vec<String>,
    /// How many times the dynamic DIDs had to be re-defined after the ECU
    /// lost them.
    pub redefines: u32,
    /// Ticks completed.
    pub ticks: u64,
    /// Requests spent on setup (probes, session, clear + define).
    pub setup_requests: u64,
    /// Requests spent inside ticks (reads, plus any re-define).
    pub tick_requests: u64,
}

impl LiveStatus {
    /// Mean requests per tick so far (0 before the first tick).
    pub fn requests_per_tick(&self) -> f64 {
        if self.ticks == 0 {
            0.0
        } else {
            self.tick_requests as f64 / self.ticks as f64
        }
    }
}

/// One source element of a planned dynamic DID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Element {
    /// `size` bytes from position 1 of `did`'s record.
    Did { did: u16, size: u8 },
    /// `size` bytes of memory at `address`.
    Memory { address: u32, size: u8 },
}

impl Element {
    fn size(self) -> usize {
        match self {
            Element::Did { size, .. } | Element::Memory { size, .. } => usize::from(size),
        }
    }
}

/// A channel's slice of a dynamic DID record.
#[derive(Debug, Clone, Copy)]
struct Slot {
    /// Index into [`LiveSession::params`].
    param: usize,
    element: Element,
    offset: usize,
}

/// A planned dynamic DID: its record layout in definition order.
#[derive(Debug, Clone)]
struct Planned {
    did: u16,
    slots: Vec<Slot>,
    record_len: usize,
}

/// A [`RawDiag`] wrapper that counts requests.
struct Counted<'a, D: RawDiag + ?Sized> {
    inner: &'a mut D,
    n: u64,
}

impl<D: RawDiag + ?Sized> RawDiag for Counted<'_, D> {
    fn request(&mut self, req: &[u8]) -> Result<Vec<u8>, String> {
        self.n += 1;
        self.inner.request(req)
    }
}

/// Outcome of reading one dynamic DID.
enum ReadOutcome {
    Record(Vec<u8>),
    /// The ECU no longer knows the definition (or answers a different shape).
    Lost(String),
    /// Any other failure, noted on the channels for this tick.
    Failed(String),
}

/// A live logging session over a fixed channel selection: fast (`0x2C`)
/// where the ECU allows it, per-channel polling otherwise.
///
/// Call [`start`](Self::start) once, [`tick`](Self::tick) per sample and
/// [`stop`](Self::stop) at the end (it clears the dynamic DIDs on the ECU).
pub struct LiveSession {
    params: Vec<LogParam>,
    cfg: FastLogConfig,
    plan: Vec<Planned>,
    /// Indices into `params` read one request each.
    polled: Vec<usize>,
    /// Whether dynamic DIDs are currently defined on the ECU (by us).
    defined: bool,
    status: LiveStatus,
    /// The SSM address layout, in [`LogMode::Ssm`].
    ssm: Option<crate::ssm::SsmPlan>,
}

impl LiveSession {
    /// A session over the selected channels of `def` (an empty `ids` selects
    /// every enabled parameter, exactly as [`poll_once`](crate::poll_once)).
    pub fn new(def: &LogDef, ids: &[&str], cfg: FastLogConfig) -> Result<Self, String> {
        cfg.validate()?;
        let params: Vec<LogParam> = selected_params(def, ids).into_iter().cloned().collect();
        let polled = (0..params.len()).collect();
        Ok(Self {
            params,
            cfg,
            plan: Vec::new(),
            polled,
            defined: false,
            status: LiveStatus {
                mode: LogMode::Polling,
                reason: Some("not started".into()),
                dynamic_dids: Vec::new(),
                polled: Vec::new(),
                notes: Vec::new(),
                redefines: 0,
                ticks: 0,
                setup_requests: 0,
                tick_requests: 0,
            },
            ssm: None,
        })
    }

    /// The channels this session samples, in output order.
    pub fn params(&self) -> &[LogParam] {
        &self.params
    }

    /// The current status.
    pub fn status(&self) -> &LiveStatus {
        &self.status
    }

    /// Plan and define the dynamic DIDs (when fast mode is enabled), falling
    /// back to polling with a reason if the ECU refuses.
    pub fn start(&mut self, diag: &mut dyn RawDiag) -> &LiveStatus {
        let mut c = Counted { inner: diag, n: 0 };
        self.status.notes.clear();
        if let Some(ssm) = &self.cfg.ssm {
            // SSM: no setup on the module, just the address layout.
            let plan = crate::ssm::plan(&self.params, ssm);
            self.status.notes.extend(plan.notes.iter().cloned());
            self.ssm = Some(plan);
            self.plan.clear();
            self.polled.clear();
            self.status.mode = LogMode::Ssm;
            self.status.reason = None;
        } else if !self.cfg.enabled {
            self.fall_back("fast mode not requested".into());
        } else {
            self.plan(&mut c);
            if self.plan.is_empty() {
                self.fall_back(
                    "no channel can be packed into a dynamic DID (only UDS DID and memory \
                     channels can)"
                        .into(),
                );
            } else {
                match self.define_all(&mut c) {
                    Ok(()) => {
                        self.defined = true;
                        self.status.mode = LogMode::Fast2c;
                        self.status.reason = None;
                    }
                    Err(why) => self.fall_back(why),
                }
            }
        }
        self.sync_status();
        self.status.setup_requests += c.n;
        &self.status
    }

    /// Read every channel once. Always yields one [`Sample`] per channel, in
    /// [`params`](Self::params) order.
    pub fn tick(&mut self, diag: &mut dyn RawDiag) -> Vec<Sample> {
        let mut c = Counted { inner: diag, n: 0 };
        if let Some(plan) = &self.ssm {
            let mut out = crate::ssm::tick(&mut c, plan, &self.params);
            crate::derived::fill(&self.params, &mut out);
            self.status.ticks += 1;
            self.status.tick_requests += c.n;
            return out;
        }
        let mut out: Vec<Option<Sample>> = vec![None; self.params.len()];

        if self.status.mode == LogMode::Fast2c {
            let mut i = 0;
            while i < self.plan.len() {
                let mut outcome = self.read_dynamic(&mut c, i);
                let lost = match &outcome {
                    ReadOutcome::Lost(why) => Some(why.clone()),
                    _ => None,
                };
                if let Some(why) = lost {
                    // The ECU dropped the definition: re-define once, re-read.
                    match self.define_all(&mut c) {
                        Ok(()) => {
                            self.status.redefines += 1;
                            outcome = self.read_dynamic(&mut c, i);
                            if let ReadOutcome::Lost(again) = &outcome {
                                let why = format!(
                                    "dynamic DID 0x{:04X} still unreadable after re-define: {again}",
                                    self.plan[i].did
                                );
                                self.fall_back_live(&mut c, why);
                                break;
                            }
                        }
                        Err(e) => {
                            let why =
                                format!("re-define after lost definition ({why}) failed: {e}");
                            self.fall_back_live(&mut c, why);
                            break;
                        }
                    }
                }
                let planned = &self.plan[i];
                match outcome {
                    ReadOutcome::Record(rec) => {
                        for slot in &planned.slots {
                            let p = &self.params[slot.param];
                            let data = &rec[slot.offset..slot.offset + slot.element.size()];
                            let mut s = blank_sample(p);
                            match (slot.element, &p.addressing) {
                                (Element::Memory { size, .. }, Addressing::Memory { bit, .. }) => {
                                    decode_memory(&mut s, p, data, size, *bit)
                                }
                                _ => decode_did_record(&mut s, p, data),
                            }
                            out[slot.param] = Some(s);
                        }
                    }
                    ReadOutcome::Failed(note) | ReadOutcome::Lost(note) => {
                        for slot in &planned.slots {
                            let mut s = blank_sample(&self.params[slot.param]);
                            s.note = note.clone();
                            out[slot.param] = Some(s);
                        }
                    }
                }
                i += 1;
            }
        }

        // Polled channels — plus, after a fallback mid-tick, every channel the
        // dynamic reads did not fill.
        for (idx, slot) in out.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(poll_param(&mut c, &self.params[idx]));
            }
        }

        self.status.ticks += 1;
        self.status.tick_requests += c.n;
        let mut out: Vec<Sample> = out.into_iter().flatten().collect();
        crate::derived::fill(&self.params, &mut out);
        out
    }

    /// Clear the dynamic DIDs this session defined (best effort) and end it.
    pub fn stop(&mut self, diag: &mut dyn RawDiag) -> &LiveStatus {
        let mut c = Counted { inner: diag, n: 0 };
        self.clear_defined(&mut c);
        self.status.setup_requests += c.n;
        &self.status
    }

    /// Run `rounds` ticks, writing the same CSV as
    /// [`poll_to_csv`](crate::poll_to_csv) (header + one row per channel per
    /// round). Call [`start`](Self::start) first and [`stop`](Self::stop)
    /// after.
    pub fn run_csv(
        &mut self,
        diag: &mut dyn RawDiag,
        rounds: usize,
        interval_ms: u64,
        out: &mut impl Write,
    ) -> std::io::Result<()> {
        self.run_csv_gps(diag, rounds, interval_ms, None, out)
    }

    /// [`run_csv`](Self::run_csv) with an optional GPS source: after each
    /// tick's channels, the round also gets the GPS channels (and the derived
    /// speed error) from [`GpsLogger::samples`](crate::gps::GpsLogger::samples),
    /// as more rows of the same round.
    pub fn run_csv_gps(
        &mut self,
        diag: &mut dyn RawDiag,
        rounds: usize,
        interval_ms: u64,
        mut gps: Option<&mut crate::gps::GpsLogger>,
        out: &mut impl Write,
    ) -> std::io::Result<()> {
        writeln!(out, "{}", crate::live::CSV_HEADER)?;
        for r in 0..rounds {
            let ts = crate::live::unix_ms();
            let samples = self.tick(diag);
            for s in &samples {
                crate::live::write_csv_row(out, r, ts, s)?;
            }
            if let Some(g) = gps.as_deref_mut() {
                for s in g.samples(&samples) {
                    crate::live::write_csv_row(out, r, ts, &s)?;
                }
            }
            if interval_ms > 0 && r + 1 < rounds {
                std::thread::sleep(std::time::Duration::from_millis(interval_ms));
            }
        }
        Ok(())
    }

    // ---- internals ----

    /// Switch to polling every channel, recording why.
    fn fall_back(&mut self, why: String) {
        self.plan.clear();
        self.polled = (0..self.params.len()).collect();
        self.status.mode = LogMode::Polling;
        self.status.reason = Some(why);
        self.sync_status();
    }

    /// Fall back mid-session: clear what is defined first.
    fn fall_back_live(&mut self, diag: &mut dyn RawDiag, why: String) {
        self.clear_defined(diag);
        self.fall_back(why);
    }

    /// Mirror the plan into the public status.
    fn sync_status(&mut self) {
        self.status.dynamic_dids = self
            .plan
            .iter()
            .map(|p| DynamicDidInfo {
                did: p.did,
                record_len: p.record_len,
                channels: p
                    .slots
                    .iter()
                    .map(|s| self.params[s.param].id.clone())
                    .collect(),
            })
            .collect();
        self.status.polled = self
            .polled
            .iter()
            .map(|&i| self.params[i].id.clone())
            .collect();
    }

    /// Decide which channels go into which dynamic DID.
    fn plan(&mut self, diag: &mut dyn RawDiag) {
        self.plan.clear();
        self.polled.clear();
        let max = self.cfg.max_record_len;
        // Pack in channel order: identifier sources before memory sources
        // inside each record, matching the define order.
        let mut bins: Vec<Vec<(usize, Element)>> = Vec::new();
        let mut bin_len = 0usize;
        for (idx, p) in self.params.iter().enumerate() {
            let element = match &p.addressing {
                Addressing::UdsDid { did } if DYNAMIC_DID_RANGE.contains(did) => {
                    self.status.notes.push(format!(
                        "{}: DID 0x{did:04X} is itself dynamic; polled",
                        p.id
                    ));
                    None
                }
                Addressing::UdsDid { did } => match probe_did(diag, *did) {
                    // Polling decodes at most the first 8 record bytes of a
                    // number; a text channel decodes the whole record.
                    Ok(len) => Some(Element::Did {
                        did: *did,
                        size: if p.value_kind.is_number() {
                            len.min(8)
                        } else {
                            len.min(255)
                        } as u8,
                    }),
                    Err(e) => {
                        self.status.notes.push(format!(
                            "{}: probe of DID 0x{did:04X} failed ({e}); polled",
                            p.id
                        ));
                        None
                    }
                },
                Addressing::Memory {
                    address, length, ..
                } if *length > 0 => Some(Element::Memory {
                    address: *address,
                    size: *length,
                }),
                _ => None,
            };
            let Some(element) = element else {
                // A calculated channel without an address is computed from the
                // others after each read, not polled.
                if !(p.addressing == Addressing::Unknown && crate::derived::is_calculated(p)) {
                    self.polled.push(idx);
                }
                continue;
            };
            if element.size() > max {
                self.status.notes.push(format!(
                    "{}: {} bytes exceed the {max}-byte record limit; polled",
                    p.id,
                    element.size()
                ));
                self.polled.push(idx);
                continue;
            }
            if bins.is_empty() || bin_len + element.size() > max {
                if bins.len() == usize::from(self.cfg.max_dids) {
                    self.status.notes.push(format!(
                        "{}: all {} dynamic DIDs are full; polled",
                        p.id, self.cfg.max_dids
                    ));
                    self.polled.push(idx);
                    continue;
                }
                bins.push(Vec::new());
                bin_len = 0;
            }
            bins.last_mut().expect("just pushed").push((idx, element));
            bin_len += element.size();
        }

        for (n, mut bin) in bins.into_iter().enumerate() {
            // Stable: identifier sources first, memory sources after.
            bin.sort_by_key(|(_, e)| matches!(e, Element::Memory { .. }));
            let mut offset = 0;
            let slots = bin
                .into_iter()
                .map(|(param, element)| {
                    let s = Slot {
                        param,
                        element,
                        offset,
                    };
                    offset += element.size();
                    s
                })
                .collect();
            self.plan.push(Planned {
                did: self.cfg.first_did + n as u16,
                slots,
                record_len: offset,
            });
        }
    }

    /// (Re-)enter the configured session, then clear + define every planned
    /// dynamic DID. On failure, clears what it defined and returns why.
    fn define_all(&mut self, diag: &mut dyn RawDiag) -> Result<(), String> {
        self.defined = false;
        if let Some(session) = self.cfg.session {
            let resp = diag.request(&[SID_SESSION, session])?;
            if resp.first() != Some(&(SID_SESSION + 0x40)) {
                return Err(format!(
                    "DiagnosticSessionControl 0x{session:02X} refused: {}",
                    describe(&resp)
                ));
            }
        }
        for (i, planned) in self.plan.iter().enumerate() {
            if let Err(e) = define_one(diag, planned, self.cfg.max_elements_per_request) {
                // Undo the ones already defined (and the partial one).
                for p in &self.plan[..=i] {
                    let _ = diag.request(&clear_request(p.did));
                }
                return Err(e);
            }
        }
        self.defined = true;
        Ok(())
    }

    /// Clear our dynamic DIDs on the ECU if they are defined.
    fn clear_defined(&mut self, diag: &mut dyn RawDiag) {
        if self.defined {
            for p in &self.plan {
                let _ = diag.request(&clear_request(p.did));
            }
            self.defined = false;
        }
    }

    /// Read planned dynamic DID `i` and check its shape.
    fn read_dynamic(&self, diag: &mut dyn RawDiag, i: usize) -> ReadOutcome {
        let p = &self.plan[i];
        let [hi, lo] = p.did.to_be_bytes();
        let resp = match diag.request(&[SID_READ_DID, hi, lo]) {
            Ok(r) => r,
            Err(e) => return ReadOutcome::Failed(e),
        };
        if resp.len() >= 3 && resp[0] == SID_READ_DID + 0x40 && resp[1] == hi && resp[2] == lo {
            let data = &resp[3..];
            if data.len() == p.record_len {
                ReadOutcome::Record(data.to_vec())
            } else {
                ReadOutcome::Lost(format!(
                    "dynamic DID 0x{:04X} returned {} bytes, planned {}",
                    p.did,
                    data.len(),
                    p.record_len
                ))
            }
        } else if resp.len() >= 3 && resp[0] == 0x7F && resp[2] == 0x31 {
            ReadOutcome::Lost(format!(
                "dynamic DID 0x{:04X} no longer defined: {}",
                p.did,
                describe(&resp)
            ))
        } else if resp.first() == Some(&0x7F) {
            ReadOutcome::Failed(format!("negative response {resp:02X?}"))
        } else {
            ReadOutcome::Failed(format!("unexpected response {resp:02X?}"))
        }
    }
}

/// Probe a DID's record length with one `0x22` read.
fn probe_did(diag: &mut dyn RawDiag, did: u16) -> Result<usize, String> {
    let [hi, lo] = did.to_be_bytes();
    let resp = diag.request(&[SID_READ_DID, hi, lo])?;
    if resp.len() >= 4 && resp[0] == SID_READ_DID + 0x40 && resp[1] == hi && resp[2] == lo {
        Ok(resp.len() - 3)
    } else {
        Err(describe(&resp))
    }
}

/// `2C 03 <DDDID>`: clear one dynamic DID.
fn clear_request(did: u16) -> [u8; 4] {
    let [hi, lo] = did.to_be_bytes();
    [SID_DDDID, 0x03, hi, lo]
}

/// Clear and define one planned dynamic DID.
fn define_one(diag: &mut dyn RawDiag, p: &Planned, per_request: usize) -> Result<(), String> {
    let [hi, lo] = p.did.to_be_bytes();
    expect_positive(
        diag,
        &clear_request(p.did),
        "clearDynamicallyDefinedDataIdentifier",
        p.did,
    )?;

    let dids: Vec<Element> = p
        .slots
        .iter()
        .map(|s| s.element)
        .filter(|e| matches!(e, Element::Did { .. }))
        .collect();
    let mems: Vec<Element> = p
        .slots
        .iter()
        .map(|s| s.element)
        .filter(|e| matches!(e, Element::Memory { .. }))
        .collect();

    for chunk in dids.chunks(per_request) {
        let mut req = vec![SID_DDDID, 0x01, hi, lo];
        for e in chunk {
            if let Element::Did { did, size } = *e {
                // sourceDataIdentifier, positionInSourceDataRecord (1-based),
                // memorySize.
                req.extend_from_slice(&did.to_be_bytes());
                req.extend_from_slice(&[0x01, size]);
            }
        }
        expect_positive(diag, &req, "defineByIdentifier", p.did)?;
    }
    for chunk in mems.chunks(per_request) {
        let mut req = vec![SID_DDDID, 0x02, hi, lo, MEM_ALFID];
        for e in chunk {
            if let Element::Memory { address, size } = *e {
                req.extend_from_slice(&address.to_be_bytes());
                req.push(size);
            }
        }
        expect_positive(diag, &req, "defineByMemoryAddress", p.did)?;
    }
    Ok(())
}

/// Send a `0x2C` request and require `6C <subFunction> …`.
fn expect_positive(diag: &mut dyn RawDiag, req: &[u8], what: &str, did: u16) -> Result<(), String> {
    let resp = diag
        .request(req)
        .map_err(|e| format!("0x2C {what} of 0x{did:04X} failed: {e}"))?;
    if resp.len() >= 2 && resp[0] == SID_DDDID + 0x40 && resp[1] == req[1] {
        Ok(())
    } else {
        Err(format!(
            "0x2C {what} of 0x{did:04X} refused: {}",
            describe(&resp)
        ))
    }
}

/// Human-readable form of a (usually negative) response.
fn describe(resp: &[u8]) -> String {
    if resp.len() >= 3 && resp[0] == 0x7F {
        let name = match resp[2] {
            0x10 => "generalReject",
            0x11 => "serviceNotSupported",
            0x12 => "subFunctionNotSupported",
            0x13 => "incorrectMessageLengthOrInvalidFormat",
            0x14 => "responseTooLong",
            0x22 => "conditionsNotCorrect",
            0x24 => "requestSequenceError",
            0x31 => "requestOutOfRange",
            0x33 => "securityAccessDenied",
            0x7E => "subFunctionNotSupportedInActiveSession",
            0x7F => "serviceNotSupportedInActiveSession",
            _ => "negative response",
        };
        format!("NRC 0x{:02X} ({name})", resp[2])
    } else {
        format!("unexpected response {resp:02X?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_rejects_dids_outside_the_dynamic_range() {
        assert!(FastLogConfig::fast().validate().is_ok());
        let bad = FastLogConfig {
            first_did: 0xF190,
            ..FastLogConfig::fast()
        };
        assert!(bad.validate().unwrap_err().contains("outside"));
        let runs_off = FastLogConfig {
            first_did: 0xF3FE,
            max_dids: 3,
            ..FastLogConfig::fast()
        };
        assert!(runs_off.validate().unwrap_err().contains("past 0xF3FF"));
        let last_ok = FastLogConfig {
            first_did: 0xF3FE,
            max_dids: 2,
            ..FastLogConfig::fast()
        };
        assert!(last_ok.validate().is_ok());
    }

    #[test]
    fn ssm_config_is_exclusive_with_uds_fast_mode() {
        let ssm = FastLogConfig {
            ssm: Some(crate::ssm::SsmPollConfig::default()),
            ..FastLogConfig::default()
        };
        assert!(ssm.validate().is_ok());
        let both = FastLogConfig {
            enabled: true,
            ..ssm.clone()
        };
        assert!(both.validate().unwrap_err().contains("exclusive"));
        let tiny = FastLogConfig {
            ssm: Some(crate::ssm::SsmPollConfig { max_packet_len: 8 }),
            ..FastLogConfig::default()
        };
        assert!(tiny.validate().is_err());
        assert_eq!(serde_json::to_string(&LogMode::Ssm).unwrap(), "\"ssm-a8\"");
    }

    #[test]
    fn mode_serializes_as_documented() {
        assert_eq!(
            serde_json::to_string(&LogMode::Fast2c).unwrap(),
            "\"fast-2c\""
        );
        assert_eq!(
            serde_json::to_string(&LogMode::Polling).unwrap(),
            "\"polling\""
        );
    }

    #[test]
    fn describe_names_common_nrcs() {
        assert_eq!(
            describe(&[0x7F, 0x2C, 0x31]),
            "NRC 0x31 (requestOutOfRange)"
        );
        assert!(describe(&[0x6C]).contains("unexpected"));
    }
}
