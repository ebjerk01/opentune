//! Coding: read a module's coding, preview a parameter edit as a dry run, and
//! write it, over [`opentune_coding`] (port of the webview app's Coding panel,
//! `feat_coding.rs` `write_one`).
//!
//! The definition (a coding pack entry or a JSON file) names the module
//! (`req_id` / `resp_id`); reads and writes go there (else to the app
//! target). A write is confirmed with `Host::confirm_ecu_for` for exactly
//! that module, named by the definition ("<definition name> (<ids>)"); the
//! dialog warns when it is not the app's target, and the write re-checks
//! `ack.target() == module` before anything is sent.
//!
//! ## Per-op table (reviewed before merge)
//!
//! | Operation | SIDs sent (in order) | Path | Backup source |
//! |---|---|---|---|
//! | Read coding | `22 <did>` per coding block | `read` (the definition's module, else the app target) | n/a (read only) |
//! | Preview edit | `22 <did>` per block (the engine's dry run builds the plan; no `2E` is ever built into a request) | `read` | n/a (read only) |
//! | Write parameter — 1. confirm | none | `Host::confirm_ecu_for(req, module, "<definition name> (<ids>)")` (typed `WRITE`; one dialog per module; red warning when the module is not the app target) | — |
//! | Write parameter — 2. pre-read | `22 <did>` per block | `read` (the confirmed module) | — |
//! | Write parameter — 3. backup | none | `Host::backup` (refused when empty) | ALL coding blocks read in step 2, concatenated in block order (label names each DID) |
//! | Write parameter — 4. write | (`ack.target() == module` re-checked first, else nothing is sent) `10 03`; `27 <lvl>`, `27 <lvl+1> <key>` (only when a seed/key id is entered, via `Host::unlocker_for`; a rejected key is never retried); `22 <did>` per block (must equal the backup byte for byte, else NOTHING is written); `22 <did>` per block (the engine's fresh baseline); `2E <did> <block>` per CHANGED block; `22 <did>` per written block (verify); `10 01` (best effort) | `mutate` (the `Ack`'s module; receipt must match the step-2 bytes: CRC + length) | as step 3 |
//!
//! Not ported: a one-click "restore from backup" (the vault keeps the old
//! blocks; re-applying them is a normal write of the old parameter values).

pub(crate) mod plumbing;
#[cfg(test)]
pub(crate) mod simkit;

use self::plumbing as pl;
use crate::host::{
    Ack, AdapterAccess, BackupReceipt, ConfirmRequest, ConfirmResult, DiagTarget, Host, JobHandle,
    NativeView, Tier, Unlocker, ViewGroup,
};
use crate::icons;
use crate::theme::{self, Tone};
use crate::widgets::packs::{PackKind, PackPicker};
use eframe::egui::{self, RichText};
use opentune_coding::{
    decode, read_coding, set, write_coding, Coding, CodingDef, ParamKind, WriteOptions,
    WriteOutcome, WriteReport,
};
use opentune_core::RawDiag;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Bench;

pub fn create() -> Box<dyn NativeView> {
    Box::new(CodingView::default())
}

// ---------------------------------------------------------------------------
// Operations (no UI; the tests drive these)
// ---------------------------------------------------------------------------

/// A loaded coding definition.
#[derive(Clone)]
pub(crate) struct Loaded {
    pub def: CodingDef,
    /// The module the definition names, if any.
    pub module: Option<DiagTarget>,
}

pub(crate) fn load(path: &Path) -> Result<Loaded, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read coding definition {}: {e}", path.display()))?;
    let def = CodingDef::from_json(&text).map_err(|e| format!("parse coding definition: {e}"))?;
    def.validate()
        .map_err(|e| format!("invalid coding definition: {e}"))?;
    Ok(Loaded {
        def,
        module: pl::def_module(&text)?,
    })
}

fn err(e: impl ToString) -> String {
    e.to_string()
}

/// Read the coding (read path: `22` only).
pub(crate) fn read_op(
    a: &Arc<dyn AdapterAccess>,
    target: DiagTarget,
    def: CodingDef,
) -> Result<Coding, String> {
    a.read("coding read", target, move |d| {
        read_coding(d, &def).map_err(err)
    })
}

/// Dry-run one edit against the live coding (read path: `22` only).
pub(crate) fn preview_op(
    a: &Arc<dyn AdapterAccess>,
    target: DiagTarget,
    def: CodingDef,
    name: String,
    value: u64,
) -> Result<WriteReport, String> {
    a.read("coding preview", target, move |d| {
        let base = read_coding(d, &def).map_err(err)?;
        let want = set(&def, &base, &name, value).map_err(err)?;
        let dry = WriteOptions {
            dry_run: true,
            ..Default::default()
        };
        write_coding(d, &def, &base, &want, &dry).map_err(err)
    })
}

/// The bytes the vault keeps: every block, in block order.
pub(crate) fn backup_blob(c: &Coding) -> Vec<u8> {
    c.blocks.iter().flat_map(|b| b.bytes.clone()).collect()
}

/// The vault label: definition, module and the DID:length of each block.
pub(crate) fn backup_label(def: &CodingDef, module: DiagTarget, c: &Coding) -> String {
    let blocks: Vec<String> = c
        .blocks
        .iter()
        .map(|b| format!("DID{:04X}x{}", b.did, b.bytes.len()))
        .collect();
    format!("coding {} tx{:X} {}", def.id, module.tx, blocks.join(" "))
}

/// What a write needs besides the `Ack` and the receipt.
pub(crate) struct WriteArgs {
    pub def: CodingDef,
    /// The coding the vault backup holds (read in the pre-read step).
    pub old: Coding,
    pub name: String,
    pub value: u64,
    pub unlock: Option<(Unlocker, u8)>,
}

/// Write one parameter on the module the user confirmed (`mutate`). The
/// receipt must be the backup of `args.old` exactly.
pub(crate) fn write_op(
    a: &Arc<dyn AdapterAccess>,
    ack: Ack,
    module: DiagTarget,
    receipt: &BackupReceipt,
    args: WriteArgs,
) -> Result<WriteReport, String> {
    if ack.target() != Some(module) {
        return Err(format!(
            "coding write refused: the confirmation does not name module {module}; nothing \
             was sent"
        ));
    }
    let blob = backup_blob(&args.old);
    if receipt.len != blob.len() || receipt.crc32 != crate::views::vault::crc32(&blob) {
        return Err(
            "coding write refused: the backup receipt is not the backup of the coding about \
             to be replaced; nothing was sent"
                .into(),
        );
    }
    a.mutate("coding write", ack, receipt, move |d| write_body(d, args))
}

/// The write itself, inside `mutate`.
fn write_body(d: &mut dyn RawDiag, args: WriteArgs) -> Result<WriteReport, String> {
    opentune_hub::adapter::enter_extended_session(d)?;
    let r = (|| {
        if let Some((u, level)) = &args.unlock {
            u.unlock_uds(d, *level)?;
        }
        let live = read_coding(d, &args.def).map_err(err)?;
        if live != args.old {
            return Err(
                "the module's coding changed since it was backed up; nothing was written \
                 (read it again)"
                    .to_string(),
            );
        }
        let want = set(&args.def, &live, &args.name, args.value).map_err(err)?;
        write_coding(d, &args.def, &live, &want, &WriteOptions::default()).map_err(err)
    })();
    opentune_hub::adapter::leave_session(d);
    r
}

fn outcome_text(o: &WriteOutcome) -> (Tone, String) {
    match o {
        WriteOutcome::Planned => (Tone::Muted, "planned (dry run): nothing written".into()),
        WriteOutcome::NoChange => (Tone::Muted, "no change: nothing written".into()),
        WriteOutcome::Verified => (Tone::Ok, "written and verified".into()),
        WriteOutcome::Unverified => (Tone::Warn, "written, not verified".into()),
        WriteOutcome::VerifyFailed { did } => (
            Tone::Crit,
            format!("VERIFY FAILED on DID 0x{did:04X}: the module reads back something else"),
        ),
        WriteOutcome::WriteError {
            uncertain_did,
            message,
        } => (
            Tone::Crit,
            format!(
                "write error on DID 0x{uncertain_did:04X} (its state is UNKNOWN, do not retry \
                 blindly): {message}"
            ),
        ),
    }
}

// ---------------------------------------------------------------------------
// The write pipeline: confirm → pre-read → backup → mutate
// ---------------------------------------------------------------------------

pub(crate) struct Pending {
    pub module: DiagTarget,
    /// The module's name in the confirmation.
    pub label: String,
    pub name: String,
    pub value: u64,
    pub unlock: Option<(Unlocker, u8)>,
}

pub(crate) enum Step {
    Idle,
    Confirm(ConfirmRequest, Pending),
    PreRead(Ack, JobHandle<Coding>, Pending),
    Write(JobHandle<WriteReport>, BackupReceipt),
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

pub struct CodingView {
    picker: PackPicker,
    pub(crate) loaded: Option<Loaded>,
    read_job: Option<JobHandle<Coding>>,
    pub(crate) coding: Option<Coding>,
    sel: Option<usize>,
    value_text: String,
    preview_job: Option<JobHandle<WriteReport>>,
    preview: Option<WriteReport>,
    pub(crate) step: Step,
    pub(crate) last: Option<Result<WriteReport, String>>,
    pub(crate) last_backup: Option<PathBuf>,
    pub(crate) security: pl::SecurityInput,
    pub(crate) records: pl::Records,
    pub(crate) msg: Option<(Tone, String)>,
}

impl Default for CodingView {
    fn default() -> Self {
        Self {
            picker: PackPicker::new(PackKind::Coding),
            loaded: None,
            read_job: None,
            coding: None,
            sel: None,
            value_text: String::new(),
            preview_job: None,
            preview: None,
            step: Step::Idle,
            last: None,
            last_backup: None,
            security: Default::default(),
            records: Default::default(),
            msg: None,
        }
    }
}

impl CodingView {
    fn set_def(&mut self, path: &Path) {
        match load(path) {
            Ok(l) => {
                self.loaded = Some(l);
                self.coding = None;
                self.preview = None;
                self.sel = None;
                self.msg = None;
            }
            Err(e) => self.msg = Some((Tone::Crit, e)),
        }
    }

    fn module(&self, host: &Host<'_>) -> Result<DiagTarget, String> {
        pl::module_of(host, self.loaded.as_ref().and_then(|l| l.module))
    }

    fn busy(&self) -> bool {
        self.read_job.is_some() || self.preview_job.is_some() || !matches!(self.step, Step::Idle)
    }

    fn start_read(&mut self, host: &Host<'_>) {
        let (Some(l), Some(a)) = (&self.loaded, host.adapter()) else {
            return;
        };
        let target = match self.module(host) {
            Ok(t) => t,
            Err(e) => {
                self.msg = Some((Tone::Crit, e));
                return;
            }
        };
        let def = l.def.clone();
        self.read_job = Some(host.spawn_job("coding read", move |_| read_op(&a, target, def)));
    }

    fn selected_param(&self) -> Option<(String, u64)> {
        let l = self.loaded.as_ref()?;
        let p = l.def.params.get(self.sel?)?;
        let v = pl::parse_num("value", &self.value_text, u64::MAX).ok()?;
        Some((p.name.clone(), v))
    }

    fn start_preview(&mut self, host: &Host<'_>) {
        let (Some(l), Some(a), Some((name, value))) =
            (&self.loaded, host.adapter(), self.selected_param())
        else {
            return;
        };
        let Ok(target) = self.module(host) else {
            return;
        };
        let def = l.def.clone();
        self.preview_job = Some(host.spawn_job("coding preview", move |_| {
            preview_op(&a, target, def, name, value)
        }));
    }

    /// Ask for the write (opens the confirmation for the definition's
    /// module). Refused, sending nothing, when there is no backup folder or
    /// the seed/key id does not resolve.
    pub(crate) fn request_write(&mut self, host: &mut Host<'_>, name: String, value: u64) {
        let Some(l) = &self.loaded else { return };
        let module = match self.module(host) {
            Ok(m) => m,
            Err(e) => return self.msg = Some((Tone::Crit, e)),
        };
        if self.records.near(host).is_none() {
            return self.msg = Some((
                Tone::Crit,
                "choose a backup folder first: the old coding must be kept".into(),
            ));
        }
        let unlock = match self.security.resolve(host, 0x01) {
            Ok(u) => u,
            Err(e) => return self.msg = Some((Tone::Crit, e)),
        };
        let req = ConfirmRequest {
            key: pl::confirm_key("coding-write", module),
            title: format!("Write coding: {name} = {value}"),
            body: format!(
                "Writes the coding of '{}' on the module below. The current coding is read \
                 and saved to the vault first; the write is refused if that fails or if the \
                 module changes in between. A wrong coding can disable functions of the \
                 module.",
                l.def.name
            ),
            typed: Some("WRITE".into()),
            confirm_label: "Write coding".into(),
        };
        self.step = Step::Confirm(
            req,
            Pending {
                module,
                label: pl::module_label(&l.def.name, module),
                name,
                value,
                unlock,
            },
        );
    }

    /// Advance the jobs and the write pipeline (every frame).
    pub(crate) fn tick(&mut self, host: &mut Host<'_>) {
        if let Some(r) = pl::take(&mut self.read_job) {
            match r {
                Ok(c) => {
                    self.coding = Some(c);
                    self.msg = None;
                }
                Err(e) => self.msg = Some((Tone::Crit, format!("read failed: {e}"))),
            }
        }
        if let Some(r) = pl::take(&mut self.preview_job) {
            match r {
                Ok(p) => self.preview = Some(p),
                Err(e) => self.msg = Some((Tone::Crit, format!("preview failed: {e}"))),
            }
        }
        self.step = match std::mem::replace(&mut self.step, Step::Idle) {
            Step::Idle => Step::Idle,
            Step::Confirm(req, p) => match host.confirm_ecu_for(req.clone(), p.module, &p.label) {
                ConfirmResult::Pending => Step::Confirm(req, p),
                ConfirmResult::Declined => {
                    self.msg = Some((Tone::Muted, "write cancelled: nothing was sent".into()));
                    Step::Idle
                }
                ConfirmResult::Accepted(ack) => {
                    match (host.adapter(), self.loaded.as_ref(), ack.target()) {
                        (Some(a), Some(l), Some(t)) if t == p.module => {
                            let def = l.def.clone();
                            let job =
                                host.spawn_job("coding pre-read", move |_| read_op(&a, t, def));
                            Step::PreRead(ack, job, p)
                        }
                        _ => {
                            self.msg = Some((
                                Tone::Crit,
                                "write refused: no adapter, or the confirmed module is not the \
                                 definition's; nothing was sent"
                                    .into(),
                            ));
                            Step::Idle
                        }
                    }
                }
            },
            Step::PreRead(ack, mut job, p) => match job.take() {
                None => Step::PreRead(ack, job, p),
                Some(Err(e)) => {
                    self.msg = Some((Tone::Crit, format!("pre-read failed, nothing written: {e}")));
                    Step::Idle
                }
                Some(Ok(old)) => self.backup_then_write(host, ack, old, p),
            },
            Step::Write(mut job, receipt) => match job.take() {
                None => Step::Write(job, receipt),
                Some(r) => {
                    self.msg = Some(match &r {
                        Ok(rep) => outcome_text(&rep.outcome),
                        Err(e) => (Tone::Crit, format!("write failed: {e}")),
                    });
                    if let Ok(rep) = &r {
                        self.coding = Some(rep.target.clone());
                    }
                    self.last_backup = Some(receipt.path.clone());
                    self.last = Some(r);
                    self.preview = None;
                    Step::Idle
                }
            },
        };
    }

    fn backup_then_write(
        &mut self,
        host: &mut Host<'_>,
        ack: Ack,
        old: Coding,
        p: Pending,
    ) -> Step {
        let (Some(l), Some(a), Some(near)) = (
            self.loaded.as_ref(),
            host.adapter(),
            self.records.near(host),
        ) else {
            self.msg = Some((Tone::Crit, "write refused: nothing was sent".into()));
            return Step::Idle;
        };
        let label = backup_label(&l.def, p.module, &old);
        let receipt = match host.backup(&near, &label, &backup_blob(&old)) {
            Ok(r) => r,
            Err(e) => {
                self.msg = Some((Tone::Crit, format!("backup failed, nothing written: {e}")));
                return Step::Idle;
            }
        };
        let args = WriteArgs {
            def: l.def.clone(),
            old,
            name: p.name,
            value: p.value,
            unlock: p.unlock,
        };
        let r2 = receipt.clone();
        let module = p.module;
        let job = host.spawn_job("coding write", move |_| {
            write_op(&a, ack, module, &r2, args)
        });
        Step::Write(job, receipt)
    }

    fn param_editor(&mut self, ui: &mut egui::Ui, host: &Host<'_>) {
        let Some(l) = &self.loaded else { return };
        let Some(p) = self.sel.and_then(|i| l.def.params.get(i)) else {
            pl::note(ui, host, Tone::Muted, "Select a parameter to edit it.");
            return;
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new(&p.name).strong());
            match p.kind {
                ParamKind::Enum if !p.options.is_empty() => {
                    let cur = pl::parse_num("v", &self.value_text, u64::MAX).ok();
                    let shown = p
                        .options
                        .iter()
                        .find(|o| Some(o.value) == cur)
                        .map(|o| format!("{} ({})", o.label, o.value))
                        .unwrap_or_else(|| self.value_text.clone());
                    egui::ComboBox::from_id_salt("coding-enum")
                        .selected_text(shown)
                        .show_ui(ui, |ui| {
                            for o in &p.options {
                                if ui
                                    .selectable_label(cur == Some(o.value), &o.label)
                                    .clicked()
                                {
                                    self.value_text = o.value.to_string();
                                }
                            }
                        });
                }
                ParamKind::Bool => {
                    let mut on = self.value_text.trim() == "1";
                    if ui.checkbox(&mut on, "on").changed() {
                        self.value_text = if on { "1" } else { "0" }.into();
                    }
                }
                _ => {
                    ui.add(egui::TextEdit::singleline(&mut self.value_text).desired_width(90.0));
                }
            }
        });
        if !p.note.is_empty() {
            pl::note(ui, host, Tone::Muted, &p.note);
        }
    }
}

impl NativeView for CodingView {
    fn id(&self) -> &str {
        "coding"
    }
    fn label(&self) -> &str {
        "Coding"
    }
    fn icon(&self) -> &str {
        icons::SLIDERS_HORIZONTAL
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
        if self.busy() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
        theme::panel(
            ui,
            &t,
            "Coding",
            |_| {},
            |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_space(6.0);
                    if let Some(d) = self.picker.ui(ui, host) {
                        self.set_def(&d.path);
                    }
                    ui.horizontal(|ui| {
                        if theme::button(ui, &t, Some(icons::FOLDER_OPEN), "Open definition…")
                            .clicked()
                        {
                            if let Some(p) =
                                host.pick_file("Coding definition", &[("JSON", &["json"])])
                            {
                                self.set_def(&p);
                            }
                        }
                    });
                    let Some(l) = self.loaded.clone() else {
                        pl::note(ui, host, Tone::Muted, "Choose a coding definition.");
                        return;
                    };
                    let module = self.module(host);
                    pl::mono(
                        ui,
                        host,
                        &format!(
                            "{}  ·  module {}{}",
                            l.def.name,
                            module
                                .as_ref()
                                .map(|m| m.to_string())
                                .unwrap_or_else(|e| e.clone()),
                            if l.module.is_none() {
                                " (app target; the definition names none)"
                            } else {
                                ""
                            }
                        ),
                    );
                    if l.def.requires_security {
                        pl::note(
                            ui,
                            host,
                            Tone::Warn,
                            "This module needs SecurityAccess to write: enter a seed/key id.",
                        );
                    }
                    let can_diag =
                        crate::widgets::caps::gate(ui, host, &[crate::widgets::caps::Need::Diag]);
                    ui.horizontal(|ui| {
                        let ok = can_diag && !self.busy() && module.is_ok();
                        if ui
                            .add_enabled_ui(ok, |ui| {
                                theme::primary_button(ui, &t, Some(icons::DOWNLOAD), "Read coding")
                            })
                            .inner
                            .clicked()
                        {
                            self.start_read(host);
                        }
                    });
                    if let Some((tone, m)) = &self.msg {
                        pl::note(ui, host, *tone, m);
                    }
                    if let Some(b) = &self.last_backup {
                        pl::mono(ui, host, &format!("backup: {}", b.display()));
                    }
                    let Some(c) = self.coding.clone() else { return };
                    let decoded = match decode(&l.def, &c) {
                        Ok(d) => d,
                        Err(e) => {
                            pl::note(ui, host, Tone::Crit, &format!("decode: {e}"));
                            return;
                        }
                    };
                    for b in &c.blocks {
                        pl::mono(
                            ui,
                            host,
                            &format!("DID {:04X}: {}", b.did, pl::hex(&b.bytes)),
                        );
                    }
                    ui.add_space(6.0);
                    egui::Grid::new("coding-params")
                        .striped(true)
                        .num_columns(3)
                        .show(ui, |ui| {
                            for (i, d) in decoded.iter().enumerate() {
                                if ui.selectable_label(self.sel == Some(i), &d.name).clicked() {
                                    self.sel = Some(i);
                                    self.value_text = d.value.to_string();
                                    self.preview = None;
                                }
                                ui.label(&d.label);
                                ui.label(RichText::new(d.value.to_string()).color(t.fg_dim));
                                ui.end_row();
                            }
                        });
                    ui.add_space(8.0);
                    theme::section_label(ui, &t, "Edit");
                    self.param_editor(ui, host);
                    let off = module
                        .as_ref()
                        .ok()
                        .and_then(|m| pl::off_target_note(host, *m));
                    ui.horizontal(|ui| {
                        let sel = self.selected_param();
                        if ui
                            .add_enabled_ui(sel.is_some() && !self.busy(), |ui| {
                                theme::button(ui, &t, Some(icons::EYE), "Preview")
                            })
                            .inner
                            .clicked()
                        {
                            self.start_preview(host);
                        }
                        let ok = self.preview.is_some() && !self.busy() && can_diag;
                        if ui
                            .add_enabled_ui(ok, |ui| {
                                theme::primary_button(ui, &t, Some(icons::WARNING), "Write…")
                            })
                            .inner
                            .clicked()
                        {
                            if let Some((n, v)) = sel {
                                self.request_write(host, n, v);
                            }
                        }
                    });
                    if let Some(b) = &off {
                        pl::note(ui, host, Tone::Warn, b);
                    }
                    self.security.ui(ui, host);
                    self.records.ui(ui, host);
                    if let Some(p) = &self.preview {
                        theme::section_label(ui, &t, "Preview (nothing written)");
                        for c in &p.param_changes {
                            pl::mono(ui, host, &format!("{}: {} → {}", c.name, c.old.1, c.new.1));
                        }
                        for c in &p.byte_changes {
                            pl::mono(
                                ui,
                                host,
                                &format!(
                                    "DID {:04X} byte {}: {:02X} → {:02X}",
                                    c.did, c.byte, c.old, c.new
                                ),
                            );
                        }
                        if p.byte_changes.is_empty() {
                            pl::note(ui, host, Tone::Muted, "no change");
                        }
                    }
                });
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::simkit::{xor_keys, Bus, Fixture};
    use super::*;
    use opentune_udssim::security::XorSecurity;
    use opentune_udssim::server::{ServerConfig, UdsServer};

    const BCM: DiagTarget = DiagTarget::can(0x70E, 0x778);
    const ENGINE: DiagTarget = DiagTarget::ENGINE;

    fn def_file(dir: &Path) -> PathBuf {
        let p = dir.join("bcm.json");
        std::fs::write(
            &p,
            r#"{"id":"bcm","name":"BCM","req_id":"70E","resp_id":"778",
                "addressing":{"kind":"did","dids":[1536]},
                "params":[{"name":"DRL","block":0,"byte":0,"bit":0,"len":1,"kind":"bool"}]}"#,
        )
        .unwrap();
        p
    }

    fn server(did: [u8; 2]) -> UdsServer {
        let mut s = UdsServer::new();
        s.set_did(0x0600, did.to_vec());
        s
    }

    /// An engine and a BCM with the same coding DID.
    fn bus() -> Bus {
        Bus::default().uds(0x7E0, 0x7E8, server([0x00, 0x11])).uds(
            0x70E,
            0x778,
            server([0x00, 0x11]),
        )
    }

    fn did(bus: &Bus, t: DiagTarget) -> Vec<u8> {
        bus.with_uds(t.tx, t.rx, |s| s.did(0x0600).unwrap().to_vec())
    }

    /// Drive the view's pipeline until it is idle again.
    fn run(view: &mut CodingView, fx: &mut Fixture, target: DiagTarget) {
        let t0 = std::time::Instant::now();
        while !matches!(view.step, Step::Idle) {
            assert!(t0.elapsed() < std::time::Duration::from_secs(10), "stuck");
            fx.host(target, |h| view.tick(h));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn view(fx: &Fixture) -> CodingView {
        let mut v = CodingView {
            records: pl::Records::in_dir(&fx.dir),
            ..Default::default()
        };
        v.set_def(&def_file(&fx.dir));
        v
    }

    #[test]
    fn reads_and_previews_send_only_read_services() {
        let bus = bus();
        let a = bus.access("sim");
        let fx = Fixture::new("coding-read");
        let l = load(&def_file(&fx.dir)).unwrap();
        assert_eq!(l.module, Some(BCM));
        let c = read_op(&a, BCM, l.def.clone()).unwrap();
        assert_eq!(c.blocks[0].bytes, vec![0x00, 0x11]);
        let p = preview_op(&a, BCM, l.def.clone(), "DRL".into(), 1).unwrap();
        assert_eq!(p.outcome, WriteOutcome::Planned);
        assert_eq!(p.byte_changes.len(), 1);
        assert!(
            bus.wire().iter().all(|(_, r)| r[0] == 0x22),
            "{:02X?}",
            bus.wire()
        );
        assert_eq!(did(&bus, BCM), vec![0x00, 0x11], "a preview writes nothing");
    }

    #[test]
    fn a_declined_or_pending_confirmation_sends_nothing() {
        let bus = bus();
        let mut fx = Fixture::new("coding-decline");
        fx.adapter = Some(bus.access("sim"));
        let mut v = view(&fx);
        fx.host(BCM, |h| v.request_write(h, "DRL".into(), 1));
        assert!(matches!(v.step, Step::Confirm(..)));
        for _ in 0..3 {
            fx.host(BCM, |h| v.tick(h));
        }
        assert!(
            matches!(v.step, Step::Confirm(..)),
            "pending until answered"
        );
        fx.confirms
            .answer(&pl::confirm_key("coding-write", BCM), false);
        fx.host(BCM, |h| v.tick(h));
        assert!(matches!(v.step, Step::Idle));
        assert!(bus.wire().is_empty(), "nothing sent: {:02X?}", bus.wire());
        assert_eq!(did(&bus, BCM), vec![0x00, 0x11]);
    }

    #[test]
    fn a_plain_confirm_ack_cannot_drive_the_write() {
        let bus = bus();
        let a = bus.access("sim");
        let mut fx = Fixture::new("coding-plain");
        let l = load(&def_file(&fx.dir)).unwrap();
        let old = read_op(&a, BCM, l.def.clone()).unwrap();
        bus.clear_wire();
        // A file-write confirmation (no module) is refused by mutate.
        let req = ConfirmRequest {
            key: "plain".into(),
            title: "t".into(),
            body: "b".into(),
            typed: None,
            confirm_label: "ok".into(),
        };
        fx.host(BCM, |h| h.confirm(req.clone()));
        fx.confirms.answer("plain", true);
        let ConfirmResult::Accepted(plain) = fx.host(BCM, |h| h.confirm(req)) else {
            panic!()
        };
        let receipt = fx.backup(&backup_blob(&old));
        let args = WriteArgs {
            def: l.def,
            old,
            name: "DRL".into(),
            value: 1,
            unlock: None,
        };
        let e = write_op(&a, plain, BCM, &receipt, args).unwrap_err();
        assert!(e.contains("does not name module"), "{e}");
        assert!(bus.wire().is_empty());
        assert_eq!(did(&bus, BCM), vec![0x00, 0x11]);
    }

    #[test]
    fn the_write_backs_up_the_old_bytes_and_reaches_only_the_confirmed_module() {
        let bus = bus();
        let mut fx = Fixture::new("coding-write");
        fx.adapter = Some(bus.access("sim"));
        let mut v = view(&fx);
        fx.host(BCM, |h| v.request_write(h, "DRL".into(), 1));
        fx.host(BCM, |h| v.tick(h));
        fx.confirms
            .answer(&pl::confirm_key("coding-write", BCM), true);
        run(&mut v, &mut fx, BCM);
        let rep = v.last.take().unwrap().unwrap();
        assert_eq!(rep.outcome, WriteOutcome::Verified, "{:?}", v.msg);
        // The vault holds the OLD bytes, the module the new ones.
        let backup = v.last_backup.clone().unwrap();
        assert_eq!(std::fs::read(&backup).unwrap(), vec![0x00, 0x11]);
        assert_eq!(did(&bus, BCM), vec![0x01, 0x11]);
        // Only the BCM was addressed; the engine (same DID) is untouched.
        assert!(bus.sent_to(0x7E0).is_empty());
        assert_eq!(did(&bus, ENGINE), vec![0x00, 0x11]);
        let bcm = bus.sent_to(0x70E);
        // Pre-read (read job) before the extended session of the mutate.
        let ext = bcm.iter().position(|r| r == &vec![0x10, 0x03]).unwrap();
        let wr = bcm.iter().position(|r| r[0] == 0x2E).unwrap();
        assert_eq!(bcm[0], vec![0x22, 0x06, 0x00], "pre-read first");
        assert!(ext < wr);
        assert_eq!(bcm.last().unwrap(), &vec![0x10, 0x01]);
    }

    #[test]
    fn an_off_target_module_is_confirmed_by_name_and_only_it_is_written() {
        let bus = bus();
        let mut fx = Fixture::new("coding-other");
        fx.adapter = Some(bus.access("sim"));
        let mut v = view(&fx);
        // The definition names the BCM, the app target is the engine: the
        // confirmation is for the BCM, by name.
        fx.host(ENGINE, |h| v.request_write(h, "DRL".into(), 1));
        let Step::Confirm(_, p) = &v.step else {
            panic!("expected the confirmation")
        };
        assert_eq!(p.module, BCM);
        assert_eq!(p.label, "BCM (tx 0x70E / rx 0x778)");
        fx.host(ENGINE, |h| v.tick(h));
        assert!(bus.wire().is_empty(), "nothing before the confirmation");
        fx.confirms
            .answer(&pl::confirm_key("coding-write", BCM), true);
        run(&mut v, &mut fx, ENGINE);
        assert_eq!(
            v.last.take().unwrap().unwrap().outcome,
            WriteOutcome::Verified
        );
        assert_eq!(did(&bus, BCM), vec![0x01, 0x11]);
        assert!(
            bus.sent_to(0x7E0).is_empty(),
            "the app target is never touched"
        );
        assert_eq!(did(&bus, ENGINE), vec![0x00, 0x11]);
    }

    #[test]
    fn an_ack_for_another_module_writes_nothing() {
        let bus = bus();
        let a = bus.access("sim");
        let mut fx = Fixture::new("coding-foreign");
        let l = load(&def_file(&fx.dir)).unwrap();
        let old = read_op(&a, BCM, l.def.clone()).unwrap();
        let receipt = fx.backup(&backup_blob(&old));
        bus.clear_wire();
        let engine_ack = fx.ecu_ack(ENGINE);
        let args = WriteArgs {
            def: l.def,
            old,
            name: "DRL".into(),
            value: 1,
            unlock: None,
        };
        let e = write_op(&a, engine_ack, BCM, &receipt, args).unwrap_err();
        assert!(e.contains("does not name module"), "{e}");
        assert!(bus.wire().is_empty());
    }

    #[test]
    fn a_changed_module_or_a_foreign_receipt_writes_nothing() {
        let bus = bus();
        let a = bus.access("sim");
        let mut fx = Fixture::new("coding-stale");
        let l = load(&def_file(&fx.dir)).unwrap();
        let old = read_op(&a, BCM, l.def.clone()).unwrap();
        let receipt = fx.backup(&backup_blob(&old));
        // The module changes between the backup and the write.
        bus.with_uds(0x70E, 0x778, |s| s.set_did(0x0600, vec![0x00, 0x22]));
        let ack = fx.ecu_ack(BCM);
        let args = WriteArgs {
            def: l.def.clone(),
            old: old.clone(),
            name: "DRL".into(),
            value: 1,
            unlock: None,
        };
        let e = write_op(&a, ack, BCM, &receipt, args).unwrap_err();
        assert!(e.contains("changed since"), "{e}");
        assert!(bus.with_sid(0x2E).is_empty());
        // A receipt for other bytes is refused before the adapter.
        let other = fx.backup(&[0xAA, 0xBB]);
        let ack = fx.ecu_ack(BCM);
        bus.clear_wire();
        let args = WriteArgs {
            def: l.def,
            old,
            name: "DRL".into(),
            value: 1,
            unlock: None,
        };
        let e = write_op(&a, ack, BCM, &other, args).unwrap_err();
        assert!(e.contains("receipt"), "{e}");
        assert!(bus.wire().is_empty());
    }

    #[test]
    fn security_access_runs_once_with_the_configured_resolver() {
        let mut bcm = UdsServer::with_config(ServerConfig {
            require_security_for_write: true,
            ..Default::default()
        });
        bcm.set_did(0x0600, vec![0x00, 0x11]);
        bcm.set_security_checker(Box::new(XorSecurity::new(0x5A)));
        let bus = Bus::default().uds(0x70E, 0x778, bcm);
        let a = bus.access("sim");
        let mut fx = Fixture::new("coding-sec");
        fx.keys = xor_keys();
        let l = load(&def_file(&fx.dir)).unwrap();
        let old = read_op(&a, BCM, l.def.clone()).unwrap();
        let receipt = fx.backup(&backup_blob(&old));
        let ack = fx.ecu_ack(BCM);
        let unlock = fx.host(BCM, |h| h.unlocker_for("xor-5a")).unwrap();
        let args = WriteArgs {
            def: l.def,
            old,
            name: "DRL".into(),
            value: 1,
            unlock: Some((unlock, 0x01)),
        };
        let rep = write_op(&a, ack, BCM, &receipt, args).unwrap();
        assert_eq!(rep.outcome, WriteOutcome::Verified);
        assert_eq!(bus.with_sid(0x27).len(), 2, "seed + key, once");
    }

    #[test]
    fn no_backup_folder_no_write() {
        let bus = bus();
        let mut fx = Fixture::new("coding-nofolder");
        fx.adapter = Some(bus.access("sim"));
        let mut v = view(&fx);
        v.records = pl::Records::default();
        fx.host(BCM, |h| v.request_write(h, "DRL".into(), 1));
        assert!(matches!(v.step, Step::Idle));
        assert!(bus.wire().is_empty());
    }
}
