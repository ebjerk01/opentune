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
        /// Directory to archive the pre-flash backup into (the safety net).
        #[arg(long, default_value = "vault")]
        vault: PathBuf,
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
    /// Compare two calibrations map-by-map through an ECU definition.
    Compare {
        /// Directory of ECU definitions.
        #[arg(long, default_value = "ecudefs")]
        ecudefs: PathBuf,
        /// Definition id (see `ecudefs`) whose maps describe the images.
        #[arg(long)]
        def: String,
        /// Original / stock image.
        stock: PathBuf,
        /// Modified / tuned image.
        tuned: PathBuf,
        /// Show the full before/after grid for just this map (by name substring).
        #[arg(long)]
        map: Option<String>,
    },
    /// Import a TunerPro XDF definition and list (or merge) its maps.
    ImportXdf {
        /// Path to the .xdf file (modern XML or legacy text format).
        file: PathBuf,
        /// Optional ECU-definition JSON to merge the imported maps into
        /// (existing maps with the same name are left untouched).
        #[arg(long)]
        into: Option<PathBuf>,
    },
    /// List archived reads in a backup vault directory.
    Vault {
        /// Vault directory to scan (default: ./vault).
        #[arg(default_value = "vault")]
        dir: PathBuf,
        /// Re-read each .bin and check its CRC32 against the .meta sidecar.
        #[arg(long)]
        verify: bool,
    },
    /// Import an ASAM A2L description and list (or merge) its calibration maps.
    ImportA2l {
        /// Path to the .a2l file.
        file: PathBuf,
        /// Optional ECU-definition JSON to merge the imported maps into
        /// (existing maps with the same name are left untouched).
        #[arg(long)]
        into: Option<PathBuf>,
        /// Base to subtract from each A2L CPU/logical address to get the bin
        /// file offset (hex ok, e.g. 0xA0000000 for TriCore). A2L addresses are
        /// logical, not file offsets, and — unlike XDF — the file carries no
        /// base, so supply the ECU's memory-map base here.
        #[arg(long, default_value = "0")]
        base: String,
    },
    /// Export an ECU definition's maps as a TunerPro XDF (modern XML).
    ExportXdf {
        /// Directory of ECU definitions.
        #[arg(long, default_value = "ecudefs")]
        ecudefs: PathBuf,
        /// Definition id whose maps to export.
        #[arg(long)]
        def: String,
        /// Output .xdf path (defaults to stdout).
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
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
        /// Known-good reference image (your own untouched stock dump) used to
        /// recover per-block constants the library doesn't ship — needed for
        /// EDC17 (VAG constants are firmware-specific); ignored by self-contained
        /// algorithms (edc16/pcr21/edc15c).
        #[arg(long)]
        reference: Option<PathBuf>,
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
            ChecksumAction::Fix {
                algo,
                file,
                reference,
            } => {
                let algo = opentune_checksum::get(&algo)
                    .ok_or_else(|| anyhow!("unknown checksum algo '{algo}'"))?;
                let mut img = std::fs::read(&file)?;
                let fixes = match reference {
                    Some(r) => {
                        let refimg = std::fs::read(&r)
                            .with_context(|| format!("read reference {}", r.display()))?;
                        algo.correct_with_reference(&mut img, &refimg)?
                    }
                    None => algo.correct(&mut img)?,
                };
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
            vault,
        } => {
            let mut drv = open_driver(&reg, &conn)?;
            let bytes = std::fs::read(&data).with_context(|| format!("read {}", data.display()))?;
            let region = MemoryRegion::new(parse_num(&addr)?, bytes.len() as u32);
            // Capture ECU identity up front so the archived backup is labelled.
            let id = drv.read_ecu_id().unwrap_or_default();
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
            // Persist the pre-flash backup — the safety net if the write is bad.
            if !report.backup.is_empty() {
                match opentune_flash::vault::save_backup(&vault, &id, region, &report.backup) {
                    Ok(p) => println!("Pre-flash backup archived: {}", p.display()),
                    Err(e) => eprintln!(
                        "WARNING: could not archive backup to {}: {e}",
                        vault.display()
                    ),
                }
            }
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
        Command::Compare {
            ecudefs,
            def,
            stock,
            tuned,
            map,
        } => {
            let defs = opentune_core::ecudef::load_dir(&ecudefs)
                .map_err(|e| anyhow!("load {}: {e}", ecudefs.display()))?;
            let d = defs
                .iter()
                .find(|d| d.id == def)
                .ok_or_else(|| anyhow!("no definition '{def}' in {}", ecudefs.display()))?;
            if d.maps.is_empty() {
                println!("Definition '{def}' has no maps to compare.");
                return Ok(());
            }
            let a = std::fs::read(&stock).with_context(|| format!("read {}", stock.display()))?;
            let b = std::fs::read(&tuned).with_context(|| format!("read {}", tuned.display()))?;
            compare_maps(d, &a, &b, map.as_deref());
        }
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
                let mut def: opentune_core::ecudef::EcuDef = serde_json::from_str(&txt)
                    .map_err(|e| anyhow!("parse {}: {e}", into.display()))?;
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
        Command::ImportA2l { file, into, base } => {
            let base = parse_num(&base)?;
            // A2L text can be Latin-1/Win-1252; decode lossily (ASCII structure).
            let raw = std::fs::read(&file).map_err(|e| anyhow!("read {}: {e}", file.display()))?;
            let src = String::from_utf8_lossy(&raw);
            let import = opentune_a2l::parse_a2l(&src).map_err(|e| anyhow!("{e}"))?;
            println!(
                "A2L: project {:?} module {:?} — {} map(s)  (base 0x{:X})",
                import.project.as_deref().unwrap_or("?"),
                import.module.as_deref().unwrap_or("?"),
                import.maps.len(),
                base
            );
            // Rebase CPU addresses to bin file offsets.
            let mut maps = import.maps;
            for m in &mut maps {
                m.address = m.address.wrapping_sub(base);
                if let Some(ax) = m.x_axis.as_mut() {
                    if let Some(a) = ax.address.as_mut() {
                        *a = a.wrapping_sub(base);
                    }
                }
                if let Some(ax) = m.y_axis.as_mut() {
                    if let Some(a) = ax.address.as_mut() {
                        *a = a.wrapping_sub(base);
                    }
                }
            }
            for m in maps.iter().take(60) {
                let note = if m.notes.is_empty() {
                    String::new()
                } else {
                    format!("  ! {}", m.notes)
                };
                println!(
                    "  {:<34} @0x{:06X}  {}x{}  {:?}/{:?}  x{}{:+}{}",
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
            if maps.len() > 60 {
                println!("  … and {} more", maps.len() - 60);
            }
            if let Some(into) = into {
                let txt = std::fs::read_to_string(&into)
                    .map_err(|e| anyhow!("read {}: {e}", into.display()))?;
                let mut def: opentune_core::ecudef::EcuDef = serde_json::from_str(&txt)
                    .map_err(|e| anyhow!("parse {}: {e}", into.display()))?;
                let have: std::collections::HashSet<String> =
                    def.maps.iter().map(|m| m.name.clone()).collect();
                let mut added = 0usize;
                for m in maps {
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
        Command::Vault { dir, verify } => {
            if !dir.is_dir() {
                return Err(anyhow!("vault directory not found: {}", dir.display()));
            }
            let mut metas: Vec<PathBuf> = std::fs::read_dir(&dir)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().map(|x| x == "meta").unwrap_or(false))
                .collect();
            metas.sort();
            if metas.is_empty() {
                println!("No backups in {}", dir.display());
                return Ok(());
            }
            println!("{} backup(s) in {}:", metas.len(), dir.display());
            for meta in &metas {
                let txt = std::fs::read_to_string(meta).unwrap_or_default();
                let field = |k: &str| -> String {
                    txt.lines()
                        .find_map(|l| l.split_once('=').filter(|(key, _)| key.trim() == k))
                        .map(|(_, v)| v.trim().to_string())
                        .unwrap_or_default()
                };
                let bin = meta.with_extension("bin");
                let name = bin
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let sw = field("sw_number");
                let addr = field("address");
                let len = field("length");
                let stored_crc = field("crc32");
                let mut status = String::new();
                if verify {
                    status = match std::fs::read(&bin) {
                        Ok(data) => {
                            let got = format!("0x{:08X}", opentune_checksum::crc32(&data));
                            if got.eq_ignore_ascii_case(&stored_crc) {
                                "  [OK]".into()
                            } else {
                                format!("  [MISMATCH got {got}]")
                            }
                        }
                        Err(_) => "  [.bin missing]".into(),
                    };
                }
                println!(
                    "  {:<40} sw={:<12} @{} {} bytes crc={}{}",
                    name, sw, addr, len, stored_crc, status
                );
            }
        }
        Command::ExportXdf { ecudefs, def, out } => {
            let defs = opentune_core::ecudef::load_dir(&ecudefs)
                .map_err(|e| anyhow!("load {}: {e}", ecudefs.display()))?;
            let d = defs
                .iter()
                .find(|d| d.id == def)
                .ok_or_else(|| anyhow!("no definition '{def}' in {}", ecudefs.display()))?;
            if d.maps.is_empty() {
                return Err(anyhow!("definition '{def}' has no maps to export"));
            }
            let xml = opentune_xdf::export_xdf(Some(&d.name), &d.maps);
            match out {
                Some(p) => {
                    std::fs::write(&p, &xml).map_err(|e| anyhow!("write {}: {e}", p.display()))?;
                    println!("Wrote {} map(s) to {}", d.maps.len(), p.display());
                }
                None => print!("{xml}"),
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

/// Read one map cell's RAW value at a byte offset, honoring type + byte order.
fn read_cell(
    img: &[u8],
    off: usize,
    dt: opentune_core::ecudef::DataType,
    endian: opentune_core::ecudef::Endian,
) -> Option<f64> {
    use opentune_core::ecudef::{DataType as T, Endian as E};
    let w = dt.byte_len() as usize;
    let b = img.get(off..off + w)?;
    let le = matches!(endian, E::Little);
    let v = match dt {
        T::U8 => b[0] as f64,
        T::S8 => (b[0] as i8) as f64,
        T::U16 => {
            let a = [b[0], b[1]];
            (if le {
                u16::from_le_bytes(a)
            } else {
                u16::from_be_bytes(a)
            }) as f64
        }
        T::S16 => {
            let a = [b[0], b[1]];
            (if le {
                i16::from_le_bytes(a)
            } else {
                i16::from_be_bytes(a)
            }) as f64
        }
        T::U32 => {
            let a = [b[0], b[1], b[2], b[3]];
            (if le {
                u32::from_le_bytes(a)
            } else {
                u32::from_be_bytes(a)
            }) as f64
        }
        T::S32 => {
            let a = [b[0], b[1], b[2], b[3]];
            (if le {
                i32::from_le_bytes(a)
            } else {
                i32::from_be_bytes(a)
            }) as f64
        }
        T::F32 => {
            let a = [b[0], b[1], b[2], b[3]];
            (if le {
                f32::from_le_bytes(a)
            } else {
                f32::from_be_bytes(a)
            }) as f64
        }
    };
    Some(v)
}

/// Decode every cell of a map (physical values) from an image, or `None` if the
/// map runs off the end of the image.
fn decode_map_phys(m: &opentune_core::ecudef::MapDef, img: &[u8]) -> Option<Vec<f64>> {
    let n = (m.rows as usize) * (m.cols as usize);
    let w = m.data_type.byte_len() as usize;
    let base = m.address as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let raw = read_cell(img, base + i * w, m.data_type, m.endian)?;
        out.push(m.scaling.to_physical(raw));
    }
    Some(out)
}

/// Compare two images map-by-map through a definition's MapDefs.
fn compare_maps(d: &opentune_core::ecudef::EcuDef, a: &[u8], b: &[u8], only: Option<&str>) {
    println!(
        "Comparing {} map(s) of '{}' ({} → {})",
        d.maps.len(),
        d.id,
        human_len(a.len()),
        human_len(b.len())
    );
    let mut any_changed = false;
    for m in &d.maps {
        if let Some(sub) = only {
            if !m.name.to_lowercase().contains(&sub.to_lowercase()) {
                continue;
            }
        }
        let (Some(pa), Some(pb)) = (decode_map_phys(m, a), decode_map_phys(m, b)) else {
            println!("  {:<32} [out of range — skipped]", m.name);
            continue;
        };
        let mut changed = 0usize;
        let mut max_abs = 0f64;
        let mut sum_delta = 0f64;
        for (x, y) in pa.iter().zip(pb.iter()) {
            let dlt = y - x;
            if dlt.abs() > 1e-9 {
                changed += 1;
                sum_delta += dlt;
                if dlt.abs() > max_abs {
                    max_abs = dlt.abs();
                }
            }
        }
        let unit = if m.unit.is_empty() {
            String::new()
        } else {
            format!(" {}", m.unit)
        };
        if changed == 0 {
            if only.is_some() {
                println!("  {:<32} unchanged", m.name);
            }
            continue;
        }
        any_changed = true;
        let avg = sum_delta / changed as f64;
        println!(
            "  {:<32} {}x{}  {} cell(s) changed  Δmax {:+.3}{u}  Δavg {:+.3}{u}",
            m.name,
            m.rows,
            m.cols,
            changed,
            max_abs,
            avg,
            u = unit
        );
        // Full grid for a single explicitly-requested map.
        if only.is_some() {
            print_grid(m, &pa, &pb);
        }
    }
    if !any_changed && only.is_none() {
        println!("  No mapped cells differ.");
    }
}

/// Print a before→after grid for one map.
fn print_grid(m: &opentune_core::ecudef::MapDef, pa: &[f64], pb: &[f64]) {
    let cols = m.cols as usize;
    for r in 0..m.rows as usize {
        let mut line = String::from("      ");
        for c in 0..cols {
            let i = r * cols + c;
            let (x, y) = (pa[i], pb[i]);
            if (y - x).abs() > 1e-9 {
                line.push_str(&format!("{:>7.1}>{:<7.1}", x, y));
            } else {
                line.push_str(&format!("{:>7.1}{:<8}", x, ""));
            }
        }
        println!("{}", line.trim_end());
    }
}

fn human_len(n: usize) -> String {
    if n >= 1024 * 1024 {
        format!("{} MiB", n / (1024 * 1024))
    } else {
        format!("{} KiB", n / 1024)
    }
}
