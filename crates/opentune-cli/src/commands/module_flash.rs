//! `flash --uds-module`: program a control module over UDS from a flash
//! container (or a plain bin + layout), behind the production flash-safety
//! policy. The default `flash` path (a raw region through the driver's block
//! write) is untouched.
use crate::cli::ConnectArgs;
use crate::context::CliContext;
use crate::util::parse_num;
use anyhow::{anyhow, bail, Context, Result};
use opentune_container::Keys;
use opentune_core::{EcuId, MemoryRegion};
use opentune_flash::module::{
    flash_module_on, plan_module_flash, ModuleFlashOptions, ModuleImage, ModulePhase,
    ModuleProgress, ModuleProtocol,
};
use opentune_flash::{FlashSafetyPolicy, VoltagePolicy};
use std::path::{Path, PathBuf};

/// The `flash` flags of the UDS module path.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct ModuleFlashArgs {
    /// Program a control module over UDS: identify → voltage → backup →
    /// programming session → security (--seedkey) → per block erase / 0x34 /
    /// 0x36 / 0x37 / checksum routine → dependencies → ECU reset. Production
    /// safety policy; simulator-tested only (see docs/FLASHING.md).
    #[arg(long)]
    pub uds_module: bool,
    /// Flash container to program: VBF, ODX/ODX-F, FRF (needs --frf-key) or
    /// SGO. Only plain blocks are sent (compressed/encrypted ones are refused).
    #[arg(long, requires = "uds_module")]
    pub container: Option<PathBuf>,
    /// FRF key material file (caller-supplied; OpenTune ships none).
    #[arg(long, requires = "container")]
    pub frf_key: Option<PathBuf>,
    /// With --data/--addr (a plain bin loaded at --addr): the blocks to program,
    /// `ADDR:LEN` (hex), comma-separated or repeated. Each is also its erase
    /// region. Default: the whole file as one block.
    #[arg(long, value_delimiter = ',', requires = "uds_module")]
    pub layout: Vec<String>,
    /// Only program block N (index as listed by --plan); repeatable.
    #[arg(long = "block", value_name = "N", requires = "uds_module")]
    pub blocks: Vec<usize>,
    /// Dry run: list every block and every request the flash would send, and
    /// send nothing (no adapter is opened).
    #[arg(long, requires = "uds_module")]
    pub plan: bool,
    /// maxNumberOfBlockLength the plan assumes for the ECU (hex or decimal).
    #[arg(long, default_value = "0x0402")]
    pub max_block_len: String,
    /// Part / hardware number (or prefix) the ECU must report; repeatable.
    #[arg(long, value_name = "PART", requires = "uds_module")]
    pub expect_part: Vec<String>,
    /// Software number (or prefix) the ECU must report; repeatable.
    #[arg(long, value_name = "SW", requires = "uds_module")]
    pub expect_sw: Vec<String>,
    /// Flash even when the connected ECU does not match the expected identity.
    #[arg(long, requires = "uds_module")]
    pub force_identity: bool,
    /// Proceed when the ECU refuses reads, so no backup of the original is
    /// possible.
    #[arg(long, requires = "uds_module")]
    pub no_backup_ok: bool,
    /// Region it is legal to write, `ADDR:LEN` (hex); repeatable. Default: the
    /// container's (or layout's) erase regions.
    #[arg(long, value_name = "ADDR:LEN", requires = "uds_module")]
    pub writable: Vec<String>,
    /// Request-seed SecurityAccess level for programming (hex).
    #[arg(long, default_value = "0x11")]
    pub security_level: String,
    /// eraseMemory routine id (hex).
    #[arg(long, default_value = "0xFF00")]
    pub erase_routine: String,
    /// Checksum routine id run after each block (hex), or `none`.
    #[arg(long, default_value = "0x0202")]
    pub check_routine: String,
    /// checkProgrammingDependencies routine id (hex), or `none`.
    #[arg(long, default_value = "0xFF01")]
    pub dependency_routine: String,
}

fn parse_region(s: &str) -> Result<MemoryRegion> {
    let (a, l) = s
        .split_once(':')
        .ok_or_else(|| anyhow!("expected ADDR:LEN, got '{s}'"))?;
    Ok(MemoryRegion::new(parse_num(a)?, parse_num(l)?))
}

fn parse_u8(s: &str) -> Result<u8> {
    u8::try_from(parse_num(s)?).map_err(|_| anyhow!("'{s}' does not fit a byte"))
}

fn parse_rid(s: &str) -> Result<Option<u16>> {
    if s.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    u16::try_from(parse_num(s)?)
        .map(Some)
        .map_err(|_| anyhow!("routine id '{s}' does not fit 16 bits"))
}

/// The common inputs of `flash` the module path also uses.
pub struct FlashCommon {
    pub conn: ConnectArgs,
    pub addr: Option<String>,
    pub data: Option<PathBuf>,
    pub min_mv: u32,
    pub vault: PathBuf,
    pub resume: Option<PathBuf>,
    pub transcript: Option<PathBuf>,
}

fn load_image(c: &FlashCommon, m: &ModuleFlashArgs) -> Result<ModuleImage> {
    let mut img = if let Some(path) = &m.container {
        if c.data.is_some() || !m.layout.is_empty() {
            bail!("give either --container or --data/--addr/--layout, not both");
        }
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let keys = Keys {
            frf_key: match &m.frf_key {
                Some(k) => Some(std::fs::read(k).with_context(|| format!("read {}", k.display()))?),
                None => None,
            },
            ..Keys::default()
        };
        ModuleImage::from_container_bytes(&bytes, &keys).map_err(|e| anyhow!("{e}"))?
    } else {
        let (Some(addr), Some(data)) = (&c.addr, &c.data) else {
            bail!("--uds-module needs --container, or --data with --addr");
        };
        let bytes = std::fs::read(data).with_context(|| format!("read {}", data.display()))?;
        let layout = m
            .layout
            .iter()
            .map(|s| parse_region(s))
            .collect::<Result<Vec<_>>>()?;
        ModuleImage::from_bin(&bytes, parse_num(addr)?, &layout).map_err(|e| anyhow!("{e}"))?
    };
    img.identity
        .part_numbers
        .extend(m.expect_part.iter().cloned());
    img.identity.sw_numbers.extend(m.expect_sw.iter().cloned());
    Ok(img)
}

fn options(c: &FlashCommon, m: &ModuleFlashArgs) -> Result<ModuleFlashOptions> {
    let mut policy = FlashSafetyPolicy::production();
    policy.min_millivolts = c.min_mv;
    if c.min_mv == 0 {
        // `--min-mv 0` disables the voltage gate, as on the default path.
        policy.voltage = VoltagePolicy::Disabled;
    }
    let protocol = ModuleProtocol {
        security_level: parse_u8(&m.security_level)?,
        erase_routine: parse_rid(&m.erase_routine)?
            .ok_or_else(|| anyhow!("an erase routine is required"))?,
        check_routine: parse_rid(&m.check_routine)?,
        dependency_routine: parse_rid(&m.dependency_routine)?,
        ..ModuleProtocol::default()
    };
    Ok(ModuleFlashOptions {
        policy,
        protocol,
        blocks: (!m.blocks.is_empty()).then(|| m.blocks.clone()),
        force_identity: m.force_identity,
        allow_no_backup: m.no_backup_ok,
        writable_regions: m
            .writable
            .iter()
            .map(|s| parse_region(s))
            .collect::<Result<Vec<_>>>()?,
        state_path: Some(
            c.resume
                .clone()
                .unwrap_or_else(|| c.vault.join("uds-module.state.json")),
        ),
    })
}

fn print_blocks(img: &ModuleImage) {
    println!("Image ({}): {} block(s)", img.source, img.blocks.len());
    for (i, b) in img.blocks.iter().enumerate() {
        let status = match b.refusal() {
            None => "ok".to_string(),
            Some(why) => format!("REFUSED: {why}"),
        };
        println!(
            "  block {i}: {} @0x{:08X} +0x{:X}, erase 0x{:08X} +0x{:X}{} — {status}",
            b.id,
            b.address,
            b.data.len(),
            b.erase.address,
            b.erase.length,
            if b.erase_declared {
                ""
            } else {
                " (block span)"
            },
        );
    }
}

fn phase_name(p: ModulePhase) -> &'static str {
    match p {
        ModulePhase::Validate => "validate",
        ModulePhase::Identify => "identify",
        ModulePhase::Voltage => "voltage",
        ModulePhase::Backup => "backup",
        ModulePhase::Session => "session",
        ModulePhase::Security => "security",
        ModulePhase::Erase => "erase",
        ModulePhase::Download => "download",
        ModulePhase::TransferExit => "exit",
        ModulePhase::Verify => "verify",
        ModulePhase::Check => "check",
        ModulePhase::Dependencies => "dependencies",
        ModulePhase::Reset => "reset",
        ModulePhase::Done => "done",
    }
}

fn write_log(dir: &Path, lines: &[String]) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;
    let p = dir.join("uds-module.log");
    std::fs::write(&p, lines.join("\n") + "\n").ok()?;
    Some(p)
}

/// `flash --uds-module`.
pub fn run(ctx: &CliContext, c: FlashCommon, m: ModuleFlashArgs) -> Result<()> {
    let img = load_image(&c, &m)?;
    let opts = options(&c, &m)?;
    if m.plan {
        let mbl = parse_num(&m.max_block_len)? as usize;
        print_blocks(&img);
        let plan = plan_module_flash(&img, &opts, mbl).map_err(|e| anyhow!("plan: {e}"))?;
        eprintln!(
            "Dry run: {} request(s), nothing sent. Assumes:",
            plan.steps.len()
        );
        for a in &plan.assumptions {
            eprintln!("  - {a}");
        }
        for s in &plan.steps {
            let block = s.block.map(|b| b.to_string()).unwrap_or_else(|| "-".into());
            println!("{:<12} {:>3}  {}", phase_name(s.phase), block, s.render());
        }
        return Ok(());
    }
    // The seed-key hook goes to the flasher, not the driver.
    let seed_key = match &c.conn.seedkey {
        Some(id) => Some(ctx.require_seed_key(id)?),
        None => None,
    };
    std::fs::create_dir_all(&c.vault)
        .with_context(|| format!("create vault {}", c.vault.display()))?;
    let cfg = c.conn.config()?;
    let mut drv = ctx.open_driver(&c.conn.driver, &cfg, None)?;
    let state = opts.state_path.clone().unwrap_or_default();
    if state.exists() {
        eprintln!(
            "Resuming the interrupted flash recorded in {}",
            state.display()
        );
    }
    let mut progress = |p: &ModuleProgress| {
        if !p.message.is_empty() {
            eprintln!("[{}] {}", phase_name(p.phase), p.message);
        }
    };
    let res = flash_module_on(
        drv.as_mut(),
        &img,
        seed_key.as_deref(),
        &opts,
        &mut progress,
    );
    let _ = drv.close();
    let rep = match res {
        Ok(r) => r,
        Err(e) => {
            if state.exists() {
                eprintln!(
                    "Progress is saved in {} — run the same command again to resume from the \
                     failed block.",
                    state.display()
                );
            }
            return Err(anyhow!("module flash: {e}"));
        }
    };
    let id = EcuId {
        hw_number: rep.identity.part.first().map(|(_, v)| v.clone()),
        sw_number: rep.identity.software.first().map(|(_, v)| v.clone()),
        ..Default::default()
    };
    for b in &rep.backup {
        match opentune_flash::vault::save_backup(&c.vault, &id, b.region, &b.data) {
            Ok(p) => println!("Original archived: {}", p.display()),
            Err(e) => eprintln!("WARNING: could not archive the original: {e}"),
        }
    }
    if let Some(note) = &rep.backup_note {
        println!("No backup of the original: {note}");
    }
    if let Some(p) = write_log(c.transcript.as_deref().unwrap_or(&c.vault), &rep.log) {
        println!("Session transcript: {}", p.display());
    }
    for b in &rep.blocks {
        println!(
            "  block {}: 0x{:08X} +0x{:X} {} (verified: {:?})",
            b.index,
            b.region.address,
            b.region.length,
            if b.reused {
                "already done"
            } else {
                "programmed"
            },
            b.verify
        );
    }
    println!(
        "Module flash complete: {} block(s){}{}",
        rep.blocks.len(),
        if rep.resumed { ", resumed" } else { "" },
        if rep.reset_sent { ", ECU reset" } else { "" }
    );
    Ok(())
}
