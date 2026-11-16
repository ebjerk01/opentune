//! End-to-end tests for resumable flashing + session transcripts, driven by the
//! offline file/mock driver.
use opentune_core::{ConnectConfig, Interface, MemoryRegion, Phase, Progress, Protocol, Result};
use opentune_driver_mock::MockDriver;
use opentune_flash::{
    flash_calibration_resumable, flash_calibration_resumable_logged, FlashEvent, SafetyConfig,
    Transcript,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// A unique scratch dir per test invocation.
fn scratch(tag: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "opentune_resume_{}_{}_{}_{tag}",
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

fn open_mock(image: &Path) -> MockDriver {
    let mut drv = MockDriver::new();
    let mut cfg = ConnectConfig {
        protocol: Protocol::IsoTp,
        ..Default::default()
    };
    cfg.options
        .insert("file".into(), image.to_string_lossy().into_owned());
    cfg.options.insert("ecu".into(), "test-edc16".into());
    drv.open(&cfg).unwrap();
    drv
}

fn noprog(_: Progress) {}

/// A driver wrapper that fails the Nth `write_memory` call, to simulate a
/// flash interrupted mid-way (e.g. a dropped link or power loss).
struct FailAfter {
    inner: MockDriver,
    writes_before_fail: usize,
    writes: usize,
}

impl FailAfter {
    fn new(inner: MockDriver, writes_before_fail: usize) -> Self {
        Self {
            inner,
            writes_before_fail,
            writes: 0,
        }
    }
}

impl Interface for FailAfter {
    fn info(&self) -> opentune_core::InterfaceInfo {
        self.inner.info()
    }
    fn capabilities(&self) -> opentune_core::Capabilities {
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
    fn read_ecu_id(&mut self) -> Result<opentune_core::EcuId> {
        self.inner.read_ecu_id()
    }
    fn read_memory(
        &mut self,
        region: MemoryRegion,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Vec<u8>> {
        self.inner.read_memory(region, progress)
    }
    fn write_memory(
        &mut self,
        region: MemoryRegion,
        data: &[u8],
        progress: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        self.writes += 1;
        if self.writes > self.writes_before_fail {
            // Simulate an interruption: the write to the device DID land (the
            // mock persists it) but the caller never sees success / checkpoint.
            self.inner.write_memory(region, data, progress)?;
            return Err(opentune_core::Error::Timeout(1));
        }
        self.inner.write_memory(region, data, progress)
    }
}

fn make_image(dir: &Path, byte: u8, len: usize) -> PathBuf {
    let p = dir.join("flash.bin");
    std::fs::write(&p, vec![byte; len]).unwrap();
    p
}

#[test]
fn clean_full_flash_chunked() {
    let dir = scratch("clean");
    let image = make_image(&dir, 0x00, 1024);
    let state = dir.join("flash.state");
    let mut drv = open_mock(&image);

    let region = MemoryRegion::new(0, 1024);
    let data = vec![0xAB; 1024];
    let safety = SafetyConfig {
        chunk_size: 256,
        ..Default::default()
    };

    let report =
        flash_calibration_resumable(&mut drv, region, &data, &safety, &state, &mut noprog).unwrap();

    assert!(report.verified);
    assert_eq!(report.bytes_written, 1024);
    assert_eq!(report.backup.len(), 1024);
    assert!(report.backup.iter().all(|&b| b == 0x00)); // original captured
                                                       // Image now holds the new payload.
    assert_eq!(std::fs::read(&image).unwrap(), data);
    // Checkpoint removed on success; backup sidecar kept.
    assert!(!state.exists(), "checkpoint should be gone after success");
    assert!(state.with_file_name("flash.state.backup").exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn interrupted_then_resume_completes() {
    let dir = scratch("resume");
    let image = make_image(&dir, 0x11, 1024);
    let state = dir.join("flash.state");
    let region = MemoryRegion::new(0, 1024);
    let data: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
    let safety = SafetyConfig {
        chunk_size: 128, // 8 chunks
        ..Default::default()
    };

    // First run: fail after the 3rd chunk write.
    {
        let mut drv = FailAfter::new(open_mock(&image), 3);
        let err =
            flash_calibration_resumable(&mut drv, region, &data, &safety, &state, &mut noprog)
                .unwrap_err();
        let _ = err; // interruption
    }
    // Checkpoint exists and records partial progress.
    assert!(state.exists(), "checkpoint should survive an interruption");
    let ck = std::fs::read_to_string(&state).unwrap();
    assert!(ck.contains("\"verified\": true"));
    assert!(ck.contains("\"verified\": false"));
    // The saved original backup is intact (all 0x11), not the partial image.
    let backup = std::fs::read(state.with_file_name("flash.state.backup")).unwrap();
    assert!(backup.iter().all(|&b| b == 0x11));

    // Second run: a fresh, healthy driver resumes and finishes.
    let mut tr = Transcript::new();
    let mut drv = open_mock(&image);
    let report = flash_calibration_resumable_logged(
        &mut drv,
        region,
        &data,
        &safety,
        &state,
        &mut tr,
        &mut noprog,
    )
    .unwrap();

    assert!(report.verified);
    // Backup returned on resume is still the ORIGINAL, not the partial image.
    assert!(report.backup.iter().all(|&b| b == 0x11));
    assert_eq!(std::fs::read(&image).unwrap(), data);
    assert!(
        !state.exists(),
        "checkpoint removed after successful resume"
    );

    // The resumed transcript shows it resumed and that some chunks were reused
    // (already correct — re-verified rather than re-written).
    let start = tr
        .entries()
        .iter()
        .find_map(|e| match &e.event {
            FlashEvent::SessionStart { resumed, .. } => Some(*resumed),
            _ => None,
        })
        .unwrap();
    assert!(start, "second run must report resumed=true");
    let reused = tr
        .entries()
        .iter()
        .any(|e| matches!(e.event, FlashEvent::ChunkWrite { reused: true, .. }));
    assert!(
        reused,
        "already-written chunks must be reused, not rewritten"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_with_wrong_payload_is_refused() {
    let dir = scratch("mismatch");
    let image = make_image(&dir, 0x22, 512);
    let state = dir.join("flash.state");
    let region = MemoryRegion::new(0, 512);
    let safety = SafetyConfig {
        chunk_size: 128,
        ..Default::default()
    };

    let data_a = vec![0xAA; 512];
    // First run interrupted partway.
    {
        let mut drv = FailAfter::new(open_mock(&image), 2);
        let _ =
            flash_calibration_resumable(&mut drv, region, &data_a, &safety, &state, &mut noprog)
                .unwrap_err();
    }
    assert!(state.exists());

    // Now try to resume the SAME checkpoint with a DIFFERENT payload.
    let data_b = vec![0xBB; 512];
    let mut drv = open_mock(&image);
    let err = flash_calibration_resumable(&mut drv, region, &data_b, &safety, &state, &mut noprog)
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("different payload") || msg.contains("data hash"),
        "expected payload-mismatch refusal, got: {msg}"
    );
    // The checkpoint is left untouched so the original resume is still possible.
    assert!(state.exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn transcript_written_next_to_backup() {
    let dir = scratch("transcript");
    let image = make_image(&dir, 0x00, 300);
    let vault = dir.join("vault");
    let state = dir.join("flash.state");
    let region = MemoryRegion::new(0, 300);
    let data: Vec<u8> = (0..300).map(|i| (i % 255) as u8).collect();
    let safety = SafetyConfig {
        chunk_size: 100, // 3 chunks
        ..Default::default()
    };

    let mut tr = Transcript::new();
    let mut drv = open_mock(&image);
    let id = drv.read_ecu_id().unwrap();
    let report = flash_calibration_resumable_logged(
        &mut drv,
        region,
        &data,
        &safety,
        &state,
        &mut tr,
        &mut noprog,
    )
    .unwrap();

    // Persist the backup to the vault exactly as the CLI would, then drop the
    // transcript next to it.
    let backup_path =
        opentune_flash::vault::save_backup(&vault, &id, region, &report.backup).unwrap();
    let (log, jsonl) = tr.write_next_to(&backup_path).unwrap();
    assert!(log.exists() && jsonl.exists());
    assert_eq!(log.parent(), Some(vault.as_path()));

    // Transcript content: start, voltage, backup, 3 chunk writes, final verify, end.
    let evs: Vec<_> = tr.entries().iter().map(|e| &e.event).collect();
    assert!(matches!(evs.first(), Some(FlashEvent::SessionStart { .. })));
    assert!(evs.iter().any(|e| matches!(
        e,
        FlashEvent::Voltage {
            millivolts: Some(12_600)
        }
    )));
    assert!(evs
        .iter()
        .any(|e| matches!(e, FlashEvent::BackupSaved { .. })));
    assert_eq!(
        evs.iter()
            .filter(|e| matches!(e, FlashEvent::ChunkWrite { .. }))
            .count(),
        3
    );
    assert!(evs
        .iter()
        .any(|e| matches!(e, FlashEvent::FinalVerify { ok: true })));
    assert!(matches!(
        evs.last(),
        Some(FlashEvent::SessionEnd { ok: true, .. })
    ));

    // .jsonl is one parseable object per line; .log mentions the ECU + region.
    let jsonl_body = std::fs::read_to_string(&jsonl).unwrap();
    for line in jsonl_body.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(v.get("event").is_some() && v.get("t_ms").is_some());
    }
    let log_body = std::fs::read_to_string(&log).unwrap();
    assert!(log_body.contains("test-edc16"));
    assert!(log_body.contains("session end ok"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn single_chunk_when_chunk_size_zero() {
    // chunk_size 0 → one chunk spanning the whole region.
    let dir = scratch("single");
    let image = make_image(&dir, 0x00, 64);
    let state = dir.join("flash.state");
    let region = MemoryRegion::new(0, 64);
    let data = vec![0x5A; 64];
    let safety = SafetyConfig {
        chunk_size: 0,
        ..Default::default()
    };
    let mut drv = open_mock(&image);
    let report =
        flash_calibration_resumable(&mut drv, region, &data, &safety, &state, &mut noprog).unwrap();
    assert!(report.verified);
    assert_eq!(std::fs::read(&image).unwrap(), data);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn voltage_gate_still_blocks() {
    let dir = scratch("voltage");
    let image = make_image(&dir, 0x00, 128);
    let state = dir.join("flash.state");
    let region = MemoryRegion::new(0, 128);
    let data = vec![0x01; 128];
    // Mock reports 12_600 mV; demand more than that.
    let safety = SafetyConfig {
        min_millivolts: 13_000,
        chunk_size: 64,
        ..Default::default()
    };
    let mut drv = open_mock(&image);
    let err = flash_calibration_resumable(&mut drv, region, &data, &safety, &state, &mut noprog)
        .unwrap_err();
    assert!(err.to_string().contains("below minimum"));
    // Nothing written; image untouched.
    assert!(std::fs::read(&image).unwrap().iter().all(|&b| b == 0x00));
    let _ = std::fs::remove_dir_all(&dir);
}

// Keep the Phase import used (progress payloads carry it in the pipeline).
#[allow(dead_code)]
fn _phase_marker(p: Phase) -> Phase {
    p
}
