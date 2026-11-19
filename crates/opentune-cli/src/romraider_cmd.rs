//! `opentune import-romraider-logger` / `export-romraider-logger`: RomRaider
//! logger definitions (`logger.xml`) to OpenTune logger definitions (JSON, the
//! format of a pack's `loggers/*.json` and what `log --def` reads) and back.
//! See docs/LIVE-LOGGING.md (RomRaider logger definitions).
use anyhow::{anyhow, Context, Result};
use opentune_logdef::{ParamStatus, RomRaiderImportOptions};
use std::path::PathBuf;

#[derive(clap::Args)]
pub struct ImportArgs {
    /// The RomRaider `logger.xml`.
    file: PathBuf,
    /// Protocol to import (`SSM`, `OBD`; default: the first in the file).
    #[arg(long)]
    protocol: Option<String>,
    /// ECU (ROM) id whose `<ecuparam>` addresses to include, e.g. the ROM ID
    /// an SSM module reports. Default: standard parameters only.
    #[arg(long, value_name = "ID")]
    ecu: Option<String>,
    /// Write the OpenTune logger definition (JSON) here.
    #[arg(short = 'o', long)]
    out: Option<PathBuf>,
    /// Write the per-parameter import report (JSON) here.
    #[arg(long, value_name = "FILE")]
    report: Option<PathBuf>,
    /// List every parameter, not only refused and calculated ones.
    #[arg(short, long)]
    verbose: bool,
}

#[derive(clap::Args)]
pub struct ExportArgs {
    /// The OpenTune logger definition (JSON) or logger XML to export.
    file: PathBuf,
    /// Write the RomRaider `logger.xml` here (otherwise to stdout).
    #[arg(short = 'o', long)]
    out: Option<PathBuf>,
}

pub fn import(args: ImportArgs) -> Result<()> {
    // Definition files are often Latin-1 / Windows-1252; decode lossily.
    let raw = std::fs::read(&args.file).with_context(|| format!("read {}", args.file.display()))?;
    let src = String::from_utf8_lossy(&raw);
    let opts = RomRaiderImportOptions {
        protocol: args.protocol,
        ecu: args.ecu,
    };
    let imp = opentune_logdef::import_romraider_logger(&src, &opts).map_err(|e| anyhow!(e))?;
    let proto = &imp.def.protocols[0];
    let ecu = imp
        .ecu
        .as_deref()
        .map(|e| format!(", ECU {e}"))
        .unwrap_or_default();
    let baud = proto
        .baud
        .map(|b| format!(" ({b} baud)"))
        .unwrap_or_default();
    let counts: Vec<String> = [
        ParamStatus::Linear,
        ParamStatus::Expression,
        ParamStatus::Calculated,
        ParamStatus::Switch,
        ParamStatus::Text,
        ParamStatus::Refused,
    ]
    .iter()
    .filter_map(|s| {
        let n = imp.count(*s);
        (n > 0).then(|| format!("{n} {}", s.as_str()))
    })
    .collect();
    println!(
        "RomRaider logger: protocol {}{baud}{ecu} — {} parameter(s): {}",
        imp.protocol,
        imp.params.len(),
        if counts.is_empty() {
            "none".into()
        } else {
            counts.join(", ")
        }
    );
    for p in &imp.params {
        let show = args.verbose
            || matches!(p.status, ParamStatus::Refused | ParamStatus::Calculated)
            || !p.detail.is_empty();
        if !show {
            continue;
        }
        let mark = if p.status == ParamStatus::Refused {
            "!"
        } else {
            " "
        };
        let detail = if p.detail.is_empty() {
            String::new()
        } else {
            format!(" — {}", p.detail)
        };
        println!(
            "  {mark} {:<10} {:<11} {}{detail}",
            p.id,
            p.status.as_str(),
            p.name
        );
    }
    for n in &imp.notes {
        println!("  note: {n}");
    }
    if let Some(out) = &args.out {
        let json = serde_json::to_string_pretty(&imp.def)?;
        std::fs::write(out, json + "\n").with_context(|| format!("write {}", out.display()))?;
        println!("wrote {}", out.display());
    }
    if let Some(path) = &args.report {
        let json = serde_json::to_string_pretty(&serde_json::json!({
            "protocol": imp.protocol,
            "ecu": imp.ecu,
            "params": imp.params,
            "notes": imp.notes,
        }))?;
        std::fs::write(path, json + "\n").with_context(|| format!("write {}", path.display()))?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

pub fn export(args: ExportArgs) -> Result<()> {
    let raw = std::fs::read(&args.file).with_context(|| format!("read {}", args.file.display()))?;
    let def = opentune_logdef::load(&String::from_utf8_lossy(&raw))
        .map_err(|e| anyhow!("{}: {e}", args.file.display()))?;
    let errs = def.validate();
    if !errs.is_empty() {
        return Err(anyhow!(
            "{} does not validate: {}",
            args.file.display(),
            errs.join("; ")
        ));
    }
    let (xml, notes) = opentune_logdef::export_romraider_logger(&def);
    for n in &notes {
        eprintln!("note: {n}");
    }
    match &args.out {
        Some(out) => {
            std::fs::write(out, xml).with_context(|| format!("write {}", out.display()))?;
            eprintln!("wrote {}", out.display());
        }
        None => print!("{xml}"),
    }
    Ok(())
}
