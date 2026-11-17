//! Control-unit reference database for OpenTune.
//!
//! One hand-curated JSON file per control-unit family/variant lives in
//! `database/units/*.json` (schema: `docs/database-unit.schema.json`, rules:
//! `docs/DATABASE.md`). Each file says **what the unit is** (maker, family,
//! processor, flash/EEPROM), **how it can be read** (OBD / bench / boot),
//! **which OpenTune checksum family applies**, **which vehicles use it** and —
//! only where a public source could be cited — **bench pinouts**.
//!
//! The pinout data is safety-relevant: a wrong pin on the bench can destroy a
//! unit. [`Database::validate`] therefore enforces the honesty rules
//! mechanically: every pin cites at least one source, a pin may only be called
//! `verified` when two or more sources agree, a unit's overall
//! `pinout_confidence` can never be higher than the weakest pin the bench
//! wiring actually uses, unverified pins can never be part of the bench wiring,
//! and a unit without a pinout must say so in its notes.
//!
//! The crate is data-only: no I/O beyond reading the JSON directory, no
//! seed/key material, no firmware.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Data model (mirrors docs/database-unit.schema.json)
// ---------------------------------------------------------------------------

/// What kind of control unit a record describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Engine control unit.
    Ecu,
    /// Transmission control unit.
    Tcu,
    /// All-wheel-drive coupling controller (e.g. Haldex).
    Awd,
}

impl Kind {
    /// The stable lowercase token used in JSON and the desktop contract.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Ecu => "ecu",
            Kind::Tcu => "tcu",
            Kind::Awd => "awd",
        }
    }
}

/// Physical diagnostic bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Bus {
    Can,
    Kline,
}

/// Diagnostic protocol spoken over the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Kwp2000,
    Uds,
    Tp20,
    Kwp1281,
}

/// What can be done through the vehicle's diagnostic socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObdAccess {
    /// Full read and write over OBD.
    #[serde(rename = "read+write")]
    ReadWrite,
    /// Calibration/firmware can be written over OBD, but a complete original
    /// read needs the bench or boot mode.
    #[serde(rename = "write-only")]
    WriteOnly,
    /// No OBD programming.
    #[serde(rename = "none")]
    None,
    /// No public source found either way — never guessed.
    #[serde(rename = "unknown")]
    Unknown,
}

/// How a unit can be read/written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadMethods {
    pub obd: ObdAccess,
    /// Readable on the bench over its normal diagnostic bus (housing closed).
    pub bench: bool,
    /// A processor boot/service mode (board-level) is known to be usable.
    pub boot: bool,
    pub notes: String,
}

/// Electrical role of a connector pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Signal {
    #[serde(rename = "12v_perm")]
    V12Perm,
    #[serde(rename = "12v_ign")]
    V12Ign,
    #[serde(rename = "gnd")]
    Gnd,
    #[serde(rename = "can_h")]
    CanH,
    #[serde(rename = "can_l")]
    CanL,
    #[serde(rename = "kline")]
    Kline,
    #[serde(rename = "boot")]
    Boot,
    #[serde(rename = "other")]
    Other,
}

impl Signal {
    /// Every signal token, in schema order (used by the schema-sync test).
    pub const ALL: [&'static str; 8] = [
        "12v_perm", "12v_ign", "gnd", "can_h", "can_l", "kline", "boot", "other",
    ];
}

/// How well a pin assignment is supported by public sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// No usable source, or sources disagree. Never part of the bench wiring.
    Unverified,
    /// One credible public source.
    Community,
    /// Two or more independent public sources agree.
    Verified,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Unverified => "unverified",
            Confidence::Community => "community",
            Confidence::Verified => "verified",
        }
    }
}

/// One pin of a connector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    /// Pin label as printed/used by the sources (number or letter).
    pub pin: String,
    /// Function in plain words.
    pub function: String,
    pub signal: Signal,
    #[serde(default)]
    pub note: Option<String>,
    /// Per-pin confidence; defaults to the unit's `pinout_confidence`.
    #[serde(default)]
    pub confidence: Option<Confidence>,
    /// Indices into the unit's `sources` that state this assignment.
    pub sources: Vec<usize>,
}

/// One connector (or connector block) on the unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connector {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub pins: Vec<Pin>,
}

/// The minimal bench wiring, as `"<connector>-<pin>"` references into
/// [`Unit::connectors`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bench {
    #[serde(default)]
    pub power: Vec<String>,
    #[serde(default)]
    pub ground: Vec<String>,
    #[serde(default)]
    pub ign: Vec<String>,
    #[serde(default)]
    pub can_h: Vec<String>,
    #[serde(default)]
    pub can_l: Vec<String>,
    #[serde(default)]
    pub kline: Vec<String>,
    #[serde(default)]
    pub boot: Vec<String>,
    pub notes: String,
    #[serde(default)]
    pub resistor_notes: Option<String>,
}

/// A vehicle application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vehicle {
    pub make: String,
    pub model: String,
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(default)]
    pub years: Option<String>,
}

/// A cited public source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub title: String,
    pub url: String,
}

/// One control-unit record (`database/units/<id>.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unit {
    pub id: String,
    pub kind: Kind,
    pub maker: String,
    pub family: String,
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub cpu: Option<String>,
    pub flash_kib: Option<u32>,
    pub eeprom: Option<String>,
    pub bus: Vec<Bus>,
    pub protocols: Vec<Protocol>,
    pub read_methods: ReadMethods,
    /// An `opentune_checksum::get` id, or null when none applies.
    pub checksum: Option<String>,
    /// An `ecudefs/` definition id, or null.
    pub ecudef: Option<String>,
    pub vehicles: Vec<Vehicle>,
    pub part_number_examples: Vec<String>,
    pub notes: String,
    pub connectors: Vec<Connector>,
    pub bench: Option<Bench>,
    pub pinout_confidence: Option<Confidence>,
    pub safety_note: Option<String>,
    pub sources: Vec<Source>,
}

impl Unit {
    /// True when the record carries at least one sourced connector pin.
    pub fn has_pinout(&self) -> bool {
        self.connectors.iter().any(|c| !c.pins.is_empty())
    }

    /// Resolve a `"<connector>-<pin>"` bench reference.
    pub fn resolve_pin(&self, reference: &str) -> Option<&Pin> {
        self.connectors.iter().find_map(|c| {
            let rest = reference.strip_prefix(c.name.as_str())?.strip_prefix('-')?;
            c.pins.iter().find(|p| p.pin == rest)
        })
    }

    /// Effective confidence of a pin (its own, else the unit's).
    pub fn pin_confidence(&self, pin: &Pin) -> Confidence {
        pin.confidence
            .or(self.pinout_confidence)
            .unwrap_or(Confidence::Unverified)
    }

    /// Case-insensitive match of every whitespace-separated token of `q`
    /// against name, family, maker, part numbers and vehicles (plus id, CPU
    /// and aliases).
    pub fn matches(&self, q: &str) -> bool {
        let mut hay = vec![
            self.id.as_str(),
            self.name.as_str(),
            self.family.as_str(),
            self.maker.as_str(),
        ];
        if let Some(cpu) = &self.cpu {
            hay.push(cpu.as_str());
        }
        hay.extend(self.aliases.iter().map(String::as_str));
        hay.extend(self.part_number_examples.iter().map(String::as_str));
        for v in &self.vehicles {
            hay.push(v.make.as_str());
            hay.push(v.model.as_str());
            if let Some(e) = &v.engine {
                hay.push(e.as_str());
            }
        }
        let hay = hay.join("\n").to_lowercase();
        // Part numbers are often typed with or without spaces/dots.
        let squashed: String = hay.chars().filter(|c| !matches!(c, ' ' | '.')).collect();
        q.split_whitespace().all(|tok| {
            let tok = tok.to_lowercase();
            hay.contains(&tok) || squashed.contains(&tok.replace('.', ""))
        })
    }

    /// The compact list row of the desktop `db_list` contract.
    pub fn summary(&self) -> UnitSummary {
        UnitSummary {
            id: self.id.clone(),
            kind: self.kind,
            maker: self.maker.clone(),
            family: self.family.clone(),
            name: self.name.clone(),
            cpu: self.cpu.clone(),
            flash_kib: self.flash_kib,
            bus: self.bus.clone(),
            read_methods: self.read_methods.clone(),
            checksum: self.checksum.clone(),
            ecudef: self.ecudef.clone(),
            vehicle_count: self.vehicles.len(),
            has_pinout: self.has_pinout(),
            pinout_confidence: self.pinout_confidence,
        }
    }
}

/// One row of `db_list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnitSummary {
    pub id: String,
    pub kind: Kind,
    pub maker: String,
    pub family: String,
    pub name: String,
    pub cpu: Option<String>,
    pub flash_kib: Option<u32>,
    pub bus: Vec<Bus>,
    pub read_methods: ReadMethods,
    pub checksum: Option<String>,
    pub ecudef: Option<String>,
    pub vehicle_count: usize,
    pub has_pinout: bool,
    pub pinout_confidence: Option<Confidence>,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Errors loading the database directory.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {msg}")]
    Parse { path: PathBuf, msg: String },
    #[error("duplicate unit id '{id}' ({first} and {second})")]
    DuplicateId {
        id: String,
        first: PathBuf,
        second: PathBuf,
    },
}

/// A loaded record: the typed view plus the JSON exactly as stored.
#[derive(Debug, Clone)]
pub struct Entry {
    pub unit: Unit,
    pub raw: Value,
    pub path: PathBuf,
}

/// The loaded database.
#[derive(Debug, Clone, Default)]
pub struct Database {
    entries: Vec<Entry>,
}

impl Database {
    /// Load every `*.json` in `dir` (the `database/units` directory, or the
    /// `database` directory that contains it), sorted by file name.
    pub fn load_dir(dir: &Path) -> Result<Database, DbError> {
        let units = dir.join("units");
        let dir = if units.is_dir() {
            units
        } else {
            dir.to_path_buf()
        };
        let rd = std::fs::read_dir(&dir).map_err(|source| DbError::Io {
            path: dir.clone(),
            source,
        })?;
        let mut files: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
            .collect();
        files.sort();
        let mut db = Database::default();
        let mut seen: HashMap<String, PathBuf> = HashMap::new();
        for path in files {
            let text = std::fs::read_to_string(&path).map_err(|source| DbError::Io {
                path: path.clone(),
                source,
            })?;
            let entry = parse_entry(&text, path.clone())?;
            if let Some(first) = seen.get(&entry.unit.id) {
                return Err(DbError::DuplicateId {
                    id: entry.unit.id.clone(),
                    first: first.clone(),
                    second: path,
                });
            }
            seen.insert(entry.unit.id.clone(), path);
            db.entries.push(entry);
        }
        Ok(db)
    }

    /// All loaded entries, in file-name order.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The `db_list` query: optional kind filter (`ecu`/`tcu`/`awd`, case-
    /// insensitive; empty = all) and optional free-text query.
    pub fn list(&self, kind: Option<&str>, q: Option<&str>) -> Vec<UnitSummary> {
        let kind = kind.map(str::trim).filter(|k| !k.is_empty());
        let q = q.map(str::trim).filter(|q| !q.is_empty());
        self.entries
            .iter()
            .map(|e| &e.unit)
            .filter(|u| kind.is_none_or(|k| u.kind.as_str().eq_ignore_ascii_case(k)))
            .filter(|u| q.is_none_or(|q| u.matches(q)))
            .map(Unit::summary)
            .collect()
    }

    /// Typed lookup by id.
    pub fn unit(&self, id: &str) -> Option<&Unit> {
        self.entries
            .iter()
            .find(|e| e.unit.id == id)
            .map(|e| &e.unit)
    }

    /// The `db_get` query: the unit JSON exactly as stored.
    pub fn get(&self, id: &str) -> Option<&Value> {
        self.entries
            .iter()
            .find(|e| e.unit.id == id)
            .map(|e| &e.raw)
    }

    /// Check every record against the database rules. `ecudef_ids`, when
    /// given, is the set of definition ids an `ecudef` reference must hit.
    pub fn validate(&self, ecudef_ids: Option<&BTreeSet<String>>) -> Vec<Issue> {
        let mut out = Vec::new();
        for e in &self.entries {
            validate_unit(&e.unit, &e.path, ecudef_ids, &mut out);
        }
        out
    }
}

/// Parse one unit file's text.
pub fn parse_entry(text: &str, path: PathBuf) -> Result<Entry, DbError> {
    let raw: Value = serde_json::from_str(text).map_err(|e| DbError::Parse {
        path: path.clone(),
        msg: e.to_string(),
    })?;
    let unit: Unit = serde_json::from_value(raw.clone()).map_err(|e| DbError::Parse {
        path: path.clone(),
        msg: e.to_string(),
    })?;
    Ok(Entry { unit, raw, path })
}

/// The `id` of every `*.json` definition in an `ecudefs/` directory.
pub fn ecudef_ids(dir: &Path) -> Result<BTreeSet<String>, DbError> {
    let rd = std::fs::read_dir(dir).map_err(|source| DbError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    let mut ids = BTreeSet::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&p).map_err(|source| DbError::Io {
            path: p.clone(),
            source,
        })?;
        let v: Value = serde_json::from_str(&text).map_err(|err| DbError::Parse {
            path: p.clone(),
            msg: err.to_string(),
        })?;
        if let Some(id) = v.get("id").and_then(Value::as_str) {
            ids.insert(id.to_string());
        }
    }
    Ok(ids)
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// One validation finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub unit: String,
    pub path: PathBuf,
    pub msg: String,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}): {}", self.unit, self.path.display(), self.msg)
    }
}

fn validate_unit(u: &Unit, path: &Path, ecudefs: Option<&BTreeSet<String>>, out: &mut Vec<Issue>) {
    let mut err = |msg: String| {
        out.push(Issue {
            unit: u.id.clone(),
            path: path.to_path_buf(),
            msg,
        })
    };

    // Identity.
    let id_ok = !u.id.is_empty()
        && u.id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !id_ok {
        err(format!("id '{}' must be lowercase [a-z0-9-]", u.id));
    }
    if path.file_stem().and_then(|s| s.to_str()) != Some(u.id.as_str()) {
        err(format!("file name must be '{}.json'", u.id));
    }
    for (field, val) in [
        ("maker", &u.maker),
        ("family", &u.family),
        ("name", &u.name),
        ("notes", &u.notes),
        ("read_methods.notes", &u.read_methods.notes),
    ] {
        if val.trim().is_empty() {
            err(format!("{field} must not be empty"));
        }
    }
    if u.bus.is_empty() {
        err("bus must list at least one bus".into());
    }
    if u.flash_kib == Some(0) {
        err("flash_kib must be > 0 or null".into());
    }

    // Cross references.
    if let Some(c) = &u.checksum {
        if opentune_checksum::get(c).is_none() {
            err(format!("checksum '{c}' is not an opentune_checksum id"));
        }
    }
    if let (Some(d), Some(ids)) = (&u.ecudef, ecudefs) {
        if !ids.contains(d) {
            err(format!("ecudef '{d}' not found in ecudefs/"));
        }
    }

    // Sources.
    if u.sources.is_empty() {
        err("at least one source must be cited".into());
    }
    for (i, s) in u.sources.iter().enumerate() {
        if s.title.trim().is_empty() {
            err(format!("sources[{i}].title is empty"));
        }
        if !(s.url.starts_with("https://") || s.url.starts_with("http://")) {
            err(format!("sources[{i}].url must be http(s)"));
        }
    }
    for v in &u.vehicles {
        if v.make.trim().is_empty() || v.model.trim().is_empty() {
            err("vehicle make/model must not be empty".into());
        }
    }

    // Pinout honesty rules.
    if !u.has_pinout() {
        if u.pinout_confidence.is_some() {
            err("pinout_confidence must be null when there is no pinout".into());
        }
        if u.bench.is_some() {
            err("bench must be null when there is no pinout".into());
        }
        if !u.notes.to_lowercase().contains("pinout") {
            err("a unit without a pinout must say so (and why) in notes".into());
        }
        return;
    }
    let Some(unit_conf) = u.pinout_confidence else {
        err("pinout_confidence is required when connectors carry pins".into());
        return;
    };
    if u.safety_note.as_deref().is_none_or(|s| s.trim().is_empty()) {
        err("safety_note is required when a pinout is given".into());
    }
    let mut names = BTreeSet::new();
    for c in &u.connectors {
        if !names.insert(c.name.as_str()) {
            err(format!("duplicate connector name '{}'", c.name));
        }
        let mut pins = BTreeSet::new();
        for p in &c.pins {
            let at = format!("{}-{}", c.name, p.pin);
            if !pins.insert(p.pin.as_str()) {
                err(format!("duplicate pin {at}"));
            }
            if p.function.trim().is_empty() {
                err(format!("{at}: function is empty"));
            }
            if p.sources.is_empty() {
                err(format!("{at}: every pin must cite a source"));
            }
            if let Some(bad) = p.sources.iter().find(|&&i| i >= u.sources.len()) {
                err(format!("{at}: source index {bad} out of range"));
            }
            let distinct: BTreeSet<usize> = p.sources.iter().copied().collect();
            if u.pin_confidence(p) == Confidence::Verified && distinct.len() < 2 {
                err(format!("{at}: 'verified' needs 2+ independent sources"));
            }
        }
    }
    let Some(b) = &u.bench else {
        err("bench is required when a pinout is given".into());
        return;
    };
    if b.notes.trim().is_empty() {
        err("bench.notes must not be empty".into());
    }
    let groups: [(&str, &Vec<String>, &[Signal]); 7] = [
        ("power", &b.power, &[Signal::V12Perm, Signal::V12Ign]),
        ("ground", &b.ground, &[Signal::Gnd]),
        ("ign", &b.ign, &[Signal::V12Ign]),
        ("can_h", &b.can_h, &[Signal::CanH]),
        ("can_l", &b.can_l, &[Signal::CanL]),
        ("kline", &b.kline, &[Signal::Kline]),
        ("boot", &b.boot, &[Signal::Boot]),
    ];
    let mut weakest: Option<Confidence> = None;
    for (group, refs, allowed) in groups {
        for r in refs {
            match u.resolve_pin(r) {
                None => err(format!("bench.{group}: '{r}' does not resolve to a pin")),
                Some(p) => {
                    if !allowed.contains(&p.signal) {
                        err(format!("bench.{group}: '{r}' has signal {:?}", p.signal));
                    }
                    let c = u.pin_confidence(p);
                    if c == Confidence::Unverified {
                        err(format!(
                            "bench.{group}: '{r}' is unverified and must not be wired"
                        ));
                    }
                    weakest = Some(weakest.map_or(c, |w| w.min(c)));
                }
            }
        }
    }
    // An empty ground list is allowed only when the notes explain why (e.g.
    // the sources disagree on which pin is ground).
    if b.ground.is_empty() && !b.notes.to_lowercase().contains("ground") {
        err("bench.ground is empty without an explanation in bench.notes".into());
    }
    match weakest {
        None => err("bench wiring references no pins".into()),
        Some(w) if unit_conf > w => err(format!(
            "pinout_confidence '{}' is higher than the weakest bench pin ('{}')",
            unit_conf.as_str(),
            w.as_str()
        )),
        _ => {}
    }
}
