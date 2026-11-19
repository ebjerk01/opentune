//! Subaru SSM2 logging end to end against the simulated module in
//! `opentune_core::protocol::ssmsim`: init and capability selection, batched
//! `A8` reads, transport faults, and a CSV run. Synthetic ROM IDs and
//! addresses only (tests/fixtures/ssm_demo_logger.xml).
use opentune_core::protocol::ssm::{SsmKLine, ECU, TCU};
use opentune_core::protocol::ssmsim::{demo, SsmSim, SsmSimConfig};
use opentune_core::CoreDiag;
use opentune_logdef::{
    parse, ssm, Addressing, FastLogConfig, LiveSession, LogDef, LogMode, Sample, SsmPollConfig,
    SsmTarget, CSV_HEADER,
};

fn demo_def() -> LogDef {
    parse(include_str!("fixtures/ssm_demo_logger.xml")).unwrap()
}

fn ssm_cfg(max_packet_len: usize) -> FastLogConfig {
    FastLogConfig {
        ssm: Some(SsmPollConfig { max_packet_len }),
        ..FastLogConfig::default()
    }
}

/// A demo module with fixed RAM: coolant 90 °C, 800 rpm, 50 km/h, switch on,
/// ROM float 1.5.
fn static_sim() -> SsmSim {
    let mut cfg = SsmSimConfig::demo();
    cfg.animate = false;
    let ram = &mut cfg.ram;
    ram.insert(demo::COOLANT, 130);
    ram.insert(demo::RPM, 0x0C);
    ram.insert(demo::RPM + 1, 0x80);
    ram.insert(demo::SPEED, 50);
    ram.insert(demo::SWITCHES, 0b0000_1000);
    for (i, b) in 1.5f32.to_be_bytes().into_iter().enumerate() {
        ram.insert(demo::ROM_FLOAT + i as u32, b);
    }
    SsmSim::new(cfg)
}

fn ids(def: &LogDef) -> Vec<String> {
    def.params().map(|(_, _, p)| p.id.clone()).collect()
}

fn value(samples: &[Sample], id: &str) -> Option<f64> {
    samples.iter().find(|s| s.param_id == id).unwrap().value
}

/// Identify, select and tick `ticks` times over a K-line to `sim`.
fn log_kline(
    sim: &mut SsmSim,
    max_packet_len: usize,
    ticks: usize,
) -> (LiveSession, Vec<Vec<Sample>>) {
    let mut kl = SsmKLine::open(sim, ECU).unwrap();
    let mut diag = CoreDiag::new(&mut kl, 200);
    let init = ssm::identify(&mut diag).unwrap();
    let sel = ssm::select(&demo_def(), &init, SsmTarget::Ecu);
    let mut session = LiveSession::new(&sel.def, &[], ssm_cfg(max_packet_len)).unwrap();
    assert_eq!(session.start(&mut diag).mode, LogMode::Ssm);
    let rounds = (0..ticks).map(|_| session.tick(&mut diag)).collect();
    (session, rounds)
}

#[test]
fn init_selects_by_capability_rom_id_and_target() {
    let mut sim = SsmSim::demo();
    let mut kl = SsmKLine::open(&mut sim, ECU).unwrap();
    let mut diag = CoreDiag::new(&mut kl, 200);
    let init = ssm::identify(&mut diag).unwrap();
    assert_eq!(init.rom_id_hex(), "5A5A000001");

    let sel = ssm::select(&demo_def(), &init, SsmTarget::Ecu);
    assert_eq!(
        ids(&sel.def),
        ["P_ECT", "P_RPM", "P_VSS", "S_DEMO", "E_FLOAT"]
    );
    let notes = sel.notes.join("\n");
    assert!(notes.contains("capability bit clear): P_NOPE"), "{notes}");
    assert!(notes.contains("other module (target): P_TCU"), "{notes}");
    assert!(notes.contains("not evaluated): P_DERIVED"), "{notes}");
    assert!(
        notes.contains("1 per-ROM parameter(s) for other ROM IDs"),
        "{notes}"
    );
    // The per-ROM address of this ROM ID, not the other one.
    let e = sel
        .def
        .params()
        .find(|(_, _, p)| p.id == "E_FLOAT")
        .unwrap()
        .2;
    assert!(matches!(
        e.addressing,
        Addressing::Memory {
            address: 0xFF80,
            length: 4,
            ..
        }
    ));

    // The transmission sees its own channel set.
    let tcu = ssm::select(&demo_def(), &init, SsmTarget::Tcu);
    assert_eq!(ids(&tcu.def), ["P_RPM", "P_VSS", "P_TCU"]);
}

#[test]
fn one_batched_read_decodes_every_channel() {
    let mut sim = static_sim();
    let (session, rounds) = log_kline(&mut sim, 80, 1);
    let s = &rounds[0];
    assert!(
        s.iter().all(|x| x.note.is_empty() || x.note == "bit 3"),
        "{s:?}"
    );
    assert_eq!(value(s, "P_ECT"), Some(90.0));
    assert_eq!(value(s, "P_RPM"), Some(800.0));
    assert_eq!(value(s, "P_VSS"), Some(50.0));
    assert_eq!(value(s, "S_DEMO"), Some(1.0));
    assert_eq!(value(s, "E_FLOAT"), Some(1.5));
    let st = session.status();
    assert_eq!(st.tick_requests, 1, "9 addresses fit one A8");
    assert_eq!(st.setup_requests, 0);
    // BF, then a single A8 with every byte address once.
    let a8 = sim.requests.last().unwrap();
    assert_eq!(a8[..2], [0xA8, 0x00]);
    assert_eq!(a8.len(), 2 + 3 * 9);
    assert!(sim.writes.is_empty() && !sim.requests.iter().any(|r| r[0] == 0xB8));
}

#[test]
fn reads_split_under_the_packet_limit_without_tearing_a_channel() {
    let mut sim = static_sim();
    // 16-byte packets carry 3 addresses per A8.
    let (session, rounds) = log_kline(&mut sim, 16, 1);
    assert_eq!(value(&rounds[0], "P_RPM"), Some(800.0));
    assert_eq!(value(&rounds[0], "E_FLOAT"), Some(1.5));
    let st = session.status();
    assert_eq!(st.tick_requests, 3);
    assert!(
        st.notes
            .iter()
            .any(|n| n.contains("E_FLOAT: 4 bytes exceed one A8")),
        "{:?}",
        st.notes
    );
    let reads: Vec<&Vec<u8>> = sim.requests.iter().filter(|r| r[0] == 0xA8).collect();
    assert_eq!(reads.len(), 3);
    assert!(reads.iter().all(|r| r.len() <= 2 + 3 * 3));
    // Both engine-speed bytes ride in the same request.
    let rpm_hi = [0x00, 0x10, 0x01];
    let rpm_lo = [0x00, 0x10, 0x02];
    let with = |a: &[u8; 3]| reads.iter().position(|r| r[2..].chunks(3).any(|c| c == a));
    assert_eq!(with(&rpm_hi), with(&rpm_lo));
}

#[test]
fn a_bad_reply_checksum_is_noted_and_the_next_sample_recovers() {
    let mut sim = static_sim();
    sim.faults.bad_checksums = 1;
    let (_, rounds) = log_kline_no_init_fault(&mut sim);
    assert!(
        rounds[0].iter().all(|s| s.note.contains("checksum")),
        "{:?}",
        rounds[0]
    );
    assert_eq!(value(&rounds[1], "P_ECT"), Some(90.0));
}

/// Like [`log_kline`] with two ticks, but the faults set on `sim` apply to
/// the ticks only (the init runs on a clean copy of the counters).
fn log_kline_no_init_fault(sim: &mut SsmSim) -> (LiveSession, Vec<Vec<Sample>>) {
    let faults = std::mem::take(&mut sim.faults);
    let init = {
        let mut kl = SsmKLine::open(sim, ECU).unwrap();
        ssm::identify(&mut CoreDiag::new(&mut kl, 200)).unwrap()
    };
    sim.faults = faults;
    let sel = ssm::select(&demo_def(), &init, SsmTarget::Ecu);
    let mut kl = SsmKLine::open(sim, ECU).unwrap();
    let mut diag = CoreDiag::new(&mut kl, 200);
    let mut session = LiveSession::new(&sel.def, &[], ssm_cfg(80)).unwrap();
    session.start(&mut diag);
    let rounds = (0..2).map(|_| session.tick(&mut diag)).collect();
    (session, rounds)
}

#[test]
fn an_echo_mismatch_is_noted() {
    let mut sim = static_sim();
    sim.faults.bad_echoes = 1;
    let (_, rounds) = log_kline_no_init_fault(&mut sim);
    assert!(
        rounds[0].iter().all(|s| s.note.contains("echo mismatch")),
        "{:?}",
        rounds[0]
    );
    assert_eq!(value(&rounds[1], "P_VSS"), Some(50.0));
}

#[test]
fn a_silent_module_times_out() {
    let mut sim = static_sim();
    sim.faults.silent = 1;
    let (_, rounds) = log_kline_no_init_fault(&mut sim);
    assert!(
        rounds[0].iter().all(|s| s.note.contains("timed out")),
        "{:?}",
        rounds[0]
    );
    assert!(rounds[1].iter().all(|s| s.answered()));
}

#[test]
fn an_adapter_that_strips_the_echo_works_too() {
    let mut sim = static_sim();
    sim.cfg.echo = false;
    let (_, rounds) = log_kline(&mut sim, 80, 1);
    assert_eq!(value(&rounds[0], "P_RPM"), Some(800.0));
}

#[test]
fn ssm_over_can_reports_a_negative_response() {
    // SSM over ISO-TP: the sim is the payload-level transport itself.
    let mut sim = static_sim();
    sim.faults.nrc = Some((0xA8, 0x22));
    let mut session = {
        let mut diag = CoreDiag::new(&mut sim, 100);
        let init = ssm::identify(&mut diag).unwrap();
        let sel = ssm::select(&demo_def(), &init, SsmTarget::Ecu);
        let mut session = LiveSession::new(&sel.def, &[], ssm_cfg(80)).unwrap();
        session.start(&mut diag);
        let s = session.tick(&mut diag);
        assert!(
            s.iter().all(|x| x
                .note
                .contains("negative response 0x22 (conditionsNotCorrect)")),
            "{s:?}"
        );
        session
    };
    sim.faults.nrc = None;
    let mut diag = CoreDiag::new(&mut sim, 100);
    assert_eq!(value(&session.tick(&mut diag), "P_ECT"), Some(90.0));
}

#[test]
fn the_transmission_is_addressed_separately() {
    let mut cfg = SsmSimConfig::demo();
    cfg.address = TCU;
    let mut sim = SsmSim::new(cfg);
    {
        // The engine address gets no answer from a TCU-only bus.
        let mut kl = SsmKLine::open(&mut sim, ECU).unwrap();
        let mut diag = CoreDiag::new(&mut kl, 50);
        assert!(ssm::identify(&mut diag).unwrap_err().contains("timed out"));
    }
    let mut kl = SsmKLine::open(&mut sim, SsmTarget::Tcu.address()).unwrap();
    let mut diag = CoreDiag::new(&mut kl, 200);
    let init = ssm::identify(&mut diag).unwrap();
    let sel = ssm::select(&demo_def(), &init, SsmTarget::Tcu);
    let mut session = LiveSession::new(&sel.def, &[], ssm_cfg(80)).unwrap();
    session.start(&mut diag);
    assert!(session.tick(&mut diag).iter().all(|s| s.answered()));
}

#[test]
fn a_csv_run_logs_moving_values() {
    let mut sim = SsmSim::demo();
    let (csv, st) = {
        let mut kl = SsmKLine::open(&mut sim, ECU).unwrap();
        let mut diag = CoreDiag::new(&mut kl, 200);
        let init = ssm::identify(&mut diag).unwrap();
        let sel = ssm::select(&demo_def(), &init, SsmTarget::Ecu);
        let mut session = LiveSession::new(&sel.def, &[], ssm_cfg(80)).unwrap();
        session.start(&mut diag);
        let mut out = Vec::new();
        session.run_csv(&mut diag, 3, 0, &mut out).unwrap();
        let st = session.stop(&mut diag).clone();
        (String::from_utf8(out).unwrap(), st)
    };
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines[0], CSV_HEADER);
    assert_eq!(lines.len(), 1 + 3 * 5);
    let rpm: Vec<&str> = lines
        .iter()
        .filter(|l| l.split(',').nth(1) == Some("P_RPM"))
        .map(|l| l.split(',').nth(4).unwrap())
        .collect();
    assert_eq!(rpm.len(), 3);
    assert!(rpm[0] != rpm[1] && rpm[1] != rpm[2], "{rpm:?}");
    assert!(
        lines[1..].iter().all(|l| l.split(',').nth(4) != Some("")),
        "{csv}"
    );
    assert_eq!(st.mode, LogMode::Ssm);
    assert_eq!(st.tick_requests, 3);
    assert!(sim.writes.is_empty(), "logging never writes");
}
