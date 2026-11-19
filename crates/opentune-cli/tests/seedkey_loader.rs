//! The UnlockECU-format definitions loader through the CLI: off by default,
//! both flags required, `builtin:sa2` unlocking the UDS simulator through
//! `--seedkey unlockecu:<name>`, the refuse-list, and `seedkey defs`. Every
//! definition and SA2 tape here is synthetic (written for the test).
use opentune_cli::seedkey_loader::{LoaderConfig, ProviderSpec};
use opentune_cli::{try_run_from, CliConfig};
use std::path::PathBuf;
use std::process::Command;

fn run(cfg: CliConfig, args: &[&str]) -> anyhow::Result<()> {
    try_run_from(cfg, args.iter().copied())
}

fn workdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("opentune-cli-skl-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A synthetic definitions file: the simulator's programming level (0x11)
/// with a tape that computes `seed XOR A5A5A5A5` (`eor 0xA5A5A5A5; finish`),
/// an entry only an external program could serve, and a refused entry.
const DEFS: &str = r#"[
  {"EcuName":"SIM-SA2","Aliases":["SIMULATOR"],"AccessLevel":17,"SeedLength":4,"KeyLength":4,
   "Provider":"VolkswagenSA2","Origin":"synthetic test",
   "Parameters":[{"Key":"InstructionTape","Value":"87A5A5A5A54C","DataType":"ByteArray"}]},
  {"EcuName":"SIM-EXT","AccessLevel":17,"SeedLength":4,"KeyLength":4,"Provider":"DemoXor",
   "Parameters":[{"Key":"Mask","Value":"A5","DataType":"ByteArray"}]},
  {"EcuName":"SIM-EZS","AccessLevel":17,"SeedLength":4,"KeyLength":4,"Provider":"DemoXor"}
]"#;

fn flash_args(dir: &str, ecu: &str, seedkey: &str) -> Vec<String> {
    [
        "opentune",
        "flash",
        "--driver",
        "udssim",
        "--opt",
        ecu,
        "--seedkey",
        seedkey,
        "--addr",
        "0x200",
        "--data",
        &format!("{dir}/new.bin"),
        "--vault",
        &format!("{dir}/vault"),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn new_bin(dir: &std::path::Path) -> Vec<u8> {
    let new: Vec<u8> = (0..0x40).map(|i| 0x3C ^ i as u8).collect();
    std::fs::write(dir.join("new.bin"), &new).unwrap();
    new
}

#[test]
fn off_by_default_and_both_flags_required() {
    let dir = workdir("off");
    new_bin(&dir);
    let defs = dir.join("defs.json");
    std::fs::write(&defs, DEFS).unwrap();
    let d = dir.display().to_string();
    let base = flash_args(&d, "ecu=cli-skl-off", "unlockecu:SIM-SA2");
    let base: Vec<&str> = base.iter().map(String::as_str).collect();

    // No flags: the id is unknown and says how to switch the loader on.
    let e = run(CliConfig::default(), &base).unwrap_err().to_string();
    assert!(
        e.contains("--seedkey-defs") && e.contains("off by default"),
        "{e}"
    );

    // A definitions file alone, or a provider alone, is refused.
    let defs_s = defs.display().to_string();
    let mut only_defs = base.clone();
    only_defs.extend(["--seedkey-defs", &defs_s]);
    let e = run(CliConfig::default(), &only_defs)
        .unwrap_err()
        .to_string();
    assert!(e.contains("needs --seedkey-provider"), "{e}");
    let mut only_prov = base.clone();
    only_prov.extend(["--seedkey-provider", "builtin:sa2"]);
    let e = run(CliConfig::default(), &only_prov)
        .unwrap_err()
        .to_string();
    assert!(e.contains("needs --seedkey-defs"), "{e}");

    // A relative program path is refused (no PATH lookup).
    let mut rel = base.clone();
    rel.extend([
        "--seedkey-defs",
        &defs_s,
        "--seedkey-provider",
        "my-provider",
    ]);
    let e = run(CliConfig::default(), &rel).unwrap_err().to_string();
    assert!(e.contains("absolute path"), "{e}");
}

#[test]
fn builtin_sa2_unlocks_the_simulator() {
    let dir = workdir("sa2");
    let new = new_bin(&dir);
    let defs = dir.join("defs.json");
    std::fs::write(&defs, DEFS).unwrap();
    let d = dir.display().to_string();
    let defs_s = defs.display().to_string();
    let ecu = "ecu=cli-skl-sa2";
    // The flags are global: they may follow the subcommand.
    let args = flash_args(&d, ecu, "unlockecu:simulator");
    let mut args: Vec<&str> = args.iter().map(String::as_str).collect();
    args.extend([
        "--seedkey-defs",
        &defs_s,
        "--seedkey-provider",
        "builtin:sa2",
    ]);
    run(CliConfig::default(), &args).unwrap();
    let back = format!("{d}/back.bin");
    run(
        CliConfig::default(),
        &[
            "opentune", "read", "--driver", "udssim", "--opt", ecu, "--addr", "0x200", "--len",
            "0x40", "--out", &back,
        ],
    )
    .unwrap();
    assert_eq!(std::fs::read(&back).unwrap(), new);

    // Without a provider program, a non-SA2 definition says what is missing.
    let args = flash_args(&d, ecu, "unlockecu:SIM-EXT");
    let mut args: Vec<&str> = args.iter().map(String::as_str).collect();
    args.extend([
        "--seedkey-defs",
        &defs_s,
        "--seedkey-provider",
        "builtin:sa2",
    ]);
    let e = format!("{:#}", run(CliConfig::default(), &args).unwrap_err());
    assert!(e.contains("'DemoXor' is not available"), "{e}");

    // A refused entry never runs.
    let args = flash_args(&d, ecu, "unlockecu:SIM-EZS");
    let mut args: Vec<&str> = args.iter().map(String::as_str).collect();
    args.extend([
        "--seedkey-defs",
        &defs_s,
        "--seedkey-provider",
        "builtin:sa2",
    ]);
    let e = run(CliConfig::default(), &args).unwrap_err().to_string();
    assert!(e.contains("CONTRIBUTING.md"), "{e}");
}

#[test]
fn embedder_config_enables_the_loader() {
    let dir = workdir("cfg");
    let new = new_bin(&dir);
    let defs = dir.join("defs.json");
    std::fs::write(&defs, DEFS).unwrap();
    let d = dir.display().to_string();
    let cfg = || CliConfig {
        seedkey_loader: Some(LoaderConfig::new(
            defs.clone(),
            vec![ProviderSpec::BuiltinSa2],
        )),
        ..CliConfig::default()
    };
    let ecu = "ecu=cli-skl-cfg";
    let args = flash_args(&d, ecu, "unlockecu:SIM-SA2");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run(cfg(), &args).unwrap();
    let back = format!("{d}/back.bin");
    run(
        cfg(),
        &[
            "opentune", "read", "--driver", "udssim", "--opt", ecu, "--addr", "0x200", "--len",
            "0x40", "--out", &back,
        ],
    )
    .unwrap();
    assert_eq!(std::fs::read(&back).unwrap(), new);
}

#[test]
fn seedkey_defs_lists_without_values() {
    let dir = workdir("defs");
    let defs = dir.join("defs.json");
    let bad =
        r#"{"EcuName":"SIM-BAD","AccessLevel":0,"SeedLength":4,"KeyLength":4,"Provider":"P"}"#;
    std::fs::write(&defs, DEFS.replacen('[', &format!("[{bad},"), 1)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_opentune"))
        .args(["seedkey", "defs", defs.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("2 loadable, 1 refused, 1 rejected"), "{text}");
    assert!(
        text.contains("SIM-SA2 (aliases: SIMULATOR)  level 0x11"),
        "{text}"
    );
    assert!(text.contains("InstructionTape:ByteArray"), "{text}");
    assert!(
        text.contains("SIM-EZS") && text.contains("CONTRIBUTING.md"),
        "{text}"
    );
    assert!(
        text.contains("SIM-BAD") && text.contains("AccessLevel 0"),
        "{text}"
    );
    assert!(
        !text.contains("87A5") && !text.contains("A5A5"),
        "values leaked: {text}"
    );

    // A malformed file fails as a whole.
    std::fs::write(&defs, "{ nope").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_opentune"))
        .args(["seedkey", "defs", defs.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not valid JSON"));
}
