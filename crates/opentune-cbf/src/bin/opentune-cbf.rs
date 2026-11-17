//! CLI front-end for `opentune-cbf`.
//!
//! Usage:
//!   opentune-cbf <FILE.cbf>              Summarise ECUs and DTC counts
//!   opentune-cbf <FILE.cbf> --json       Emit the full parsed model as JSON
//!   opentune-cbf <FILE.cbf> --dtcs       Emit one `code\tdescription` per line
//!   opentune-cbf <FILE.cbf> --diag       Classified diagnostic view + warnings
//!   opentune-cbf <FILE.cbf> --diag-json  The diagnostic view as JSON

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args.len() > 3 {
        eprintln!(
            "usage: {} <FILE.cbf> [--json|--dtcs|--diag|--diag-json]",
            args[0]
        );
        return ExitCode::from(2);
    }
    let path = &args[1];
    let mode = args.get(2).map(String::as_str).unwrap_or("--summary");

    let cbf = match opentune_cbf::parse_file(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error parsing {path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    match mode {
        "--json" => match opentune_cbf::to_json(&cbf) {
            Ok(j) => println!("{j}"),
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        },
        "--dtcs" => {
            for entry in opentune_cbf::dtc_entries(&cbf) {
                println!("{}\t{}", entry.code, entry.description);
            }
        }
        "--diag-json" => {
            let diags = opentune_cbf::diagnostics(&cbf);
            match serde_json::to_string_pretty(&diags) {
                Ok(j) => println!("{j}"),
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            }
        }
        "--diag" => {
            for diag in opentune_cbf::diagnostics(&cbf) {
                println!(
                    "ECU {} ({}) protocols={:?}",
                    diag.ecu.qualifier,
                    diag.ecu.name.as_deref().unwrap_or("-"),
                    diag.ecu.protocols,
                );
                println!("  {} DTCs, {} services", diag.dtcs.len(), diag.services.len());
                // Count services by classified kind.
                let mut kinds: std::collections::BTreeMap<String, usize> =
                    std::collections::BTreeMap::new();
                for svc in &diag.services {
                    *kinds.entry(format!("{:?}", svc.kind)).or_default() += 1;
                }
                for (kind, n) in &kinds {
                    println!("    {kind}: {n}");
                }
                for warning in &diag.warnings {
                    println!("  note: {warning}");
                }
            }
        }
        _ => {
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
    }
    ExitCode::SUCCESS
}
