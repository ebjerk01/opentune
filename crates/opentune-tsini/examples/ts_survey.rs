//! Parse every `.ini` (and `.msq`) under a directory and print a summary: a
//! robustness check against real definition files kept outside the repo.
//!
//! ```sh
//! cargo run -p opentune-tsini --example ts_survey -- /path/to/inis [--ini for.msq]
//! ```
use opentune_tsini::*;
use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = PathBuf::from(args.first().expect("usage: ts_survey <dir> [--ini x.ini]"));
    let msq_ini = args
        .iter()
        .position(|a| a == "--ini")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    let mut files = Vec::new();
    walk(&dir, &mut files);
    files.sort();
    let (mut n, mut clean) = (0, 0);
    for f in files
        .iter()
        .filter(|f| f.extension().is_some_and(|e| e == "ini"))
    {
        n += 1;
        match load_ini(f, &IniOptions::default()) {
            Ok(ini) => {
                let inv: Vec<&Warning> = ini.warnings_of(WarnKind::Invalid).collect();
                if inv.is_empty() {
                    clean += 1;
                }
                println!(
                    "{} | sig {:?} | {} pages {}B | {} const {} pc {} och | {} tables {} curves {} menus | envelope {} | warn invalid {} ref {} ignored {}",
                    f.file_name().unwrap().to_string_lossy(),
                    ini.signature.as_deref().unwrap_or("-"),
                    ini.pages.len(),
                    ini.total_page_bytes(),
                    ini.constants.len(),
                    ini.pc_variables.len(),
                    ini.output_channels.len(),
                    ini.tables.len(),
                    ini.curves.len(),
                    ini.menus.len(),
                    ini.comms.uses_envelope(),
                    inv.len(),
                    ini.warnings_of(WarnKind::Reference).count(),
                    ini.warnings_of(WarnKind::Ignored).count(),
                );
                for w in inv.iter().take(5) {
                    println!("    INVALID line {} [{}]: {}", w.line, w.section, w.message);
                }
                for w in ini.warnings_of(WarnKind::Reference).take(3) {
                    println!("    ref line {} [{}]: {}", w.line, w.section, w.message);
                }
                // Every page constant must decode from a blank tune.
                let t = Tune::blank(&ini);
                let bad: Vec<String> = t
                    .values(&ini)
                    .into_iter()
                    .filter_map(|(n, r)| r.err().map(|e| format!("{n}: {e}")))
                    .collect();
                if !bad.is_empty() {
                    println!("    {} constants do not decode, e.g. {}", bad.len(), bad[0]);
                }
                let block = vec![0u8; ini.comms.och_block_size.unwrap_or(0)];
                let s = decode_channels(&ini, &block, &|n| t.var(&ini, n), None);
                let unsupported = s.iter().filter(|x| x.note.contains("unsupported")).count();
                let missing = s.iter().filter(|x| x.value.is_none()).count();
                println!(
                    "    channels: {} decoded, {} without value ({} unsupported expressions)",
                    s.len() - missing,
                    missing,
                    unsupported
                );
            }
            Err(e) => println!("{}: ERROR {e}", f.display()),
        }
    }
    println!("INI: {clean}/{n} parsed with no invalid lines");

    // INI per signature, for picking the definition a tune was saved with.
    let mut by_sig: std::collections::BTreeMap<String, PathBuf> = Default::default();
    for f in files
        .iter()
        .filter(|f| f.extension().is_some_and(|e| e == "ini"))
    {
        if let Ok(i) = load_ini(f, &IniOptions::default()) {
            if let Some(s) = i.signature {
                by_sig
                    .entry(normalize_signature(&s).to_owned())
                    .or_insert(f.clone());
            }
        }
    }
    if let Some(fallback) = msq_ini {
        for f in files
            .iter()
            .filter(|f| f.extension().is_some_and(|e| e == "msq"))
        {
            let m = match Msq::load(f) {
                Ok(m) => m,
                Err(e) => {
                    println!("{}: ERROR {e}", f.display());
                    continue;
                }
            };
            let opts = IniOptions {
                set: m.settings_conditions(),
                ..IniOptions::default()
            };
            let ini_path = m
                .signature()
                .and_then(|s| by_sig.get(normalize_signature(s)))
                .unwrap_or(&fallback);
            let ini2 = load_ini(ini_path, &opts).unwrap();
            let r = m.to_tune(
                &ini2,
                Tune::blank(&ini2),
                MsqLoadOptions {
                    force_signature: true,
                },
            );
            match r {
                Ok((t, rep)) => {
                    let (m2, sk) = Msq::from_tune(&ini2, &t, &MsqMeta::default());
                    let same = Msq::parse(&m.to_xml()).map(|x| x == m).unwrap_or(false);
                    let (t2, _) = m2
                        .to_tune(&ini2, Tune::blank(&ini2), MsqLoadOptions::default())
                        .unwrap();
                    println!(
                        "{} | sig {:?} via {} | applied {} unknown {} skipped {} out-of-range {} | xml round trip {} | tune->msq->tune bytes equal {} (skipped {})",
                        f.file_name().unwrap().to_string_lossy(),
                        m.signature().unwrap_or("-"),
                        ini_path.file_name().unwrap().to_string_lossy(),
                        rep.applied,
                        rep.unknown.len(),
                        rep.skipped.len(),
                        rep.out_of_range.len(),
                        same,
                        t2.pages == t.pages,
                        sk.len()
                    );
                    for (n, why) in rep.skipped.iter().take(3) {
                        println!("    skipped {n}: {why}");
                    }
                }
                Err(e) => println!("{}: ERROR {e}", f.display()),
            }
        }
    }
}
