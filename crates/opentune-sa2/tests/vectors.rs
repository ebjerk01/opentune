//! Differential vectors: synthetic SA2 programs whose expected keys come from
//! the public reference implementation (`bri3d/sa2-seed-key`, MIT), produced
//! by `tools/sa2/sa2_differential.py generate`. No OEM script is in the file.
use opentune_sa2::{parse_hex, run};

const VECTORS: &str = include_str!("fixtures/sa2_vectors.txt");

#[test]
fn reference_vectors_agree() {
    let mut checked = 0usize;
    let mut errors = 0usize;
    let mut hand = 0usize;
    for (n, line) in VECTORS.lines().enumerate() {
        let (body, tag) = line.split_once('#').unwrap_or((line, ""));
        let parts: Vec<&str> = body.split_whitespace().collect();
        let [script, seed, expected] = parts.as_slice() else {
            continue;
        };
        let script = parse_hex(script).expect("fixture script hex");
        let seed = u32::from_str_radix(seed, 16).expect("fixture seed");
        let got = run(&script, seed);
        if *expected == "ERR" {
            errors += 1;
            assert!(
                got.is_err(),
                "line {}: [{}] expected an error, got {got:?}",
                n + 1,
                tag.trim()
            );
        } else {
            let want = u32::from_str_radix(expected, 16).expect("fixture key");
            assert_eq!(
                got,
                Ok(want),
                "line {}: [{}] seed {seed:08X}",
                n + 1,
                tag.trim()
            );
        }
        if tag.trim_start().starts_with("hand:") {
            hand += 1;
        }
        checked += 1;
    }
    assert!(checked > 1000, "fixture too small: {checked}");
    assert!(errors > 20, "fixture lacks error cases: {errors}");
    assert!(hand > 100, "fixture lacks the hand-written cases: {hand}");
}
