//! Commands that talk to an ECU through a registry driver (`--driver`),
//! plus the backup-vault listing their reads and flashes archive into.
use crate::cli::ConnectArgs;
use crate::context::CliContext;
use crate::util::{noprogress, parse_num};
use anyhow::{anyhow, Context, Result};
use opentune_core::MemoryRegion;
use std::path::PathBuf;

/// `drivers`
pub fn drivers(ctx: &CliContext) -> Result<()> {
    println!("Available drivers:");
    for e in ctx.registry().list() {
        println!("  {:<12} {}", e.id, e.display_name);
    }
    Ok(())
}

/// `info`
pub fn info(ctx: &CliContext, conn: ConnectArgs) -> Result<()> {
    let mut drv = ctx.open_connect(&conn)?;
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
    Ok(())
}

/// `read`
pub fn read(
    ctx: &CliContext,
    conn: ConnectArgs,
    addr: String,
    len: String,
    out: PathBuf,
) -> Result<()> {
    let mut drv = ctx.open_connect(&conn)?;
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
    Ok(())
}

/// `flash`
#[allow(clippy::too_many_arguments)]
pub fn flash(
    ctx: &CliContext,
    conn: ConnectArgs,
    addr: String,
    data: PathBuf,
    min_mv: u32,
    vault: PathBuf,
    resume: Option<PathBuf>,
    transcript: Option<PathBuf>,
    chunk_size: usize,
) -> Result<()> {
    let mut drv = ctx.open_connect(&conn)?;
    let bytes = std::fs::read(&data).with_context(|| format!("read {}", data.display()))?;
    let region = MemoryRegion::new(parse_num(&addr)?, bytes.len() as u32);
    // Capture ECU identity up front so the archived backup is labelled.
    let id = drv.read_ecu_id().unwrap_or_default();
    let safety = opentune_flash::SafetyConfig {
        min_millivolts: min_mv,
        chunk_size,
        ..Default::default()
    };
    let report = if let Some(state) = resume.as_ref() {
        // Resumable, chunked, checkpointed flash with a session transcript.
        let mut tr = opentune_flash::Transcript::new();
        let rep = opentune_flash::flash_calibration_resumable_logged(
            drv.as_mut(),
            region,
            &bytes,
            &safety,
            state,
            &mut tr,
            &mut noprogress,
        )
        .map_err(|e| anyhow!("flash: {e}"))?;
        let backup_path = if !rep.backup.is_empty() {
            opentune_flash::vault::save_backup(&vault, &id, region, &rep.backup).ok()
        } else {
            None
        };
        let written = match (&transcript, &backup_path) {
            (Some(dir), _) => tr.write(dir, "flash").ok(),
            (None, Some(bp)) => tr.write_next_to(bp).ok(),
            (None, None) => tr.write(&vault, "flash").ok(),
        };
        if let Some(bp) = &backup_path {
            println!("Pre-flash backup archived: {}", bp.display());
        }
        if let Some((log, jsonl)) = written {
            println!(
                "Session transcript: {} / {}",
                log.display(),
                jsonl.display()
            );
        }
        rep
    } else {
        let rep = opentune_flash::flash_calibration(
            drv.as_mut(),
            region,
            &bytes,
            &safety,
            &mut noprogress,
        )
        .map_err(|e| anyhow!("flash: {e}"))?;
        // Persist the pre-flash backup — the safety net if the write is bad.
        if !rep.backup.is_empty() {
            match opentune_flash::vault::save_backup(&vault, &id, region, &rep.backup) {
                Ok(p) => println!("Pre-flash backup archived: {}", p.display()),
                Err(e) => eprintln!(
                    "WARNING: could not archive backup to {}: {e}",
                    vault.display()
                ),
            }
        }
        rep
    };
    println!(
        "Flashed {} bytes @0x{:08X} — backup {} bytes, verified={}",
        report.bytes_written,
        region.address,
        report.backup.len(),
        report.verified
    );
    drv.close()?;
    Ok(())
}

/// `dump`
pub fn dump(
    ctx: &CliContext,
    conn: ConnectArgs,
    addr: String,
    len: String,
    vault: PathBuf,
) -> Result<()> {
    let mut drv = ctx.open_connect(&conn)?;
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
    Ok(())
}

/// `identify`
pub fn identify(
    ctx: &CliContext,
    conn: ConnectArgs,
    ecudefs: PathBuf,
    catalog: Option<PathBuf>,
) -> Result<()> {
    let mut drv = ctx.open_connect(&conn)?;
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
    // The mock driver serves a file: recognise the image itself too.
    if let (true, Some(file)) = (conn.driver == "mock", &conn.file) {
        if let Ok(bytes) = std::fs::read(file) {
            let ctx = opentune_scan::ScanContext {
                catalog: Vec::new(),
                ecudefs: defs.clone(),
            };
            let root = file.parent().unwrap_or_else(|| std::path::Path::new("."));
            let rec = opentune_scan::scan_file(&ctx, root, file, &bytes);
            println!("\nOffline image recognition:");
            for l in opentune_scan::describe(&rec) {
                println!("  {l}");
            }
        }
    }
    drv.close()?;
    Ok(())
}

/// `vault`
pub fn vault(dir: PathBuf, verify: bool) -> Result<()> {
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
    Ok(())
}
