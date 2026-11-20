//! `opentune findmaps <image>` — the single-file map finder
//! (`opentune-mapfind`). The two-file diff mode is in `commands/image.rs`
//! (clustering from `opentune_mapfind::diff`).
use anyhow::{anyhow, Context, Result};
use clap::Args;
use opentune_mapfind::{find_maps, match_defs, FindOptions, MapCandidate, MatchKind};
use std::path::{Path, PathBuf};

/// Options that only apply when `findmaps` is given a single image.
#[derive(Args, Debug)]
pub struct SingleArgs {
    /// Single-file mode: how many top candidates to print / emit (0 = all).
    #[arg(long, default_value_t = 40)]
    pub top: usize,
    /// Single-file mode: drop candidates scoring below this (0..1).
    #[arg(long, default_value_t = 0.5)]
    pub min_score: f64,
    /// Single-file mode: mark candidates that match this definition's maps and
    /// report definition recall (e.g. `bosch-edc16u34`).
    #[arg(long)]
    pub def: Option<String>,
    /// Directory of ECU definitions (for `--def`).
    #[arg(long, default_value = "ecudefs")]
    pub ecudefs: PathBuf,
    /// Single-file mode: print the evidence and hints for each candidate.
    #[arg(long)]
    pub evidence: bool,
    /// Single-file mode: skip the 8-bit counted layouts.
    #[arg(long)]
    pub no_byte_layouts: bool,
    /// Single-file mode: skip the header-less axis-pair layout.
    #[arg(long)]
    pub no_headerless: bool,
}

fn axis_summary(c: &MapCandidate) -> String {
    let one = |label: &str, a: &opentune_mapfind::AxisCandidate| {
        format!(
            "{label} {}x{}..{}{}",
            a.count,
            a.values.first().copied().unwrap_or(0),
            a.values.last().copied().unwrap_or(0),
            if a.kind == opentune_mapfind::AxisKind::Unknown {
                String::new()
            } else {
                format!(" {}?", a.kind.as_str())
            }
        )
    };
    match &c.y_axis {
        Some(y) => format!("{} | {}", one("y", y), one("x", &c.x_axis)),
        None => one("x", &c.x_axis),
    }
}

/// Run the single-image finder and print / emit the ranked candidates.
pub fn run_single(image: &Path, args: &SingleArgs, emit: Option<&Path>) -> Result<()> {
    let img = std::fs::read(image).with_context(|| format!("read {}", image.display()))?;
    let opts = FindOptions {
        min_score: args.min_score,
        byte_layouts: !args.no_byte_layouts,
        headerless: !args.no_headerless,
        ..FindOptions::default()
    };
    let cands = find_maps(&img, &opts);
    let shown = if args.top == 0 {
        cands.len()
    } else {
        args.top.min(cands.len())
    };
    let defs = match &args.def {
        Some(id) => {
            let all = opentune_core::ecudef::load_dir(&args.ecudefs)
                .map_err(|e| anyhow!("load {}: {e}", args.ecudefs.display()))?;
            let d = all
                .into_iter()
                .find(|d| &d.id == id)
                .ok_or_else(|| anyhow!("no definition '{id}' in {}", args.ecudefs.display()))?;
            d.maps
        }
        None => Vec::new(),
    };
    let matches = match_defs(&cands, &defs);
    println!(
        "{} candidate map(s) in {} (min score {:.2}); showing {shown}. \
         CANDIDATES ONLY — shapes and hints are unverified.",
        cands.len(),
        image.display(),
        args.min_score
    );
    println!(
        "  {:>4} {:>5}  {:<10} {:>6}  {:<7} {:<13} axes",
        "rank", "score", "address", "shape", "cells", "layout"
    );
    for (i, c) in cands.iter().take(shown).enumerate() {
        let mark: Vec<String> = matches
            .iter()
            .filter(|m| m.rank == Some(i))
            .map(|m| format!("  <= def '{}' ({:?})", m.name, m.kind))
            .collect();
        println!(
            "  {:>4} {:>5.2}  0x{:08X} {:>6}  {:<7} {:<13} {}{}",
            i + 1,
            c.score,
            c.address,
            format!("{}x{}", c.rows, c.cols),
            c.cell_desc(),
            c.layout.as_str(),
            axis_summary(c),
            mark.join("")
        );
        if args.evidence {
            for e in &c.evidence {
                println!("           evidence: {e}");
            }
            for h in &c.hints {
                println!("           hint (unverified): {h}");
            }
        }
    }
    if !defs.is_empty() {
        let found = matches
            .iter()
            .filter(|m| m.kind != MatchKind::Missed)
            .count();
        println!(
            "Definition recall vs '{}': {found}/{} map(s) located",
            args.def.as_deref().unwrap_or(""),
            matches.len()
        );
        for m in &matches {
            println!(
                "  {:<8} {:>8}  0x{:08X}  {}",
                format!("{:?}", m.kind).to_lowercase(),
                m.rank.map_or("-".into(), |r| format!("rank {}", r + 1)),
                m.address,
                m.name
            );
        }
    }
    if let Some(path) = emit {
        let maps: Vec<_> = cands.iter().take(shown).map(|c| c.to_mapdef()).collect();
        std::fs::write(path, serde_json::to_string_pretty(&maps)?)
            .with_context(|| format!("write {}", path.display()))?;
        println!(
            "wrote {} candidate MapDef(s) -> {}",
            maps.len(),
            path.display()
        );
    }
    Ok(())
}
