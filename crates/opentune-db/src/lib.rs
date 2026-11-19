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
//! Each unit can also carry an **access profile** ([`Access`]): how it is
//! reached over OBD, on the bench, in a processor boot mode (entry, strap
//! points, whether the case must be opened) and what security access the
//! diagnostic read and the flash write need. Every part cites sources and
//! states a confidence; [`Database::validate`] checks it against
//! `read_methods`, and [`Database::access_gate`] keeps new units from being
//! added without one (`database/units/.access-pending` lists the older units
//! that are not filled yet; the list only shrinks). See
//! `docs/UNIT-ACCESS-PROFILE.md`.
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
    /// A complete read over OBD, but no programming.
    #[serde(rename = "read")]
    Read,
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

impl ObdAccess {
    /// The stable JSON token.
    pub fn as_str(self) -> &'static str {
        match self {
            ObdAccess::ReadWrite => "read+write",
            ObdAccess::Read => "read",
            ObdAccess::WriteOnly => "write-only",
            ObdAccess::None => "none",
            ObdAccess::Unknown => "unknown",
        }
    }
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
    /// How a `signal: boot` pin is strapped to enter the boot mode. Only
    /// allowed on boot pins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strap: Option<Strap>,
}

/// What to do with a boot strap point or pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StrapAction {
    /// Pull the net to ground (usually through a resistor).
    #[serde(rename = "pull-to-gnd")]
    PullToGnd,
    /// Pull the net to the logic supply.
    #[serde(rename = "pull-to-vcc")]
    PullToVcc,
    /// Bridge two points.
    #[serde(rename = "bridge")]
    Bridge,
    /// Attach a debug probe (BDM / JTAG header or pads). Not a strap: only on
    /// a boot point whose entry is `bdm-jtag`, never on a pin's `strap`.
    #[serde(rename = "connect")]
    Connect,
}

/// The strap of a `signal: boot` connector pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Strap {
    pub action: StrapAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resistor_ohms: Option<u32>,
}

// ---------------------------------------------------------------------------
// Access profile (docs/UNIT-ACCESS-PROFILE.md)
// ---------------------------------------------------------------------------

/// The phrase every `access.security.notes` must contain. OpenTune never
/// ships OEM seed/key material, and the schema has no field that could hold
/// any.
pub const NO_KEY_PHRASE: &str = "OpenTune ships no OEM key";

/// File in `database/units/` listing the ids of units whose access profile
/// is not filled yet. Any unit not listed must carry `access`.
pub const ACCESS_PENDING_FILE: &str = ".access-pending";

/// How a unit can be reached: OBD, bench, boot mode, and the security access
/// in the way. Every part cites sources (indices into the unit's `sources`)
/// and states a confidence with the same vocabulary as pins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Access {
    pub obd: AccessObd,
    pub bench: AccessBench,
    pub boot: AccessBoot,
    pub security: AccessSecurity,
}

/// Access through the vehicle's diagnostic socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessObd {
    /// Must equal `read_methods.obd`.
    pub mode: ObdAccess,
    /// Bus and protocol in plain words, or null when not known.
    #[serde(default)]
    pub protocol: Option<String>,
    pub notes: String,
    pub confidence: Confidence,
    pub sources: Vec<usize>,
}

/// Whether an access route exists: `true`, `false` (a source says there is
/// none) or `"unknown"` (no public source documents one either way). Plain
/// JSON booleans keep parsing, so older files and embedders are unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Possible {
    Yes,
    No,
    Unknown,
}

impl Possible {
    /// Only `Yes` counts as an available route (`read_methods` is a bool).
    pub fn is_yes(self) -> bool {
        self == Possible::Yes
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Possible::Yes => "true",
            Possible::No => "false",
            Possible::Unknown => "unknown",
        }
    }
}

impl fmt::Display for Possible {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Possible {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Possible::Yes => s.serialize_bool(true),
            Possible::No => s.serialize_bool(false),
            Possible::Unknown => s.serialize_str("unknown"),
        }
    }
}

impl<'de> Deserialize<'de> for Possible {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bool(bool),
            Str(String),
        }
        match Raw::deserialize(d)? {
            Raw::Bool(true) => Ok(Possible::Yes),
            Raw::Bool(false) => Ok(Possible::No),
            Raw::Str(s) if s == "unknown" => Ok(Possible::Unknown),
            Raw::Str(s) => Err(serde::de::Error::custom(format!(
                "possible must be true, false or \"unknown\", not \"{s}\""
            ))),
        }
    }
}

/// Access on the bench through the unit's own connector, case closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessBench {
    /// `true` must match `read_methods.bench = true`; `false`/`"unknown"`
    /// match `read_methods.bench = false`.
    pub possible: Possible,
    pub how: String,
    pub confidence: Confidence,
    pub sources: Vec<usize>,
}

/// Where a boot mode is entered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BootEntry {
    /// A pin on the unit's own connector.
    ConnectorPin,
    /// A pad, component pin or test point on the board.
    BoardPad,
    /// A BDM / JTAG / debug header.
    BdmJtag,
    /// A source says there is no usable boot entry.
    None,
    /// No public source either way.
    Unknown,
}

/// Whether the boot points can be reached without opening the unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reachable {
    ExternalConnector,
    OpenCase,
    Unknown,
}

/// One boot strap point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootPoint {
    pub name: String,
    /// `connector:<connector>-<pin>` (resolves to a pin of this unit) or
    /// `pcb:<label>` (a board pad, component pin or test point).
    pub location: String,
    pub action: StrapAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resistor_ohms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub confidence: Confidence,
}

/// A processor boot / service mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessBoot {
    /// `true`: a documented boot route (points, entry, reachable required).
    /// `false`: a source says there is no boot route (entry `none`).
    /// `"unknown"`: no public source documents one either way.
    /// `true` must match `read_methods.boot = true`; the others match `false`.
    pub possible: Possible,
    pub entry: BootEntry,
    /// The processor's boot mode in plain words (loader, serial settings).
    #[serde(default)]
    pub mcu_mode: Option<String>,
    #[serde(default)]
    pub points: Vec<BootPoint>,
    #[serde(default)]
    pub reset_timing: Option<String>,
    pub reachable: Reachable,
    pub notes: String,
    pub confidence: Confidence,
    pub sources: Vec<usize>,
}

/// Whether security access is needed for an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SecurityNeed {
    Required,
    NotRequired,
    Unknown,
}

/// The security access in the way of a read or write. Facts only: there is
/// deliberately no field for keys, seeds or algorithms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessSecurity {
    /// A complete read over the diagnostic protocol (OBD or bench).
    pub diag_read: SecurityNeed,
    /// Programming over the diagnostic protocol.
    pub flash_write: SecurityNeed,
    /// Must contain [`NO_KEY_PHRASE`].
    pub notes: String,
    pub confidence: Confidence,
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
    /// A caveat on the flash size, e.g. that it differs by hardware revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flash_note: Option<String>,
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
    /// The access profile; required for every unit not listed in
    /// `.access-pending` (see [`Database::access_gate`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<Access>,
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
            has_access: self.access.is_some(),
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
    /// The unit carries an access profile.
    pub has_access: bool,
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

    /// The `--require-access` gate. Every unit must carry an `access`
    /// profile unless it is listed in `pending` (`.access-pending`), which
    /// only holds units that predate the profile:
    ///
    /// * listed and not filled: warning;
    /// * not listed and not filled (a new unit): error;
    /// * listed but filled: error (remove it from the list, which only shrinks);
    /// * listed id with no unit file: error (stale entry).
    pub fn access_gate(&self, pending: &AccessPending) -> AccessGate {
        let mut gate = AccessGate::default();
        let issue = |e: &Entry, msg: String| Issue {
            unit: e.unit.id.clone(),
            path: e.path.clone(),
            msg,
        };
        for e in &self.entries {
            let listed = pending.ids.contains(&e.unit.id);
            match (e.unit.access.is_some(), listed) {
                (true, false) => {}
                (true, true) => gate.errors.push(issue(
                    e,
                    format!(
                        "has an access profile: remove it from {} (the list only shrinks)",
                        ACCESS_PENDING_FILE
                    ),
                )),
                (false, true) => gate.warnings.push(issue(
                    e,
                    format!("access profile not filled yet (listed in {ACCESS_PENDING_FILE})"),
                )),
                (false, false) => gate.errors.push(issue(
                    e,
                    format!(
                        "no access profile: a new unit must carry `access` \
                         (docs/UNIT-ACCESS-PROFILE.md); {ACCESS_PENDING_FILE} only lists older units"
                    ),
                )),
            }
        }
        for id in &pending.ids {
            if self.unit(id).is_none() {
                gate.errors.push(Issue {
                    unit: id.clone(),
                    path: pending.path.clone(),
                    msg: format!("stale {ACCESS_PENDING_FILE} entry: no unit has this id"),
                });
            }
        }
        gate
    }
}

/// The parsed `.access-pending` list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccessPending {
    pub path: PathBuf,
    pub ids: BTreeSet<String>,
}

/// Result of [`Database::access_gate`].
#[derive(Debug, Clone, Default)]
pub struct AccessGate {
    pub errors: Vec<Issue>,
    pub warnings: Vec<Issue>,
}

/// Read `.access-pending` from the `database` directory (or the
/// `database/units` directory itself): one unit id per line, `#` starts a
/// comment. A missing file is an empty list.
pub fn load_access_pending(dir: &Path) -> Result<AccessPending, DbError> {
    let units = dir.join("units");
    let dir = if units.is_dir() {
        units
    } else {
        dir.to_path_buf()
    };
    let path = dir.join(ACCESS_PENDING_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => return Err(DbError::Io { path, source }),
    };
    let mut ids = BTreeSet::new();
    for (n, line) in text.lines().enumerate() {
        let id = line.split('#').next().unwrap_or("").trim();
        if id.is_empty() {
            continue;
        }
        if !ids.insert(id.to_string()) {
            return Err(DbError::Parse {
                path,
                msg: format!("line {}: duplicate id '{id}'", n + 1),
            });
        }
    }
    Ok(AccessPending { path, ids })
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

/// The access-profile rules (docs/UNIT-ACCESS-PROFILE.md).
fn validate_access(u: &Unit, a: &Access, err: &mut dyn FnMut(String)) {
    let rm = &u.read_methods;
    // Agreement with read_methods.
    if a.obd.mode != rm.obd {
        err(format!(
            "access.obd.mode '{}' disagrees with read_methods.obd '{}'",
            a.obd.mode.as_str(),
            rm.obd.as_str()
        ));
    }
    if a.bench.possible.is_yes() != rm.bench {
        err(format!(
            "access.bench.possible ({}) disagrees with read_methods.bench ({})",
            a.bench.possible, rm.bench
        ));
    }
    if a.boot.possible.is_yes() != rm.boot {
        err(format!(
            "access.boot.possible ({}) disagrees with read_methods.boot ({})",
            a.boot.possible, rm.boot
        ));
    }

    // Sources and confidence of every part.
    let parts: [(&str, &[usize], Confidence, &str); 4] = [
        ("obd", &a.obd.sources, a.obd.confidence, &a.obd.notes),
        ("bench", &a.bench.sources, a.bench.confidence, &a.bench.how),
        ("boot", &a.boot.sources, a.boot.confidence, &a.boot.notes),
        (
            "security",
            &a.security.sources,
            a.security.confidence,
            &a.security.notes,
        ),
    ];
    for (part, sources, conf, text) in parts {
        if sources.is_empty() {
            err(format!("access.{part}: must cite at least one source"));
        }
        if let Some(bad) = sources.iter().find(|&&i| i >= u.sources.len()) {
            err(format!("access.{part}: source index {bad} out of range"));
        }
        let distinct: BTreeSet<usize> = sources.iter().copied().collect();
        if conf == Confidence::Verified && distinct.len() < 2 {
            err(format!(
                "access.{part}: 'verified' needs 2+ independent sources"
            ));
        }
        if text.trim().is_empty() {
            let field = if part == "bench" { "how" } else { "notes" };
            err(format!("access.{part}.{field} must not be empty"));
        }
    }

    // Boot.
    let b = &a.boot;
    match b.possible {
        Possible::Yes => {
            if b.points.is_empty() {
                err("access.boot: possible=true needs at least one boot point".into());
            }
            if b.reachable == Reachable::Unknown {
                err(
                    "access.boot: possible=true needs 'reachable' (external-connector or open-case)"
                        .into(),
                );
            }
            if matches!(b.entry, BootEntry::None | BootEntry::Unknown) {
                err("access.boot: possible=true needs a known 'entry'".into());
            }
        }
        Possible::No => {
            // A source says there is no boot route.
            if b.entry != BootEntry::None {
                err("access.boot: possible=false (no boot route) needs entry 'none'; use possible \"unknown\" when no source documents a route".into());
            }
            if !b.points.is_empty() {
                err("access.boot: possible=false cannot list boot points".into());
            }
            if b.reachable != Reachable::Unknown {
                err("access.boot: possible=false needs reachable 'unknown'".into());
            }
        }
        Possible::Unknown => {
            if b.entry != BootEntry::Unknown {
                err("access.boot: possible \"unknown\" needs entry 'unknown'".into());
            }
            if !b.points.is_empty() {
                err("access.boot: possible \"unknown\" cannot list boot points".into());
            }
            if b.reachable != Reachable::Unknown {
                err("access.boot: possible \"unknown\" needs reachable 'unknown'".into());
            }
        }
    }
    let boot_sources: BTreeSet<usize> = b.sources.iter().copied().collect();
    for (i, p) in b.points.iter().enumerate() {
        let at = format!("access.boot.points[{i}]");
        if p.name.trim().is_empty() {
            err(format!("{at}: name is empty"));
        }
        if p.resistor_ohms == Some(0) {
            err(format!("{at}: resistor_ohms must be > 0"));
        }
        if p.action == StrapAction::Connect && b.entry != BootEntry::BdmJtag {
            err(format!(
                "{at}: action 'connect' (attach a debug probe) needs entry 'bdm-jtag'"
            ));
        }
        if p.confidence == Confidence::Verified && boot_sources.len() < 2 {
            err(format!(
                "{at}: 'verified' needs 2+ independent boot sources"
            ));
        }
        if p.confidence > b.confidence {
            err(format!(
                "{at}: confidence '{}' is higher than access.boot's '{}'",
                p.confidence.as_str(),
                b.confidence.as_str()
            ));
        }
        if let Some(r) = p.location.strip_prefix("connector:") {
            match u.resolve_pin(r) {
                None => err(format!("{at}: '{}' does not resolve to a pin", p.location)),
                Some(pin) if pin.signal != Signal::Boot => {
                    err(format!("{at}: '{}' is not a 'boot' pin", p.location))
                }
                Some(_) => {}
            }
        } else if let Some(label) = p.location.strip_prefix("pcb:") {
            if label.trim().is_empty() {
                err(format!("{at}: 'pcb:' needs a label"));
            }
            if b.reachable == Reachable::ExternalConnector {
                err(format!(
                    "{at}: a board point cannot be reachable from the external connector"
                ));
            }
        } else {
            err(format!(
                "{at}: location must be 'connector:<connector>-<pin>' or 'pcb:<label>'"
            ));
        }
    }

    // Security: facts only, and say that no key is shipped.
    if !a
        .security
        .notes
        .to_lowercase()
        .contains(&NO_KEY_PHRASE.to_lowercase())
    {
        err(format!(
            "access.security.notes must state \"{NO_KEY_PHRASE}\""
        ));
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
    if u.flash_note.as_deref().is_some_and(|s| s.trim().is_empty()) {
        err("flash_note must not be empty when present".into());
    }

    // Boot straps belong on boot pins only.
    for c in &u.connectors {
        for p in &c.pins {
            if let Some(s) = &p.strap {
                if p.signal != Signal::Boot {
                    err(format!(
                        "{}-{}: strap is only allowed on a 'boot' pin",
                        c.name, p.pin
                    ));
                }
                if s.resistor_ohms == Some(0) {
                    err(format!(
                        "{}-{}: strap.resistor_ohms must be > 0",
                        c.name, p.pin
                    ));
                }
                if s.action == StrapAction::Connect {
                    err(format!(
                        "{}-{}: strap.action 'connect' is for bdm-jtag boot points, not a pin strap",
                        c.name, p.pin
                    ));
                }
            }
        }
    }

    // Access profile.
    if let Some(a) = &u.access {
        validate_access(u, a, &mut err);
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
