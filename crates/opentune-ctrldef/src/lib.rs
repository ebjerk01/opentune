//! # opentune-ctrldef — drivetrain controller definitions
//!
//! One data-defined shape for the units that sit in the driveline: automatic /
//! dual-clutch **transmission control units** (TCU) and **AWD/4WD** couplings,
//! transfer cases and active differentials. A controller definition
//! (`ctrldefs/*.json`, schema `docs/ctrldef.schema.json`, guide
//! `docs/CONTROLLER-DEFS.md`) carries, all optional except identity, safety and
//! sources:
//!
//! * **identity** — kind, maker, family, vehicles, part/software-number and
//!   gearbox-code patterns ([`Identify`]);
//! * **links** — the `ecudefs/` definitions that hold the flash layout, maps and
//!   checksum of each software generation, and the `database/units/` entries
//!   with pinouts and bench notes (nothing is duplicated);
//! * **diagnostics** — one [`DiagChannel`] per way to reach the unit: UDS or
//!   KWP2000 over ISO-TP, KWP2000 over VW TP2.0, K-line, with ids/addresses;
//! * **CAN broadcast decode** — named [`CanBus`] frame sets (e.g. `pq`/`mqb`,
//!   `egs51`) with DBC-style [`Signal`]s: Intel or Motorola bit order,
//!   signed, linear or piecewise-linear scaling, enums, invalid markers;
//! * **live channels** ([`LiveChannel`]) that name a CAN signal, a KWP2000
//!   measuring-block cell or a UDS DID;
//! * **adaptations / basic settings** ([`Adaptation`]);
//! * **tables** ([`TableDef`]) — shift schedules, shift time, fill/pressure, TCC
//!   and adaptation maps with their axes, with or without a known storage
//!   address, each with a confidence label;
//! * **gear sets** ([`GearSet`]), a **checksum** id, an optional inline
//!   [`FlashLayout`], **safety** notes and **sources** with licences.
//!
//! [`load_dir`] reads a directory, [`ControllerDef::validate`] checks the
//! structural and honesty rules, [`ControllerDef::validate_links`] checks the
//! references, and [`decode`] turns CAN frames into named, scaled signal values
//! (used by `opentune controller decode`).
#![forbid(unsafe_code)]

pub mod decode;
pub mod hexnum;
pub mod signal;
pub mod validate;

use opentune_core::ecudef::{DataType, Endian, FullRead, NamedRegion};
use opentune_core::{DiagLayer, Protocol};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub use decode::{DecodedFrame, DecodedSignal, Decoder, Summary, Value};
pub use validate::{has_errors, Issue, LinkContext, Severity};

/// What kind of drivetrain unit a definition describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControllerKind {
    /// Transmission control unit (automatic, DCT, CVT, AMT).
    Tcu,
    /// AWD/4WD coupling or its controller (Haldex, BorgWarner ITM, ...).
    Awd,
    /// Transfer case controller.
    TransferCase,
    /// Active / locking differential controller.
    Differential,
    /// Anything else in the driveline (e.g. an aftermarket coupling box).
    Other,
}

impl ControllerKind {
    /// Wire name (`tcu`, `awd`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            ControllerKind::Tcu => "tcu",
            ControllerKind::Awd => "awd",
            ControllerKind::TransferCase => "transfer_case",
            ControllerKind::Differential => "differential",
            ControllerKind::Other => "other",
        }
    }
}

/// Evidence level, exactly the vocabulary of `docs/DEVELOPMENT.md` §7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ValidationState {
    /// Written down from sources; no test exercises it yet.
    Implemented,
    /// Tests pass on synthetic data (constructed frames, simulators).
    SyntheticTested,
    /// Checked against real dumps / real captures offline.
    RealImageTested,
    /// Exercised against real hardware on the bench.
    BenchTested,
    /// Exercised end to end on a real vehicle.
    VehicleTested,
}

impl ValidationState {
    /// The vocabulary word (`synthetic-tested`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            ValidationState::Implemented => "implemented",
            ValidationState::SyntheticTested => "synthetic-tested",
            ValidationState::RealImageTested => "real-image-tested",
            ValidationState::BenchTested => "bench-tested",
            ValidationState::VehicleTested => "vehicle-tested",
        }
    }
}

/// A drivetrain controller definition (one `ctrldefs/*.json` file).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerDef {
    /// Stable id, lowercase, equal to the file stem: `<maker>-<family>[-<variant>]`.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Unit kind.
    pub kind: ControllerKind,
    /// Controller supplier (e.g. `Siemens`, `Temic`, `BorgWarner`).
    pub maker: String,
    /// Family (e.g. `722.6 EGS51`, `DQ250`, `Haldex Gen4`).
    pub family: String,
    /// Vehicles / platforms (free text, examples).
    #[serde(default)]
    pub vehicles: Vec<String>,
    /// Overall evidence level of this definition.
    pub validation: ValidationState,
    /// Free text: scope, what is unknown, how to confirm.
    #[serde(default)]
    pub notes: String,
    /// Keys that match a unit's reported identity to this definition.
    #[serde(default)]
    pub identify: Identify,
    /// `ecudefs/` ids that hold this controller's flash layout / maps / checksum
    /// (one per software generation).
    #[serde(default)]
    pub ecudefs: Vec<String>,
    /// `database/units/` ids with pinouts and bench notes.
    #[serde(default)]
    pub units: Vec<String>,
    /// Ways to reach the unit for diagnostics.
    #[serde(default)]
    pub diagnostics: Vec<DiagChannel>,
    /// CAN frame sets the unit sends or listens to.
    #[serde(default)]
    pub buses: Vec<CanBus>,
    /// Live values (CAN signal, measuring-block cell or UDS DID).
    #[serde(default)]
    pub live: Vec<LiveChannel>,
    /// Adaptations, basic settings and routines.
    #[serde(default)]
    pub adaptations: Vec<Adaptation>,
    /// Shift / pressure / TCC / adaptation tables with axes.
    #[serde(default)]
    pub tables: Vec<TableDef>,
    /// Gear ratio sets (per gearbox variant).
    #[serde(default)]
    pub gearsets: Vec<GearSet>,
    /// Checksum algorithm id (`opentune-checksum`) when the unit has one and no
    /// linked ecudef already names it.
    #[serde(default)]
    pub checksum: Option<String>,
    /// Inline flash layout, for a controller without an `ecudefs/` entry.
    #[serde(default)]
    pub flash: Option<FlashLayout>,
    /// Safety notes shown before any live action. At least one is required.
    pub safety: Vec<String>,
    /// Where the facts come from, with licences. At least one is required.
    pub sources: Vec<Source>,
}

/// Identification keys. A key without `*`/`?` matches as a case- and
/// space-insensitive prefix or substring (like ecudef `identify`); a key with
/// wildcards must match the whole value (`*` = any run, `?` = one character).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identify {
    /// Control-unit / spare-part numbers or patterns.
    #[serde(default)]
    pub part_numbers: Vec<String>,
    /// Software numbers or patterns.
    #[serde(default)]
    pub sw_numbers: Vec<String>,
    /// Gearbox / coupling codes (e.g. `02E`, `722.6`, `0AY`).
    #[serde(default)]
    pub gearbox_codes: Vec<String>,
}

fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase()
}

fn glob(p: &[u8], v: &[u8]) -> bool {
    match (p.first(), v.first()) {
        (None, None) => true,
        (Some(b'*'), _) => glob(&p[1..], v) || (!v.is_empty() && glob(p, &v[1..])),
        (Some(b'?'), Some(_)) => glob(&p[1..], &v[1..]),
        (Some(a), Some(b)) if a == b => glob(&p[1..], &v[1..]),
        _ => false,
    }
}

/// Does `value` match `key` under the [`Identify`] rules?
pub fn key_matches(key: &str, value: &str) -> bool {
    let (k, v) = (norm(key), norm(value));
    if k.is_empty() {
        return false;
    }
    if k.contains('*') || k.contains('?') {
        glob(k.as_bytes(), v.as_bytes())
    } else {
        v.starts_with(&k) || v.contains(&k)
    }
}

impl Identify {
    /// True if a reported part number matches.
    pub fn matches_part(&self, part: &str) -> bool {
        self.part_numbers.iter().any(|k| key_matches(k, part))
    }
    /// True if a reported software number matches.
    pub fn matches_sw(&self, sw: &str) -> bool {
        self.sw_numbers.iter().any(|k| key_matches(k, sw))
    }
    /// True if a gearbox / coupling code matches.
    pub fn matches_code(&self, code: &str) -> bool {
        self.gearbox_codes.iter().any(|k| key_matches(k, code))
    }
    /// True if no keys are set.
    pub fn is_empty(&self) -> bool {
        self.part_numbers.is_empty() && self.sw_numbers.is_empty() && self.gearbox_codes.is_empty()
    }
}

/// One way to reach the unit for diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagChannel {
    /// Short name (`kwp-isotp`, `tp20`, `uds`).
    pub name: String,
    /// Transport, as in ecudefs: `iso_tp`, `vwtp20`, `iso14230_kline`, `iso9141`, `raw_can`.
    pub transport: Protocol,
    /// Service layer: `uds` or `kwp2000`.
    pub layer: DiagLayer,
    /// CAN bit/s or K-line baud.
    pub bitrate: u32,
    /// Request CAN id (ISO-TP).
    #[serde(default, with = "hexnum::opt_u32_hex")]
    pub tx_id: Option<u32>,
    /// Response CAN id (ISO-TP), or the id the tester asks a TP2.0 unit to answer on.
    #[serde(default, with = "hexnum::opt_u32_hex")]
    pub rx_id: Option<u32>,
    /// 29-bit CAN ids.
    #[serde(default)]
    pub extended_ids: bool,
    /// VW TP2.0 logical address (channel setup on 0x200).
    #[serde(default, with = "hexnum::opt_u8_hex")]
    pub tp20_address: Option<u8>,
    /// K-line target address.
    #[serde(default, with = "hexnum::opt_u8_hex")]
    pub kline_address: Option<u8>,
    /// Diagnostic session byte used for reads (e.g. KWP 0x89).
    #[serde(default, with = "hexnum::opt_u8_hex")]
    pub session: Option<u8>,
    /// Identification request, hex (e.g. `1A9B`, `22F187`).
    #[serde(default)]
    pub identification: Option<String>,
    /// Evidence for this channel.
    pub validation: ValidationState,
    /// Free text.
    #[serde(default)]
    pub notes: String,
}

/// A named set of CAN frames on one bus (e.g. `pq`, `egs51`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanBus {
    /// Set name, selected with `--bus` (lowercase).
    pub name: String,
    /// Bus description (e.g. "drivetrain CAN", "CAN-C").
    #[serde(default)]
    pub description: String,
    /// Bit rate in bit/s.
    pub bitrate: u32,
    /// Free text.
    #[serde(default)]
    pub notes: String,
    /// The frames.
    pub frames: Vec<CanFrameDef>,
}

/// One broadcast frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanFrameDef {
    /// CAN id (number or `"0x…"`).
    #[serde(with = "hexnum::u32_hex")]
    pub id: u32,
    /// 29-bit id.
    #[serde(default)]
    pub extended: bool,
    /// Frame name (e.g. `Allrad_1`, `GS_418`).
    pub name: String,
    /// Sending unit (e.g. `ABS`, `engine`, `TCU`).
    #[serde(default)]
    pub sender: String,
    /// Nominal payload length; when set, signals must fit in it.
    #[serde(default)]
    pub dlc: Option<u8>,
    /// A shorter payload is not decoded at all (default 1).
    #[serde(default)]
    pub min_len: Option<u8>,
    /// Nominal cycle time, ms.
    #[serde(default)]
    pub cycle_ms: Option<u32>,
    /// Evidence for this frame layout, when it differs from the definition's.
    #[serde(default)]
    pub validation: Option<ValidationState>,
    /// Free text (source, caveats).
    #[serde(default)]
    pub notes: String,
    /// The signals.
    pub signals: Vec<Signal>,
}

/// Bit order of a signal, with DBC semantics for `start`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ByteOrder {
    /// Intel (DBC `@1`): `start` is the least-significant bit; bit `n` is byte
    /// `n / 8`, bit `n % 8`.
    #[default]
    Little,
    /// Motorola (DBC `@0`): `start` is the most-significant bit in DBC numbering
    /// (byte `start / 8`, bit `start % 8`); the signal continues towards bit 0
    /// of that byte, then bit 7 of the next byte.
    Big,
}

/// How a signal's raw value is presented.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    /// Scaled number (`factor`/`offset` or `curve`).
    #[default]
    Number,
    /// One bit, true when set.
    Bool,
    /// Named raw values (`values`).
    Enum,
    /// One ASCII character (display letters).
    Ascii,
}

fn one() -> f64 {
    1.0
}

fn is_one(v: &f64) -> bool {
    *v == 1.0
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

/// One signal in a frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signal {
    /// Signal name, unique within the frame.
    pub name: String,
    /// Start bit (see [`ByteOrder`]).
    pub start: u16,
    /// Length in bits, 1..=64.
    pub len: u8,
    /// Bit order.
    #[serde(default)]
    pub byte_order: ByteOrder,
    /// Two's-complement signed raw value.
    #[serde(default)]
    pub signed: bool,
    /// Presentation.
    #[serde(default)]
    pub kind: SignalKind,
    /// physical = raw * factor + offset.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub factor: f64,
    /// See `factor`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub offset: f64,
    /// Piecewise-linear `[raw, physical]` points (raw ascending), clamped at the
    /// ends; replaces `factor`/`offset` when present.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub curve: Vec<[f64; 2]>,
    /// Physical unit.
    #[serde(default)]
    pub unit: String,
    /// Raw values that mean "not available / implausible".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invalid: Vec<u64>,
    /// Valid raw range `[min, max]`; outside it the value is invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_range: Option<[u64; 2]>,
    /// Named raw values (for `enum`, optional for others).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<EnumValue>,
    /// Free text (meaning, source, confidence).
    #[serde(default)]
    pub notes: String,
}

/// A named raw value.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnumValue {
    /// Raw value.
    pub raw: u64,
    /// Short name.
    pub name: String,
}

/// A live value and where to read it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveChannel {
    /// Channel name (e.g. `ATF temperature`).
    pub name: String,
    /// Physical unit.
    #[serde(default)]
    pub unit: String,
    /// Source.
    pub source: LiveSource,
    /// Free text.
    #[serde(default)]
    pub notes: String,
}

/// Where a live channel comes from.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LiveSource {
    /// A signal of a broadcast frame in this definition.
    CanSignal {
        /// [`CanBus::name`].
        bus: String,
        /// [`CanFrameDef::name`].
        frame: String,
        /// [`Signal::name`].
        signal: String,
    },
    /// A cell of a KWP2000 `0x21` measuring block (VAG 3-byte cells, decoded
    /// by the engine's formula table).
    KwpMeasuringBlock {
        /// Group number.
        group: u8,
        /// Cell index, 0-based.
        cell: u8,
    },
    /// A UDS `0x22` data identifier, read as an unsigned big-endian number.
    UdsDid {
        /// DID.
        #[serde(with = "hexnum::u16_hex")]
        did: u16,
        /// First byte of the value in the response record.
        #[serde(default)]
        byte: u16,
        /// Length in bytes (1..=4).
        len: u8,
        /// Two's-complement signed.
        #[serde(default)]
        signed: bool,
        /// physical = raw * factor + offset.
        #[serde(default = "one")]
        factor: f64,
        /// See `factor`.
        #[serde(default)]
        offset: f64,
    },
}

/// What an adaptation entry does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdaptationKind {
    /// A learned or set value the unit keeps.
    Adaptation,
    /// A basic setting / calibration run.
    BasicSetting,
    /// A routine (UDS 0x31 / KWP 0x31).
    Routine,
    /// Reset of learned values.
    Reset,
}

/// An adaptation, basic setting or routine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Adaptation {
    /// Name.
    pub name: String,
    /// Kind.
    pub kind: AdaptationKind,
    /// Service and identifier as text (e.g. `KWP 0x31 0x33`, `UDS 0x2E 0xFDA2`),
    /// empty when not publicly known.
    #[serde(default)]
    pub service: String,
    /// Whether running it changes the unit's state.
    #[serde(default)]
    pub writes: bool,
    /// Whether security access is needed.
    #[serde(default)]
    pub requires_security: bool,
    /// [`TableDef::name`] holding the learned values, if any.
    #[serde(default)]
    pub table: Option<String>,
    /// Evidence.
    pub validation: ValidationState,
    /// Free text.
    #[serde(default)]
    pub notes: String,
}

/// What a table controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableRole {
    /// Upshift schedule.
    ShiftUp,
    /// Downshift schedule.
    ShiftDown,
    /// Target shift time.
    ShiftTime,
    /// Shift / working pressure.
    ShiftPressure,
    /// Clutch fill time.
    FillTime,
    /// Clutch fill pressure.
    FillPressure,
    /// Torque-converter clutch slip target.
    TccSlip,
    /// Torque-converter clutch pressure / PWM.
    TccPressure,
    /// Torque limit.
    TorqueLimit,
    /// Learned torque / pressure adaptation.
    Adaptation,
    /// Drive-off / launch.
    Launch,
    /// AWD lock / coupling map.
    CouplingLock,
    /// Anything else.
    Other,
}

/// How sure the definition is about a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Location and meaning confirmed.
    High,
    /// Location/shape confirmed, meaning inferred.
    Medium,
    /// Guess; review before use.
    Low,
    /// Only the shape and axes are known (no storage location).
    StructureOnly,
}

/// One table axis: numeric breakpoints, labels, or a derived axis.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableAxis {
    /// Axis name (e.g. `pedal`, `shift`, `ATF temperature`).
    pub name: String,
    /// Unit.
    #[serde(default)]
    pub unit: String,
    /// Numeric breakpoints.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<f64>,
    /// Categorical labels (e.g. `1-2`, `K1`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// How the breakpoints are computed when they are not fixed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub derived: String,
    /// Breakpoint count for a derived axis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
}

impl TableAxis {
    /// Number of breakpoints this axis declares.
    pub fn len(&self) -> u32 {
        if !self.values.is_empty() {
            self.values.len() as u32
        } else if !self.labels.is_empty() {
            self.labels.len() as u32
        } else {
            self.count.unwrap_or(0)
        }
    }
    /// True when the axis declares no breakpoints.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Where a table's cells are stored in a flash image.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableStorage {
    /// Byte offset of the first cell in the image.
    pub address: u32,
    /// Cell type.
    pub data_type: DataType,
    /// Byte order.
    pub endian: Endian,
    /// physical = raw * factor + offset.
    #[serde(default = "one")]
    pub factor: f64,
    /// See `factor`.
    #[serde(default)]
    pub offset: f64,
}

/// A map in a linked ecudef.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EcuMapRef {
    /// ecudef id (must be in [`ControllerDef::ecudefs`]).
    pub ecudef: String,
    /// Map name in that ecudef.
    pub map: String,
}

/// A shift / pressure / TCC / adaptation table.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableDef {
    /// Name, unique within the definition.
    pub name: String,
    /// What it controls.
    pub role: TableRole,
    /// Driving programmes / profiles it applies to (e.g. `S`, `C`).
    #[serde(default)]
    pub profiles: Vec<String>,
    /// Cell unit.
    #[serde(default)]
    pub unit: String,
    /// Y dimension (1 for a curve).
    pub rows: u32,
    /// X dimension.
    pub cols: u32,
    /// X axis (`cols` breakpoints).
    pub x_axis: TableAxis,
    /// Y axis (`rows` breakpoints); omit for a curve.
    #[serde(default)]
    pub y_axis: Option<TableAxis>,
    /// Storage in a flash image, when known.
    #[serde(default)]
    pub storage: Option<TableStorage>,
    /// The same table as a map of a linked ecudef.
    #[serde(default)]
    pub ecudef_map: Option<EcuMapRef>,
    /// Confidence label.
    pub confidence: Confidence,
    /// Free text.
    #[serde(default)]
    pub notes: String,
    /// Source of the structure.
    #[serde(default)]
    pub source: String,
}

/// One gear and its ratio.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gear {
    /// Gear name (`1`..`9`, `R`, `R1`, `R2`).
    pub gear: String,
    /// Ratio (input / output speed); reverse gears are negative.
    pub ratio: f64,
}

/// The gear ratios of one gearbox variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GearSet {
    /// Variant name (e.g. `W5A 580 (1996)`).
    pub name: String,
    /// Applications / gearbox codes.
    #[serde(default)]
    pub applies_to: Vec<String>,
    /// The gears, forward first.
    pub gears: Vec<Gear>,
    /// Source.
    #[serde(default)]
    pub source: String,
    /// Free text.
    #[serde(default)]
    pub notes: String,
}

/// Inline flash layout (for controllers without an ecudef).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlashLayout {
    /// Image size in bytes (0 = unknown).
    pub size: u32,
    /// Named regions.
    #[serde(default)]
    pub regions: Vec<NamedRegion>,
    /// How a complete original is obtained.
    #[serde(default)]
    pub full_read: FullRead,
    /// Free text.
    #[serde(default)]
    pub notes: String,
}

/// A cited source.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// Title / project.
    pub title: String,
    /// Link.
    #[serde(default)]
    pub url: String,
    /// Licence of the source (`GPL-3.0`, `MIT`, `facts`, `standard`, ...).
    pub license: String,
    /// What was taken from it.
    pub used_for: String,
}

/// Error loading a definition file.
#[derive(Debug)]
pub struct LoadError {
    /// The file.
    pub path: PathBuf,
    /// What went wrong.
    pub message: String,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

impl std::error::Error for LoadError {}

impl ControllerDef {
    /// Parse one definition from JSON.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// The CAN bus set with this name.
    pub fn bus(&self, name: &str) -> Option<&CanBus> {
        self.buses
            .iter()
            .find(|b| b.name.eq_ignore_ascii_case(name))
    }

    /// The table with this name.
    pub fn table(&self, name: &str) -> Option<&TableDef> {
        self.tables.iter().find(|t| t.name == name)
    }

    /// True if a reported part number, software number or gearbox code matches.
    pub fn matches(&self, value: &str) -> bool {
        self.identify.matches_part(value)
            || self.identify.matches_sw(value)
            || self.identify.matches_code(value)
    }
}

/// Is this a scaffolding file a directory scan skips (`TEMPLATE.json`, `_*`, `.*`)?
pub fn is_scaffolding_file(name: &str) -> bool {
    opentune_core::ecudef::is_scaffolding_file(name)
}

/// Load every `*.json` definition in `dir` (scaffolding skipped), sorted by id.
/// Fails on the first file that does not parse.
pub fn load_dir(dir: &Path) -> Result<Vec<(PathBuf, ControllerDef)>, LoadError> {
    let rd = std::fs::read_dir(dir).map_err(|e| LoadError {
        path: dir.to_path_buf(),
        message: e.to_string(),
    })?;
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if is_scaffolding_file(name) || path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|e| LoadError {
            path: path.clone(),
            message: e.to_string(),
        })?;
        let def = ControllerDef::from_json(&text).map_err(|e| LoadError {
            path: path.clone(),
            message: e.to_string(),
        })?;
        out.push((path, def));
    }
    out.sort_by(|a, b| a.1.id.cmp(&b.1.id));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identify_prefix_substring_and_glob() {
        let id = Identify {
            part_numbers: vec!["0AY907554C".into(), "0CQ 907 554 *".into()],
            sw_numbers: vec![],
            gearbox_codes: vec!["722.6".into()],
        };
        assert!(id.matches_part("0ay 907 554 c"));
        assert!(id.matches_part("0CQ907554B"));
        assert!(!id.matches_part("0AY907554F"));
        assert!(id.matches_code("722.6"));
        assert!(key_matches("A0??5450*32", "A0345450632"));
        assert!(!key_matches("A0??5450*32", "A034545063"));
        assert!(!key_matches("", "x"));
    }
}
