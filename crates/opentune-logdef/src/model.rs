//! The live-data parameter model produced by parsing a RomRaider-style
//! `logger.xml`.
//!
//! Everything here is plain data with `serde` derives so a parsed definition
//! can be serialized to JSON (or any other serde format) and fed to OpenTune's
//! future logging engine. Nothing in this module performs I/O, transport or
//! value decoding — it only *describes* how a parameter would be read and
//! scaled.

use opentune_core::ecudef::{DataType, Endian};
use serde::{Deserialize, Serialize};

/// Which concrete on-the-wire definition format a [`LogDef`] was parsed from.
///
/// Both formats are "RomRaider-style" live-data definitions and map onto the
/// same model; the variant is kept so callers can report provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFormat {
    /// The canonical RomRaider logger format: `<logger><protocols><protocol>
    /// <parameters>/<ecuparams>…</protocol></protocols></logger>` (the format
    /// described by `romraider-logger.dtd`, also emitted by FastECU).
    RomRaiderLogger,
    /// The EcuFlash / FastECU "logprotocols" format: `<ecus><logprotocols>
    /// <logprotocol><ecu><parameter offset=… expr=… metric=…/></ecu>…`.
    EcuFlashLogProtocols,
}

/// Transport/service kind a parameter is read with.
///
/// This classifies *how* the addressing field should be interpreted; it does
/// not model the transport itself (that is the future logging engine's job).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProtocolKind {
    /// OBD-II service `0x01` (show current data) — the addressing is a single
    /// PID byte.
    Obd2Mode01,
    /// UDS service `0x22` (ReadDataByIdentifier) — the addressing is a 2-byte
    /// Data Identifier (DID).
    UdsMode22,
    /// Direct memory read ("mode 23" / RomRaider `<address>` read) — the
    /// addressing is a raw ECU memory address + length.
    EcuMode23,
    /// Subaru Select Monitor (SSM) — Subaru's proprietary read-addresses
    /// protocol; the addressing is a raw ECU memory address + length.
    SubaruSsm,
    /// Any other / unrecognised protocol id. The original id is preserved so
    /// nothing is lost; addressing is treated as a raw memory address.
    Other {
        /// The protocol id as written in the definition (e.g. `MUT_DMA`).
        id: String,
    },
}

/// How a single parameter is addressed on its protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "addressing", rename_all = "snake_case")]
pub enum Addressing {
    /// OBD-II service-01 PID (one byte, e.g. `0x0C` = engine RPM).
    ObdPid {
        /// The PID byte.
        pid: u8,
    },
    /// UDS service-22 Data Identifier (two bytes).
    UdsDid {
        /// The 16-bit DID.
        did: u16,
    },
    /// A raw ECU memory address (mode 23 / SSM read).
    Memory {
        /// Start address.
        address: u32,
        /// Number of bytes read, 1 for a bit/flag parameter.
        length: u8,
        /// Optional bit index (0..=31) for a single-bit flag parameter.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        bit: Option<u8>,
    },
    /// Addressing could not be determined (missing/unparsable address). The
    /// parameter is still kept so the omission is visible, with a warning on
    /// the owning [`LogDef`].
    Unknown,
}

/// Classification of a conversion equation after fitting it to a linear scale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "fit", rename_all = "snake_case")]
pub enum ConversionFit {
    /// The equation is affine: `physical = raw * factor + offset`.
    Linear {
        /// Multiplier applied to the raw value.
        factor: f64,
        /// Added after the multiply.
        offset: f64,
    },
    /// The equation parsed but is not linear in the raw value (e.g. it calls
    /// `GetLogParam(...)`, uses `x*x`, `x^2`, …). Flagged rather than
    /// approximated: a correct value needs runtime expression evaluation, which
    /// is out of scope for this def parser.
    NonLinear {
        /// Why it could not be reduced to a linear scale.
        reason: String,
    },
}

/// One raw→physical conversion for a parameter (a `<conversion>` element, or a
/// Format-B parameter's inline `expr`/`metric`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Conversion {
    /// Physical units (`<conversion units>` / Format-B `metric`).
    pub units: String,
    /// The conversion expression, normalized so the raw value is the single
    /// uppercase variable `X` (Format-B `[value]` is rewritten to `X`).
    pub expr: String,
    /// The display format string (`<conversion format>`), if given.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub format: Option<String>,
    /// Result of fitting [`expr`](Self::expr) to a linear scale.
    pub fit: ConversionFit,
    /// Gauge lower bound (`gauge_min`), if given.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub gauge_min: Option<f64>,
    /// Gauge upper bound (`gauge_max`), if given.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub gauge_max: Option<f64>,
    /// Gauge tick step (`gauge_step`), if given.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub gauge_step: Option<f64>,
}

/// An alternate address for a parameter (Format-B `<alt>`), used when the same
/// logical channel lives at different addresses across ECU variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AltAddress {
    /// The alternate's id.
    pub id: String,
    /// The alternate addressing.
    pub addressing: Addressing,
}

/// A single live-data parameter: how to read it and how to scale it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogParam {
    /// Stable parameter id (`id` attribute).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Long description (may be empty).
    pub description: String,
    /// Physical units of the primary conversion (convenience copy of
    /// `conversions[0].units`; empty if there is no conversion).
    pub units: String,
    /// The protocol/service this parameter is read with.
    pub protocol: ProtocolKind,
    /// How the parameter is addressed on that protocol.
    pub addressing: Addressing,
    /// Stored value type (`storagetype`), if declared.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub storagetype: Option<DataType>,
    /// Byte order of the stored value (defaults to big-endian, as SSM/RomRaider
    /// definitions are Motorola order).
    pub endian: Endian,
    /// The primary conversion (first `<conversion>` / the inline `expr`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub conversion: Option<Conversion>,
    /// All conversions declared for this parameter (a `<conversions>` block can
    /// list several unit alternatives).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub conversions: Vec<Conversion>,
    /// Lower gauge bound of the primary conversion, if any.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub min: Option<f64>,
    /// Upper gauge bound of the primary conversion, if any.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub max: Option<f64>,
    /// Alternate addresses (`<alt>` entries), if any.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub alternates: Vec<AltAddress>,
    /// Parameter ids this parameter's computation depends on
    /// (`<depends>`/`<dependson>` / `GetLogParam("…")` references). Captured,
    /// **not** resolved in v1 — the presence of entries flags that this
    /// parameter cannot be logged standalone.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub depends_on: Vec<String>,
    /// Name of the Format-B `<convert_factor>` group (`type` attribute)
    /// offering alternate unit conversions for this parameter, if referenced.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub convert_group: Option<String>,
    /// Whether the parameter is marked enabled (`enabled` attribute), if given.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub enabled: Option<bool>,
}

/// A group of parameters belonging to one ECU (or to the protocol's standard
/// parameter set when [`id`](Self::id) is `None`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EcuGroup {
    /// ECU calibration id this group's addresses apply to. `None` means the
    /// protocol's standard parameters (RomRaider `<parameters>`), which apply
    /// to every ECU on the protocol.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub id: Option<String>,
    /// ECU display name, if given.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
    /// ECU type/definition id (`type` attribute), if given.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ecu_type: Option<String>,
    /// The parameters defined for this ECU.
    pub params: Vec<LogParam>,
}

/// One protocol's worth of live-data parameters, grouped by ECU.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProtocolGroup {
    /// Protocol id as written (`SSM`, `OBD`, `MUT_DMA`, …).
    pub id: String,
    /// Classified protocol kind.
    pub kind: ProtocolKind,
    /// Serial baud rate (`<protocol baud>`), if declared.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub baud: Option<u32>,
    /// Parameters grouped by ECU.
    pub ecus: Vec<EcuGroup>,
}

/// A named Format-B unit-conversion factor (`<convert_factor>`), e.g. "Convert
/// Celsius to Fahrenheit". Parameters reference these by their `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConvertFactor {
    /// The group key parameters reference (`type` attribute, e.g. `temp`).
    pub kind: String,
    /// Display name.
    pub name: String,
    /// Target metric/units.
    pub metric: String,
    /// The conversion expression, normalized to the uppercase variable `X`.
    pub expr: String,
    /// Result of fitting [`expr`](Self::expr) to a linear scale.
    pub fit: ConversionFit,
}

/// A parsed RomRaider-style logger definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogDef {
    /// Which concrete format this was parsed from.
    pub format: SourceFormat,
    /// `version` attribute of the root element, if present.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub version: Option<String>,
    /// Named unit-conversion factors (Format-B `<ecu_tools><convert_factor>`).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub convert_factors: Vec<ConvertFactor>,
    /// Protocols, each grouped by ECU.
    pub protocols: Vec<ProtocolGroup>,
    /// Non-fatal issues encountered while parsing (unknown storagetype, missing
    /// address, unsupported construct, …). Never silently dropped.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

impl LogDef {
    /// Total number of parameters across every protocol and ECU group.
    pub fn param_count(&self) -> usize {
        self.protocols
            .iter()
            .flat_map(|p| p.ecus.iter())
            .map(|e| e.params.len())
            .sum()
    }

    /// Iterate every parameter together with its owning protocol and ECU group.
    pub fn params(&self) -> impl Iterator<Item = (&ProtocolGroup, &EcuGroup, &LogParam)> {
        self.protocols.iter().flat_map(|p| {
            p.ecus
                .iter()
                .flat_map(move |e| e.params.iter().map(move |param| (p, e, param)))
        })
    }
}
