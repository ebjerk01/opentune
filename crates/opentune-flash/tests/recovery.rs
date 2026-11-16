//! Fault-injection tests for the production flash safety policy and
//! identity-bound recovery, driven by the offline file/mock driver.
//!
//! Covers: interrupted transfer + resume, verification failure, checkpoint
//! persistence failure, erase/reset between runs (must NOT skip blanked data),
//! and resume against the wrong ECU identity (must refuse).
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Error, Interface, InterfaceInfo, MemoryRegion, Progress,
    Protocol, Result,
};
use opentune_driver_mock::MockDriver;
use opentune_flash::{
    flash_calibration_checked, flash_calibration_resumable_checked, ChecksumAttestation,
    FlashSafetyPolicy, FlashTarget, ProgrammingProcedure, SafetyConfig, TargetIdentity, Transcript,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

fn scratch(tag: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "opentune_recovery_{}_{}_{}_{tag}",
        std::process::id(),
        n,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_image(dir: &Path, byte: u8, len: usize) -> PathBuf {
    let p = dir.join("flash.bin");
    std::fs::write(&p, vec![byte; len]).unwrap();
    p
}

fn open_mock(image: &Path, ecu: &str) -> MockDriver {
    let mut drv = MockDriver::new();
    let mut cfg = ConnectConfig {
        protocol: Protocol::IsoTp,
        ..Default::default()
    };
    cfg.options
        .insert("file".into(), image.to_string_lossy().into_owned());
    cfg.options.insert("ecu".into(), ecu.into());
    drv.open(&cfg).unwrap();
    drv
}

fn noprog(_: Progress) {}

/// A target whose declared writable region covers `[0, len)`, whose identity
/// matches the mock's reported `sw_number` ("<n> bytes"), and whose checksum
/// attestation is bound to `payload`.
fn target_for(payload: &[u8], len: u32) -> FlashTarget {
    FlashTarget {
        ecu_def_id: "mock-edc16".into(),
        identity: TargetIdentity {
            part_numbers: vec![],
            // MockDriver reports sw_number = "<len> bytes"; match that substring.
            sw_numbers: vec!["bytes".into()],
            label: Some("mock EDC16".into()),
        },
        writable_regions: vec![MemoryRegion::new(0, len)],
        procedure: ProgrammingProcedure {
            name: "uds-34-36-37".into(),
            erase_granularity: 256,
            whole_region_erase: true,
        },
        checksum: ChecksumAttestation::for_payload(payload, true, "edc16"),
    }
}

// --- Fault-injecting driver wrappers ---------------------------------------

/// Fails the Nth `write_memory` call (the write still lands on the device, as a
/// real interrupted transfer where the ECU got the data but the tool lost the
/// link before seeing success).
struct FailAfter {
    inner: MockDriver,
    writes_before_fail: usize,
    writes: usize,
}

impl Interface for FailAfter {
    fn info(&self) -> InterfaceInfo {
        self.inner.info()
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        self.inner.open(cfg)
    }
    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }
    fn battery_millivolts(&mut self) -> Result<Option<u32>> {
        self.inner.battery_millivolts()
    }
    fn read_ecu_id(&mut self) -> Result<EcuId> {
        self.inner.read_ecu_id()
    }
    fn read_memory(&mut self, r: MemoryRegion, p: &mut dyn FnMut(Progress)) -> Result<Vec<u8>> {
        self.inner.read_memory(r, p)
    }
    fn write_memory(
        &mut self,
        r: MemoryRegion,
        data: &[u8],
        p: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        self.writes += 1;
        if self.writes > self.writes_before_fail {
            self.inner.write_memory(r, data, p)?;
            return Err(Error::Timeout(1));
        }
        self.inner.write_memory(r, data, p)
    }
}

/// Corrupts the read-back after a write so verification fails.
struct CorruptReadback {
    inner: MockDriver,
    wrote: bool,
}

impl Interface for CorruptReadback {
    fn info(&self) -> InterfaceInfo {
        self.inner.info()
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        self.inner.open(cfg)
    }
    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }
    fn battery_millivolts(&mut self) -> Result<Option<u32>> {
        self.inner.battery_millivolts()
    }
    fn read_ecu_id(&mut self) -> Result<EcuId> {
        self.inner.read_ecu_id()
    }
    fn read_memory(&mut self, r: MemoryRegion, p: &mut dyn FnMut(Progress)) -> Result<Vec<u8>> {
        let mut v = self.inner.read_memory(r, p)?;
        // Once a write has happened, flip a byte in every read so no read-back
        // can match the intended data.
        if self.wrote {
            if let Some(b) = v.first_mut() {
                *b ^= 0xFF;
            }
        }
        Ok(v)
    }
    fn write_memory(
        &mut self,
        r: MemoryRegion,
        data: &[u8],
        p: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        self.wrote = true;
        self.inner.write_memory(r, data, p)
    }
}

/// An interface that can write but cannot report battery voltage (like the real
/// SocketCAN / SLCAN / gs_usb / J2534 / K-line-bridge drivers).
struct NoVoltage {
    inner: MockDriver,
}

impl Interface for NoVoltage {
    fn info(&self) -> InterfaceInfo {
        self.inner.info()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            read_memory: true,
            write_memory: true,
            battery_voltage: false,
            ..Default::default()
        }
    }
    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        self.inner.open(cfg)
    }
    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }
    fn battery_millivolts(&mut self) -> Result<Option<u32>> {
        Ok(None)
    }
    fn read_ecu_id(&mut self) -> Result<EcuId> {
        self.inner.read_ecu_id()
    }
    fn read_memory(&mut self, r: MemoryRegion, p: &mut dyn FnMut(Progress)) -> Result<Vec<u8>> {
        self.inner.read_memory(r, p)
    }
    fn write_memory(
        &mut self,
        r: MemoryRegion,
        data: &[u8],
        p: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        self.inner.write_memory(r, data, p)
    }
}

/// Reports a fixed ECU identity, to simulate connecting to the wrong controller.
struct FixedId {
    inner: MockDriver,
    id: EcuId,
}

impl Interface for FixedId {
    fn info(&self) -> InterfaceInfo {
        self.inner.info()
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn open(&mut self, cfg: &ConnectConfig) -> Result<()> {
        self.inner.open(cfg)
    }
    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }
    fn battery_millivolts(&mut self) -> Result<Option<u32>> {
        self.inner.battery_millivolts()
    }
    fn read_ecu_id(&mut self) -> Result<EcuId> {
        Ok(self.id.clone())
    }
    fn read_memory(&mut self, r: MemoryRegion, p: &mut dyn FnMut(Progress)) -> Result<Vec<u8>> {
        self.inner.read_memory(r, p)
    }
    fn write_memory(
        &mut self,
        r: MemoryRegion,
        data: &[u8],
        p: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        self.inner.write_memory(r, data, p)
    }
}

// --- Production policy (§1) -------------------------------------------------

#[test]
fn production_happy_path_on_mock() {
    let dir = scratch("prod_ok");
    let image = make_image(&dir, 0x00, 512);
    let data = vec![0xAB; 512];
    let target = target_for(&data, 512);
    let mut drv = open_mock(&image, "mock-edc16");
    let region = MemoryRegion::new(0, 512);

    let rep = flash_calibration_checked(
        &mut drv,
        region,
        &data,
        &target,
        &FlashSafetyPolicy::production(),
        &mut noprog,
    )
    .unwrap();
    assert!(rep.verified);
    assert_eq!(std::fs::read(&image).unwrap(), data);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn production_voltage_unreadable_fails_closed() {
    let dir = scratch("prod_novolt");
    let image = make_image(&dir, 0x00, 128);
    let data = vec![0x01; 128];
    let target = target_for(&data, 128);
    let mut drv = NoVoltage {
        inner: open_mock(&image, "mock-edc16"),
    };
    let err = flash_calibration_checked(
        &mut drv,
        MemoryRegion::new(0, 128),
        &data,
        &target,
        &FlashSafetyPolicy::production(),
        &mut noprog,
    )
    .unwrap_err();
    assert!(err.to_string().contains("voltage unreadable"), "{err}");
    // Nothing written.
    assert!(std::fs::read(&image).unwrap().iter().all(|&b| b == 0x00));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn production_region_not_writable_is_refused() {
    let dir = scratch("prod_region");
    let image = make_image(&dir, 0x00, 256);
    let data = vec![0x02; 256];
    // Declare a writable region that does NOT cover the write.
    let mut target = target_for(&data, 256);
    target.writable_regions = vec![MemoryRegion::new(0x1000, 0x100)];
    let mut drv = open_mock(&image, "mock-edc16");
    let err = flash_calibration_checked(
        &mut drv,
        MemoryRegion::new(0, 256),
        &data,
        &target,
        &FlashSafetyPolicy::production(),
        &mut noprog,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("not inside any declared writable"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn production_identity_mismatch_is_refused() {
    let dir = scratch("prod_id");
    let image = make_image(&dir, 0x00, 64);
    let data = vec![0x03; 64];
    let mut target = target_for(&data, 64);
    // Demand a part number the mock does not report.
    target.identity = TargetIdentity {
        part_numbers: vec!["03G906016".into()],
        sw_numbers: vec![],
        label: Some("some other ECU".into()),
    };
    let mut drv = open_mock(&image, "mock-edc16");
    let err = flash_calibration_checked(
        &mut drv,
        MemoryRegion::new(0, 64),
        &data,
        &target,
        &FlashSafetyPolicy::production(),
        &mut noprog,
    )
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("does not match the intended target"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn production_checksum_attestation_must_match_payload() {
    let dir = scratch("prod_cksum");
    let image = make_image(&dir, 0x00, 64);
    let data = vec![0x04; 64];
    let mut target = target_for(&data, 64);
    // Attestation bound to DIFFERENT bytes than the payload being written.
    target.checksum = ChecksumAttestation::for_payload(&[0x99u8; 64], true, "edc16");
    let mut drv = open_mock(&image, "mock-edc16");
    let err = flash_calibration_checked(
        &mut drv,
        MemoryRegion::new(0, 64),
        &data,
        &target,
        &FlashSafetyPolicy::production(),
        &mut noprog,
    )
    .unwrap_err();
    assert!(err.to_string().contains("different payload"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

// --- Recovery / interruption (§2) ------------------------------------------

#[test]
fn interrupted_then_resume_completes_checked() {
    let dir = scratch("resume_ok");
    let image = make_image(&dir, 0x11, 1024);
    let state = dir.join("flash.state");
    let region = MemoryRegion::new(0, 1024);
    let data: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
    let target = target_for(&data, 1024);
    let policy = FlashSafetyPolicy {
        chunk_size: 128,
        ..FlashSafetyPolicy::production()
    };

    // First run: fail after the 3rd chunk write.
    {
        let mut drv = FailAfter {
            inner: open_mock(&image, "mock-edc16"),
            writes_before_fail: 3,
            writes: 0,
        };
        let mut tr = Transcript::new();
        let _ = flash_calibration_resumable_checked(
            &mut drv,
            region,
            &data,
            &target,
            &policy,
            &state,
            &mut tr,
            &mut noprog,
        )
        .unwrap_err();
    }
    assert!(state.exists());
    // The checkpoint bound an ECU identity.
    let ck = std::fs::read_to_string(&state).unwrap();
    assert!(ck.contains("ecu_identity"));
    assert!(ck.contains("mock-edc16"));

    // Resume with a healthy driver → completes.
    let mut drv = open_mock(&image, "mock-edc16");
    let mut tr = Transcript::new();
    let rep = flash_calibration_resumable_checked(
        &mut drv,
        region,
        &data,
        &target,
        &policy,
        &state,
        &mut tr,
        &mut noprog,
    )
    .unwrap();
    assert!(rep.verified);
    assert_eq!(std::fs::read(&image).unwrap(), data);
    assert!(!state.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_against_wrong_identity_is_refused() {
    let dir = scratch("resume_wrongid");
    let image = make_image(&dir, 0x11, 512);
    let state = dir.join("flash.state");
    let region = MemoryRegion::new(0, 512);
    let data: Vec<u8> = (0..512).map(|i| (i % 251) as u8).collect();
    let target = target_for(&data, 512);
    let policy = FlashSafetyPolicy {
        chunk_size: 128,
        ..FlashSafetyPolicy::production()
    };

    // First run interrupted on the original ECU (sw_number "512 bytes").
    {
        let mut drv = FailAfter {
            inner: open_mock(&image, "mock-edc16"),
            writes_before_fail: 1,
            writes: 0,
        };
        let mut tr = Transcript::new();
        let _ = flash_calibration_resumable_checked(
            &mut drv,
            region,
            &data,
            &target,
            &policy,
            &state,
            &mut tr,
            &mut noprog,
        )
        .unwrap_err();
    }
    assert!(state.exists());

    // Resume, but the connected ECU now reports a DIFFERENT identity (different
    // sw_number fingerprint) while still matching the target's loose key.
    let wrong = EcuId {
        ecu_name: Some("mock-edc16".into()),
        sw_number: Some("DIFFERENT 512 bytes".into()),
        hw_number: Some("OTHER".into()),
        ..Default::default()
    };
    let mut drv = FixedId {
        inner: open_mock(&image, "mock-edc16"),
        id: wrong,
    };
    let mut tr = Transcript::new();
    let err = flash_calibration_resumable_checked(
        &mut drv,
        region,
        &data,
        &target,
        &policy,
        &state,
        &mut tr,
        &mut noprog,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("different ECU") || err.to_string().contains("wrong controller"),
        "{err}"
    );
    // Checkpoint untouched so the correct ECU can still resume.
    assert!(state.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_after_erase_does_not_skip_blanked_data() {
    // The anti-brick case: a prior run verified + checkpointed chunks 0..k, then
    // the ECU erased/reset and those bytes went blank. Resume must re-read and
    // REWRITE them, not trust the checkpoint and skip.
    let dir = scratch("resume_erase");
    let image = make_image(&dir, 0x11, 1024);
    let state = dir.join("flash.state");
    let region = MemoryRegion::new(0, 1024);
    let data: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
    let target = target_for(&data, 1024);
    let policy = FlashSafetyPolicy {
        chunk_size: 128,
        ..FlashSafetyPolicy::production()
    };

    // First run: fail after 4 chunk writes (chunks 0..=3 land + are checkpointed
    // or in-flight; chunks 4..7 untouched).
    {
        let mut drv = FailAfter {
            inner: open_mock(&image, "mock-edc16"),
            writes_before_fail: 4,
            writes: 0,
        };
        let mut tr = Transcript::new();
        let _ = flash_calibration_resumable_checked(
            &mut drv,
            region,
            &data,
            &target,
            &policy,
            &state,
            &mut tr,
            &mut noprog,
        )
        .unwrap_err();
    }
    assert!(state.exists());

    // Simulate an erase/reset: blank the ENTIRE region on the device to 0xFF.
    std::fs::write(&image, vec![0xFFu8; 1024]).unwrap();

    // Resume. Despite the checkpoint marking early chunks verified, every chunk
    // is read back; the blanked ones no longer match and get rewritten.
    let mut drv = open_mock(&image, "mock-edc16");
    let mut tr = Transcript::new();
    let rep = flash_calibration_resumable_checked(
        &mut drv,
        region,
        &data,
        &target,
        &policy,
        &state,
        &mut tr,
        &mut noprog,
    )
    .unwrap();
    assert!(rep.verified);
    // The whole region is the intended payload — no blanked bytes left behind.
    assert_eq!(std::fs::read(&image).unwrap(), data);
    assert!(!state.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn verification_failure_aborts_and_keeps_checkpoint() {
    let dir = scratch("verify_fail");
    let image = make_image(&dir, 0x00, 256);
    let state = dir.join("flash.state");
    let region = MemoryRegion::new(0, 256);
    let data = vec![0x5A; 256];
    let target = target_for(&data, 256);
    let policy = FlashSafetyPolicy {
        chunk_size: 128,
        ..FlashSafetyPolicy::production()
    };
    let mut drv = CorruptReadback {
        inner: open_mock(&image, "mock-edc16"),
        wrote: false,
    };
    let mut tr = Transcript::new();
    let err = flash_calibration_resumable_checked(
        &mut drv,
        region,
        &data,
        &target,
        &policy,
        &state,
        &mut tr,
        &mut noprog,
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::Verify(_)),
        "expected a verify error, got {err}"
    );
    // The checkpoint survives so the operator can retry / recover.
    assert!(state.exists());
    // The original backup is preserved intact.
    let backup = std::fs::read(state.with_file_name("flash.state.backup")).unwrap();
    assert!(backup.iter().all(|&b| b == 0x00));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn checkpoint_persistence_failure_aborts_before_any_write() {
    // If progress cannot be persisted, the flash must fail rather than write
    // blind. With backup disabled, the first persistence is the baseline
    // checkpoint; an unwritable state directory makes it fail before any write.
    let dir = scratch("persist_fail");
    let image = make_image(&dir, 0x00, 256);
    let data = vec![0x7E; 256];
    let region = MemoryRegion::new(0, 256);

    // A state path inside a directory that does not exist and cannot be created
    // under it (parent is a regular file).
    let not_a_dir = dir.join("afile");
    std::fs::write(&not_a_dir, b"x").unwrap();
    let state = not_a_dir.join("nested").join("flash.state");

    // Use a legacy-style permissive config (no backup, no target) so the first
    // disk access attempted is the checkpoint path itself.
    let safety = SafetyConfig {
        require_backup: false,
        chunk_size: 128,
        ..Default::default()
    };

    let mut drv = open_mock(&image, "mock-edc16");
    let mut tr = Transcript::new();
    // Drive the permissive path through the public legacy function.
    let err = opentune_flash::flash_calibration_resumable_logged(
        &mut drv,
        region,
        &data,
        &safety,
        &state,
        &mut tr,
        &mut noprog,
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::Io(_)),
        "expected an IO error, got {err}"
    );
    // The device image was never written.
    assert!(std::fs::read(&image).unwrap().iter().all(|&b| b == 0x00));
    let _ = std::fs::remove_dir_all(&dir);
}
