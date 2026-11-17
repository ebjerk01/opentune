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

#[test]
fn pair_corpus_round_trip_summary() {
    let Some(root) = fixtures() else { return };
    let mut rows = Vec::new();
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
            let (Ok(a), Ok(b)) = (std::fs::read(&p), std::fs::read(&t)) else {
                continue;
            };
            if a.len() != b.len() {
                continue;
            }
            let res = round_trip(&a, &b, None);
            rows.push((
                format!("{d}/{}", p.file_name().unwrap().to_string_lossy()),
                res,
            ));
        }
    }
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
