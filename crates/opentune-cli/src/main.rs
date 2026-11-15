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
    /// Full read via a driver, archived to the backup vault with ECU id.
    Dump {
        #[command(flatten)]
        conn: ConnectArgs,
        /// Start address (hex), default 0.
        #[arg(long, default_value = "0x0")]
        addr: String,
        /// Length to read (hex or decimal).
        #[arg(long)]
        len: String,
        /// Vault directory to archive the dump into.
        #[arg(long, default_value = "backups")]
        vault: PathBuf,
    },
    /// Find candidate calibration maps by clustering the diff of two images.
    Findmaps {
        /// Stock image.
        stock: PathBuf,
        /// Modified image.
        tuned: PathBuf,
        /// Merge changed bytes closer than this many bytes into one region.
        #[arg(long, default_value_t = 16)]
        gap: usize,
        /// Minimum region length (bytes) to report.
        #[arg(long, default_value_t = 2)]
        min_len: usize,
        /// Optionally write the candidate regions as JSON.
        #[arg(long)]
        emit: Option<PathBuf>,
    },
    /// List ECU definitions in a directory.
    Ecudefs {
        /// Directory of *.json ECU definitions.
        dir: PathBuf,
    },
    /// Read the connected ECU's identity and match it to a definition/catalog.
    Identify {
        #[command(flatten)]
        conn: ConnectArgs,
        /// Directory of ECU definitions to match against.
        #[arg(long, default_value = "ecudefs")]
        ecudefs: PathBuf,
        /// Optional catalog directory (harvested vehicle→ECU data).
        #[arg(long)]
        catalog: Option<PathBuf>,
    },
    /// Look up a diagnostic trouble code (e.g. P0101).
    Dtc { code: String },
    /// Import a TunerPro XDF definition and list (or merge) its maps.
    ImportXdf {
        /// Path to the .xdf file (modern XML or legacy text format).
        file: PathBuf,
        /// Optional ECU-definition JSON to merge the imported maps into
        /// (existing maps with the same name are left untouched).
        #[arg(long)]
        into: Option<PathBuf>,
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
    r.register("kkl", "K-line (KKL 409.1 raw, FTDI/CH340)", || {
        Box::new(opentune_driver_kkl::KklDriver::new())
    });
    r.register("kline-bridge", "Arduino K-line bridge", || {
        Box::new(opentune_driver_kline_bridge::BridgeDriver::new())
    });
    r.register("slcan", "slcan USB-CAN (CANable/CANtact)", || {
        Box::new(opentune_driver_slcan::SlcanDriver::new())
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
        Command::Dump {
            conn,
            addr,
            len,
            vault,
        } => {
            let mut drv = open_driver(&reg, &conn)?;
            let region = MemoryRegion::new(parse_num(&addr)?, parse_num(&len)?);
            let id = drv.read_ecu_id().unwrap_or_default();
            let data = drv.read_memory(region, &mut noprogress)?;
            let path = opentune_flash::vault::save_backup(&vault, &id, region, &data)
                .map_err(|e| anyhow!("vault: {e}"))?;
            println!(
                "Dumped {} bytes @0x{:08X} -> {}",
                data.len(),
                region.address,
                path.display()
            );
            drv.close()?;
        }
        Command::Findmaps {
            stock,
            tuned,
            gap,
            min_len,
            emit,
        } => {
            let a = std::fs::read(&stock).with_context(|| format!("read {}", stock.display()))?;
            let b = std::fs::read(&tuned).with_context(|| format!("read {}", tuned.display()))?;
            let regions = find_maps(&a, &b, gap, min_len);
            println!(
                "{} candidate map region(s) (gap={gap}, min_len={min_len}):",
                regions.len()
            );
            for (i, (s, e)) in regions.iter().enumerate() {
                println!(
                    "  map{:<3} 0x{:08X}..0x{:08X}  {:>6} bytes{}",
                    i,
                    s,
                    e,
                    e - s,
                    guess_stride(e - s)
                );
            }
            if let Some(path) = emit {
                let arr: Vec<_> = regions
                    .iter()
                    .enumerate()
                    .map(|(i, (s, e))| {
                        serde_json::json!({
                            "name": format!("map{i}"),
                            "address": s,
                            "length": e - s,
                            "notes": "auto-detected from stock/tuned diff; confirm type/axes",
                        })
                    })
                    .collect();
                std::fs::write(&path, serde_json::to_string_pretty(&arr)?)?;
                println!("wrote {} candidate(s) -> {}", regions.len(), path.display());
            }
        }
        Command::Ecudefs { dir } => {
            let defs = opentune_core::ecudef::load_dir(&dir).map_err(|e| anyhow!("{e}"))?;
            println!("{} ECU definition(s):", defs.len());
            for d in &defs {
                println!(
                    "  {:<22} {:<16} {:>5} KiB  {:?}",
                    d.id,
                    d.name,
                    d.flash_size / 1024,
                    d.protocol
                );
            }
        }
        Command::Identify {
            conn,
            ecudefs,
            catalog,
        } => {
            let mut drv = open_driver(&reg, &conn)?;
            let id = drv.read_ecu_id()?;
            println!("Reported identity:");
            if let Some(v) = &id.ecu_name {
                println!("  ECU name : {v}");
            }
            if let Some(v) = &id.hw_number {
                println!("  HW/part  : {v}");
            }
            if let Some(v) = &id.sw_number {
                println!("  SW number: {v}");
            }
            if let Some(v) = &id.supplier {
                println!("  Supplier : {v}");
            }
            if let Some(v) = &id.vin {
                println!("  VIN      : {v}");
            }
            let defs = opentune_core::ecudef::load_dir(&ecudefs).unwrap_or_default();
            match opentune_core::ecudef::match_ecu(&defs, &id) {
                Some(d) => println!(
                    "\nMatched definition: {} ({}) — protocol {:?}, checksum {:?}, seedkey {:?}",
                    d.id, d.name, d.protocol, d.checksum, d.seedkey
                ),
                None => println!("\nNo exact definition match in {}", ecudefs.display()),
            }
            if let Some(cat) = catalog {
                let entries = opentune_core::catalog::load_catalog(&cat).unwrap_or_default();
                let hits = opentune_core::catalog::match_catalog(&entries, &id);
                if hits.is_empty() {
                    println!("No catalog candidates.");
                } else {
                    println!("Catalog candidates:");
                    for h in hits.iter().take(10) {
                        println!(
                            "  {} {} [{}] {}",
                            h.make,
                            h.models.join("/"),
                            h.family.clone().unwrap_or_default(),
                            h.source.clone().unwrap_or_default()
                        );
                    }
                }
            }
            drv.close()?;
        }
        Command::Dtc { code } => match opentune_dtc::lookup(&code) {
            Some(d) => println!("{}  [{:?}]  {}", d.code, d.system, d.description),
            None => println!("{code}: malformed code"),
        },
        Command::ImportXdf { file, into } => {
            // XDF payloads in the wild are sometimes Windows-1251/Latin-1, not
            // UTF-8. The structure is ASCII; decode lossily so a non-UTF-8 unit
            // string can't abort the import (affected text is flagged, not lost).
            let raw = std::fs::read(&file).map_err(|e| anyhow!("read {}: {e}", file.display()))?;
            let src = String::from_utf8_lossy(&raw);
            let import = opentune_xdf::parse_xdf(&src).map_err(|e| anyhow!("{e:?}"))?;
            println!(
                "XDF: {}  (base offset 0x{:X}) — {} map(s)",
                import.title.as_deref().unwrap_or("(untitled)"),
                import.base_offset,
                import.maps.len()
            );
            for m in &import.maps {
                let note = if m.notes.is_empty() {
                    String::new()
                } else {
                    format!("  ! {}", m.notes)
                };
                println!(
                    "  {:<28} @0x{:06X}  {}x{}  {:?}/{:?}  x{}{:+}{}",
                    m.name,
                    m.address,
                    m.rows,
                    m.cols,
                    m.data_type,
                    m.endian,
                    m.scaling.factor,
                    m.scaling.offset,
                    note
                );
            }
            if let Some(into) = into {
                let txt = std::fs::read_to_string(&into)
                    .map_err(|e| anyhow!("read {}: {e}", into.display()))?;
                let mut def: opentune_core::ecudef::EcuDef =
                    serde_json::from_str(&txt).map_err(|e| anyhow!("parse {}: {e}", into.display()))?;
                let have: std::collections::HashSet<String> =
                    def.maps.iter().map(|m| m.name.clone()).collect();
                let mut added = 0usize;
                for m in import.maps {
                    if have.contains(&m.name) {
                        continue;
                    }
                    def.maps.push(m);
                    added += 1;
                }
                let out = serde_json::to_string_pretty(&def)?;
                std::fs::write(&into, out).map_err(|e| anyhow!("write {}: {e}", into.display()))?;
                println!(
                    "Merged {added} new map(s) into {} ({} total).",
                    into.display(),
                    def.maps.len()
                );
            }
        }
    }
    Ok(())
}

/// Cluster the byte differences between two images into candidate map regions,
/// merging changes separated by fewer than `gap` unchanged bytes.
fn find_maps(a: &[u8], b: &[u8], gap: usize, min_len: usize) -> Vec<(usize, usize)> {
    let n = a.len().min(b.len());
    let mut regions = Vec::new();
    let mut i = 0;
    while i < n {
        if a[i] != b[i] {
            let start = i;
            let mut end = i + 1;
            let mut since_change = 0usize;
            let mut j = i + 1;
            while j < n && since_change <= gap {
                if a[j] != b[j] {
                    end = j + 1;
                    since_change = 0;
                } else {
                    since_change += 1;
                }
                j += 1;
            }
            regions.push((start, end));
            i = end;
        } else {
            i += 1;
        }
    }
    regions
        .into_iter()
        .filter(|(s, e)| e - s >= min_len)
        .collect()
}

/// Hint whether a region's size looks like a table of 16-bit cells.
fn guess_stride(len: usize) -> String {
    if len >= 4 && len.is_multiple_of(2) {
        let cells = len / 2;
        for &w in &[16usize, 12, 10, 8] {
            if cells.is_multiple_of(w) {
                return format!("  (~{}x{} u16?)", cells / w, w);
            }
        }
        return format!("  (~{cells} u16 cells?)");
    }
    String::new()
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
