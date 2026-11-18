//! Corpus run of the FastECU importer over real EcuFlash / RomRaider
//! definitions (never shipped — third-party files).
//!
//! `OPENTUNE_FASTECU_CORPUS=<dir> cargo test -p opentune-ecuflash --test
//! fastecu_corpus -- --ignored --nocapture`. Every `*.xml` under the directory
//! is indexed by rom `xmlid` (the base-definition store, like FastECU's
//! definition directory); every calibration rom (one that inherits from a
//! base) is imported and the clean / warned / refused counts and top issue
//! kinds are printed.

use opentune_ecuflash::fastecu::{index_base_dir, load_base, parse_fastecu, Options};
use opentune_xdf::Summary;
use std::path::{Path, PathBuf};

fn xml_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).expect("dir").flatten() {
        let p = e.path();
        if p.is_dir() {
            xml_files(&p, out);
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("xml")) {
            out.push(p);
        }
    }
}

#[test]
#[ignore = "needs a local definition corpus via OPENTUNE_FASTECU_CORPUS"]
fn fastecu_corpus_report() {
    let Ok(dir) = std::env::var("OPENTUNE_FASTECU_CORPUS") else {
        eprintln!("OPENTUNE_FASTECU_CORPUS not set; skipping");
        return;
    };
    let dir = PathBuf::from(dir);
    let index = index_base_dir(&dir).expect("index");
    let mut files = Vec::new();
    xml_files(&dir, &mut files);
    files.sort();

    let mut total = Summary::default();
    let (mut roms, mut errors, mut maps, mut strict_clean) = (0usize, 0usize, 0usize, 0usize);
    let mut by_source = std::collections::BTreeMap::<String, Summary>::new();
    for f in &files {
        let raw = std::fs::read(f).expect("read");
        let src = String::from_utf8_lossy(&raw);
        let Ok(doc) = roxmltree::Document::parse(&src) else {
            errors += 1;
            continue;
        };
        // Calibration roms: those with a base (`base=` or `<include>`).
        let ids: Vec<String> = doc
            .descendants()
            .filter(|n| n.is_element() && n.tag_name().name() == "rom")
            .filter(|r| {
                r.has_attribute("base")
                    || r.children()
                        .any(|c| c.is_element() && c.tag_name().name() == "include")
            })
            .filter_map(|r| {
                r.descendants()
                    .find(|c| c.is_element() && c.tag_name().name() == "xmlid")
                    .and_then(|x| x.text())
                    .map(|t| t.trim().to_string())
            })
            .collect();
        drop(doc);
        for id in ids {
            let opts = Options {
                rom: Some(id.clone()),
            };
            match parse_fastecu(&src, &opts, |b| load_base(&index, b)) {
                Ok(imp) => {
                    roms += 1;
                    maps += imp.maps.len();
                    let s = imp.summary();
                    if imp.strict_failures().is_empty() {
                        strict_clean += 1;
                    }
                    by_source
                        .entry(imp.source.to_string())
                        .or_default()
                        .absorb(&s);
                    total.absorb(&s);
                }
                Err(e) => {
                    errors += 1;
                    println!("{} [{id}]: ERROR {e}", f.display());
                }
            }
        }
    }
    println!(
        "\n== FastECU corpus: {} file(s), {roms} calibration rom(s), {errors} error(s)",
        files.len()
    );
    println!("   strict-clean roms: {strict_clean}; maps emitted: {maps}");
    for (src, s) in &by_source {
        println!("   {src}: {s}");
    }
    println!("   total: {total}");
    println!("   top issue kinds:");
    for (k, n) in total.top_kinds().into_iter().take(15) {
        println!("     {n:>8}  {k}");
    }
}
