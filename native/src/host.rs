//! The plugin host: what an embedding build adds to the native app
//! ([`NativeConfig`]) and what its views may do ([`Host`]).
//!
//! Generic by design: nothing here knows about any particular plugin.
//!
//! # Safety rules (enforced here, not by convention)
//!
//! * Long or blocking work (serial reads, flash reads) runs as a background
//!   job ([`Host::spawn_job`]), never inside `ui()`.
//! * A file write that replaces data goes through [`Host::write_file`]: it
//!   needs an [`Ack`] (only [`Host::confirm`] makes one) and archives the
//!   current contents into the vault before anything is written.
//! * A mutating adapter operation ([`AdapterAccess::mutate`]) needs an [`Ack`]
//!   and a [`BackupReceipt`] (only [`Host::backup`] makes one). Plugins cannot
//!   construct either type, so the host's confirm + backup-first sequence
//!   cannot be skipped through this API.
//! * A NON-persistent ECU action (output test, mode toggle) runs through
//!   [`AdapterAccess::actuate`]: it needs the target-stamped [`Ack`] of
//!   [`Host::confirm_ecu`] but no backup, and its `restore` half always runs
//!   after `run` (auto-return). It is not a mutation: the write helpers
//!   (`transfer::download`) refuse inside it.
//! * Multi-module scans ([`AdapterAccess::scan`]) are read-only, and raw
//!   driver access ([`AdapterAccess::raw`]) is Bench tier and read-only (see
//!   [`crate::ops`]).
//!
//! # Persistent or not: one table per panel
//!
//! Every panel that changes an ECU keeps an explicit, reviewable per-op
//! table (a `const` slice next to its ops) that classifies each operation it
//! can send as `mutate` (PERSISTENT: coding, adaptation values, flash,
//! anything the ECU keeps after a power cycle; read old value, `backup`,
//! write) or `actuate` (NON-persistent: output tests, timed routines, mode
//! toggles that the hand-back undoes). The class is written down per op,
//! never inferred from the service id or the request bytes.

use crate::theme::{self, Tokens};
use eframe::egui;
use opentune_checksum::ChecksumAlgo;
use opentune_core::ecudef::EcuDef;
pub use opentune_core::Capabilities;
use opentune_core::RawDiag;
pub use opentune_hub::channel::BuildChannel;
use std::any::Any;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, Weak};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Looks up an extra checksum algorithm by id (consulted before the core set).
pub type ChecksumResolver = fn(&str) -> Option<Box<dyn ChecksumAlgo>>;

pub use crate::livefeed::{FeedEvent, FeedValue, LiveFeed};
pub use crate::ops::{
    refuse_ecu_changing, run_then_restore, Actuation, LongJob, ModuleSession, OpHandle, RawBinding,
    RawInterface, RestoreJob, StopFlag, ECU_CHANGING_SERVICES, UNSUPPORTED,
};
pub use crate::security::{Unlocked, Unlocker};
pub use opentune_core::protocol::seedkey::SeedKey;
/// Looks up a seed/key algorithm by id (`None` for ids it does not know).
pub use opentune_hub::seedkeys::SeedKeyResolver;
/// The UnlockECU-format seed/key definitions loader configuration.
pub use opentune_seedkey_loader::LoaderConfig;

/// How the app names itself (an embedding build can rebrand).
#[derive(Clone, Debug)]
pub struct Branding {
    /// Brand text in the top bar and the window title.
    pub name: String,
    /// Small pill under the brand (empty = none).
    pub pill: String,
    /// Window / desktop app id (Wayland, .desktop StartupWMClass).
    pub app_id: String,
}

impl Default for Branding {
    fn default() -> Self {
        Self {
            name: "OpenTune".into(),
            pill: "native".into(),
            app_id: "opentune-native".into(),
        }
    }
}

/// What an embedding build passes to [`crate::run`]. Build it with
/// `..Default::default()` so new fields keep it compiling.
#[derive(Default)]
pub struct NativeConfig {
    pub branding: Branding,
    /// The build channel (default: [`BuildChannel::compiled`], `alpha` with
    /// the `alpha` feature). Bench-tier views and raw adapter access exist
    /// only on `alpha` / `bench` ([`bench_channel`]); a plugin build that
    /// ships Bench views sets `bench`.
    pub channel: BuildChannel,
    /// The user's UnlockECU-format seed/key definitions (see
    /// `docs/SEEDKEY-LOADER.md`), answering ids `unlockecu:<EcuName>`.
    /// `None` (the public build) keeps the loader off. Bench tier: honoured
    /// only on a channel with `debug_enabled()`, ignored with a message on
    /// stable (as the Tauri app's `DesktopConfig::seedkey_loader`).
    pub seedkey_loader: Option<LoaderConfig>,
    /// Extra checksum algorithms, consulted before the core set.
    pub checksums: Vec<ChecksumResolver>,
    /// Extra sidebar destinations.
    pub views: Vec<Box<dyn NativeView>>,
    /// The adapter the views run diagnostics through (None: no adapter layer).
    pub adapter: Option<Arc<dyn AdapterAccess>>,
    /// Seed/key algorithms by id, consulted in order (empty in the public
    /// build: OpenTune ships none). An ECU definition names the id it needs
    /// (`seedkey`); views unlock with [`Host::unlocker`].
    pub seedkeys: Vec<SeedKeyResolver>,
    /// Where an embedding build may send diagnostics reports (None in the
    /// public build: nothing is ever uploaded). The core never calls it on
    /// its own; a build that sets it decides what is sent and when, with the
    /// user's consent.
    pub diagnostics: Option<Arc<dyn DiagnosticsSink>>,
}

/// Resolve a checksum id: the build's resolvers first, then the core set.
pub fn resolve_checksum(resolvers: &[ChecksumResolver], id: &str) -> Option<Box<dyn ChecksumAlgo>> {
    resolvers
        .iter()
        .find_map(|r| r(id))
        .or_else(|| opentune_checksum::get(id))
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// Where a view appears in the sidebar. The groups run top to bottom
/// Calibration, Vehicle, Reference, Help, then custom groups, then Bench.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewGroup {
    /// The old name of [`ViewGroup::Calibration`] (shown there).
    Workspace,
    Reference,
    /// A group of its own (label shown as the section header).
    Custom(String),
    Calibration,
    Vehicle,
    Help,
    /// Bench-tier tools. A view with [`Tier::Bench`] is listed here whatever
    /// group it names (its home group once it graduates).
    Bench,
}

impl ViewGroup {
    /// The sidebar section header.
    pub fn label(&self) -> &str {
        match self {
            ViewGroup::Workspace | ViewGroup::Calibration => "Calibration",
            ViewGroup::Vehicle => "Vehicle",
            ViewGroup::Reference => "Reference",
            ViewGroup::Help => "Help",
            ViewGroup::Bench => "Bench",
            ViewGroup::Custom(l) => l,
        }
    }
}

/// How far a view is trusted. Decided at registration: a Bench view is not
/// registered at all (no sidebar entry, no `ui()`) unless the build channel
/// is `alpha` or `bench`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tier {
    /// Tested; shown in every build, in its own group.
    #[default]
    Public,
    /// Untested on a real car or not public: alpha / bench builds only,
    /// listed in the Bench group. A view graduates by changing its tier.
    Bench,
}

/// Does `channel` enable the Bench tier (`alpha` or `bench`)?
pub fn bench_channel(channel: &BuildChannel) -> bool {
    matches!(channel.as_str(), BuildChannel::ALPHA | BuildChannel::BENCH)
}

/// A sidebar destination supplied by an embedding build.
pub trait NativeView {
    /// Stable id (persisted as the last open view).
    fn id(&self) -> &str;
    fn label(&self) -> &str;
    /// A Phosphor glyph (see `icons`).
    fn icon(&self) -> &str {
        crate::icons::SQUARES_FOUR
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Custom("Plugins".into())
    }
    /// [`Tier::Bench`] views exist only on alpha / bench channels.
    fn tier(&self) -> Tier {
        Tier::Public
    }
    /// Right-aligned sidebar count.
    fn count(&self) -> Option<String> {
        None
    }
    fn ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>);
}

// ---------------------------------------------------------------------------
// Background jobs
// ---------------------------------------------------------------------------

struct JobShared {
    name: String,
    progress: Mutex<(f32, String)>,
    cancel: AtomicBool,
    done: AtomicBool,
}

/// Handed to a job: report progress, check for cancellation. Cheap to clone,
/// so a job can move a copy into an adapter closure (e.g. progress per block
/// of a download).
#[derive(Clone)]
pub struct ProgressSink {
    shared: Arc<JobShared>,
    ctx: egui::Context,
}

impl ProgressSink {
    /// `frac` in 0..=1 (clamped), plus a short status line.
    pub fn set(&self, frac: f32, msg: impl Into<String>) {
        if let Ok(mut p) = self.shared.progress.lock() {
            *p = (frac.clamp(0.0, 1.0), msg.into());
        }
        self.ctx.request_repaint();
    }
    /// True once the user (or the view) asked to cancel; the job should stop
    /// at the next safe point.
    pub fn cancelled(&self) -> bool {
        self.shared.cancel.load(Ordering::SeqCst)
    }
}

/// The result side of a job.
pub struct JobHandle<T> {
    shared: Arc<JobShared>,
    rx: mpsc::Receiver<Result<T, String>>,
    result: Option<Result<T, String>>,
}

impl<T> JobHandle<T> {
    pub fn name(&self) -> &str {
        &self.shared.name
    }
    /// Latest progress fraction and message.
    pub fn progress(&self) -> (f32, String) {
        self.shared
            .progress
            .lock()
            .map(|p| p.clone())
            .unwrap_or_default()
    }
    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::SeqCst);
    }
    pub fn is_running(&mut self) -> bool {
        self.poll().is_none()
    }
    /// The result once the job has finished.
    pub fn poll(&mut self) -> Option<&Result<T, String>> {
        if self.result.is_none() {
            if let Ok(r) = self.rx.try_recv() {
                self.result = Some(r);
            }
        }
        self.result.as_ref()
    }
    /// Take the result out once the job has finished.
    pub fn take(&mut self) -> Option<Result<T, String>> {
        self.poll();
        self.result.take()
    }
}

/// Runs jobs on background threads and lists the running ones (status bar).
#[derive(Clone)]
pub struct Jobs {
    ctx: egui::Context,
    list: Arc<Mutex<Vec<Weak<JobShared>>>>,
}

impl Jobs {
    pub fn new(ctx: egui::Context) -> Self {
        Self {
            ctx,
            list: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn spawn<T, F>(&self, name: &str, f: F) -> JobHandle<T>
    where
        T: Send + 'static,
        F: FnOnce(&ProgressSink) -> Result<T, String> + Send + 'static,
    {
        let shared = Arc::new(JobShared {
            name: name.to_string(),
            progress: Mutex::new((0.0, String::new())),
            cancel: AtomicBool::new(false),
            done: AtomicBool::new(false),
        });
        if let Ok(mut l) = self.list.lock() {
            l.retain(|w| w.upgrade().is_some_and(|s| !s.done.load(Ordering::SeqCst)));
            l.push(Arc::downgrade(&shared));
        }
        let (tx, rx) = mpsc::channel();
        let sink = ProgressSink {
            shared: shared.clone(),
            ctx: self.ctx.clone(),
        };
        std::thread::Builder::new()
            .name(format!("job: {name}"))
            .spawn(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&sink)))
                    .unwrap_or_else(|_| Err("the job panicked".to_string()));
                sink.shared.done.store(true, Ordering::SeqCst);
                let _ = tx.send(r);
                sink.ctx.request_repaint();
            })
            .expect("spawn job thread");
        JobHandle {
            shared,
            rx,
            result: None,
        }
    }

    /// Running jobs: (name, fraction, message).
    pub fn active(&self) -> Vec<(String, f32, String)> {
        let Ok(l) = self.list.lock() else {
            return Vec::new();
        };
        l.iter()
            .filter_map(Weak::upgrade)
            .filter(|s| !s.done.load(Ordering::SeqCst))
            .map(|s| {
                let (f, m) = s.progress.lock().map(|p| p.clone()).unwrap_or_default();
                (s.name.clone(), f, m)
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Confirmation, backups, guarded writes
// ---------------------------------------------------------------------------

/// Proof that the user confirmed one destructive operation. Only
/// [`Host::confirm`] creates it; it is consumed by the operation it guards.
#[derive(Debug)]
pub struct Ack {
    /// The module the user saw in the dialog ([`Host::confirm_ecu`] only).
    target: Option<DiagTarget>,
}

impl Ack {
    /// The diagnostic module this confirmation names, if it was an ECU
    /// operation ([`Host::confirm_ecu`]).
    pub fn target(&self) -> Option<DiagTarget> {
        self.target
    }

    /// The module a mutating adapter operation must address: the one the
    /// user confirmed. A file-write confirmation is refused here.
    pub fn ecu_target(&self) -> Result<DiagTarget, String> {
        self.target.ok_or_else(|| {
            "this confirmation does not name a target module (use Host::confirm_ecu)".into()
        })
    }
}

/// The diagnostic module requests go to: request (tx) and response (rx)
/// CAN ids. One global target for the whole app (Settings → Adapter
/// settings, else the definition's ids, else the engine).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DiagTarget {
    pub tx: u32,
    pub rx: u32,
}

impl DiagTarget {
    /// The engine ECU's physical diagnostic address (ISO 15765-4).
    pub const ENGINE: DiagTarget = DiagTarget {
        tx: 0x7E0,
        rx: 0x7E8,
    };
}

impl Default for DiagTarget {
    fn default() -> Self {
        Self::ENGINE
    }
}

impl std::fmt::Display for DiagTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let id = |v: u32| {
            if v > 0x7FF {
                format!("0x{v:08X}")
            } else {
                format!("0x{v:03X}")
            }
        };
        write!(f, "tx {} / rx {}", id(self.tx), id(self.rx))
    }
}

/// The app's target from the Adapter settings fields (hex, both or neither)
/// and the selected definition: typed ids win, then the definition's
/// (non-zero) ids, then [`DiagTarget::ENGINE`].
pub fn resolve_target(tx: &str, rx: &str, def: Option<&EcuDef>) -> Result<DiagTarget, String> {
    let parse = |s: &str| {
        let h = s.trim();
        let h = h
            .strip_prefix("0x")
            .or_else(|| h.strip_prefix("0X"))
            .unwrap_or(h);
        u32::from_str_radix(h, 16)
            .ok()
            .filter(|v| *v <= 0x1FFF_FFFF && !h.is_empty() && h.len() <= 8)
            .ok_or_else(|| format!("'{}' is not a hex CAN id", s.trim()))
    };
    match (tx.trim().is_empty(), rx.trim().is_empty()) {
        (false, false) => Ok(DiagTarget {
            tx: parse(tx)?,
            rx: parse(rx)?,
        }),
        (true, true) => Ok(match def {
            Some(d)
                if d.tx_id != 0
                    && d.rx_id != 0
                    && d.tx_id <= 0x1FFF_FFFF
                    && d.rx_id <= 0x1FFF_FFFF =>
            {
                DiagTarget {
                    tx: d.tx_id,
                    rx: d.rx_id,
                }
            }
            _ => DiagTarget::ENGINE,
        }),
        _ => Err("set both the request (tx) and response (rx) ids, or neither".into()),
    }
}

/// Proof that a backup was written to the vault. Only [`Host::backup`] (and
/// [`Host::write_file`] internally) create it.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BackupReceipt {
    pub path: PathBuf,
    pub crc32: u32,
    pub len: usize,
}

/// What a destructive operation asks the user.
#[derive(Clone, Debug)]
pub struct ConfirmRequest {
    /// Stable key per operation (one dialog per key).
    pub key: String,
    pub title: String,
    pub body: String,
    /// If set, the user must type this text to enable the confirm button.
    pub typed: Option<String>,
    pub confirm_label: String,
}

#[derive(Debug)]
pub enum ConfirmResult {
    /// The dialog is open (or just opened).
    Pending,
    Accepted(Ack),
    Declined,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfirmState {
    Open,
    Accepted,
    Declined,
}

/// Open confirmation dialogs, rendered by the app every frame.
#[derive(Default)]
pub struct Confirms {
    /// key → (request, state, typed text, target module shown).
    #[allow(clippy::type_complexity)]
    open: BTreeMap<String, (ConfirmRequest, ConfirmState, String, Option<DiagTarget>)>,
}

impl Confirms {
    pub(crate) fn ask(&mut self, req: ConfirmRequest) -> ConfirmResult {
        self.ask_with(req, None)
    }

    /// Forget any dialog for `key` (open or answered), so no earlier answer
    /// can be consumed later.
    pub(crate) fn withdraw(&mut self, key: &str) {
        self.open.remove(key);
    }

    /// Like `ask`, for an operation on module `target`: the dialog shows it
    /// and the `Ack` carries the target exactly as shown when it opened.
    pub(crate) fn ask_with(
        &mut self,
        req: ConfirmRequest,
        target: Option<DiagTarget>,
    ) -> ConfirmResult {
        match self.open.get(&req.key).map(|(_, s, _, _)| *s) {
            Some(ConfirmState::Accepted) => {
                let (_, _, _, shown) = self.open.remove(&req.key).expect("present");
                ConfirmResult::Accepted(Ack { target: shown })
            }
            Some(ConfirmState::Declined) => {
                self.open.remove(&req.key);
                ConfirmResult::Declined
            }
            Some(ConfirmState::Open) => ConfirmResult::Pending,
            None => {
                self.open.insert(
                    req.key.clone(),
                    (req, ConfirmState::Open, String::new(), target),
                );
                ConfirmResult::Pending
            }
        }
    }

    /// Draw the open dialogs (one at a time).
    pub fn ui(&mut self, ctx: &egui::Context) {
        let t = theme::tokens(ctx);
        let Some((key, (req, _, typed, target))) = self
            .open
            .iter_mut()
            .find(|(_, (_, s, _, _))| *s == ConfirmState::Open)
        else {
            return;
        };
        let key = key.clone();
        let mut decided = None;
        egui::Modal::new(egui::Id::unique("host-confirm")).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.label(
                egui::RichText::new(format!("{}  {}", crate::icons::WARNING, req.title))
                    .font(theme::font(theme::DISPLAY, 16.0))
                    .color(t.fg),
            );
            ui.add_space(6.0);
            ui.label(egui::RichText::new(&req.body).color(t.fg_dim));
            if let Some(tg) = target {
                ui.add_space(8.0);
                egui::Frame::new()
                    .fill(t.warn.gamma_multiply(0.14))
                    .stroke(egui::Stroke::new(1.0, t.warn.gamma_multiply(0.5)))
                    .corner_radius(egui::CornerRadius::same(8))
                    .inner_margin(egui::Margin::symmetric(10, 7))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("Target module")
                                    .font(theme::font(theme::UI_STRONG, 12.0))
                                    .color(t.fg),
                            );
                            ui.label(
                                egui::RichText::new(tg.to_string())
                                    .font(theme::font(theme::MONO_STRONG, 13.0))
                                    .color(t.warn),
                            );
                        });
                    });
            }
            let ok_enabled = match &req.typed {
                Some(word) => {
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(format!("Type {word} to confirm"))
                            .small()
                            .color(t.fg_faint),
                    );
                    ui.add(egui::TextEdit::singleline(typed).desired_width(220.0));
                    typed.trim() == word
                }
                None => true,
            };
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled_ui(ok_enabled, |ui| {
                        theme::primary_button(ui, &t, None, &req.confirm_label)
                    })
                    .inner
                    .clicked()
                {
                    decided = Some(ConfirmState::Accepted);
                }
                if theme::button(ui, &t, None, "Cancel").clicked() {
                    decided = Some(ConfirmState::Declined);
                }
            });
        });
        if let Some(d) = decided {
            if let Some(e) = self.open.get_mut(&key) {
                e.1 = d;
            }
            ctx.request_repaint();
        }
    }

    #[cfg(test)]
    pub(crate) fn answer(&mut self, key: &str, accept: bool) {
        if let Some(e) = self.open.get_mut(key) {
            e.1 = if accept {
                ConfirmState::Accepted
            } else {
                ConfirmState::Declined
            };
        }
    }
}

/// Archive `data` into the vault beside `near` (an image path, or a directory
/// to use as the vault's parent).
///
/// SAFETY INVARIANT: a [`BackupReceipt`] means "a real, NON-EMPTY backup was
/// written". Zero-length data is refused here and no receipt is issued (an
/// empty read must never stand in for the data a write is about to
/// replace). Only the length counts: all-zero bytes are a valid backup.
fn backup_into_vault(near: &Path, label: &str, data: &[u8]) -> Result<BackupReceipt, String> {
    if data.is_empty() {
        return Err(
            "backup refused: there is no data to back up (0 bytes); nothing was \
                    written and no receipt was issued"
                .into(),
        );
    }
    let dir = if near.is_dir() {
        near.join("opentune-vault")
    } else {
        crate::views::vault::vault_dir_for(near)
    };
    let ecu = opentune_core::EcuId {
        ecu_name: Some(label.to_string()),
        ..Default::default()
    };
    let region = opentune_core::MemoryRegion::new(0, data.len() as u32);
    let path = opentune_flash::vault::save_backup(&dir, &ecu, region, data)
        .map_err(|e| format!("backup failed: {e}"))?;
    Ok(BackupReceipt {
        path,
        crc32: crate::views::vault::crc32(data),
        len: data.len(),
    })
}

/// Write through a temporary file and rename, so a crash never leaves half a file.
pub(crate) fn atomic_write(target: &Path, data: &[u8]) -> Result<(), String> {
    let tmp = target.with_extension("opentune-tmp");
    std::fs::write(&tmp, data).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("replace {}: {e}", target.display())
    })
}

/// The outcome of a guarded write.
#[derive(Debug)]
pub struct WriteReceipt {
    pub target: PathBuf,
    /// The vault copy of what the target held before (None: it did not exist
    /// or was empty).
    pub backup: Option<BackupReceipt>,
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

/// An embedding build's diagnostics upload target (see
/// [`NativeConfig::diagnostics`]). A report is opaque text the build
/// assembled; implementations must only send it with the user's consent.
pub trait DiagnosticsSink: Send + Sync {
    /// Human-readable destination, shown before anything is sent.
    fn destination(&self) -> String;
    /// Send one report; returns a reference (ticket id, URL) or an error.
    fn submit(&self, report: &str) -> Result<String, String>;
}

/// Snapshot of the adapter connection the top bar shows.
#[derive(Clone, Debug, Default)]
pub struct LinkInfo {
    pub driver: String,
    pub port: String,
    pub connected: bool,
    pub battery_v: Option<f32>,
    /// The app's current target module (`None`: the Adapter settings ids are
    /// incomplete or invalid, so no ECU operation can run).
    pub target: Option<DiagTarget>,
}

/// One diagnostic exchange sequence, run where the driver lives (drivers are
/// not `Send`, so an implementation may run it on its own worker thread).
pub type DiagJob =
    Box<dyn FnOnce(&mut dyn RawDiag) -> Result<Box<dyn Any + Send>, String> + Send + 'static>;

/// Diagnostics through the app's adapter. Implementations serialize access
/// (one operation at a time) and never let a view open a driver itself.
///
/// Call it through the typed wrappers on `dyn AdapterAccess`
/// ([`read`](#method.read), [`mutate`](#method.mutate)) from a background job.
pub trait AdapterAccess: Send + Sync {
    fn link(&self) -> LinkInfo;
    /// A read-only operation (identification, reads, live data) on module
    /// `target` (pass [`Host::target`]: the app's one global target).
    fn read_job(
        &self,
        op: &str,
        target: DiagTarget,
        job: DiagJob,
    ) -> Result<Box<dyn Any + Send>, String>;
    /// An operation that changes the ECU. Needs the user's [`Ack`] from
    /// [`Host::confirm_ecu`] and a [`BackupReceipt`] of the data it is about
    /// to change. Implementations address [`Ack::ecu_target`] — the module
    /// the user saw in the dialog — and refuse an `Ack` without one.
    fn mutate_job(
        &self,
        op: &str,
        ack: Ack,
        backup: &BackupReceipt,
        job: DiagJob,
    ) -> Result<Box<dyn Any + Send>, String>;

    // The methods below are additive: their default bodies refuse with
    // [`UNSUPPORTED`], so adapters written before them keep compiling.

    /// What the connected adapter can do (its last probe), if known.
    fn capabilities(&self) -> Option<Capabilities> {
        None
    }

    /// A NON-persistent action on the module the user confirmed
    /// ([`Ack::ecu_target`], from [`Host::confirm_ecu`]; no backup). `run`
    /// then `restore` on the same link, through [`run_then_restore`]:
    /// `restore` ALWAYS runs, also when `run` fails or panics. It is not a
    /// mutation: implementations must not mark it as one, so
    /// `transfer::download` refuses inside it. `Err` means nothing ran
    /// (refused, busy, the link would not open).
    fn actuate_job(
        &self,
        op: &str,
        ack: Ack,
        run: DiagJob,
        restore: RestoreJob,
    ) -> Result<Actuation<Box<dyn Any + Send>>, String> {
        let _ = (op, ack, run, restore);
        Err(UNSUPPORTED.into())
    }

    /// Start a long-running READ-ONLY operation on `target` and return once
    /// it is queued. It holds the adapter until the job returns (every other
    /// operation gets "adapter busy" meanwhile); the job must end on its own
    /// when its [`StopFlag`] is set. The implementation calls exactly one of
    /// [`LongJob::run`] / [`LongJob::fail`].
    fn start_job(&self, op: &str, target: DiagTarget, job: LongJob) -> Result<(), String> {
        let _ = (op, target, job);
        Err(UNSUPPORTED.into())
    }

    /// One READ-ONLY operation over several modules: the job re-binds with
    /// [`ModuleSession::bind`]. Implementations refuse ECU-changing requests
    /// ([`refuse_ecu_changing`]).
    fn scan_job(&self, op: &str, job: ScanJob) -> Result<Box<dyn Any + Send>, String> {
        let _ = (op, job);
        Err(UNSUPPORTED.into())
    }

    /// Raw driver access, Bench tier: implementations refuse it unless the
    /// build channel is alpha / bench, and hand the job only a
    /// [`RawInterface`] (which refuses flash writes and ECU-changing
    /// requests).
    fn raw_job(
        &self,
        op: &str,
        bind: RawBinding,
        job: RawJob,
    ) -> Result<Box<dyn Any + Send>, String> {
        let _ = (op, bind, job);
        Err(UNSUPPORTED.into())
    }
}

/// A multi-module read-only job ([`AdapterAccess::scan_job`]).
pub type ScanJob =
    Box<dyn FnOnce(&mut dyn ModuleSession) -> Result<Box<dyn Any + Send>, String> + Send + 'static>;
/// A raw driver job ([`AdapterAccess::raw_job`]).
pub type RawJob =
    Box<dyn FnOnce(&mut RawInterface<'_>) -> Result<Box<dyn Any + Send>, String> + Send + 'static>;

/// Run the core protocol clients (`UdsClient`, `Kwp2000Client`) over the
/// [`RawDiag`] an adapter job gets:
///
/// ```ignore
/// adapter.read("identify", host.target()?, |d| {
///     let mut t = RawTransport::new(d);
///     UdsClient::new(&mut t).read_data_by_id(0xF187).map_err(|e| e.to_string())
/// })?;
/// ```
///
/// The responsePending (`7F <sid> 78`) wait happens once, below the
/// `RawDiag`, in the driver's transport (ISO-TP, TP2.0, DoIP, K-line), which
/// keeps receiving and never re-sends the request. A pending reply that still
/// reaches this bridge (the transport gave up waiting) is an error here: the
/// request is never sent again, so a write cannot be duplicated. Negative
/// responses pass through as raw `7F` bytes, which the clients turn into
/// `Error::NegativeResponse`; a lost link becomes `Error::AdapterReset`.
/// The per-request timeout is the adapter's (`timeout_ms` is ignored).
pub struct RawTransport<'a> {
    inner: &'a mut dyn RawDiag,
}

impl<'a> RawTransport<'a> {
    pub fn new(inner: &'a mut dyn RawDiag) -> Self {
        Self { inner }
    }
}

impl opentune_core::protocol::DiagTransport for RawTransport<'_> {
    fn request(&mut self, req: &[u8], _timeout_ms: u64) -> opentune_core::Result<Vec<u8>> {
        use opentune_core::resilient::LINK_LOST_PREFIX;
        match self.inner.request(req) {
            Ok(resp) if opentune_core::protocol::diag::is_response_pending(req, &resp) => {
                Err(opentune_core::Error::Protocol(format!(
                    "service 0x{:02X}: the ECU was still answering responsePending (0x78) \
                     when the adapter stopped waiting; the request was not re-sent",
                    req.first().copied().unwrap_or(0)
                )))
            }
            Ok(resp) => Ok(resp),
            Err(e) => Err(match e.strip_prefix(LINK_LOST_PREFIX) {
                Some(why) => opentune_core::Error::AdapterReset(why.to_string()),
                None => opentune_core::Error::Protocol(e),
            }),
        }
    }
}

/// Test-only constructors for the proof types (other modules' tests).
#[cfg(test)]
pub(crate) fn test_ack(t: DiagTarget) -> Ack {
    Ack { target: Some(t) }
}
/// A file-write confirmation (names no module).
#[cfg(test)]
pub(crate) fn test_file_ack() -> Ack {
    Ack { target: None }
}
#[cfg(test)]
pub(crate) fn test_backup(dir: &Path, data: &[u8]) -> BackupReceipt {
    backup_into_vault(dir, "test", data).unwrap()
}

thread_local! {
    static IN_MUTATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Is the current thread running the closure of an `adapter.mutate(…)`?
/// (Write helpers such as [`crate::transfer::download`] refuse otherwise.)
pub(crate) fn in_mutation() -> bool {
    IN_MUTATION.with(|c| c.get())
}

/// Marks the closure of a `mutate` while it runs (on whichever thread the
/// adapter runs it). Only the host creates one.
pub(crate) struct MutationScope(bool);

impl MutationScope {
    pub(crate) fn enter() -> Self {
        Self(IN_MUTATION.with(|c| c.replace(true)))
    }
}

impl Drop for MutationScope {
    fn drop(&mut self) {
        let prev = self.0;
        IN_MUTATION.with(|c| c.set(prev));
    }
}

fn boxed<R, F>(f: F) -> DiagJob
where
    R: Send + 'static,
    F: FnOnce(&mut dyn RawDiag) -> Result<R, String> + Send + 'static,
{
    Box::new(move |d: &mut dyn RawDiag| f(d).map(|r| Box::new(r) as Box<dyn Any + Send>))
}

/// `Err` for a zero-length [`BackupReceipt`]: the mutate path never runs on
/// a backup that holds nothing (see `backup_into_vault`).
pub(crate) fn refuse_empty_receipt(op: &str, backup: &BackupReceipt) -> Result<(), String> {
    if backup.len == 0 {
        return Err(format!(
            "{op}: the backup {} is empty (0 bytes); nothing was sent",
            backup.path.display()
        ));
    }
    Ok(())
}

pub(crate) fn unbox<R: 'static>(r: Result<Box<dyn Any + Send>, String>) -> Result<R, String> {
    r?.downcast::<R>()
        .map(|b| *b)
        .map_err(|_| "adapter returned an unexpected result type".into())
}

impl dyn AdapterAccess {
    /// Run `f` read-only on module `target` and return its result.
    pub fn read<R, F>(&self, op: &str, target: DiagTarget, f: F) -> Result<R, String>
    where
        R: Send + 'static,
        F: FnOnce(&mut dyn RawDiag) -> Result<R, String> + Send + 'static,
    {
        unbox(self.read_job(op, target, boxed(f)))
    }

    /// Run `f`, which changes the ECU, on the module the user confirmed.
    pub fn mutate<R, F>(
        &self,
        op: &str,
        ack: Ack,
        backup: &BackupReceipt,
        f: F,
    ) -> Result<R, String>
    where
        R: Send + 'static,
        F: FnOnce(&mut dyn RawDiag) -> Result<R, String> + Send + 'static,
    {
        // SAFETY INVARIANT (see backup_into_vault): a receipt for 0 bytes
        // backs nothing up. Refused here, before any adapter sees the job,
        // so no AdapterAccess implementation can be reached with one.
        refuse_empty_receipt(op, backup)?;
        // The closure runs as a mutation (the write helpers check for it).
        let f = move |d: &mut dyn RawDiag| {
            let _scope = MutationScope::enter();
            f(d)
        };
        unbox(self.mutate_job(op, ack, backup, boxed(f)))
    }

    /// Run the NON-persistent action `run` on the module the user confirmed,
    /// then `restore` (always, also after an error or a panic). Not a
    /// mutation: no backup, and the write helpers refuse inside it. `Err`:
    /// nothing ran; otherwise check [`Actuation::restored`].
    pub fn actuate<R, F, G>(
        &self,
        op: &str,
        ack: Ack,
        run: F,
        restore: G,
    ) -> Result<Actuation<R>, String>
    where
        R: Send + 'static,
        F: FnOnce(&mut dyn RawDiag) -> Result<R, String> + Send + 'static,
        G: FnOnce(&mut dyn RawDiag) -> Result<(), String> + Send + 'static,
    {
        let a = self.actuate_job(op, ack, boxed(run), Box::new(restore))?;
        Ok(a.map(|b| unbox(Ok(b))))
    }

    /// Start the long-running read-only `f` on `target`; it should return
    /// once its [`StopFlag`] is set. Dropping the handle stops it.
    pub fn start<R, F>(&self, op: &str, target: DiagTarget, f: F) -> Result<OpHandle<R>, String>
    where
        R: Send + 'static,
        F: FnOnce(&mut dyn RawDiag, &StopFlag) -> Result<R, String> + Send + 'static,
    {
        let (job, handle) = LongJob::new(f);
        self.start_job(op, target, job)?;
        Ok(handle)
    }

    /// Run the read-only multi-module `f` as one operation.
    pub fn scan<R, F>(&self, op: &str, f: F) -> Result<R, String>
    where
        R: Send + 'static,
        F: FnOnce(&mut dyn ModuleSession) -> Result<R, String> + Send + 'static,
    {
        let job: ScanJob = Box::new(move |m: &mut dyn ModuleSession| {
            f(m).map(|r| Box::new(r) as Box<dyn Any + Send>)
        });
        unbox(self.scan_job(op, job))
    }

    /// Run `f` on the raw driver (Bench tier; see [`RawInterface`]).
    pub fn raw<R, F>(&self, op: &str, bind: RawBinding, f: F) -> Result<R, String>
    where
        R: Send + 'static,
        F: FnOnce(&mut RawInterface<'_>) -> Result<R, String> + Send + 'static,
    {
        let job: RawJob = Box::new(move |i: &mut RawInterface<'_>| {
            f(i).map(|r| Box::new(r) as Box<dyn Any + Send>)
        });
        unbox(self.raw_job(op, bind, job))
    }
}

// ---------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------

/// Something a view asks the app to do after its `ui()` returns.
#[derive(Clone, Debug)]
pub enum HostAction {
    OpenImage(PathBuf),
    SelectDefinition(String),
    SelectMap(usize),
    HexJump(usize),
    /// Change cells of map `map` of the selected definition in the working
    /// image: `(cell index row-major, physical value)`. Nothing is written to
    /// disk; the user saves through the Flash view's confirm + backup-first
    /// path, with checksums corrected by the configured resolvers.
    EditCells {
        map: usize,
        cells: Vec<(usize, f64)>,
    },
    /// Add an ECU definition for this session (an imported XDF / A2L, a
    /// promoted map region). It is appended to the definitions; one with the
    /// same id replaces the earlier one. Nothing is written to disk.
    AddDefinition(Box<EcuDef>),
}

impl PartialEq for HostAction {
    fn eq(&self, other: &Self) -> bool {
        use HostAction::*;
        match (self, other) {
            (OpenImage(a), OpenImage(b)) => a == b,
            (SelectDefinition(a), SelectDefinition(b)) => a == b,
            (SelectMap(a), SelectMap(b)) => a == b,
            (HexJump(a), HexJump(b)) => a == b,
            (EditCells { map: m, cells: c }, EditCells { map: n, cells: d }) => m == n && c == d,
            // EcuDef has no PartialEq: compare the serialized definitions.
            (AddDefinition(a), AddDefinition(b)) => {
                serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
            }
            _ => false,
        }
    }
}

/// The bundled data folders a view may read (see `paths.rs` for where each
/// is looked for).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataRoot {
    EcuDefs,
    Database,
    CtrlDefs,
    /// The vehicle catalog.
    Catalog,
    /// Definition packs.
    Packs,
    /// The in-app guide (Markdown).
    Guide,
}

impl DataRoot {
    pub const ALL: [DataRoot; 6] = [
        DataRoot::EcuDefs,
        DataRoot::Database,
        DataRoot::CtrlDefs,
        DataRoot::Catalog,
        DataRoot::Packs,
        DataRoot::Guide,
    ];

    /// The folder name.
    pub fn dir_name(self) -> &'static str {
        match self {
            DataRoot::EcuDefs => "ecudefs",
            DataRoot::Database => "database",
            DataRoot::CtrlDefs => "ctrldefs",
            DataRoot::Catalog => "catalog",
            DataRoot::Packs => "packs",
            DataRoot::Guide => "guide",
        }
    }
}

/// App-lifetime state every view's [`Host`] shares (built once by the app).
pub(crate) struct HostShared {
    pub(crate) channel: BuildChannel,
    pub(crate) data: crate::paths::DataRoots,
    pub(crate) live: LiveFeed,
    pub(crate) gps: crate::widgets::gps::GpsSource,
}

impl HostShared {
    pub(crate) fn new(channel: BuildChannel, ctx: Option<egui::Context>) -> Self {
        Self {
            channel,
            data: crate::paths::DataRoots::resolve(),
            live: ctx.map(LiveFeed::new).unwrap_or_default(),
            gps: Default::default(),
        }
    }
}

#[cfg(test)]
impl Default for HostShared {
    fn default() -> Self {
        Self::new(BuildChannel::new(BuildChannel::STABLE), None)
    }
}

/// What a view gets every frame.
pub struct Host<'a> {
    pub(crate) tokens: Tokens,
    pub(crate) image_path: Option<&'a Path>,
    pub(crate) image: Option<&'a [u8]>,
    pub(crate) image_edited: bool,
    pub(crate) defs: &'a [EcuDef],
    pub(crate) def_idx: Option<usize>,
    pub(crate) map_idx: usize,
    pub(crate) checksums: &'a [ChecksumResolver],
    pub(crate) adapter: Option<&'a Arc<dyn AdapterAccess>>,
    pub(crate) diagnostics: Option<&'a Arc<dyn DiagnosticsSink>>,
    pub(crate) seedkeys: &'a opentune_hub::seedkeys::SeedKeys,
    pub(crate) target: Result<DiagTarget, String>,
    pub(crate) jobs: &'a Jobs,
    pub(crate) confirms: &'a mut Confirms,
    pub(crate) actions: &'a mut Vec<HostAction>,
    pub(crate) status: &'a mut Option<String>,
    pub(crate) shared: &'a HostShared,
}

impl<'a> Host<'a> {
    /// The build channel.
    pub fn channel(&self) -> &BuildChannel {
        &self.shared.channel
    }
    /// Is the Bench tier on (channel `alpha` or `bench`)? A view checks it
    /// before offering a Bench-only operation inside a public view.
    pub fn bench_enabled(&self) -> bool {
        bench_channel(&self.shared.channel)
    }
    /// Where the bundled data folder `root` was found, if anywhere.
    pub fn data_dir(&self, root: DataRoot) -> Option<&Path> {
        self.shared.data.get(root)
    }
    /// The live feed (Dashboard / Live log publish, the Drive view reads).
    pub fn live_feed(&self) -> LiveFeed {
        self.shared.live.clone()
    }
    /// The app-wide GPS source (one receiver, shared by the panels' GPS
    /// cards).
    pub fn gps(&self) -> crate::widgets::gps::GpsSource {
        self.shared.gps.clone()
    }
    /// What the connected adapter can do (its last probe), if known.
    pub fn capabilities(&self) -> Option<Capabilities> {
        self.adapter.and_then(|a| a.capabilities())
    }
    /// Theme tokens (use them with `theme::*` helpers so the view looks native).
    pub fn tokens(&self) -> &Tokens {
        &self.tokens
    }
    pub fn image(&self) -> Option<&[u8]> {
        self.image
    }
    pub fn image_path(&self) -> Option<&Path> {
        self.image_path
    }
    /// Does the working image hold unsaved edits? ([`Host::image`] shows them.)
    pub fn image_edited(&self) -> bool {
        self.image_edited
    }
    pub fn definitions(&self) -> &[EcuDef] {
        self.defs
    }
    pub fn definition(&self) -> Option<&EcuDef> {
        self.def_idx.map(|i| &self.defs[i])
    }
    pub fn map_index(&self) -> usize {
        self.map_idx
    }
    pub fn checksum(&self, id: &str) -> Option<Box<dyn ChecksumAlgo>> {
        resolve_checksum(self.checksums, id)
    }
    /// The adapter, to move into a background job.
    pub fn adapter(&self) -> Option<Arc<dyn AdapterAccess>> {
        self.adapter.cloned()
    }
    /// The link, with the app's current target module.
    /// The SecurityAccess unlocker for the selected definition's `seedkey`
    /// id. An error (nothing sent) when no definition is selected, it names
    /// no algorithm, or no configured resolver knows the id.
    pub fn unlocker(&self) -> Result<Unlocker, String> {
        let d = self
            .definition()
            .ok_or("select an ECU definition first (it names the seed/key algorithm)")?;
        let id = d
            .seedkey
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| format!("the definition '{}' names no seed/key algorithm", d.id))?;
        Unlocker::new(self.seedkeys.clone(), id)
    }

    /// The unlocker for an explicit seed/key `id` (e.g. a per-module
    /// algorithm the view knows better than the definition).
    pub fn unlocker_for(&self, id: &str) -> Result<Unlocker, String> {
        Unlocker::new(self.seedkeys.clone(), id)
    }

    /// The build's diagnostics upload target (None in the public build).
    pub fn diagnostics(&self) -> Option<Arc<dyn DiagnosticsSink>> {
        self.diagnostics.cloned()
    }

    /// The link, with the app's current target module.
    pub fn link(&self) -> LinkInfo {
        let mut l = self.adapter.map(|a| a.link()).unwrap_or_default();
        l.target = self.target.as_ref().ok().copied();
        l
    }

    /// The app's one target module (Settings → Adapter settings, else the
    /// definition's ids, else the engine), or why it is unusable.
    pub fn target(&self) -> Result<DiagTarget, String> {
        self.target.clone()
    }

    /// Confirm an operation that changes an ECU. The dialog shows the target
    /// module, and the returned [`Ack`] carries it, so the write goes to the
    /// module the user confirmed even if the global target changes later.
    /// Declined at once when the target is unusable.
    pub fn confirm_ecu(&mut self, req: ConfirmRequest) -> ConfirmResult {
        match &self.target {
            Ok(t) => self.confirms.ask_with(req, Some(*t)),
            Err(e) => {
                // An earlier dialog for this key must not be accepted later
                // with a target that is no longer the app's.
                self.confirms.withdraw(&req.key);
                *self.status = Some(format!("no target module: {e}"));
                ConfirmResult::Declined
            }
        }
    }

    pub fn spawn_job<T, F>(&self, name: &str, f: F) -> JobHandle<T>
    where
        T: Send + 'static,
        F: FnOnce(&ProgressSink) -> Result<T, String> + Send + 'static,
    {
        self.jobs.spawn(name, f)
    }

    /// Ask the user to confirm a destructive operation. Call it every frame
    /// with the same `key` until it returns `Accepted` or `Declined`.
    pub fn confirm(&mut self, req: ConfirmRequest) -> ConfirmResult {
        self.confirms.ask(req)
    }

    /// Archive `data` into the vault beside `near` (image path or directory).
    pub fn backup(&self, near: &Path, label: &str, data: &[u8]) -> Result<BackupReceipt, String> {
        backup_into_vault(near, label, data)
    }

    /// Replace `target` with `data`: the current contents go to the vault
    /// first (the write is refused if that fails), then an atomic replace.
    pub fn write_file(
        &mut self,
        ack: Ack,
        target: &Path,
        data: &[u8],
    ) -> Result<WriteReceipt, String> {
        guarded_write(ack, target, data)
    }

    pub fn pick_file(&self, title: &str, filters: &[(&str, &[&str])]) -> Option<PathBuf> {
        let mut d = rfd::FileDialog::new().set_title(title);
        for (name, ext) in filters {
            d = d.add_filter(*name, ext);
        }
        d.pick_file()
    }
    pub fn pick_save(&self, title: &str, file_name: &str) -> Option<PathBuf> {
        rfd::FileDialog::new()
            .set_title(title)
            .set_file_name(file_name)
            .save_file()
    }
    pub fn pick_dir(&self, title: &str) -> Option<PathBuf> {
        rfd::FileDialog::new().set_title(title).pick_folder()
    }

    /// A one-line status message (status bar).
    pub fn status(&mut self, msg: impl Into<String>) {
        *self.status = Some(msg.into());
    }
    pub fn act(&mut self, a: HostAction) {
        self.actions.push(a);
    }
}

pub(crate) fn guarded_write(_ack: Ack, target: &Path, data: &[u8]) -> Result<WriteReceipt, String> {
    let backup = match std::fs::read(target) {
        // An existing EMPTY file holds nothing to lose: no backup (and no
        // receipt, per the invariant), as for a file that did not exist.
        Ok(old) if old.is_empty() => None,
        Ok(old) => Some(backup_into_vault(target, "before write", &old)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("read {} for the backup: {e}", target.display())),
    };
    atomic_write(target, data)?;
    Ok(WriteReceipt {
        target: target.to_path_buf(),
        backup,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ot-host-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn wait<T>(h: &mut JobHandle<T>) -> Result<T, String> {
        let t0 = Instant::now();
        loop {
            if let Some(r) = h.take() {
                return r;
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "job timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn jobs_report_progress_finish_and_cancel() {
        let jobs = Jobs::new(egui::Context::default());
        let mut h = jobs.spawn("sum", |p| {
            p.set(0.5, "half");
            Ok(21 * 2)
        });
        assert_eq!(wait(&mut h), Ok(42));
        assert_eq!(h.progress(), (0.5, "half".to_string()));

        let (go_tx, go_rx) = mpsc::channel::<()>();
        let mut c = jobs.spawn("cancellable", move |p| {
            go_rx.recv().ok();
            if p.cancelled() {
                Err("cancelled".into())
            } else {
                Ok(())
            }
        });
        assert_eq!(
            jobs.active()
                .iter()
                .filter(|(n, ..)| n == "cancellable")
                .count(),
            1
        );
        c.cancel();
        go_tx.send(()).unwrap();
        assert_eq!(wait(&mut c), Err("cancelled".to_string()));

        let mut p = jobs.spawn::<(), _>("boom", |_| panic!("x"));
        assert_eq!(wait(&mut p), Err("the job panicked".to_string()));
    }

    #[test]
    fn confirm_hands_out_one_ack_per_acceptance() {
        let mut c = Confirms::default();
        let req = || ConfirmRequest {
            key: "k".into(),
            title: "t".into(),
            body: "b".into(),
            typed: None,
            confirm_label: "Do it".into(),
        };
        assert!(matches!(c.ask(req()), ConfirmResult::Pending));
        assert!(matches!(c.ask(req()), ConfirmResult::Pending), "still open");
        c.answer("k", true);
        assert!(matches!(c.ask(req()), ConfirmResult::Accepted(_)));
        assert!(
            matches!(c.ask(req()), ConfirmResult::Pending),
            "a new dialog for the next operation"
        );
        c.answer("k", false);
        assert!(matches!(c.ask(req()), ConfirmResult::Declined));
    }

    #[test]
    fn guarded_write_backs_up_first_and_refuses_without_a_backup() {
        let d = tmp("write");
        let f = d.join("image.bin");
        std::fs::write(&f, b"OLD-CONTENTS").unwrap();
        let r = guarded_write(Ack { target: None }, &f, b"NEW").unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"NEW");
        let b = r.backup.expect("backup of the old contents");
        assert_eq!(std::fs::read(&b.path).unwrap(), b"OLD-CONTENTS");
        assert!(b.path.starts_with(d.join("opentune-vault")));
        // A new file needs no backup.
        let n = d.join("new.bin");
        assert!(guarded_write(Ack { target: None }, &n, b"X")
            .unwrap()
            .backup
            .is_none());
        // An unreadable target (a directory) is refused before anything is written.
        let dir_target = d.join("adir");
        std::fs::create_dir_all(&dir_target).unwrap();
        assert!(guarded_write(Ack { target: None }, &dir_target, b"X").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn public_default_uploads_nothing_and_has_no_adapter() {
        let c = NativeConfig::default();
        assert!(c.diagnostics.is_none());
        assert!(
            c.seedkeys.is_empty(),
            "OpenTune ships no seed/key algorithm"
        );
        assert!(c.adapter.is_none());
        assert!(c.views.is_empty() && c.checksums.is_empty());
        assert_eq!(c.branding.name, "OpenTune");
    }

    #[test]
    fn unlocker_follows_the_definition_and_the_resolvers() {
        fn resolver(id: &str) -> Option<Box<dyn SeedKey>> {
            struct K;
            impl SeedKey for K {
                fn id(&self) -> &str {
                    "test-k"
                }
                fn compute(&self, s: &[u8], _l: u8) -> opentune_core::Result<Vec<u8>> {
                    Ok(s.to_vec())
                }
            }
            (id == "test-k").then(|| Box::new(K) as Box<dyn SeedKey>)
        }
        let mk = |seedkey: Option<&str>| {
            let mut d = EcuDef::from_json(
                r#"{"id":"t","name":"T","maker":"x","family":"x","protocol":"iso14230_kline","diag_layer":"kwp2000","bitrate":10400,"flash_size":0,"regions":[],"full_read":"obd"}"#,
            )
            .unwrap();
            d.seedkey = seedkey.map(str::to_string);
            d
        };
        let defs = vec![mk(None), mk(Some("test-k")), mk(Some("vendor-y"))];
        let ctx = egui::Context::default();
        let jobs = Jobs::new(ctx);
        let mut confirms = Confirms::default();
        let (mut actions, mut status) = (Vec::new(), None);
        let keys = opentune_hub::seedkeys::SeedKeys::new(vec![resolver]);
        let mut on = |def_idx: Option<usize>| {
            let h = Host {
                tokens: crate::theme::DARK,
                image_path: None,
                image: None,
                image_edited: false,
                defs: &defs,
                def_idx,
                map_idx: 0,
                checksums: &[],
                adapter: None,
                diagnostics: None,
                seedkeys: &keys,
                target: Ok(DiagTarget::ENGINE),
                jobs: &jobs,
                confirms: &mut confirms,
                actions: &mut actions,
                status: &mut status,
                shared: &HostShared::default(),
            };
            (
                h.unlocker().map(|u| u.id().to_string()),
                h.unlocker_for("test-k").is_ok(),
            )
        };
        assert!(on(None).0.unwrap_err().contains("select an ECU definition"));
        assert!(on(Some(0)).0.unwrap_err().contains("names no seed/key"));
        assert_eq!(on(Some(1)).0, Ok("test-k".to_string()));
        assert!(on(Some(2)).0.unwrap_err().contains("vendor-y"));
        assert!(on(None).1);
    }

    #[test]
    fn target_resolution_and_display() {
        let d = |tx, rx| {
            let mut d = EcuDef::from_json(
                r#"{"id":"t","name":"T","maker":"x","family":"x","protocol":"iso14230_kline","diag_layer":"kwp2000","bitrate":10400,"flash_size":0,"regions":[],"full_read":"obd"}"#,
            )
            .unwrap();
            d.tx_id = tx;
            d.rx_id = rx;
            d
        };
        let tcu = DiagTarget {
            tx: 0x7E1,
            rx: 0x7E9,
        };
        assert_eq!(resolve_target("", "", None), Ok(DiagTarget::ENGINE));
        assert_eq!(resolve_target(" ", "", Some(&d(0x7E1, 0x7E9))), Ok(tcu));
        assert_eq!(
            resolve_target("", "", Some(&d(1, 0))),
            Ok(DiagTarget::ENGINE),
            "a K-line def without CAN ids"
        );
        assert_eq!(
            resolve_target("0x7e1", "7E9", Some(&d(0x7E0, 0x7E8))),
            Ok(tcu),
            "typed ids win"
        );
        assert!(resolve_target("7E1", "", None).is_err(), "both or neither");
        assert!(resolve_target("xyz", "7E9", None).is_err());
        assert!(
            resolve_target("3FFFFFFF", "7E9", None).is_err(),
            "> 29 bits"
        );
        assert_eq!(tcu.to_string(), "tx 0x7E1 / rx 0x7E9");
        assert_eq!(
            DiagTarget {
                tx: 0x18DA10F1,
                rx: 0x18DAF110
            }
            .to_string(),
            "tx 0x18DA10F1 / rx 0x18DAF110"
        );
    }

    /// The Ack carries the module shown when the dialog opened, even if the
    /// global target changes before the user clicks.
    #[test]
    fn ecu_confirm_stamps_the_shown_target() {
        let ctx = egui::Context::default();
        let jobs = Jobs::new(ctx);
        let mut confirms = Confirms::default();
        let mut actions = Vec::new();
        let mut status = None;
        let tcu = DiagTarget {
            tx: 0x7E1,
            rx: 0x7E9,
        };
        let req = || ConfirmRequest {
            key: "flash-tcu".into(),
            title: "Write".into(),
            body: "b".into(),
            typed: None,
            confirm_label: "Write".into(),
        };
        let mut host = |target: Result<DiagTarget, String>,
                        confirms: &mut Confirms,
                        f: &mut dyn FnMut(&mut Host) -> ConfirmResult| {
            let mut h = Host {
                tokens: crate::theme::DARK,
                image_path: None,
                image: None,
                image_edited: false,
                defs: &[],
                def_idx: None,
                map_idx: 0,
                checksums: &[],
                adapter: None,
                diagnostics: None,
                seedkeys: &opentune_hub::seedkeys::SeedKeys::default(),
                target,
                jobs: &jobs,
                confirms,
                actions: &mut actions,
                status: &mut status,
                shared: &HostShared::default(),
            };
            f(&mut h)
        };
        let r = host(Ok(tcu), &mut confirms, &mut |h| h.confirm_ecu(req()));
        assert!(matches!(r, ConfirmResult::Pending));
        confirms.answer("flash-tcu", true);
        // The user switched the global target to the engine meanwhile.
        let r = host(Ok(DiagTarget::ENGINE), &mut confirms, &mut |h| {
            h.confirm_ecu(req())
        });
        match r {
            ConfirmResult::Accepted(ack) => assert_eq!(ack.ecu_target(), Ok(tcu)),
            other => panic!("{other:?}"),
        }
        // An unusable target never opens a dialog.
        let r = host(Err("half set".into()), &mut confirms, &mut |h| {
            h.confirm_ecu(req())
        });
        assert!(matches!(r, ConfirmResult::Declined));
        // A plain confirm's Ack cannot drive a mutation.
        let r = host(Ok(tcu), &mut confirms, &mut |h| h.confirm(req()));
        assert!(matches!(r, ConfirmResult::Pending));
        confirms.answer("flash-tcu", true);
        if let ConfirmResult::Accepted(ack) =
            host(Ok(tcu), &mut confirms, &mut |h| h.confirm(req()))
        {
            assert!(ack.ecu_target().is_err());
        } else {
            panic!("expected Accepted");
        }
    }

    #[test]
    fn raw_transport_runs_the_core_clients() {
        use opentune_core::protocol::UdsClient;
        let mut sent: Vec<Vec<u8>> = Vec::new();
        let mut raw = |req: &[u8]| -> Result<Vec<u8>, String> {
            sent.push(req.to_vec());
            match req {
                [0x22, 0xF1, 0x87] => Ok(b"\x62\xF1\x87PART".to_vec()),
                [0x22, ..] => Ok(vec![0x7F, 0x22, 0x31]),
                [0x31, ..] => Ok(vec![0x7F, 0x31, 0x78]),
                [0x3E, ..] => Err(format!(
                    "{}adapter reset",
                    opentune_core::resilient::LINK_LOST_PREFIX
                )),
                _ => Err("timeout".into()),
            }
        };
        {
            let mut t = RawTransport::new(&mut raw);
            let mut uds = UdsClient::new(&mut t);
            assert_eq!(uds.read_data_by_id(0xF187).unwrap(), b"PART");
            let e = uds.read_data_by_id(0xF190).unwrap_err();
            assert!(
                matches!(e, opentune_core::Error::NegativeResponse { code: 0x31, .. }),
                "{e:?}"
            );
            use opentune_core::protocol::DiagTransport;
            let e = t.request(&[0x31, 0x01, 0xFF, 0x00], 100).unwrap_err();
            assert!(e.to_string().contains("not re-sent"), "{e}");
            let e = t.request(&[0x3E, 0x00], 100).unwrap_err();
            assert!(opentune_core::resilient::is_link_lost(&e), "{e:?}");
        }
        // The pending routine was sent exactly once.
        assert_eq!(sent.iter().filter(|r| r[0] == 0x31).count(), 1);
    }

    /// A mock adapter: answers every request with a fixed reply and counts
    /// mutating calls (which can only happen with an Ack + BackupReceipt).
    struct MockAdapter {
        mutations: Mutex<u32>,
        /// Every module an operation addressed.
        targets: Mutex<Vec<DiagTarget>>,
    }
    impl AdapterAccess for MockAdapter {
        fn link(&self) -> LinkInfo {
            LinkInfo {
                driver: "mock".into(),
                port: "mem".into(),
                connected: true,
                battery_v: Some(12.6),
                target: None,
            }
        }
        fn read_job(
            &self,
            _op: &str,
            target: DiagTarget,
            job: DiagJob,
        ) -> Result<Box<dyn Any + Send>, String> {
            self.targets.lock().unwrap().push(target);
            // Run on another thread, as a real adapter worker would.
            std::thread::spawn(move || {
                let mut diag = |req: &[u8]| Ok::<Vec<u8>, String>(vec![req[0] + 0x40]);
                job(&mut diag)
            })
            .join()
            .map_err(|_| "job panicked".to_string())?
        }
        fn mutate_job(
            &self,
            op: &str,
            ack: Ack,
            backup: &BackupReceipt,
            job: DiagJob,
        ) -> Result<Box<dyn Any + Send>, String> {
            assert!(backup.path.exists(), "{op}: backup must exist");
            let target = ack.ecu_target()?;
            *self.mutations.lock().unwrap() += 1;
            self.read_job(op, target, job)
        }
    }

    #[test]
    fn adapter_reads_in_a_job_and_mutates_only_with_ack_and_backup() {
        let jobs = Jobs::new(egui::Context::default());
        let mock = Arc::new(MockAdapter {
            mutations: Mutex::new(0),
            targets: Mutex::new(Vec::new()),
        });
        let adapter: Arc<dyn AdapterAccess> = mock.clone();
        let a = adapter.clone();
        let mut h = jobs.spawn("identify", move |p| {
            // A clone of the sink travels into the adapter closure (which runs
            // on the adapter's own thread) and reports from there.
            let inner = p.clone();
            let reply = a.read("identify", DiagTarget::ENGINE, move |d| {
                inner.set(0.5, "block 1 of 2");
                let r = d.request(&[0x22, 0xF1, 0x90])?;
                Ok((r, inner.cancelled()))
            })?;
            Ok(reply)
        });
        let (reply, cancelled) = wait(&mut h).unwrap();
        assert_eq!(reply, vec![0x62]);
        assert!(!cancelled);
        assert_eq!(h.progress(), (0.5, "block 1 of 2".to_string()));
        assert_eq!(adapter.link().battery_v, Some(12.6));

        let d = tmp("mut");
        let backup = backup_into_vault(&d, "flash before write", b"ECU-DATA").unwrap();
        adapter
            .mutate("write", Ack { target: None }, &backup, |dg| {
                dg.request(&[0x2E]).map(|_| ())
            })
            .expect_err("a file-write Ack names no module");
        assert_eq!(*mock.mutations.lock().unwrap(), 0);
        let tcu = DiagTarget {
            tx: 0x7E1,
            rx: 0x7E9,
        };
        adapter
            .mutate("write", Ack { target: Some(tcu) }, &backup, |dg| {
                dg.request(&[0x2E])
            })
            .map(|r: Vec<u8>| assert_eq!(r, vec![0x6E]))
            .unwrap();
        assert_eq!(*mock.mutations.lock().unwrap(), 1);
        assert_eq!(mock.targets.lock().unwrap().last(), Some(&tcu));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A plugin view as an embedding build would write it.
    struct DummyView {
        seen_len: Option<usize>,
        seen_def: Option<String>,
        job: Option<JobHandle<u32>>,
        confirm: Option<ConfirmResult>,
    }
    impl NativeView for DummyView {
        fn id(&self) -> &str {
            "dummy"
        }
        fn label(&self) -> &str {
            "Dummy"
        }
        fn count(&self) -> Option<String> {
            Some("1".into())
        }
        fn ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
            theme::pill(ui, host.tokens(), "dummy", theme::Tone::Muted, false);
            self.seen_len = host.image().map(<[u8]>::len);
            self.seen_def = host.definition().map(|d| d.id.clone());
            if self.job.is_none() {
                self.job = Some(host.spawn_job("count", |p| {
                    p.set(1.0, "counted");
                    Ok(7)
                }));
            }
            self.confirm = Some(host.confirm(ConfirmRequest {
                key: "dummy-write".into(),
                title: "Write?".into(),
                body: "test".into(),
                typed: Some("WRITE".into()),
                confirm_label: "Write".into(),
            }));
            host.status("dummy ran");
            host.act(HostAction::HexJump(0x10));
        }
    }

    #[test]
    fn dummy_view_runs_against_the_host() {
        let ctx = egui::Context::default();
        crate::theme::setup(&ctx, crate::theme::ThemeMode::Dark);
        let jobs = Jobs::new(ctx.clone());
        let mut confirms = Confirms::default();
        let mut actions = Vec::new();
        let mut status = None;
        let image = vec![0u8; 64];
        let defs: Vec<EcuDef> = Vec::new();
        let mut view = DummyView {
            seen_len: None,
            seen_def: None,
            job: None,
            confirm: None,
        };
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let mut host = Host {
                image_edited: false,
                tokens: crate::theme::DARK,
                image_path: Some(Path::new("/tmp/x.bin")),
                image: Some(&image),
                defs: &defs,
                def_idx: None,
                map_idx: 0,
                checksums: &[],
                adapter: None,
                diagnostics: None,
                seedkeys: &opentune_hub::seedkeys::SeedKeys::default(),
                target: Ok(DiagTarget::ENGINE),
                jobs: &jobs,
                confirms: &mut confirms,
                actions: &mut actions,
                status: &mut status,
                shared: &HostShared::default(),
            };
            view.ui(ui, &mut host);
        });
        // Headless: nothing uploads the font atlas.
        out.textures_delta.clear();
        assert_eq!(view.seen_len, Some(64));
        assert_eq!(view.seen_def, None);
        assert!(
            matches!(view.confirm, Some(ConfirmResult::Pending)),
            "a typed confirm opens, never auto-accepts"
        );
        assert_eq!(actions, vec![HostAction::HexJump(0x10)]);
        assert_eq!(status.as_deref(), Some("dummy ran"));
        assert_eq!(wait(view.job.as_mut().unwrap()), Ok(7));
        assert_eq!(view.count().as_deref(), Some("1"));
        assert_eq!(view.group(), ViewGroup::Custom("Plugins".into()));
    }

    #[test]
    fn checksum_resolvers_come_before_the_core() {
        fn r(id: &str) -> Option<Box<dyn ChecksumAlgo>> {
            (id == "plugin-algo")
                .then(|| opentune_checksum::get("crc32-tail"))
                .flatten()
        }
        assert!(resolve_checksum(&[r], "plugin-algo").is_some());
        assert!(
            resolve_checksum(&[r], "edc15c6").is_some(),
            "core still resolves"
        );
        assert!(resolve_checksum(&[], "plugin-algo").is_none());
    }

    // -----------------------------------------------------------------------
    // Edge cases of the safety seams
    // -----------------------------------------------------------------------

    /// What a `Host` borrows, owned by a test.
    struct Fixture {
        jobs: Jobs,
        confirms: Confirms,
        actions: Vec<HostAction>,
        status: Option<String>,
        keys: opentune_hub::seedkeys::SeedKeys,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                jobs: Jobs::new(egui::Context::default()),
                confirms: Confirms::default(),
                actions: Vec::new(),
                status: None,
                keys: opentune_hub::seedkeys::SeedKeys::default(),
            }
        }

        /// Run `f` against a host whose global target is `target`.
        fn host<R>(
            &mut self,
            target: Result<DiagTarget, String>,
            f: impl FnOnce(&mut Host<'_>) -> R,
        ) -> R {
            let mut h = Host {
                tokens: crate::theme::DARK,
                image_path: None,
                image: None,
                image_edited: false,
                defs: &[],
                def_idx: None,
                map_idx: 0,
                checksums: &[],
                adapter: None,
                diagnostics: None,
                seedkeys: &self.keys,
                target,
                jobs: &self.jobs,
                confirms: &mut self.confirms,
                actions: &mut self.actions,
                status: &mut self.status,
                shared: &HostShared::default(),
            };
            f(&mut h)
        }
    }

    fn req(key: &str) -> ConfirmRequest {
        ConfirmRequest {
            key: key.into(),
            title: "t".into(),
            body: "b".into(),
            typed: None,
            confirm_label: "ok".into(),
        }
    }

    const TCU: DiagTarget = DiagTarget {
        tx: 0x7E1,
        rx: 0x7E9,
    };

    #[test]
    fn job_progress_is_clamped_and_finished_jobs_leave_the_list() {
        let jobs = Jobs::new(egui::Context::default());
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let mut h = jobs.spawn("clamp", move |p| {
            p.set(1.5, "over");
            go_rx.recv().ok();
            p.set(-0.25, "under");
            Ok(())
        });
        assert_eq!(h.name(), "clamp");
        // Wait for the first report, then look while the job is parked.
        let t0 = Instant::now();
        while h.progress().1 != "over" {
            assert!(t0.elapsed() < Duration::from_secs(5), "no progress");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(h.progress(), (1.0, "over".to_string()));
        assert!(h.is_running());
        assert_eq!(
            jobs.active(),
            vec![("clamp".to_string(), 1.0, "over".into())]
        );
        go_tx.send(()).unwrap();
        assert_eq!(wait(&mut h), Ok(()));
        assert_eq!(h.progress(), (0.0, "under".to_string()));
        assert!(h.take().is_none(), "the result is handed out once");
        assert!(h.poll().is_none());
        assert!(jobs.active().is_empty(), "a finished job is not listed");
    }

    #[test]
    fn a_panicking_job_is_reported_and_unlisted_and_a_dropped_handle_is_harmless() {
        let jobs = Jobs::new(egui::Context::default());
        // A String payload (format!) as well as a &str one.
        let n = 3;
        let mut p = jobs.spawn::<u8, _>("panics", move |_| panic!("synthetic panic {n}"));
        assert_eq!(wait(&mut p), Err("the job panicked".to_string()));
        assert!(jobs.active().iter().all(|(name, ..)| name != "panics"));

        // A handle dropped while its job runs: the job still ends cleanly and
        // leaves the list.
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let h = jobs.spawn("orphan", move |p| {
            go_rx.recv().ok();
            p.set(0.5, "still here");
            done_tx.send(()).ok();
            Ok(1u8)
        });
        drop(h);
        go_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let t0 = Instant::now();
        while jobs.active().iter().any(|(name, ..)| name == "orphan") {
            assert!(t0.elapsed() < Duration::from_secs(5), "orphan still listed");
            std::thread::sleep(Duration::from_millis(2));
        }
        // Jobs keep working after a panic.
        let mut ok = jobs.spawn("after", |_| Ok("fine"));
        assert_eq!(wait(&mut ok), Ok("fine"));
    }

    #[test]
    fn a_cancel_before_the_job_looks_is_seen_and_is_idempotent() {
        let jobs = Jobs::new(egui::Context::default());
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let mut h = jobs.spawn("c", move |p| {
            go_rx.recv().ok();
            Ok(p.cancelled())
        });
        h.cancel();
        h.cancel();
        go_tx.send(()).unwrap();
        assert_eq!(wait(&mut h), Ok(true));
        // A job never cancelled sees false.
        let mut h = jobs.spawn("n", |p| Ok(p.cancelled()));
        assert_eq!(wait(&mut h), Ok(false));
    }

    #[test]
    fn confirm_keys_are_independent() {
        let mut c = Confirms::default();
        assert!(matches!(c.ask(req("a")), ConfirmResult::Pending));
        assert!(matches!(
            c.ask_with(req("b"), Some(TCU)),
            ConfirmResult::Pending
        ));
        c.answer("nope", true); // an unknown key changes nothing
        c.answer("a", false);
        assert!(matches!(c.ask(req("b")), ConfirmResult::Pending), "b open");
        assert!(matches!(c.ask(req("a")), ConfirmResult::Declined));
        c.answer("b", true);
        // `b` carries the target it was opened with, even when re-asked
        // without one.
        match c.ask(req("b")) {
            ConfirmResult::Accepted(ack) => {
                assert_eq!(ack.target(), Some(TCU));
                assert_eq!(ack.ecu_target(), Ok(TCU));
            }
            other => panic!("{other:?}"),
        }
        assert!(c.open.is_empty(), "both dialogs are closed");
        // A declined ECU confirmation hands out nothing.
        c.ask_with(req("e"), Some(TCU));
        c.answer("e", false);
        assert!(matches!(
            c.ask_with(req("e"), Some(TCU)),
            ConfirmResult::Declined
        ));
    }

    #[test]
    fn confirm_ecu_with_an_unusable_target_opens_nothing_and_says_why() {
        let mut fx = Fixture::new();
        let r = fx.host(Err("tx set, rx empty".into()), |h| h.confirm_ecu(req("w")));
        assert!(matches!(r, ConfirmResult::Declined));
        assert!(fx.confirms.open.is_empty(), "no dialog was opened");
        let s = fx.status.clone().unwrap();
        assert!(
            s.contains("no target module") && s.contains("tx set, rx empty"),
            "{s}"
        );
        // A 29-bit target is stamped exactly as shown.
        let ext = DiagTarget {
            tx: 0x18DA_10F1,
            rx: 0x18DA_F110,
        };
        assert!(matches!(
            fx.host(Ok(ext), |h| h.confirm_ecu(req("w"))),
            ConfirmResult::Pending
        ));
        assert_eq!(fx.confirms.open["w"].3, Some(ext), "the dialog shows it");
        fx.confirms.answer("w", true);
        match fx.host(Ok(DiagTarget::ENGINE), |h| h.confirm_ecu(req("w"))) {
            ConfirmResult::Accepted(ack) => assert_eq!(ack.ecu_target(), Ok(ext)),
            other => panic!("{other:?}"),
        }
        // A plain confirm through the host carries no target.
        fx.host(Ok(ext), |h| h.confirm(req("f")));
        fx.confirms.answer("f", true);
        match fx.host(Ok(ext), |h| h.confirm(req("f"))) {
            ConfirmResult::Accepted(ack) => assert_eq!(ack.target(), None),
            other => panic!("{other:?}"),
        }
    }

    /// A view that was told `Declined` (the target became unusable while its
    /// dialog was open) must not later get an `Accepted` for that old dialog
    /// without the user seeing a new one.
    #[test]
    fn a_declined_ecu_confirmation_leaves_no_acceptable_dialog_behind() {
        let mut fx = Fixture::new();
        assert!(matches!(
            fx.host(Ok(TCU), |h| h.confirm_ecu(req("w"))),
            ConfirmResult::Pending
        ));
        // The target becomes unusable; the view is told Declined and stops.
        assert!(matches!(
            fx.host(Err("half set".into()), |h| h.confirm_ecu(req("w"))),
            ConfirmResult::Declined
        ));
        // The dialog the view gave up on is still drawn; the user clicks it.
        fx.confirms.answer("w", true);
        // Later the user starts a new write with the engine selected.
        let r = fx.host(Ok(DiagTarget::ENGINE), |h| h.confirm_ecu(req("w")));
        assert!(
            matches!(r, ConfirmResult::Pending),
            "a new operation must open a new dialog, got {r:?}"
        );
    }

    #[test]
    fn guarded_write_receipt_matches_the_old_contents_and_leaves_no_temp_file() {
        let d = tmp("write-edge");
        let f = d.join("image.bin");
        std::fs::write(&f, b"FIRST").unwrap();
        let r1 = guarded_write(Ack { target: None }, &f, b"SECOND").unwrap();
        let b1 = r1.backup.unwrap();
        assert_eq!(r1.target, f);
        assert_eq!(
            (b1.len, b1.crc32),
            (5, crate::views::vault::crc32(b"FIRST"))
        );
        assert!(!f.with_extension("opentune-tmp").exists(), "no temp file");
        // A second write archives what the first one wrote.
        let r2 = guarded_write(Ack { target: Some(TCU) }, &f, b"").unwrap();
        let b2 = r2.backup.unwrap();
        assert_eq!(std::fs::read(&b2.path).unwrap(), b"SECOND");
        assert_eq!(std::fs::read(&f).unwrap(), b"", "an empty write is a write");
        // A target in a missing directory: refused, nothing created.
        let lost = d.join("missing-dir").join("x.bin");
        assert!(guarded_write(Ack { target: None }, &lost, b"X").is_err());
        assert!(!d.join("missing-dir").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Two guarded writes of the same file in quick succession (e.g. Save,
    /// then Save again): the vault must still hold the ORIGINAL contents.
    #[test]
    #[ignore = "BUG: vault backups are named <label>_<address>_<unix seconds>, so a second backup with the same label within one second overwrites the first (the original contents are lost)"]
    fn two_writes_in_one_second_keep_both_vault_copies() {
        let d = tmp("write-twice");
        let f = d.join("image.bin");
        std::fs::write(&f, b"ORIGINAL").unwrap();
        let b1 = guarded_write(Ack { target: None }, &f, b"EDIT-1")
            .unwrap()
            .backup
            .unwrap();
        let b2 = guarded_write(Ack { target: None }, &f, b"EDIT-2")
            .unwrap()
            .backup
            .unwrap();
        let (r1, r2) = (std::fs::read(&b1.path), std::fs::read(&b2.path));
        let _ = std::fs::remove_dir_all(&d);
        assert_ne!(b1.path, b2.path, "each write gets its own vault copy");
        assert_eq!(r1.unwrap(), b"ORIGINAL", "the original is kept");
        assert_eq!(r2.unwrap(), b"EDIT-1");
    }

    fn def_with_ids(tx: u32, rx: u32) -> EcuDef {
        let mut d = EcuDef::from_json(
            r#"{"id":"t","name":"T","maker":"x","family":"x","protocol":"iso14230_kline","diag_layer":"kwp2000","bitrate":10400,"flash_size":0,"regions":[],"full_read":"obd"}"#,
        )
        .unwrap();
        d.tx_id = tx;
        d.rx_id = rx;
        d
    }

    #[test]
    fn resolve_target_hex_forms_and_limits() {
        let ext = DiagTarget {
            tx: 0x18DA_10F1,
            rx: 0x18DA_F110,
        };
        // 29-bit ids with surrounding whitespace, lower case, both prefixes.
        assert_eq!(resolve_target("  18DA10F1\t", "\n18DAF110 ", None), Ok(ext));
        assert_eq!(resolve_target("18da10f1", "0x18daf110", None), Ok(ext));
        assert_eq!(resolve_target("0X18DA10F1", "0X18dAf110", None), Ok(ext));
        assert_eq!(resolve_target(" 0x7e1 ", "0X7E9", None), Ok(TCU));
        // The 29-bit maximum is accepted, one more is not.
        assert_eq!(
            resolve_target("1FFFFFFF", "0", None),
            Ok(DiagTarget {
                tx: 0x1FFF_FFFF,
                rx: 0
            })
        );
        for bad in [
            "20000000",
            "0x",
            "0X",
            "0x0x7E1",
            "7E 1",
            "000000007E1",
            "7E1h",
            "-7E1",
        ] {
            let e = resolve_target(bad, "7E9", None).unwrap_err();
            assert!(e.contains("not a hex CAN id"), "{bad}: {e}");
            assert!(e.contains(bad.trim()), "names the input: {e}");
            assert!(resolve_target("7E1", bad, None).is_err(), "rx {bad}");
        }
        // Whitespace-only counts as empty: neither set means the fallback.
        assert_eq!(resolve_target(" \t", "\n", None), Ok(DiagTarget::ENGINE));
        assert!(resolve_target("", " 7E9 ", None).is_err(), "rx only");
        // A definition with 29-bit ids is used; one beyond 29 bits is not.
        assert_eq!(
            resolve_target("", "", Some(&def_with_ids(0x18DA_10F1, 0x18DA_F110))),
            Ok(ext)
        );
        assert_eq!(
            resolve_target("", "", Some(&def_with_ids(0x2000_0000, 0x7E8))),
            Ok(DiagTarget::ENGINE)
        );
        assert_eq!(
            resolve_target("", "", Some(&def_with_ids(0x7E1, 0))),
            Ok(DiagTarget::ENGINE),
            "half a definition is not a target"
        );
        // Display switches to 8 digits above 11 bits.
        let t = |tx, rx| DiagTarget { tx, rx }.to_string();
        assert_eq!(t(0x7FF, 0x800), "tx 0x7FF / rx 0x00000800");
        assert_eq!(t(0, 1), "tx 0x000 / rx 0x001");
        assert_eq!(DiagTarget::default(), DiagTarget::ENGINE);
    }

    #[test]
    fn resolve_target_refuses_a_definition_rx_beyond_29_bits() {
        assert_eq!(
            resolve_target("", "", Some(&def_with_ids(0x7E1, 0xFFFF_FFFF))),
            Ok(DiagTarget::ENGINE)
        );
    }

    #[test]
    fn raw_transport_edge_replies() {
        use opentune_core::protocol::{DiagTransport, UdsClient};
        let mut sent: Vec<Vec<u8>> = Vec::new();
        let mut raw = |req: &[u8]| -> Result<Vec<u8>, String> {
            sent.push(req.to_vec());
            match req {
                // A pending reply for ANOTHER service: not this request's 0x78.
                [0x22, 0xF1, 0x90] => Ok(vec![0x7F, 0x31, 0x78]),
                // An NRC for another service.
                [0x22, 0xF1, 0x91] => Ok(vec![0x7F, 0x2E, 0x31]),
                [0x22, 0xF1, 0x92] => Ok(vec![]),
                // A truncated negative response.
                [0x22, 0xF1, 0x93] => Ok(vec![0x7F, 0x22]),
                // A pending reply that names this service but is padded.
                [0x2E, ..] => Ok(vec![0x7F, 0x2E, 0x78, 0x00]),
                _ => Err("plain transport failure".into()),
            }
        };
        {
            let mut t = RawTransport::new(&mut raw);
            // The bridge passes other services' replies through untouched.
            assert_eq!(
                t.request(&[0x22, 0xF1, 0x90], 1).unwrap(),
                vec![0x7F, 0x31, 0x78]
            );
            assert_eq!(t.request(&[0x22, 0xF1, 0x92], 1).unwrap(), Vec::<u8>::new());
            let e = t.request(&[0x2E, 0x01, 0x02, 0xAA], 1).unwrap_err();
            assert!(e.to_string().contains("not re-sent"), "{e}");
            assert!(e.to_string().contains("0x2E"), "{e}");
            let e = t.request(&[0x10, 0x01], 1).unwrap_err();
            assert!(
                matches!(&e, opentune_core::Error::Protocol(m) if m == "plain transport failure"),
                "{e:?}"
            );
            assert!(!opentune_core::resilient::is_link_lost(&e));
            // Through the client: each is an error, never a value.
            let mut uds = UdsClient::new(&mut t);
            for did in [0xF190u16, 0xF191, 0xF192, 0xF193] {
                let e = uds.read_data_by_id(did).unwrap_err();
                assert!(
                    matches!(e, opentune_core::Error::Protocol(_)),
                    "{did:04X}: {e:?}"
                );
            }
        }
        // Every request went out exactly once.
        assert_eq!(sent.len(), 4 + 4);
        assert_eq!(sent.iter().filter(|r| r[0] == 0x2E).count(), 1);
    }

    #[test]
    fn mutation_scopes_nest_and_restore_even_on_panic() {
        assert!(!in_mutation());
        {
            let _outer = MutationScope::enter();
            assert!(in_mutation());
            {
                let _inner = MutationScope::enter();
                assert!(in_mutation());
            }
            assert!(in_mutation(), "the inner scope restores the outer one");
        }
        assert!(!in_mutation());
        let r = std::panic::catch_unwind(|| {
            let _s = MutationScope::enter();
            assert!(in_mutation());
            panic!("synthetic panic inside a mutation");
        });
        assert!(r.is_err());
        assert!(!in_mutation(), "restored while unwinding");
        // Another thread never sees this thread's scope.
        let _s = MutationScope::enter();
        assert!(!std::thread::spawn(in_mutation).join().unwrap());
    }

    /// Runs every job on the calling thread (like an adapter without a
    /// worker), catching a panic as the hub does.
    struct InlineAdapter {
        reads: Mutex<u32>,
        mutations: Mutex<u32>,
    }
    impl InlineAdapter {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                reads: Mutex::new(0),
                mutations: Mutex::new(0),
            })
        }
    }
    impl AdapterAccess for InlineAdapter {
        fn link(&self) -> LinkInfo {
            LinkInfo::default()
        }
        fn read_job(
            &self,
            _op: &str,
            _target: DiagTarget,
            job: DiagJob,
        ) -> Result<Box<dyn Any + Send>, String> {
            *self.reads.lock().unwrap() += 1;
            let mut diag = |req: &[u8]| Ok::<Vec<u8>, String>(vec![req[0] + 0x40]);
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || job(&mut diag)))
                .unwrap_or_else(|_| Err("job panicked".into()))
        }
        fn mutate_job(
            &self,
            op: &str,
            ack: Ack,
            _backup: &BackupReceipt,
            job: DiagJob,
        ) -> Result<Box<dyn Any + Send>, String> {
            let t = ack.ecu_target()?;
            *self.mutations.lock().unwrap() += 1;
            let r = self.read_job(op, t, job);
            *self.reads.lock().unwrap() -= 1;
            r
        }
    }

    #[test]
    fn the_mutation_marker_follows_the_wrappers() {
        let d = tmp("scope");
        let backup = backup_into_vault(&d, "scope", b"OLD").unwrap();
        let inline = InlineAdapter::new();
        let a: Arc<dyn AdapterAccess> = inline.clone();
        // Not set during a read, set during a mutation, gone afterwards.
        assert!(!a.read("r", TCU, |_| Ok(in_mutation())).unwrap());
        assert!(a
            .mutate("m", test_ack(TCU), &backup, |_| Ok(in_mutation()))
            .unwrap());
        assert!(!in_mutation());
        assert!(
            !a.read("next", TCU, |_| Ok(in_mutation())).unwrap(),
            "not leaked into the next job on the same thread"
        );
        // A refused mutation (no target) never runs the closure.
        let ran = Arc::new(AtomicBool::new(false));
        let r2 = ran.clone();
        a.mutate("m", Ack { target: None }, &backup, move |_| {
            r2.store(true, Ordering::SeqCst);
            Ok(())
        })
        .unwrap_err();
        assert!(!ran.load(Ordering::SeqCst));
        assert!(!in_mutation());

        // A panic inside the closure: the marker is restored on this thread.
        let e = a
            .mutate("boom", test_ack(TCU), &backup, |_| -> Result<(), String> {
                assert!(in_mutation());
                panic!("synthetic panic inside mutate")
            })
            .unwrap_err();
        assert_eq!(e, "job panicked");
        assert!(!in_mutation());
        assert!(!a.read("after panic", TCU, |_| Ok(in_mutation())).unwrap());

        // A mutate inside a mutate restores the outer marker, then clears.
        let a2 = a.clone();
        let b2 = backup.clone();
        let (inner, after_inner) = a
            .mutate("outer", test_ack(TCU), &backup, move |_| {
                let inner = a2.mutate("inner", test_ack(TCU), &b2, |_| Ok(in_mutation()))?;
                Ok((inner, in_mutation()))
            })
            .unwrap();
        assert!(inner && after_inner);
        assert!(!in_mutation());
        assert_eq!(*inline.mutations.lock().unwrap(), 4);
        assert_eq!(*inline.reads.lock().unwrap(), 3);

        // On an adapter that runs jobs on another thread, the same holds.
        let mock = Arc::new(MockAdapter {
            mutations: Mutex::new(0),
            targets: Mutex::new(Vec::new()),
        });
        let m: Arc<dyn AdapterAccess> = mock;
        assert!(!m.read("r", TCU, |_| Ok(in_mutation())).unwrap());
        assert!(m
            .mutate("w", test_ack(TCU), &backup, |_| Ok(in_mutation()))
            .unwrap());
        assert!(!m.read("r", TCU, |_| Ok(in_mutation())).unwrap());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_unexpected_result_type_is_an_error_not_a_panic() {
        struct WrongType;
        impl AdapterAccess for WrongType {
            fn link(&self) -> LinkInfo {
                LinkInfo::default()
            }
            fn read_job(
                &self,
                _op: &str,
                _t: DiagTarget,
                _job: DiagJob,
            ) -> Result<Box<dyn Any + Send>, String> {
                Ok(Box::new("not a u32"))
            }
            fn mutate_job(
                &self,
                _op: &str,
                _ack: Ack,
                _b: &BackupReceipt,
                _job: DiagJob,
            ) -> Result<Box<dyn Any + Send>, String> {
                Err("refused".into())
            }
        }
        let a: Arc<dyn AdapterAccess> = Arc::new(WrongType);
        let e = a.read("r", TCU, |_| Ok(7u32)).unwrap_err();
        assert!(e.contains("unexpected result type"), "{e}");
    }

    /// An adapter written before actuate / start / scan / raw (only the
    /// three original methods) still compiles and refuses them.
    #[test]
    fn older_adapters_refuse_the_newer_operations() {
        let mock = Arc::new(MockAdapter {
            mutations: Mutex::new(0),
            targets: Mutex::new(Vec::new()),
        });
        let a: Arc<dyn AdapterAccess> = mock.clone();
        assert_eq!(a.capabilities(), None);
        let e = a
            .actuate("x", test_ack(TCU), |_d| Ok(()), |_d| Ok(()))
            .unwrap_err();
        assert_eq!(e, UNSUPPORTED);
        let e = a.start("x", TCU, |_d, _s| Ok(())).err().unwrap();
        assert_eq!(e, UNSUPPORTED);
        assert_eq!(a.scan("x", |_m| Ok(())).unwrap_err(), UNSUPPORTED);
        let e = a.raw("x", RawBinding::new(TCU), |_i| Ok(())).unwrap_err();
        assert_eq!(e, UNSUPPORTED);
        assert!(mock.targets.lock().unwrap().is_empty(), "nothing ran");
    }

    #[test]
    fn the_host_exposes_channel_data_roots_and_shared_state() {
        let mut f = Fixture::new();
        f.host(Ok(TCU), |h| {
            assert_eq!(h.channel().as_str(), BuildChannel::STABLE);
            assert!(!h.bench_enabled());
            assert_eq!(h.capabilities(), None);
            // From native/ (cargo test) the repository's folders are found.
            if Path::new("../ecudefs").is_dir() {
                assert!(h.data_dir(DataRoot::EcuDefs).is_some());
                assert!(h.data_dir(DataRoot::Packs).is_some());
                assert!(h.data_dir(DataRoot::Guide).is_some());
            }
            // Clones of the feed and the GPS source are the app's one each.
            let rx = h.live_feed().subscribe();
            h.live_feed()
                .publish(FeedEvent::Stop { source: "t".into() });
            assert!(rx.try_recv().is_ok());
            assert!(!h.gps().is_open());
            h.act(HostAction::AddDefinition(Box::new(def_with_ids(
                0x7E1, 0x7E9,
            ))));
        });
        assert_eq!(
            f.actions,
            vec![HostAction::AddDefinition(Box::new(def_with_ids(
                0x7E1, 0x7E9
            )))]
        );
        assert_ne!(
            f.actions[0],
            HostAction::AddDefinition(Box::new(def_with_ids(0x7E0, 0x7E8)))
        );
        for (name, on) in [
            (BuildChannel::STABLE, false),
            (BuildChannel::ALPHA, true),
            (BuildChannel::BENCH, true),
            ("beta", false),
        ] {
            assert_eq!(bench_channel(&BuildChannel::new(name)), on, "{name}");
        }
        assert_eq!(ViewGroup::Workspace.label(), "Calibration");
        assert_eq!(ViewGroup::Custom("Plugins".into()).label(), "Plugins");
    }

    // -----------------------------------------------------------------------
    // Invariant: a BackupReceipt means a real, NON-EMPTY backup was written
    // -----------------------------------------------------------------------

    fn vault_files(d: &Path) -> usize {
        std::fs::read_dir(d.join("opentune-vault"))
            .map(|r| r.count())
            .unwrap_or(0)
    }

    #[test]
    fn an_empty_backup_is_refused_and_writes_nothing() {
        let d = tmp("empty-backup");
        let mut f = Fixture::new();
        let e = f
            .host(Ok(TCU), |h| h.backup(&d, "before write", &[]))
            .unwrap_err();
        assert!(e.contains("0 bytes") && e.contains("no receipt"), "{e}");
        assert_eq!(vault_files(&d), 0, "nothing reached the vault");
        // Only the length counts: all-zero bytes are a real backup.
        let r = f
            .host(Ok(TCU), |h| h.backup(&d, "before write", &[0u8; 16]))
            .unwrap();
        assert_eq!(r.len, 16);
        assert_eq!(std::fs::read(&r.path).unwrap(), vec![0u8; 16]);
        // Replacing an existing EMPTY file: nothing to lose, no receipt.
        let target = d.join("empty.bin");
        std::fs::write(&target, b"").unwrap();
        let w = guarded_write(Ack { target: None }, &target, b"NEW").unwrap();
        assert!(w.backup.is_none());
        assert_eq!(std::fs::read(&target).unwrap(), b"NEW");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn mutate_refuses_a_zero_length_receipt_before_the_adapter() {
        let d = tmp("empty-receipt");
        let mock = Arc::new(MockAdapter {
            mutations: Mutex::new(0),
            targets: Mutex::new(Vec::new()),
        });
        let a: Arc<dyn AdapterAccess> = mock.clone();
        // An in-crate receipt for an existing file but 0 bytes (only the
        // crate can build one; Host::backup never issues it).
        let path = d.join("present.bin");
        std::fs::write(&path, b"X").unwrap();
        let empty = BackupReceipt {
            path,
            crc32: 0,
            len: 0,
        };
        let e = a
            .mutate("write coding", test_ack(TCU), &empty, |dg| {
                dg.request(&[0x2E])
            })
            .unwrap_err();
        assert!(
            e.contains("empty (0 bytes)") && e.contains("nothing was sent"),
            "{e}"
        );
        assert_eq!(
            *mock.mutations.lock().unwrap(),
            0,
            "mutate_job never called"
        );
        assert!(mock.targets.lock().unwrap().is_empty(), "nothing sent");
        // actuate takes no receipt and is unaffected.
        let act = a
            .actuate("output test", test_ack(TCU), |_d| Ok(1u8), |_d| Ok(()))
            .unwrap_err();
        assert_eq!(act, UNSUPPORTED, "(the mock predates actuate)");
        let _ = std::fs::remove_dir_all(&d);
    }
}
