//! End-to-end: plan → engine → simulated C167 BSL + monitor kernel,
//! including the error paths.
use opentune_c167bsl::plan::{build_read_plan, PlanInput, Tx};
use opentune_c167bsl::protocol::{sfr, ChipFamily};
use opentune_c167bsl::{
    find_preset, run_plan, BslError, Exchange, Fault, LinkOptions, Port, ReadOutcome, ReadPlan,
    SimC167, SimConfig,
};
use std::io;
use std::time::Duration;

const LOADER: [u8; 32] = [0x11; 32];

fn kernel(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + 3) as u8).collect()
}

fn image(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (x >> 16) as u8
        })
        .collect()
}

fn plan_for(preset: &str, region: &str, len: u32, kernel: &[u8], setup: bool) -> ReadPlan {
    let p = find_preset(preset).unwrap();
    build_read_plan(&PlanInput {
        preset: p,
        region: p.region(region).unwrap(),
        offset: 0,
        len: Some(len),
        block: 0x200,
        loader: Some(&LOADER),
        loader_sha256: None,
        kernel: Some(kernel),
        setup,
    })
    .unwrap()
}

/// A sim with the EDC15 external flash behind the CS3 window and the
/// internal ROM behind SYSCON's ROM mapping.
fn edc15_sim(cfg: SimConfig, ext: &[u8], int: &[u8]) -> SimC167 {
    SimC167::new(cfg)
        .with_memory(
            0x40_0000,
            ext.to_vec(),
            &[(sfr::ADDRSEL3, 0x4008), (sfr::BUSCON3, 0x848E)],
        )
        .with_memory(0x01_0000, int.to_vec(), &[(sfr::SYSCON, 0xF604)])
}

fn run(
    sim: &mut dyn Port,
    plan: &ReadPlan,
    echo: bool,
) -> (Result<ReadOutcome, BslError>, Vec<Exchange>) {
    let mut t = Vec::new();
    let opts = LinkOptions {
        reply_timeout: Duration::from_millis(5),
        ..LinkOptions::new(57_600, echo)
    };
    let r = run_plan(sim, plan, &opts, &mut t, &mut |_, _| {});
    (r, t)
}

#[test]
fn reads_edc15c6_external_flash_end_to_end() {
    let ext = image(0x10_0000, 1);
    let k = kernel(394);
    let mut sim = edc15_sim(SimConfig::default(), &ext, &[]);
    let plan = plan_for("edc15c6", "ext-flash", 0x10_0000, &k, true);
    let mut seen = 0;
    let mut t = Vec::new();
    let opts = LinkOptions {
        reply_timeout: Duration::from_millis(5),
        ..LinkOptions::new(57_600, true)
    };
    let out = run_plan(&mut sim, &plan, &opts, &mut t, &mut |d, total| {
        assert!(d <= total);
        seen = d;
    })
    .unwrap();
    assert_eq!(out.data, ext);
    assert_eq!(seen, 0x10_0000);
    assert_eq!(out.chip, Some(ChipFamily::C167));
    assert!(!out.kernel_was_running);
    assert_eq!(sim.loader, LOADER);
    assert_eq!(sim.kernel, k);
    assert_eq!(sim.register(sfr::ADDRSEL3), Some(0x4008));
    assert_eq!(sim.register(sfr::SYSCON), Some(0xE204));
    // The engine sent exactly what the plan lists, in order, echo checked.
    assert_eq!(t.len(), plan.steps.len());
    for (ex, step) in t.iter().zip(&plan.steps) {
        assert_eq!(Tx::Bytes(ex.sent.clone()), step.tx, "step {}", step.index);
        assert_eq!(ex.echo, ex.sent);
    }
}

#[test]
fn reads_internal_rom_with_rom_mapping() {
    let int = image(0x8000, 2);
    let mut sim = edc15_sim(SimConfig::default(), &[], &int);
    let plan = plan_for("edc15vm-p", "int-rom", 0x8000, &kernel(394), true);
    let (r, _) = run(&mut sim, &plan, true);
    assert_eq!(r.unwrap().data, int);
    assert_eq!(sim.register(sfr::SYSCON), Some(0xF604));
}

#[test]
fn without_setup_the_unmapped_window_reads_blank() {
    let ext = image(0x400, 3);
    let mut sim = edc15_sim(SimConfig::default(), &ext, &[]);
    let plan = plan_for("edc15c6", "ext-flash", 0x400, &kernel(394), false);
    let (r, _) = run(&mut sim, &plan, true);
    assert!(r.unwrap().data.iter().all(|&b| b == 0xFF));
    assert!(sim.writes.is_empty());
}

#[test]
fn running_kernel_skips_the_upload() {
    let ext = image(0x300, 4);
    let cfg = SimConfig {
        kernel_running: true,
        ..SimConfig::default()
    };
    let mut sim = edc15_sim(cfg, &ext, &[]);
    let plan = plan_for("edc15c6", "ext-flash", 0x300, &kernel(394), true);
    let (r, t) = run(&mut sim, &plan, true);
    let out = r.unwrap();
    assert!(out.kernel_was_running);
    assert_eq!(out.chip, None);
    assert_eq!(out.data, ext);
    assert!(sim.loader.is_empty() && sim.kernel.is_empty());
    assert_eq!(t.len(), plan.steps.len() - 2);
}

#[test]
fn full_duplex_link_without_echo() {
    let ext = image(0x200, 5);
    let cfg = SimConfig {
        echo: false,
        ..SimConfig::default()
    };
    let mut sim = edc15_sim(cfg, &ext, &[]);
    let plan = plan_for("edc15c6", "ext-flash", 0x200, &kernel(394), true);
    let (r, t) = run(&mut sim, &plan, false);
    assert_eq!(r.unwrap().data, ext);
    assert!(t.iter().all(|e| e.echo.is_empty()));
}

#[test]
fn wrong_identification_byte_stops_before_the_loader() {
    for id in [0x55u8, 0x12] {
        let cfg = SimConfig {
            id_byte: id,
            ..SimConfig::default()
        };
        let mut sim = edc15_sim(cfg, &[], &[]);
        let plan = plan_for("edc15c6", "ext-flash", 0x200, &kernel(394), true);
        let (r, t) = run(&mut sim, &plan, true);
        match r.unwrap_err() {
            BslError::WrongId { got, accepted } => {
                assert_eq!(got, id);
                assert_eq!(accepted, [0xA5, 0xC5, 0xD5]);
            }
            e => panic!("{e}"),
        }
        assert_eq!(t.len(), 1);
        assert!(sim.loader.is_empty());
    }
}

#[test]
fn echo_mismatch_is_reported_with_both_sides() {
    // Byte 5 of the session = byte 4 of the loader.
    let cfg = SimConfig {
        fault: Fault::CorruptEcho { at: 5 },
        ..SimConfig::default()
    };
    let mut sim = edc15_sim(cfg, &[], &[]);
    let plan = plan_for("edc15c6", "ext-flash", 0x200, &kernel(394), true);
    let (r, _) = run(&mut sim, &plan, true);
    match r.unwrap_err() {
        BslError::EchoMismatch {
            step, sent, got, ..
        } => {
            assert_eq!(step, 2);
            assert_eq!(sent, LOADER);
            assert_eq!(got[4], 0x11 ^ 0xFF);
        }
        e => panic!("{e}"),
    }
}

#[test]
fn missing_echo_on_a_half_duplex_link_is_caught() {
    // The ECU answers but nothing echoes: the ID byte arrives where the
    // echo of the zero byte should be.
    let cfg = SimConfig {
        echo: false,
        ..SimConfig::default()
    };
    let mut sim = edc15_sim(cfg, &[], &[]);
    let plan = plan_for("edc15c6", "ext-flash", 0x200, &kernel(394), true);
    let (r, _) = run(&mut sim, &plan, true);
    assert!(matches!(
        r.unwrap_err(),
        BslError::EchoMismatch { step: 1, .. }
    ));
}

#[test]
fn silent_line_times_out_at_the_hello() {
    let cfg = SimConfig {
        fault: Fault::SilentFrom { at: 0 },
        ..SimConfig::default()
    };
    let mut sim = edc15_sim(cfg, &[], &[]);
    let plan = plan_for("edc15c6", "ext-flash", 0x200, &kernel(394), true);
    let (r, _) = run(&mut sim, &plan, true);
    let e = r.unwrap_err();
    assert!(matches!(e, BslError::Timeout { step: 1, .. }), "{e}");
}

/// A cable with no ECU behind it (or an ECU not in bootstrap mode): the
/// K-line echoes the zero byte and nothing else ever comes.
struct EchoOnly(Vec<u8>);

impl Port for EchoOnly {
    fn send(&mut self, d: &[u8]) -> io::Result<()> {
        self.0.extend_from_slice(d);
        Ok(())
    }
    fn recv(&mut self, buf: &mut [u8], _t: Duration) -> io::Result<usize> {
        let n = buf.len().min(self.0.len());
        buf[..n].copy_from_slice(&self.0[..n]);
        self.0.drain(..n);
        Ok(n)
    }
    fn discard_input(&mut self) -> io::Result<()> {
        self.0.clear();
        Ok(())
    }
}

#[test]
fn cable_echo_without_a_bsl_names_the_bootstrap_strap() {
    let plan = plan_for("edc15c6", "ext-flash", 0x200, &kernel(394), true);
    let (r, t) = run(&mut EchoOnly(Vec::new()), &plan, true);
    let e = r.unwrap_err();
    assert!(matches!(e, BslError::Timeout { step: 1, .. }));
    assert!(e.to_string().contains("P0L.4"), "{e}");
    assert_eq!(t[0].echo, [0x00]);
}

#[test]
fn loader_that_never_reports_times_out() {
    let cfg = SimConfig {
        fault: Fault::LoaderSilent,
        ..SimConfig::default()
    };
    let mut sim = edc15_sim(cfg, &[], &[]);
    let plan = plan_for("edc15c6", "ext-flash", 0x200, &kernel(394), true);
    let (r, _) = run(&mut sim, &plan, true);
    let e = r.unwrap_err();
    assert!(matches!(e, BslError::Timeout { step: 2, .. }), "{e}");
}

#[test]
fn short_kernel_times_out_waiting_for_started() {
    let mut sim = edc15_sim(SimConfig::default(), &[], &[]);
    let plan = plan_for("edc15c6", "ext-flash", 0x200, &kernel(100), true);
    let (r, _) = run(&mut sim, &plan, true);
    let e = r.unwrap_err();
    assert!(matches!(e, BslError::Timeout { step: 3, .. }), "{e}");
    assert!(e.to_string().contains("length"));
}

#[test]
fn bad_block_checksum_stops_the_read() {
    let ext = image(0x600, 6);
    let cfg = SimConfig {
        fault: Fault::BadChecksum { block: 1 },
        ..SimConfig::default()
    };
    let mut sim = edc15_sim(cfg, &ext, &[]);
    let plan = plan_for("edc15c6", "ext-flash", 0x600, &kernel(394), true);
    let (r, _) = run(&mut sim, &plan, true);
    match r.unwrap_err() {
        BslError::ChecksumMismatch {
            addr,
            computed,
            reported,
        } => {
            assert_eq!(addr, 0x40_0200);
            assert_eq!(computed ^ reported, 0x5A);
        }
        e => panic!("{e}"),
    }
}

#[test]
fn nak_and_bad_block_end_are_unexpected_replies() {
    // Command 0 is the test-communication; command 1 the first write-word.
    for fault in [
        Fault::NakCommand { nth: 1 },
        Fault::BadBlockAck { block: 0 },
    ] {
        let cfg = SimConfig {
            fault,
            ..SimConfig::default()
        };
        let mut sim = edc15_sim(cfg, &image(0x200, 7), &[]);
        let plan = plan_for("edc15c6", "ext-flash", 0x200, &kernel(394), true);
        let (r, _) = run(&mut sim, &plan, true);
        let e = r.unwrap_err();
        assert!(
            matches!(e, BslError::UnexpectedReply { .. }),
            "{fault:?}: {e}"
        );
        let nak = matches!(fault, Fault::NakCommand { .. });
        assert_eq!(e.to_string().contains("NAK"), nak, "{e}");
    }
}

#[test]
fn register_that_does_not_take_the_value_fails_setup() {
    let cfg = SimConfig {
        fault: Fault::StuckRegister {
            addr: sfr::ADDRSEL3,
        },
        ..SimConfig::default()
    };
    let mut sim = edc15_sim(cfg, &[], &[]);
    let plan = plan_for("edc15c6", "ext-flash", 0x200, &kernel(394), true);
    let (r, _) = run(&mut sim, &plan, true);
    match r.unwrap_err() {
        BslError::SetupReadback {
            reg, wrote, read, ..
        } => {
            assert_eq!(reg, "ADDRSEL3");
            assert_eq!(wrote, 0x4008);
            assert_eq!(read, 0xFFFF);
        }
        e => panic!("{e}"),
    }
}

#[test]
fn dry_run_plan_cannot_be_executed() {
    let p = find_preset("c167").unwrap();
    let plan = build_read_plan(&PlanInput {
        preset: p,
        region: p.region("int-rom").unwrap(),
        offset: 0,
        len: Some(0x10),
        block: 0x10,
        loader: None,
        loader_sha256: None,
        kernel: None,
        setup: true,
    })
    .unwrap();
    let mut sim = edc15_sim(SimConfig::default(), &[], &[]);
    let (r, _) = run(&mut sim, &plan, true);
    assert!(matches!(
        r.unwrap_err(),
        BslError::MissingPayload { step: 2, .. }
    ));
}

#[test]
fn egs52_requires_complete_operator_setup_before_io() {
    use opentune_c167bsl::plan::build_with_registers;
    let preset = find_preset("egs52").unwrap();
    let region = &preset.regions[0];
    assert_eq!(
        (region.base, region.max_len, region.default_len),
        (0x80000, 0x80000, 0x80000)
    );
    let k = kernel(394);
    let mut input = PlanInput {
        preset,
        region,
        offset: region.max_len - 16,
        len: Some(16),
        block: 16,
        loader: Some(&LOADER),
        loader_sha256: None,
        kernel: Some(&k),
        setup: true,
    };
    // Arbitrary simulator-only sentinels, unrelated to board settings.
    let values: Vec<_> = region
        .unknown_setup
        .iter()
        .enumerate()
        .map(|(i, &(name, _))| (name, i as u16 + 1))
        .collect();
    let requires: Vec<_> = region
        .unknown_setup
        .iter()
        .zip(&values)
        .map(|((_, addr), (_, value))| (*addr, *value))
        .collect();
    let data = image(16, 3);
    let mut sim = SimC167::new(SimConfig::default()).with_memory(0xffff0, data.clone(), &requires);
    let incomplete = build_with_registers(&input, &values[..9]).unwrap();
    let (err, transcript) = run(&mut sim, &incomplete, true);
    assert!(err.unwrap_err().to_string().contains("BUSCON4"));
    assert!(transcript.is_empty());
    assert!(build_with_registers(&input, &[values[0], values[0]]).is_err());
    assert!(build_with_registers(&input, &[("BOGUS", 0)]).is_err());
    let complete = build_with_registers(&input, &values).unwrap();
    assert_eq!(run(&mut sim, &complete, true).0.unwrap().data, data);
    input.setup = false;
    assert!(build_read_plan(&input).is_err());
    input.setup = true;
    input.len = Some(17);
    assert!(build_read_plan(&input).is_err());
}
