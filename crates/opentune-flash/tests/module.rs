//! UDS module flashing end to end against the simulated flashable ECU
//! (`opentune-udssim`), driven from a synthetic VBF container.
use opentune_container::vbf::crc16_ccitt_false;
use opentune_container::Keys;
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::{Error, MemoryRegion};
use opentune_flash::module::{
    describe_request, flash_module, plan_module_flash, BlockEncoding, BlockStatus, ModuleBlock,
    ModuleFlashOptions, ModuleFlashReport, ModuleImage, ModuleState, VerifyMethod, Voltage,
};
use opentune_flash::FlashSafetyPolicy;
use opentune_udssim::{FlashConfig, FlashRegion, SimTransport, UdsServer};
use std::path::PathBuf;

/// The simulator's default seed→key relationship (`seed XOR 0xA5`) — a test
/// hook, not any manufacturer's algorithm.
struct TestKey;
impl SeedKey for TestKey {
    fn id(&self) -> &str {
        "test-xor-a5"
    }
    fn compute(&self, seed: &[u8], _level: u8) -> opentune_core::Result<Vec<u8>> {
        Ok(seed.iter().map(|b| b ^ 0xA5).collect())
    }
}

const R0: u32 = 0x0001_0000;
const R1: u32 = 0x0002_0000;
const PART: &str = "5Q0907000A";

fn stock(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(7) ^ seed).collect()
}

fn new_data(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_add(seed)).collect()
}

/// A synthetic VBF: two blocks, each inside its own erase region.
fn vbf() -> Vec<u8> {
    let mut v = format!(
        "vbf_version = 2.6;\nheader {{\n  sw_part_number = \"TEST-14C000-AA\";\n  \
         sw_part_type = EXE;\n  erase = {{ {{ 0x{R0:08X}, 0x00000400 }}, {{ 0x{R1:08X}, 0x00000200 }} }};\n}}\n"
    )
    .into_bytes();
    for (addr, data) in [(R0, new_data(0x300, 0x10)), (R1, new_data(0x150, 0x80))] {
        v.extend_from_slice(&addr.to_be_bytes());
        v.extend_from_slice(&(data.len() as u32).to_be_bytes());
        v.extend_from_slice(&data);
        v.extend_from_slice(&crc16_ccitt_false(&data).to_be_bytes());
    }
    v
}

fn image() -> ModuleImage {
    let mut img = ModuleImage::from_container_bytes(&vbf(), &Keys::default()).unwrap();
    img.identity.part_numbers = vec!["5Q0907000".into()];
    img
}

fn flash_config() -> FlashConfig {
    FlashConfig {
        regions: vec![
            FlashRegion::new(R0, stock(0x400, 0x11)),
            FlashRegion::new(R1, stock(0x200, 0x22)),
        ],
        // 0x82 → 128 data bytes per TransferData.
        max_block_len: 0x82,
        ..FlashConfig::default()
    }
}

fn ecu_with(cfg: FlashConfig) -> SimTransport {
    let mut s = UdsServer::new();
    s.set_did(0xF187, PART);
    s.set_did(0xF188, "0001");
    s.enable_flash(cfg);
    SimTransport::new(s)
}

fn ecu() -> SimTransport {
    ecu_with(flash_config())
}

fn opts() -> ModuleFlashOptions {
    ModuleFlashOptions::new(FlashSafetyPolicy::production())
}

const VOLTS: Voltage = Voltage {
    millivolts: Some(12_800),
    capable: true,
};

fn run(
    t: &mut SimTransport,
    img: &ModuleImage,
    o: &ModuleFlashOptions,
    key: bool,
) -> opentune_core::Result<ModuleFlashReport> {
    let sk = TestKey;
    let sk: Option<&dyn SeedKey> = if key { Some(&sk) } else { None };
    flash_module(t, img, VOLTS, sk, o, &mut |_| {})
}

fn mem(t: &SimTransport, addr: u32, len: usize) -> Vec<u8> {
    t.server().flash().unwrap().read(addr, len).unwrap()
}

/// How many requests started with `prefix`.
fn sent(t: &SimTransport, prefix: &[u8]) -> usize {
    t.trace()
        .requests()
        .iter()
        .filter(|r| r.starts_with(prefix))
        .count()
}

fn state_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("opentune-module-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("flash.state.json")
}

fn assert_programmed(t: &SimTransport) {
    assert_eq!(mem(t, R0, 0x300), new_data(0x300, 0x10));
    assert_eq!(mem(t, R0 + 0x300, 0x100), vec![0xFF; 0x100], "erased tail");
    assert_eq!(mem(t, R1, 0x150), new_data(0x150, 0x80));
    assert!(t.server().flash().unwrap().is_valid());
}

#[test]
fn happy_path_from_a_synthetic_vbf() {
    let mut t = ecu();
    let img = image();
    assert_eq!(img.blocks.len(), 2);
    assert!(img.blocks.iter().all(|b| b.erase_declared));
    let mut phases = Vec::new();
    let sk = TestKey;
    let rep = flash_module(&mut t, &img, VOLTS, Some(&sk), &opts(), &mut |p| {
        phases.push(p.phase)
    })
    .unwrap();
    assert_programmed(&t);
    // The backup is the stock content of both erase regions.
    assert_eq!(rep.backup.len(), 2);
    assert_eq!(rep.backup[0].data, stock(0x400, 0x11));
    assert_eq!(rep.backup[1].region, MemoryRegion::new(R1, 0x200));
    assert_eq!(rep.dependencies_ok, Some(true));
    assert!(rep.reset_sent && !rep.resumed && !rep.identity_forced);
    assert!(rep
        .blocks
        .iter()
        .all(|b| b.verify == VerifyMethod::ReadBack && !b.reused));
    assert!(phases.contains(&opentune_flash::module::ModulePhase::Erase));
    // Sequence landmarks: programming session, unlock, 2 erases, reset last.
    assert_eq!(sent(&t, &[0x10, 0x02]), 1);
    assert_eq!(sent(&t, &[0x27, 0x12]), 1);
    assert_eq!(sent(&t, &[0x31, 0x01, 0xFF, 0x00]), 2);
    assert_eq!(sent(&t, &[0x31, 0x01, 0x02, 0x02]), 2);
    assert_eq!(t.trace().requests().last().unwrap(), &[0x11, 0x01]);
    // 0x300 / 128 + 0x150 / 128 (rounded up) TransferData blocks.
    assert_eq!(sent(&t, &[0x36]), 6 + 3);
    assert!(t
        .trace()
        .requests()
        .iter()
        .filter(|r| r[0] == 0x36)
        .all(|r| r.len() <= 0x82));
    assert_eq!(t.server().session(), 0x01, "reset back to default");
}

#[test]
fn wrong_ecu_is_refused_before_any_write() {
    let mut s = UdsServer::new();
    s.set_did(0xF187, "8E0910155");
    s.set_did(0xF188, "0001");
    s.enable_flash(flash_config());
    let mut t = SimTransport::new(s);
    let err = run(&mut t, &image(), &opts(), true).unwrap_err();
    assert!(
        matches!(&err, Error::Safety(m) if m.contains("does not match")),
        "{err}"
    );
    assert_eq!(sent(&t, &[0x31]) + sent(&t, &[0x34]) + sent(&t, &[0x10]), 0);
    assert_eq!(mem(&t, R0, 0x400), stock(0x400, 0x11));

    // Forcing it is possible, and recorded.
    let mut o = opts();
    o.force_identity = true;
    let rep = run(&mut t, &image(), &o, true).unwrap();
    assert!(rep.identity_forced);
    assert!(rep.log.iter().any(|l| l.contains("OVERRIDDEN")));
}

#[test]
fn missing_identity_in_the_container_is_refused_under_production() {
    let mut t = ecu();
    let img = ModuleImage::from_container_bytes(&vbf(), &Keys::default()).unwrap();
    let err = run(&mut t, &img, &opts(), true).unwrap_err();
    assert!(err.to_string().contains("identity"), "{err}");
    assert_eq!(t.request_count(), 0, "refused before touching the ECU");
}

#[test]
fn missing_security_is_refused_before_erase() {
    let mut t = ecu();
    let err = run(&mut t, &image(), &opts(), false).unwrap_err();
    assert!(
        matches!(&err, Error::Safety(m) if m.contains("no seed-key")),
        "{err}"
    );
    assert_eq!(sent(&t, &[0x27, 0x11]), 1, "the seed was requested");
    assert_eq!(sent(&t, &[0x31]) + sent(&t, &[0x34]), 0, "nothing erased");
    assert_eq!(mem(&t, R0, 0x400), stock(0x400, 0x11));
    assert_eq!(t.server().flash().unwrap().stats().erases, 0);
}

#[test]
fn power_loss_mid_block_then_resume_completes() {
    let mut cfg = flash_config();
    // Block 0 takes 6 TransferData; lose power after the 2nd one of block 1.
    cfg.faults.power_loss_after_blocks = Some(8);
    let mut t = ecu_with(cfg);
    let path = state_path("power");
    let mut o = opts();
    o.state_path = Some(path.clone());

    let err = run(&mut t, &image(), &o, true).unwrap_err();
    assert!(!matches!(err, Error::Safety(_)), "{err}");
    let flash = t.server().flash().unwrap();
    assert_eq!(flash.stats().power_losses, 1);
    assert!(
        !flash.is_valid(),
        "interrupted block leaves the ECU invalid"
    );
    assert_eq!(t.server().session(), 0x01, "the ECU rebooted");
    let st = ModuleState::load(&path).unwrap().expect("state kept");
    assert_eq!(st.blocks[0].status, BlockStatus::Done);
    assert_eq!(st.blocks[1].status, BlockStatus::Failed);
    assert!(st.backup_saved);
    assert!(st.events.iter().any(|e| e.message.contains("ABORTED")));

    let before = sent(&t, &[0x31, 0x01, 0xFF, 0x00]);
    let rep = run(&mut t, &image(), &o, true).unwrap();
    assert!(rep.resumed);
    assert!(rep.blocks[0].reused && !rep.blocks[1].reused);
    // The backup is the original, not the half-written state.
    assert_eq!(rep.backup[1].data, stock(0x200, 0x22));
    // Only the failed erase region was erased again.
    assert_eq!(sent(&t, &[0x31, 0x01, 0xFF, 0x00]) - before, 1);
    assert_programmed(&t);
    assert!(!path.exists(), "state removed after success");
}

#[test]
fn resume_refuses_a_different_image() {
    let mut cfg = flash_config();
    cfg.faults.power_loss_after_blocks = Some(2);
    let mut t = ecu_with(cfg);
    let path = state_path("otherimg");
    let mut o = opts();
    o.state_path = Some(path);
    run(&mut t, &image(), &o, true).unwrap_err();
    let mut other = image();
    other.blocks[1].data[0] ^= 1;
    other.blocks[1].container_crc_ok = None;
    let err = run(&mut t, &other, &o, true).unwrap_err();
    assert!(err.to_string().contains("different image"), "{err}");
}

#[test]
fn lost_transfer_response_is_retransmitted_with_the_same_counter() {
    let mut cfg = flash_config();
    cfg.faults.drop_transfer_response_at = Some(3);
    let mut t = ecu_with(cfg);
    let rep = run(&mut t, &image(), &opts(), true).unwrap();
    let stats = t.server().flash().unwrap().stats().clone();
    assert_eq!(stats.duplicate_blocks, 1, "ECU acknowledged the repeat");
    assert_eq!(stats.blocks_written, 9, "the repeat was not written twice");
    // Counter 0x03 twice in block 0 (original + retransmit), once in block 1.
    assert_eq!(sent(&t, &[0x36, 0x03]), 3);
    assert!(rep.log.iter().any(|l| l.contains("retransmitting")));
    assert_programmed(&t);
}

#[test]
fn wrong_block_counter_stops_the_block_and_resume_recovers() {
    let mut t = ecu();
    // The ECU answers wrongBlockSequenceCounter to the 4th TransferData.
    t.faults().schedule(
        Some(0x36),
        3,
        opentune_udssim::Fault::NegativeResponse(0x73),
    );
    let path = state_path("counter");
    let mut o = opts();
    o.state_path = Some(path.clone());
    let err = run(&mut t, &image(), &o, true).unwrap_err();
    assert!(
        matches!(err, Error::NegativeResponse { code: 0x73, .. }),
        "{err}"
    );
    assert_eq!(sent(&t, &[0x37]), 0, "no transfer exit after the refusal");
    let st = ModuleState::load(&path).unwrap().unwrap();
    assert_eq!(st.blocks[0].status, BlockStatus::Failed);

    // Re-running from the state file: the session change aborts the ECU's
    // half-open download, the block is erased and written again.
    let rep = run(&mut t, &image(), &o, true).unwrap();
    assert!(rep.resumed && !rep.blocks[0].reused);
    assert_programmed(&t);
}

#[test]
fn response_pending_during_erase_is_drained_not_resent() {
    let mut cfg = flash_config();
    cfg.erase_pending = 3;
    let mut t = ecu_with(cfg);
    run(&mut t, &image(), &opts(), true).unwrap();
    assert_eq!(t.trace().pending_count(), 6, "3 per erase");
    assert_eq!(
        sent(&t, &[0x31, 0x01, 0xFF, 0x00]),
        2,
        "each erase sent once"
    );
    assert_programmed(&t);
}

#[test]
fn checksum_routine_failure_stops_before_reset() {
    let mut cfg = flash_config();
    cfg.faults.fail_check = true;
    let mut t = ecu_with(cfg);
    let err = run(&mut t, &image(), &opts(), true).unwrap_err();
    assert!(
        matches!(&err, Error::Verify(m) if m.contains("checksum routine")),
        "{err}"
    );
    assert_eq!(sent(&t, &[0x11]), 0, "no reset");
    assert_eq!(
        sent(&t, &[0x31, 0x01, 0xFF, 0x01]),
        0,
        "no dependency check"
    );
    assert_eq!(sent(&t, &[0x34]), 1, "stopped at the first block");
}

#[test]
fn corrupted_write_fails_read_back_before_the_checksum_routine() {
    let mut cfg = flash_config();
    cfg.faults.corrupt_block_at = Some(2);
    let mut t = ecu_with(cfg);
    let err = run(&mut t, &image(), &opts(), true).unwrap_err();
    assert!(err.to_string().contains("read-back"), "{err}");
    assert_eq!(sent(&t, &[0x31, 0x01, 0x02, 0x02]), 0);
    assert_eq!(sent(&t, &[0x11]), 0);
}

#[test]
fn erase_refused_leaves_memory_untouched() {
    let mut cfg = flash_config();
    cfg.faults.refuse_erase = Some(0x22);
    let mut t = ecu_with(cfg);
    let err = run(&mut t, &image(), &opts(), true).unwrap_err();
    assert!(
        matches!(err, Error::NegativeResponse { code: 0x22, .. }),
        "{err}"
    );
    assert_eq!(sent(&t, &[0x34]), 0);
    assert_eq!(mem(&t, R0, 0x400), stock(0x400, 0x11));
}

#[test]
fn unreadable_ecu_needs_an_explicit_no_backup_override() {
    let mut cfg = flash_config();
    cfg.readable = false;
    let mut t = ecu_with(cfg.clone());
    let err = run(&mut t, &image(), &opts(), true).unwrap_err();
    assert!(err.to_string().contains("no backup"), "{err}");
    assert_eq!(sent(&t, &[0x31]), 0, "refused before erase");

    let mut t = ecu_with(cfg);
    let mut o = opts();
    o.allow_no_backup = true;
    let rep = run(&mut t, &image(), &o, true).unwrap();
    assert!(rep.backup.is_empty() && rep.backup_note.is_some());
    assert!(rep
        .blocks
        .iter()
        .all(|b| b.verify == VerifyMethod::EcuRoutine));
    assert!(t.server().flash().unwrap().is_valid());
}

#[test]
fn programming_preconditions_refused() {
    let mut cfg = flash_config();
    cfg.preconditions_ok = false;
    let mut t = ecu_with(cfg);
    let err = run(&mut t, &image(), &opts(), true).unwrap_err();
    assert!(
        matches!(
            err,
            Error::NegativeResponse {
                service: 0x10,
                code: 0x22,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(sent(&t, &[0x31]), 0);
}

#[test]
fn low_voltage_is_refused_before_the_session_change() {
    let mut t = ecu();
    let sk = TestKey;
    let low = Voltage {
        millivolts: Some(11_000),
        capable: true,
    };
    let err = flash_module(&mut t, &image(), low, Some(&sk), &opts(), &mut |_| {}).unwrap_err();
    assert!(err.to_string().contains("below minimum"), "{err}");
    assert_eq!(sent(&t, &[0x10]), 0);
}

#[test]
fn compressed_and_encrypted_blocks_are_refused_by_name() {
    let mut img = image();
    img.blocks[0].encoding = BlockEncoding::Compressed("ODX method 'A0', Lzss10".into());
    img.blocks[1].encoding = BlockEncoding::Encrypted("ODX method '0A'".into());
    let mut t = ecu();
    let err = run(&mut t, &img, &opts(), true).unwrap_err().to_string();
    assert!(
        err.contains("compressed on the wire") && err.contains("Lzss10"),
        "{err}"
    );
    assert!(err.contains("encrypted on the wire"), "{err}");
    assert_eq!(t.request_count(), 0);
    // A block filter that leaves only bad blocks is still refused; the plan too.
    assert!(plan_module_flash(&img, &opts(), 0x82).is_err());
}

#[test]
fn block_filter_flashes_only_the_selected_block() {
    let mut t = ecu();
    let mut o = opts();
    o.blocks = Some(vec![1]);
    let rep = run(&mut t, &image(), &o, true).unwrap();
    assert_eq!(rep.blocks.len(), 1);
    assert_eq!(rep.blocks[0].index, 1);
    assert_eq!(mem(&t, R0, 0x400), stock(0x400, 0x11), "block 0 untouched");
    assert_eq!(mem(&t, R1, 0x150), new_data(0x150, 0x80));
    o.blocks = Some(vec![7]);
    assert!(run(&mut ecu(), &image(), &o, true).is_err());
}

#[test]
fn plain_bin_with_layout() {
    let mut file = stock(0x10000 + 0x200, 0x33);
    file[..0x400].copy_from_slice(&new_data(0x400, 0x01));
    let mut img = ModuleImage::from_bin(
        &file,
        R0,
        &[MemoryRegion::new(R0, 0x400), MemoryRegion::new(R1, 0x200)],
    )
    .unwrap();
    img.identity.sw_numbers = vec!["0001".into()];
    let mut t = ecu();
    run(&mut t, &img, &opts(), true).unwrap();
    assert_eq!(mem(&t, R0, 0x400), new_data(0x400, 0x01));
    assert_eq!(mem(&t, R1, 0x200), file[0x10000..].to_vec());
}

#[test]
fn plan_matches_the_real_request_sequence() {
    let img = image();
    let plan = plan_module_flash(&img, &opts(), 0x82).unwrap();
    let mut t = ecu();
    run(&mut t, &img, &opts(), true).unwrap();
    let real: Vec<String> = t
        .trace()
        .requests()
        .iter()
        .map(|r| describe_request(r))
        .collect();
    assert_eq!(plan.lines(), real);
    assert!(plan.lines().iter().any(|l| l == "27 12 <key>"));
    assert!(plan.lines().iter().any(|l| l == "36 01 <128 bytes>"));
}

#[test]
fn plain_block_constructor_is_flashable() {
    let b = ModuleBlock::plain("x", R0, vec![1, 2, 3]);
    assert!(b.refusal().is_none());
}
