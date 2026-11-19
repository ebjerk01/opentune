//! Diagnostics upload: send the (redacted) diagnostics log to a developer.
//!
//! The public app ships **no endpoint and no token**: this is a capability.
//! An upload target comes from exactly two places:
//!
//! * an **embedder** (a plugin build) sets [`crate::DesktopConfig::diagnostics`]
//!   to a [`DiagUpload`] — endpoint, optional token, label and mode
//!   ([`DiagMode::Manual`] or [`DiagMode::Auto`]);
//! * the environment: `OPENTUNE_DIAG_ENDPOINT` (+ `OPENTUNE_DIAG_TOKEN`)
//!   gives **manual mode only** — the environment can never turn on
//!   automatic sending.
//!
//! `OPENTUNE_DIAG=off` turns every upload off, an embedder's automatic mode
//! included.
//!
//! **Manual mode** sends only when all three hold: an endpoint is configured,
//! the user turned on "Allow sending diagnostics" (persisted, off by
//! default), and the user clicked Send after a preview of the exact redacted
//! payload. After a crash, the next start only *offers* to send.
//!
//! **Auto mode** (embedder only) sends without a click: a crash report on
//! the next start, coalesced error reports, an optional periodic session
//! summary. It starts only after the one-time notice was shown, it has a
//! global off switch, and it is rate-limited (one send per 10 minutes, 20 a
//! day, failed sends count too) with an offline queue of at most 2 MiB that
//! is retried at the next allowed slot.
//!
//! A payload holds only diagnostics-log records, a session summary and the
//! user's note ([`payload`]), redacted as a whole
//! ([`opentune_core::redact`]) before the preview and before the send. The
//! token never appears in a log, preview, history entry, error or `Debug`
//! output ([`SecretString`]). See docs/DIAGNOSTICS.md.

pub mod payload;
pub mod store;
#[cfg(test)]
mod tests;
pub mod transport;

pub use reqwest::Url;
pub use transport::{Outcome, Sender};

use crate::diaglog::{self, Level, Record};
use crate::state::BuildChannel;
use opentune_core::redact::{self, RedactContext};
use payload::Trigger;
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use store::{HistoryEntry, QueueEntry, Store};

/// `OPENTUNE_DIAG=off` turns every upload off.
pub const ENV_SWITCH: &str = "OPENTUNE_DIAG";
/// A manual-mode endpoint from the environment.
pub const ENV_ENDPOINT: &str = "OPENTUNE_DIAG_ENDPOINT";
/// The bearer token for [`ENV_ENDPOINT`].
pub const ENV_TOKEN: &str = "OPENTUNE_DIAG_TOKEN";
/// Developer flag (alpha / bench channels only): allow plain HTTP and
/// certificates that do not verify.
pub const ENV_INSECURE: &str = "OPENTUNE_DIAG_INSECURE";

// ---------------------------------------------------------------------------
// Configuration (the embedder's half)
// ---------------------------------------------------------------------------

/// A secret (the upload token): its `Debug` and `Display` never show it.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(s: impl Into<String>) -> Self {
        SecretString(s.into())
    }

    /// The secret itself (only the HTTP client reads it).
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl From<String> for SecretString {
    fn from(s: String) -> Self {
        SecretString(s)
    }
}

impl From<&str> for SecretString {
    fn from(s: &str) -> Self {
        SecretString(s.to_string())
    }
}

/// When automatic mode sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoPolicy {
    /// Send a crash report at the next start when the previous session
    /// panicked or never painted its first frame.
    pub crash_on_next_start: bool,
    /// Send new errors of this session (coalesced into one report per slot).
    pub on_error: bool,
    /// Send a session summary every this many minutes, only when there are
    /// new events (`None` = never).
    pub periodic_min: Option<u32>,
}

impl Default for AutoPolicy {
    fn default() -> Self {
        AutoPolicy {
            crash_on_next_start: true,
            on_error: true,
            periodic_min: None,
        }
    }
}

/// How diagnostics are sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagMode {
    /// Only after the user allowed it and clicked Send on a preview.
    Manual,
    /// Without a click (embedder-compiled only; never from the environment).
    Auto(AutoPolicy),
}

/// An upload target (see [`crate::DesktopConfig::diagnostics`]).
#[derive(Debug, Clone)]
pub struct DiagUpload {
    /// HTTPS endpoint the payload is POSTed to.
    pub endpoint: Url,
    /// Sent as `Authorization: Bearer <token>`.
    pub token: Option<SecretString>,
    /// A short name sent with every payload (e.g. the build's name).
    pub label: Option<String>,
    pub mode: DiagMode,
}

/// The upload-related environment variables.
#[derive(Clone, Default)]
pub struct EnvVars {
    pub switch: Option<String>,
    pub endpoint: Option<String>,
    pub token: Option<String>,
    pub insecure: Option<String>,
}

impl fmt::Debug for EnvVars {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnvVars")
            .field("switch", &self.switch)
            .field("endpoint", &self.endpoint.as_ref().map(|_| "<set>"))
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("insecure", &self.insecure)
            .finish()
    }
}

impl EnvVars {
    pub fn from_process() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        EnvVars {
            switch: get(ENV_SWITCH),
            endpoint: get(ENV_ENDPOINT),
            token: get(ENV_TOKEN),
            insecure: get(ENV_INSECURE),
        }
    }

    /// `OPENTUNE_DIAG=off` (or `0`, `false`, `no`, `disabled`).
    pub fn off(&self) -> bool {
        self.switch.as_deref().is_some_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "off" | "0" | "false" | "no" | "none" | "disable" | "disabled"
            )
        })
    }
}

/// Where the active target came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Embedder,
    Env,
}

/// The effective upload configuration of this process.
#[derive(Debug, Clone, Default)]
pub struct Resolved {
    /// The target, when uploads are possible at all.
    pub upload: Option<DiagUpload>,
    pub source: Option<Source>,
    /// `OPENTUNE_DIAG=off` is set.
    pub env_off: bool,
    /// The developer flag is on (HTTP / unverified certificates allowed).
    pub insecure: bool,
    /// Why something configured is not used (shown in the status).
    pub notes: Vec<String>,
}

/// Decide the effective configuration from the embedder's setting and the
/// environment. The environment yields manual mode only; `OPENTUNE_DIAG=off`
/// beats everything.
pub fn resolve(embedder: Option<DiagUpload>, env: &EnvVars, channel: &BuildChannel) -> Resolved {
    let mut r = Resolved::default();
    if env.off() {
        r.env_off = true;
        r.notes
            .push(format!("diagnostics upload turned off by {ENV_SWITCH}=off"));
        return r;
    }
    r.insecure = env.insecure.as_deref() == Some("1") && channel.debug_enabled();
    if env.insecure.is_some() && !r.insecure {
        r.notes.push(format!(
            "{ENV_INSECURE} is honoured on alpha / bench channels only (ignored)"
        ));
    }
    let (cand, source) = match embedder {
        Some(e) => {
            if env.endpoint.is_some() {
                r.notes.push(format!(
                    "{ENV_ENDPOINT} ignored: this build configures its own diagnostics upload"
                ));
            }
            (e, Source::Embedder)
        }
        None => {
            let Some(ep) = env.endpoint.as_deref() else {
                return r;
            };
            match Url::parse(ep.trim()) {
                Ok(endpoint) => (
                    DiagUpload {
                        endpoint,
                        token: env.token.clone().map(SecretString::new),
                        label: None,
                        // The environment can only ever give manual mode.
                        mode: DiagMode::Manual,
                    },
                    Source::Env,
                ),
                Err(_) => {
                    r.notes
                        .push(format!("{ENV_ENDPOINT} is not a valid URL (ignored)"));
                    return r;
                }
            }
        }
    };
    match transport::check_endpoint(&cand.endpoint, r.insecure) {
        Ok(()) => {
            r.upload = Some(cand);
            r.source = Some(source);
        }
        Err(e) => r.notes.push(format!("{e} (diagnostics upload off)")),
    }
    r
}

/// Why a manual send is refused (`Ok` = it may go out after the click).
pub fn manual_gate(resolved: &Resolved, allow_send: bool) -> Result<(), String> {
    if resolved.env_off {
        return Err(format!("diagnostics upload is off ({ENV_SWITCH}=off)"));
    }
    if resolved.upload.is_none() {
        return Err("no diagnostics upload is configured in this build".into());
    }
    if !allow_send {
        return Err(
            "sending diagnostics is not allowed: turn on \"Allow sending diagnostics\" first"
                .into(),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

/// A built payload waiting for the user's click.
struct Preview {
    id: String,
    trigger: Trigger,
    body: Vec<u8>,
    crash_ts: Option<String>,
}

/// The upload state of one process (managed by Tauri; tests build their own).
pub struct Engine {
    resolved: Resolved,
    sender: Option<Arc<dyn Sender>>,
    store: Mutex<Store>,
    log_dir: Option<PathBuf>,
    ctx: RedactContext,
    version: String,
    channel: String,
    started: u64,
    /// The crashed previous session: header time and reason.
    crash: Option<(String, &'static str)>,
    panels: Mutex<BTreeMap<String, u32>>,
    previews: Mutex<VecDeque<Preview>>,
    /// Records after this time are new for the error trigger ...
    error_mark: Mutex<String>,
    /// ... and for the periodic summary (with the panel counts it saw).
    summary_mark: Mutex<(String, u64, BTreeMap<String, u32>)>,
    busy: AtomicBool,
    seq: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn ts_at(secs: u64) -> String {
    diaglog::ts_from(UNIX_EPOCH + Duration::from_secs(secs))
}

/// Unix seconds now.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// What [`Engine::new`] needs besides the resolved configuration.
pub struct EngineParts {
    /// `<app data dir>/diagnostics` (settings, history, queue).
    pub store_dir: Option<PathBuf>,
    /// The diagnostics log directory.
    pub log_dir: Option<PathBuf>,
    /// This machine's home, user and host names (redacted from payloads).
    pub ctx: RedactContext,
    pub version: String,
    pub channel: String,
    /// The transport; `None` builds the HTTPS client from the configuration.
    pub sender: Option<Arc<dyn Sender>>,
    pub now: u64,
}

impl Engine {
    pub fn new(resolved: Resolved, parts: EngineParts) -> Engine {
        let mut resolved = resolved;
        let mut ctx = parts.ctx;
        let mut sender = parts.sender;
        if let Some(u) = &resolved.upload {
            // The token and the endpoint are scrubbed from everything shown
            // or sent, in case an error message repeats them.
            ctx.secrets.push(u.endpoint.as_str().to_string());
            if let Some(t) = &u.token {
                ctx.secrets.push(t.expose().to_string());
            }
            if sender.is_none() {
                match transport::HttpsSender::new(
                    u.endpoint.clone(),
                    u.token.clone(),
                    resolved.insecure,
                    &parts.version,
                ) {
                    Ok(s) => sender = Some(Arc::new(s)),
                    Err(e) => {
                        resolved.notes.push(e);
                        resolved.upload = None;
                    }
                }
            }
        }
        let store = Store::open(parts.store_dir);
        let all = parts
            .log_dir
            .as_deref()
            .map(diaglog::read_all)
            .unwrap_or_default();
        let crash = payload::previous_crash(&all)
            .filter(|(ts, _, _)| store.crash_done() != Some(ts.as_str()))
            .map(|(ts, why, _)| (ts, why));
        // This session's records are new for the triggers from its start.
        let session_start = diaglog::sessions(&all)
            .last()
            .and_then(|s| s.first())
            .filter(|r| r.is_session_header())
            .map(|r| r.ts.clone())
            .unwrap_or_else(|| ts_at(parts.now));
        Engine {
            resolved,
            sender,
            store: Mutex::new(store),
            log_dir: parts.log_dir,
            ctx,
            version: parts.version,
            channel: parts.channel,
            started: parts.now,
            crash,
            panels: Mutex::new(BTreeMap::new()),
            previews: Mutex::new(VecDeque::new()),
            error_mark: Mutex::new(session_start.clone()),
            summary_mark: Mutex::new((session_start, parts.now, BTreeMap::new())),
            busy: AtomicBool::new(false),
            seq: AtomicU64::new(0),
        }
    }

    pub fn resolved(&self) -> &Resolved {
        &self.resolved
    }

    fn host(&self) -> Option<String> {
        self.resolved
            .upload
            .as_ref()
            .and_then(|u| u.endpoint.host_str().map(str::to_string))
    }

    /// The automatic policy in force: an embedder's Auto mode, not switched
    /// off by the user or by `OPENTUNE_DIAG=off`.
    pub fn auto_policy(&self) -> Option<AutoPolicy> {
        let u = self.resolved.upload.as_ref()?;
        let DiagMode::Auto(p) = u.mode else {
            return None;
        };
        if self.resolved.env_off
            || self.resolved.source != Some(Source::Embedder)
            || lock(&self.store).settings().auto_off
        {
            return None;
        }
        Some(p)
    }

    fn is_auto_mode(&self) -> bool {
        matches!(
            self.resolved.upload.as_ref().map(|u| u.mode),
            Some(DiagMode::Auto(_))
        )
    }

    fn new_id(&self, now: u64) -> String {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        format!("{now}-{}-{n}", std::process::id())
    }

    fn all_records(&self) -> Vec<Record> {
        self.log_dir
            .as_deref()
            .map(diaglog::read_all)
            .unwrap_or_default()
    }

    /// The current session (the last one in the log) of `all`.
    fn current(all: &[Record]) -> &[Record] {
        diaglog::sessions(all).last().copied().unwrap_or(&[])
    }

    fn summary_now(&self, all: &[Record], now: u64) -> Value {
        let panels = lock(&self.panels).clone();
        payload::summary(
            Self::current(all),
            all,
            &panels,
            now.saturating_sub(self.started),
        )
    }

    fn build(
        &self,
        trigger: Trigger,
        note: Option<&str>,
        now: u64,
    ) -> Result<(Vec<u8>, Option<String>), String> {
        let all = self.all_records();
        let sessions = diaglog::sessions(&all);
        let current = sessions.last().copied().unwrap_or(&[]);
        let mut crash_ts = None;
        let mut header = current.first().filter(|r| r.is_session_header());
        let label = self
            .resolved
            .upload
            .as_ref()
            .and_then(|u| u.label.as_deref());
        let records: Vec<Record> = match trigger {
            Trigger::Manual => {
                let from = sessions.len().saturating_sub(2);
                let two: Vec<Record> = sessions[from..].iter().flat_map(|s| s.to_vec()).collect();
                payload::newest(&two, payload::MAX_RECORDS).to_vec()
            }
            Trigger::Crash => {
                let (ts, _, recs) =
                    payload::previous_crash(&all).ok_or("the previous session did not crash")?;
                crash_ts = Some(ts);
                header = recs.first();
                payload::newest(recs, payload::MAX_RECORDS).to_vec()
            }
            Trigger::Error => {
                let mark = lock(&self.error_mark).clone();
                let new: Vec<Record> = current.iter().filter(|r| r.ts > mark).cloned().collect();
                payload::newest(&new, payload::MAX_ERROR_RECORDS).to_vec()
            }
            Trigger::Periodic => Vec::new(),
        };
        let reason = self
            .crash
            .as_ref()
            .filter(|(ts, _)| Some(ts) == crash_ts.as_ref())
            .map(|(_, why)| *why)
            .unwrap_or("crash");
        let parts = payload::Parts {
            trigger,
            created: ts_at(now),
            label,
            app: payload::app_info(header, &self.version, &self.channel),
            summary: self.summary_now(&all, now),
            crash: crash_ts.as_deref().map(|ts| (ts, reason)),
            note,
            records: &records,
        };
        Ok((payload::build(&parts, &self.ctx)?, crash_ts))
    }

    // ---- status and settings ------------------------------------------------

    /// Everything the UI shows (no token, no full endpoint).
    pub fn status(&self, now: u64) -> Value {
        let st = lock(&self.store);
        let s = st.settings().clone();
        let mode = match self.resolved.upload.as_ref().map(|u| u.mode) {
            None => "none",
            Some(DiagMode::Manual) => "manual",
            Some(DiagMode::Auto(_)) => "auto",
        };
        let host = self.host();
        let policy = self.resolved.upload.as_ref().and_then(|u| match u.mode {
            DiagMode::Auto(p) => Some(json!({
                "crash_on_next_start": p.crash_on_next_start,
                "on_error": p.on_error,
                "periodic_min": p.periodic_min,
            })),
            DiagMode::Manual => None,
        });
        let auto_active = self.is_auto_mode() && !s.auto_off && !self.resolved.env_off;
        let crash_offer = self.crash_offer(&st);
        let gap = if auto_active { store::MIN_GAP_S } else { 0 };
        json!({
            "configured": self.resolved.upload.is_some(),
            "mode": mode,
            "source": self.resolved.source.map(|s| match s { Source::Embedder => "embedder", Source::Env => "env" }),
            "host": host,
            "label": self.resolved.upload.as_ref().and_then(|u| u.label.clone()),
            "policy": policy,
            "env_off": self.resolved.env_off,
            "insecure": self.resolved.insecure,
            "allow_send": s.allow_send,
            "auto_off": s.auto_off,
            "auto_active": auto_active,
            "notice_needed": self.is_auto_mode() && !self.resolved.env_off && s.notice_seen.as_deref() != host.as_deref(),
            "crash_offer": crash_offer.map(|(ts, why)| json!({ "session": ts, "reason": why })),
            "history": st.history(),
            "queue": { "items": st.queue().len(), "bytes": st.queue_bytes(), "cap": store::QUEUE_CAP },
            "next_slot": store::next_slot(st.sends(), now, gap).map(ts_at),
            "limits": { "min_gap_s": store::MIN_GAP_S, "per_day": store::PER_DAY, "max_payload": payload::MAX_PAYLOAD },
            "notes": self.resolved.notes,
        })
    }

    /// The crash the user may be offered to send (manual mode only).
    fn crash_offer(&self, st: &Store) -> Option<(String, &'static str)> {
        let (ts, why) = self.crash.clone()?;
        let manual = matches!(
            self.resolved.upload.as_ref().map(|u| u.mode),
            Some(DiagMode::Manual)
        );
        (manual
            && st.crash_done() != Some(ts.as_str())
            && st.settings().crash_dismissed.as_deref() != Some(ts.as_str()))
        .then_some((ts, why))
    }

    /// Change settings: "Allow sending diagnostics", the auto-mode off
    /// switch, the notice seen, the crash offer dismissed.
    pub fn set(
        &self,
        allow_send: Option<bool>,
        auto_off: Option<bool>,
        notice_seen: Option<bool>,
        dismiss_crash: Option<bool>,
    ) -> Result<(), String> {
        let host = self.host();
        let crash = self.crash.as_ref().map(|(ts, _)| ts.clone());
        lock(&self.store).update_settings(|s| {
            if let Some(v) = allow_send {
                s.allow_send = v;
            }
            if let Some(v) = auto_off {
                s.auto_off = v;
            }
            if notice_seen == Some(true) {
                s.notice_seen = host;
            }
            if dismiss_crash == Some(true) {
                s.crash_dismissed = crash;
            }
        })
    }

    /// One panel was opened (counted for the session summary).
    pub fn panel_opened(&self, panel: &str) {
        let name: String = panel
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
            .take(40)
            .collect();
        if name.is_empty() {
            return;
        }
        let mut p = lock(&self.panels);
        if p.len() < 64 || p.contains_key(&name) {
            *p.entry(name).or_insert(0) += 1;
        }
    }

    // ---- manual mode ---------------------------------------------------------

    /// Build the payload a manual send would POST (gated), keep it for
    /// [`Engine::send_preview`] and return its exact text.
    pub fn preview(&self, note: Option<&str>, crash: bool, now: u64) -> Result<Value, String> {
        manual_gate(&self.resolved, lock(&self.store).settings().allow_send)?;
        let trigger = if crash {
            Trigger::Crash
        } else {
            Trigger::Manual
        };
        let (body, crash_ts) = self.build(trigger, note, now)?;
        let id = self.new_id(now);
        let text = String::from_utf8_lossy(&body).into_owned();
        let bytes = body.len();
        let mut q = lock(&self.previews);
        q.push_back(Preview {
            id: id.clone(),
            trigger,
            body,
            crash_ts,
        });
        while q.len() > 4 {
            q.pop_front();
        }
        Ok(
            json!({ "id": id, "trigger": trigger.as_str(), "bytes": bytes, "text": text, "host": self.host() }),
        )
    }

    /// Send exactly the previewed payload `id` (the user's click). Gated
    /// again; at most [`store::PER_DAY`] sends a day.
    pub async fn send_preview(&self, id: &str, now: u64) -> Result<HistoryEntry, String> {
        manual_gate(&self.resolved, lock(&self.store).settings().allow_send)?;
        let p = {
            let mut q = lock(&self.previews);
            let i = q
                .iter()
                .position(|p| p.id == id)
                .ok_or("that preview is no longer available: preview again")?;
            q.remove(i).ok_or("preview gone")?
        };
        if let Some(t) = store::next_slot(lock(&self.store).sends(), now, 0) {
            return Err(format!(
                "the daily limit of {} uploads is reached; next possible at {}",
                store::PER_DAY,
                ts_at(t)
            ));
        }
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err("another upload is running".into());
        }
        let out = self.transmit(&p.body).await;
        self.busy.store(false, Ordering::Release);
        let mut st = lock(&self.store);
        st.note_send(now);
        if let (Outcome::Sent(_), Some(ts)) = (&out, &p.crash_ts) {
            st.set_crash_done(ts);
        }
        let e = self.entry(
            &p.id,
            p.trigger,
            p.body.len(),
            status_text(&out, false),
            now,
        );
        st.add_history(e.clone(), &p.body);
        match out {
            Outcome::Sent(_) => Ok(e),
            _ => Err(format!("upload {}", e.status)),
        }
    }

    // ---- auto mode ---------------------------------------------------------

    async fn transmit(&self, body: &[u8]) -> Outcome {
        let Some(s) = &self.sender else {
            return Outcome::Failed("no diagnostics upload is configured".into());
        };
        match s.post(body).await {
            Outcome::Failed(e) => Outcome::Failed(redact::redact(&e, &self.ctx)),
            o => o,
        }
    }

    fn entry(&self, id: &str, t: Trigger, bytes: usize, status: String, now: u64) -> HistoryEntry {
        HistoryEntry {
            id: id.to_string(),
            ts: ts_at(now),
            trigger: t.as_str().to_string(),
            bytes,
            status: redact::redact(&status, &self.ctx),
        }
    }

    /// One step of automatic mode (called about once a minute): at most one
    /// send, in this order — the crash report, the oldest queued payload,
    /// new errors (all of them in one report), the periodic summary. Returns
    /// the history entry of what happened, if anything.
    pub async fn tick(&self, now: u64) -> Option<HistoryEntry> {
        let policy = self.auto_policy()?;
        let host = self.host();
        if lock(&self.store).settings().notice_seen.as_deref() != host.as_deref() {
            return None; // the user has not seen the notice yet
        }
        if self.busy.swap(true, Ordering::AcqRel) {
            return None;
        }
        let r = self.tick_inner(policy, now).await;
        self.busy.store(false, Ordering::Release);
        r
    }

    async fn tick_inner(&self, policy: AutoPolicy, now: u64) -> Option<HistoryEntry> {
        let slot_free =
            store::next_slot(lock(&self.store).sends(), now, store::MIN_GAP_S).is_none();
        // 1. The crash of the previous session: sent now, or queued for the
        //    next slot (once).
        if let Some((ts, _)) = self.crash.clone().filter(|_| policy.crash_on_next_start) {
            if lock(&self.store).crash_done() != Some(ts.as_str()) {
                lock(&self.store).set_crash_done(&ts);
                if let Ok((body, _)) = self.build(Trigger::Crash, None, now) {
                    return Some(
                        self.send_or_queue(Trigger::Crash, body, slot_free, now)
                            .await,
                    );
                }
            }
        }
        if !slot_free {
            return None;
        }
        // 2. The offline queue, oldest first.
        let front = {
            let mut st = lock(&self.store);
            st.drop_broken_front();
            st.queue_front()
        };
        if let Some((q, body)) = front {
            let out = self.transmit(&body).await;
            let mut st = lock(&self.store);
            st.note_send(now);
            match out {
                Outcome::Failed(_) => st.queue_tried(&q.id),
                _ => st.dequeue(&q.id),
            }
            let t = match q.trigger.as_str() {
                "crash" => Trigger::Crash,
                "periodic" => Trigger::Periodic,
                "manual" => Trigger::Manual,
                _ => Trigger::Error,
            };
            let e = self.entry(&q.id, t, body.len(), status_text(&out, true), now);
            st.add_history(e.clone(), &body);
            return Some(e);
        }
        // 3. New errors, coalesced: everything since the last report.
        if policy.on_error {
            let all = self.all_records();
            let mark = lock(&self.error_mark).clone();
            let current = Self::current(&all);
            let new_errors = current
                .iter()
                .any(|r| r.level == Level::Error && r.ts > mark);
            if new_errors {
                let last = current.last().map(|r| r.ts.clone()).unwrap_or(mark);
                let built = self.build(Trigger::Error, None, now);
                *lock(&self.error_mark) = last;
                if let Ok((body, _)) = built {
                    return Some(self.send_or_queue(Trigger::Error, body, true, now).await);
                }
            }
        }
        // 4. The periodic summary, only when something new happened.
        if let Some(min) = policy.periodic_min.filter(|m| *m > 0) {
            let (mark, at, seen) = lock(&self.summary_mark).clone();
            if now >= at + u64::from(min) * 60 {
                let all = self.all_records();
                let current = Self::current(&all);
                let panels = lock(&self.panels).clone();
                let events = current
                    .iter()
                    .any(|r| r.level >= Level::Warn && r.ts > mark)
                    || panels != seen;
                let last = current.last().map(|r| r.ts.clone()).unwrap_or(mark);
                *lock(&self.summary_mark) = (last, now, panels);
                if events {
                    if let Ok((body, _)) = self.build(Trigger::Periodic, None, now) {
                        return Some(self.send_or_queue(Trigger::Periodic, body, true, now).await);
                    }
                }
            }
        }
        None
    }

    /// Send `body` now (when `slot_free`), queue it when that fails for a
    /// retryable reason or when there is no free slot.
    async fn send_or_queue(
        &self,
        t: Trigger,
        body: Vec<u8>,
        slot_free: bool,
        now: u64,
    ) -> HistoryEntry {
        let id = self.new_id(now);
        let out = if slot_free {
            let o = self.transmit(&body).await;
            lock(&self.store).note_send(now);
            Some(o)
        } else {
            None
        };
        let mut st = lock(&self.store);
        let status = match out {
            Some(o @ (Outcome::Sent(_) | Outcome::Rejected(_))) => status_text(&o, false),
            other => {
                let why = match other {
                    Some(Outcome::Failed(e)) => format!("queued: {e}"),
                    _ => "queued: waiting for the next allowed slot".to_string(),
                };
                let dropped = st.enqueue(
                    QueueEntry {
                        id: id.clone(),
                        trigger: t.as_str().into(),
                        created: ts_at(now),
                        bytes: body.len(),
                        tries: u32::from(slot_free),
                    },
                    &body,
                );
                if dropped.contains(&id) {
                    "dropped: the offline queue cannot hold it".to_string()
                } else {
                    why
                }
            }
        };
        let e = self.entry(&id, t, body.len(), status, now);
        st.add_history(e.clone(), &body);
        e
    }

    /// The exact payload of a history entry.
    pub fn history_payload(&self, id: &str) -> Result<String, String> {
        lock(&self.store)
            .history_payload(id)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .ok_or_else(|| "that payload is no longer kept".into())
    }
}

fn status_text(out: &Outcome, from_queue: bool) -> String {
    match out {
        Outcome::Sent(c) => format!("sent ({c})"),
        Outcome::Rejected(c) => format!("rejected ({c})"),
        Outcome::Failed(e) if from_queue => format!("queued, retry failed: {e}"),
        Outcome::Failed(e) => format!("failed: {e}"),
    }
}

// ---------------------------------------------------------------------------
// App wiring
// ---------------------------------------------------------------------------

/// Managed state: the process's [`Engine`].
#[derive(Clone)]
pub struct DiagUploadState(pub Arc<Engine>);

/// This machine's home, user and host names.
fn local_context() -> RedactContext {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let host = env("HOSTNAME").or_else(|| env("COMPUTERNAME")).or_else(|| {
        std::fs::read_to_string("/proc/sys/kernel/hostname")
            .or_else(|_| std::fs::read_to_string("/etc/hostname"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    });
    RedactContext {
        home: env("HOME").or_else(|| env("USERPROFILE")),
        user: env("USER")
            .or_else(|| env("USERNAME"))
            .or_else(|| env("LOGNAME")),
        host,
        secrets: Vec::new(),
    }
}

/// Set up the upload for this process (called from the setup plugin after
/// the diagnostics log is open) and start the auto-mode timer.
pub(crate) fn init<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    embedder: Option<DiagUpload>,
    channel: &BuildChannel,
    debug: &crate::DebugLog,
) -> DiagUploadState {
    use tauri::Manager;
    let resolved = resolve(embedder, &EnvVars::from_process(), channel);
    for n in &resolved.notes {
        debug.push(n.clone());
    }
    let engine = Arc::new(Engine::new(
        resolved,
        EngineParts {
            store_dir: app
                .path()
                .app_data_dir()
                .ok()
                .map(|d| d.join("diagnostics")),
            log_dir: diaglog::log_dir(),
            ctx: local_context(),
            version: app.package_info().version.to_string(),
            channel: channel.as_str().to_string(),
            sender: None,
            now: now_secs(),
        },
    ));
    if engine.is_auto_mode() && !engine.resolved.env_off {
        let e = engine.clone();
        let _ = std::thread::Builder::new()
            .name("diag-upload".into())
            .spawn(move || {
                std::thread::sleep(Duration::from_secs(20));
                loop {
                    tauri::async_runtime::block_on(e.tick(now_secs()));
                    std::thread::sleep(Duration::from_secs(60));
                }
            });
    }
    DiagUploadState(engine)
}

// ---------------------------------------------------------------------------
// Commands (every channel)
// ---------------------------------------------------------------------------

/// Upload configuration, settings, history (without payloads) and queue.
#[tauri::command]
pub async fn diag_upload_status(state: tauri::State<'_, DiagUploadState>) -> Result<Value, String> {
    Ok(state.0.status(now_secs()))
}

/// Change the upload settings; returns the new status.
#[tauri::command]
pub async fn diag_upload_set(
    state: tauri::State<'_, DiagUploadState>,
    allow_send: Option<bool>,
    auto_off: Option<bool>,
    notice_seen: Option<bool>,
    dismiss_crash: Option<bool>,
) -> Result<Value, String> {
    state
        .0
        .set(allow_send, auto_off, notice_seen, dismiss_crash)?;
    Ok(state.0.status(now_secs()))
}

/// Build the exact redacted payload a manual send would POST.
#[tauri::command]
pub async fn diag_upload_preview(
    state: tauri::State<'_, DiagUploadState>,
    note: Option<String>,
    crash: Option<bool>,
) -> Result<Value, String> {
    state
        .0
        .preview(note.as_deref(), crash.unwrap_or(false), now_secs())
}

/// Send the previewed payload `id` (the user's click).
#[tauri::command]
pub async fn diag_upload_send(
    state: tauri::State<'_, DiagUploadState>,
    id: String,
) -> Result<HistoryEntry, String> {
    let engine = state.0.clone();
    engine.send_preview(&id, now_secs()).await
}

/// The exact payload of an upload in the history.
#[tauri::command]
pub async fn diag_upload_payload(
    state: tauri::State<'_, DiagUploadState>,
    id: String,
) -> Result<String, String> {
    state.0.history_payload(&id)
}

/// A panel was opened (counted for the session summary).
#[tauri::command]
pub async fn diag_upload_panel(
    state: tauri::State<'_, DiagUploadState>,
    panel: String,
) -> Result<(), String> {
    state.0.panel_opened(&panel);
    Ok(())
}
