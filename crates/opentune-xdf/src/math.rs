//! Linear scaling extraction from TunerPro `<MATH equation="...">` strings.
//!
//! OpenTune's [`Scaling`](opentune_core::ecudef::Scaling) is strictly linear
//! (`physical = raw * factor + offset`). TunerPro equations are free-form
//! expressions in the variable `X`, e.g. `X*0.01+0`, `0.351567 * X + 0.0`,
//! `(45*X)/127-20`, `X/10`. Rather than pattern-match a fixed set of textual
//! shapes, we evaluate the expression at several sample points and, if it turns
//! out to be an affine function of `X`, recover its slope and intercept
//! exactly. Any non-linear equation (`X*X`, `X^2`, a lookup, an unknown
//! identifier or function) fails the linearity check and is reported as
//! unsupported so the caller can import pre-scaled with a note instead of
//! guessing.

/// A tiny recursive-descent evaluator for the four arithmetic operators,
/// parentheses, unary minus, decimal/hex numeric literals and the single
/// variable `X` (case-insensitive). Returns `None` on any token it does not
/// understand (an unknown identifier, a function call, bad syntax), which
/// is exactly what we want: such equations are treated as non-linear.
/// Maximum parenthesis / unary-operator nesting the evaluator will descend. The
/// evaluator is recursive, so an equation such as `((((…X…))))` with very deep
/// nesting would otherwise overflow the stack. Real scaling equations nest only
/// a few levels; anything past this is treated as unparsable (non-linear).
const MAX_EVAL_DEPTH: usize = 256;

struct Eval<'a> {
    bytes: &'a [u8],
    pos: usize,
    x: f64,
    depth: usize,
}

impl<'a> Eval<'a> {
    fn new(src: &'a str, x: f64) -> Self {
        Eval {
            bytes: src.as_bytes(),
            pos: 0,
            x,
            depth: 0,
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && (self.bytes[self.pos] as char).is_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.bytes.get(self.pos).copied()
    }

    fn expr(&mut self) -> Option<f64> {
        let mut v = self.term()?;
        loop {
            match self.peek() {
                Some(b'+') => {
                    self.pos += 1;
                    v += self.term()?;
                }
                Some(b'-') => {
                    self.pos += 1;
                    v -= self.term()?;
                }
                _ => return Some(v),
            }
        }
    }

    fn term(&mut self) -> Option<f64> {
        let mut v = self.power()?;
        loop {
            match self.peek() {
                Some(b'*') => {
                    self.pos += 1;
                    v *= self.power()?;
                }
                Some(b'/') => {
                    self.pos += 1;
                    v /= self.power()?;
                }
                _ => return Some(v),
            }
        }
    }

    /// `a ^ b` (TunerPro exponent, right-associative, binds tighter than
    /// `*`/`/`). Common in fixed-point scalings such as `X/2^14`; a power of
    /// `X` itself (`X^2`) evaluates fine and is then rejected by the
    /// linearity check, so this never lets a non-linear equation through.
    fn power(&mut self) -> Option<f64> {
        let base = self.factor()?;
        if self.peek() == Some(b'^') {
            self.pos += 1;
            self.depth += 1;
            if self.depth > MAX_EVAL_DEPTH {
                self.depth -= 1;
                return None;
            }
            let exp = self.power();
            self.depth -= 1;
            return Some(base.powf(exp?));
        }
        Some(base)
    }

    fn factor(&mut self) -> Option<f64> {
        // Bound recursion depth; `factor` is the only recursive entry point
        // (unary +/- and parenthesised sub-expressions), so guarding it here caps
        // the whole descent.
        self.depth += 1;
        if self.depth > MAX_EVAL_DEPTH {
            self.depth -= 1;
            return None;
        }
        let result = self.factor_inner();
        self.depth -= 1;
        result
    }

    fn factor_inner(&mut self) -> Option<f64> {
        match self.peek()? {
            b'-' => {
                self.pos += 1;
                Some(-self.factor()?)
            }
            b'+' => {
                self.pos += 1;
                self.factor()
            }
            b'(' => {
                self.pos += 1;
                let v = self.expr()?;
                if self.peek()? != b')' {
                    return None;
                }
                self.pos += 1;
                Some(v)
            }
            c if c.is_ascii_digit() || c == b'.' => self.number(),
            c if c == b'x' || c == b'X' => {
                self.pos += 1;
                // Reject multi-char identifiers (e.g. a function name) so we do
                // not silently accept `exp(...)` as `X` times something.
                if let Some(n) = self.bytes.get(self.pos) {
                    let n = *n;
                    if n.is_ascii_alphanumeric() || n == b'_' {
                        return None;
                    }
                }
                Some(self.x)
            }
            _ => None,
        }
    }

    fn number(&mut self) -> Option<f64> {
        let start = self.pos;
        // Hex literal.
        if self.bytes[self.pos] == b'0'
            && self
                .bytes
                .get(self.pos + 1)
                .is_some_and(|c| *c == b'x' || *c == b'X')
        {
            self.pos += 2;
            let hs = self.pos;
            while self.pos < self.bytes.len() && (self.bytes[self.pos] as char).is_ascii_hexdigit()
            {
                self.pos += 1;
            }
            let s = std::str::from_utf8(&self.bytes[hs..self.pos]).ok()?;
            return u64::from_str_radix(s, 16).ok().map(|v| v as f64);
        }
        while self.pos < self.bytes.len() {
            let c = self.bytes[self.pos];
            if c.is_ascii_digit() || c == b'.' || c == b'e' || c == b'E' {
                // Allow an exponent sign only directly after e/E.
                self.pos += 1;
            } else if (c == b'+' || c == b'-')
                && self.pos > start
                && matches!(self.bytes[self.pos - 1], b'e' | b'E')
            {
                self.pos += 1;
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.bytes[start..self.pos]).ok()?;
        s.parse::<f64>().ok()
    }
}

/// Evaluate `eq` with the variable `X` bound to `x`. An empty equation, or the
/// TunerPro placeholder `(null)`, is treated as the identity `f(X) = X`.
/// Returns `None` if the expression contains anything the evaluator does not
/// understand or does not fully consume.
fn eval(eq: &str, x: f64) -> Option<f64> {
    let t = eq.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("(null)") {
        return Some(x);
    }
    let mut e = Eval::new(t, x);
    let v = e.expr()?;
    // Must have consumed the whole string.
    if e.peek().is_some() {
        return None;
    }
    if v.is_finite() {
        Some(v)
    } else {
        None
    }
}

/// Outcome of interpreting a MATH equation as a linear scaling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LinearFit {
    /// The equation is affine: `physical = raw * factor + offset`.
    Linear { factor: f64, offset: f64 },
    /// The equation parsed but is not a linear function of `X`
    /// (e.g. `X*X`, `X^2`, a square root). The caller should import the map
    /// pre-scaled (`factor = 1`, `offset = 0`) and flag it in `notes`.
    NonLinear,
    /// The equation could not be evaluated at all (unknown token / bad syntax).
    Unparsable,
}

/// Recover a linear `(factor, offset)` from a TunerPro MATH equation.
///
/// The equation is sampled at four points; if it behaves as an affine function
/// everywhere sampled, the exact slope and intercept are returned. Otherwise it
/// is classified as [`LinearFit::NonLinear`] or [`LinearFit::Unparsable`].
pub fn linear_from_equation(eq: &str) -> LinearFit {
    let samples = [0.0_f64, 1.0, 2.0, -3.0, 7.5];
    let mut ys = [0.0_f64; 5];
    for (i, &xs) in samples.iter().enumerate() {
        match eval(eq, xs) {
            Some(y) => ys[i] = y,
            None => return LinearFit::Unparsable,
        }
    }
    let offset = ys[0]; // f(0)
    let factor = ys[1] - ys[0]; // f(1) - f(0)
    let close = |a: f64, b: f64| (a - b).abs() <= 1e-9 + 1e-6 * a.abs().max(b.abs());
    for (i, &xs) in samples.iter().enumerate() {
        if !close(ys[i], factor * xs + offset) {
            return LinearFit::NonLinear;
        }
    }
    LinearFit::Linear { factor, offset }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lin(eq: &str) -> (f64, f64) {
        match linear_from_equation(eq) {
            LinearFit::Linear { factor, offset } => (factor, offset),
            other => panic!("expected linear for {eq:?}, got {other:?}"),
        }
    }

    #[test]
    fn simple_linear_forms() {
        let (f, o) = lin("X*0.01+0");
        assert!((f - 0.01).abs() < 1e-12 && o.abs() < 1e-12);
        let (f, o) = lin("0.351567 * X + 0.000000");
        assert!((f - 0.351567).abs() < 1e-9 && o.abs() < 1e-9);
        let (f, o) = lin("45*X+10");
        assert!((f - 45.0).abs() < 1e-9 && (o - 10.0).abs() < 1e-9);
    }

    #[test]
    fn division_and_parens() {
        let (f, o) = lin("(45*X)/127-20");
        assert!((f - 45.0 / 127.0).abs() < 1e-9);
        assert!((o + 20.0).abs() < 1e-9);
        let (f, o) = lin("X/10");
        assert!((f - 0.1).abs() < 1e-12 && o.abs() < 1e-12);
    }

    #[test]
    fn identity_and_bare_variable() {
        assert_eq!(lin("X"), (1.0, 0.0));
        assert_eq!(lin(""), (1.0, 0.0));
        assert_eq!(lin("(null)"), (1.0, 0.0));
    }

    #[test]
    fn hex_constants() {
        let (f, o) = lin("X*0x10");
        assert!((f - 16.0).abs() < 1e-9 && o.abs() < 1e-9);
    }

    #[test]
    fn nonlinear_is_flagged() {
        assert_eq!(linear_from_equation("X*X"), LinearFit::NonLinear);
        assert_eq!(linear_from_equation("X*X+2*X"), LinearFit::NonLinear);
    }

    #[test]
    fn unknown_tokens_are_unparsable() {
        assert_eq!(linear_from_equation("sqrt(X)"), LinearFit::Unparsable);
        assert_eq!(linear_from_equation("X + THROTTLE"), LinearFit::Unparsable);
    }

    #[test]
    fn power_operator() {
        // A power of X is evaluated and rejected as non-linear.
        assert_eq!(linear_from_equation("X^2"), LinearFit::NonLinear);
        // A constant power is just a constant: fixed-point scalings are linear.
        let (f, o) = lin("x/2^14");
        assert!((f - 1.0 / 16384.0).abs() < 1e-15 && o == 0.0);
        let (f, _) = lin("X*2^-3");
        assert!((f - 0.125).abs() < 1e-15);
        // Right-associative: 2^3^2 = 2^9.
        let (f, _) = lin("X*2^3^2");
        assert!((f - 512.0).abs() < 1e-9);
        // Deep exponent chains hit the depth cap instead of the stack.
        let deep = format!("X*{}2", "2^".repeat(10_000));
        let _ = linear_from_equation(&deep);
    }

    #[test]
    fn deeply_nested_parens_do_not_overflow() {
        // A pathological equation with far more nesting than the depth cap must
        // be rejected as unparsable, never overflow the stack.
        let deep = format!("{}X{}", "(".repeat(100_000), ")".repeat(100_000));
        assert_eq!(linear_from_equation(&deep), LinearFit::Unparsable);
        // Deeply nested unary minus too.
        let minus = format!("{}X", "-".repeat(100_000));
        assert_eq!(linear_from_equation(&minus), LinearFit::Unparsable);
    }

    #[test]
    fn unbalanced_and_garbage_equations_are_clean() {
        // These must each classify cleanly without panicking.
        for eq in ["(((", ")))", "X*/", "/X", "((X)", "1e", "0x", "*", ""] {
            let _ = linear_from_equation(eq);
        }
    }

    #[test]
    fn modestly_nested_is_still_linear() {
        // A realistic level of nesting still works.
        let (f, o) = lin("((((X*2))))+1");
        assert!((f - 2.0).abs() < 1e-9 && (o - 1.0).abs() < 1e-9);
    }
}
