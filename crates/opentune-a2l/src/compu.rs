//! `COMPU_METHOD` interpretation: recover an exact linear scaling where the
//! conversion really is affine, and otherwise report the conversion verbatim so
//! the caller can import the map **pre-scaled** with a note (never guessed).
//!
//! OpenTune's [`Scaling`](opentune_core::ecudef::Scaling) is strictly linear
//! (`physical = raw * factor + offset`). A2L conversions come in several flavours:
//!
//! * `IDENTITY` — physical equals raw. Linear with factor 1, offset 0.
//! * `LINEAR` — `COEFFS_LINEAR a b` means `physical = a*raw + b`. Exact.
//! * `RAT_FUNC` — `COEFFS a b c d e f` defines the **physical→internal** rational
//!   function `int = (a·p² + b·p + c) / (d·p² + e·p + f)`. When `a=d=e=0` and
//!   `b≠0` this inverts to the affine `physical = (f/b)·raw − c/b`, which we
//!   recover exactly. Any other coefficient set is genuinely non-linear and is
//!   reported, not approximated.
//! * `TAB_INTP` / `TAB_NOINTP` / `TAB_VERB` — table conversions (a `COMPU_TAB`
//!   /`COMPU_VTAB`). Not linear; reported.
//! * `FORM` — an arbitrary formula string. Not generally linear; reported.

use crate::lex::Block;
use crate::num::parse_f64;
use opentune_core::ecudef::Scaling;

/// The interpreted conversion of a `COMPU_METHOD`.
#[derive(Debug, Clone, PartialEq)]
pub enum Conv {
    /// Exact affine conversion `physical = raw * factor + offset`.
    Linear { factor: f64, offset: f64 },
    /// A conversion that is not a linear function of the raw value. The string
    /// describes it (e.g. `"TAB_INTP table conversion"`, `"RAT_FUNC non-affine
    /// coeffs [..]"`) and is meant to be surfaced in a map's `notes`. The caller
    /// imports pre-scaled (factor 1, offset 0).
    NonLinear(String),
}

impl Conv {
    /// The OpenTune [`Scaling`] for this conversion, plus an optional note
    /// explaining a pre-scaled import.
    pub fn to_scaling(&self) -> (Scaling, Option<String>) {
        match self {
            Conv::Linear { factor, offset } => (
                Scaling {
                    factor: *factor,
                    offset: *offset,
                },
                None,
            ),
            Conv::NonLinear(desc) => (Scaling::default(), Some(desc.clone())),
        }
    }
}

/// A parsed `COMPU_METHOD`.
#[derive(Debug, Clone, PartialEq)]
pub struct CompuMethod {
    pub name: String,
    /// Physical unit string (`COMPU_METHOD` field 5), e.g. `"deg"`, `"mg/stroke"`.
    pub unit: String,
    pub conv: Conv,
}

/// Parse a `COMPU_METHOD` block.
///
/// Positional fields: `Name  "LongIdentifier"  ConversionType  "Format"
/// "Unit"`, followed by optional keyword fields (`COEFFS`, `COEFFS_LINEAR`,
/// `COMPU_TAB_REF`, `FORMULA`, …).
pub fn parse_compu_method(block: &Block) -> CompuMethod {
    let name = block.word(0).unwrap_or_default().to_string();
    let conv_type = block.word(2).unwrap_or_default().to_string();
    let unit = block.word(4).unwrap_or_default().to_string();

    let conv = match conv_type.as_str() {
        "IDENTITY" => Conv::Linear {
            factor: 1.0,
            offset: 0.0,
        },
        "LINEAR" => linear_coeffs(block),
        "RAT_FUNC" => rat_func(block),
        "TAB_INTP" | "TAB_NOINTP" => Conv::NonLinear(format!(
            "{conv_type} table conversion (interpolated lookup); imported pre-scaled"
        )),
        "TAB_VERB" => {
            Conv::NonLinear("TAB_VERB verbal/enum conversion; imported pre-scaled".to_string())
        }
        "FORM" => {
            let f = block
                .child("FORMULA")
                .and_then(|fm| fm.word(0))
                .or_else(|| block.after_keyword("FORMULA", 1))
                .unwrap_or("")
                .to_string();
            Conv::NonLinear(if f.is_empty() {
                "FORM formula conversion; imported pre-scaled".to_string()
            } else {
                format!("FORM formula conversion \"{f}\"; imported pre-scaled")
            })
        }
        "" => {
            Conv::NonLinear("missing COMPU_METHOD conversion type; imported pre-scaled".to_string())
        }
        other => Conv::NonLinear(format!(
            "unrecognized conversion type {other}; imported pre-scaled"
        )),
    };

    CompuMethod { name, unit, conv }
}

/// `LINEAR`: `COEFFS_LINEAR a b` → `physical = a*raw + b`.
fn linear_coeffs(block: &Block) -> Conv {
    let p = match block.keyword_pos("COEFFS_LINEAR") {
        Some(p) => p,
        None => return Conv::NonLinear("LINEAR without COEFFS_LINEAR; imported pre-scaled".into()),
    };
    let a = block.word(p + 1).and_then(parse_f64);
    let b = block.word(p + 2).and_then(parse_f64);
    match (a, b) {
        (Some(a), Some(b)) => Conv::Linear {
            factor: a,
            offset: b,
        },
        _ => Conv::NonLinear("LINEAR with unreadable COEFFS_LINEAR; imported pre-scaled".into()),
    }
}

/// `RAT_FUNC`: `COEFFS a b c d e f`, where the internal (raw) value is
/// `(a·p² + b·p + c)/(d·p² + e·p + f)` for physical `p`. Affine only when
/// `a = d = e = 0` and `b ≠ 0`, giving `physical = (f/b)·raw − c/b`.
fn rat_func(block: &Block) -> Conv {
    let p = match block.keyword_pos("COEFFS") {
        Some(p) => p,
        None => return Conv::NonLinear("RAT_FUNC without COEFFS; imported pre-scaled".into()),
    };
    let c: Vec<f64> = (1..=6)
        .map(|k| block.word(p + k).and_then(parse_f64))
        .collect::<Option<Vec<_>>>()
        .unwrap_or_default();
    if c.len() != 6 {
        return Conv::NonLinear("RAT_FUNC with unreadable COEFFS; imported pre-scaled".into());
    }
    let (a, b, cc, d, e, f) = (c[0], c[1], c[2], c[3], c[4], c[5]);
    let is_zero = |x: f64| x.abs() <= f64::EPSILON;
    if is_zero(a) && is_zero(d) && is_zero(e) && !is_zero(b) {
        Conv::Linear {
            factor: f / b,
            offset: -cc / b,
        }
    } else {
        Conv::NonLinear(format!(
            "RAT_FUNC non-affine COEFFS [{a} {b} {cc} {d} {e} {f}]; imported pre-scaled"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::parse_tree;

    fn cm(src: &str) -> CompuMethod {
        let root = parse_tree(src);
        parse_compu_method(root.child("COMPU_METHOD").expect("COMPU_METHOD block"))
    }

    #[test]
    fn rat_func_affine_recovered() {
        // internal = 655.35*p / 10.24  =>  physical = raw * (10.24/655.35)
        let c = cm(r#"/begin COMPU_METHOD ACK_KD "d" RAT_FUNC "%8.4" "(mg/Hub)"
                        COEFFS 0 655.350000 0.000000 0 0.000000 10.240000
                      /end COMPU_METHOD"#);
        match c.conv {
            Conv::Linear { factor, offset } => {
                assert!((factor - 10.24 / 655.35).abs() < 1e-12);
                assert!(offset.abs() < 1e-12);
            }
            other => panic!("expected linear, got {other:?}"),
        }
        assert_eq!(c.unit, "(mg/Hub)");
    }

    #[test]
    fn rat_func_identity_like() {
        let c = cm(
            r#"/begin COMPU_METHOD CNV_0 "" RAT_FUNC "%6.3" "" COEFFS 0 1 0 0 0 1 /end COMPU_METHOD"#,
        );
        assert_eq!(
            c.conv,
            Conv::Linear {
                factor: 1.0,
                offset: 0.0
            }
        );
    }

    #[test]
    fn rat_func_with_offset() {
        // internal = (2*p + 100)/4 => physical = raw*(4/2) - 100/2 = 2*raw - 50
        let c = cm(
            r#"/begin COMPU_METHOD X "" RAT_FUNC "%.1" "C" COEFFS 0 2 100 0 0 4 /end COMPU_METHOD"#,
        );
        match c.conv {
            Conv::Linear { factor, offset } => {
                assert!((factor - 2.0).abs() < 1e-12);
                assert!((offset + 50.0).abs() < 1e-12);
            }
            other => panic!("expected linear, got {other:?}"),
        }
    }

    #[test]
    fn rat_func_nonlinear_flagged() {
        // d != 0 => genuinely non-linear (reciprocal-ish): must be flagged.
        let c = cm(
            r#"/begin COMPU_METHOD X "" RAT_FUNC "%.1" "" COEFFS 0 0 1638 0 1 0 /end COMPU_METHOD"#,
        );
        assert!(matches!(c.conv, Conv::NonLinear(_)));
    }

    #[test]
    fn linear_coeffs_parsed() {
        let c = cm(
            r#"/begin COMPU_METHOD L "" LINEAR "%.3" "kPa" COEFFS_LINEAR 0.75 -20 /end COMPU_METHOD"#,
        );
        assert_eq!(
            c.conv,
            Conv::Linear {
                factor: 0.75,
                offset: -20.0
            }
        );
    }

    #[test]
    fn identity_is_unit_scaling() {
        let c = cm(r#"/begin COMPU_METHOD I "" IDENTITY "%.0" "" /end COMPU_METHOD"#);
        assert_eq!(
            c.conv,
            Conv::Linear {
                factor: 1.0,
                offset: 0.0
            }
        );
    }

    #[test]
    fn tab_intp_is_flagged_not_guessed() {
        let c = cm(
            r#"/begin COMPU_METHOD T "" TAB_INTP "%.2" "V" COMPU_TAB_REF TBL_1 /end COMPU_METHOD"#,
        );
        match c.conv {
            Conv::NonLinear(ref s) => assert!(s.contains("TAB_INTP")),
            other => panic!("TAB_INTP must be non-linear, got {other:?}"),
        }
        // And it must map to the identity scaling, so values import pre-scaled.
        assert_eq!(c.conv.to_scaling().0, Scaling::default());
    }
}
