//! `opentune flash --uds-module` through the CLI host API, against a driver
//! that wraps the simulated flashable ECU (`opentune-udssim`).
use opentune_cli::{try_run_from, CliConfig};
use opentune_container::vbf::crc16_ccitt_false;
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::protocol::DiagTransport;
use opentune_core::{
    Capabilities, ConnectConfig, EcuId, Interface, InterfaceInfo, MemoryRegion, Progress,
};
use opentune_udssim::{FlashConfig, FlashRegion, SimTransport, UdsServer};
use std::path::PathBuf;
use std::sync::Mutex;

const R0: u32 = 0x0001_0000;

/// The simulated ECU, shared by every driver instance (moved in on open).
static ECU: Mutex<Option<SimTransport>> = Mutex::new(None);
/// Serialises the tests in this file (they share `ECU`).
static SERIAL: Mutex<()> = Mutex::new(());

fn install_ecu(part: &str) {
    let mut s = UdsServer::new();
    s.set_did(0xF187, part);
    s.set_did(0xF188, "0001");
    s.enable_flash(FlashConfig {
        regions: vec![FlashRegion::new(R0, vec![0x5A; 0x400])],
        max_block_len: 0x102,
        ..FlashConfig::default()
    });
    *ECU.lock().unwrap() = Some(SimTransport::new(s));
}

fn with_ecu<T>(f: impl FnOnce(&SimTransport) -> T) -> T {
    f(ECU.lock().unwrap().as_ref().expect("ECU installed"))
}

#[derive(Default)]
struct SimDriver {
    t: Option<SimTransport>,
}

impl Drop for SimDriver {
    fn drop(&mut self) {
        if let Some(t) = self.t.take() {
            *ECU.lock().unwrap() = Some(t);
        }
    }
}

impl Interface for SimDriver {
    fn info(&self) -> InterfaceInfo {
        InterfaceInfo {
            id: "simflash".into(),
            display_name: "simulated flashable ECU".into(),
            firmware: None,
            serial: None,
        }
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            can: true,
            battery_voltage: true,
            ..Capabilities::default()
        }
    }
    fn open(&mut self, _cfg: &ConnectConfig) -> opentune_core::Result<()> {
        self.t = ECU.lock().unwrap().take();
        Ok(())
    }
    fn close(&mut self) -> opentune_core::Result<()> {
        if let Some(t) = self.t.take() {
            *ECU.lock().unwrap() = Some(t);
        }
        Ok(())
    }
    fn battery_millivolts(&mut self) -> opentune_core::Result<Option<u32>> {
        Ok(Some(12_700))
    }
    fn read_ecu_id(&mut self) -> opentune_core::Result<EcuId> {
        Ok(EcuId::default())
    }
    fn read_memory(
        &mut self,
        _region: MemoryRegion,
        _progress: &mut dyn FnMut(Progress),
    ) -> opentune_core::Result<Vec<u8>> {
        panic!("the module path must not use the block read");
    }
    fn write_memory(
        &mut self,
        _region: MemoryRegion,
        _data: &[u8],
        _progress: &mut dyn FnMut(Progress),
    ) -> opentune_core::Result<()> {
        panic!("the module path must not use the block write");
    }
    fn diag(&mut self) -> Option<&mut dyn DiagTransport> {
        self.t.as_mut().map(|t| t as &mut dyn DiagTransport)
    }
}

fn simflash() -> Box<dyn Interface> {
    Box::<SimDriver>::default()
}

/// The simulator's test seed→key (`seed XOR 0xA5`), not an OEM algorithm.
struct TestKey;
impl SeedKey for TestKey {
    fn id(&self) -> &str {
        "sim-xor"
    }
    fn compute(&self, seed: &[u8], _level: u8) -> opentune_core::Result<Vec<u8>> {
        Ok(seed.iter().map(|b| b ^ 0xA5).collect())
    }
}

fn seedkeys(id: &str) -> Option<Box<dyn SeedKey>> {
    (id == "sim-xor").then(|| Box::new(TestKey) as Box<dyn SeedKey>)
}

fn config() -> CliConfig {
    CliConfig {
        seedkeys: vec![seedkeys],
        drivers: vec![("simflash", "Simulated flashable ECU (test)", simflash)],
        ..CliConfig::default()
    }
}

fn payload() -> Vec<u8> {
    (0..0x280).map(|i| (i % 251) as u8).collect()
}

fn workdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("opentune-cli-module-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_vbf(dir: &std::path::Path) -> PathBuf {
    let data = payload();
    let mut v = format!(
        "vbf_version = 2.6;\nheader {{\n  sw_part_number = \"TEST-1\";\n  \
         erase = {{ {{ 0x{R0:08X}, 0x00000400 }} }};\n}}\n"
    )
    .into_bytes();
    v.extend_from_slice(&R0.to_be_bytes());
    v.extend_from_slice(&(data.len() as u32).to_be_bytes());
    v.extend_from_slice(&data);
    v.extend_from_slice(&crc16_ccitt_false(&data).to_be_bytes());
    let p = dir.join("module.vbf");
    std::fs::write(&p, v).unwrap();
    p
}

fn flash_args(vbf: &std::path::Path, vault: &std::path::Path, extra: &[&str]) -> Vec<String> {
    let mut a: Vec<String> = [
        "opentune",
        "flash",
        "--uds-module",
        "--driver",
        "simflash",
        "--container",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    a.push(vbf.display().to_string());
    a.push("--vault".into());
    a.push(vault.display().to_string());
    a.extend(extra.iter().map(|s| s.to_string()));
    a
}

#[test]
fn plan_sends_nothing_and_needs_no_adapter() {
    let _g = SERIAL.lock().unwrap();
    install_ecu("5Q0907000A");
    let dir = workdir("plan");
    let vbf = write_vbf(&dir);
    try_run_from(
        config(),
        flash_args(&vbf, &dir, &["--plan", "--expect-part", "5Q0907000"]),
    )
    .unwrap();
    assert_eq!(with_ecu(|t| t.request_count()), 0);
}

#[test]
fn module_flash_end_to_end_and_refusals() {
    let _g = SERIAL.lock().unwrap();
    let dir = workdir("run");
    let vbf = write_vbf(&dir);

    // Wrong ECU: refused, nothing erased.
    install_ecu("8E0910155");
    let e = try_run_from(
        config(),
        flash_args(
            &vbf,
            &dir,
            &["--seedkey", "sim-xor", "--expect-part", "5Q0907000"],
        ),
    )
    .unwrap_err();
    assert!(format!("{e:#}").contains("does not match"), "{e:#}");
    assert_eq!(with_ecu(|t| t.server().flash().unwrap().stats().erases), 0);

    // No seed-key handler: refused before erase.
    install_ecu("5Q0907000A");
    let e = try_run_from(
        config(),
        flash_args(&vbf, &dir, &["--expect-part", "5Q0907000"]),
    )
    .unwrap_err();
    assert!(format!("{e:#}").contains("no seed-key"), "{e:#}");
    assert_eq!(with_ecu(|t| t.server().flash().unwrap().stats().erases), 0);

    // The real thing.
    try_run_from(
        config(),
        flash_args(
            &vbf,
            &dir,
            &["--seedkey", "sim-xor", "--expect-part", "5Q0907000"],
        ),
    )
    .unwrap();
    with_ecu(|t| {
        let f = t.server().flash().unwrap();
        assert_eq!(f.read(R0, 0x280).unwrap(), payload());
        assert!(f.is_valid());
        assert_eq!(t.trace().requests().last().unwrap(), &[0x11, 0x01]);
    });
    assert!(dir.join("uds-module.log").exists());
    assert!(!dir.join("uds-module.state.json").exists());
    // The original was archived to the vault.
    let archived = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.path().extension().is_some_and(|x| x == "bin"));
    assert!(archived);
}

fn run(args: &[&str]) -> anyhow::Result<()> {
    try_run_from(CliConfig::default(), args.iter().copied())
}

#[test]
fn public_cli_reads_and_flashes_the_udssim_driver() {
    let dir = workdir("udssim");
    let p = |n: &str| dir.join(n).display().to_string();
    let ecu = "ecu=cli-udssim";
    // Fresh instance, read the stock ramp.
    run(&[
        "opentune",
        "read",
        "--driver",
        "udssim",
        "--opt",
        ecu,
        "--opt",
        "reset=1",
        "--addr",
        "0x100",
        "--len",
        "0x40",
        "--out",
        &p("stock.bin"),
    ])
    .unwrap();
    let stock = std::fs::read(dir.join("stock.bin")).unwrap();
    assert_eq!(
        stock,
        (0x100..0x140).map(|i| (i & 0xFF) as u8).collect::<Vec<_>>()
    );

    // Default flash path (driver write_memory) with the simulator test key.
    let new: Vec<u8> = (0..0x40).map(|i| 0xC0 ^ i as u8).collect();
    std::fs::write(dir.join("new.bin"), &new).unwrap();
    run(&[
        "opentune",
        "flash",
        "--driver",
        "udssim",
        "--opt",
        ecu,
        "--seedkey",
        "udssim-test",
        "--addr",
        "0x100",
        "--data",
        &p("new.bin"),
        "--vault",
        &p("vault"),
    ])
    .unwrap();
    run(&[
        "opentune",
        "read",
        "--driver",
        "udssim",
        "--opt",
        ecu,
        "--addr",
        "0x100",
        "--len",
        "0x40",
        "--out",
        &p("back.bin"),
    ])
    .unwrap();
    assert_eq!(std::fs::read(dir.join("back.bin")).unwrap(), new);

    // UDS module path from a plain bin + layout.
    let img: Vec<u8> = (0..0x400).map(|i| (i * 5 % 256) as u8).collect();
    std::fs::write(dir.join("app.bin"), &img).unwrap();
    run(&[
        "opentune",
        "flash",
        "--uds-module",
        "--driver",
        "udssim",
        "--opt",
        ecu,
        "--seedkey",
        "udssim-test",
        "--data",
        &p("app.bin"),
        "--addr",
        "0x1000",
        "--layout",
        "0x1000:0x400",
        "--expect-part",
        "OTSIM",
        "--vault",
        &p("vault2"),
    ])
    .unwrap();
    run(&[
        "opentune",
        "read",
        "--driver",
        "udssim",
        "--opt",
        ecu,
        "--addr",
        "0x1000",
        "--len",
        "0x400",
        "--out",
        &p("app-back.bin"),
    ])
    .unwrap();
    assert_eq!(std::fs::read(dir.join("app-back.bin")).unwrap(), img);
}

#[test]
fn default_flash_path_still_needs_addr_and_data() {
    let _g = SERIAL.lock().unwrap();
    let r = try_run_from(
        CliConfig::default(),
        ["opentune", "flash", "--driver", "mock"],
    );
    assert!(r.is_err());
}
