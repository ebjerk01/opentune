//! Fixture-gated precision/recall of `relocate` on real sibling-software reads.
//! Set `OPENTUNE_FIXTURES` to the fixture root (never committed); without it
//! every test is a no-op. Run with `--nocapture` to see the per-case table.
//!
//! Ground truth per case (see docs/MAP-LIBRARY.md):
//!
//! * **DQ250 02E** (`temic-dq250-02e`, built on `02E300041N`): `02E300046K` and
//!   `02E300042M` share the code build, so every map sits at the same address;
//!   `02E300042F`/`042G` hold shift-schedule group A 0x15A6 later and the
//!   drive-off table behind its axis at 0x6315C; the R32 software holds group A
//!   0x16EE later and the drive-off axis at 0x63664; `02E300011C` the drive-off
//!   axis at 0x63644 (all checked by `opentune-core/tests/dsg_fixtures.rs`).
//! * **PCR2.1** (`siemens-pcr21`, built on CASM2K30 `pcr21/golf90_stock.bin`):
//!   the two CASM2L95 reads keep each table family at one constant shift (A/B
//!   +0x7D8, C +0x5F8, D +0xB18 in the full 2 MiB read; one byte less in the
//!   owner's read, which is one byte short at the start). The shifts are pinned
//!   by byte-identical tables at both ends of each family and the 0x280 stride.
//! * **EDC16U34 P447** (`bosch-edc16u34`, built on `golf-real/stock.bin`): no
//!   independent ground truth; a placed map must have its header and axes
//!   intact (guaranteed by construction), and the conversion map the sibling
//!   tunes changed must be among them.
use opentune_core::ecudef::{load_dir, MapDef};
use opentune_mapfind::relocate::{relocate, RelocateOptions, RelocationStatus};
use std::path::{Path, PathBuf};

fn fixtures() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("OPENTUNE_FIXTURES")?);
    p.is_dir().then_some(p)
}

fn def_maps(id: &str) -> Vec<MapDef> {
    load_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ecudefs"))
        .unwrap()
        .into_iter()
        .find(|d| d.id == id)
        .unwrap()
        .maps
}

/// The first file in `dir` whose name contains every needle.
fn find(dir: &Path, needles: &[&str]) -> Option<Vec<u8>> {
    let mut names: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    names.sort();
    let p = names.into_iter().find(|p| {
        let n = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        needles.iter().all(|k| n.contains(k))
    })?;
    std::fs::read(p).ok()
}

struct Score {
    placed: usize,
    tp: usize,
    fp: usize,
    gt: usize,
}

/// Relocate and score against `truth` (None = no ground truth for that map).
fn score(
    label: &str,
    a: &[u8],
    maps: &[MapDef],
    b: &[u8],
    truth: impl Fn(&MapDef) -> Option<u32>,
) -> Score {
    let r = relocate(a, maps, b, &RelocateOptions::default());
    let mut s = Score {
        placed: 0,
        tp: 0,
        fp: 0,
        gt: 0,
    };
    for (m, x) in maps.iter().zip(&r.maps) {
        let t = truth(m);
        s.gt += t.is_some() as usize;
        if x.status == RelocationStatus::Relocated {
            s.placed += 1;
            match t {
                Some(t) if Some(t) == x.address => s.tp += 1,
                Some(t) => {
                    s.fp += 1;
                    eprintln!(
                        "  WRONG {}: 0x{:X} (truth 0x{t:X})",
                        m.name,
                        x.address.unwrap()
                    );
                }
                None => {}
            }
        }
    }
    let pct = |a: usize, b: usize| {
        if b == 0 {
            "n/a".to_string()
        } else {
            format!("{:.2}", a as f64 / b as f64)
        }
    };
    eprintln!(
        "{label:<34} placed {:>2}/{:<2} with truth {:>2}: TP {:>2} FP {} precision {} recall {}",
        s.placed,
        maps.len(),
        s.gt,
        s.tp,
        s.fp,
        pct(s.tp, s.tp + s.fp),
        pct(s.tp, s.gt)
    );
    s
}

#[test]
fn dq250_02e_relocation_matches_documented_offsets() {
    let Some(root) = fixtures() else { return };
    let dir = root.join("tcu/dq250-02e");
    let Some(a) = find(&dir, &["02E300041N"]) else {
        return;
    };
    let maps = def_maps("temic-dq250-02e");
    let drive_off = |m: &MapDef| m.name.starts_with("drive-off");
    let group_a = |m: &MapDef| m.name.starts_with("shift schedule A");
    let mut total = Score {
        placed: 0,
        tp: 0,
        fp: 0,
        gt: 0,
    };
    for (tag, needles) in [
        ("046K (same build)", &["02E300046K"][..]),
        ("042M (same build)", &["02E300042M"][..]),
        ("042F", &["02E300042F"][..]),
        ("042G", &["02E300042G"][..]),
        ("R32", &["R32"][..]),
        ("011C", &["02E300011C"][..]),
    ] {
        let Some(b) = find(&dir, needles) else {
            continue;
        };
        let s = score(
            &format!("dq250 041N -> {tag}"),
            &a,
            &maps,
            &b,
            |m| match tag {
                t if t.contains("same build") => Some(m.address),
                "042F" | "042G" if group_a(m) => Some(m.address + 0x15A6),
                "042F" | "042G" if drive_off(m) => Some(0x6315C + 16),
                "R32" if group_a(m) => Some(m.address + 0x16EE),
                "R32" if drive_off(m) => Some(0x63664 + 16),
                "011C" if drive_off(m) => Some(0x63644 + 16),
                _ => None,
            },
        );
        if tag == "042F" || tag == "042G" {
            assert_eq!(s.tp, 11, "{tag}: group A + drive-off all found");
        }
        total.tp += s.tp;
        total.fp += s.fp;
    }
    assert_eq!(total.fp, 0, "a wrong placement");
}

#[test]
fn pcr21_casm2k30_def_relocates_onto_casm2l95() {
    let Some(root) = fixtures() else { return };
    let Ok(a) = std::fs::read(root.join("pcr21/golf90_stock.bin")) else {
        return;
    };
    let maps = def_maps("siemens-pcr21");
    for (file, delta, min_recall) in [
        ("pcr21/vw16_stock.bin", -1i64, 0.95),
        ("pcr21/seat_ibiza_023S_stock.bin", 0, 0.6),
    ] {
        let Ok(b) = std::fs::read(root.join(file)) else {
            continue;
        };
        let s = score(&format!("pcr21 K30 -> {file}"), &a, &maps, &b, |m| {
            let fam = m.name.split("inferred) ").nth(1)?.chars().next()?;
            let shift: i64 = match fam {
                'A' | 'B' => 0x7D8,
                'C' => 0x5F8,
                'D' => 0xB18,
                _ => return None,
            };
            Some((m.address as i64 + shift + delta) as u32)
        });
        assert_eq!(s.fp, 0, "{file}: wrong placement");
        assert!(
            s.tp as f64 >= min_recall * s.gt as f64,
            "{file}: recall {}/{}",
            s.tp,
            s.gt
        );
    }
}

#[test]
fn edc16u34_relocation_onto_p447_siblings() {
    let Some(root) = fixtures() else { return };
    let Ok(a) = std::fs::read(root.join("golf-real/stock.bin")) else {
        return;
    };
    let maps = def_maps("bosch-edc16u34");
    for sw in ["381814", "389289"] {
        let (Ok(b), Ok(tuned)) = (
            std::fs::read(root.join(format!("edc16-pd/golf19_{sw}_ori.bin"))),
            std::fs::read(root.join(format!("edc16-pd/golf19_{sw}_mod.bin"))),
        ) else {
            continue;
        };
        let s = score(&format!("edc16u34 382088 -> {sw}"), &a, &maps, &b, |_| None);
        let r = relocate(&a, &maps, &b, &RelocateOptions::default());
        let conv = maps
            .iter()
            .zip(&r.maps)
            .find(|(m, _)| m.name.starts_with("torque to injection quantity"))
            .map(|(m, x)| (m, x.address));
        let (m, Some(at)) = conv.expect("conversion map") else {
            panic!("{sw}: conversion map not relocated");
        };
        let (s0, e0) = (at as usize, (at + m.byte_len()) as usize);
        assert_ne!(
            b[s0..e0],
            tuned[s0..e0],
            "{sw}: the sibling tune changed it"
        );
        for (m, x) in maps.iter().zip(&r.maps) {
            eprintln!(
                "  {:<9} {:<55} {}",
                format!("{:?}", x.status).to_lowercase(),
                m.name,
                x.address
                    .map(|a| format!("0x{a:X} ({:.2})", x.confidence))
                    .or(x.reason.clone())
                    .unwrap_or_default()
            );
        }
        assert!(s.placed >= 2);
    }
}
