//! `opentune bsl`: C167/ST10 bootstrap-mode (BSL) memory read for EDC15
//! boards. `plan` is a dry run that opens no port; `read` is unverified on
//! hardware and refuses to run without `--i-understand`. Read only: nothing
//! here writes or erases flash or EEPROM. See docs/C167-BSL.md.
use crate::util::parse_num;
use anyhow::{anyhow, bail, Context, Result};
use opentune_c167bsl::plan::{build_with_registers, PlanInput, ReadPlan, RenderOptions};
use opentune_c167bsl::protocol;
use opentune_c167bsl::{
    find_preset, run_plan, Exchange, LinkOptions, Port, Preset, Region, SerialPort, SimC167,
    SimConfig, PRESETS,
};
use opentune_flash::hash::sha256_hex;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Baud `plan` assumes when none is given.
const PLAN_DEFAULT_BAUD: u32 = 57_600;

#[derive(clap::Subcommand)]
pub enum BslAction {
    /// List the memory presets (EDC15VM+/P+, EDC15C6, generic C167) and
    /// their register setup.
    Presets,
    /// Dry run: print every step and byte exchange a read would do. Opens no
    /// port; the loader/kernel files are optional.
    Plan(PlanArgs),
    /// Read memory in bootstrap mode. UNVERIFIED on hardware: needs
    /// --i-understand. `--port sim` runs against the simulated C167.
    Read(ReadArgs),
}

#[derive(clap::Args)]
pub struct Target {
    /// Board preset (see `opentune bsl presets`).
    #[arg(long)]
    preset: String,
    /// Region of the preset (default: its first, the external flash on EDC15).
    #[arg(long)]
    region: Option<String>,
    /// Offset into the region (hex or decimal).
    #[arg(long, default_value = "0", value_parser = parse_num)]
    offset: u32,
    /// Bytes to read (default: the region's default length).
    #[arg(long, value_parser = parse_num)]
    len: Option<u32>,
    /// Bytes per block read (max 0x200).
    #[arg(long, default_value = "0x200", value_parser = parse_num)]
    block: u32,
    /// The 32-byte primary loader (user-supplied; none ships with OpenTune).
    #[arg(long, value_name = "FILE")]
    loader: Option<PathBuf>,
    /// The monitor kernel the loader pulls in (user-supplied).
    #[arg(long, value_name = "FILE")]
    kernel: Option<PathBuf>,
    /// Supply an unknown board SFR in programming order (repeat NAME=VALUE).
    #[arg(long, value_name = "NAME=VALUE", conflicts_with = "no_setup")]
    register: Vec<String>,
    /// Skip the bus / chip-select register setup.
    #[arg(long)]
    no_setup: bool,
    /// Full-duplex link (TTL/RS232 straight to RxD0/TxD0): no echo expected.
    /// Default is half-duplex K-line, every sent byte echoed and checked.
    #[arg(long)]
    no_echo: bool,
    /// Serial baud; the BSL adapts to it (autobaud). Required for `read` on
    /// a real port; `plan` assumes 57600.
    #[arg(long)]
    baud: Option<u32>,
    /// ECU CPU clock in MHz, to check the BSL's baud deviation (≤ 2.5 %).
    #[arg(long, value_name = "MHZ")]
    fcpu_mhz: Option<f64>,
}

#[derive(clap::Args)]
pub struct PlanArgs {
    #[command(flatten)]
    target: Target,
    /// Print every block read instead of the first and last.
    #[arg(long)]
    all_blocks: bool,
}

#[derive(clap::Args)]
pub struct ReadArgs {
    #[command(flatten)]
    target: Target,
    /// Serial device of the K-line (KKL) cable, or `sim`.
    #[arg(long)]
    port: String,
    /// Output file for the read.
    #[arg(long)]
    out: PathBuf,
    /// Acknowledge that this path is unverified on hardware and that the ECU
    /// is on a bench, strapped into bootstrap mode.
    #[arg(long)]
    i_understand: bool,
    /// Write the byte-level transcript (every send, echo and reply) here.
    #[arg(long, value_name = "FILE")]
    transcript: Option<PathBuf>,
    /// Reply timeout per step in ms (on top of the wire time).
    #[arg(long, default_value_t = 1500)]
    timeout_ms: u64,
}

pub fn run(action: BslAction) -> Result<()> {
    match action {
        BslAction::Presets => presets(),
        BslAction::Plan(a) => plan(a),
        BslAction::Read(a) => read(a),
    }
}

fn presets() -> Result<()> {
    for p in PRESETS {
        println!("{}  — {}", p.id, p.name);
        println!(
            "    accepts ID [{}]; {}",
            protocol::hex(p.accept_ids),
            p.notes
        );
        for r in p.regions {
            println!(
                "    {:<10} {:#08x} default {:#x}, window {:#x} — {}",
                r.id, r.base, r.default_len, r.max_len, r.description
            );
            for (name, _) in r.unknown_setup {
                println!("        {name}: unknown/unverified");
            }
            for w in r.setup {
                let d = protocol::describe_register(w.addr, w.value);
                println!(
                    "        {:<8} @ {:#06x} := {:#06x}  {d}",
                    w.name, w.addr, w.value
                );
            }
            println!(
                "        source: {}{}",
                r.source,
                if r.verified { "" } else { " (unverified)" }
            );
        }
    }
    Ok(())
}

struct Payloads {
    loader: Option<Vec<u8>>,
    loader_sha: Option<String>,
    kernel: Option<Vec<u8>>,
}

fn load(path: &Option<PathBuf>) -> Result<Option<Vec<u8>>> {
    path.as_ref()
        .map(|p| std::fs::read(p).with_context(|| format!("read {}", p.display())))
        .transpose()
}

fn payloads(t: &Target) -> Result<Payloads> {
    let loader = load(&t.loader)?;
    let loader_sha = loader.as_deref().map(sha256_hex);
    Ok(Payloads {
        loader,
        loader_sha,
        kernel: load(&t.kernel)?,
    })
}

fn target(t: &Target) -> Result<(&'static Preset, &'static Region)> {
    let preset = find_preset(&t.preset).ok_or_else(|| {
        let ids: Vec<&str> = PRESETS.iter().map(|p| p.id).collect();
        anyhow!("unknown preset '{}' (known: {})", t.preset, ids.join(", "))
    })?;
    let region = match &t.region {
        Some(r) => preset.region(r).ok_or_else(|| {
            let ids: Vec<&str> = preset.regions.iter().map(|r| r.id).collect();
            anyhow!(
                "preset {} has no region '{r}' (known: {})",
                preset.id,
                ids.join(", ")
            )
        })?,
        None => &preset.regions[0],
    };
    Ok((preset, region))
}

fn build(t: &Target, p: &Payloads) -> Result<ReadPlan> {
    let (preset, region) = target(t)?;
    let block =
        u16::try_from(t.block).map_err(|_| anyhow!("block size {:#x} too large", t.block))?;
    let mut values = Vec::new();
    for arg in &t.register {
        let (name, value) = arg
            .split_once('=')
            .context("expected --register NAME=VALUE")?;
        values.push((
            name,
            u16::try_from(parse_num(value)?).context("register value exceeds 16 bits")?,
        ));
    }
    Ok(build_with_registers(
        &PlanInput {
            preset,
            region,
            offset: t.offset,
            len: t.len,
            block,
            loader: p.loader.as_deref(),
            loader_sha256: p.loader_sha.as_deref(),
            kernel: p.kernel.as_deref(),
            setup: !t.no_setup,
        },
        &values,
    )?)
}

fn fcpu_hz(t: &Target) -> Result<Option<u32>> {
    match t.fcpu_mhz {
        Some(m) if m > 0.0 && m < 100.0 => Ok(Some((m * 1_000_000.0).round() as u32)),
        Some(m) => bail!("--fcpu-mhz {m} is not a plausible C167 clock"),
        None => Ok(None),
    }
}

fn plan(a: PlanArgs) -> Result<()> {
    let t = &a.target;
    let p = payloads(t)?;
    let plan = build(t, &p)?;
    let baud = t.baud.unwrap_or(PLAN_DEFAULT_BAUD);
    println!("DRY RUN — nothing is sent, no port is opened.");
    if t.baud.is_none() {
        println!("(baud {baud} assumed; pass --baud)");
    }
    print!(
        "{}",
        plan.render(&RenderOptions {
            echo: !t.no_echo,
            baud,
            fcpu_hz: fcpu_hz(t)?,
            all_blocks: a.all_blocks,
        })
    );
    Ok(())
}

fn read(a: ReadArgs) -> Result<()> {
    let t = &a.target;
    if !a.i_understand {
        bail!(
            "`opentune bsl read` is UNVERIFIED on hardware. It needs the ECU on a bench, \
             strapped into bootstrap mode, and a user-supplied loader + kernel. Run \
             `opentune bsl plan` first, read docs/C167-BSL.md, then pass --i-understand."
        );
    }
    let sim = a.port.eq_ignore_ascii_case("sim");
    let mut p = payloads(t)?;
    if sim {
        // The simulator takes any 32-byte loader and a kernel of the length
        // it is configured for; synthesise them when not given.
        p.loader.get_or_insert_with(|| vec![0xA5; 32]);
        p.kernel.get_or_insert_with(|| vec![0x5A; 394]);
    } else if p.loader.is_none() || p.kernel.is_none() {
        bail!("a real read needs --loader and --kernel (no loader ships with OpenTune)");
    }
    let plan = build(t, &p)?;
    plan.ensure_setup_complete()?;
    let baud = match (t.baud, sim) {
        (Some(b), _) => b,
        (None, true) => PLAN_DEFAULT_BAUD,
        (None, false) => bail!("--baud is required for a real read (the BSL adapts to it)"),
    };
    if let Some(f) = fcpu_hz(t)? {
        let c = protocol::baud_check(f, baud);
        if !c.ok {
            bail!(
                "baud {baud} at {:.1} MHz: the BSL would run at {:.0} baud ({:+.2} %, limit 2.5 %); \
                 pick another --baud",
                f as f64 / 1e6,
                c.bsl_baud,
                c.deviation_pct
            );
        }
    }
    let echo = !t.no_echo;

    let mut port: Box<dyn Port> = if sim {
        println!("port         sim (simulated C167 BSL, synthetic memory, no hardware)");
        Box::new(sim_ecu(
            &plan,
            echo,
            p.kernel.as_ref().map_or(394, Vec::len),
        ))
    } else {
        eprintln!("WARNING: unverified bootstrap-mode read on {} — read only, but the ECU is driven outside its normal program.", a.port);
        let s = SerialPort::open(&a.port, baud).with_context(|| format!("open {}", a.port))?;
        println!("port         {} @ {baud} baud 8N1", a.port);
        Box::new(s)
    };
    println!(
        "read         {} / {} {:#08x}..{:#08x} ({} bytes)",
        plan.preset.id,
        plan.region.id,
        plan.start,
        plan.start as u64 + plan.len as u64 - 1,
        plan.len
    );
    let opts = LinkOptions {
        reply_timeout: Duration::from_millis(a.timeout_ms),
        ..LinkOptions::new(baud, echo)
    };
    let mut transcript: Vec<Exchange> = Vec::new();
    let mut last_pct = u32::MAX;
    let result = run_plan(
        port.as_mut(),
        &plan,
        &opts,
        &mut transcript,
        &mut |done, total| {
            let pct = (done as u64 * 100 / total.max(1) as u64) as u32;
            if pct / 10 != last_pct / 10 || done == total {
                eprint!("\r  {done}/{total} bytes ({pct} %)");
                last_pct = pct;
            }
        },
    );
    eprintln!();
    if let Some(path) = &a.transcript {
        write_transcript(path, &transcript)?;
        println!(
            "transcript   {} ({} exchanges)",
            path.display(),
            transcript.len()
        );
    }
    let out = result.map_err(|e| anyhow!("{e} (after {} exchanges)", transcript.len()))?;
    match out.chip {
        Some(c) => println!("chip         {} (ID {:#04x})", c.name(), out.id_byte),
        None => println!("chip         monitor kernel was already running (no ID byte)"),
    }
    std::fs::write(&a.out, &out.data).with_context(|| format!("write {}", a.out.display()))?;
    println!(
        "wrote        {} ({} bytes, sha256 {})",
        a.out.display(),
        out.data.len(),
        sha256_hex(&out.data)
    );
    println!("status       every block checksum matched; the read path is UNVERIFIED on hardware");
    Ok(())
}

/// The simulated ECU for `--port sim`: the region holds a deterministic
/// pattern, readable once the region's setup registers are programmed.
fn sim_ecu(plan: &ReadPlan, echo: bool, kernel_len: usize) -> SimC167 {
    let (start, len) = (plan.start, plan.len);
    let bytes: Vec<u8> = (0..len)
        .map(|i| {
            let a = start.wrapping_add(i);
            (a ^ (a >> 8) ^ (a >> 16)) as u8
        })
        .collect();
    let requires = if plan.region.unknown_setup.is_empty() {
        plan.region
            .setup
            .iter()
            .filter(|w| w.value != 0)
            .map(|w| (w.addr, w.value))
            .collect::<Vec<_>>()
    } else {
        plan.steps
            .iter()
            .filter_map(|step| match step.expect {
                opentune_c167bsl::Expect::Word { addr, expect, .. } => Some((addr, expect)),
                _ => None,
            })
            .collect()
    };
    SimC167::new(SimConfig {
        echo,
        kernel_len,
        ..SimConfig::default()
    })
    .with_memory(start, bytes, &requires)
}

fn write_transcript(path: &Path, t: &[Exchange]) -> Result<()> {
    let mut f = std::io::BufWriter::new(
        std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?,
    );
    for e in t {
        writeln!(f, "{}", e.line())?;
    }
    f.flush()?;
    Ok(())
}
