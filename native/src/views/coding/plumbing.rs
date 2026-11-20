//! Shared plumbing of the live-ECU module-service panels (coding, auto-scan,
//! adaptation, actuator tests, service functions; batch N5): which module a
//! panel addresses, where its vault backups and DTC reports go, the security
//! inputs, and small parsing helpers. Nothing here talks to an adapter.
//!
//! **Which module.** A definition may name its module (`req_id` / `resp_id`,
//! or a KWP `kwp_addr`). Reads go to that module (the read API takes any
//! target). An ECU change is confirmed with [`Host::confirm_ecu`], whose
//! [`Ack`](crate::host::Ack) carries the APP's target (Settings → Adapter
//! settings), and `mutate` / `actuate` address exactly that. So a panel only
//! offers a change when the module it means IS the app's target
//! ([`change_blocker`]); otherwise it says which target to set. Nothing is
//! ever re-addressed behind the user's back.

use crate::host::{DiagTarget, Host, Transport, Unlocker};
use crate::icons;
use crate::theme::{self, font, MONO_MEDIUM, UI_MEDIUM};
use crate::views::vault::{utc_parts, vault_dir_for, VAULT_DIR};
use eframe::egui::{self, RichText, Ui};
use opentune_core::RawDiag;
use std::io::Write;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Uppercase space-separated hex.
pub(crate) fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whitespace-tolerant hex; empty input is an empty vector.
pub(crate) fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !clean.len().is_multiple_of(2) {
        return Err(format!("hex '{s}' has an odd number of digits"));
    }
    (0..clean.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&clean[i..i + 2], 16).map_err(|_| format!("invalid hex in '{s}'"))
        })
        .collect()
}

/// A number, decimal or `0x` hex, at most `max`.
pub(crate) fn parse_num(what: &str, s: &str, max: u64) -> Result<u64, String> {
    let t = s.trim();
    let v = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => t.parse().ok(),
    };
    v.filter(|v| *v <= max)
        .ok_or_else(|| format!("{what} '{s}': expected 0..={max} (decimal or 0x hex)"))
}

/// The module a definition file names with top-level `req_id` / `resp_id`
/// (hex strings or numbers), as an ISO-TP target.
pub(crate) fn def_module(text: &str) -> Result<Option<DiagTarget>, String> {
    Ok(opentune_hub::adapter::def_addr(text)?.map(|a| DiagTarget::can(a.tx_id, a.rx_id)))
}

/// The module a panel addresses: the definition's own, else the app target.
pub(crate) fn module_of(host: &Host<'_>, named: Option<DiagTarget>) -> Result<DiagTarget, String> {
    match named {
        Some(t) => Ok(t),
        None => host.target(),
    }
}

/// Why an ECU change on `module` cannot be confirmed now (`None`: it can).
/// The confirmation, and so the write, goes to the app's target only.
pub(crate) fn change_blocker(host: &Host<'_>, module: DiagTarget) -> Option<String> {
    match host.target() {
        Err(e) => Some(format!("no usable target module: {e}")),
        Ok(t) if t != module => Some(format!(
            "this changes module {module}, but the app's target is {t}. Set Settings → \
             Adapter settings to {module} first: a confirmed change goes to the app's \
             target only, never to another module."
        )),
        Ok(_) => None,
    }
}

/// The transport the desktop's `kwp_key` picks for a KWP2000 module by the
/// connected driver: a TP2.0 channel on a raw-CAN cable, the K-line address
/// on a K-line cable; any other cable is refused.
pub(crate) fn kwp_target(driver: &str, addr: u8) -> Result<DiagTarget, String> {
    use opentune_adaptation::kwp::{KLINE_DRIVERS, TP20_DRIVERS};
    if TP20_DRIVERS.contains(&driver) {
        Ok(DiagTarget::tp20(addr))
    } else if KLINE_DRIVERS.contains(&driver) {
        Ok(DiagTarget::kline(addr))
    } else {
        Err(format!(
            "the connected cable '{driver}' cannot carry VAG KWP2000: use a raw-CAN adapter \
             for TP2.0 ({}) or a K-line adapter ({})",
            TP20_DRIVERS.join(", "),
            KLINE_DRIVERS.join(", ")
        ))
    }
}

/// `22 <did>` with the echo checked: the record.
pub(crate) fn read_did(d: &mut dyn RawDiag, did: u16) -> Result<Vec<u8>, String> {
    let r = d.request(&[0x22, (did >> 8) as u8, did as u8])?;
    match r.as_slice() {
        [0x62, h, l, rest @ ..] if *h == (did >> 8) as u8 && *l == did as u8 => Ok(rest.to_vec()),
        [0x7F, 0x22, nrc, ..] => Err(format!("read DID 0x{did:04X}: NRC 0x{nrc:02X}")),
        other => Err(format!(
            "read DID 0x{did:04X}: unexpected reply {other:02X?}"
        )),
    }
}

/// Is `t` an ISO-TP (UDS on CAN) target? The UDS panels refuse the others.
pub(crate) fn is_isotp(t: DiagTarget) -> bool {
    t.transport == Transport::IsoTp
}

// ---------------------------------------------------------------------------
// SecurityAccess inputs
// ---------------------------------------------------------------------------

/// The optional seed/key id + request-seed level a write may unlock with.
/// Blank id: no SecurityAccess is sent (the module's refusal is shown).
pub(crate) struct SecurityInput {
    pub id: String,
    pub level: String,
}

impl Default for SecurityInput {
    fn default() -> Self {
        Self {
            id: String::new(),
            level: "01".into(),
        }
    }
}

impl SecurityInput {
    /// The unlocker and level, resolved before anything is sent; `None` when
    /// no id is given.
    pub(crate) fn resolve(&self, host: &Host<'_>) -> Result<Option<(Unlocker, u8)>, String> {
        let id = self.id.trim();
        if id.is_empty() {
            return Ok(None);
        }
        let level = u8::from_str_radix(
            self.level
                .trim()
                .trim_start_matches("0x")
                .trim_start_matches("0X"),
            16,
        )
        .map_err(|_| format!("security level '{}': expected hex, e.g. 01", self.level))?;
        Ok(Some((host.unlocker_for(id)?, level)))
    }

    pub(crate) fn ui(&mut self, ui: &mut Ui, host: &Host<'_>) {
        let t = host.tokens();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Seed/key id").color(t.fg_dim));
            ui.add(
                egui::TextEdit::singleline(&mut self.id)
                    .desired_width(140.0)
                    .hint_text("none"),
            );
            ui.label(RichText::new("level").color(t.fg_dim));
            ui.add(egui::TextEdit::singleline(&mut self.level).desired_width(36.0));
        })
        .response
        .on_hover_text(
            "Optional UDS SecurityAccess before the change (27 level → key → 27 level+1), \
             through a configured seed/key resolver. OpenTune ships no algorithm. A \
             rejected key is never retried.",
        );
    }
}

// ---------------------------------------------------------------------------
// Where backups and records go
// ---------------------------------------------------------------------------

/// The folder vault backups (and DTC reports) are written to: the user's
/// choice, else the vault beside the opened dump. With neither, every
/// persistent change and DTC clear is refused (nothing to keep the old data
/// in).
#[derive(Default)]
pub(crate) struct Records {
    chosen: Option<PathBuf>,
}

impl Records {
    #[cfg(test)]
    pub(crate) fn in_dir(dir: &Path) -> Self {
        Self {
            chosen: Some(dir.to_path_buf()),
        }
    }

    /// What `Host::backup` takes as `near` (a directory or the dump).
    pub(crate) fn near(&self, host: &Host<'_>) -> Option<PathBuf> {
        self.chosen
            .clone()
            .or_else(|| host.image_path().map(Path::to_path_buf))
    }

    pub(crate) fn ui(&mut self, ui: &mut Ui, host: &Host<'_>) {
        let t = *host.tokens();
        ui.horizontal_wrapped(|ui| {
            let near = self.near(host);
            match &near {
                Some(n) => {
                    ui.label(RichText::new("Backups:").color(t.fg_dim));
                    ui.label(
                        RichText::new(vault_dir(n).display().to_string())
                            .font(font(MONO_MEDIUM, 11.5))
                            .color(t.fg),
                    );
                }
                None => {
                    ui.label(
                        RichText::new(format!(
                            "{}  choose a folder for backups and reports: changes are refused \
                             without one",
                            icons::WARNING
                        ))
                        .font(font(UI_MEDIUM, 12.5))
                        .color(t.warn),
                    );
                }
            }
            if theme::button(ui, &t, Some(icons::FOLDER_OPEN), "Folder…").clicked() {
                if let Some(d) = host.pick_dir("Folder for ECU backups and reports") {
                    self.chosen = Some(d);
                }
            }
        });
    }
}

/// The vault directory `Host::backup(near, …)` writes into.
pub(crate) fn vault_dir(near: &Path) -> PathBuf {
    if near.is_dir() {
        near.join(VAULT_DIR)
    } else {
        vault_dir_for(near)
    }
}

/// `YYYYMMDD-HHMMSS` (UTC) of now.
fn stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = utc_parts(secs);
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

/// Save a DTC report (a panel record, not a vault backup) as a NEW file
/// `<vault>/dtc-reports/<time>-<label>.txt`. An existing file is never
/// replaced (a numbered name is used instead).
pub(crate) fn save_dtc_report(near: &Path, label: &str, text: &str) -> Result<PathBuf, String> {
    let dir = vault_dir(near).join("dtc-reports");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let clean: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let base = format!("{}-{}", stamp(), clean.trim_matches('-'));
    for n in 0..1000 {
        let name = if n == 0 {
            format!("{base}.txt")
        } else {
            format!("{base}-{n}.txt")
        };
        let p = dir.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
        {
            Ok(mut f) => {
                f.write_all(text.as_bytes())
                    .and_then(|_| f.sync_all())
                    .map_err(|e| format!("write {}: {e}", p.display()))?;
                return Ok(p);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("create {}: {e}", p.display())),
        }
    }
    Err(format!("no free report name in {}", dir.display()))
}

/// The header of a DTC report.
pub(crate) fn report_header(what: &str, module: &str, target: DiagTarget) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!(
        "OpenTune DTC report: {what}\nmodule: {module}\ntarget: {target}\ntime: {}\n",
        crate::views::vault::fmt_utc(secs)
    )
}

// ---------------------------------------------------------------------------
// Small UI helpers
// ---------------------------------------------------------------------------

/// A one-line message in a tone.
pub(crate) fn note(ui: &mut Ui, host: &Host<'_>, tone: theme::Tone, text: &str) {
    let t = host.tokens();
    let c = match tone {
        theme::Tone::Ok => t.ok,
        theme::Tone::Warn => t.warn,
        theme::Tone::Crit => t.crit,
        theme::Tone::Accent => t.accent,
        theme::Tone::Muted => t.fg_dim,
    };
    ui.label(RichText::new(text).font(font(UI_MEDIUM, 12.5)).color(c));
}

/// A monospace line.
pub(crate) fn mono(ui: &mut Ui, host: &Host<'_>, text: &str) {
    let t = host.tokens();
    ui.label(
        RichText::new(text)
            .font(font(MONO_MEDIUM, 11.5))
            .color(t.fg),
    );
}

/// Take a finished job's result (`None` while it runs or when there is none).
pub(crate) fn take<T: 'static>(
    job: &mut Option<crate::host::JobHandle<T>>,
) -> Option<Result<T, String>> {
    let r = job.as_mut()?.take()?;
    *job = None;
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing_and_targets() {
        assert_eq!(parse_hex("01 0a FF").unwrap(), vec![1, 10, 255]);
        assert!(parse_hex("abc").is_err());
        assert_eq!(parse_num("v", "0x10", 0xFF).unwrap(), 16);
        assert!(parse_num("v", "300", 0xFF).is_err());
        let d = def_module(r#"{"req_id":"70E","resp_id":"778"}"#).unwrap();
        assert_eq!(d, Some(DiagTarget::can(0x70E, 0x778)));
        assert_eq!(def_module(r#"{"id":"x"}"#).unwrap(), None);
        assert_eq!(kwp_target("socketcan", 2).unwrap(), DiagTarget::tp20(2));
        assert_eq!(kwp_target("kkl", 1).unwrap(), DiagTarget::kline(1));
        assert!(kwp_target("elm327obd", 1).is_err());
    }

    #[test]
    fn dtc_reports_are_new_files_never_replacements() {
        let dir = std::env::temp_dir().join(format!("ot-n5-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = save_dtc_report(&dir, "Engine 7E0", "one").unwrap();
        let b = save_dtc_report(&dir, "Engine 7E0", "two").unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "two");
        assert!(a.starts_with(dir.join(VAULT_DIR).join("dtc-reports")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
