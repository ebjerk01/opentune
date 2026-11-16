//! Public, serialisable model produced by parsing a CBF file.
//!
//! These types are the parser's output surface. They are intentionally plain
//! data (no binary-reader state) so they can be serialised to JSON and consumed
//! by other OpenTune crates such as `opentune-dtc`.

use serde::{Deserialize, Serialize};

/// A fully parsed CBF container.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CbfFile {
    /// The `CBF-TRANSLATOR-VERSION:...` banner from the stub header.
    pub cbf_version: String,
    /// The source XML/GPD string embedded in the CFF header (often a path or
    /// build tag); empty when absent.
    pub source_xml: String,
    /// Every ECU described by the file (Mercedes CBFs usually hold exactly one).
    pub ecus: Vec<Ecu>,
}

/// One ECU family described by the file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ecu {
    /// Stable internal identifier, e.g. `"DCMP01T"`.
    pub qualifier: String,
    /// Human-readable name (from the language string pool), if present.
    pub name: Option<String>,
    /// Longer description, if present.
    pub description: Option<String>,
    /// The ECU-global DTC pool: every fault code this ECU family defines,
    /// with its description. This is the authoritative code→description source.
    pub dtcs: Vec<Dtc>,
    /// ECU-global diagnostic services (metadata only: name, qualifier, type).
    pub services: Vec<DiagService>,
    /// The concrete ECU variants (software versions) and which DTCs/services
    /// each one exposes.
    pub variants: Vec<EcuVariant>,
}

/// A single diagnostic trouble code definition from the CBF.
///
/// Mercedes CBF DTCs are identified by an internal `qualifier` (a
/// manufacturer-specific fault name, **not** an SAE J2012 `P/C/B/U` code). The
/// human text lives in `description`; `reference` is an optional cross-reference
/// / summary string.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dtc {
    /// Internal fault qualifier / code (Mercedes-specific).
    pub qualifier: String,
    /// Human-readable fault description, if present in the string pool.
    pub description: Option<String>,
    /// Optional reference / summary text.
    pub reference: Option<String>,
    /// Index of this DTC in the ECU-global DTC pool (used to link variants).
    pub pool_index: usize,
}

/// The underlying diagnostic protocol a service / variant speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Protocol {
    /// Keyword Protocol 2000 (ISO 14230).
    Kwp,
    /// Unified Diagnostic Services (ISO 14229).
    Uds,
    /// Unknown / not determinable.
    #[default]
    Unknown,
}

/// CAESAR service category, from the record's `data_class_service_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ServiceType {
    Data,
    Download,
    DiagnosticFunction,
    DiagnosticJob,
    Session,
    StoredData,
    Routine,
    IoControl,
    #[default]
    Unknown,
}

impl ServiceType {
    /// Map the raw CAESAR service-type discriminant to a [`ServiceType`].
    pub fn from_raw(x: u16) -> ServiceType {
        match x {
            5 => ServiceType::Data,
            7 => ServiceType::Download,
            10 => ServiceType::DiagnosticFunction,
            19 => ServiceType::DiagnosticJob,
            21 => ServiceType::Session,
            22 => ServiceType::StoredData,
            23 => ServiceType::Routine,
            24 => ServiceType::IoControl,
            _ => ServiceType::Unknown,
        }
    }
}

/// A diagnostic service definition (metadata level).
///
/// The full CAESAR service record also carries request/response parameter
/// layouts (presentations); this parser extracts the identifying metadata only,
/// which is what callers need to enumerate an ECU's services.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiagService {
    /// Internal service qualifier.
    pub qualifier: String,
    /// Human-readable name, if present.
    pub name: Option<String>,
    /// Description, if present.
    pub description: Option<String>,
    /// Service category.
    pub service_type: ServiceType,
    /// Index of this service in the ECU-global service pool.
    pub pool_index: usize,
}

/// A hardware/software pattern that identifies a concrete ECU build.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VariantPattern {
    /// ECU vendor / supplier name.
    pub vendor: String,
    /// Vendor identifier (KWP or UDS, depending on protocol).
    pub vendor_id: u32,
    /// Protocol this pattern implies.
    pub protocol: Protocol,
}

/// A concrete ECU variant (a specific software version of the ECU family).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EcuVariant {
    /// Internal variant qualifier.
    pub qualifier: String,
    /// Human-readable name, if present.
    pub name: Option<String>,
    /// Description, if present.
    pub description: Option<String>,
    /// Matching patterns that identify this variant on a real vehicle.
    pub patterns: Vec<VariantPattern>,
    /// Qualifiers of the DTCs this variant exposes (subset of the ECU-global
    /// pool).
    pub dtc_qualifiers: Vec<String>,
    /// Qualifiers of the diagnostic services this variant exposes.
    pub service_qualifiers: Vec<String>,
}

/// A flattened DTC record suitable for feeding into a DTC database such as
/// `opentune-dtc`.
///
/// One entry per DTC definition, carrying the owning ECU qualifier so that
/// manufacturer codes from multiple files can be merged without collision.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DtcEntry {
    /// The DTC code (the CBF `qualifier`; Mercedes-specific, not an SAE code).
    pub code: String,
    /// Human-readable description (empty string if the file had none).
    pub description: String,
    /// Reference / summary text (empty string if none).
    pub summary: String,
    /// Qualifier of the ECU that defines this code.
    pub ecu: String,
}
