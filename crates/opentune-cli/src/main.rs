//! OpenTune command-line interface.
//!
//! Wires the available interface drivers into a registry and exposes the
//! offline-capable operations (identify, read, diff, checksum) plus a
//! safety-gated flash. With the `mock` driver everything works on local files.
use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use opentune_core::{ConnectConfig, Interface, MemoryRegion, Progress, Registry};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "opentune",
    version,
    about = "Open ECU reading/flashing/calibration"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the interface drivers this build knows about.
    Drivers,
    /// Open a driver and print ECU identity + battery voltage.
    Info(ConnectArgs),
    /// Read a memory region to a file.
    Read {
        #[command(flatten)]
        conn: ConnectArgs,
        /// Start address, hex (e.g. 0x0).
        #[arg(long)]
        addr: String,
        /// Length in bytes, hex or decimal.
        #[arg(long)]
        len: String,
        /// Output file.
        #[arg(long)]
        out: PathBuf,
    },
    /// Byte-diff two images and summarise changed regions.
    Diff { a: PathBuf, b: PathBuf },
    /// Verify or correct checksums in an image.
    Checksum {
        #[command(subcommand)]
        action: ChecksumAction,
    },
    /// Write a region through the safety orchestrator (backup → write → verify).
    Flash {
        #[command(flatten)]
        conn: ConnectArgs,
        /// Address to write at (hex).
        #[arg(long)]
        addr: String,
        /// File of the exact bytes to write at `addr`.
        #[arg(long)]
        data: PathBuf,
        /// Minimum battery mV gate (0 disables).
        #[arg(long, default_value_t = 12_000)]
        min_mv: u32,
    },
}

#[derive(Subcommand)]
enum ChecksumAction {
    /// Report each protected block and whether it matches.
    Verify {
        /// Algorithm id (e.g. crc32-tail, edc16).
        #[arg(long)]
        algo: String,
        file: PathBuf,
    },
    /// Correct checksums in place (writes the file).
    Fix {
        #[arg(long)]
        algo: String,
        file: PathBuf,
    },
}

#[derive(clap::Args)]
struct ConnectArgs {
    /// Driver id (see `drivers`).
    #[arg(long, default_value = "mock")]
    driver: String,
    /// For the mock driver: the image file to use as the "ECU".
    #[arg(long)]
    file: Option<PathBuf>,
    /// Base address mapped to file offset 0 (hex).
    #[arg(long, default_value = "0x0")]
    base: String,
}

fn registry() -> Registry {
    let mut r = Registry::new();
    r.register("mock", "Mock / file driver", || {
        Box::new(opentune_driver_mock::MockDriver::new())
    });
    r.register("openport2", "OpenPort 2.0 (J2534/CDC-ACM)", || {
        Box::new(opentune_driver_openport2::OpenPort2Driver::new())
    });
    r.register("socketcan", "SocketCAN", || {
        Box::new(opentune_driver_socketcan::SocketCanDriver::new())
    });
    r.register("elm327", "K-line (ELM327)", || {
        Box::new(opentune_driver_elm327::Elm327Driver::new())
    });
    r
}

fn parse_num(s: &str) -> Result<u32> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x") {
        u32::from_str_radix(hex, 16).with_context(|| format!("bad hex: {s}"))
    } else {
        s.parse::<u32>().with_context(|| format!("bad number: {s}"))
    }
}

fn open_driver(reg: &Registry, conn: &ConnectArgs) -> Result<Box<dyn Interface>> {
    let mut drv = reg
        .make(&conn.driver)
        .ok_or_else(|| anyhow!("unknown driver '{}'", conn.driver))?;
    let mut cfg = ConnectConfig::default();
    if let Some(f) = &conn.file {
        cfg.options
            .insert("file".into(), f.to_string_lossy().into_owned());
    }
    cfg.options.insert("base".into(), conn.base.clone());
    drv.open(&cfg)
        .map_err(|e| anyhow!("open {}: {e}", conn.driver))?;
    Ok(drv)
}

fn noprogress(_: Progress) {}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let reg = registry();

    match cli.cmd {
        Command::Drivers => {
            println!("Available drivers:");
            for e in reg.list() {
                println!("  {:<12} {}", e.id, e.display_name);
            }
        }
        Command::Info(conn) => {
            let mut drv = open_driver(&reg, &conn)?;
            let info = drv.info();
            println!("Interface : {} ({})", info.display_name, info.id);
            if let Some(mv) = drv.battery_millivolts()? {
                println!("Battery   : {:.2} V", mv as f64 / 1000.0);
            }
            let id = drv.read_ecu_id()?;
            println!("ECU       : {}", id.ecu_name.unwrap_or_else(|| "?".into()));
            if let Some(sw) = id.sw_number {
                println!("SW        : {sw}");
            }
            if let Some(hw) = id.hw_number {
                println!("HW        : {hw}");
            }
            if let Some(vin) = id.vin {
                println!("VIN       : {vin}");
            }
            drv.close()?;
        }
        Command::Read {
            conn,
            addr,
            len,
            out,
        } => {
            let mut drv = open_driver(&reg, &conn)?;
            let region = MemoryRegion::new(parse_num(&addr)?, parse_num(&len)?);
            let data = drv.read_memory(region, &mut noprogress)?;
            std::fs::write(&out, &data)?;
            println!(
                "Read {} bytes from 0x{:08X} -> {}",
                data.len(),
                region.address,
                out.display()
            );
            drv.close()?;
        }
        Command::Diff { a, b } => {
            let da = std::fs::read(&a).with_context(|| format!("read {}", a.display()))?;
            let db = std::fs::read(&b).with_context(|| format!("read {}", b.display()))?;
            diff(&da, &db);
        }
        Command::Checksum { action } => match action {
            ChecksumAction::Verify { algo, file } => {
                let algo = opentune_checksum::get(&algo)
                    .ok_or_else(|| anyhow!("unknown checksum algo '{algo}'"))?;
                let img = std::fs::read(&file)?;
                for b in algo.verify(&img)? {
                    println!(
                        "{:<16} {} stored=0x{:08X} computed=0x{:08X}",
                        b.name,
                        if b.ok { "OK  " } else { "BAD " },
                        b.stored,
                        b.computed
                    );
                }
            }
            ChecksumAction::Fix { algo, file } => {
                let algo = opentune_checksum::get(&algo)
                    .ok_or_else(|| anyhow!("unknown checksum algo '{algo}'"))?;
                let mut img = std::fs::read(&file)?;
                let fixes = algo.correct(&mut img)?;
                if fixes.is_empty() {
                    println!("No corrections needed.");
                } else {
                    std::fs::write(&file, &img)?;
                    for f in fixes {
                        println!(
                            "fixed {:<16} @0x{:08X} 0x{:08X} -> 0x{:08X}",
                            f.name, f.offset, f.old, f.new
                        );
                    }
                }
            }
        },
        Command::Flash {
            conn,
            addr,
            data,
            min_mv,
        } => {
            let mut drv = open_driver(&reg, &conn)?;
            let bytes = std::fs::read(&data).with_context(|| format!("read {}", data.display()))?;
            let region = MemoryRegion::new(parse_num(&addr)?, bytes.len() as u32);
            let safety = opentune_flash::SafetyConfig {
                min_millivolts: min_mv,
                ..Default::default()
            };
            let report = opentune_flash::flash_calibration(
                drv.as_mut(),
                region,
                &bytes,
                &safety,
                &mut noprogress,
            )
            .map_err(|e| anyhow!("flash: {e}"))?;
            println!(
                "Flashed {} bytes @0x{:08X} — backup {} bytes, verified={}",
                report.bytes_written,
                region.address,
                report.backup.len(),
                report.verified
            );
            drv.close()?;
        }
    }
    Ok(())
}

/// Summarise the byte differences between two images as contiguous ranges.
fn diff(a: &[u8], b: &[u8]) {
    if a.len() != b.len() {
        println!("size differs: {} vs {} bytes", a.len(), b.len());
    }
    let n = a.len().min(b.len());
    let mut changed = 0usize;
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        if a[i] != b[i] {
            let start = i;
            while i < n && a[i] != b[i] {
                changed += 1;
                i += 1;
            }
            ranges.push((start, i));
        } else {
            i += 1;
        }
    }
    println!(
        "{} differing bytes in {} region(s) over {} compared bytes",
        changed,
        ranges.len(),
        n
    );
    for (s, e) in ranges.iter().take(40) {
        println!("  0x{:08X}..0x{:08X}  ({} bytes)", s, e, e - s);
    }
    if ranges.len() > 40 {
        println!("  … and {} more regions", ranges.len() - 40);
    }
}
