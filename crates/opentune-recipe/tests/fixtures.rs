//! Fixture-gated round trips on real dumps. Set `OPENTUNE_FIXTURES` to the
//! fixture root (never committed); without it every test is a no-op.

use opentune_recipe::{apply, create, ApplyOptions, CreateOptions, Recipe};
use std::path::PathBuf;

fn fixtures() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("OPENTUNE_FIXTURES")?);
    p.is_dir().then_some(p)
}

/// Create a recipe from `stock`/`tuned`, apply it to `stock`, and report
/// whether the output is byte-identical to `tuned`.
fn round_trip(stock: &[u8], tuned: &[u8], checksum: Option<&str>) -> Result<bool, String> {
    let opts = CreateOptions {
        checksum: checksum.map(str::to_string),
        ..CreateOptions::default()
    };
    let (r, _) = create(stock, tuned, &opts).map_err(|e| e.to_string())?;
    let r = Recipe::from_json(&r.to_json()).map_err(|e| e.to_string())?;
    let rep = apply(&r, stock, &ApplyOptions::default());
    if let Some(why) = rep.refusal {
        return Err(why);
    }
    assert!(rep.attestation.as_ref().unwrap().satisfied);
    Ok(rep.output.as_deref() == Some(tuned))
}

#[test]
fn golf_edc16_recipe_reproduces_the_tuned_file() {
    let Some(root) = fixtures() else { return };
    let dir = root.join("golf-real");
    let (Ok(stock), Ok(tuned)) = (
        std::fs::read(dir.join("stock.bin")),
        std::fs::read(dir.join("tuned_fixed.bin")),
    ) else {
        return;
    };
    let (r, _) = create(&stock, &tuned, &CreateOptions::default()).unwrap();
    assert_eq!(r.source.family, "EDC16");
    assert_eq!(r.checksum.as_deref(), Some("edc16"));
    assert!(!r.checksum_fields.is_empty(), "checksum field detected");
    assert_eq!(round_trip(&stock, &tuned, None), Ok(true));
    // The uncorrected tune applies too; its checksum is fixed on the way out,
    // so the result is exactly the corrected tune.
    if let Ok(unc) = std::fs::read(dir.join("tuned_uncorrected.bin")) {
        let (r, _) = create(&stock, &unc, &CreateOptions::default()).unwrap();
        let rep = apply(&r, &stock, &ApplyOptions::default());
        assert_eq!(rep.refusal, None);
        assert_eq!(rep.output.as_deref(), Some(&tuned[..]));
    }
}

/// Every real EDC17 pair: the recipe never patches a compensation word, the
/// output is attested against the stock read, and the result is the tuner's
/// file byte-for-byte. A pair whose tune changed a nested sub-block of
/// unknown rule (CP42) is refused by default and, with the explicit override,
/// reproduces the tuned file while its attestation stays unsatisfied.
#[test]
fn edc17_pairs_apply_or_refuse_for_a_stated_reason() {
    use opentune_checksum::edc17::{is_nested, parse_directory};
    let Some(root) = fixtures() else { return };
    let Ok(rd) = std::fs::read_dir(root.join("edc17")) else {
        return;
    };
    let mut stocks: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.to_string_lossy().ends_with("_ori.bin"))
        .collect();
    stocks.sort();
    let (mut identical, mut override_only) = (0, 0);
    for (i, p) in stocks.iter().enumerate() {
        let tuned_path = PathBuf::from(p.to_string_lossy().replace("_ori.bin", "_mod.bin"));
        let (Ok(stock), Ok(tuned)) = (std::fs::read(p), std::fs::read(&tuned_path)) else {
            continue;
        };
        let label = format!("edc17 pair #{i}");
        let (r, _) = create(&stock, &tuned, &CreateOptions::default()).unwrap();
        assert_eq!(r.checksum.as_deref(), Some("edc17"), "{label}");
        let dir = parse_directory(&stock);
        // No patch covers the compensation word of a top-level block.
        for b in dir.iter().filter(|b| !is_nested(**b, &dir)) {
            let w = b.store_offset() as u64;
            for pa in &r.patches {
                assert!(
                    pa.offset + pa.len() as u64 <= w || pa.offset >= w + 4,
                    "{label}: a patch covers the word @0x{w:X}"
                );
            }
        }
        // A changed nested sub-block cannot be verified.
        let nested_changed = dir.iter().any(|b| {
            let (s, e) = b.range();
            is_nested(*b, &dir) && stock[s..e] != tuned[s..e]
        });
        let rep = apply(&r, &stock, &ApplyOptions::default());
        if nested_changed {
            let why = rep.refusal.expect("unverifiable nested block must refuse");
            assert!(why.contains("cannot be verified"), "{label}: {why}");
            let rep = apply(
                &r,
                &stock,
                &ApplyOptions {
                    allow_unverified: true,
                    ..ApplyOptions::default()
                },
            );
            assert_eq!(rep.refusal, None, "{label}");
            assert!(!rep.attestation.unwrap().satisfied, "{label}");
            assert_eq!(rep.output.as_deref(), Some(&tuned[..]), "{label}");
            override_only += 1;
        } else {
            assert_eq!(rep.refusal, None, "{label}");
            assert!(rep.attestation.unwrap().satisfied, "{label}");
            assert_eq!(rep.output.as_deref(), Some(&tuned[..]), "{label}");
            identical += 1;
        }
    }
    eprintln!("EDC17 pairs: identical+attested={identical} override-only={override_only}");
}

#[test]
fn pair_corpus_round_trip_summary() {
    let Some(root) = fixtures() else { return };
    // (label, stock path, tuned path) in directory + name order.
    let mut pairs: Vec<(String, PathBuf, PathBuf)> = Vec::new();
    for d in [
        "edc16-pairs",
        "edc16-pd",
        "edc16-cr",
        "edc15",
        "edc17",
        "pcr21",
    ] {
        let Ok(rd) = std::fs::read_dir(root.join(d)) else {
            continue;
        };
        let mut names: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        names.sort();
        for p in names {
            let s = p.to_string_lossy().to_string();
            let t = if s.ends_with("_ori.bin") {
                s.replace("_ori.bin", "_mod.bin")
            } else if s.ends_with("_stock.bin") {
                s.replace("_stock.bin", "_tuned.bin")
            } else {
                continue;
            };
            let label = format!("{d}/{}", p.file_name().unwrap().to_string_lossy());
            pairs.push((label, p, PathBuf::from(t)));
        }
    }
    // Each round trip is independent: spread the pairs over the available
    // cores (one pair per worker at a time) instead of walking them one by one.
    // A pair that can't be read or differs in size yields no row, as before.
    type Row = Option<(String, Result<bool, String>)>;
    let run = |(label, a, b): &(String, PathBuf, PathBuf)| -> Row {
        let (Ok(a), Ok(b)) = (std::fs::read(a), std::fs::read(b)) else {
            return None;
        };
        if a.len() != b.len() {
            return None;
        }
        Some((label.clone(), round_trip(&a, &b, None)))
    };
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(pairs.len().max(1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots: Vec<std::sync::Mutex<Row>> =
        pairs.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(pair) = pairs.get(i) else { break };
                *slots[i].lock().unwrap() = run(pair);
            });
        }
    });
    // Results come back in pair order, so the report reads exactly as before.
    let rows: Vec<(String, Result<bool, String>)> = slots
        .into_iter()
        .filter_map(|m| m.into_inner().unwrap())
        .collect();
    let identical = rows.iter().filter(|r| r.1 == Ok(true)).count();
    let differ = rows.iter().filter(|r| r.1 == Ok(false)).count();
    for (name, res) in &rows {
        eprintln!(
            "{name:<52} {}",
            match res {
                Ok(true) => "identical".to_string(),
                Ok(false) => "applied+attested, checksum bytes differ from tuner's".to_string(),
                Err(e) => format!("refused: {e}"),
            }
        );
    }
    eprintln!(
        "pairs={} identical={identical} differ={differ} refused={}",
        rows.len(),
        rows.len() - identical - differ
    );
}
