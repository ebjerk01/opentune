//! Fixture-gated recall checks against real dumps. Set `OPENTUNE_FIXTURES` to
//! the fixture root (never committed); without it every test is a no-op.
//!
//! Two measurements:
//! * **definition recall** — do the maps defined in `ecudefs/` appear among the
//!   top candidates of the image they were derived from?
//! * **diff coverage** — for stock/tuned pairs, the regions a tuner changed are
//!   (almost always) maps: what fraction of changed regions does a candidate
//!   cover, and what fraction of candidates touch a changed region (a lower
//!   bound on precision — a tune changes only a few of the real maps)?

use opentune_core::ecudef::{load_dir, MapDef};
use opentune_mapfind::{find_maps, match_defs, FindOptions, MapCandidate, MatchKind};
use std::path::{Path, PathBuf};

fn fixtures() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("OPENTUNE_FIXTURES")?);
    p.is_dir().then_some(p)
}

fn ecudefs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ecudefs")
}

fn def_maps(id: &str) -> Vec<MapDef> {
    load_dir(&ecudefs_dir())
        .unwrap()
        .into_iter()
        .find(|d| d.id == id)
        .unwrap()
        .maps
}

#[test]
fn edc16u34_defined_maps_are_top_candidates() {
    let Some(root) = fixtures() else { return };
    let Ok(img) = std::fs::read(root.join("golf-real/stock.bin")) else {
        return;
    };
    let cands = find_maps(&img, &FindOptions::default());
    let defs = def_maps("bosch-edc16u34");
    let matches = match_defs(&cands, &defs);
    eprintln!("golf-real/stock.bin: {} candidates", cands.len());
    for m in &matches {
        eprintln!(
            "  {:?} rank {:?} @0x{:06X} {}",
            m.kind, m.rank, m.address, m.name
        );
    }
    for m in &matches {
        assert!(
            matches!(m.kind, MatchKind::Exact | MatchKind::Axes),
            "{} not recovered: {:?}",
            m.name,
            m.kind
        );
        assert!(m.rank.unwrap() < 150, "{} ranked {:?}", m.name, m.rank);
    }
}

/// Changed regions between two images, merged across gaps < 16 bytes, keeping
/// only regions longer than 4 bytes (shorter ones are mostly checksum words).
fn changed_regions(a: &[u8], b: &[u8]) -> Vec<(u32, u32)> {
    let n = a.len().min(b.len());
    let mut out: Vec<(u32, u32)> = Vec::new();
    for i in 0..n {
        if a[i] != b[i] {
            match out.last_mut() {
                Some((_, e)) if i as u32 <= *e + 16 => *e = i as u32 + 1,
                _ => out.push((i as u32, i as u32 + 1)),
            }
        }
    }
    out.retain(|(s, e)| e - s > 4);
    out
}

fn overlaps(c: &MapCandidate, (s, e): (u32, u32)) -> bool {
    let (cs, ce) = c.footprint();
    cs < e && s < ce
}

#[test]
fn diff_coverage_on_stock_tuned_pairs() {
    let Some(root) = fixtures() else { return };
    let dirs = [
        "golf-real",
        "edc16-pairs",
        "edc16-pd",
        "edc16-cr",
        "edc15",
        "edc17",
        "me7",
        "pcr21",
    ];
    let mut pairs: Vec<(PathBuf, PathBuf)> = Vec::new();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(root.join(d)) else {
            continue;
        };
        let mut names: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        names.sort();
        for p in &names {
            let s = p.to_string_lossy();
            let tuned = if s.ends_with("_ori.bin") {
                PathBuf::from(s.replace("_ori.bin", "_mod.bin"))
            } else if s.ends_with("/stock.bin") {
                PathBuf::from(s.replace("/stock.bin", "/tuned_fixed.bin"))
            } else if s.ends_with("_stock.bin") {
                PathBuf::from(s.replace("_stock.bin", "_tuned.bin"))
            } else {
                continue;
            };
            if tuned.is_file() {
                pairs.push((p.clone(), tuned));
            }
        }
    }
    if pairs.is_empty() {
        return;
    }
    let (mut regions, mut covered, mut cands_total, mut cands_hit) = (0, 0, 0, 0);
    let (mut top_regions, mut top_hit) = (0, 0);
    eprintln!(
        "pair                                         regions covered  cands  touched  top40"
    );
    // One pair per worker: each scan is independent, so spread them over the
    // available cores instead of walking ~50 images one by one.
    // Per pair: (changed regions, covered, candidates, candidates touching a change, top-40 hits).
    type Row = Option<(usize, usize, usize, usize, usize)>;
    let measure = |(a, b): &(PathBuf, PathBuf)| -> Row {
        let (Ok(sa), Ok(sb)) = (std::fs::read(a), std::fs::read(b)) else {
            return None;
        };
        if sa.len() != sb.len() {
            return None;
        }
        let regs = changed_regions(&sa, &sb);
        let cands = find_maps(&sa, &FindOptions::default());
        let cov = regs
            .iter()
            .filter(|r| cands.iter().any(|c| overlaps(c, **r)))
            .count();
        let hit = cands
            .iter()
            .filter(|c| regs.iter().any(|r| overlaps(c, *r)))
            .count();
        let top = cands
            .iter()
            .take(40)
            .filter(|c| regs.iter().any(|r| overlaps(c, *r)))
            .count();
        Some((regs.len(), cov, cands.len(), hit, top))
    };
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(pairs.len());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let rows: Vec<Row> = {
        let slots: Vec<std::sync::Mutex<Row>> =
            pairs.iter().map(|_| std::sync::Mutex::new(None)).collect();
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(pair) = pairs.get(i) else { break };
                    *slots[i].lock().unwrap() = measure(pair);
                });
            }
        });
        slots.into_iter().map(|m| m.into_inner().unwrap()).collect()
    };
    for ((a, _), row) in pairs.iter().zip(rows) {
        let Some((n_regs, cov, n_cands, hit, top)) = row else {
            continue;
        };
        eprintln!(
            "{:<44} {:>7} {:>7} {:>6} {:>8} {:>6}",
            a.strip_prefix(&root).unwrap_or(a).display(),
            n_regs,
            cov,
            n_cands,
            hit,
            top
        );
        regions += n_regs;
        covered += cov;
        cands_total += n_cands;
        cands_hit += hit;
        top_regions += 40.min(n_cands);
        top_hit += top;
    }
    let pct = |a: usize, b: usize| 100.0 * a as f64 / b.max(1) as f64;
    eprintln!(
        "TOTAL pairs={} changed regions covered {covered}/{regions} ({:.1}%); \
         candidates touching a change {cands_hit}/{cands_total} ({:.1}%); \
         top-40 touching a change {top_hit}/{top_regions} ({:.1}%)",
        pairs.len(),
        pct(covered, regions),
        pct(cands_hit, cands_total),
        pct(top_hit, top_regions)
    );
    assert!(regions > 0);
    // Loose floor: the finder must explain a meaningful share of real edits.
    assert!(pct(covered, regions) >= 40.0);
}
