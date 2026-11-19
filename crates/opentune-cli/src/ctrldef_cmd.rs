//! `opentune controller`: drivetrain controller definitions (TCU, AWD) —
//! list, show, validate, match a part number, and decode a CAN log with a
//! definition's broadcast frame sets. All offline.
use anyhow::{anyhow, bail, Context, Result};
use opentune_ctrldef::decode::{fmt_num, summarize, RawFrame, SignalStats};
use opentune_ctrldef::{
    load_dir, ControllerDef, Decoder, LinkContext, LiveSource, Severity, Value,
};
use opentune_haldex::canlog::parse_log;
use std::path::{Path, PathBuf};

#[derive(clap::Subcommand)]
pub enum ControllerAction {
    /// List the controller definitions in a directory.
    List {
        /// Directory of controller definitions.
        #[arg(long, default_value = "ctrldefs")]
        dir: PathBuf,
        /// JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Show one definition: identity, links, diagnostics, CAN frame sets,
    /// live channels, tables, gear ratios, safety notes and sources.
    Show {
        /// Definition id, a part number / gearbox code it matches, or a path.
        def: String,
        /// Directory of controller definitions.
        #[arg(long, default_value = "ctrldefs")]
        dir: PathBuf,
        /// Also list every signal of every frame.
        #[arg(long)]
        signals: bool,
        /// JSON output (the definition itself).
        #[arg(long)]
        json: bool,
    },
    /// Validate definitions (structure, honesty and link rules; see
    /// docs/CONTROLLER-DEFS.md). Exits non-zero on any error.
    Validate {
        /// A definition file or a directory of them.
        #[arg(default_value = "ctrldefs")]
        path: PathBuf,
        /// ECU-definition directory the `ecudefs` links resolve against.
        #[arg(long, default_value = "ecudefs")]
        ecudefs: PathBuf,
        /// Database unit directory the `units` links resolve against.
        #[arg(long, default_value = "database/units")]
        units: PathBuf,
        /// Also print warnings.
        #[arg(long)]
        warnings: bool,
    },
    /// Which definitions match a part number, software number or gearbox code.
    Match {
        /// The value to match (e.g. 0AY907554C, 02E300042M, 722.6).
        value: String,
        /// Directory of controller definitions.
        #[arg(long, default_value = "ctrldefs")]
        dir: PathBuf,
    },
    /// Decode a CAN log (candump, OpenTune CSV, SavvyCAN CSV or Vector ASC) with a
    /// definition's frame sets: per-frame signal ranges, live channels,
    /// optionally every frame. Offline; nothing is transmitted.
    Decode {
        /// Definition id, a part number / gearbox code it matches, or a path.
        def: String,
        /// The log file.
        log: PathBuf,
        /// Frame set (e.g. egs51, pq, mqb). Needed when sets share ids.
        #[arg(long)]
        bus: Option<String>,
        /// Directory of controller definitions.
        #[arg(long, default_value = "ctrldefs")]
        dir: PathBuf,
        /// Also print every decoded frame.
        #[arg(long)]
        frames: bool,
        /// JSON output.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(action: ControllerAction) -> Result<()> {
    match action {
        ControllerAction::List { dir, json } => list(&dir, json),
        ControllerAction::Show {
            def,
            dir,
            signals,
            json,
        } => show(&resolve(&def, &dir)?, signals, json),
        ControllerAction::Validate {
            path,
            ecudefs,
            units,
            warnings,
        } => validate(&path, &ecudefs, &units, warnings),
        ControllerAction::Match { value, dir } => match_value(&value, &dir),
        ControllerAction::Decode {
            def,
            log,
            bus,
            dir,
            frames,
            json,
        } => decode(&resolve(&def, &dir)?, &log, bus.as_deref(), frames, json),
    }
}

fn load(dir: &Path) -> Result<Vec<ControllerDef>> {
    Ok(load_dir(dir)
        .map_err(|e| anyhow!("{e}"))?
        .into_iter()
        .map(|(_, d)| d)
        .collect())
}

/// A path, an id, or a value exactly one definition matches.
fn resolve(key: &str, dir: &Path) -> Result<ControllerDef> {
    let p = Path::new(key);
    if key.ends_with(".json") && p.is_file() {
        let text = std::fs::read_to_string(p).with_context(|| format!("read {key}"))?;
        return ControllerDef::from_json(&text).map_err(|e| anyhow!("{key}: {e}"));
    }
    let defs = load(dir)?;
    if let Some(d) = defs.iter().find(|d| d.id == key) {
        return Ok(d.clone());
    }
    let hits: Vec<&ControllerDef> = defs.iter().filter(|d| d.matches(key)).collect();
    match hits.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!(
            "no controller definition '{key}' in {} (ids: {})",
            dir.display(),
            defs.iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        many => bail!(
            "'{key}' matches several definitions: {}; give the id",
            many.iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn list(dir: &Path, json: bool) -> Result<()> {
    let defs = load(dir)?;
    if json {
        let v: Vec<_> = defs
            .iter()
            .map(|d| {
                serde_json::json!({
                    "id": d.id, "name": d.name, "kind": d.kind.as_str(), "family": d.family,
                    "validation": d.validation.as_str(),
                    "buses": d.buses.iter().map(|b| &b.name).collect::<Vec<_>>(),
                    "ecudefs": d.ecudefs,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!(
        "{:<26} {:<5} {:<20} {:<18} {:<14} ECUDEFS",
        "ID", "KIND", "FAMILY", "VALIDATION", "CAN SETS"
    );
    for d in &defs {
        let sets = d
            .buses
            .iter()
            .map(|b| b.name.as_str())
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "{:<26} {:<5} {:<20} {:<18} {:<14} {}",
            d.id,
            d.kind.as_str(),
            d.family,
            d.validation.as_str(),
            if sets.is_empty() { "-".into() } else { sets },
            if d.ecudefs.is_empty() {
                "-".into()
            } else {
                d.ecudefs.join(",")
            }
        );
    }
    println!("{} definition(s)", defs.len());
    Ok(())
}

fn hex_opt(v: Option<u32>) -> String {
    v.map(|v| format!("0x{v:X}")).unwrap_or_else(|| "-".into())
}

fn show(d: &ControllerDef, signals: bool, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(d)?);
        return Ok(());
    }
    println!("{} — {}", d.id, d.name);
    println!(
        "  kind {}  maker {}  family {}  validation {}",
        d.kind.as_str(),
        d.maker,
        d.family,
        d.validation.as_str()
    );
    if !d.vehicles.is_empty() {
        println!("  vehicles: {}", d.vehicles.join("; "));
    }
    let id = &d.identify;
    println!(
        "  identify: parts [{}] sw [{}] codes [{}]",
        id.part_numbers.join(", "),
        id.sw_numbers.join(", "),
        id.gearbox_codes.join(", ")
    );
    if !d.ecudefs.is_empty() || !d.units.is_empty() {
        println!(
            "  links: ecudefs [{}] units [{}]",
            d.ecudefs.join(", "),
            d.units.join(", ")
        );
    }
    println!(
        "  checksum: {}",
        d.checksum
            .as_deref()
            .unwrap_or("- (or in the linked ecudefs)")
    );
    if let Some(f) = &d.flash {
        println!("  flash: size {} bytes, read {:?}", f.size, f.full_read);
        for r in &f.regions {
            println!(
                "    {:<12} 0x{:X} +0x{:X}{}",
                r.name,
                r.region.address,
                r.region.length,
                if r.writable { " writable" } else { "" }
            );
        }
    }
    if !d.notes.is_empty() {
        println!("\n  {}", d.notes);
    }
    if !d.diagnostics.is_empty() {
        println!("\nDiagnostics:");
        for c in &d.diagnostics {
            let mut addr = format!("tx {} rx {}", hex_opt(c.tx_id), hex_opt(c.rx_id));
            if let Some(a) = c.tp20_address {
                addr = format!("TP2.0 address 0x{a:02X}, answer on {}", hex_opt(c.rx_id));
            }
            println!(
                "  {:<10} {:?}/{:?} {} bit/s, {addr} [{}]",
                c.name,
                c.transport,
                c.layer,
                c.bitrate,
                c.validation.as_str()
            );
        }
    }
    for b in &d.buses {
        println!(
            "\nCAN set '{}' — {} ({} bit/s, {} frames)",
            b.name,
            b.description,
            b.bitrate,
            b.frames.len()
        );
        for f in &b.frames {
            println!(
                "  0x{:03X} {:<18} from {:<14} {} signal(s){}",
                f.id,
                f.name,
                f.sender,
                f.signals.len(),
                f.validation
                    .map(|v| format!(" [{}]", v.as_str()))
                    .unwrap_or_default()
            );
            if signals {
                for s in &f.signals {
                    println!(
                        "      {:<34} {:?} start {:>2} len {:>2} {:<6} {}",
                        s.name, s.byte_order, s.start, s.len, s.unit, s.notes
                    );
                }
            }
        }
    }
    if !d.live.is_empty() {
        println!("\nLive channels:");
        for l in &d.live {
            let src = match &l.source {
                LiveSource::CanSignal { bus, frame, signal } => {
                    format!("CAN {bus}/{frame}.{signal}")
                }
                LiveSource::KwpMeasuringBlock { group, cell } => {
                    format!("KWP 0x21 group {group} cell {cell}")
                }
                LiveSource::UdsDid { did, byte, len, .. } => {
                    format!("UDS 0x22 0x{did:04X} byte {byte} len {len}")
                }
            };
            println!("  {:<28} {:<6} {src}", l.name, l.unit);
        }
    }
    if !d.adaptations.is_empty() {
        println!("\nAdaptations:");
        for a in &d.adaptations {
            println!(
                "  {:<28} {:?} {}{} [{}]",
                a.name,
                a.kind,
                if a.service.is_empty() {
                    "(service not public)"
                } else {
                    &a.service
                },
                if a.writes { " WRITES" } else { "" },
                a.validation.as_str()
            );
        }
    }
    if !d.tables.is_empty() {
        println!("\nTables:");
        for t in &d.tables {
            let axis = |a: &opentune_ctrldef::TableAxis| -> String {
                let pts = if !a.values.is_empty() {
                    a.values
                        .iter()
                        .map(|v| fmt_num(*v))
                        .collect::<Vec<_>>()
                        .join(" ")
                } else if !a.labels.is_empty() {
                    a.labels.join(" ")
                } else {
                    a.derived.clone()
                };
                format!("{} [{}] {}", a.name, a.unit, pts)
            };
            println!(
                "  {:<34} {:?} {}x{} {} ({:?}){}",
                t.name,
                t.role,
                t.rows,
                t.cols,
                t.unit,
                t.confidence,
                if t.profiles.is_empty() {
                    String::new()
                } else {
                    format!(" profiles {}", t.profiles.join(","))
                }
            );
            println!("      x: {}", axis(&t.x_axis));
            if let Some(y) = &t.y_axis {
                println!("      y: {}", axis(y));
            }
        }
    }
    if !d.gearsets.is_empty() {
        println!("\nGear ratios:");
        for g in &d.gearsets {
            let gears: Vec<String> = g
                .gears
                .iter()
                .map(|x| format!("{} {}", x.gear, fmt_num(x.ratio)))
                .collect();
            println!("  {:<36} {}", g.name, gears.join("  "));
        }
    }
    println!("\nSafety:");
    for s in &d.safety {
        println!("  - {s}");
    }
    println!("\nSources:");
    for s in &d.sources {
        println!("  - {} ({}): {}", s.title, s.license, s.used_for);
    }
    Ok(())
}

fn link_context(ecudefs: &Path, units: &Path) -> Result<LinkContext> {
    let ecudefs = if ecudefs.is_dir() {
        opentune_core::ecudef::load_dir(ecudefs)
            .map_err(|e| anyhow!("{}: {e}", ecudefs.display()))?
    } else {
        Vec::new()
    };
    let mut unit_ids = Vec::new();
    if units.is_dir() {
        for e in std::fs::read_dir(units)? {
            let p = e?.path();
            if p.extension().and_then(|s| s.to_str()) == Some("json") {
                let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p)?)
                    .with_context(|| format!("parse {}", p.display()))?;
                if let Some(id) = v["id"].as_str() {
                    unit_ids.push(id.to_string());
                }
            }
        }
    }
    Ok(LinkContext {
        ecudefs,
        unit_ids,
        checksum_ids: opentune_checksum::algorithms()
            .into_iter()
            .map(|a| a.id.to_string())
            .collect(),
    })
}

fn validate(path: &Path, ecudefs: &Path, units: &Path, warnings: bool) -> Result<()> {
    let files: Vec<(PathBuf, ControllerDef)> = if path.is_dir() {
        load_dir(path).map_err(|e| anyhow!("{e}"))?
    } else {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        vec![(
            path.to_path_buf(),
            ControllerDef::from_json(&text).map_err(|e| anyhow!("{}: {e}", path.display()))?,
        )]
    };
    let ctx = link_context(ecudefs, units)?;
    let (mut errors, mut warns) = (0usize, 0usize);
    for (p, d) in &files {
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let issues: Vec<_> = d
            .validate()
            .into_iter()
            .chain(d.validate_filename(stem))
            .chain(d.validate_links(&ctx))
            .collect();
        let e = issues
            .iter()
            .filter(|i| i.severity == Severity::Error)
            .count();
        let w = issues.len() - e;
        errors += e;
        warns += w;
        println!(
            "{:<34} {}",
            p.display(),
            if e > 0 {
                format!("{e} error(s), {w} warning(s)")
            } else if w > 0 {
                format!("ok ({w} warning(s))")
            } else {
                "ok".into()
            }
        );
        for i in &issues {
            if i.severity == Severity::Error || warnings {
                println!("    {i}");
            }
        }
    }
    println!(
        "{} definition(s): {errors} error(s), {warns} warning(s)",
        files.len()
    );
    if errors > 0 {
        bail!("{errors} validation error(s)");
    }
    Ok(())
}

fn match_value(value: &str, dir: &Path) -> Result<()> {
    let defs = load(dir)?;
    let hits: Vec<&ControllerDef> = defs.iter().filter(|d| d.matches(value)).collect();
    if hits.is_empty() {
        println!("no controller definition matches '{value}'");
    }
    for d in hits {
        println!("{:<26} {}", d.id, d.name);
    }
    Ok(())
}

fn stats_text(s: &SignalStats, unit: &str) -> String {
    let mut out = String::new();
    if let (Some(lo), Some(hi)) = (s.min, s.max) {
        out = format!("{} .. {} {unit}", fmt_num(lo), fmt_num(hi));
    }
    if !s.counts.is_empty() {
        let mut c: Vec<(&String, &u64)> = s.counts.iter().collect();
        c.sort_by(|a, b| b.1.cmp(a.1));
        let parts: Vec<String> = c.iter().take(6).map(|(k, n)| format!("{k} x{n}")).collect();
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str(&parts.join(", "));
        if c.len() > 6 {
            out.push_str(&format!(", +{} more", c.len() - 6));
        }
    }
    if let Some(last) = &s.last {
        out.push_str(&format!("  last {last}"));
    }
    if s.invalid > 0 {
        out.push_str(&format!("  ({} n/a)", s.invalid));
    }
    out
}

fn decode(
    d: &ControllerDef,
    log: &Path,
    bus: Option<&str>,
    frames: bool,
    json: bool,
) -> Result<()> {
    if d.buses.is_empty() {
        bail!("{} defines no CAN frame sets to decode with", d.id);
    }
    let bus = bus
        .map(str::to_string)
        .or_else(|| (d.buses.len() == 1).then(|| d.buses[0].name.clone()));
    let dec = Decoder::new(d, bus.as_deref()).map_err(|e| anyhow!(e))?;
    let text = std::fs::read_to_string(log).with_context(|| format!("read {}", log.display()))?;
    let parsed = parse_log(&text);
    let label = bus.clone().unwrap_or_else(|| "all".into());
    let (summary, decoded) = summarize(
        &dec,
        &label,
        parsed.frames.iter().map(|f| RawFrame {
            ts: f.ts,
            id: f.id,
            extended: f.extended,
            data: &f.data,
        }),
    );
    if json {
        let v = serde_json::json!({
            "bad_lines": parsed.bad_lines,
            "summary": summary,
            "frames": if frames { serde_json::to_value(&decoded)? } else { serde_json::Value::Null },
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    if frames {
        for (ts, f) in &decoded {
            let ts = ts.map(|t| format!("{t:.6}")).unwrap_or_default();
            let sigs: Vec<String> = f
                .signals
                .iter()
                .map(|s| match &s.value {
                    Value::Number(_) if !s.unit.is_empty() && s.unit != "raw" => {
                        format!("{}={} {}", s.name, s.value, s.unit)
                    }
                    _ => format!("{}={}", s.name, s.value),
                })
                .collect();
            println!("{ts:>18} {:03X} {} {}", f.id, f.frame, sigs.join(" "));
        }
        println!();
    }
    println!(
        "{} ({}), set {}: {} frame(s), {} decoded",
        d.id,
        d.validation.as_str(),
        label,
        summary.frames,
        summary.decoded
    );
    let unit_of = |frame: &str, sig: &str| -> String {
        d.buses
            .iter()
            .flat_map(|b| &b.frames)
            .find(|f| f.name == frame)
            .and_then(|f| f.signals.iter().find(|s| s.name == sig))
            .map(|s| s.unit.clone())
            .unwrap_or_default()
    };
    if !summary.live.is_empty() {
        println!("\nLive channels:");
        for l in &d.live {
            if let Some(s) = summary.live.get(&l.name) {
                let in_set = match (&l.source, bus.as_deref()) {
                    (LiveSource::CanSignal { .. }, None) => true,
                    (LiveSource::CanSignal { bus: b, .. }, Some(x)) => b.eq_ignore_ascii_case(x),
                    _ => false,
                };
                if in_set {
                    println!("  {:<26} {}", l.name, stats_text(s, &l.unit));
                }
            }
        }
    }
    if !summary.per_frame.is_empty() {
        println!("\nFrames:");
        for (name, f) in &summary.per_frame {
            let span = match (f.first_ts, f.last_ts) {
                (Some(a), Some(b)) if b > a => format!("  {:.3} s span", b - a),
                _ => String::new(),
            };
            println!("  0x{:03X} {name}  x{}{span}", f.id, f.count);
            for (sig, st) in &f.signals {
                println!("      {:<34} {}", sig, stats_text(st, &unit_of(name, sig)));
            }
        }
    } else {
        println!("  no frame of this definition's set(s) in the log");
    }
    if parsed.bad_lines > 0 {
        println!("  ({} unparsed line(s) skipped)", parsed.bad_lines);
    }
    if d.validation < opentune_ctrldef::ValidationState::RealImageTested {
        println!(
            "\nnote: {} is {} — its layouts are not yet checked against a capture from a real car.",
            d.id,
            d.validation.as_str()
        );
    }
    Ok(())
}
