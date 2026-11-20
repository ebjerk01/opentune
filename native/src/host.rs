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

use crate::theme::{self, Tokens};
use eframe::egui;
use opentune_checksum::ChecksumAlgo;
use opentune_core::ecudef::EcuDef;
use opentune_core::RawDiag;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, Weak};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Looks up an extra checksum algorithm by id (consulted before the core set).
pub type ChecksumResolver = fn(&str) -> Option<Box<dyn ChecksumAlgo>>;

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

/// What an embedding build passes to [`crate::run`].
#[derive(Default)]
pub struct NativeConfig {
    pub branding: Branding,
    /// Extra checksum algorithms, consulted before the core set.
    pub checksums: Vec<ChecksumResolver>,
    /// Extra sidebar destinations.
    pub views: Vec<Box<dyn NativeView>>,
    /// The adapter the views run diagnostics through (None: no adapter layer).
    pub adapter: Option<Arc<dyn AdapterAccess>>,
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

/// Where a view appears in the sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewGroup {
    Workspace,
    Reference,
    /// A group of its own (label shown as the section header).
    Custom(String),
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

/// Handed to a job: report progress, check for cancellation.
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
    _p: (),
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
    open: BTreeMap<String, (ConfirmRequest, ConfirmState, String)>,
}

impl Confirms {
    fn ask(&mut self, req: ConfirmRequest) -> ConfirmResult {
        match self.open.get(&req.key).map(|(_, s, _)| *s) {
            Some(ConfirmState::Accepted) => {
                self.open.remove(&req.key);
                ConfirmResult::Accepted(Ack { _p: () })
            }
            Some(ConfirmState::Declined) => {
                self.open.remove(&req.key);
                ConfirmResult::Declined
            }
            Some(ConfirmState::Open) => ConfirmResult::Pending,
            None => {
                self.open
                    .insert(req.key.clone(), (req, ConfirmState::Open, String::new()));
                ConfirmResult::Pending
            }
        }
    }

    /// Draw the open dialogs (one at a time).
    pub fn ui(&mut self, ctx: &egui::Context) {
        let t = theme::tokens(ctx);
        let Some((key, (req, _, typed))) = self
            .open
            .iter_mut()
            .find(|(_, (_, s, _))| *s == ConfirmState::Open)
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
    fn answer(&mut self, key: &str, accept: bool) {
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
fn backup_into_vault(near: &Path, label: &str, data: &[u8]) -> Result<BackupReceipt, String> {
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
fn atomic_write(target: &Path, data: &[u8]) -> Result<(), String> {
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
    /// The vault copy of what the target held before (None: it did not exist).
    pub backup: Option<BackupReceipt>,
}

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

/// Snapshot of the adapter connection the top bar shows.
#[derive(Clone, Debug, Default)]
pub struct LinkInfo {
    pub driver: String,
    pub port: String,
    pub connected: bool,
    pub battery_v: Option<f32>,
}

/// Diagnostics through the app's adapter. Implementations serialize access
/// (one operation at a time) and never let a view open a driver itself.
pub trait AdapterAccess: Send + Sync {
    fn link(&self) -> LinkInfo;
    /// A read-only operation (identification, reads, live data).
    fn read(
        &self,
        op: &str,
        f: &mut dyn FnMut(&mut dyn RawDiag) -> Result<(), String>,
    ) -> Result<(), String>;
    /// An operation that changes the ECU. Needs the user's [`Ack`] and a
    /// [`BackupReceipt`] of the data it is about to change.
    fn mutate(
        &self,
        op: &str,
        ack: Ack,
        backup: &BackupReceipt,
        f: &mut dyn FnMut(&mut dyn RawDiag) -> Result<(), String>,
    ) -> Result<(), String>;
}

// ---------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------

/// Something a view asks the app to do after its `ui()` returns.
#[derive(Clone, Debug, PartialEq)]
pub enum HostAction {
    OpenImage(PathBuf),
    SelectDefinition(String),
    SelectMap(usize),
    HexJump(usize),
}

/// What a view gets every frame.
pub struct Host<'a> {
    pub(crate) tokens: Tokens,
    pub(crate) image_path: Option<&'a Path>,
    pub(crate) image: Option<&'a [u8]>,
    pub(crate) defs: &'a [EcuDef],
    pub(crate) def_idx: Option<usize>,
    pub(crate) map_idx: usize,
    pub(crate) checksums: &'a [ChecksumResolver],
    pub(crate) adapter: Option<&'a Arc<dyn AdapterAccess>>,
    pub(crate) jobs: &'a Jobs,
    pub(crate) confirms: &'a mut Confirms,
    pub(crate) actions: &'a mut Vec<HostAction>,
    pub(crate) status: &'a mut Option<String>,
}

impl<'a> Host<'a> {
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
    pub fn link(&self) -> LinkInfo {
        self.adapter.map(|a| a.link()).unwrap_or_default()
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

fn guarded_write(_ack: Ack, target: &Path, data: &[u8]) -> Result<WriteReceipt, String> {
    let backup = match std::fs::read(target) {
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
        let r = guarded_write(Ack { _p: () }, &f, b"NEW").unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"NEW");
        let b = r.backup.expect("backup of the old contents");
        assert_eq!(std::fs::read(&b.path).unwrap(), b"OLD-CONTENTS");
        assert!(b.path.starts_with(d.join("opentune-vault")));
        // A new file needs no backup.
        let n = d.join("new.bin");
        assert!(guarded_write(Ack { _p: () }, &n, b"X")
            .unwrap()
            .backup
            .is_none());
        // An unreadable target (a directory) is refused before anything is written.
        let dir_target = d.join("adir");
        std::fs::create_dir_all(&dir_target).unwrap();
        assert!(guarded_write(Ack { _p: () }, &dir_target, b"X").is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A mock adapter: answers every request with a fixed reply and counts
    /// mutating calls (which can only happen with an Ack + BackupReceipt).
    struct MockAdapter {
        mutations: Mutex<u32>,
    }
    impl AdapterAccess for MockAdapter {
        fn link(&self) -> LinkInfo {
            LinkInfo {
                driver: "mock".into(),
                port: "mem".into(),
                connected: true,
                battery_v: Some(12.6),
            }
        }
        fn read(
            &self,
            _op: &str,
            f: &mut dyn FnMut(&mut dyn RawDiag) -> Result<(), String>,
        ) -> Result<(), String> {
            let mut diag = |req: &[u8]| Ok::<Vec<u8>, String>(vec![req[0] + 0x40]);
            f(&mut diag)
        }
        fn mutate(
            &self,
            op: &str,
            _ack: Ack,
            backup: &BackupReceipt,
            f: &mut dyn FnMut(&mut dyn RawDiag) -> Result<(), String>,
        ) -> Result<(), String> {
            assert!(backup.path.exists(), "{op}: backup must exist");
            *self.mutations.lock().unwrap() += 1;
            self.read(op, f)
        }
    }

    #[test]
    fn adapter_reads_in_a_job_and_mutates_only_with_ack_and_backup() {
        let jobs = Jobs::new(egui::Context::default());
        let mock = Arc::new(MockAdapter {
            mutations: Mutex::new(0),
        });
        let adapter: Arc<dyn AdapterAccess> = mock.clone();
        let a = adapter.clone();
        let mut h = jobs.spawn("identify", move |p| {
            let mut reply = Vec::new();
            a.read("identify", &mut |d| {
                reply = d.request(&[0x22, 0xF1, 0x90])?;
                Ok(())
            })?;
            p.set(1.0, "done");
            Ok(reply)
        });
        assert_eq!(wait(&mut h), Ok(vec![0x62]));
        assert_eq!(adapter.link().battery_v, Some(12.6));

        let d = tmp("mut");
        let backup = backup_into_vault(&d, "flash before write", b"ECU-DATA").unwrap();
        adapter
            .mutate("write", Ack { _p: () }, &backup, &mut |dg| {
                dg.request(&[0x2E]).map(|_| ())
            })
            .unwrap();
        assert_eq!(*mock.mutations.lock().unwrap(), 1);
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
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            let mut host = Host {
                tokens: crate::theme::DARK,
                image_path: Some(Path::new("/tmp/x.bin")),
                image: Some(&image),
                defs: &defs,
                def_idx: None,
                map_idx: 0,
                checksums: &[],
                adapter: None,
                jobs: &jobs,
                confirms: &mut confirms,
                actions: &mut actions,
                status: &mut status,
            };
            view.ui(ui, &mut host);
        });
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
}
