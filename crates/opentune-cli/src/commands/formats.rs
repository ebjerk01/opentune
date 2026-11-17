//! Definition-format importers and exporters (XDF, EcuFlash, WinOLS, A2L).
use crate::util::parse_num;
use anyhow::{anyhow, Result};
use std::path::PathBuf;

/// `import-xdf`
pub fn import_xdf(file: PathBuf, into: Option<PathBuf>) -> Result<()> {
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
    // Scalar constants are a distinct object class; surface them too
    // (previously dropped) with name/addr/type/scaling/unit.
    if !import.constants.is_empty() {
        println!("Constants ({}):", import.constants.len());
        for c in &import.constants {
            let unit = if c.unit.is_empty() {
                String::new()
            } else {
                format!("  [{}]", c.unit)
            };
            let note = if c.notes.is_empty() {
                String::new()
            } else {
                format!("  ! {}", c.notes)
            };
            println!(
                "  {:<28} @0x{:06X}  {:?}/{:?}  x{}{:+}{}{}",
                c.name,
                c.address,
                c.data_type,
                c.endian,
                c.scaling.factor,
                c.scaling.offset,
                unit,
                note
            );
        }
    }
    // Import warnings were also dropped before; print them so a lossy /
    // skipped-entry import is visible rather than silent.
    for w in &import.warnings {
        println!("  ! warning: {w}");
    }
    if let Some(into) = into {
        let txt =
            std::fs::read_to_string(&into).map_err(|e| anyhow!("read {}: {e}", into.display()))?;
        let mut def: opentune_core::ecudef::EcuDef =
            serde_json::from_str(&txt).map_err(|e| anyhow!("parse {}: {e}", into.display()))?;
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
    Ok(())
}

/// `import-ecuflash`
pub fn import_ecuflash(file: PathBuf, into: Option<PathBuf>) -> Result<()> {
    // EcuFlash/RomRaider XML can be Latin-1/Win-1252; decode lossily.
    let raw = std::fs::read(&file).map_err(|e| anyhow!("read {}: {e}", file.display()))?;
    let src = String::from_utf8_lossy(&raw);
    let import = opentune_ecuflash::parse_ecuflash(&src).map_err(|e| anyhow!("{e}"))?;
    let r = &import.rom_id;
    println!(
        "EcuFlash: {} {} {} {} [{}] ({}) — {} map(s)",
        r.make,
        r.model,
        r.submodel,
        r.year,
        r.xmlid,
        r.memmodel,
        import.maps.len()
    );
    for w in &import.warnings {
        println!("  ! warning: {w}");
    }
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
        let txt =
            std::fs::read_to_string(&into).map_err(|e| anyhow!("read {}: {e}", into.display()))?;
        let mut def: opentune_core::ecudef::EcuDef =
            serde_json::from_str(&txt).map_err(|e| anyhow!("parse {}: {e}", into.display()))?;
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
        std::fs::write(&into, serde_json::to_string_pretty(&def)?)
            .map_err(|e| anyhow!("write {}: {e}", into.display()))?;
        println!(
            "Merged {added} new map(s) into {} ({} total).",
            into.display(),
            def.maps.len()
        );
    }
    Ok(())
}

/// `import-ols`
pub fn import_ols(file: PathBuf, version: u32, into: Option<PathBuf>) -> Result<()> {
    let raw = std::fs::read(&file).map_err(|e| anyhow!("read {}: {e}", file.display()))?;
    let project = opentune_ols::OlsProject::parse_version(&raw, version)
        .map_err(|e| anyhow!("parse {}: {e}", file.display()))?;
    println!(
        "WinOLS: format {} — ROM {} byte(s) (version {}) — {} map(s)",
        project.format_version,
        project.rom.size(),
        project.rom.version_index,
        project.maps.len()
    );
    for w in &project.warnings {
        println!("  ! warning: {w}");
    }
    for m in &project.maps {
        println!(
            "  {:<28} @0x{:06X}  {}x{}",
            m.name, m.address, m.rows, m.cols
        );
    }
    if let Some(into) = into {
        let txt =
            std::fs::read_to_string(&into).map_err(|e| anyhow!("read {}: {e}", into.display()))?;
        let mut def: opentune_core::ecudef::EcuDef =
            serde_json::from_str(&txt).map_err(|e| anyhow!("parse {}: {e}", into.display()))?;
        let have: std::collections::HashSet<String> =
            def.maps.iter().map(|m| m.name.clone()).collect();
        let mut added = 0usize;
        for m in project.maps {
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
    Ok(())
}

/// `import-a2l`
pub fn import_a2l(file: PathBuf, into: Option<PathBuf>, base: String) -> Result<()> {
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
    // Rebase CPU addresses to bin file offsets (checked: never wrap).
    let mut maps = import.maps;
    let rebase = rebase_maps_checked(&mut maps, base);
    for d in &rebase.dropped {
        println!("  DROPPED (address below base): {d}");
    }
    for a in &rebase.axis_failures {
        println!("  WARN axis address below base (cleared): {a}");
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
        let txt =
            std::fs::read_to_string(&into).map_err(|e| anyhow!("read {}: {e}", into.display()))?;
        let mut def: opentune_core::ecudef::EcuDef =
            serde_json::from_str(&txt).map_err(|e| anyhow!("parse {}: {e}", into.display()))?;
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
    Ok(())
}

/// `a2l2xdf`
pub fn a2l2xdf(
    file: PathBuf,
    out: Option<PathBuf>,
    base: String,
    only: Option<String>,
) -> Result<()> {
    let base = parse_num(&base)?;
    // A2L text can be Latin-1/Win-1252; decode lossily (ASCII structure),
    // exactly like `import-a2l`.
    let raw = std::fs::read(&file).map_err(|e| anyhow!("read {}: {e}", file.display()))?;
    let src = String::from_utf8_lossy(&raw);
    let import = opentune_a2l::parse_a2l(&src).map_err(|e| anyhow!("{e}"))?;
    let title = import
        .module
        .clone()
        .or_else(|| import.project.clone())
        .unwrap_or_else(|| "a2l2xdf".to_string());
    let parsed_total = import.maps.len();
    let mut maps = import.maps;

    // Optional name filter (case-insensitive substring), applied before
    // rebasing so dropped-vs-filtered stay distinct in the report.
    let mut filtered_out: Vec<String> = Vec::new();
    if let Some(sub) = &only {
        let needle = sub.to_ascii_lowercase();
        let mut kept = Vec::with_capacity(maps.len());
        for m in std::mem::take(&mut maps) {
            if m.name.to_ascii_lowercase().contains(&needle) {
                kept.push(m);
            } else {
                filtered_out.push(m.name.clone());
            }
        }
        maps = kept;
    }

    // Rebase CPU/logical addresses to flash-file offsets (checked).
    let rebase = rebase_maps_checked(&mut maps, base);

    // Flag non-linear conversions: the A2L importer imports any
    // non-affine COMPU_METHOD pre-scaled (factor 1, offset 0) and records
    // it in `notes` — surface that so the XDF is not mistaken for exact.
    let nonlinear: Vec<String> = maps
        .iter()
        .filter(|m| m.notes.contains("pre-scaled"))
        .map(|m| format!("{}: {}", m.name, m.notes))
        .collect();

    if maps.is_empty() {
        return Err(anyhow!(
            "no maps to export (parsed {parsed_total}, {} filtered out, {} dropped by rebasing)",
            filtered_out.len(),
            rebase.dropped.len()
        ));
    }

    let xml = opentune_xdf::export_xdf(Some(&title), &maps);
    match &out {
        Some(p) => {
            std::fs::write(p, &xml).map_err(|e| anyhow!("write {}: {e}", p.display()))?;
        }
        None => print!("{xml}"),
    }

    // --- Conversion-loss report (always; to stderr so it never mixes
    // into a piped XDF on stdout). Never silently lossy. ---
    eprintln!("=== a2l2xdf conversion-loss report ===");
    eprintln!(
        "A2L: project {:?} module {:?} — parsed {parsed_total} map(s), base 0x{base:X}",
        import.project.as_deref().unwrap_or("?"),
        import.module.as_deref().unwrap_or("?"),
    );
    eprintln!("Exported {} map(s) to XDF.", maps.len());
    if let Some(sub) = &only {
        eprintln!(
            "Filtered by --only {sub:?}: {} map(s) excluded (not loss).",
            filtered_out.len()
        );
        for n in &filtered_out {
            eprintln!("    - {n}");
        }
    }
    if rebase.dropped.is_empty() {
        eprintln!("Maps dropped (address rebasing underflowed base): none.");
    } else {
        eprintln!(
            "Maps dropped (address rebasing underflowed base): {}",
            rebase.dropped.len()
        );
        for d in &rebase.dropped {
            eprintln!("    - {d}");
        }
    }
    if rebase.axis_failures.is_empty() {
        eprintln!("Axis addresses that failed rebasing: none.");
    } else {
        eprintln!(
            "Axis addresses that failed rebasing (axis cleared, map kept): {}",
            rebase.axis_failures.len()
        );
        for a in &rebase.axis_failures {
            eprintln!("    - {a}");
        }
    }
    if nonlinear.is_empty() {
        eprintln!("Non-linear scalings imported pre-scaled: none.");
    } else {
        eprintln!(
            "Non-linear scalings flagged (imported pre-scaled, factor 1 / offset 0): {}",
            nonlinear.len()
        );
        for n in &nonlinear {
            eprintln!("    ! {n}");
        }
    }
    if let Some(p) = &out {
        eprintln!("Wrote {} map(s) to {}", maps.len(), p.display());
    }
    Ok(())
}

/// `export-xdf`
pub fn export_xdf(ecudefs: PathBuf, def: String, out: Option<PathBuf>) -> Result<()> {
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
    Ok(())
}

/// `export-a2l`
pub fn export_a2l(ecudefs: PathBuf, def: String, out: Option<PathBuf>) -> Result<()> {
    let defs = opentune_core::ecudef::load_dir(&ecudefs)
        .map_err(|e| anyhow!("load {}: {e}", ecudefs.display()))?;
    let d = defs
        .iter()
        .find(|d| d.id == def)
        .ok_or_else(|| anyhow!("no definition '{def}' in {}", ecudefs.display()))?;
    if d.maps.is_empty() {
        return Err(anyhow!("definition '{def}' has no maps to export"));
    }
    let a2l = opentune_a2l::export_a2l(&d.maps, Some(&d.id), Some(&d.name));
    match out {
        Some(p) => {
            std::fs::write(&p, &a2l).map_err(|e| anyhow!("write {}: {e}", p.display()))?;
            println!("Wrote {} map(s) to {}", d.maps.len(), p.display());
        }
        None => print!("{a2l}"),
    }
    Ok(())
}

/// Outcome of rebasing an A2L import's CPU/logical addresses to flash-file
/// offsets. Both lists are empty when every address sat at or above the base.
#[derive(Debug, Default)]
struct RebaseReport {
    /// Maps whose own data address was below `base` and so could not be located
    /// in the image; each was dropped from the map list. Formatted for display.
    dropped: Vec<String>,
    /// Stored axis addresses that were below `base`. The map was kept but the
    /// offending axis address was cleared (so no wrong byte is ever read).
    axis_failures: Vec<String>,
}

/// Rebase every map (and its stored axes) in place from A2L CPU/logical
/// addresses down to flash-file offsets by subtracting `base`, using **checked**
/// subtraction so an address below `base` is reported rather than silently
/// wrapped. A map whose own data address underflows is removed from `maps`; an
/// axis address that underflows is cleared and the map kept. This is the single
/// rebasing path shared by `import-a2l` and `a2l2xdf` so both behave identically.
fn rebase_maps_checked(maps: &mut Vec<opentune_core::ecudef::MapDef>, base: u32) -> RebaseReport {
    fn rebase_axis(
        ax: Option<&mut opentune_core::ecudef::AxisDef>,
        base: u32,
        map_name: &str,
        which: &str,
        failures: &mut Vec<String>,
    ) {
        if let Some(ax) = ax {
            if let Some(a) = ax.address {
                match a.checked_sub(base) {
                    Some(v) => ax.address = Some(v),
                    None => {
                        ax.address = None;
                        failures.push(format!(
                            "{map_name} {which}-axis @0x{a:X} < base 0x{base:X}"
                        ));
                    }
                }
            }
        }
    }

    let mut report = RebaseReport::default();
    let mut kept = Vec::with_capacity(maps.len());
    for mut m in std::mem::take(maps) {
        match m.address.checked_sub(base) {
            Some(a) => m.address = a,
            None => {
                report
                    .dropped
                    .push(format!("{} @0x{:X} < base 0x{:X}", m.name, m.address, base));
                continue;
            }
        }
        rebase_axis(
            m.x_axis.as_mut(),
            base,
            &m.name,
            "x",
            &mut report.axis_failures,
        );
        rebase_axis(
            m.y_axis.as_mut(),
            base,
            &m.name,
            "y",
            &mut report.axis_failures,
        );
        kept.push(m);
    }
    *maps = kept;
    report
}
