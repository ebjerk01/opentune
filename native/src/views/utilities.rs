//! Utilities: VIN decoder, part-number breakdown, DTC lookup and the
//! seed/key calculator. Port of the webview app's Utilities panel
//! (`utilities.rs`, `app-tools.js`). Offline: nothing is sent anywhere.
//!
//! The seed/key calculator is the user's own: a generic, fully transparent
//! transform with the user's parameters (`opentune_hub::seedkeys::generic`),
//! or an id the app's seed/key lookup resolves (a plugin algorithm, the SA2
//! interpreter over the user's own script, or the Bench-tier definitions
//! loader), labelled as such. OpenTune bundles no OEM algorithm, so an id no
//! resolver knows is an honest error, never a fabricated key.

use crate::host::{Host, NativeView, Tier, ViewGroup};
use crate::icons;
use crate::theme::{self, font, Tokens, Tone, MONO_MEDIUM, UI_MEDIUM, UI_STRONG};
use eframe::egui::{self, Label, RichText, Ui, Vec2};
use opentune_hub::seedkeys::{generic, SeedKeys};

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Public;

pub fn create() -> Box<dyn NativeView> {
    Box::new(UtilitiesView::default())
}

// ---------------------------------------------------------------------------
// Operations (no UI)
// ---------------------------------------------------------------------------

fn hexs(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Hex bytes, ignoring everything that is not a hex digit (`AA BB`, `aa:bb`).
fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if cleaned.is_empty() {
        return Err("empty seed".into());
    }
    if !cleaned.len().is_multiple_of(2) {
        return Err("hex has an odd number of digits".into());
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

/// A number, `0x`-prefixed hex or decimal.
fn parse_u32(s: &str) -> Result<u32, String> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u32::from_str_radix(h, 16).map_err(|e| format!("'{s}': {e}")),
        None => s.parse::<u32>().map_err(|e| format!("'{s}': {e}")),
    }
}

/// One generic transform, as the calculator lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Algo {
    pub id: String,
    pub name: String,
    /// `mask`, `add`.
    pub params: Vec<String>,
    pub desc: String,
}

/// The generic transforms (`generic::catalog`).
pub(crate) fn algos() -> Vec<Algo> {
    let s = |v: &serde_json::Value, k: &str| v[k].as_str().unwrap_or_default().to_string();
    generic::catalog()
        .as_array()
        .map(|a| {
            a.iter()
                .map(|v| Algo {
                    id: s(v, "id"),
                    name: s(v, "name"),
                    params: v["params"]
                        .as_array()
                        .map(|p| {
                            p.iter()
                                .filter_map(|x| x.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                    desc: s(v, "desc"),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Who computed a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeySource {
    /// A generic transform with the user's parameters.
    Generic,
    /// The SA2 interpreter over the user's own script.
    Sa2,
    /// A plugin-supplied algorithm (or the Bench-tier definitions loader).
    Plugin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeyResult {
    pub key: String,
    pub algo: String,
    pub source: KeySource,
    /// The access level passed to a resolved algorithm.
    pub level: Option<u8>,
    pub note: String,
}

/// Compute a key from `seed` with the generic transform `algo`, or, when
/// `algo` is not a generic id, with the algorithm the app's lookup resolves
/// for it (`level` = the request-seed access level, default `0x01`).
pub(crate) fn compute_seedkey(
    seed: &str,
    algo: &str,
    mask: &str,
    add: &str,
    level: Option<u8>,
    keys: &SeedKeys,
) -> Result<KeyResult, String> {
    let algo = algo.trim();
    let s = parse_hex_bytes(seed)?;
    let add_val: u8 = match add.trim() {
        "" => 0,
        x => parse_u32(x)? as u8,
    };
    let mask_bytes = match mask.trim() {
        "" => Vec::new(),
        x => parse_hex_bytes(x)?,
    };
    match generic::transform(algo, &s, &mask_bytes, add_val) {
        Some(key) => {
            return Ok(KeyResult {
                key: hexs(&key?),
                algo: algo.to_string(),
                source: KeySource::Generic,
                level: None,
                note: generic::NOTE.to_string(),
            })
        }
        None if algo.is_empty() => return Err("pick a transform first".into()),
        None => {}
    }
    let Some(handler) = keys.get(algo) else {
        return Err(format!(
            "no seed/key algorithm '{algo}' is available — the public edition ships no OEM \
             routines; pick a generic transform or supply one"
        ));
    };
    let level = level.unwrap_or(0x01);
    let key = opentune_hub::seedkeys::compute(handler.as_ref(), &s, level)?;
    let (source, note) = if keys.is_builtin_sa2(algo) {
        (
            KeySource::Sa2,
            "Computed by OpenTune's SA2 interpreter running YOUR script (armed in the SA2 \
             script panel, or given inline). OpenTune ships no SA2 script."
                .to_string(),
        )
    } else {
        (
            KeySource::Plugin,
            format!(
                "Computed by a plugin-supplied algorithm ('{}'), not by OpenTune. The plugin \
                 owns this algorithm and its correctness; OpenTune bundles no vehicle-specific \
                 seed/key routines.",
                handler.id()
            ),
        )
    };
    Ok(KeyResult {
        key: hexs(&key),
        algo: algo.to_string(),
        source,
        level: Some(level),
        note,
    })
}

/// A DTC system in words.
fn system_text(s: opentune_dtc::System) -> &'static str {
    match s {
        opentune_dtc::System::Powertrain => "Powertrain (engine / transmission / emissions)",
        opentune_dtc::System::Chassis => "Chassis (ABS / steering / suspension)",
        opentune_dtc::System::Body => "Body (airbags / lighting / HVAC / comfort)",
        opentune_dtc::System::Network => "Network / communication (CAN bus / modules)",
    }
}

fn dtc_lookup(code: &str) -> Result<opentune_dtc::Dtc, String> {
    opentune_dtc::lookup(code).ok_or_else(|| {
        "not a recognisable DTC — it must start with P, C, B or U (e.g. P0101)".to_string()
    })
}

fn part_decode(part: &str) -> Result<opentune_vin::PartNumber, String> {
    if part.trim().is_empty() {
        return Err("enter a part number".into());
    }
    Ok(opentune_vin::parse_part_number(part))
}

/// `key: value` rows of a decoded VIN, as the webview app shows them.
fn vin_rows(v: &opentune_vin::Vin) -> Vec<(&'static str, String)> {
    let my = &v.model_year;
    let mut year = match my.resolved {
        Some(y) => y.to_string(),
        None => "ambiguous".into(),
    };
    year.push_str(&format!(" (code {}", my.code));
    if !my.candidates.is_empty() {
        let c: Vec<String> = my.candidates.iter().map(|y| y.to_string()).collect();
        year.push_str(&format!(" → {}", c.join(" / ")));
    }
    year.push(')');
    let check = match v.check_digit_valid {
        None => "n/a (non-NA)",
        Some(true) => "valid",
        Some(false) => "INVALID",
    };
    let mut rows = vec![
        (
            "manufacturer",
            v.wmi_info.manufacturer.unwrap_or("unknown WMI").to_string(),
        ),
        ("country", v.wmi_info.country.unwrap_or("–").to_string()),
        (
            "WMI / VDS / VIS",
            format!("{} · {} · {}", v.wmi, v.vds, v.vis),
        ),
        ("check digit", format!("{} · {check}", v.check_digit)),
        ("model year", year),
        ("plant", v.plant_code.to_string()),
        ("serial", v.serial.clone()),
    ];
    if !v.flags.is_empty() {
        rows.push(("flags", v.flags.join("; ")));
    }
    rows
}

fn part_rows(p: &opentune_vin::PartNumber) -> Vec<(&'static str, String)> {
    let mut rows = vec![("kind", format!("{:?}", p.kind))];
    if let Some(s) = p.supplier {
        rows.push(("supplier", s.to_string()));
    }
    let o = |x: &Option<String>| x.clone().unwrap_or_default();
    if p.vag_main_group.is_some() {
        rows.push((
            "VAG type/main/sub",
            format!(
                "{} / {} / {}",
                o(&p.vag_type_group),
                o(&p.vag_main_group),
                o(&p.vag_subgroup)
            ),
        ));
        rows.push((
            "version",
            p.vag_version.clone().unwrap_or_else(|| "–".into()),
        ));
    }
    if let Some(f) = &p.bosch_family {
        rows.push(("Bosch family", f.clone()));
        rows.push((
            "consecutive",
            p.bosch_consecutive.clone().unwrap_or_else(|| "–".into()),
        ));
    }
    if !p.flags.is_empty() {
        rows.push(("flags", p.flags.join("; ")));
    }
    rows
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

/// The calculator's choice: a generic transform, or an id for the lookup.
const OTHER_ID: &str = "";

pub struct UtilitiesView {
    vin: String,
    vin_out: Option<Result<opentune_vin::Vin, String>>,
    part: String,
    part_out: Option<Result<opentune_vin::PartNumber, String>>,
    dtc: String,
    dtc_out: Option<Result<opentune_dtc::Dtc, String>>,
    algos: Vec<Algo>,
    /// A generic id, or [`OTHER_ID`] for the free-text id below.
    algo: String,
    other_id: String,
    level: String,
    seed: String,
    mask: String,
    add: String,
    sk_out: Option<Result<KeyResult, String>>,
}

impl Default for UtilitiesView {
    fn default() -> Self {
        let algos = algos();
        let algo = algos.first().map(|a| a.id.clone()).unwrap_or_default();
        Self {
            vin: String::new(),
            vin_out: None,
            part: String::new(),
            part_out: None,
            dtc: String::new(),
            dtc_out: None,
            algos,
            algo,
            other_id: String::new(),
            level: "0x01".into(),
            seed: String::new(),
            mask: String::new(),
            add: String::new(),
            sk_out: None,
        }
    }
}

impl UtilitiesView {
    fn compute(&mut self, keys: &SeedKeys) {
        let custom = self.algo == OTHER_ID;
        let algo = if custom { &self.other_id } else { &self.algo };
        let level = if custom {
            match self.level.trim() {
                "" => None,
                l => match parse_u32(l) {
                    Ok(v) if v <= 0xFF => Some(v as u8),
                    Ok(_) => {
                        self.sk_out = Some(Err("the access level is one byte (0x01..0xFF)".into()));
                        return;
                    }
                    Err(e) => {
                        self.sk_out = Some(Err(format!("access level {e}")));
                        return;
                    }
                },
            }
        } else {
            None
        };
        if custom && algo.trim().is_empty() {
            self.sk_out = Some(Err(
                "enter the seed/key id (a plugin id, sa2 or sa2:<hex>)".into()
            ));
            return;
        }
        self.sk_out = Some(compute_seedkey(
            &self.seed, algo, &self.mask, &self.add, level, keys,
        ));
    }

    fn seedkey_card(&mut self, ui: &mut Ui, host: &Host<'_>, t: &Tokens) {
        card_title(ui, t, icons::KEY, "Seed / key calculator");
        note(
            ui,
            t,
            Tone::Muted,
            "A generic, transparent transform with your parameters — not an OEM algorithm. \
             OpenTune bundles no vehicle-specific seed/key routines.",
        );
        let sel_name = if self.algo == OTHER_ID {
            "Other id (plugin / SA2)".to_string()
        } else {
            self.algos
                .iter()
                .find(|a| a.id == self.algo)
                .map(|a| a.name.clone())
                .unwrap_or_default()
        };
        let params: Vec<String> = self
            .algos
            .iter()
            .find(|a| a.id == self.algo)
            .map(|a| a.params.clone())
            .unwrap_or_default();
        egui::Grid::new("sk-grid")
            .num_columns(2)
            .spacing(Vec2::new(12.0, 6.0))
            .show(ui, |ui| {
                key_label(ui, t, "transform");
                egui::ComboBox::from_id_salt("sk-algo")
                    .selected_text(sel_name)
                    .width(260.0)
                    .show_ui(ui, |ui| {
                        for a in &self.algos {
                            ui.selectable_value(&mut self.algo, a.id.clone(), &a.name);
                        }
                        ui.selectable_value(
                            &mut self.algo,
                            OTHER_ID.to_string(),
                            "Other id (plugin / SA2)",
                        );
                    });
                ui.end_row();
                if self.algo == OTHER_ID {
                    key_label(ui, t, "seed/key id");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.other_id)
                            .font(egui::FontId::monospace(12.0))
                            .hint_text("sa2, sa2:<hex> or a plugin id")
                            .desired_width(260.0),
                    );
                    ui.end_row();
                    key_label(ui, t, "access level");
                    ui.add(egui::TextEdit::singleline(&mut self.level).desired_width(80.0));
                    ui.end_row();
                }
                key_label(ui, t, "seed (hex)");
                ui.add(
                    egui::TextEdit::singleline(&mut self.seed)
                        .font(egui::FontId::monospace(12.0))
                        .hint_text("e.g. 12 34 56 78")
                        .desired_width(260.0),
                );
                ui.end_row();
                if params.iter().any(|p| p == "mask") {
                    key_label(ui, t, "mask (hex)");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.mask)
                            .font(egui::FontId::monospace(12.0))
                            .desired_width(260.0),
                    );
                    ui.end_row();
                }
                if params.iter().any(|p| p == "add") {
                    key_label(ui, t, "add (0x.. or decimal)");
                    ui.add(egui::TextEdit::singleline(&mut self.add).desired_width(80.0));
                    ui.end_row();
                }
            });
        if let Some(a) = self.algos.iter().find(|a| a.id == self.algo) {
            note(ui, t, Tone::Muted, &a.desc);
        }
        if theme::primary_button(ui, t, Some(icons::CALCULATOR), "Compute key").clicked() {
            self.compute(host.seedkeys);
        }
        match &self.sk_out {
            None => {}
            Some(Err(e)) => note(ui, t, Tone::Crit, e),
            Some(Ok(r)) => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("key").color(t.fg_dim));
                    ui.add(
                        Label::new(
                            RichText::new(&r.key)
                                .font(font(MONO_MEDIUM, 14.0))
                                .color(t.ok),
                        )
                        .selectable(true),
                    );
                    match r.source {
                        KeySource::Generic => {}
                        KeySource::Sa2 => {
                            theme::pill(ui, t, "your SA2 script", Tone::Accent, false);
                        }
                        KeySource::Plugin => {
                            theme::pill(ui, t, "plugin algorithm", Tone::Accent, false);
                        }
                    }
                    if let Some(l) = r.level {
                        ui.label(
                            RichText::new(format!("level 0x{l:02X}"))
                                .small()
                                .color(t.fg_faint),
                        );
                    }
                });
                note(ui, t, Tone::Muted, &r.note);
            }
        }
    }
}

impl NativeView for UtilitiesView {
    fn id(&self) -> &str {
        "utilities"
    }
    fn label(&self) -> &str {
        "Utilities"
    }
    fn icon(&self) -> &str {
        icons::TOOLBOX
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Reference
    }
    fn tier(&self) -> Tier {
        TIER
    }
    fn ui(&mut self, ui: &mut Ui, host: &mut Host<'_>) {
        let t = *host.tokens();
        theme::panel(
            ui,
            &t,
            "Utilities",
            |_| {},
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("utilities")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .inner_margin(egui::Margin::symmetric(16, 12))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                self.vin_card(ui, &t);
                                ui.add_space(16.0);
                                self.part_card(ui, &t);
                                ui.add_space(16.0);
                                self.dtc_card(ui, &t);
                                ui.add_space(16.0);
                                self.seedkey_card(ui, host, &t);
                            });
                    });
            },
        );
    }
}

impl UtilitiesView {
    fn vin_card(&mut self, ui: &mut Ui, t: &Tokens) {
        card_title(ui, t, icons::CAR, "VIN decoder");
        if input_row(ui, t, &mut self.vin, "17-character VIN", "Decode") {
            self.vin_out = Some(opentune_vin::decode_vin(&self.vin).map_err(|e| e.to_string()));
        }
        match &self.vin_out {
            None => {}
            Some(Err(e)) => note(ui, t, Tone::Crit, e),
            Some(Ok(v)) => {
                note(ui, t, Tone::Ok, &format!("VIN {}", v.vin));
                kv_grid(ui, t, "vin-rows", &vin_rows(v));
            }
        }
    }

    fn part_card(&mut self, ui: &mut Ui, t: &Tokens) {
        card_title(ui, t, icons::BARCODE, "Part number");
        if input_row(
            ui,
            t,
            &mut self.part,
            "e.g. 03G 906 018 or 0281 012 345",
            "Decode",
        ) {
            self.part_out = Some(part_decode(&self.part));
        }
        match &self.part_out {
            None => {}
            Some(Err(e)) => note(ui, t, Tone::Crit, e),
            Some(Ok(p)) => {
                note(ui, t, Tone::Ok, &p.normalized);
                kv_grid(ui, t, "part-rows", &part_rows(p));
            }
        }
    }

    fn dtc_card(&mut self, ui: &mut Ui, t: &Tokens) {
        card_title(ui, t, icons::WARNING_CIRCLE, "Fault code (DTC)");
        if input_row(ui, t, &mut self.dtc, "e.g. P0101", "Look up") {
            self.dtc_out = Some(dtc_lookup(&self.dtc));
        }
        match &self.dtc_out {
            None => {}
            Some(Err(e)) => note(ui, t, Tone::Crit, e),
            Some(Ok(d)) => {
                note(ui, t, Tone::Ok, &d.code);
                kv_grid(
                    ui,
                    t,
                    "dtc-rows",
                    &[
                        ("description", d.description.clone()),
                        ("system", system_text(d.system).to_string()),
                    ],
                );
            }
        }
    }
}

fn card_title(ui: &mut Ui, t: &Tokens, icon: &str, title: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(icon).size(16.0).color(t.accent));
        theme::section_label(ui, t, title);
    });
    ui.add_space(4.0);
}

/// A text field plus a button; true when the button (or Enter) fired.
fn input_row(ui: &mut Ui, t: &Tokens, text: &mut String, hint: &str, button: &str) -> bool {
    ui.horizontal(|ui| {
        let r = ui.add(
            egui::TextEdit::singleline(text)
                .font(egui::FontId::monospace(12.5))
                .hint_text(hint)
                .desired_width(280.0),
        );
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        theme::primary_button(ui, t, None, button).clicked() || enter
    })
    .inner
}

fn kv_grid(ui: &mut Ui, t: &Tokens, id: &str, rows: &[(&str, String)]) {
    egui::Grid::new(id)
        .num_columns(2)
        .spacing(Vec2::new(18.0, 5.0))
        .show(ui, |ui| {
            for (k, v) in rows {
                key_label(ui, t, k);
                ui.add(
                    Label::new(RichText::new(v).font(font(MONO_MEDIUM, 12.0)).color(t.fg))
                        .wrap()
                        .selectable(true),
                );
                ui.end_row();
            }
        });
}

fn key_label(ui: &mut Ui, t: &Tokens, k: &str) {
    ui.label(
        RichText::new(k.to_uppercase())
            .font(font(UI_STRONG, 10.5))
            .color(t.fg_faint),
    );
}

fn note(ui: &mut Ui, t: &Tokens, tone: Tone, text: &str) {
    let c = match tone {
        Tone::Ok => t.ok,
        Tone::Warn => t.warn,
        Tone::Crit => t.crit,
        Tone::Accent => t.accent,
        Tone::Muted => t.fg_dim,
    };
    ui.add(Label::new(RichText::new(text).font(font(UI_MEDIUM, 12.5)).color(c)).wrap());
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentune_hub::seedkeys::testkeys::{first, second, vcar};

    fn sk(seed: &str, algo: &str, mask: &str, add: &str) -> Result<KeyResult, String> {
        compute_seedkey(seed, algo, mask, add, None, &SeedKeys::default())
    }

    #[test]
    fn generic_transforms() {
        assert_eq!(sk("0102", "identity", "", "").unwrap().key, "01 02");
        assert_eq!(sk("0102", "byteswap", "", "").unwrap().key, "02 01");
        assert_eq!(sk("F0F0", "xor-mask", "FF", "").unwrap().key, "0F 0F");
        assert_eq!(sk("FE", "add-const", "", "3").unwrap().key, "01");
        let r = sk("0A", "xor-add", "0F", "0x01").unwrap();
        assert_eq!(r.key, "06");
        assert_eq!((r.source, r.level), (KeySource::Generic, None));
        assert_eq!(r.note, generic::NOTE);
    }

    #[test]
    fn unknown_ids_and_missing_parameters_are_honest_errors() {
        assert!(sk("00", "vendor-x", "", "")
            .unwrap_err()
            .contains("'vendor-x'"));
        assert!(sk("00", "", "", "")
            .unwrap_err()
            .contains("pick a transform"));
        // xor-mask without a mask must error, not fabricate.
        assert!(sk("00", "xor-mask", "", "").is_err());
        assert!(sk("", "identity", "", "").is_err());
        assert!(sk("123", "identity", "", "").is_err());
        assert!(sk("00", "add-const", "", "zz").is_err());
    }

    #[test]
    fn resolver_and_sa2_results_are_labelled() {
        let keys = SeedKeys::new(vec![first, second, vcar]);
        // A plugin id resolves (first resolver wins) and is labelled.
        let r = compute_seedkey("0001", "shared", "", "", Some(0x03), &keys).unwrap();
        assert_eq!(r.key, "13 12"); // seed ^ 0x10 ^ level 0x03
        assert_eq!((r.source, r.level), (KeySource::Plugin, Some(3)));
        assert!(r.note.contains("plugin-supplied") && r.note.contains("'first'"));
        // Default level 0x01.
        let r = compute_seedkey("1234", "vcar-test", "", "", None, &keys).unwrap();
        assert_eq!(r.key, "48 6E");
        // Generic ids are never routed to a resolver.
        let r = compute_seedkey("0102", "identity", "", "", None, &keys).unwrap();
        assert_eq!(r.source, KeySource::Generic);
        // The SA2 interpreter over an inline synthetic script (NOT seed).
        let r = sk("00000001", "sa2:87FFFFFFFF4C", "", "").unwrap();
        assert_eq!((r.key.as_str(), r.source), ("FF FF FF FE", KeySource::Sa2));
        // `sa2` with nothing armed is an error.
        assert!(sk("00000001", "sa2", "", "").is_err());
    }

    #[test]
    fn the_calculator_lists_the_generic_transforms() {
        let a = algos();
        assert!(a.iter().any(|x| x.id == "identity" && x.params.is_empty()));
        let xa = a.iter().find(|x| x.id == "xor-add").unwrap();
        assert_eq!(xa.params, ["mask", "add"]);
        assert!(a.iter().all(|x| !x.name.is_empty() && !x.desc.is_empty()));
        // The view starts on the first transform; a custom id needs text and
        // a one-byte level.
        let mut v = UtilitiesView::default();
        assert_eq!(v.algo, a[0].id);
        v.algo = OTHER_ID.into();
        v.seed = "00".into();
        v.compute(&SeedKeys::default());
        assert!(matches!(&v.sk_out, Some(Err(e)) if e.contains("seed/key id")));
        v.other_id = "sa2:87FFFFFFFF4C".into();
        v.level = "0x100".into();
        v.compute(&SeedKeys::default());
        assert!(matches!(&v.sk_out, Some(Err(e)) if e.contains("one byte")));
    }

    #[test]
    fn decoders() {
        // A synthetic 17-character string in the VIN alphabet (no real VIN).
        let v = opentune_vin::decode_vin("abcde1234fg567890").unwrap();
        let rows = vin_rows(&v);
        assert_eq!(v.vin, "ABCDE1234FG567890");
        assert!(rows
            .iter()
            .any(|(k, v)| *k == "WMI / VDS / VIS" && v.starts_with("ABC ·")));
        assert!(rows
            .iter()
            .any(|(k, v)| *k == "model year" && v.contains("code F")));
        assert!(rows.iter().any(|(k, _)| *k == "check digit"));
        assert!(opentune_vin::decode_vin("short").is_err());
        let p = part_decode("0281 012 345").unwrap();
        let rows = part_rows(&p);
        assert!(rows
            .iter()
            .any(|(k, v)| *k == "Bosch family" && v == "0281"));
        assert!(part_decode("  ").is_err());
        let p = part_decode("03G 906 018 AB").unwrap();
        assert!(part_rows(&p).iter().any(|(k, _)| *k == "VAG type/main/sub"));
        let d = dtc_lookup("p0101").unwrap();
        assert_eq!(d.code, "P0101");
        assert!(system_text(d.system).starts_with("Powertrain"));
        assert!(dtc_lookup("X123").is_err());
        assert_eq!(parse_u32("0x1F").unwrap(), 31);
        assert_eq!(parse_u32(" 12 ").unwrap(), 12);
        assert_eq!(parse_hex_bytes("aa:bb").unwrap(), [0xAA, 0xBB]);
    }
}
