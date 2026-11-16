//! The central flash safety policy.
//!
//! [`SafetyConfig`](crate::SafetyConfig) is a deliberately flexible set of
//! switches: every one of its guarantees (voltage, backup, verify) can be turned
//! off, and — critically — its voltage gate only *fails closed* for interfaces
//! that advertise [`Capabilities::battery_voltage`](opentune_core::Capabilities).
//! An interface that cannot read voltage (several real drivers: SocketCAN, SLCAN,
//! gs_usb, raw J2534, the K-line bridge) is silently let through. That is the
//! right amount of flexibility for the offline mock and for bench rigs the
//! operator vouches for, and the wrong amount for a real write to a vehicle.
//!
//! This module adds one thing on top: a single [`FlashSafetyPolicy`] that a
//! *production* write must satisfy, enforced centrally by the flash orchestrator
//! so no driver and no caller can weaken it piecemeal. A production write
//! **fails closed**: anything it cannot positively verify is a typed
//! [`PolicyViolation`], never a silent pass.
//!
//! # What the policy can and cannot guarantee
//!
//! Enforced here, against the live interface and the supplied [`FlashTarget`]:
//!
//! - a battery voltage at or above the minimum, with *unreadable* voltage
//!   treated as a failure (not a pass) under [`VoltagePolicy::Required`];
//! - a verified pre-flash backup of the original exists before any write;
//! - the exact target region lies inside a region the target declares writable;
//! - the connected ECU's reported identity matches the payload's intended
//!   target (controller + software id);
//! - a checksum attestation is present **and bound to the exact payload bytes**
//!   being written (so an attestation for a different image cannot be reused);
//! - post-write read-back verification runs and matches.
//!
//! Deliberately **not** guaranteed here (documented limitations):
//!
//! - OpenTune does not *recompute* OEM checksums inside the flash layer; the
//!   checksum attestation is produced by the caller (`opentune-checksum`) and
//!   this layer only binds and enforces it. A wrong-but-self-consistent
//!   attestation from a buggy caller is out of scope.
//! - Electrical validity beyond a single voltage sample (ripple, a failing
//!   charger, contact resistance) is not observable through this abstraction.
//! - Writable-region and identity facts are only as good as the [`FlashTarget`]
//!   the caller builds from its ECU definition. The policy enforces the facts it
//!   is given; it cannot invent a memory map the definition does not describe.
//! - Correct programming of a particular ECU's erase/program sequence is modelled
//!   by [`ProgrammingProcedure`] for *recovery* decisions (see
//!   [`crate::resume`]); it is not a substitute for a per-ECU validated routine.

use crate::hash::sha256_hex;
use opentune_core::{EcuId, Error, MemoryRegion};
use serde::{Deserialize, Serialize};

/// How a write path is classified. Production is fail-closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// A real write to a controller. Every acceptance criterion is mandatory and
    /// anything unverifiable is a [`PolicyViolation`].
    Production,
    /// A deliberately flexible mock/test workflow. Clearly named so it can never
    /// be reached by accident from a production path.
    MockTesting,
    /// Back-compat shim for the legacy [`SafetyConfig`](crate::SafetyConfig)
    /// callers. Reproduces the historical behaviour exactly.
    Compat,
}

/// How to treat battery voltage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoltagePolicy {
    /// Voltage must be readable and at or above the minimum. An interface that
    /// cannot report voltage **fails closed**. This is the production default.
    Required,
    /// Voltage is checked when the interface advertises the capability; a driver
    /// with no voltage capability is allowed through. This is the historical
    /// [`SafetyConfig`](crate::SafetyConfig) behaviour — appropriate for mock and
    /// operator-vouched bench setups, not for an unattended production write.
    RequiredWhenAvailable,
    /// No voltage gate at all. Mock/testing only.
    Disabled,
}

/// The intended target of a flash: the identity the connected ECU must match,
/// the regions it is legal to write, the programming procedure, and the checksum
/// attestation for the payload. The caller builds this from its ECU definition.
#[derive(Debug, Clone, PartialEq)]
pub struct FlashTarget {
    /// Stable id of the ECU definition this payload was built for. Recorded in
    /// the recovery checkpoint and the transcript for provenance.
    pub ecu_def_id: String,
    /// Identity the connected ECU must report.
    pub identity: TargetIdentity,
    /// Regions that may be written. The flash region must lie fully inside one.
    pub writable_regions: Vec<MemoryRegion>,
    /// Erase/program procedure, used for recovery (resume) decisions.
    pub procedure: ProgrammingProcedure,
    /// Checksum attestation for the exact payload being written.
    pub checksum: ChecksumAttestation,
}

/// Identity the connected ECU must match. Matching mirrors the ECU-definition
/// identify logic (case/space-insensitive prefix or substring), so a family
/// prefix covers many exact part/software numbers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TargetIdentity {
    /// Hardware / spare-part numbers or prefixes the ECU must report.
    #[serde(default)]
    pub part_numbers: Vec<String>,
    /// Software / calibration numbers or prefixes the ECU must report.
    #[serde(default)]
    pub sw_numbers: Vec<String>,
    /// Optional human label for messages/logs (e.g. the ECU definition name).
    #[serde(default)]
    pub label: Option<String>,
}

fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase()
}

fn key_hit(value: &str, keys: &[String]) -> bool {
    let v = norm(value);
    keys.iter().any(|k| {
        let k = norm(k);
        !k.is_empty() && (v.starts_with(&k) || v.contains(&k))
    })
}

impl TargetIdentity {
    /// True if this identity asserts no keys at all — nothing to match against.
    #[must_use]
    pub fn is_unspecified(&self) -> bool {
        self.part_numbers.iter().all(|s| norm(s).is_empty())
            && self.sw_numbers.iter().all(|s| norm(s).is_empty())
    }

    /// True if `connected` matches any declared part or software key.
    #[must_use]
    pub fn matches(&self, connected: &EcuId) -> bool {
        if let Some(hw) = &connected.hw_number {
            if key_hit(hw, &self.part_numbers) {
                return true;
            }
        }
        if let Some(sw) = &connected.sw_number {
            if key_hit(sw, &self.sw_numbers) {
                return true;
            }
        }
        false
    }

    /// A compact, stable fingerprint of the *connected* ECU, for binding into a
    /// recovery checkpoint and comparing on resume.
    #[must_use]
    pub fn fingerprint_of(connected: &EcuId) -> String {
        format!(
            "hw={}|sw={}|vin={}|name={}",
            connected.hw_number.as_deref().unwrap_or(""),
            connected.sw_number.as_deref().unwrap_or(""),
            connected.vin.as_deref().unwrap_or(""),
            connected.ecu_name.as_deref().unwrap_or(""),
        )
    }
}

/// The erase/program procedure of the target, used to decide what a resume may
/// safely skip. Generic chunk resumption is *not* proof of recovery on an ECU
/// that erases: see [`crate::resume`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgrammingProcedure {
    /// Short name for logs, e.g. `"uds-34-36-37"`.
    pub name: String,
    /// Erase block size in bytes. `0` means unknown / no modelled erase.
    pub erase_granularity: u32,
    /// If the procedure erases the whole region before programming, a mid-region
    /// resume cannot trust prior progress by index alone; it must re-read the
    /// device. See [`ProgrammingProcedure::requires_readback_on_resume`].
    pub whole_region_erase: bool,
}

impl Default for ProgrammingProcedure {
    fn default() -> Self {
        // The safe default: assume an erase happens, so recovery always confirms
        // what is actually on the device before skipping anything.
        Self {
            name: "generic".into(),
            erase_granularity: 0,
            whole_region_erase: true,
        }
    }
}

impl ProgrammingProcedure {
    /// Whether a resume must read the device back before trusting recorded
    /// progress. True whenever an erase can have blanked already-written bytes,
    /// which — absent proof to the contrary — we assume.
    #[must_use]
    pub fn requires_readback_on_resume(&self) -> bool {
        self.whole_region_erase || self.erase_granularity > 0
    }
}

/// Attestation that the payload's checksums are satisfied, **bound to the exact
/// payload bytes**. Produced by the caller (`opentune-checksum`); the flash layer
/// enforces presence and binding, it does not recompute OEM checksums itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChecksumAttestation {
    /// The caller asserts every required checksum block is correct for `payload`.
    pub satisfied: bool,
    /// SHA-256 (hex) of the payload the attestation was produced for.
    pub payload_sha256: String,
    /// Algorithm id(s) that were checked (informational / log).
    #[serde(default)]
    pub algorithm: String,
    /// Regions the checksums cover (informational / log).
    #[serde(default)]
    pub covered: Vec<MemoryRegion>,
    /// Free-text note, e.g. "corrected 2 blocks".
    #[serde(default)]
    pub note: String,
}

impl ChecksumAttestation {
    /// Build an attestation bound to `payload`.
    #[must_use]
    pub fn for_payload(payload: &[u8], satisfied: bool, algorithm: impl Into<String>) -> Self {
        Self {
            satisfied,
            payload_sha256: sha256_hex(payload),
            algorithm: algorithm.into(),
            covered: Vec::new(),
            note: String::new(),
        }
    }

    /// An explicit "no checksum applies to this target" attestation, still bound
    /// to the payload so it cannot be reused for a different image.
    #[must_use]
    pub fn none_required(payload: &[u8]) -> Self {
        Self {
            satisfied: true,
            payload_sha256: sha256_hex(payload),
            algorithm: "none".into(),
            covered: Vec::new(),
            note: "no checksum algorithm applies to this target".into(),
        }
    }

    /// Check the attestation is satisfied and bound to exactly these bytes.
    pub fn verify_for(&self, payload: &[u8]) -> std::result::Result<(), PolicyViolation> {
        if !self.satisfied {
            return Err(PolicyViolation::ChecksumCoverageUnsatisfied {
                algorithm: self.algorithm.clone(),
            });
        }
        if self.payload_sha256 != sha256_hex(payload) {
            return Err(PolicyViolation::ChecksumAttestationPayloadMismatch);
        }
        Ok(())
    }
}

/// A single, central policy for a write. Build it with [`FlashSafetyPolicy::production`]
/// for real writes, [`FlashSafetyPolicy::mock_testing`] for deliberately flexible
/// test flows, or [`FlashSafetyPolicy::from_legacy`] to reproduce the historical
/// [`SafetyConfig`](crate::SafetyConfig) behaviour.
#[derive(Debug, Clone)]
pub struct FlashSafetyPolicy {
    /// Classification of this write path.
    pub mode: WriteMode,
    /// Minimum battery voltage (mV).
    pub min_millivolts: u32,
    /// How to treat voltage, including the unreadable case.
    pub voltage: VoltagePolicy,
    /// Require a verified pre-flash backup before writing.
    pub require_verified_backup: bool,
    /// Require post-write read-back verification.
    pub require_readback_verify: bool,
    /// Require the flash region to lie inside a declared writable region.
    pub require_writable_region: bool,
    /// Require the connected ECU identity to match the target.
    pub require_identity_match: bool,
    /// Require a satisfied checksum attestation bound to the payload.
    pub require_checksum_coverage: bool,
    /// Chunk size (bytes) for the resumable path. 0 → [`crate::DEFAULT_CHUNK_SIZE`].
    pub chunk_size: usize,
}

impl FlashSafetyPolicy {
    /// The strict, fail-closed policy for a real write to a controller.
    #[must_use]
    pub fn production() -> Self {
        Self {
            mode: WriteMode::Production,
            min_millivolts: 12_000,
            voltage: VoltagePolicy::Required,
            require_verified_backup: true,
            require_readback_verify: true,
            require_writable_region: true,
            require_identity_match: true,
            require_checksum_coverage: true,
            chunk_size: crate::DEFAULT_CHUNK_SIZE,
        }
    }

    /// A deliberately flexible policy for mock/test flows. Keeps backup + verify
    /// on (cheap, and they exercise the pipeline) but drops the gates that need
    /// real hardware or a real target.
    #[must_use]
    pub fn mock_testing() -> Self {
        Self {
            mode: WriteMode::MockTesting,
            min_millivolts: 0,
            voltage: VoltagePolicy::Disabled,
            require_verified_backup: true,
            require_readback_verify: true,
            require_writable_region: false,
            require_identity_match: false,
            require_checksum_coverage: false,
            chunk_size: crate::DEFAULT_CHUNK_SIZE,
        }
    }

    /// Reproduce the legacy [`SafetyConfig`](crate::SafetyConfig) behaviour
    /// exactly: a voltage gate that only fails closed for voltage-capable
    /// interfaces, and no target-level checks.
    #[must_use]
    pub fn from_legacy(cfg: &crate::SafetyConfig) -> Self {
        Self {
            mode: WriteMode::Compat,
            min_millivolts: cfg.min_millivolts,
            voltage: if cfg.min_millivolts == 0 {
                VoltagePolicy::Disabled
            } else {
                VoltagePolicy::RequiredWhenAvailable
            },
            require_verified_backup: cfg.require_backup,
            require_readback_verify: cfg.verify_after_write,
            require_writable_region: false,
            require_identity_match: false,
            require_checksum_coverage: false,
            chunk_size: cfg.chunk_size,
        }
    }

    /// True for a fail-closed production write.
    #[must_use]
    pub fn is_production(&self) -> bool {
        self.mode == WriteMode::Production
    }

    /// Up-front structural check: a production write needs a target that can
    /// actually satisfy the criteria this policy demands. Returns the first
    /// problem as a typed violation. Call this before touching the interface.
    pub fn check_target(
        &self,
        target: Option<&FlashTarget>,
    ) -> std::result::Result<(), PolicyViolation> {
        let Some(t) = target else {
            if self.is_production() {
                return Err(PolicyViolation::ProductionRequiresTarget);
            }
            return Ok(());
        };
        if self.require_writable_region && t.writable_regions.is_empty() {
            return Err(PolicyViolation::NoWritableRegionsDeclared);
        }
        if self.require_identity_match && t.identity.is_unspecified() {
            return Err(PolicyViolation::TargetIdentityUnspecified);
        }
        Ok(())
    }

    /// Voltage gate. `cap_voltage` is whether the interface advertises the
    /// battery-voltage capability.
    pub fn check_voltage(
        &self,
        measured: Option<u32>,
        cap_voltage: bool,
    ) -> std::result::Result<(), PolicyViolation> {
        match self.voltage {
            VoltagePolicy::Disabled => Ok(()),
            VoltagePolicy::Required => match measured {
                Some(v) if v < self.min_millivolts => Err(PolicyViolation::VoltageTooLow {
                    measured_mv: v,
                    min_mv: self.min_millivolts,
                }),
                // Fail closed: no reading means we cannot prove a safe voltage.
                None => Err(PolicyViolation::VoltageUnreadable),
                _ => Ok(()),
            },
            VoltagePolicy::RequiredWhenAvailable => {
                if self.min_millivolts == 0 {
                    return Ok(());
                }
                match measured {
                    Some(v) if v < self.min_millivolts => Err(PolicyViolation::VoltageTooLow {
                        measured_mv: v,
                        min_mv: self.min_millivolts,
                    }),
                    None if cap_voltage => Err(PolicyViolation::VoltageUnreadable),
                    _ => Ok(()),
                }
            }
        }
    }

    /// The flash region must lie fully inside a declared writable region.
    pub fn check_region_writable(
        &self,
        region: MemoryRegion,
        target: Option<&FlashTarget>,
    ) -> std::result::Result<(), PolicyViolation> {
        if !self.require_writable_region {
            return Ok(());
        }
        let Some(t) = target else {
            return Err(PolicyViolation::ProductionRequiresTarget);
        };
        let start = u64::from(region.address);
        let end = start + u64::from(region.length);
        let ok = t.writable_regions.iter().any(|w| {
            let ws = u64::from(w.address);
            let we = ws + u64::from(w.length);
            start >= ws && end <= we
        });
        if ok {
            Ok(())
        } else {
            Err(PolicyViolation::RegionNotWritable { region })
        }
    }

    /// The connected ECU must match the target identity. `connected` is the raw
    /// identity read from the device (or `None` if it could not be read).
    pub fn check_identity(
        &self,
        connected: Option<&EcuId>,
        target: Option<&FlashTarget>,
    ) -> std::result::Result<(), PolicyViolation> {
        if !self.require_identity_match {
            return Ok(());
        }
        let Some(t) = target else {
            return Err(PolicyViolation::ProductionRequiresTarget);
        };
        let Some(id) = connected else {
            return Err(PolicyViolation::IdentityUnreadable);
        };
        if t.identity.matches(id) {
            Ok(())
        } else {
            Err(PolicyViolation::IdentityMismatch {
                expected: t.identity.label.clone().unwrap_or_else(|| {
                    format!("{:?}/{:?}", t.identity.part_numbers, t.identity.sw_numbers)
                }),
                connected: TargetIdentity::fingerprint_of(id),
            })
        }
    }

    /// The payload must carry a satisfied checksum attestation bound to its bytes.
    pub fn check_checksum(
        &self,
        payload: &[u8],
        target: Option<&FlashTarget>,
    ) -> std::result::Result<(), PolicyViolation> {
        if !self.require_checksum_coverage {
            return Ok(());
        }
        let Some(t) = target else {
            return Err(PolicyViolation::ProductionRequiresTarget);
        };
        t.checksum.verify_for(payload)
    }
}

/// A single, typed reason a write was refused. Every production refusal is one of
/// these rather than a silently weakened guarantee. Converts into
/// [`opentune_core::Error`] for the orchestrator's `Result`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyViolation {
    /// A production write was attempted without a [`FlashTarget`].
    ProductionRequiresTarget,
    /// The target declares no writable regions but the policy requires one.
    NoWritableRegionsDeclared,
    /// The target declares no identity keys but the policy requires a match.
    TargetIdentityUnspecified,
    /// Payload length does not equal the region length.
    PayloadLengthMismatch { got: usize, want: u32 },
    /// Voltage could not be read and the policy fails closed on that.
    VoltageUnreadable,
    /// Measured voltage is below the minimum.
    VoltageTooLow { measured_mv: u32, min_mv: u32 },
    /// The pre-flash backup is required but was not obtained.
    BackupMissing,
    /// The backup read came up short.
    BackupShort { got: usize, want: u32 },
    /// The flash region is not inside any declared writable region.
    RegionNotWritable { region: MemoryRegion },
    /// The connected ECU identity could not be read.
    IdentityUnreadable,
    /// The connected ECU identity does not match the intended target.
    IdentityMismatch { expected: String, connected: String },
    /// The checksum attestation says checksums are not satisfied.
    ChecksumCoverageUnsatisfied { algorithm: String },
    /// The checksum attestation is for a different payload than the one written.
    ChecksumAttestationPayloadMismatch,
    /// A recovery checkpoint carried no bound identity (an older/foreign file),
    /// so it cannot be safely resumed.
    CheckpointIdentityMissing,
    /// The connected ECU does not match the identity bound into the checkpoint.
    CheckpointIdentityMismatch { bound: String, connected: String },
    /// On resume, identity could not be read, so auto-resume is refused; a fresh
    /// verified read is required.
    ResumeIdentityUnrecoverable,
    /// The checkpoint's programming procedure differs from the current one.
    CheckpointProcedureMismatch { bound: String, current: String },
}

impl std::fmt::Display for PolicyViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProductionRequiresTarget => write!(
                f,
                "production write requires a FlashTarget (identity, writable regions, checksum) — refusing to write without one"
            ),
            Self::NoWritableRegionsDeclared => {
                write!(f, "target declares no writable regions — refusing to write")
            }
            Self::TargetIdentityUnspecified => write!(
                f,
                "target declares no ECU identity to match — refusing a production write that cannot confirm the controller"
            ),
            Self::PayloadLengthMismatch { got, want } => {
                write!(f, "data length {got} != region length {want}")
            }
            Self::VoltageUnreadable => write!(
                f,
                "battery voltage unreadable — production writes fail closed without a confirmed voltage"
            ),
            Self::VoltageTooLow {
                measured_mv,
                min_mv,
            } => write!(f, "battery {measured_mv} mV below minimum {min_mv} mV"),
            Self::BackupMissing => write!(f, "required pre-flash backup was not obtained"),
            Self::BackupShort { got, want } => {
                write!(f, "backup read short — aborting ({got} of {want} bytes)")
            }
            Self::RegionNotWritable { region } => write!(
                f,
                "region 0x{:08X} +{} is not inside any declared writable region — refusing to write",
                region.address, region.length
            ),
            Self::IdentityUnreadable => write!(
                f,
                "connected ECU identity could not be read — refusing to write to an unidentified controller"
            ),
            Self::IdentityMismatch {
                expected,
                connected,
            } => write!(
                f,
                "connected ECU does not match the intended target (expected {expected}, connected {connected}) — refusing to write"
            ),
            Self::ChecksumCoverageUnsatisfied { algorithm } => write!(
                f,
                "checksum coverage not satisfied for algorithm '{algorithm}' — refusing to write an image whose checksums are not corrected"
            ),
            Self::ChecksumAttestationPayloadMismatch => write!(
                f,
                "checksum attestation is for a different payload (data hash mismatch) — refusing to write"
            ),
            Self::CheckpointIdentityMissing => write!(
                f,
                "recovery checkpoint carries no bound ECU identity — refusing to auto-resume; take a fresh verified read"
            ),
            Self::CheckpointIdentityMismatch { bound, connected } => write!(
                f,
                "recovery checkpoint was created for a different ECU (bound {bound}, connected {connected}) — refusing to resume onto the wrong controller"
            ),
            Self::ResumeIdentityUnrecoverable => write!(
                f,
                "ECU identity could not be recovered after interruption — refusing to auto-resume; take a fresh verified read"
            ),
            Self::CheckpointProcedureMismatch { bound, current } => write!(
                f,
                "recovery checkpoint programming procedure '{bound}' != current '{current}' — refusing to resume"
            ),
        }
    }
}

impl std::error::Error for PolicyViolation {}

impl From<PolicyViolation> for Error {
    /// Every policy violation is a pre-write *safety* refusal. Read-back
    /// verification failures are raised directly as [`Error::Verify`] by the
    /// orchestrator, not through this type.
    fn from(v: PolicyViolation) -> Self {
        Error::Safety(v.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ecu(hw: &str, sw: &str) -> EcuId {
        EcuId {
            hw_number: Some(hw.into()),
            sw_number: Some(sw.into()),
            ecu_name: Some("edc16".into()),
            ..Default::default()
        }
    }

    #[test]
    fn production_voltage_fails_closed_without_reading() {
        let p = FlashSafetyPolicy::production();
        // No reading at all, regardless of capability → refusal.
        assert_eq!(
            p.check_voltage(None, false),
            Err(PolicyViolation::VoltageUnreadable)
        );
        assert_eq!(
            p.check_voltage(None, true),
            Err(PolicyViolation::VoltageUnreadable)
        );
        assert_eq!(
            p.check_voltage(Some(11_000), true),
            Err(PolicyViolation::VoltageTooLow {
                measured_mv: 11_000,
                min_mv: 12_000
            })
        );
        assert!(p.check_voltage(Some(12_600), true).is_ok());
    }

    #[test]
    fn legacy_voltage_matches_historical_behaviour() {
        let cfg = crate::SafetyConfig::default();
        let p = FlashSafetyPolicy::from_legacy(&cfg);
        // Voltage-capable but unreadable → refusal, as before.
        assert_eq!(
            p.check_voltage(None, true),
            Err(PolicyViolation::VoltageUnreadable)
        );
        // NOT voltage-capable and unreadable → historically allowed through.
        assert!(p.check_voltage(None, false).is_ok());
        // Below minimum → "below minimum" message preserved.
        let e: Error = p.check_voltage(Some(100), true).unwrap_err().into();
        assert!(e.to_string().contains("below minimum"));
    }

    #[test]
    fn identity_match_and_mismatch() {
        let p = FlashSafetyPolicy::production();
        let target = FlashTarget {
            ecu_def_id: "edc16_vag".into(),
            identity: TargetIdentity {
                part_numbers: vec!["03G906".into()],
                sw_numbers: vec!["1037".into()],
                label: Some("EDC16 VAG".into()),
            },
            writable_regions: vec![MemoryRegion::new(0, 1024)],
            procedure: ProgrammingProcedure::default(),
            checksum: ChecksumAttestation::none_required(&[]),
        };
        assert!(p
            .check_identity(Some(&ecu("03G906012A", "1037369105")), Some(&target))
            .is_ok());
        assert!(matches!(
            p.check_identity(Some(&ecu("0281010", "9999")), Some(&target)),
            Err(PolicyViolation::IdentityMismatch { .. })
        ));
        assert_eq!(
            p.check_identity(None, Some(&target)),
            Err(PolicyViolation::IdentityUnreadable)
        );
    }

    #[test]
    fn checksum_attestation_is_payload_bound() {
        let p = FlashSafetyPolicy::production();
        let payload = vec![0xAAu8; 32];
        let other = vec![0xBBu8; 32];
        let good = ChecksumAttestation::for_payload(&payload, true, "edc16");
        let target = FlashTarget {
            ecu_def_id: "x".into(),
            identity: TargetIdentity::default(),
            writable_regions: vec![],
            procedure: ProgrammingProcedure::default(),
            checksum: good.clone(),
        };
        assert!(p.check_checksum(&payload, Some(&target)).is_ok());
        // Same attestation, different payload → refused.
        assert_eq!(
            p.check_checksum(&other, Some(&target)),
            Err(PolicyViolation::ChecksumAttestationPayloadMismatch)
        );
        // Unsatisfied → refused.
        let bad = ChecksumAttestation::for_payload(&payload, false, "edc16");
        assert!(matches!(
            bad.verify_for(&payload),
            Err(PolicyViolation::ChecksumCoverageUnsatisfied { .. })
        ));
    }

    #[test]
    fn writable_region_containment() {
        let p = FlashSafetyPolicy::production();
        let target = FlashTarget {
            ecu_def_id: "x".into(),
            identity: TargetIdentity::default(),
            writable_regions: vec![MemoryRegion::new(0x8000, 0x4000)],
            procedure: ProgrammingProcedure::default(),
            checksum: ChecksumAttestation::none_required(&[]),
        };
        assert!(p
            .check_region_writable(MemoryRegion::new(0x8000, 0x100), Some(&target))
            .is_ok());
        assert!(p
            .check_region_writable(MemoryRegion::new(0xB000, 0x1000), Some(&target))
            .is_ok());
        // Spills past the end.
        assert!(matches!(
            p.check_region_writable(MemoryRegion::new(0xBFFF, 0x10), Some(&target)),
            Err(PolicyViolation::RegionNotWritable { .. })
        ));
        // Below the start.
        assert!(matches!(
            p.check_region_writable(MemoryRegion::new(0x7000, 0x10), Some(&target)),
            Err(PolicyViolation::RegionNotWritable { .. })
        ));
    }

    #[test]
    fn production_requires_target() {
        let p = FlashSafetyPolicy::production();
        assert_eq!(
            p.check_target(None),
            Err(PolicyViolation::ProductionRequiresTarget)
        );
        assert!(FlashSafetyPolicy::mock_testing().check_target(None).is_ok());
    }
}
