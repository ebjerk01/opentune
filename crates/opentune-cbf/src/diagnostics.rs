//! Aggregated **diagnostic description** view over a parsed [`CbfFile`].
//!
//! [`parse_bytes`](crate::parse_bytes) returns the faithful on-disk model
//! ([`CbfFile`] → [`Ecu`] → pools). That model mirrors the container layout; it
//! is not the shape most OpenTune callers want. This module normalises one ECU
//! into a flat, self-describing [`CbfDiagnostics`] record:
//!
//! * [`CbfEcuIdentity`] — who the ECU is, and which wire protocols its variants
//!   speak (derived from the variant match patterns).
//! * [`CbfDtc`] — the fault-code catalogue (internal code → human text).
//! * [`CbfService`] — the diagnostic services, each **classified** into a
//!   [`ServiceKind`] so measuring reads and actuation/output controls can be
//!   told apart without the caller re-deriving Mercedes naming conventions.
//! * [`CbfVariant`] — the per-variant subset of DTCs/services plus match
//!   patterns.
//!
//! ## Plaintext vs opaque — be honest
//!
//! Everything surfaced here comes from the CBF string pools and record
//! metadata, which are **plaintext** in the files shipped with DAS/Xentry. What
//! this view deliberately does **not** contain, and what callers must not assume
//! is present, is recorded in [`CbfDiagnostics::warnings`] rather than silently
//! omitted:
//!
//! * Service **request/response parameter layouts** — the byte-level request
//!   templates, response scaling, units and I/O limits (CAESAR "presentations"
//!   and environment/freeze-frame contexts). These exist in the container but
//!   this parser does not reconstruct them, so a [`CbfService`] tells you *that*
//!   a measuring/actuation service exists and what it is called, not how to
//!   encode its bytes.
//! * The [`CbfDtc::code`] is the Mercedes-internal fault **qualifier** (e.g.
//!   `"01F0EC"`), not a decoded SAE J2012 `P/C/B/U` code.
//!
//! Classification of a service into [`ServiceKind`] uses the authoritative
//! [`ServiceType`] discriminant when the file sets one, and falls back to the
//! service qualifier's Mercedes prefix (`DT_`, `IOC_`, `ACT_`, …) only when the
//! discriminant is [`ServiceType::Unknown`] (common in older KWP files). The
//! basis is recorded per service in [`CbfService::classified_by`] so a caller
//! never has to trust a heuristic blindly.

use serde::{Deserialize, Serialize};

use crate::model::{CbfFile, DiagService, Dtc, Ecu, EcuVariant, Protocol, ServiceType};

/// A normalised diagnostic description for a single ECU.
///
/// Build one per ECU with [`ecu_diagnostics`], or all of a file's ECUs at once
/// with [`diagnostics`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CbfDiagnostics {
    /// Who this ECU is and which protocols it speaks.
    pub ecu: CbfEcuIdentity,
    /// Concrete software variants and the DTCs/services each exposes.
    pub variants: Vec<CbfVariant>,
    /// The ECU-global fault-code catalogue.
    pub dtcs: Vec<CbfDtc>,
    /// The ECU-global diagnostic services, each classified by [`ServiceKind`].
    pub services: Vec<CbfService>,
    /// Honest notes about what was missing, heuristic or not decoded. Empty when
    /// the file was fully self-describing for everything this view reports.
    pub warnings: Vec<String>,
}

/// Identity of an ECU: its names and the protocols its variants speak.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CbfEcuIdentity {
    /// Stable internal identifier, e.g. `"DCMP01T"`.
    pub qualifier: String,
    /// Human-readable name from the string pool, if present.
    pub name: Option<String>,
    /// Longer description, if present.
    pub description: Option<String>,
    /// Distinct diagnostic protocols implied by the variants' match patterns
    /// (de-duplicated; [`Protocol::Unknown`] is omitted when a real protocol is
    /// also present). Empty when no pattern exposed a protocol.
    pub protocols: Vec<Protocol>,
}

/// A single fault-code definition, flattened for a DTC database.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CbfDtc {
    /// Mercedes-internal fault qualifier / code (not an SAE J2012 code).
    pub code: String,
    /// Human-readable fault text, if the string pool held one.
    pub text: Option<String>,
    /// Optional cross-reference / summary string.
    pub reference: Option<String>,
}

/// What a diagnostic service fundamentally does.
///
/// This is the OpenTune-facing categorisation; it is coarser and more stable
/// than the raw CAESAR [`ServiceType`] discriminant, and in particular it tells
/// **measuring** reads apart from **actuation** output controls, which is the
/// distinction a tuning/diagnostic tool cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ServiceKind {
    /// Reads a measurement / live value or stored data (`DT_`, `MW_`;
    /// [`ServiceType::Data`] / [`ServiceType::StoredData`]).
    Measurement,
    /// Drives an output / forces an actuator (`IOC_`, `ACT_`;
    /// [`ServiceType::IoControl`]).
    Actuation,
    /// Starts/stops/monitors a routine or self-test (`RT_`;
    /// [`ServiceType::Routine`]).
    Routine,
    /// Reads or writes variant coding / adjustment data (`ADJ_`, `WVC_`,
    /// `RVC_`).
    Coding,
    /// Starts or changes a diagnostic session (`SES_`; [`ServiceType::Session`]).
    Session,
    /// Flash/data download or upload (`DL_`; [`ServiceType::Download`]).
    Download,
    /// Security access / seed-key exchange (`SEC_`).
    Security,
    /// A higher-level diagnostic function or job (`FN_`, `DJ_`;
    /// [`ServiceType::DiagnosticFunction`] / [`ServiceType::DiagnosticJob`]).
    Function,
    /// Could not be categorised from the type discriminant or the qualifier.
    #[default]
    Other,
}

/// How a service's [`ServiceKind`] was decided — so callers can weigh how much
/// to trust it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Classification {
    /// Taken from the authoritative CAESAR [`ServiceType`] discriminant.
    TypeField,
    /// The discriminant was [`ServiceType::Unknown`]; the kind was inferred from
    /// the service qualifier's Mercedes prefix. A heuristic, not authoritative.
    NamePrefix,
    /// Neither the discriminant nor the qualifier identified the service.
    #[default]
    Unclassified,
}

/// A diagnostic service, classified into a [`ServiceKind`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CbfService {
    /// Internal service qualifier, e.g. `"DT_Raildruck"`.
    pub qualifier: String,
    /// Human-readable name, if present.
    pub name: Option<String>,
    /// Description, if present.
    pub description: Option<String>,
    /// Raw CAESAR service category from the record.
    pub service_type: ServiceType,
    /// OpenTune-facing category (measuring vs actuation vs …).
    pub kind: ServiceKind,
    /// How [`kind`](Self::kind) was decided.
    pub classified_by: Classification,
}

/// A concrete ECU software variant and the subset of DTCs/services it exposes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CbfVariant {
    /// Internal variant qualifier.
    pub qualifier: String,
    /// Human-readable name, if present.
    pub name: Option<String>,
    /// Description, if present.
    pub description: Option<String>,
    /// Protocols implied by this variant's match patterns (de-duplicated).
    pub protocols: Vec<Protocol>,
    /// Qualifiers of the global DTCs this variant exposes.
    pub dtc_codes: Vec<String>,
    /// Qualifiers of the global services this variant exposes.
    pub service_qualifiers: Vec<String>,
}

impl ServiceKind {
    /// Classify a service from its CAESAR [`ServiceType`] and its qualifier.
    ///
    /// The discriminant wins when it is anything but [`ServiceType::Unknown`];
    /// otherwise the Mercedes qualifier prefix is consulted. The returned
    /// [`Classification`] records which basis was used.
    pub fn classify(service_type: ServiceType, qualifier: &str) -> (ServiceKind, Classification) {
        if let Some(kind) = Self::from_service_type(service_type) {
            return (kind, Classification::TypeField);
        }
        if let Some(kind) = Self::from_qualifier_prefix(qualifier) {
            return (kind, Classification::NamePrefix);
        }
        (ServiceKind::Other, Classification::Unclassified)
    }

    /// Map the authoritative discriminant, or `None` for
    /// [`ServiceType::Unknown`].
    fn from_service_type(t: ServiceType) -> Option<ServiceKind> {
        Some(match t {
            ServiceType::Data | ServiceType::StoredData => ServiceKind::Measurement,
            ServiceType::IoControl => ServiceKind::Actuation,
            ServiceType::Routine => ServiceKind::Routine,
            ServiceType::Session => ServiceKind::Session,
            ServiceType::Download => ServiceKind::Download,
            ServiceType::DiagnosticFunction | ServiceType::DiagnosticJob => ServiceKind::Function,
            ServiceType::Unknown => return None,
        })
    }

    /// Infer a kind from the Mercedes service-qualifier prefix (`DT_`, `IOC_`,
    /// …). Case-insensitive on the prefix token; `None` when unrecognised.
    fn from_qualifier_prefix(qualifier: &str) -> Option<ServiceKind> {
        let prefix = qualifier.split('_').next().unwrap_or(qualifier);
        let kind = match prefix.to_ascii_uppercase().as_str() {
            "DT" | "MW" => ServiceKind::Measurement,
            "IOC" | "ACT" => ServiceKind::Actuation,
            "RT" => ServiceKind::Routine,
            "ADJ" | "WVC" | "RVC" => ServiceKind::Coding,
            "SES" => ServiceKind::Session,
            "DL" => ServiceKind::Download,
            "SEC" => ServiceKind::Security,
            "FN" | "DJ" => ServiceKind::Function,
            _ => return None,
        };
        Some(kind)
    }
}

/// Collapse a sequence of protocols to the distinct, meaningful ones.
///
/// Preserves first-seen order, drops duplicates, and drops
/// [`Protocol::Unknown`] when any concrete protocol is present.
fn distinct_protocols<I: IntoIterator<Item = Protocol>>(it: I) -> Vec<Protocol> {
    let mut out: Vec<Protocol> = Vec::new();
    for p in it {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    if out.iter().any(|p| *p != Protocol::Unknown) {
        out.retain(|p| *p != Protocol::Unknown);
    }
    out
}

fn convert_dtc(d: &Dtc) -> CbfDtc {
    CbfDtc {
        code: d.qualifier.clone(),
        text: d.description.clone(),
        reference: d.reference.clone(),
    }
}

fn convert_service(s: &DiagService) -> CbfService {
    let (kind, classified_by) = ServiceKind::classify(s.service_type, &s.qualifier);
    CbfService {
        qualifier: s.qualifier.clone(),
        name: s.name.clone(),
        description: s.description.clone(),
        service_type: s.service_type,
        kind,
        classified_by,
    }
}

fn convert_variant(v: &EcuVariant) -> CbfVariant {
    CbfVariant {
        qualifier: v.qualifier.clone(),
        name: v.name.clone(),
        description: v.description.clone(),
        protocols: distinct_protocols(v.patterns.iter().map(|p| p.protocol)),
        dtc_codes: v.dtc_qualifiers.clone(),
        service_qualifiers: v.service_qualifiers.clone(),
    }
}

/// Build the normalised diagnostic view for one [`Ecu`].
///
/// Never fails: missing plaintext becomes `None`/empty plus a note in
/// [`CbfDiagnostics::warnings`].
pub fn ecu_diagnostics(ecu: &Ecu) -> CbfDiagnostics {
    let dtcs: Vec<CbfDtc> = ecu.dtcs.iter().map(convert_dtc).collect();
    let services: Vec<CbfService> = ecu.services.iter().map(convert_service).collect();
    let variants: Vec<CbfVariant> = ecu.variants.iter().map(convert_variant).collect();

    let protocols = distinct_protocols(
        ecu.variants
            .iter()
            .flat_map(|v| v.patterns.iter().map(|p| p.protocol)),
    );

    let mut warnings = Vec::new();

    if ecu.name.is_none() {
        warnings.push(format!(
            "ECU '{}' has no plaintext name in the string pool",
            ecu.qualifier
        ));
    }

    let dtcs_without_text = dtcs.iter().filter(|d| d.text.is_none()).count();
    if dtcs_without_text > 0 {
        warnings.push(format!(
            "{dtcs_without_text} of {} DTC(s) have no plaintext description",
            dtcs.len()
        ));
    }

    let services_without_name = services.iter().filter(|s| s.name.is_none()).count();
    if services_without_name > 0 {
        warnings.push(format!(
            "{services_without_name} of {} service(s) have no plaintext name",
            services.len()
        ));
    }

    let heuristic = services
        .iter()
        .filter(|s| s.classified_by == Classification::NamePrefix)
        .count();
    let unclassified = services
        .iter()
        .filter(|s| s.classified_by == Classification::Unclassified)
        .count();
    if heuristic > 0 {
        warnings.push(format!(
            "{heuristic} service(s) were classified by qualifier-prefix heuristic \
             (the file left the CAESAR service-type field unset)"
        ));
    }
    if unclassified > 0 {
        warnings.push(format!(
            "{unclassified} service(s) could not be categorised into a ServiceKind"
        ));
    }

    if protocols.is_empty() && !variants.is_empty() {
        warnings.push(
            "no diagnostic protocol could be determined from the variant match patterns"
                .to_string(),
        );
    }

    if !services.is_empty() {
        warnings.push(
            "service request/response parameter layouts (byte templates, scaling, units, \
             I/O limits) are present in the container but not decoded by this parser"
                .to_string(),
        );
    }

    CbfDiagnostics {
        ecu: CbfEcuIdentity {
            qualifier: ecu.qualifier.clone(),
            name: ecu.name.clone(),
            description: ecu.description.clone(),
            protocols,
        },
        variants,
        dtcs,
        services,
        warnings,
    }
}

/// Build the normalised diagnostic view for every ECU in a parsed file.
///
/// Mercedes CBFs usually hold exactly one ECU, so this often returns a single
/// element, but a multi-ECU container yields one [`CbfDiagnostics`] per ECU.
pub fn diagnostics(cbf: &CbfFile) -> Vec<CbfDiagnostics> {
    cbf.ecus.iter().map(ecu_diagnostics).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DiagService, Dtc, Ecu, EcuVariant, VariantPattern};

    #[test]
    fn classify_prefers_type_field() {
        let (kind, by) = ServiceKind::classify(ServiceType::IoControl, "ACT_whatever");
        assert_eq!(kind, ServiceKind::Actuation);
        assert_eq!(by, Classification::TypeField);

        let (kind, by) = ServiceKind::classify(ServiceType::Data, "DT_Raildruck");
        assert_eq!(kind, ServiceKind::Measurement);
        assert_eq!(by, Classification::TypeField);
    }

    #[test]
    fn classify_falls_back_to_prefix_when_type_unknown() {
        // KWP files often leave the discriminant unset; prefix carries meaning.
        let cases = [
            ("ACT_Abgasklappe", ServiceKind::Actuation),
            ("DT_Luftmasse", ServiceKind::Measurement),
            ("MW_Drehzahl", ServiceKind::Measurement),
            ("RT_Kompression", ServiceKind::Routine),
            ("ADJ_Leerlauf", ServiceKind::Coding),
            ("WVC_Codierung", ServiceKind::Coding),
            ("SES_Default", ServiceKind::Session),
            ("DL_Flash", ServiceKind::Download),
            ("SEC_SeedKey", ServiceKind::Security),
            ("FN_SelfTest", ServiceKind::Function),
            ("DJ_Job", ServiceKind::Function),
        ];
        for (qual, want) in cases {
            let (kind, by) = ServiceKind::classify(ServiceType::Unknown, qual);
            assert_eq!(kind, want, "qualifier {qual}");
            assert_eq!(by, Classification::NamePrefix, "qualifier {qual}");
        }
    }

    #[test]
    fn classify_prefix_is_case_insensitive() {
        let (kind, by) = ServiceKind::classify(ServiceType::Unknown, "act_lowercase");
        assert_eq!(kind, ServiceKind::Actuation);
        assert_eq!(by, Classification::NamePrefix);
    }

    #[test]
    fn classify_unknown_type_and_prefix_is_unclassified() {
        let (kind, by) = ServiceKind::classify(ServiceType::Unknown, "ZZZ_mystery");
        assert_eq!(kind, ServiceKind::Other);
        assert_eq!(by, Classification::Unclassified);
    }

    #[test]
    fn distinct_protocols_drops_unknown_and_dedups() {
        assert_eq!(
            distinct_protocols([Protocol::Uds, Protocol::Uds, Protocol::Unknown]),
            vec![Protocol::Uds]
        );
        // Unknown survives only when nothing concrete is present.
        assert_eq!(
            distinct_protocols([Protocol::Unknown, Protocol::Unknown]),
            vec![Protocol::Unknown]
        );
        assert_eq!(distinct_protocols([]), Vec::<Protocol>::new());
    }

    fn sample_ecu() -> Ecu {
        Ecu {
            qualifier: "CR3".into(),
            name: Some("Common-Rail 3.x".into()),
            description: None,
            dtcs: vec![
                Dtc {
                    qualifier: "0100".into(),
                    description: Some("Luftmassensensor".into()),
                    reference: None,
                    pool_index: 0,
                },
                Dtc {
                    qualifier: "0105".into(),
                    description: None, // no plaintext text -> should warn
                    reference: None,
                    pool_index: 1,
                },
            ],
            services: vec![
                DiagService {
                    qualifier: "DT_Raildruck".into(),
                    name: Some("Raildruck".into()),
                    description: None,
                    service_type: ServiceType::Data,
                    pool_index: 0,
                },
                DiagService {
                    qualifier: "ACT_Abgasklappe".into(),
                    name: Some("Abgasklappe".into()),
                    description: None,
                    service_type: ServiceType::Unknown, // KWP: classify by prefix
                    pool_index: 1,
                },
                DiagService {
                    qualifier: "ZZZ_mystery".into(),
                    name: None, // no plaintext name -> should warn
                    description: None,
                    service_type: ServiceType::Unknown,
                    pool_index: 2,
                },
            ],
            variants: vec![EcuVariant {
                qualifier: "CR3_Diag_05h".into(),
                name: Some("C3M bis C4M".into()),
                description: None,
                patterns: vec![
                    VariantPattern {
                        vendor: "Bosch".into(),
                        vendor_id: 5,
                        protocol: Protocol::Kwp,
                    },
                    VariantPattern {
                        vendor: "Bosch".into(),
                        vendor_id: 5,
                        protocol: Protocol::Kwp,
                    },
                ],
                dtc_qualifiers: vec!["0100".into()],
                service_qualifiers: vec!["DT_Raildruck".into(), "ACT_Abgasklappe".into()],
            }],
        }
    }

    #[test]
    fn ecu_diagnostics_normalises_and_classifies() {
        let diag = ecu_diagnostics(&sample_ecu());

        assert_eq!(diag.ecu.qualifier, "CR3");
        assert_eq!(diag.ecu.protocols, vec![Protocol::Kwp]);

        assert_eq!(diag.dtcs.len(), 2);
        assert_eq!(diag.dtcs[0].code, "0100");
        assert_eq!(diag.dtcs[0].text.as_deref(), Some("Luftmassensensor"));

        let act = diag
            .services
            .iter()
            .find(|s| s.qualifier == "ACT_Abgasklappe")
            .unwrap();
        assert_eq!(act.kind, ServiceKind::Actuation);
        assert_eq!(act.classified_by, Classification::NamePrefix);

        let dt = diag
            .services
            .iter()
            .find(|s| s.qualifier == "DT_Raildruck")
            .unwrap();
        assert_eq!(dt.kind, ServiceKind::Measurement);
        assert_eq!(dt.classified_by, Classification::TypeField);

        assert_eq!(diag.variants.len(), 1);
        assert_eq!(diag.variants[0].protocols, vec![Protocol::Kwp]);
        assert_eq!(diag.variants[0].dtc_codes, vec!["0100".to_string()]);
    }

    #[test]
    fn ecu_diagnostics_emits_honest_warnings() {
        let diag = ecu_diagnostics(&sample_ecu());
        let joined = diag.warnings.join("\n");
        // A DTC without text, a service without a name, a heuristic
        // classification, an unclassified service, and the standing note about
        // undecoded parameter layouts should all be reported.
        assert!(
            joined.contains("DTC(s) have no plaintext description"),
            "{joined}"
        );
        assert!(
            joined.contains("service(s) have no plaintext name"),
            "{joined}"
        );
        assert!(joined.contains("qualifier-prefix heuristic"), "{joined}");
        assert!(joined.contains("could not be categorised"), "{joined}");
        assert!(joined.contains("not decoded by this parser"), "{joined}");
    }

    #[test]
    fn ecu_with_name_and_full_text_has_no_identity_warnings() {
        let mut ecu = sample_ecu();
        // Make everything fully self-describing.
        ecu.dtcs[1].description = Some("Ladedrucksensor".into());
        ecu.services[1].service_type = ServiceType::IoControl;
        ecu.services.pop(); // remove the unnamed/unclassified mystery service
        let diag = ecu_diagnostics(&ecu);
        let joined = diag.warnings.join("\n");
        assert!(
            !joined.contains("no plaintext name in the string pool"),
            "{joined}"
        );
        assert!(!joined.contains("no plaintext description"), "{joined}");
        assert!(!joined.contains("qualifier-prefix heuristic"), "{joined}");
        assert!(!joined.contains("could not be categorised"), "{joined}");
        // The standing parameter-layout note is still present (it always is when
        // services exist) — that is honest, not a defect.
        assert!(joined.contains("not decoded by this parser"), "{joined}");
    }

    #[test]
    fn diagnostics_covers_every_ecu() {
        let cbf = CbfFile {
            cbf_version: "v".into(),
            source_xml: String::new(),
            ecus: vec![sample_ecu(), sample_ecu()],
        };
        assert_eq!(diagnostics(&cbf).len(), 2);
    }
}
