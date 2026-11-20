//! Decode a CAN log offline with a controller definition's frame sets:
//! candump, OpenTune CSV, SavvyCAN CSV or Vector ASC, through
//! `opentune_hub::controllers::decode_text` (the same reader and decoder as
//! `opentune controller decode` and the webview app's `ctrl_decode_log`).
//! Nothing is transmitted. The decode runs on a worker thread.

use crate::icons;
use crate::theme::{self, font, Tokens, Tone, MONO_MEDIUM, UI_MEDIUM, UI_STRONG};
use eframe::egui::{self, Label, RichText, Ui, Vec2};
use opentune_ctrldef::ControllerDef;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// Decoded frames returned (the summary covers all) and drawn at most.
const ROWS: usize = 2000;
const ROWS_SHOWN: usize = 400;
const ROW_H: f32 = 18.0;

/// The decode card's state (one definition at a time).
#[derive(Default)]
pub(crate) struct Decode {
    /// The definition the frame-set choice and result belong to.
    def_path: Option<PathBuf>,
    log: Option<PathBuf>,
    /// Frame set; empty = the only set, or all sets.
    bus: String,
    rx: Option<mpsc::Receiver<Result<Value, String>>>,
    result: Option<Result<Value, String>>,
}

/// Read and decode `log` with `def` (blocking).
pub(crate) fn decode_file(def: &ControllerDef, log: &Path, bus: &str) -> Result<Value, String> {
    let bytes = std::fs::read(log).map_err(|e| format!("read '{}': {e}", log.display()))?;
    let mut v = opentune_hub::controllers::decode_text(
        def,
        &String::from_utf8_lossy(&bytes),
        Some(bus),
        ROWS,
    )?;
    v["log"] = Value::String(log.display().to_string());
    Ok(v)
}

/// A number without trailing noise (12, 12.5, 0.004), like the CLI.
fn fmt_num(v: f64) -> String {
    opentune_ctrldef::decode::fmt_num(v)
}

/// A decoded value (`{type, value}`) as text, like the CLI.
pub(crate) fn fmt_value(v: &Value) -> String {
    let x = &v["value"];
    match v["type"].as_str() {
        Some("number") => x.as_f64().map(fmt_num).unwrap_or_default(),
        Some("bool") => if x.as_bool() == Some(true) { "1" } else { "0" }.into(),
        Some("enum") => x.as_str().unwrap_or("?").into(),
        Some("ascii") => format!("'{}'", x.as_str().unwrap_or_default()),
        Some("invalid") => "n/a".into(),
        _ => x.as_str().map(str::to_string).unwrap_or_default(),
    }
}

/// Signal statistics as one line: range, most frequent states, last value.
pub(crate) fn stats_text(s: &Value, unit: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let (Some(lo), Some(hi)) = (s["min"].as_f64(), s["max"].as_f64()) {
        let u = if unit.is_empty() {
            String::new()
        } else {
            format!(" {unit}")
        };
        parts.push(format!("{} .. {}{u}", fmt_num(lo), fmt_num(hi)));
    }
    if let Some(c) = s["counts"].as_object() {
        let mut counts: Vec<(&String, u64)> = c
            .iter()
            .map(|(k, n)| (k, n.as_u64().unwrap_or(0)))
            .collect();
        counts.sort_by_key(|c| std::cmp::Reverse(c.1));
        if !counts.is_empty() {
            let mut l: Vec<String> = counts
                .iter()
                .take(6)
                .map(|(k, n)| format!("{k} ×{n}"))
                .collect();
            if counts.len() > 6 {
                l.push(format!("+{} more", counts.len() - 6));
            }
            parts.push(l.join(", "));
        }
    }
    let mut out = parts.join(", ");
    if s["last"].is_object() {
        if !out.is_empty() {
            out.push_str("  ");
        }
        out.push_str(&format!("last {}", fmt_value(&s["last"])));
    }
    if let Some(n) = s["invalid"].as_u64().filter(|n| *n > 0) {
        out.push_str(&format!(" ({n} n/a)"));
    }
    out
}

/// One decoded signal of a frame row (`name=value unit`).
fn signal_text(s: &Value) -> String {
    let v = fmt_value(&s["value"]);
    let unit = s["unit"].as_str().unwrap_or_default();
    if s["value"]["type"] == "number" && !unit.is_empty() && unit != "raw" {
        format!("{}={v} {unit}", s["name"].as_str().unwrap_or_default())
    } else {
        format!("{}={v}", s["name"].as_str().unwrap_or_default())
    }
}

fn note(ui: &mut Ui, t: &Tokens, tone: Tone, s: &str) {
    let c = match tone {
        Tone::Ok => t.ok,
        Tone::Warn => t.warn,
        Tone::Crit => t.crit,
        Tone::Accent => t.accent,
        Tone::Muted => t.fg_dim,
    };
    ui.add(Label::new(RichText::new(s).font(font(UI_MEDIUM, 12.5)).color(c)).wrap());
}

fn mono(ui: &mut Ui, t: &Tokens, s: &str) {
    ui.add(Label::new(RichText::new(s).font(font(MONO_MEDIUM, 11.5)).color(t.fg)).wrap());
}

impl Decode {
    fn poll(&mut self) {
        if let Some(rx) = &self.rx {
            match rx.try_recv() {
                Ok(r) => {
                    self.result = Some(r);
                    self.rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.result = Some(Err("the decode stopped unexpectedly".into()));
                    self.rx = None;
                }
            }
        }
    }

    /// Start decoding the chosen log with `def` on a worker thread.
    pub(crate) fn start(&mut self, ctx: &egui::Context, def: &ControllerDef) {
        let Some(log) = self.log.clone() else { return };
        let (tx, rx) = mpsc::channel();
        let def = def.clone();
        let bus = self.bus.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(decode_file(&def, &log, &bus));
            ctx.request_repaint();
        });
        self.rx = Some(rx);
        self.result = None;
    }

    #[cfg(test)]
    pub(crate) fn wait(&mut self) -> Option<&Result<Value, String>> {
        for _ in 0..1000 {
            self.poll();
            if self.rx.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        self.result.as_ref()
    }

    /// The decode card for the definition at `path`.
    pub(crate) fn ui(&mut self, ui: &mut Ui, t: &Tokens, path: &Path, def: &ControllerDef) {
        if self.def_path.as_deref() != Some(path) {
            self.def_path = Some(path.to_path_buf());
            self.result = None;
            self.rx = None;
            self.bus = if def.buses.len() == 1 {
                def.buses[0].name.clone()
            } else {
                String::new()
            };
        }
        self.poll();
        if self.rx.is_some() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
        note(
            ui,
            t,
            Tone::Muted,
            "A candump log, OpenTune CSV, SavvyCAN CSV or Vector ASC capture, decoded with this \
             definition's frame sets — the same decoder as `opentune controller decode`. \
             Offline: nothing is sent.",
        );
        ui.horizontal_wrapped(|ui| {
            if theme::button(ui, t, Some(icons::FOLDER_OPEN), "Choose log…").clicked() {
                if let Some(p) = rfd::FileDialog::new()
                    .set_title("CAN log")
                    .add_filter("CAN log", &["log", "csv", "asc", "txt"])
                    .add_filter("All files", &["*"])
                    .pick_file()
                {
                    self.log = Some(p);
                    self.result = None;
                }
            }
            let name = self
                .log
                .as_deref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "no log chosen".into());
            ui.label(
                RichText::new(name)
                    .font(font(MONO_MEDIUM, 11.5))
                    .color(t.fg_dim),
            );
            if def.buses.len() > 1 {
                egui::ComboBox::from_id_salt("ctrl-decode-bus")
                    .selected_text(if self.bus.is_empty() {
                        "all sets"
                    } else {
                        self.bus.as_str()
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.bus, String::new(), "all sets");
                        for b in &def.buses {
                            ui.selectable_value(&mut self.bus, b.name.clone(), &b.name);
                        }
                    });
            }
            let ok = self.log.is_some() && self.rx.is_none();
            let r = ui.add_enabled_ui(ok, |ui| {
                theme::primary_button(ui, t, Some(icons::PLAY), "Decode")
            });
            if r.inner.clicked() {
                self.start(ui.ctx(), def);
            }
            if self.rx.is_some() {
                ui.spinner();
            }
        });
        match &self.result {
            None => {}
            Some(Err(e)) => note(ui, t, Tone::Crit, e),
            Some(Ok(v)) => result(ui, t, v),
        }
    }
}

fn result(ui: &mut Ui, t: &Tokens, r: &Value) {
    let n = |k: &str| r[k].as_u64().unwrap_or(0);
    let file = r["log"]
        .as_str()
        .map(|p| {
            Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string())
        })
        .unwrap_or_default();
    let mut frames = format!(
        "{} in the log, {} decoded",
        n("frames_in_log"),
        n("decoded")
    );
    if n("bad_lines") > 0 {
        frames.push_str(&format!(
            ", {} unreadable line{} skipped",
            n("bad_lines"),
            if n("bad_lines") == 1 { "" } else { "s" }
        ));
    }
    ui.add_space(6.0);
    egui::Grid::new("ctrl-decode-head")
        .num_columns(2)
        .spacing(Vec2::new(18.0, 5.0))
        .show(ui, |ui| {
            crate::views::maps::kv(ui, t, "log", &file);
            crate::views::maps::kv(ui, t, "frame set", r["bus"].as_str().unwrap_or("all"));
            crate::views::maps::kv(ui, t, "frames", &frames);
            if n("undecoded_total") > 0 {
                let ids: Vec<String> = r["undecoded_ids"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|x| {
                                format!(
                                    "{} ×{}",
                                    x["id"].as_str().unwrap_or("?"),
                                    x["count"].as_u64().unwrap_or(0)
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let more = if ids.len() >= 12 { " …" } else { "" };
                crate::views::maps::kv(ui, t, "other ids", &format!("{}{more}", ids.join(", ")));
            }
        });
    if r["unconfirmed"].as_bool() == Some(true) {
        note(
            ui,
            t,
            Tone::Warn,
            &format!(
                "{} is {}: its frame layouts are not yet checked against a capture from a real car.",
                r["controller"].as_str().unwrap_or("this definition"),
                r["validation"].as_str().unwrap_or("unconfirmed")
            ),
        );
    }
    if n("decoded") == 0 {
        note(
            ui,
            t,
            Tone::Muted,
            "No frame of this definition's set(s) in the log. Check the bus the capture was \
             taken on and the frame set.",
        );
        return;
    }
    let head = |ui: &mut Ui, s: &str| {
        ui.add_space(8.0);
        ui.label(RichText::new(s).font(font(UI_STRONG, 12.5)).color(t.fg));
    };
    if let Some(live) = r["live"].as_array().filter(|a| !a.is_empty()) {
        head(ui, "Live channels");
        egui::Grid::new("ctrl-decode-live")
            .striped(true)
            .num_columns(3)
            .spacing(Vec2::new(14.0, 4.0))
            .show(ui, |ui| {
                for l in live {
                    ui.label(l["name"].as_str().unwrap_or_default());
                    mono(ui, t, l["source"].as_str().unwrap_or_default());
                    ui.label(stats_text(l, l["unit"].as_str().unwrap_or_default()));
                    ui.end_row();
                }
            });
    }
    head(ui, "Per frame");
    for (i, f) in r["per_frame"].as_array().into_iter().flatten().enumerate() {
        let mut title = format!(
            "{}  {}  ×{}",
            f["id"].as_str().unwrap_or_default(),
            f["name"].as_str().unwrap_or_default(),
            f["count"].as_u64().unwrap_or(0)
        );
        if let Some(s) = f["span"].as_f64() {
            title.push_str(&format!(" · {} s span", fmt_num(s)));
        }
        egui::CollapsingHeader::new(RichText::new(title).font(font(MONO_MEDIUM, 11.5)))
            .id_salt(("ctrl-decode-frame", i))
            .default_open(true)
            .show(ui, |ui| {
                egui::Grid::new(("ctrl-decode-sigs", i))
                    .num_columns(2)
                    .spacing(Vec2::new(14.0, 3.0))
                    .show(ui, |ui| {
                        for s in f["signals"].as_array().into_iter().flatten() {
                            mono(ui, t, s["name"].as_str().unwrap_or_default());
                            ui.label(stats_text(s, s["unit"].as_str().unwrap_or_default()));
                            ui.end_row();
                        }
                    });
            });
    }
    head(ui, "Frames");
    let rows: Vec<&Value> = r["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .take(ROWS_SHOWN)
        .collect();
    let total = n("rows_total");
    if total > rows.len() as u64 {
        note(
            ui,
            t,
            Tone::Muted,
            &format!(
                "Showing the first {} of {total} decoded frames.",
                rows.len()
            ),
        );
    }
    egui::ScrollArea::vertical()
        .id_salt("ctrl-decode-rows")
        .max_height(320.0)
        .auto_shrink([false, true])
        .show_rows(ui, ROW_H, rows.len(), |ui, range| {
            for x in &rows[range] {
                let ts = x["t"]
                    .as_f64()
                    .map(|t| format!("{t:.3}"))
                    .unwrap_or_default();
                let sigs: Vec<String> = x["signals"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(signal_text)
                    .collect();
                ui.label(
                    RichText::new(format!(
                        "{ts:>10}  {:<6} {:<22} {}",
                        x["id"].as_str().unwrap_or_default(),
                        x["frame"].as_str().unwrap_or_default(),
                        sigs.join("  ")
                    ))
                    .font(font(MONO_MEDIUM, 11.0))
                    .color(t.fg),
                );
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic definition: one frame set, one frame, a number and an enum.
    fn synthetic_def() -> ControllerDef {
        serde_json::from_value(serde_json::json!({
            "id": "synthetic-coupling", "name": "Synthetic coupling", "kind": "awd",
            "maker": "Acme", "family": "SYN", "validation": "synthetic-tested",
            "safety": ["synthetic"],
            "sources": [{"title": "t", "license": "facts", "used_for": "u"}],
            "buses": [{"name": "main", "bitrate": 500000, "frames": [{
                "id": "0x123", "name": "State", "sender": "Acme", "min_len": 2, "validation": "synthetic-tested",
                "signals": [
                    {"name": "lock", "start": 0, "len": 8, "kind": "number", "unit": "%"},
                    {"name": "mode", "start": 8, "len": 8, "kind": "enum",
                     "values": [{"raw": 0, "name": "off"}, {"raw": 1, "name": "on"}]}
                ]
            }]}]
        }))
        .unwrap_or_else(|e| panic!("ctrldef: {e}"))
    }

    #[test]
    fn a_synthetic_log_decodes_on_the_worker() {
        let dir = std::env::temp_dir().join(format!("ot-n3-decode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("cap.log");
        std::fs::write(
            &log,
            "(1000.000000) can0 123#1000\n(1000.500000) can0 123#3201\n(1001.000000) can0 456#00\nnot a frame\n",
        )
        .unwrap();
        let def = synthetic_def();
        let mut d = Decode {
            log: Some(log.clone()),
            ..Default::default()
        };
        d.start(&egui::Context::default(), &def);
        let v = d.wait().unwrap().clone().unwrap();
        assert_eq!(v["frames_in_log"], 3);
        assert_eq!(v["decoded"], 2);
        assert_eq!(v["bad_lines"], 1);
        assert_eq!(v["undecoded_ids"][0]["id"], "0x456");
        let f = &v["per_frame"][0];
        let lock = f["signals"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "lock")
            .unwrap();
        assert_eq!(stats_text(lock, "%"), "16 .. 50 %  last 50");
        let mode = f["signals"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "mode")
            .unwrap();
        let m = stats_text(mode, "");
        assert!(
            m.contains("off ×1") && m.contains("on ×1") && m.ends_with("last on"),
            "{m}"
        );
        let row = &v["rows"][1];
        let sigs: Vec<String> = row["signals"]
            .as_array()
            .unwrap()
            .iter()
            .map(signal_text)
            .collect();
        assert_eq!(sigs, ["lock=50 %", "mode=on"]);
        // A missing log is an error result, not a panic.
        d.log = Some(dir.join("missing.log"));
        d.start(&egui::Context::default(), &def);
        assert!(d.wait().unwrap().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn value_formatting() {
        let v = |j: serde_json::Value| fmt_value(&j);
        assert_eq!(
            v(serde_json::json!({"type": "number", "value": 12.5})),
            "12.5"
        );
        assert_eq!(v(serde_json::json!({"type": "number", "value": 3.0})), "3");
        assert_eq!(v(serde_json::json!({"type": "bool", "value": true})), "1");
        assert_eq!(v(serde_json::json!({"type": "enum", "value": null})), "?");
        assert_eq!(v(serde_json::json!({"type": "ascii", "value": "A"})), "'A'");
        assert_eq!(v(serde_json::json!({"type": "invalid"})), "n/a");
        let s = serde_json::json!({"invalid": 2, "counts": {"a": 1, "b": 5}});
        assert_eq!(stats_text(&s, ""), "b ×5, a ×1 (2 n/a)");
    }
}
