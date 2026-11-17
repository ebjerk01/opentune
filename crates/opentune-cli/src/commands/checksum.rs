//! `checksum`: verify, fix, list and attest.
use crate::cli::ChecksumAction;
use anyhow::{anyhow, Context, Result};

/// `checksum <action>`
pub fn run(action: ChecksumAction) -> Result<()> {
    match action {
        ChecksumAction::Verify { algo, file } => {
            use opentune_checksum::{ChecksumKind, Recognition, Representation};
            let algo = opentune_checksum::get(&algo)
                .ok_or_else(|| anyhow!("unknown checksum algo '{algo}'"))?;
            let img = std::fs::read(&file)?;
            // A file handed to `checksum verify` is a full ECU image; declare
            // it so, and let the algorithm narrow it (or confirm recognition).
            let reports =
                opentune_checksum::report_detailed(&*algo, &img, Representation::RawFullImage)?;
            for r in &reports {
                let rep = match r.representation {
                    Representation::RawFullImage => "full image",
                    Representation::ProgrammingBlock => "extracted block",
                    Representation::ContainerSegment => "container segment",
                    Representation::Unknown => "unknown representation",
                };
                let kind = match r.checksum_kind {
                    ChecksumKind::Embedded => "embedded",
                    ChecksumKind::Transfer => "transfer",
                };
                // A merely-consistent match must never read as a recognised
                // ECU family: only a Recognised block is "supported".
                let recog = match r.recognition {
                    Recognition::Recognised => "recognised",
                    Recognition::Consistent => "consistent-only",
                };
                println!(
                    "{:<16} {} stored=0x{:08X} computed=0x{:08X}  [{} | {} | {}]",
                    r.status.name,
                    if r.status.ok { "OK  " } else { "BAD " },
                    r.status.stored,
                    r.status.computed,
                    rep,
                    kind,
                    recog,
                );
                if r.status.ok && !r.is_supported() {
                    println!(
                        "                 note: mathematical match only — NOT a recognised {} family (do not treat as supported)",
                        algo.id()
                    );
                }
            }
        }
        ChecksumAction::Fix {
            algo,
            file,
            reference,
            only_bad,
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
                None if only_bad => opentune_checksum::correct_bad_only(&*algo, &mut img)?,
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
        ChecksumAction::Algos => {
            for a in opentune_checksum::algorithms() {
                println!(
                    "{:<12} {:<12} {:<4} {}",
                    a.id,
                    a.family,
                    if a.needs_reference { "ref" } else { "-" },
                    a.name
                );
                if !a.notes.is_empty() {
                    println!("               {}", a.notes);
                }
            }
        }
        ChecksumAction::Attest { algo, file, out } => {
            let img = std::fs::read(&file).map_err(|e| anyhow!("read {}: {e}", file.display()))?;
            let att = opentune_attest::attest_image(&algo, &img).map_err(|e| anyhow!("{e}"))?;
            let json = serde_json::to_string_pretty(&att)?;
            if let Some(out) = out {
                std::fs::write(&out, &json).map_err(|e| anyhow!("write {}: {e}", out.display()))?;
                println!(
                    "Attestation ({}) written to {} — satisfied={}",
                    att.algorithm,
                    out.display(),
                    att.satisfied
                );
            } else {
                println!("{json}");
            }
            if !att.satisfied {
                println!("! {}", att.note);
            }
        }
    }
    Ok(())
}
