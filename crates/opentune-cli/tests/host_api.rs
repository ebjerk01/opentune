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
        drivers: vec![("seedprobe", "Seed-key probe (test)", seedprobe)],
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
