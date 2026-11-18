//! The `.msq` tune format: an XML file of `<constant name=...>` values in user
//! units, grouped by `<page number=...>`, with the firmware signature in
//! `<versionInfo>`.
//!
//! [`Msq::parse`] / [`Msq::to_xml`] keep everything the file holds
//! (attributes in order, element text verbatim, unknown top-level elements as
//! raw XML), so reading and writing a file loses nothing. [`Msq::to_tune`] and
//! [`Msq::from_tune`] convert through an [`Ini`]; values are written with as
//! many decimals as needed for the stored raw value to read back bit-exact.
use crate::model::{Constant, DataType, Ini, Kind};
use crate::tune::Tune;
use crate::value::{self, Scaling, Value};
use crate::{normalize_signature, text, Error, Result};
use std::collections::BTreeSet;
use std::fmt::Write as _;

/// One `<constant>` or `<pcVariable>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsqEntry {
    /// `constant` or `pcVariable`.
    pub tag: String,
    pub name: String,
    /// All attributes in file order, `name` included.
    pub attrs: Vec<(String, String)>,
    /// Element text exactly as in the file (quotes and whitespace kept).
    pub text: String,
}

impl MsqEntry {
    pub fn attr(&self, k: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(a, _)| a == k)
            .map(|(_, v)| v.as_str())
    }
}

/// One `<page>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MsqPage {
    pub attrs: Vec<(String, String)>,
    pub entries: Vec<MsqEntry>,
}

impl MsqPage {
    /// The 0-based page number, absent for the PC-variable page.
    pub fn number(&self) -> Option<usize> {
        self.attrs
            .iter()
            .find(|(k, _)| k == "number")
            .and_then(|(_, v)| v.trim().parse().ok())
    }
}

/// A parsed `.msq` file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Msq {
    pub bibliography: Vec<(String, String)>,
    pub version_info: Vec<(String, String)>,
    pub pages: Vec<MsqPage>,
    /// `<settings>` attributes and its `<setting>` children's attributes.
    pub settings_attrs: Vec<(String, String)>,
    pub settings: Vec<Vec<(String, String)>>,
    /// Other top-level elements, verbatim (e.g. `<userComments>`).
    pub extra: Vec<String>,
    /// Whether a `<settings>` element was present.
    pub has_settings: bool,
}

/// Who/when for a tune written by [`Msq::from_tune`].
#[derive(Debug, Clone, Default)]
pub struct MsqMeta {
    pub author: String,
    pub comment: String,
    pub write_date: String,
    pub firmware_info: String,
}

/// Options for [`Msq::to_tune`].
#[derive(Debug, Clone, Copy, Default)]
pub struct MsqLoadOptions {
    /// Load even when the tune's signature differs from the INI's.
    pub force_signature: bool,
}

/// What [`Msq::to_tune`] did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MsqReport {
    pub applied: usize,
    /// Names in the tune the INI does not define.
    pub unknown: Vec<String>,
    /// Entries that could not be applied, with the reason; their bytes keep
    /// the base tune's value.
    pub skipped: Vec<(String, String)>,
    /// Values outside the INI's `lo`/`hi` (applied anyway; the ECU limits are
    /// the INI author's display limits).
    pub out_of_range: Vec<String>,
    /// Signature notes (missing on either side, or forced past a mismatch).
    pub notes: Vec<String>,
}

fn attrs_of(n: roxmltree::Node) -> Vec<(String, String)> {
    n.attributes()
        .map(|a| (a.name().to_owned(), a.value().to_owned()))
        .collect()
}

fn esc(s: &str, attr: bool) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' if attr => o.push_str("&quot;"),
            '\n' if attr => o.push_str("&#10;"),
            _ => o.push(c),
        }
    }
    o
}

fn write_attrs(out: &mut String, attrs: &[(String, String)]) {
    for (k, v) in attrs {
        let _ = write!(out, " {k}=\"{}\"", esc(v, true));
    }
}

fn unquote(s: &str) -> &str {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

/// Index of a bit-field label (labels beyond the definition's list are the
/// synthesised numbers).
fn bit_index(c: &Constant, label: &str) -> Option<u32> {
    let slots = c.bit_slots();
    (0..slots)
        .find(|&j| c.bit_label(j) == label)
        .or_else(|| (0..slots).find(|&j| c.bit_label(j).trim() == label.trim()))
        .map(|j| j as u32)
}

/// Parse an entry's text as a value of constant `c`.
pub fn entry_value(c: &Constant, e: &MsqEntry) -> std::result::Result<Value, String> {
    let raw = e.text.trim();
    match &c.kind {
        Kind::Scalar => text::parse_number(unquote(raw))
            .map(Value::Scalar)
            .ok_or_else(|| format!("not a number: {raw:?}")),
        Kind::Array { .. } => {
            let vals: Vec<f64> = unquote(raw)
                .split_whitespace()
                .map(|t| text::parse_number(t).ok_or_else(|| format!("not a number: {t:?}")))
                .collect::<std::result::Result<_, _>>()?;
            if vals.len() != c.elements() {
                return Err(format!(
                    "{} values, the INI shape needs {}",
                    vals.len(),
                    c.elements()
                ));
            }
            Ok(Value::Array(vals))
        }
        Kind::Bits { .. } => {
            let quoted = raw.starts_with('"');
            let s = unquote(raw);
            if quoted {
                if let Some(i) = bit_index(c, s) {
                    return Ok(Value::Bits(i));
                }
            }
            match s.parse::<f64>() {
                Ok(v) if v >= 0.0 && v.fract() == 0.0 && (v as usize) < c.bit_slots() => {
                    Ok(Value::Bits(v as u32))
                }
                _ => Err(format!("no option {raw}")),
            }
        }
        Kind::String { .. } => Ok(Value::Text(unquote(raw).to_owned())),
    }
}

fn same_raw(ty: DataType, s: &Scaling, a: f64, b_raw: f64) -> bool {
    let r = value::user_to_raw(ty, s, a);
    match ty {
        DataType::F32 => (r as f32).to_bits() == (b_raw as f32).to_bits(),
        DataType::F64 => r.to_bits() == b_raw.to_bits(),
        _ => r == b_raw,
    }
}

/// Format a user value with the fewest decimals (at least `digits`, at least
/// one) that still converts back to the same raw value.
pub fn format_user(c: &Constant, s: &Scaling, user: f64) -> String {
    let raw = value::user_to_raw(c.ty, s, user);
    let start = c.digits_lit().unwrap_or(0).clamp(1, 15);
    for d in start..=17 {
        let t = format!("{user:.d$}");
        if let Ok(back) = t.parse::<f64>() {
            if same_raw(c.ty, s, back, raw) {
                return t;
            }
        }
    }
    format!("{user:?}")
}

impl Msq {
    /// Parse `.msq` XML (already decoded to text).
    pub fn parse(xml: &str) -> Result<Self> {
        let doc = roxmltree::Document::parse(xml).map_err(|e| Error::Xml(e.to_string()))?;
        let root = doc.root_element();
        if root.tag_name().name() != "msq" {
            return Err(Error::Format(format!(
                "root element is <{}>, not <msq>",
                root.tag_name().name()
            )));
        }
        let mut m = Msq::default();
        for n in root.children().filter(|n| n.is_element()) {
            match n.tag_name().name() {
                "bibliography" => m.bibliography = attrs_of(n),
                "versionInfo" => m.version_info = attrs_of(n),
                "page" => {
                    let mut p = MsqPage {
                        attrs: attrs_of(n),
                        entries: Vec::new(),
                    };
                    for e in n.children().filter(|e| e.is_element()) {
                        p.entries.push(MsqEntry {
                            tag: e.tag_name().name().to_owned(),
                            name: e.attribute("name").unwrap_or_default().to_owned(),
                            attrs: attrs_of(e),
                            text: e.text().unwrap_or_default().to_owned(),
                        });
                    }
                    m.pages.push(p);
                }
                "settings" => {
                    m.has_settings = true;
                    m.settings_attrs = attrs_of(n);
                    m.settings = n
                        .children()
                        .filter(|e| e.is_element())
                        .map(attrs_of)
                        .collect();
                }
                _ => m.extra.push(xml[n.range()].to_owned()),
            }
        }
        Ok(m)
    }

    /// Read and parse a `.msq` file (UTF-8 or ISO-8859-1).
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        Self::parse(&text::decode(&bytes))
    }

    /// Serialise as XML (UTF-8).
    pub fn to_xml(&self) -> String {
        let mut o = String::new();
        o.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        o.push_str("<msq xmlns=\"http://www.msefi.com/:msq\">\n");
        o.push_str("<bibliography");
        write_attrs(&mut o, &self.bibliography);
        o.push_str("/>\n<versionInfo");
        write_attrs(&mut o, &self.version_info);
        o.push_str("/>\n");
        for p in &self.pages {
            o.push_str("<page");
            write_attrs(&mut o, &p.attrs);
            o.push_str(">\n");
            for e in &p.entries {
                let _ = write!(o, "<{}", e.tag);
                write_attrs(&mut o, &e.attrs);
                let _ = writeln!(o, ">{}</{}>", esc(&e.text, false), e.tag);
            }
            o.push_str("</page>\n");
        }
        if self.has_settings {
            o.push_str("<settings");
            write_attrs(&mut o, &self.settings_attrs);
            o.push_str(">\n");
            for s in &self.settings {
                o.push_str("<setting");
                write_attrs(&mut o, s);
                o.push_str("/>\n");
            }
            o.push_str("</settings>\n");
        }
        for x in &self.extra {
            o.push_str(x);
            o.push('\n');
        }
        o.push_str("</msq>\n");
        o
    }

    pub fn signature(&self) -> Option<&str> {
        self.version_info
            .iter()
            .find(|(k, _)| k == "signature")
            .map(|(_, v)| v.as_str())
    }

    /// The first entry named `name` in any page.
    pub fn entry(&self, name: &str) -> Option<&MsqEntry> {
        self.pages
            .iter()
            .flat_map(|p| &p.entries)
            .find(|e| e.name == name)
    }

    /// Every entry, in file order.
    pub fn entries(&self) -> impl Iterator<Item = &MsqEntry> {
        self.pages.iter().flat_map(|p| &p.entries)
    }

    /// Condition names stored under `<settings>` (the project settings the
    /// tune was saved with), for [`crate::IniOptions`].
    pub fn settings_conditions(&self) -> BTreeSet<String> {
        self.settings
            .iter()
            .filter_map(|a| a.iter().find(|(k, _)| k == "name").map(|(_, v)| v.clone()))
            .collect()
    }

    /// Compare signatures; `Err` on a mismatch unless `force`.
    pub fn check_signature(&self, ini: &Ini, force: bool) -> Result<Option<String>> {
        match (self.signature(), ini.signature.as_deref()) {
            (Some(a), Some(b)) if normalize_signature(a) != normalize_signature(b) => {
                if force {
                    Ok(Some(format!(
                        "signature mismatch forced: tune {a:?}, INI {b:?}"
                    )))
                } else {
                    Err(Error::SignatureMismatch {
                        msq: a.to_owned(),
                        ini: b.to_owned(),
                    })
                }
            }
            (None, _) => Ok(Some("tune has no signature".into())),
            (_, None) => Ok(Some("INI has no signature".into())),
            _ => Ok(None),
        }
    }

    /// Apply this tune's values over `base` (a blank tune, or the ECU's
    /// current pages so bytes the tune does not mention keep their value).
    pub fn to_tune(
        &self,
        ini: &Ini,
        base: Tune,
        opts: MsqLoadOptions,
    ) -> Result<(Tune, MsqReport)> {
        let mut rep = MsqReport::default();
        rep.notes
            .extend(self.check_signature(ini, opts.force_signature)?);
        let mut tune = base;
        if tune.pages.len() != ini.pages.len() {
            return Err(Error::Value(
                "base tune does not match the INI's pages".into(),
            ));
        }
        // Literal scalings first, then expression scalings (which may read
        // the values applied in the first pass).
        let mut deferred = Vec::new();
        for e in self.entries() {
            let Some(c) = ini.constant(&e.name) else {
                rep.unknown.push(e.name.clone());
                continue;
            };
            if c.scale.lit().is_none() || c.translate.lit().is_none() {
                deferred.push((e, c));
                continue;
            }
            apply(&mut tune, ini, c, e, &mut rep);
        }
        for (e, c) in deferred {
            apply(&mut tune, ini, c, e, &mut rep);
        }
        Ok((tune, rep))
    }

    /// Build a tune file from page bytes. Returns the names that could not be
    /// written (e.g. an unevaluable scale expression) next to the file.
    pub fn from_tune(ini: &Ini, tune: &Tune, meta: &MsqMeta) -> (Msq, Vec<(String, String)>) {
        let mut skipped = Vec::new();
        let mut m = Msq {
            bibliography: vec![
                ("author".into(), meta.author.clone()),
                ("tuneComment".into(), meta.comment.clone()),
                ("writeDate".into(), meta.write_date.clone()),
            ],
            version_info: vec![
                ("fileFormat".into(), "5.0".into()),
                ("firmwareInfo".into(), meta.firmware_info.clone()),
                ("nPages".into(), ini.pages.len().to_string()),
                (
                    "signature".into(),
                    ini.signature.clone().unwrap_or_default(),
                ),
            ],
            ..Msq::default()
        };
        if !tune.pc.is_empty() {
            let mut p = MsqPage::default();
            for c in &ini.pc_variables {
                if let Some(v) = tune.pc.get(&c.name) {
                    match entry_for(c, v, &Scaling::IDENTITY, "pcVariable") {
                        Ok(e) => p.entries.push(e),
                        Err(err) => skipped.push((c.name.clone(), err)),
                    }
                }
            }
            m.pages.push(p);
        }
        for pd in &ini.pages {
            let mut p = MsqPage {
                attrs: vec![
                    ("number".into(), pd.index.to_string()),
                    ("size".into(), pd.size.to_string()),
                ],
                entries: Vec::new(),
            };
            for c in ini.page_constants(pd.index).filter(|c| !c.no_msq_save) {
                let r = tune.scaling(ini, &c.name).and_then(|s| {
                    let v = tune.get(ini, &c.name)?;
                    entry_for(c, &v, &s, "constant").map_err(Error::Value)
                });
                match r {
                    Ok(e) => p.entries.push(e),
                    Err(err) => skipped.push((c.name.clone(), err.to_string())),
                }
            }
            m.pages.push(p);
        }
        let named: BTreeSet<&str> = ini
            .setting_groups
            .iter()
            .flat_map(|g| {
                std::iter::once(g.name.as_str()).chain(g.options.iter().map(|(n, _)| n.as_str()))
            })
            .collect();
        let active: Vec<&String> = ini
            .conditions
            .iter()
            .filter(|c| named.contains(c.as_str()))
            .collect();
        m.has_settings = true;
        m.settings_attrs = vec![(
            "Comment".into(),
            "These setting are only used if this msq is opened without a project.".into(),
        )];
        m.settings = active
            .into_iter()
            .map(|c| vec![("name".into(), c.clone()), ("value".into(), c.clone())])
            .collect();
        (m, skipped)
    }
}

fn apply(tune: &mut Tune, ini: &Ini, c: &Constant, e: &MsqEntry, rep: &mut MsqReport) {
    let r = entry_value(c, e).and_then(|v| {
        let issues = value::range_issues(c, &v);
        tune.set(ini, &c.name, &v).map_err(|e| e.to_string())?;
        Ok(issues)
    });
    match r {
        Ok(issues) => {
            rep.applied += 1;
            rep.out_of_range.extend(issues);
        }
        Err(why) => rep.skipped.push((e.name.clone(), why)),
    }
}

fn entry_for(
    c: &Constant,
    v: &Value,
    s: &Scaling,
    tag: &str,
) -> std::result::Result<MsqEntry, String> {
    let mut attrs: Vec<(String, String)> = Vec::new();
    if let Kind::Array { cols, .. } = c.kind {
        attrs.push(("cols".into(), cols.to_string()));
    }
    let numeric = matches!(c.kind, Kind::Scalar | Kind::Array { .. });
    if numeric {
        if let Some(d) = c.digits_lit() {
            attrs.push(("digits".into(), d.to_string()));
        }
    }
    attrs.push(("name".into(), c.name.clone()));
    if let Kind::Array { rows, .. } = c.kind {
        attrs.push(("rows".into(), rows.to_string()));
    }
    if numeric && !c.units.is_empty() {
        attrs.push(("units".into(), c.units.clone()));
    }
    let text = match (&c.kind, v) {
        (Kind::Scalar, Value::Scalar(u)) => format_user(c, s, *u),
        (Kind::Array { cols, .. }, Value::Array(vals)) => {
            let mut t = String::from("\n");
            for row in vals.chunks((*cols).max(1)) {
                t.push_str("         ");
                for u in row {
                    t.push_str(&format_user(c, s, *u));
                    t.push(' ');
                }
                t.push('\n');
            }
            t.push_str("      ");
            t
        }
        (Kind::Bits { .. }, Value::Bits(i)) => {
            let label = c.bit_label(*i as usize);
            if bit_index(c, &label) == Some(*i) && !label.contains('"') {
                format!("\"{label}\"")
            } else {
                i.to_string()
            }
        }
        (Kind::String { .. }, Value::Text(t)) => format!("\"{t}\""),
        (k, v) => return Err(format!("value {v:?} does not match {k:?}")),
    };
    Ok(MsqEntry {
        tag: tag.into(),
        name: c.name.clone(),
        attrs,
        text,
    })
}
