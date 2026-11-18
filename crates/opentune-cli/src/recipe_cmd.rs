//! `opentune recipe create|apply|show` — portable tune recipes
//! (`opentune-recipe`, format documented in docs/RECIPES.md).
use crate::context::CliContext;
use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use opentune_recipe::{
    apply_with, create_with, ApplyMode, ApplyOptions, CreateOptions, PatchStatus, Recipe,
};
use std::path::{Path, PathBuf};

/// `opentune recipe …` subcommands.
#[derive(Subcommand, Debug)]
pub enum RecipeAction {
    /// Capture a stock/tuned pair as an anchored-patch recipe.
    Create {
        /// Stock (original) image.
        stock: PathBuf,
        /// Tuned image (checksums corrected or not).
        tuned: PathBuf,
        /// ECU definition id: names patches after the maps they touch.
        #[arg(long)]
        def: Option<String>,
        /// Directory of ECU definitions (for `--def`).
        #[arg(long, default_value = "ecudefs")]
        ecudefs: PathBuf,
        /// Checksum algorithm id; default: from the definition, else from the
        /// structurally detected family (EDC15/16/17, PCR2.1).
        #[arg(long)]
        checksum: Option<String>,
        /// Unchanged context bytes stored on each side of a patch.
        #[arg(long, default_value_t = 8)]
        context: usize,
        /// Merge changes separated by fewer unchanged bytes than this.
        #[arg(long, default_value_t = 16)]
        gap: usize,
        /// Free-text note stored in the recipe.
        #[arg(long, default_value = "")]
        note: String,
        /// Output recipe file (e.g. tune.otrecipe).
        #[arg(short, long)]
        out: PathBuf,
    },
    /// Apply a recipe to a matching image, fix checksums and attest the result.
    /// Refuses (writes nothing) unless the identity and every anchor match.
    Apply {
        /// Recipe file.
        recipe: PathBuf,
        /// Target image (e.g. your own stock read).
        target: PathBuf,
        /// Output image; written only on success.
        #[arg(short, long)]
        out: PathBuf,
        /// Relocation mode: if a patch's bytes moved, find its context+original
        /// sequence elsewhere (must occur exactly once). Requires the same size,
        /// family and hardware part number; see docs/RECIPES.md.
        #[arg(long)]
        relocate: bool,
        /// Override the recipe's checksum algorithm id.
        #[arg(long)]
        checksum: Option<String>,
        /// Where to write the attestation JSON (default: <out>.attestation.json).
        #[arg(long)]
        attestation: Option<PathBuf>,
        /// Also write the full per-patch report as JSON.
        #[arg(long)]
        report: Option<PathBuf>,
        /// Write the output even when some checksum blocks CANNOT be verified
        /// (e.g. an EDC17 nested sub-block the tune changed). The attestation
        /// stays unsatisfied, so the flash policy refuses the image; for
        /// inspection/bench use only. A failing checksum still refuses.
        #[arg(long)]
        allow_unverified: bool,
    },
    /// Print a recipe's identity and patches.
    Show {
        /// Recipe file.
        recipe: PathBuf,
    },
}

fn load(path: &Path) -> Result<Recipe> {
    let s = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Recipe::from_json(&s).map_err(|e| anyhow!("{}: {e}", path.display()))
}

fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("read {}", path.display()))
}

fn status_text(s: &PatchStatus) -> String {
    match s {
        PatchStatus::Anchored { at } => format!("ok        anchored @0x{at:08X}"),
        PatchStatus::Relocated { from, to } => format!(
            "ok        RELOCATED 0x{from:08X} -> 0x{to:08X} ({:+} bytes)",
            *to as i64 - *from as i64
        ),
        PatchStatus::AlreadyApplied { at } => format!("present   already applied @0x{at:08X}"),
        PatchStatus::Mismatch { detail } => format!("REFUSED   original bytes differ: {detail}"),
        PatchStatus::NotFound => "REFUSED   context+original not found anywhere".into(),
        PatchStatus::Ambiguous { matches } => {
            format!("REFUSED   ambiguous: context+original occurs {matches}+ times")
        }
        PatchStatus::Overlap => "REFUSED   overlaps another patch after relocation".into(),
    }
}

fn show(r: &Recipe) {
    let s = &r.source;
    println!("Recipe {} v{} ({})", r.format, r.version, r.tool);
    if !r.note.is_empty() {
        println!("  note:          {}", r.note);
    }
    println!("  source size:   {} bytes", s.size);
    println!("  family:        {}", s.family);
    if let Some(d) = &s.def_id {
        println!("  definition:    {d}");
    }
    println!("  part numbers:  {}", s.part_numbers.join(", "));
    println!("  sw numbers:    {}", s.sw_numbers.join(", "));
    println!("  stock sha256:  {}", s.sha256);
    println!("  unchanged sha: {}", s.unchanged_sha256);
    println!("  tuned sha256:  {}", r.result_sha256);
    println!(
        "  checksum:      {}  ({} field range(s) recomputed, not patched)",
        r.checksum
            .as_deref()
            .unwrap_or("NONE (apply needs --checksum)"),
        r.checksum_fields.len()
    );
    println!(
        "  {} patch(es), {} byte(s):",
        r.patches.len(),
        r.patched_bytes()
    );
    for (i, p) in r.patches.iter().enumerate() {
        println!(
            "    #{i:<3} 0x{:08X} {:>5} B{}",
            p.offset,
            p.len(),
            if p.maps.is_empty() {
                String::new()
            } else {
                format!("  [{}]", p.maps.join("; "))
            }
        );
    }
}

/// Run a `recipe` subcommand.
pub fn run(ctx: &CliContext, action: RecipeAction) -> Result<()> {
    // Checksum ids resolve like `checksum --algo`: embedder first, then public.
    let lookup = |id: &str| ctx.checksum(id);
    match action {
        RecipeAction::Create {
            stock,
            tuned,
            def,
            ecudefs,
            checksum,
            context,
            gap,
            note,
            out,
        } => {
            let (a, b) = (read(&stock)?, read(&tuned)?);
            let mut opts = CreateOptions {
                context,
                gap,
                checksum,
                note,
                tool: format!("opentune {}", env!("CARGO_PKG_VERSION")),
                ..CreateOptions::default()
            };
            if let Some(id) = &def {
                let defs = opentune_core::ecudef::load_dir(&ecudefs)
                    .map_err(|e| anyhow!("load {}: {e}", ecudefs.display()))?;
                let d = defs
                    .into_iter()
                    .find(|d| &d.id == id)
                    .ok_or_else(|| anyhow!("no definition '{id}' in {}", ecudefs.display()))?;
                if opts.checksum.is_none() {
                    opts.checksum = d.checksum.clone();
                }
                opts.maps = d.maps;
                opts.def_id = Some(d.id);
            }
            let (recipe, warnings) =
                create_with(&a, &b, &opts, &lookup).map_err(|e| anyhow!("{e}"))?;
            for w in &warnings {
                eprintln!("warning: {w}");
            }
            std::fs::write(&out, recipe.to_json())
                .with_context(|| format!("write {}", out.display()))?;
            show(&recipe);
            println!("wrote recipe -> {}", out.display());
        }
        RecipeAction::Apply {
            recipe,
            target,
            out,
            relocate,
            checksum,
            attestation,
            report,
            allow_unverified,
        } => {
            let r = load(&recipe)?;
            let t = read(&target)?;
            let opts = ApplyOptions {
                mode: if relocate {
                    ApplyMode::Relocate
                } else {
                    ApplyMode::Strict
                },
                checksum,
                allow_unverified,
            };
            let rep = apply_with(&r, &t, &opts, &lookup);
            println!(
                "Applying {} ({} patch(es)) to {} in {:?} mode",
                recipe.display(),
                r.patches.len(),
                target.display(),
                rep.mode
            );
            println!("Identity:");
            for c in &rep.identity {
                println!(
                    "  {:<4} {:<26} recipe: {}  target: {}{}",
                    if c.ok { "ok" } else { "DIFF" },
                    c.item,
                    c.expected,
                    c.found,
                    if !c.ok && !c.required {
                        "  (informational in this mode)"
                    } else {
                        ""
                    }
                );
            }
            if !rep.patches.is_empty() {
                println!("Patches:");
                for p in &rep.patches {
                    println!(
                        "  #{:<3} 0x{:08X} {:>5} B  {}{}",
                        p.index,
                        p.offset,
                        p.len,
                        status_text(&p.status),
                        if p.maps.is_empty() {
                            String::new()
                        } else {
                            format!("  [{}]", p.maps.join("; "))
                        }
                    );
                }
            }
            for f in &rep.checksum_fixes {
                println!("Checksum fix: {f}");
            }
            if let Some(path) = &report {
                std::fs::write(path, serde_json::to_string_pretty(&rep)?)
                    .with_context(|| format!("write {}", path.display()))?;
            }
            if let Some(why) = &rep.refusal {
                bail!("REFUSED: {why} (no output written)");
            }
            let (Some(img), Some(att)) = (&rep.output, &rep.attestation) else {
                bail!("internal error: success without output/attestation");
            };
            std::fs::write(&out, img).with_context(|| format!("write {}", out.display()))?;
            let att_path = attestation.unwrap_or_else(|| {
                let mut p = out.clone().into_os_string();
                p.push(".attestation.json");
                PathBuf::from(p)
            });
            std::fs::write(&att_path, serde_json::to_string_pretty(att)?)
                .with_context(|| format!("write {}", att_path.display()))?;
            println!(
                "Checksums ({}) corrected and attested: satisfied={} — {}",
                att.algorithm, att.satisfied, att.note
            );
            if !att.satisfied {
                eprintln!(
                    "WARNING: --allow-unverified: {} block(s) could not be verified ({}); the \
                     attestation is NOT satisfied and the flash policy will refuse this image.",
                    rep.unverified_blocks.len(),
                    rep.unverified_blocks.join(", ")
                );
            }
            if rep.identical_to_recipe_result {
                println!("Output is byte-identical to the recipe's tuned image.");
            }
            println!(
                "wrote {} -> {} (attestation {})",
                img.len(),
                out.display(),
                att_path.display()
            );
        }
        RecipeAction::Show { recipe } => show(&load(&recipe)?),
    }
    Ok(())
}
