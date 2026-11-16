//! Standalone runner for the prototype scanner (the `scan <dir>` command the
//! caller will later fold into `opentune-cli`). Writes one JSON record per file
//! to `--jsonl` and prints the summary to stdout.

use std::path::PathBuf;

use clap::Parser;

use opentune_scan::{scan_dir, ScanContext};

#[derive(Parser)]
#[command(name = "opentune-scan", about = "Offline ECU-file scanner (prototype)")]
struct Args {
    /// Directory of ECU dumps to scan (recursive).
    dir: PathBuf,
    /// Catalog directory (opendbc + manual).
    #[arg(long, default_value = "catalog")]
    catalog: PathBuf,
    /// ECU-definition directory.
    #[arg(long, default_value = "ecudefs")]
    ecudefs: PathBuf,
    /// Write one JSON record per file here (JSONL).
    #[arg(long)]
    jsonl: Option<PathBuf>,
    /// Write the full report (records + summary) as pretty JSON here.
    #[arg(long)]
    report: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let ctx = ScanContext::load(&args.catalog, &args.ecudefs)?;
    eprintln!(
        "loaded {} catalog entries, {} ecudefs",
        ctx.catalog.len(),
        ctx.ecudefs.len()
    );
    let report = scan_dir(&ctx, &args.dir)?;

    if let Some(p) = &args.jsonl {
        use std::io::Write;
        let mut f = std::io::BufWriter::new(std::fs::File::create(p)?);
        for r in &report.records {
            writeln!(f, "{}", serde_json::to_string(r)?)?;
        }
    }
    if let Some(p) = &args.report {
        std::fs::write(p, serde_json::to_string_pretty(&report)?)?;
    }

    println!("{}", serde_json::to_string_pretty(&report.summary)?);
    eprintln!(
        "scanned {} files; skipped {} archives, {} oversize; {} errors",
        report.records.len(),
        report.skipped_archives,
        report.skipped_oversize,
        report.errors
    );
    Ok(())
}
