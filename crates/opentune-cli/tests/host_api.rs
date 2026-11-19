//! The CLI host API: an embedder reuses the public CLI with its own
//! subcommands, drivers, checksum algorithms and seed-key handlers.
use clap::{Args, FromArgMatches};
use opentune_checksum::{BlockStatus, ChecksumAlgo, Fix};
use opentune_cli::{
    check_config, command, try_run_from, CliConfig, CliContext, ConnectArgs, ExitStatus,
    ExtraCommand,
};
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Interface, InterfaceInfo, MemoryRegion, Progress,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

// ---------------------------------------------------------------- fixtures

/// A checksum only the embedder knows. Records that it was asked to verify.
struct TestSum;
static TEST_SUM_VERIFIED: AtomicBool = AtomicBool::new(false);

impl ChecksumAlgo for TestSum {
    fn id(&self) -> &str {
        "test-sum"
    }
    fn verify(&self, image: &[u8]) -> opentune_core::Result<Vec<BlockStatus>> {
        TEST_SUM_VERIFIED.store(true, Ordering::SeqCst);
        Ok(vec![BlockStatus {
            name: "whole".into(),
            region: MemoryRegion::new(0, image.len() as u32),
            stored: 0,
            computed: 0,
            ok: true,
        }])
    }
    fn correct(&self, _image: &mut [u8]) -> opentune_core::Result<Vec<Fix>> {
        Ok(Vec::new())
    }
}

fn checksums(id: &str) -> Option<Box<dyn ChecksumAlgo>> {
    (id == "test-sum").then(|| Box::new(TestSum) as Box<dyn ChecksumAlgo>)
}

/// A seed-key handler only the embedder knows (not an OEM algorithm).
struct TestKey;
impl SeedKey for TestKey {
    fn id(&self) -> &str {
        "test-key"
    }
    fn compute(&self, seed: &[u8], _level: u8) -> opentune_core::Result<Vec<u8>> {
        Ok(seed.iter().map(|b| !b).collect())
    }
}

fn seedkeys(id: &str) -> Option<Box<dyn SeedKey>> {
    (id == "test-key").then(|| Box::new(TestKey) as Box<dyn SeedKey>)
}

/// A driver with an unlock step: records the id of the seed key it receives.
struct SeedProbe;
static ATTACHED: Mutex<Option<String>> = Mutex::new(None);

impl Interface for SeedProbe {
    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: "seedprobe".into(),
            display_name: "seed probe".into(),
            firmware: None,
            serial: None,
        }
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    fn open(&mut self, _cfg: &ConnectConfig) -> opentune_core::Result<()> {
        Ok(())
    }
    fn close(&mut self) -> opentune_core::Result<()> {
        Ok(())
    }
    fn read_ecu_id(&mut self) -> opentune_core::Result<EcuId> {
        Ok(EcuId::default())
    }
    fn read_memory(
        &mut self,
        region: MemoryRegion,
        _progress: &mut dyn FnMut(Progress),
    ) -> opentune_core::Result<Vec<u8>> {
        Ok(vec![0; region.length as usize])
    }
    fn write_memory(
        &mut self,
        _region: MemoryRegion,
        _data: &[u8],
        _progress: &mut dyn FnMut(Progress),
    ) -> opentune_core::Result<()> {
        Ok(())
    }
    fn set_seed_key(&mut self, seed_key: Box<dyn SeedKey>) -> bool {
        *ATTACHED.lock().unwrap() = Some(seed_key.id().to_owned());
        true
    }
}

fn seedprobe() -> Box<dyn Interface> {
    Box::new(SeedProbe)
}

/// A driver that can plan a read: records the config it was opened with and
/// panics if anything would actually be sent.
struct PlanProbe;
static OPENED_WITH: Mutex<Vec<ConnectConfig>> = Mutex::new(Vec::new());

/// The recorded open whose `port` option is `port` (tests run in parallel, so
/// each one marks its opens with its own port).
fn opened_on(port: &str) -> Option<ConnectConfig> {
    OPENED_WITH
        .lock()
        .unwrap()
        .iter()
        .find(|c| c.options.get("port").map(String::as_str) == Some(port))
        .cloned()
}

impl Interface for PlanProbe {
    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: "planprobe".into(),
            display_name: "plan probe".into(),
            firmware: None,
            serial: None,
        }
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }
    fn open(&mut self, cfg: &ConnectConfig) -> opentune_core::Result<()> {
        OPENED_WITH.lock().unwrap().push(cfg.clone());
        Ok(())
    }
    fn close(&mut self) -> opentune_core::Result<()> {
        Ok(())
    }
    fn read_ecu_id(&mut self) -> opentune_core::Result<EcuId> {
        panic!("a dry run must not read the identity");
    }
    fn read_memory(
        &mut self,
        _region: MemoryRegion,
        _progress: &mut dyn FnMut(Progress),
    ) -> opentune_core::Result<Vec<u8>> {
        panic!("a dry run must not read");
    }
    fn write_memory(
        &mut self,
        _region: MemoryRegion,
        _data: &[u8],
        _progress: &mut dyn FnMut(Progress),
    ) -> opentune_core::Result<()> {
        panic!("a dry run must not write");
    }
    fn plan_read_memory(
        &mut self,
        region: MemoryRegion,
    ) -> Option<opentune_core::Result<Vec<String>>> {
        if region.length > 0x100 {
            return Some(Err(opentune_core::Error::Unsupported(
                "too long for one frame".into(),
            )));
        }
        Some(Ok(vec![
            "1003".into(),
            format!("2314{:08X}{:02X}", region.address, region.length),
        ]))
    }
}

fn planprobe() -> Box<dyn Interface> {
    Box::new(PlanProbe)
}

/// `probe-ctx <tag>`: records what it saw of the context.
static PROBE_SEEN: Mutex<Option<String>> = Mutex::new(None);

fn probe_ctx(m: &clap::ArgMatches, ctx: &CliContext) -> anyhow::Result<()> {
    let tag = m.get_one::<String>("tag").cloned().unwrap_or_default();
    let ids: Vec<&str> = ctx.registry().ids().collect();
    assert!(ids.contains(&"mock") && ids.contains(&"seedprobe"));
    assert!(ctx.checksum("test-sum").is_some(), "extra checksum");
    assert!(ctx.checksum("crc32-tail").is_some(), "public checksum");
    assert!(ctx.seed_key("test-key").is_some());
    assert!(ctx.seed_key("nope").is_none());
    *PROBE_SEEN.lock().unwrap() = Some(tag);
    Ok(())
}

/// `probe-open <ConnectArgs>`: opens a driver through the context helper.
fn probe_open(m: &clap::ArgMatches, ctx: &CliContext) -> anyhow::Result<()> {
    let conn = ConnectArgs::from_arg_matches(m)?;
    let mut drv = ctx.open_connect(&conn)?;
    drv.close()?;
    Ok(())
}

fn config() -> CliConfig {
    CliConfig {
        name: "bench-test",
        about: Some("OpenTune plus test commands".into()),
        seedkeys: vec![seedkeys],
        checksums: vec![checksums],
        drivers: vec![
            ("seedprobe", "Seed-key probe (test)", seedprobe),
            ("planprobe", "Dry-run probe (test)", planprobe),
        ],
        extra: vec![
            ExtraCommand {
                command: clap::Command::new("probe-ctx")
                    .about("Probe the host context")
                    .arg(clap::Arg::new("tag")),
                run: probe_ctx,
            },
            ExtraCommand {
                command: ConnectArgs::augment_args(
                    clap::Command::new("probe-open").about("Open a driver"),
                ),
                run: probe_open,
            },
        ],
        seedkey_loader: None,
    }
}

fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
    let p = std::env::temp_dir().join(format!("opentune-host-api-{}-{name}", std::process::id()));
    std::fs::write(&p, bytes).unwrap();
    p
}

fn err_text(r: anyhow::Result<()>) -> String {
    format!("{:#}", r.expect_err("expected an error"))
}

// ---------------------------------------------------------------- tests

#[test]
fn extra_command_is_dispatched_with_the_context() {
    try_run_from(config(), ["bench-test", "probe-ctx", "hello"]).unwrap();
    assert_eq!(PROBE_SEEN.lock().unwrap().as_deref(), Some("hello"));
}

#[test]
fn core_commands_still_run_under_a_host() {
    try_run_from(config(), ["bench-test", "dtc", "P0101"]).unwrap();
    try_run_from(config(), ["bench-test", "drivers"]).unwrap();
}

#[test]
fn colliding_names_are_refused_at_startup() {
    let mut cfg = config();
    cfg.extra.push(ExtraCommand {
        command: clap::Command::new("read"),
        run: probe_ctx,
    });
    let msg = format!("{}", check_config(&cfg).unwrap_err());
    assert!(msg.contains("'read'") && msg.contains("collides"), "{msg}");
    let msg = err_text(try_run_from(cfg, ["bench-test", "probe-ctx"]));
    assert!(msg.contains("'read'") && msg.contains("collides"), "{msg}");

    // An alias counts, as does `help` and a repeated extra.
    for command in [
        clap::Command::new("mine").alias("checksum"),
        clap::Command::new("help"),
        clap::Command::new("probe-ctx"),
    ] {
        let mut cfg = config();
        cfg.extra.push(ExtraCommand {
            command,
            run: probe_ctx,
        });
        assert!(check_config(&cfg).is_err());
    }

    // A driver id that shadows a public driver is refused too.
    let mut cfg = config();
    cfg.drivers.push(("mock", "shadow", seedprobe));
    let msg = format!("{}", check_config(&cfg).unwrap_err());
    assert!(msg.contains("'mock'"), "{msg}");

    assert!(check_config(&config()).is_ok());
    assert!(check_config(&CliConfig::default()).is_ok());
}

#[test]
fn checksum_verify_uses_the_extra_resolver_first() {
    let img = temp_file("sum.bin", &[1, 2, 3, 4]);
    let path = img.to_str().unwrap();
    try_run_from(
        config(),
        [
            "bench-test",
            "checksum",
            "verify",
            "--algo",
            "test-sum",
            path,
        ],
    )
    .unwrap();
    assert!(TEST_SUM_VERIFIED.load(Ordering::SeqCst));

    // The public CLI does not know the id.
    let msg = err_text(try_run_from(
        CliConfig::default(),
        ["opentune", "checksum", "verify", "--algo", "test-sum", path],
    ));
    assert_eq!(msg, "unknown checksum algo 'test-sum'");
    let _ = std::fs::remove_file(img);
}

#[test]
fn unknown_seedkey_is_an_error() {
    // Resolved before any driver is made or file read.
    for cfg in [CliConfig::default(), config()] {
        let msg = err_text(try_run_from(
            cfg,
            ["opentune", "info", "--driver", "mock", "--seedkey", "nope"],
        ));
        assert!(msg.contains("unknown seed-key id 'nope'"), "{msg}");
    }
}

#[test]
fn seedkey_is_attached_to_the_driver() {
    try_run_from(
        config(),
        [
            "bench-test",
            "info",
            "--driver",
            "seedprobe",
            "--seedkey",
            "test-key",
        ],
    )
    .unwrap();
    assert_eq!(ATTACHED.lock().unwrap().as_deref(), Some("test-key"));

    // Through the context helper an extra command uses.
    *ATTACHED.lock().unwrap() = None;
    try_run_from(
        config(),
        [
            "bench-test",
            "probe-open",
            "--driver",
            "seedprobe",
            "--seedkey",
            "test-key",
        ],
    )
    .unwrap();
    assert_eq!(ATTACHED.lock().unwrap().as_deref(), Some("test-key"));

    // A driver without an unlock step refuses the key instead of dropping it.
    let msg = err_text(try_run_from(
        config(),
        [
            "bench-test",
            "info",
            "--driver",
            "mock",
            "--seedkey",
            "test-key",
        ],
    ));
    assert!(msg.contains("no security-access unlock step"), "{msg}");
}

#[test]
fn help_lists_the_extra_commands() {
    let help = command(&config()).unwrap().render_help().to_string();
    assert!(help.contains("OpenTune plus test commands"), "{help}");
    assert!(help.contains("probe-ctx") && help.contains("Probe the host context"));
    assert!(help.contains("probe-open"));
    assert!(help.contains("checksum"), "core commands stay listed");
    assert!(help.contains("Usage: bench-test"), "{help}");

    // `--help` and `--version` come back as clap errors, not a process exit.
    let e = try_run_from(config(), ["bench-test", "--help"]).unwrap_err();
    let ce = e.downcast_ref::<clap::Error>().expect("clap error");
    assert_eq!(ce.kind(), clap::error::ErrorKind::DisplayHelp);
    assert!(ce.render().to_string().contains("probe-ctx"));
    let e = try_run_from(config(), ["bench-test", "--version"]).unwrap_err();
    let ce = e.downcast_ref::<clap::Error>().expect("clap error");
    assert!(ce.render().to_string().starts_with("bench-test "));

    let public = command(&CliConfig::default())
        .unwrap()
        .render_help()
        .to_string();
    assert!(!public.contains("probe-ctx"));
}

#[test]
fn failed_validation_is_a_silent_exit_status() {
    let bad = temp_file("bad.json", b"{ not json");
    let e = try_run_from(
        CliConfig::default(),
        ["opentune", "ecudefs", "validate", bad.to_str().unwrap()],
    )
    .unwrap_err();
    assert_eq!(e.downcast_ref::<ExitStatus>(), Some(&ExitStatus(2)));
    let _ = std::fs::remove_file(bad);
}

#[test]
fn adapter_flags_reach_the_driver() {
    try_run_from(
        config(),
        [
            "bench-test",
            "read",
            "--driver",
            "planprobe",
            "--port",
            "/dev/ttyUSB3",
            "--baud",
            "38400",
            "--bus",
            "ms",
            "--tx-id",
            "0x726",
            "--rx-id",
            "72E",
            "--opt",
            "read_mode=upload",
            "--opt",
            "timeout_ms=5000",
            "--addr",
            "0x1000",
            "--len",
            "16",
            "--plan",
        ],
    )
    .unwrap();
    let cfg = opened_on("/dev/ttyUSB3").expect("opened");
    assert_eq!((cfg.tx_id, cfg.rx_id), (0x726, 0x72E));
    for (k, v) in [
        ("port", "/dev/ttyUSB3"),
        ("baud", "38400"),
        ("bus", "ms"),
        ("read_mode", "upload"),
        ("timeout_ms", "5000"),
        ("base", "0x0"),
    ] {
        assert_eq!(cfg.options.get(k).map(String::as_str), Some(v), "{k}");
    }

    // A bad flag is refused before any driver is made.
    let msg = err_text(try_run_from(
        config(),
        [
            "bench-test",
            "info",
            "--driver",
            "planprobe",
            "--port",
            "/dev/bad-flag",
            "--tx-id",
            "7XE",
            "--rx-id",
            "7E8",
        ],
    ));
    assert!(msg.contains("--tx-id '7XE'"), "{msg}");
    assert!(opened_on("/dev/bad-flag").is_none());
}

#[test]
fn read_and_dump_plan_send_nothing() {
    let out =
        std::env::temp_dir().join(format!("opentune-host-api-{}-plan.bin", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let out_s = out.to_str().unwrap();
    // PlanProbe panics on any read; --plan must not reach one.
    try_run_from(
        config(),
        [
            "bench-test",
            "read",
            "--driver",
            "planprobe",
            "--addr",
            "0x10",
            "--len",
            "8",
            "--out",
            out_s,
            "--plan",
        ],
    )
    .unwrap();
    assert!(!out.exists(), "a dry run writes no output file");
    let vault = std::env::temp_dir().join(format!(
        "opentune-host-api-{}-planvault",
        std::process::id()
    ));
    try_run_from(
        config(),
        [
            "bench-test",
            "dump",
            "--driver",
            "planprobe",
            "--len",
            "8",
            "--vault",
            vault.to_str().unwrap(),
            "--plan",
        ],
    )
    .unwrap();
    assert!(!vault.exists(), "a dry run archives nothing");

    // The driver's refusal comes through.
    let msg = err_text(try_run_from(
        config(),
        [
            "bench-test",
            "read",
            "--driver",
            "planprobe",
            "--addr",
            "0",
            "--len",
            "0x200",
            "--plan",
        ],
    ));
    assert!(msg.contains("too long for one frame"), "{msg}");

    // `read` without --plan still needs --out.
    let e = try_run_from(
        config(),
        [
            "bench-test",
            "read",
            "--driver",
            "planprobe",
            "--addr",
            "0",
            "--len",
            "8",
        ],
    )
    .unwrap_err();
    let ce = e.downcast_ref::<clap::Error>().expect("clap usage error");
    assert!(ce.render().to_string().contains("--out"), "{}", ce.render());
}

#[test]
fn a_driver_without_a_dry_run_is_refused() {
    let msg = err_text(try_run_from(
        config(),
        [
            "bench-test",
            "read",
            "--driver",
            "seedprobe",
            "--addr",
            "0",
            "--len",
            "8",
            "--plan",
        ],
    ));
    assert!(
        msg.contains("driver cannot plan") && msg.contains("'seedprobe'"),
        "{msg}"
    );
}

#[test]
fn the_context_plans_for_embedders() {
    let ctx = CliContext::from_config(&config());
    let conn = ConnectArgs::from_arg_matches(
        &ConnectArgs::augment_args(clap::Command::new("x"))
            .try_get_matches_from(["x", "--driver", "planprobe"])
            .unwrap(),
    )
    .unwrap();
    let mut drv = ctx.open_connect(&conn).unwrap();
    let lines = ctx
        .plan_read(
            drv.as_mut(),
            "planprobe",
            MemoryRegion::new(0x8000_0000, 0x20),
        )
        .unwrap();
    assert_eq!(lines, ["1003", "23148000000020"]);
    let mut bare = ctx
        .open_driver("seedprobe", &ConnectConfig::default(), None)
        .unwrap();
    let msg = format!(
        "{:#}",
        ctx.plan_read(bare.as_mut(), "seedprobe", MemoryRegion::new(0, 1))
            .unwrap_err()
    );
    assert!(msg.contains("driver cannot plan"), "{msg}");
}

#[test]
fn mock_read_plan_prints_the_file_slice() {
    let img = temp_file("mockplan.bin", &[0u8; 64]);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_opentune"))
        .args(["read", "--driver", "mock", "--file"])
        .arg(&img)
        .args([
            "--base", "0x100", "--addr", "0x110", "--len", "0x20", "--plan",
        ])
        .output()
        .expect("run opentune");
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        stdout.trim_end(),
        format!("copy {} [0x10..0x30] (32 bytes)", img.display())
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("nothing sent"));
    // Past the end of the image: refused like the real read.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_opentune"))
        .args(["read", "--driver", "mock", "--file"])
        .arg(&img)
        .args(["--addr", "0x30", "--len", "0x20", "--plan"])
        .output()
        .expect("run opentune");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("read past end of image"));
    let _ = std::fs::remove_file(img);
}
