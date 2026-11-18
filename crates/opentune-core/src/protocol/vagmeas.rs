//! VAG **measuring-block** cells: the records a VAG module returns for KWP2000
//! `readDataByLocalIdentifier` (`0x21 <group>`, see
//! [`Kwp2000Client::read_local_id`](crate::protocol::kwp2000::Kwp2000Client::read_local_id))
//! are a run of 3-byte cells `(formula, a, b)`, typically four per group.
//! The formula byte selects how `a` and `b` scale to a physical value.
//!
//! This is the one shared formula table (used by `opentune-haldex` and
//! `opentune-dsg`). It holds only formulas with a public, cross-checked source:
//! the six below come from Forbes-Automotive/OpenHaldex-C6 (MIT) — README
//! "Live Diagnostics" and `src/OpenHaldexC6_UDS.cpp` — which state they were
//! checked against a scan tool on a Haldex Gen4 unit, and OpenTune's Haldex
//! tests check them against that project's public real-unit capture.
//! **Community confidence.** Any other formula byte is reported raw, never
//! guessed. The formula byte is a VAG-wide convention, but a formula is only
//! confirmed for a module once a capture from that module agrees.

/// Scale a measuring-block cell: `Some((value, unit))` for a known formula,
/// `None` (report raw) for any other.
pub fn scale(formula: u8, a: u8, b: u8) -> Option<(f64, &'static str)> {
    let (a, b) = (a as f64, b as f64);
    Some(match formula {
        0x06 => (0.001 * a * b, "V"),
        0x0E => (0.01 * a * (b - 100.0), "bar"),
        0x18 => (0.001 * a * b, "A"),
        0x1A => (b - a, "°C"),
        0x21 => (0.01 * a * b, "%"),
        0x5E => (0.1 * a * (b - 128.0), "Nm"),
        _ => return None,
    })
}

/// Formula bytes [`scale`] knows.
pub const KNOWN_FORMULAS: [u8; 6] = [0x06, 0x0E, 0x18, 0x1A, 0x21, 0x5E];

/// One decoded measuring-block cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell {
    pub formula: u8,
    pub a: u8,
    pub b: u8,
    /// Scaled value, `None` for a formula not in the table.
    pub value: Option<f64>,
    /// Unit of `value` (empty when raw).
    pub unit: &'static str,
}

/// Split a measuring-block record (after the echoed group) into cells. A
/// trailing partial cell is ignored; callers keep the raw record.
pub fn cells(rec: &[u8]) -> Vec<Cell> {
    rec.chunks_exact(3)
        .map(|c| {
            let s = scale(c[0], c[1], c[2]);
            Cell {
                formula: c[0],
                a: c[1],
                b: c[2],
                value: s.map(|(v, _)| v),
                unit: s.map(|(_, u)| u).unwrap_or(""),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_examples() {
        assert_eq!(scale(0x1A, 0, 24), Some((24.0, "°C")));
        let (v, u) = scale(0x18, 10, 191).unwrap();
        assert!((v - 1.910).abs() < 1e-9 && u == "A");
        let (v, _) = scale(0x5E, 160, 255).unwrap();
        assert!((v - 2032.0).abs() < 1e-9);
        for f in 0..=255u8 {
            assert_eq!(scale(f, 1, 2).is_some(), KNOWN_FORMULAS.contains(&f));
        }
    }

    #[test]
    fn cells_drop_partial_tail() {
        let c = cells(&[0x0E, 27, 200, 0x77, 1, 2, 0x21]);
        assert_eq!(c.len(), 2);
        assert!((c[0].value.unwrap() - 27.0).abs() < 1e-9);
        assert_eq!((c[1].value, c[1].unit), (None, ""));
    }
}
