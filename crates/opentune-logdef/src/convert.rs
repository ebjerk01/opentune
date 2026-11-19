//! Equation normalization and linear fitting for logger conversions.
//!
//! RomRaider `<conversion expr>` uses the lowercase variable `x`; the EcuFlash
//! / FastECU format writes the raw value as the token `[value]`. Both are
//! normalized to the single uppercase variable `X` and then handed to
//! [`opentune_xdf::math::linear_from_equation`] — the exact fitter the XDF and
//! EcuFlash importers use — so a linear conversion imports as `factor`/`offset`
//! and anything non-linear is flagged rather than silently approximated.

use crate::expr::LogExpr;
use crate::model::ConversionFit;
use opentune_core::ecudef::DataType;
use opentune_xdf::math::{linear_from_equation, LinearFit};

/// Rewrite a conversion equation so the raw value is the uppercase variable
/// `X`: the EcuFlash/FastECU `[value]` placeholder (any case, surrounding
/// whitespace tolerated) becomes `X`, and a standalone RomRaider `x` token is
/// upper-cased so the stored `expr` is canonical. An `x` inside a word (`max`,
/// a parameter id), a hex literal (`0x1F`), a quoted `GetLogParam` name or a
/// `[id:units]` reference is left alone.
pub fn normalize_equation(expr: &str) -> String {
    retoken_x(expr, b'x', 'X', true)
}

/// The inverse of [`normalize_equation`] for export: a standalone `X` token
/// becomes RomRaider's lowercase `x`.
pub fn denormalize_equation(expr: &str) -> String {
    retoken_x(expr, b'X', 'x', false)
}

fn retoken_x(expr: &str, from: u8, to: char, value_placeholder: bool) -> String {
    let mut out = String::with_capacity(expr.len());
    let bytes = expr.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'[' {
            // Copy a `[...]` token whole; `[value]` becomes `X` when normalizing.
            if let Some(close) = expr[i..].find(']') {
                let inner = expr[i + 1..i + close].trim();
                if value_placeholder && inner.eq_ignore_ascii_case("value") {
                    out.push('X');
                } else {
                    out.push_str(&expr[i..=i + close]);
                }
                i += close + 1;
                continue;
            }
        } else if c == b'"' {
            if let Some(close) = expr[i + 1..].find('"') {
                out.push_str(&expr[i..i + close + 2]);
                i += close + 2;
                continue;
            }
        } else if c == from
            && (i == 0 || !word(bytes[i - 1]))
            && !bytes.get(i + 1).is_some_and(|&n| word(n))
        {
            out.push(to);
            i += 1;
            continue;
        }
        // Copy one UTF-8 scalar so multi-byte unit text inside the string
        // (rare, but possible) is preserved.
        let ch_len = utf8_len(c);
        out.push_str(&expr[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Byte length of the UTF-8 sequence that starts with `b`.
fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// Normalize `expr` and classify it, returning the normalized expression
/// alongside the fit so the caller can store the canonical `X`-form.
///
/// An affine equation (checked with the XDF fitter) is
/// [`ConversionFit::Linear`]; anything else the safe grammar of
/// [`LogExpr`] accepts is [`ConversionFit::Expression`] (evaluated per
/// sample); the rest is [`ConversionFit::NonLinear`] with the parse error.
pub fn fit_equation(expr: &str) -> (String, ConversionFit) {
    let normalized = normalize_equation(expr);
    if let LinearFit::Linear { factor, offset } = linear_from_equation(&normalized) {
        return (normalized, ConversionFit::Linear { factor, offset });
    }
    let fit = match LogExpr::parse(&normalized) {
        Ok(e) => ConversionFit::Expression { refs: e.ref_ids() },
        Err(e) => ConversionFit::NonLinear {
            reason: format!("unsupported expression: {e}"),
        },
    };
    (normalized, fit)
}

/// Map a RomRaider/EcuFlash `storagetype` string to an OpenTune [`DataType`].
///
/// Returns `None` for an empty or unrecognised type (the caller records a
/// warning rather than guessing a width).
pub fn storage_type(s: &str) -> Option<DataType> {
    match s.trim().to_ascii_lowercase().as_str() {
        "uint8" => Some(DataType::U8),
        "int8" => Some(DataType::S8),
        "uint16" => Some(DataType::U16),
        "int16" => Some(DataType::S16),
        "uint32" => Some(DataType::U32),
        "int32" => Some(DataType::S32),
        "float" | "float32" => Some(DataType::F32),
        _ => None,
    }
}

/// Parse a hex address written as `0x1234`, `#1234` or bare `1234` into a
/// `u32`. Returns `None` if the remaining text is not valid hex.
pub fn parse_address(raw: &str) -> Option<u32> {
    let t = raw.trim();
    let hex = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .or_else(|| t.strip_prefix('#'))
        .unwrap_or(t);
    if hex.is_empty() {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_value_placeholder() {
        assert_eq!(normalize_equation("[value]/4"), "X/4");
        assert_eq!(
            normalize_equation("([value]-128)*100/128"),
            "(X-128)*100/128"
        );
        assert_eq!(normalize_equation("[VALUE] * 2"), "X * 2");
        assert_eq!(normalize_equation("[ value ]+1"), "X+1");
    }

    #[test]
    fn normalizes_lowercase_x() {
        assert_eq!(normalize_equation("x-40"), "X-40");
        assert_eq!(normalize_equation("x*1000/256"), "X*1000/256");
    }

    #[test]
    fn linear_expr_fits() {
        let (expr, fit) = fit_equation("[value]/4");
        assert_eq!(expr, "X/4");
        match fit {
            ConversionFit::Linear { factor, offset } => {
                assert!((factor - 0.25).abs() < 1e-12 && offset.abs() < 1e-12);
            }
            other => panic!("expected linear, got {other:?}"),
        }
    }

    #[test]
    fn offset_expr_fits() {
        let (_e, fit) = fit_equation("([value]-128)*100/128");
        match fit {
            ConversionFit::Linear { factor, offset } => {
                assert!((factor - 100.0 / 128.0).abs() < 1e-9);
                assert!((offset + 100.0).abs() < 1e-9);
            }
            other => panic!("expected linear, got {other:?}"),
        }
    }

    #[test]
    fn dependent_expr_is_an_expression_with_refs() {
        // References another parameter via GetLogParam -> evaluated per sample.
        let (_e, fit) = fit_equation("GetLogParam(\"Engine Speed\")*([value]/1200)");
        assert_eq!(
            fit,
            ConversionFit::Expression {
                refs: vec!["Engine Speed".into()]
            }
        );
        let (e, fit) = fit_equation("(P8*P21)/1200");
        assert_eq!(e, "(P8*P21)/1200");
        assert!(matches!(fit, ConversionFit::Expression { refs } if refs == ["P8", "P21"]));
    }

    #[test]
    fn non_affine_is_an_expression_and_garbage_is_refused() {
        for eq in ["[value]*[value]", "1/x", "14.7/(1+x)"] {
            let (_e, fit) = fit_equation(eq);
            assert_eq!(fit, ConversionFit::Expression { refs: vec![] }, "{eq}");
        }
        let (_e, fit) = fit_equation("x > 3");
        assert!(matches!(fit, ConversionFit::NonLinear { .. }));
    }

    #[test]
    fn x_tokens_only() {
        assert_eq!(normalize_equation("max(x,0x10)"), "max(X,0x10)");
        assert_eq!(normalize_equation("[P9:km/h]*x"), "[P9:km/h]*X");
        assert_eq!(
            normalize_equation("GetLogParam(\"x axis\")*x"),
            "GetLogParam(\"x axis\")*X"
        );
        assert_eq!(normalize_equation("Px+x"), "Px+X");
        assert_eq!(denormalize_equation("max(X,0X1)*X"), "max(x,0X1)*x");
    }

    #[test]
    fn storage_types_map() {
        assert_eq!(storage_type("uint16"), Some(DataType::U16));
        assert_eq!(storage_type("INT8"), Some(DataType::S8));
        assert_eq!(storage_type("float"), Some(DataType::F32));
        assert_eq!(storage_type("weird"), None);
    }

    #[test]
    fn addresses_parse() {
        assert_eq!(parse_address("0x000E"), Some(0x0e));
        assert_eq!(parse_address("#20118"), Some(0x2_0118));
        assert_eq!(parse_address("804cfc"), Some(0x80_4cfc));
        assert_eq!(parse_address("0x"), None);
        assert_eq!(parse_address("zz"), None);
    }
}
