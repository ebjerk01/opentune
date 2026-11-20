//! Flash backup vault: the full-image backups OpenTune saves before every
//! write (`opentune-vault/` beside the edited file).
//!
//! The on-disk layout is the core's (`opentune_flash::vault::save_backup`):
//! one `<label>_<ADDR>_<unix time>.bin` per backup plus a human-readable
//! `.meta` sidecar of `key = value` lines (ecu_name, sw_number, hw_number,
//! vin, address, length, crc32, unix_time). The listing is a local
//! equivalent of `opentune_hub::vault::list` (what the Tauri app's
//! `vault_list` uses); it also parses the sidecar and re-checks the CRC-32.
//!
//! This view never changes a backup: it opens one in the Flash view (which
//! only reads it), reveals it in the file manager, or writes a copy to a new
//! path the user picks.

use crate::icons;
use crate::theme::{self, font, Tokens, Tone, MONO_MEDIUM, UI_MEDIUM, UI_STRONG};
use crate::views::maps::{kv, two_pane};
use eframe::egui::{
    self, Align, Color32, CornerRadius, Label, Layout, Margin, Rect, RichText, Sense, Stroke, Ui,
    UiBuilder, Vec2,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

/// Directory name of the vault beside an image (same as the Tauri app).
pub(crate) const VAULT_DIR: &str = "opentune-vault";

/// Height of a list row (title + meta line).
pub(crate) const ROW_H: f32 = 44.0;

/// The vault beside `image`: `<dir of image>/opentune-vault`. When the image
/// is itself a backup (opened from this view), its own folder is the vault.
pub(crate) fn vault_dir_for(image: &Path) -> PathBuf {
    let dir = image.parent().unwrap_or_else(|| Path::new("."));
    if dir.file_name().is_some_and(|n| n == VAULT_DIR) {
        dir.to_path_buf()
    } else {
        dir.join(VAULT_DIR)
    }
}

// ---------------------------------------------------------------------------
// Listing (pure, unit-tested)
// ---------------------------------------------------------------------------

/// One backup in a vault directory.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Backup {
    pub path: PathBuf,
    /// File name of the `.bin`.
    pub name: String,
    pub size: u64,
    /// File modification time (unix seconds), the fallback timestamp.
    pub modified: Option<u64>,
    /// True when a `.meta` sidecar exists.
    pub has_meta: bool,
    /// The sidecar's `key = value` (or `key: value`) pairs, in file order.
    pub meta: Vec<(String, String)>,
}

/// Keys the core writes; anything else is shown under "Other sidecar fields".
const KNOWN_KEYS: [&str; 8] = [
    "ecu_name",
    "sw_number",
    "hw_number",
    "vin",
    "address",
    "length",
    "crc32",
    "unix_time",
];

impl Backup {
    /// A non-empty sidecar value.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.meta
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    }

    /// When the backup was taken: the sidecar's `unix_time`, else the file's
    /// modification time.
    pub fn when(&self) -> Option<u64> {
        self.get("unix_time")
            .and_then(|v| v.parse().ok())
            .or(self.modified)
    }

    /// What the backup is of: the ECU name / label, else the SW number, else
    /// the file stem.
    pub fn label(&self) -> String {
        self.get("ecu_name")
            .or_else(|| self.get("sw_number"))
            .map(str::to_string)
            .unwrap_or_else(|| {
                self.path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
    }

    /// The CRC-32 the sidecar records.
    pub fn stored_crc(&self) -> Option<u32> {
        self.get("crc32").and_then(parse_u32)
    }

    /// Sidecar fields the core does not write (hand-made or newer sidecars).
    pub fn other_meta(&self) -> impl Iterator<Item = &(String, String)> {
        self.meta
            .iter()
            .filter(|(k, _)| !KNOWN_KEYS.contains(&k.as_str()))
    }
}

/// `0x1F` (hex) or decimal.
fn parse_u32(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(h, 16).ok()
    } else {
        s.parse().ok()
    }
}

/// Parse a sidecar: one `key = value` or `key: value` per line (whichever
/// separator comes first); blank and separator-less lines are skipped.
pub(crate) fn parse_meta(txt: &str) -> Vec<(String, String)> {
    txt.lines()
        .filter_map(|line| {
            let i = line.find(['=', ':'])?;
            let k = line[..i].trim();
            if k.is_empty() {
                return None;
            }
            Some((k.to_string(), line[i + 1..].trim().to_string()))
        })
        .collect()
}

/// Every `.bin` in `dir` with its sidecar, newest first (ties by name,
/// descending). A missing directory is an empty vault.
pub(crate) fn list_vault(dir: &Path) -> std::io::Result<Vec<Backup>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let is_bin = path
            .extension()
            .and_then(|s| s.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("bin"));
        if !is_bin || !path.is_file() {
            continue;
        }
        let md = std::fs::metadata(&path).ok();
        let meta_txt = std::fs::read_to_string(path.with_extension("meta")).ok();
        out.push(Backup {
            name: path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            size: md.as_ref().map(|m| m.len()).unwrap_or(0),
            modified: md
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            has_meta: meta_txt.is_some(),
            meta: meta_txt.as_deref().map(parse_meta).unwrap_or_default(),
            path,
        });
    }
    out.sort_by(|a, b| b.when().cmp(&a.when()).then_with(|| b.name.cmp(&a.name)));
    Ok(out)
}

/// CRC-32 (IEEE), the vault sidecar's integrity tag (same as the core's).
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Result of re-checking a backup against its sidecar.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Integrity {
    /// CRC-32 and length match the sidecar.
    Intact,
    /// The data no longer matches what the sidecar recorded.
    Changed {
        crc: u32,
        len: u64,
    },
    /// No CRC recorded (no sidecar, or an older one).
    Unknown,
    Unreadable(String),
}

pub(crate) fn check_integrity(b: &Backup) -> Integrity {
    let Some(stored) = b.stored_crc() else {
        return Integrity::Unknown;
    };
    match std::fs::read(&b.path) {
        Ok(data) => {
            let crc = crc32(&data);
            let len = data.len() as u64;
            let len_ok = b
                .get("length")
                .and_then(|v| v.parse::<u64>().ok())
                .is_none_or(|l| l == len);
            if crc == stored && len_ok {
                Integrity::Intact
            } else {
                Integrity::Changed { crc, len }
            }
        }
        Err(e) => Integrity::Unreadable(e.to_string()),
    }
}

/// Why a restore target is refused (`Ok` = allowed). The copy never lands on
/// the backup itself, on the dump open in the Flash view, or inside the vault.
pub(crate) fn check_restore_target(
    dest: &Path,
    backup: &Path,
    image: Option<&Path>,
    vault: &Path,
) -> Result<(), String> {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let d = canon(dest);
    if d == canon(backup) {
        return Err("That is the backup itself; choose a new file.".into());
    }
    if image.is_some_and(|i| d == canon(i)) {
        return Err(
            "That is the dump open in Flash dump. Restore to a new file, then open that.".into(),
        );
    }
    if dest.parent().is_some_and(|p| canon(p) == canon(vault)) {
        return Err("Choose a folder outside the vault; it only holds backups.".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Small shared helpers (also used by the Project view)
// ---------------------------------------------------------------------------

/// `(year, month, day, hour, minute, second)` of a unix time, UTC.
pub(crate) fn utc_parts(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (H. Hinnant), proleptic Gregorian.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        (rem / 60 % 60) as u32,
        (rem % 60) as u32,
    )
}

/// `2026-11-19 22:10 UTC`.
pub(crate) fn fmt_utc(secs: u64) -> String {
    let (y, mo, d, h, mi, _) = utc_parts(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02} UTC")
}

/// Exact byte count plus a short form: `1 MiB · 1048576 bytes`.
fn size_long(n: u64) -> String {
    let short = crate::views::flash::size_label(n as usize);
    if n < 1024 {
        short
    } else {
        format!("{short} · {n} bytes")
    }
}

#[cfg(target_os = "windows")]
fn opener(path: &Path) -> Command {
    use std::os::windows::process::CommandExt;
    let mut c = Command::new("explorer");
    if path.is_file() {
        c.raw_arg(format!("/select,\"{}\"", path.display()));
    } else {
        c.arg(path);
    }
    c
}

#[cfg(target_os = "macos")]
fn opener(path: &Path) -> Command {
    let mut c = Command::new("open");
    if path.is_file() {
        c.arg("-R");
    }
    c.arg(path);
    c
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn opener(path: &Path) -> Command {
    let dir = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    let mut c = Command::new("xdg-open");
    c.arg(dir);
    c
}

/// Open the OS file manager at `path` (the file is selected where the OS
/// supports it, else its folder is opened). Non-blocking: the opener runs in
/// the background and a helper thread reaps it.
pub(crate) fn reveal(path: &Path) -> std::io::Result<()> {
    use std::process::Stdio;
    let mut child = opener(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// A tinted message box: icon + wrapped text.
pub(crate) fn callout(ui: &mut Ui, t: &Tokens, icon: &str, fg: Color32, bg: Color32, s: &str) {
    egui::Frame::new()
        .fill(bg)
        .stroke(Stroke::new(1.0, fg.gamma_multiply(0.4)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.label(RichText::new(icon).size(18.0).color(fg));
                ui.add(Label::new(RichText::new(s).color(t.fg)).wrap());
            });
        });
}

/// A status message after an action.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Msg {
    Ok(String),
    Warn(String),
    Err(String),
}

pub(crate) fn show_msg(ui: &mut Ui, t: &Tokens, m: &Msg) {
    match m {
        Msg::Ok(s) => callout(ui, t, icons::CHECK_CIRCLE, t.ok, t.ok_soft, s),
        Msg::Warn(s) => callout(ui, t, icons::WARNING, t.warn, t.crit_soft, s),
        Msg::Err(s) => callout(ui, t, icons::X_CIRCLE, t.crit, t.crit_soft, s),
    }
}

/// A selectable two-line list row (title + mono meta line), styled like the
/// Maps browser's rows.
pub(crate) fn list_row(
    ui: &mut Ui,
    t: &Tokens,
    r: Rect,
    id: egui::Id,
    title: &str,
    meta: &str,
    selected: bool,
) -> egui::Response {
    let resp = ui.interact(r, id, Sense::click());
    let bg = r.shrink2(Vec2::new(6.0, 1.0));
    if selected {
        ui.painter()
            .rect_filled(bg, CornerRadius::same(theme::RADIUS_SM), t.sel);
        ui.painter().rect_filled(
            Rect::from_min_size(
                bg.min + Vec2::new(0.0, 8.0),
                Vec2::new(3.0, bg.height() - 16.0),
            ),
            CornerRadius::same(2),
            t.accent,
        );
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(bg, CornerRadius::same(theme::RADIUS_SM), t.surface2);
    }
    let inner = r.shrink2(Vec2::new(16.0, 5.0));
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::top_down(Align::Min)),
        |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.add(
                Label::new(
                    RichText::new(title)
                        .font(font(if selected { UI_STRONG } else { UI_MEDIUM }, 13.0))
                        .color(t.fg),
                )
                .truncate()
                .selectable(false),
            );
            ui.add(
                Label::new(
                    RichText::new(meta)
                        .font(font(MONO_MEDIUM, 11.0))
                        .color(t.fg_faint),
                )
                .truncate()
                .selectable(false),
            );
        },
    );
    resp
}

/// A one-line mono path, truncated, full path on hover.
pub(crate) fn path_line(ui: &mut Ui, t: &Tokens, p: &Path) {
    let s = p.display().to_string();
    ui.add(
        Label::new(
            RichText::new(&s)
                .font(font(MONO_MEDIUM, 11.0))
                .color(t.fg_dim),
        )
        .truncate(),
    )
    .on_hover_text(s);
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct VaultView {
    /// A folder picked with "Choose folder…" (else the vault beside the dump).
    chosen: Option<PathBuf>,
    /// The directory `backups` was listed from.
    loaded: Option<PathBuf>,
    backups: Vec<Backup>,
    error: Option<String>,
    exists: bool,
    selected: Option<PathBuf>,
    integrity: HashMap<PathBuf, Integrity>,
    msg: Option<Msg>,
    last_pass: u64,
}

/// What the user asked for this frame.
enum Act {
    Open(PathBuf),
    Restore(usize),
    Reveal(PathBuf),
    Choose,
    UseDumpVault,
    Refresh,
}

impl VaultView {
    /// `image` is the dump open in the Flash view (its vault is beside it).
    /// Returns a backup path the user chose to open in the Flash view.
    pub fn ui(&mut self, ui: &mut Ui, t: &Tokens, image: Option<&Path>) -> Option<PathBuf> {
        let dir = self.chosen.clone().or_else(|| image.map(vault_dir_for));
        // Re-list when the folder changes or the view comes back into sight
        // (it is not drawn while another view is shown).
        let pass = ui.ctx().cumulative_pass_nr();
        let back = pass > self.last_pass + 1;
        self.last_pass = pass;
        if dir != self.loaded || back {
            self.rescan(dir.clone());
        }

        let mut act = None;
        let Some(dir) = dir else {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.28).max(24.0));
                ui.label(RichText::new(icons::VAULT).size(40.0).color(t.accent));
                ui.add_space(8.0);
                ui.label(
                    RichText::new("Backup vault")
                        .font(font(theme::DISPLAY, 20.0))
                        .color(t.fg),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "OpenTune archives the whole file to opentune-vault/ beside it before \
                         every write. Open a dump in Flash dump to see its vault, or browse one.",
                    )
                    .color(t.fg_dim),
                );
                ui.add_space(12.0);
                if theme::button(ui, t, Some(icons::FOLDER_OPEN), "Choose folder…").clicked() {
                    act = Some(Act::Choose);
                }
            });
            return self.apply(act, image, None);
        };

        let (left, right) = two_pane(ui);
        ui.scope_builder(UiBuilder::new().max_rect(left), |ui| {
            if let Some(a) = self.list_pane(ui, t, &dir, image) {
                act = Some(a);
            }
        });
        ui.scope_builder(UiBuilder::new().max_rect(right), |ui| {
            if let Some(a) = self.detail_pane(ui, t) {
                act = Some(a);
            }
        });
        self.apply(act, image, Some(&dir))
    }

    fn rescan(&mut self, dir: Option<PathBuf>) {
        self.integrity.clear();
        self.error = None;
        self.exists = dir.as_ref().is_some_and(|d| d.is_dir());
        self.backups = match &dir {
            Some(d) => match list_vault(d) {
                Ok(v) => v,
                Err(e) => {
                    self.error = Some(format!("Could not read {}: {e}", d.display()));
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        // Keep the selection when it is still there, else pick the newest.
        if !self
            .selected
            .as_ref()
            .is_some_and(|s| self.backups.iter().any(|b| &b.path == s))
        {
            self.selected = self.backups.first().map(|b| b.path.clone());
        }
        self.loaded = dir;
    }

    fn apply(
        &mut self,
        act: Option<Act>,
        image: Option<&Path>,
        dir: Option<&Path>,
    ) -> Option<PathBuf> {
        match act? {
            Act::Open(p) => return Some(p),
            Act::Refresh => {
                self.rescan(self.loaded.clone());
                self.msg = None;
            }
            Act::Choose => {
                let mut d = rfd::FileDialog::new().set_title("Choose a backup vault folder");
                if let Some(start) = dir
                    .filter(|d| d.is_dir())
                    .or_else(|| image.and_then(Path::parent))
                {
                    d = d.set_directory(start);
                }
                if let Some(p) = d.pick_folder() {
                    // Picking the folder that holds a vault means that vault.
                    let inner = p.join(VAULT_DIR);
                    let p = if inner.is_dir() && !looks_like_vault(&p) {
                        inner
                    } else {
                        p
                    };
                    self.chosen = Some(p);
                    self.msg = None;
                }
            }
            Act::UseDumpVault => {
                self.chosen = None;
                self.msg = None;
            }
            Act::Reveal(p) => {
                self.msg = reveal(&p)
                    .err()
                    .map(|e| Msg::Err(format!("Could not open the file manager: {e}")));
            }
            Act::Restore(i) => {
                let (Some(b), Some(dir)) = (self.backups.get(i), dir) else {
                    return None;
                };
                if let Some(m) = restore_copy(b, image, dir) {
                    self.msg = Some(m);
                }
            }
        }
        None
    }

    fn list_pane(
        &mut self,
        ui: &mut Ui,
        t: &Tokens,
        dir: &Path,
        image: Option<&Path>,
    ) -> Option<Act> {
        let mut act = None;
        let mut head = None;
        let n = self.backups.len();
        theme::panel(
            ui,
            t,
            "Backups",
            |ui| {
                if theme::icon_button(ui, t, icons::FOLDER_OPEN, "Choose another vault folder")
                    .clicked()
                {
                    head = Some(Act::Choose);
                }
                if theme::icon_button(ui, t, icons::ARROWS_CLOCKWISE, "Refresh").clicked() {
                    head = Some(Act::Refresh);
                }
                theme::pill(ui, t, &n.to_string(), Tone::Muted, true);
            },
            |ui| {
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let from_dump = self.chosen.is_none();
                            theme::pill(
                                ui,
                                t,
                                if from_dump {
                                    "beside the open dump"
                                } else {
                                    "chosen folder"
                                },
                                if from_dump { Tone::Accent } else { Tone::Muted },
                                false,
                            );
                            if !from_dump
                                && image.is_some()
                                && ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("Use the dump's vault")
                                                .font(font(UI_MEDIUM, 11.5))
                                                .color(t.accent),
                                        )
                                        .frame(false),
                                    )
                                    .clicked()
                            {
                                act = Some(Act::UseDumpVault);
                            }
                        });
                        ui.add_space(2.0);
                        path_line(ui, t, dir);
                    });
                theme::hline(ui, t.border_soft);

                if let Some(e) = &self.error {
                    egui::Frame::new()
                        .inner_margin(Margin::same(12))
                        .show(ui, |ui| {
                            callout(ui, t, icons::X_CIRCLE, t.crit, t.crit_soft, e)
                        });
                    return;
                }
                if self.backups.is_empty() {
                    ui.add_space(18.0);
                    egui::Frame::new()
                        .inner_margin(Margin::symmetric(14, 0))
                        .show(ui, |ui| {
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    RichText::new(icons::ARCHIVE).size(28.0).color(t.fg_faint),
                                );
                                ui.add_space(4.0);
                                let s = if self.exists {
                                    "No backups in this folder."
                                } else {
                                    "No vault here yet. OpenTune creates it on the first write \
                                     (checksum fix, map write-back or file flash)."
                                };
                                ui.add(Label::new(RichText::new(s).color(t.fg_dim)).wrap());
                            });
                        });
                    return;
                }
                let mut clicked = None;
                egui::ScrollArea::vertical()
                    .id_salt("vault-list")
                    .auto_shrink([false, false])
                    .show_rows(ui, ROW_H, n, |ui, range| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for i in range {
                            let b = &self.backups[i];
                            let (r, _) = ui.allocate_exact_size(
                                Vec2::new(ui.available_width(), ROW_H),
                                Sense::hover(),
                            );
                            let mut meta = vec![
                                b.when()
                                    .map(fmt_utc)
                                    .unwrap_or_else(|| "time unknown".into()),
                                crate::views::flash::size_label(b.size as usize),
                            ];
                            if let Some(c) = b.stored_crc() {
                                meta.push(format!("crc {c:08X}"));
                            }
                            let sel = self.selected.as_ref() == Some(&b.path);
                            let resp = list_row(
                                ui,
                                t,
                                r,
                                ui.make_persistent_id(("vault-row", &b.path)),
                                &b.label(),
                                &meta.join(" · "),
                                sel,
                            );
                            if resp.double_clicked() {
                                act = Some(Act::Open(b.path.clone()));
                            }
                            if resp.clicked() || resp.double_clicked() {
                                clicked = Some(b.path.clone());
                            }
                        }
                    });
                if clicked.is_some() {
                    self.selected = clicked;
                    self.msg = None;
                }
            },
        );
        act.or(head)
    }

    fn detail_pane(&mut self, ui: &mut Ui, t: &Tokens) -> Option<Act> {
        let idx = self
            .selected
            .as_ref()
            .and_then(|s| self.backups.iter().position(|b| &b.path == s));
        let Some(idx) = idx else {
            theme::panel(
                ui,
                t,
                "Backup",
                |_| {},
                |ui| {
                    theme::empty_state(
                        ui,
                        t,
                        icons::VAULT,
                        "Pick a backup",
                        "Select a backup on the left. Double-click opens it in Flash dump.",
                    )
                },
            );
            return None;
        };
        let b = self.backups[idx].clone();
        let integ = self
            .integrity
            .entry(b.path.clone())
            .or_insert_with(|| check_integrity(&b))
            .clone();
        let mut act = None;
        let mut head = None;
        theme::panel(
            ui,
            t,
            &b.label(),
            |ui| {
                if theme::primary_button(ui, t, Some(icons::ARROW_SQUARE_OUT), "Open in Flash dump")
                    .on_hover_text("Load this backup in the Flash dump view (read only)")
                    .clicked()
                {
                    head = Some(Act::Open(b.path.clone()));
                }
            },
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("vault-detail")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .inner_margin(Margin::symmetric(16, 14))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal_wrapped(|ui| {
                                    if theme::button(ui, t, Some(icons::COPY), "Restore copy…")
                                        .on_hover_text(
                                            "Write a copy of this backup to a new file you choose",
                                        )
                                        .clicked()
                                    {
                                        act = Some(Act::Restore(idx));
                                    }
                                    if theme::button(
                                        ui,
                                        t,
                                        Some(icons::FOLDER_OPEN),
                                        "Reveal in folder",
                                    )
                                    .clicked()
                                    {
                                        act = Some(Act::Reveal(b.path.clone()));
                                    }
                                });
                                ui.add_space(12.0);
                                if let Some(m) = &self.msg {
                                    show_msg(ui, t, m);
                                    ui.add_space(10.0);
                                }
                                integrity_callout(ui, t, &b, &integ);
                                ui.add_space(14.0);
                                details(ui, t, &b, &integ);
                            });
                    });
            },
        );
        act.or(head)
    }
}

/// True when `p` itself holds vault files (a `.bin` with a `.meta`).
fn looks_like_vault(p: &Path) -> bool {
    list_vault(p).is_ok_and(|v| v.iter().any(|b| b.has_meta))
}

/// "Restore copy…": ask for a new path, then copy the backup there. `None`
/// when the dialog was cancelled.
fn restore_copy(b: &Backup, image: Option<&Path>, vault: &Path) -> Option<Msg> {
    let stem = b
        .path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "backup".into());
    let mut d = rfd::FileDialog::new()
        .set_title("Restore a copy of the backup as")
        .set_file_name(format!("{stem}_restored.bin"))
        .add_filter("flash image", &["bin", "ori", "mod"]);
    if let Some(start) = image
        .and_then(Path::parent)
        .or_else(|| vault.parent())
        .filter(|p| p.is_dir())
    {
        d = d.set_directory(start);
    }
    // The dialog itself asks before replacing an existing file.
    let dest = d.save_file()?;
    if let Err(e) = check_restore_target(&dest, &b.path, image, vault) {
        return Some(Msg::Warn(e));
    }
    Some(match std::fs::copy(&b.path, &dest) {
        Ok(n) => Msg::Ok(format!(
            "Copied {} to {}. The backup is unchanged.",
            crate::views::flash::size_label(n as usize),
            dest.display()
        )),
        Err(e) => Msg::Err(format!("Could not write {}: {e}", dest.display())),
    })
}

fn integrity_callout(ui: &mut Ui, t: &Tokens, b: &Backup, i: &Integrity) {
    match i {
        Integrity::Intact => callout(
            ui,
            t,
            icons::SHIELD_CHECK,
            t.ok,
            t.ok_soft,
            "Intact: the data matches the CRC-32 and length recorded when it was saved.",
        ),
        Integrity::Changed { crc, len } => callout(
            ui,
            t,
            icons::SHIELD_WARNING,
            t.crit,
            t.crit_soft,
            &format!(
                "Changed since it was saved: CRC-32 is now {crc:08X} over {len} bytes, the \
                 sidecar recorded {} over {} bytes. Do not rely on this copy as an original.",
                b.stored_crc()
                    .map(|c| format!("{c:08X}"))
                    .unwrap_or_default(),
                b.get("length").unwrap_or("?"),
            ),
        ),
        Integrity::Unknown => callout(
            ui,
            t,
            icons::INFO,
            t.warn,
            t.crit_soft,
            if b.has_meta {
                "The sidecar records no CRC-32, so this copy cannot be checked."
            } else {
                "No .meta sidecar beside this file: when and from what it was taken is unknown."
            },
        ),
        Integrity::Unreadable(e) => callout(
            ui,
            t,
            icons::X_CIRCLE,
            t.crit,
            t.crit_soft,
            &format!("Could not read the backup: {e}"),
        ),
    }
}

fn details(ui: &mut Ui, t: &Tokens, b: &Backup, integ: &Integrity) {
    theme::section_label(ui, t, "Backup");
    ui.add_space(4.0);
    egui::Grid::new("vault-backup")
        .num_columns(2)
        .spacing(Vec2::new(18.0, 7.0))
        .show(ui, |ui| {
            kv(ui, t, "File", &b.name);
            kv(
                ui,
                t,
                "Folder",
                &b.path
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            );
            let when = match (b.get("unix_time").and_then(|v| v.parse().ok()), b.modified) {
                (Some(s), _) => fmt_utc(s),
                (None, Some(m)) => format!("{} (file time)", fmt_utc(m)),
                (None, None) => "–".into(),
            };
            kv(ui, t, "Saved", &when);
            kv(ui, t, "Size", &size_long(b.size));
            if let Some(a) = b.get("address").and_then(parse_u32) {
                let len = b
                    .get("length")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(b.size);
                kv(
                    ui,
                    t,
                    "Region",
                    &format!("0x{a:08X} – 0x{:08X}", (a as u64 + len).saturating_sub(1)),
                );
            }
            let crc = match (b.stored_crc(), integ) {
                (Some(c), Integrity::Intact) => format!("{c:08X} · verified"),
                (Some(c), Integrity::Changed { .. }) => format!("{c:08X} · MISMATCH"),
                (Some(c), _) => format!("{c:08X}"),
                (None, _) => "–".into(),
            };
            kv(ui, t, "CRC-32", &crc);
            kv(
                ui,
                t,
                "Sidecar",
                if b.has_meta {
                    "present (.meta)"
                } else {
                    "missing"
                },
            );
        });

    let ident: Vec<(&str, &str)> = [
        ("Label / ECU", "ecu_name"),
        ("SW number", "sw_number"),
        ("HW number", "hw_number"),
        ("VIN", "vin"),
    ]
    .into_iter()
    .filter_map(|(k, key)| b.get(key).map(|v| (k, v)))
    .collect();
    if !ident.is_empty() {
        ui.add_space(14.0);
        theme::section_label(ui, t, "Identification");
        ui.add_space(4.0);
        egui::Grid::new("vault-ident")
            .num_columns(2)
            .spacing(Vec2::new(18.0, 7.0))
            .show(ui, |ui| {
                for (k, v) in ident {
                    kv(ui, t, k, v);
                }
            });
    }
    let other: Vec<_> = b.other_meta().collect();
    if !other.is_empty() {
        ui.add_space(14.0);
        theme::section_label(ui, t, "Other sidecar fields");
        ui.add_space(4.0);
        egui::Grid::new("vault-other")
            .num_columns(2)
            .spacing(Vec2::new(18.0, 7.0))
            .show(ui, |ui| {
                for (k, v) in other {
                    kv(ui, t, k, if v.is_empty() { "–" } else { v });
                }
            });
    }
}

/// A unique, self-removing temp directory for tests.
#[cfg(test)]
pub(crate) struct TmpDir(pub PathBuf);

#[cfg(test)]
impl TmpDir {
    pub fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "ot-native-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}

#[cfg(test)]
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentune_core::{EcuId, MemoryRegion};

    #[test]
    fn crc32_matches_the_core_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn utc_formatting() {
        assert_eq!(utc_parts(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(utc_parts(1_700_000_000), (2023, 11, 14, 22, 13, 20));
        // Leap day.
        assert_eq!(utc_parts(951_782_400), (2000, 2, 29, 0, 0, 0));
        assert_eq!(fmt_utc(1_700_000_000), "2023-11-14 22:13 UTC");
    }

    #[test]
    fn sidecar_parsing_accepts_both_separators() {
        let m = parse_meta("ecu_name = demo\nvin = \n\nsaved: 2026-11-01T09:00:00Z\njunk\n");
        assert_eq!(
            m,
            vec![
                ("ecu_name".into(), "demo".into()),
                ("vin".into(), "".into()),
                ("saved".into(), "2026-11-01T09:00:00Z".into()),
            ]
        );
    }

    #[test]
    fn missing_vault_is_empty() {
        let d = TmpDir::new("vault-missing");
        assert!(list_vault(&d.0.join(VAULT_DIR)).unwrap().is_empty());
        assert_eq!(
            vault_dir_for(&d.0.join("stock.bin")),
            d.0.join("opentune-vault")
        );
        // A backup opened in Flash dump keeps showing its own vault.
        assert_eq!(
            vault_dir_for(&d.0.join("opentune-vault").join("x_00000000_1.bin")),
            d.0.join("opentune-vault")
        );
    }

    #[test]
    fn lists_core_backups_newest_first_with_metadata() {
        let d = TmpDir::new("vault-list");
        let vault = d.0.join(VAULT_DIR);
        // Written by the core's own writer, so the layout is the real one.
        let ecu = EcuId {
            ecu_name: Some("before flash stock".into()),
            sw_number: Some("SW-SYNTH-1".into()),
            ..Default::default()
        };
        let data: Vec<u8> = (0..4096u32).map(|i| (i * 7) as u8).collect();
        let core =
            opentune_flash::vault::save_backup(&vault, &ecu, MemoryRegion::new(0, 4096), &data)
                .unwrap();
        // Hand-written synthetic backups with explicit times (older / newer).
        std::fs::write(vault.join("old_00000000_100.bin"), [1u8; 16]).unwrap();
        std::fs::write(
            vault.join("old_00000000_100.meta"),
            "ecu_name = old\naddress = 0x00000000\nlength = 16\ncrc32 = 0x00000000\nunix_time = 100\n",
        )
        .unwrap();
        std::fs::write(vault.join("future_00000000_9999999999.bin"), [2u8; 8]).unwrap();
        std::fs::write(
            vault.join("future_00000000_9999999999.meta"),
            "ecu_name: future\nunix_time: 9999999999\nreason: synthetic\n",
        )
        .unwrap();
        // A bare .bin (no sidecar) and files that are not backups.
        std::fs::write(vault.join("bare.BIN"), [3u8; 4]).unwrap();
        std::fs::write(vault.join("notes.txt"), "x").unwrap();
        std::fs::write(vault.join("orphan.meta"), "ecu_name = orphan\n").unwrap();

        let list = list_vault(&vault).unwrap();
        let names: Vec<&str> = list.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(list.len(), 4, "{names:?}");
        assert_eq!(list[0].label(), "future");
        assert_eq!(list.last().unwrap().label(), "old");

        let c = list.iter().find(|b| b.path == core).unwrap();
        assert!(c.has_meta);
        assert_eq!(c.size, 4096);
        assert_eq!(c.label(), "before flash stock");
        assert_eq!(c.get("sw_number"), Some("SW-SYNTH-1"));
        assert_eq!(c.get("vin"), None, "empty values read as absent");
        assert_eq!(c.stored_crc(), Some(crc32(&data)));
        assert_eq!(check_integrity(c), Integrity::Intact);
        assert_eq!(c.other_meta().count(), 0);

        let bare = list.iter().find(|b| b.name == "bare.BIN").unwrap();
        assert!(!bare.has_meta);
        assert_eq!(bare.label(), "bare");
        assert!(bare.when().is_some(), "falls back to the file time");
        assert_eq!(check_integrity(bare), Integrity::Unknown);

        // A recorded CRC that does not fit the data is reported, not hidden.
        let old = list.iter().find(|b| b.label() == "old").unwrap();
        assert!(matches!(
            check_integrity(old),
            Integrity::Changed { len: 16, .. }
        ));
        assert_eq!(
            list[0].other_meta().collect::<Vec<_>>(),
            vec![&("reason".to_string(), "synthetic".to_string())]
        );
    }

    #[test]
    fn restore_target_rules() {
        let d = TmpDir::new("vault-restore");
        let vault = d.0.join(VAULT_DIR);
        std::fs::create_dir_all(&vault).unwrap();
        let img = d.0.join("stock.bin");
        let bak = vault.join("x_00000000_1.bin");
        std::fs::write(&img, [0u8; 4]).unwrap();
        std::fs::write(&bak, [0u8; 4]).unwrap();
        assert!(check_restore_target(&bak, &bak, Some(&img), &vault).is_err());
        assert!(check_restore_target(&img, &bak, Some(&img), &vault).is_err());
        assert!(check_restore_target(&vault.join("new.bin"), &bak, Some(&img), &vault).is_err());
        assert!(check_restore_target(&d.0.join("restored.bin"), &bak, Some(&img), &vault).is_ok());
        assert!(check_restore_target(&d.0.join("restored.bin"), &bak, None, &vault).is_ok());
    }
}
