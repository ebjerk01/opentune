//! Corpus run over real community XDFs (never shipped — third-party files).
//!
//! Point `OPENTUNE_XDF_CORPUS` at a directory of `.xdf` files and run
//! `cargo test -p opentune-xdf --test corpus -- --ignored --nocapture`. Prints,
//! per file and in total, how many tables/functions/constants imported clean /
//! with warnings / were refused, and the most frequent issue kinds. It asserts
//! only the invariants that must hold on any input: no panic, and every
//! emitted map is decodable (supported type, non-zero dims).

use opentune_xdf::{parse_xdf, Summary};
use std::collections::BTreeMap;

#[test]
#[ignore = "needs a local XDF corpus via OPENTUNE_XDF_CORPUS"]
fn xdf_corpus_report() {
    let Ok(dir) = std::env::var("OPENTUNE_XDF_CORPUS") else {
        eprintln!("OPENTUNE_XDF_CORPUS not set; skipping");
        return;
    };
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .expect("corpus dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("xdf")))
        .collect();
    paths.sort();

    let mut total = Summary::default();
    let mut errors: BTreeMap<String, usize> = BTreeMap::new();
    let (mut ok_files, mut strict_clean, mut maps) = (0usize, 0usize, 0usize);
    for p in &paths {
        let raw = std::fs::read(p).expect("read");
        let src = String::from_utf8_lossy(&raw);
        match parse_xdf(&src) {
            Ok(imp) => {
                ok_files += 1;
                maps += imp.maps.len();
                for m in &imp.maps {
                    assert!(m.rows >= 1 && m.cols >= 1, "{}: {}", p.display(), m.name);
                }
                let s = imp.summary();
                if imp.strict_failures().is_empty() {
                    strict_clean += 1;
                }
                println!("{:<60} {}", p.file_name().unwrap().to_string_lossy(), s);
                total.absorb(&s);
            }
            Err(e) => {
                let key = format!("{e}");
                let key = key.split(':').next().unwrap_or(&key).to_string();
                println!(
                    "{:<60} ERROR {key}",
                    p.file_name().unwrap().to_string_lossy()
                );
                *errors.entry(key).or_default() += 1;
            }
        }
    }
    println!("\n== XDF corpus: {} file(s)", paths.len());
    println!("   parsed: {ok_files}, strict-clean files: {strict_clean}, file errors: {errors:?}");
    println!("   objects: {total}; maps emitted: {maps}");
    println!("   top issue kinds:");
    for (k, n) in total.top_kinds().into_iter().take(15) {
        println!("     {n:>8}  {k}");
    }
}
