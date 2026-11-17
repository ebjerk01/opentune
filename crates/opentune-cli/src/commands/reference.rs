//! Lookups and offline recognition: DTC, VIN, part numbers, CBF, scan.
use anyhow::{anyhow, Result};
use std::path::PathBuf;

/// `dtc`
pub fn dtc(code: String) -> Result<()> {
    match opentune_dtc::lookup(&code) {
        Some(d) => println!("{}  [{:?}]  {}", d.code, d.system, d.description),
        None => println!("{code}: malformed code"),
    }
    Ok(())
}

/// `vin`
pub fn vin(vin: String) -> Result<()> {
    match opentune_vin::decode_vin(&vin) {
        Ok(v) => {
            println!("VIN {}", v.vin);
            println!(
                "  WMI        : {} ({}, {})",
                v.wmi,
                v.wmi_info.manufacturer.unwrap_or("unknown make"),
                v.wmi_info.country.unwrap_or("unknown country")
            );
            match v.model_year.resolved {
                Some(y) => println!("  Model year : {} -> {}", v.model_year.code, y),
                None => println!(
                    "  Model year : {} -> {:?} (ambiguous)",
                    v.model_year.code, v.model_year.candidates
                ),
            }
            println!(
                "  Check digit: {} ({})",
                v.check_digit,
                match v.check_digit_valid {
                    Some(true) => "valid",
                    Some(false) => "INVALID",
                    None => "not verifiable (EU filler)",
                }
            );
            println!("  Plant      : {}   Serial: {}", v.plant_code, v.serial);
            if !v.flags.is_empty() {
                println!("  Notes      : {}", v.flags.join("; "));
            }
        }
        Err(e) => println!("invalid VIN: {e:?}"),
    }
    Ok(())
}

/// `decode-part`
pub fn decode_part(part: String) -> Result<()> {
    let p = opentune_vin::parse_part_number(&part);
    println!("Part {}  [{:?}]", p.normalized, p.kind);
    if let Some(s) = p.supplier {
        println!("  Supplier: {s}");
    }
    if let Some(g) = &p.vag_type_group {
        println!(
            "  VAG: type={} main={} sub={} ver={}",
            g,
            p.vag_main_group.as_deref().unwrap_or("-"),
            p.vag_subgroup.as_deref().unwrap_or("-"),
            p.vag_version.as_deref().unwrap_or("-")
        );
    }
    if let Some(f) = &p.bosch_family {
        println!(
            "  Bosch: family={} consecutive={}",
            f,
            p.bosch_consecutive.as_deref().unwrap_or("-")
        );
    }
    if !p.flags.is_empty() {
        println!("  Notes: {}", p.flags.join("; "));
    }
    Ok(())
}

/// `cbf`
pub fn cbf(file: PathBuf, json: bool, dtcs: bool) -> Result<()> {
    let cbf =
        opentune_cbf::parse_file(&file).map_err(|e| anyhow!("parse {}: {e}", file.display()))?;
    if json {
        println!(
            "{}",
            opentune_cbf::to_json(&cbf).map_err(|e| anyhow!("{e}"))?
        );
    } else if dtcs {
        for entry in opentune_cbf::dtc_entries(&cbf) {
            println!("{}\t{}", entry.code, entry.description);
        }
    } else {
        println!("CBF version: {}", cbf.cbf_version);
        if !cbf.source_xml.is_empty() {
            println!("source: {}", cbf.source_xml);
        }
        for ecu in &cbf.ecus {
            println!(
                "ECU {} ({}): {} DTCs, {} services, {} variants",
                ecu.qualifier,
                ecu.name.as_deref().unwrap_or("-"),
                ecu.dtcs.len(),
                ecu.services.len(),
                ecu.variants.len(),
            );
        }
    }
    Ok(())
}

/// `scan`
pub fn scan(dir: PathBuf, human: bool, catalog: PathBuf, ecudefs: PathBuf) -> Result<()> {
    let ctx = opentune_scan::ScanContext::load(&catalog, &ecudefs)?;
    if dir.is_file() {
        let bytes = std::fs::read(&dir).map_err(|e| anyhow!("read {}: {e}", dir.display()))?;
        let root = dir.parent().unwrap_or_else(|| std::path::Path::new("."));
        let rec = opentune_scan::scan_file(&ctx, root, &dir, &bytes);
        if human {
            println!("{}", dir.display());
            for l in opentune_scan::describe(&rec) {
                println!("{l}");
            }
        } else {
            println!("{}", serde_json::to_string_pretty(&rec)?);
        }
    } else {
        let report = opentune_scan::scan_dir(&ctx, &dir)?;
        if human {
            for rec in &report.records {
                println!("{}", rec.rel_path);
                for l in opentune_scan::describe(rec) {
                    println!("  {l}");
                }
            }
            println!("{}", serde_json::to_string_pretty(&report.summary)?);
        } else {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(())
}
