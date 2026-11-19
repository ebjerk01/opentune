//! A small, safe evaluator for logger conversion expressions.
//!
//! RomRaider `<conversion expr>` and EcuFlash `expr` strings are arithmetic
//! over the raw value and, for calculated parameters, over other parameters.
//! This module parses them into a tree once and evaluates the tree; nothing is
//! ever handed to an interpreter. The grammar is the common subset those
//! definitions use:
//!
//! * numbers: `12`, `0.25`, `.004`, `1e-3`, `0x1F`;
//! * the raw value: `x` / `X` (and the EcuFlash `[value]`, normalized to `X`
//!   before parsing);
//! * other parameters: a bare id (`P8`, `E12`) is that parameter's value in
//!   its primary (first) conversion; `[P9:mph]` is its value in the conversion
//!   whose units are `mph` (units may contain `/`, `µ`, spaces);
//!   `GetLogParam("Engine Speed")` is the EcuFlash spelling of a bare id;
//! * `+ - * /`, `^` (power, right-associative), unary `-`/`+`, parentheses;
//! * functions `abs sqrt exp ln log10 floor ceil round min max pow`.
//!
//! Anything else (comparisons, conditionals, unknown functions, strings
//! outside `GetLogParam`) is a parse error naming the construct, so the
//! importer can report it instead of guessing. Evaluation fails (rather than
//! returning a wrong number) on a missing input, a division by zero or any
//! non-finite result.

use std::fmt;

/// Deepest nesting accepted (parentheses, unary operators, call arguments).
const MAX_DEPTH: usize = 64;

/// A reference to another parameter inside an expression.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParamRef {
    /// The referenced parameter id.
    pub id: String,
    /// Units of the conversion to read it in (`[P9:mph]`); `None` means its
    /// primary conversion.
    pub units: Option<String>,
}

impl fmt::Display for ParamRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.units {
            Some(u) => write!(f, "[{}:{u}]", self.id),
            None => write!(f, "{}", self.id),
        }
    }
}

/// Why an expression could not be parsed or evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExprError(pub String);

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ExprError {}

fn err<T>(msg: impl Into<String>) -> Result<T, ExprError> {
    Err(ExprError(msg.into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Func {
    Abs,
    Sqrt,
    Exp,
    Ln,
    Log10,
    Floor,
    Ceil,
    Round,
    Min,
    Max,
    Pow,
}

impl Func {
    fn by_name(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "abs" => Func::Abs,
            "sqrt" => Func::Sqrt,
            "exp" => Func::Exp,
            "ln" => Func::Ln,
            "log10" => Func::Log10,
            "floor" => Func::Floor,
            "ceil" => Func::Ceil,
            "round" => Func::Round,
            "min" => Func::Min,
            "max" => Func::Max,
            "pow" => Func::Pow,
            _ => return None,
        })
    }

    fn arity(self) -> usize {
        match self {
            Func::Min | Func::Max | Func::Pow => 2,
            _ => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Num(f64),
    X,
    Ref(ParamRef),
    Neg(Box<Node>),
    Bin(u8, Box<Node>, Box<Node>),
    Call(Func, Vec<Node>),
}

/// A parsed conversion expression.
#[derive(Debug, Clone, PartialEq)]
pub struct LogExpr {
    root: Node,
}

struct Parser<'a> {
    s: &'a str,
    b: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.b.len() && self.b[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.b.get(self.pos).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn enter(&mut self) -> Result<(), ExprError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return err(format!("nested deeper than {MAX_DEPTH} levels"));
        }
        Ok(())
    }

    fn expr(&mut self) -> Result<Node, ExprError> {
        self.enter()?;
        let mut lhs = self.term()?;
        while let Some(op @ (b'+' | b'-')) = self.peek() {
            self.pos += 1;
            let rhs = self.term()?;
            lhs = Node::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        self.depth -= 1;
        Ok(lhs)
    }

    fn term(&mut self) -> Result<Node, ExprError> {
        let mut lhs = self.unary()?;
        while let Some(op @ (b'*' | b'/')) = self.peek() {
            self.pos += 1;
            let rhs = self.unary()?;
            lhs = Node::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<Node, ExprError> {
        self.enter()?;
        let node = if self.eat(b'-') {
            Node::Neg(Box::new(self.unary()?))
        } else if self.eat(b'+') {
            self.unary()?
        } else {
            self.power()?
        };
        self.depth -= 1;
        Ok(node)
    }

    fn power(&mut self) -> Result<Node, ExprError> {
        let base = self.atom()?;
        if self.eat(b'^') {
            // Right-associative, and binds tighter than a unary minus on its
            // left: -2^2 = -(2^2), 2^-1 = 0.5.
            let exp = self.unary()?;
            return Ok(Node::Bin(b'^', Box::new(base), Box::new(exp)));
        }
        Ok(base)
    }

    fn atom(&mut self) -> Result<Node, ExprError> {
        match self.peek() {
            None => err("unexpected end of expression"),
            Some(b'(') => {
                self.pos += 1;
                let inner = self.expr()?;
                if !self.eat(b')') {
                    return err(format!("missing ')' at offset {}", self.pos));
                }
                Ok(inner)
            }
            Some(b'[') => self.bracket_ref(),
            Some(c) if c.is_ascii_digit() || c == b'.' => self.number(),
            Some(c) if c.is_ascii_alphabetic() || c == b'_' => self.ident(),
            Some(_) => {
                let ch = self.s[self.pos..].chars().next().unwrap_or('?');
                err(format!("unexpected '{ch}' at offset {}", self.pos))
            }
        }
    }

    fn number(&mut self) -> Result<Node, ExprError> {
        let start = self.pos;
        if self.b[self.pos] == b'0' && matches!(self.b.get(self.pos + 1), Some(b'x' | b'X')) {
            self.pos += 2;
            let hs = self.pos;
            while self.pos < self.b.len() && self.b[self.pos].is_ascii_hexdigit() {
                self.pos += 1;
            }
            return u64::from_str_radix(&self.s[hs..self.pos], 16)
                .map(|v| Node::Num(v as f64))
                .or_else(|_| err(format!("bad hex number at offset {start}")));
        }
        while self.pos < self.b.len() {
            let c = self.b[self.pos];
            let exp_sign = (c == b'+' || c == b'-')
                && self.pos > start
                && matches!(self.b[self.pos - 1], b'e' | b'E');
            if c.is_ascii_digit() || c == b'.' || c == b'e' || c == b'E' || exp_sign {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = &self.s[start..self.pos];
        text.parse::<f64>()
            .map(Node::Num)
            .or_else(|_| err(format!("bad number '{text}'")))
    }

    fn ident(&mut self) -> Result<Node, ExprError> {
        let start = self.pos;
        while self.pos < self.b.len()
            && (self.b[self.pos].is_ascii_alphanumeric() || self.b[self.pos] == b'_')
        {
            self.pos += 1;
        }
        let name = &self.s[start..self.pos];
        if name.eq_ignore_ascii_case("x") {
            return Ok(Node::X);
        }
        if self.peek() != Some(b'(') {
            return Ok(Node::Ref(ParamRef {
                id: name.to_string(),
                units: None,
            }));
        }
        self.pos += 1;
        if name == "GetLogParam" {
            return self.get_log_param();
        }
        let Some(func) = Func::by_name(name) else {
            return err(format!("unsupported function '{name}'"));
        };
        let mut args = Vec::new();
        if !self.eat(b')') {
            loop {
                self.enter()?;
                args.push(self.expr()?);
                self.depth -= 1;
                if self.eat(b',') {
                    continue;
                }
                if self.eat(b')') {
                    break;
                }
                return err(format!("expected ',' or ')' in {name}(…)"));
            }
        }
        if args.len() != func.arity() {
            return err(format!(
                "{name}() takes {} argument(s), got {}",
                func.arity(),
                args.len()
            ));
        }
        Ok(Node::Call(func, args))
    }

    /// `GetLogParam("name")` after the `(`.
    fn get_log_param(&mut self) -> Result<Node, ExprError> {
        if self.peek() != Some(b'"') {
            return err("GetLogParam expects a quoted parameter name");
        }
        self.pos += 1;
        let start = self.pos;
        let Some(len) = self.s[start..].find('"') else {
            return err("unterminated string in GetLogParam");
        };
        let id = self.s[start..start + len].to_string();
        self.pos = start + len + 1;
        if !self.eat(b')') || id.is_empty() {
            return err("GetLogParam expects one quoted parameter name");
        }
        Ok(Node::Ref(ParamRef { id, units: None }))
    }

    /// `[id]` or `[id:units]`.
    fn bracket_ref(&mut self) -> Result<Node, ExprError> {
        let start = self.pos + 1;
        let Some(len) = self.s[start..].find(']') else {
            return err("unterminated '[' reference");
        };
        let inner = &self.s[start..start + len];
        self.pos = start + len + 1;
        let (id, units) = match inner.split_once(':') {
            Some((id, units)) => (id.trim(), Some(units.trim().to_string())),
            None => (inner.trim(), None),
        };
        if id.is_empty() || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
            return err(format!("bad parameter reference '[{inner}]'"));
        }
        if id.eq_ignore_ascii_case("value") && units.is_none() {
            return Ok(Node::X);
        }
        Ok(Node::Ref(ParamRef {
            id: id.to_string(),
            units: units.filter(|u| !u.is_empty()),
        }))
    }
}

impl LogExpr {
    /// Parse `s`. An empty expression is the identity `x`.
    pub fn parse(s: &str) -> Result<Self, ExprError> {
        if s.trim().is_empty() {
            return Ok(Self { root: Node::X });
        }
        let mut p = Parser {
            s,
            b: s.as_bytes(),
            pos: 0,
            depth: 0,
        };
        let root = p.expr()?;
        if let Some(c) = p.peek() {
            return err(format!("unexpected '{}' at offset {}", c as char, p.pos));
        }
        Ok(Self { root })
    }

    /// The parameters referenced, each once, in first-use order.
    pub fn refs(&self) -> Vec<ParamRef> {
        fn walk(n: &Node, out: &mut Vec<ParamRef>) {
            match n {
                Node::Ref(r) => {
                    if !out.contains(r) {
                        out.push(r.clone());
                    }
                }
                Node::Neg(a) => walk(a, out),
                Node::Bin(_, a, b) => {
                    walk(a, out);
                    walk(b, out);
                }
                Node::Call(_, args) => args.iter().for_each(|a| walk(a, out)),
                Node::Num(_) | Node::X => {}
            }
        }
        let mut out = Vec::new();
        walk(&self.root, &mut out);
        out
    }

    /// The referenced parameter ids, each once, in first-use order.
    pub fn ref_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for r in self.refs() {
            if !ids.contains(&r.id) {
                ids.push(r.id);
            }
        }
        ids
    }

    /// Whether the raw value `x` appears.
    pub fn uses_x(&self) -> bool {
        fn walk(n: &Node) -> bool {
            match n {
                Node::X => true,
                Node::Neg(a) => walk(a),
                Node::Bin(_, a, b) => walk(a) || walk(b),
                Node::Call(_, args) => args.iter().any(walk),
                Node::Num(_) | Node::Ref(_) => false,
            }
        }
        walk(&self.root)
    }

    /// Evaluate with `x` bound to the raw value (if any) and parameter
    /// references resolved by `lookup`. Fails on an unbound `x`, an
    /// unresolved reference, division by zero or a non-finite result.
    pub fn eval(
        &self,
        x: Option<f64>,
        lookup: &mut dyn FnMut(&ParamRef) -> Result<f64, String>,
    ) -> Result<f64, ExprError> {
        let v = eval(&self.root, x, lookup)?;
        if v.is_finite() {
            Ok(v)
        } else {
            err("result is not a finite number")
        }
    }

    /// Evaluate an expression with no parameter references.
    pub fn eval_x(&self, x: f64) -> Result<f64, ExprError> {
        self.eval(Some(x), &mut |r| {
            Err(format!(
                "reference to {r} needs the other parameter's value"
            ))
        })
    }
}

fn eval(
    n: &Node,
    x: Option<f64>,
    lookup: &mut dyn FnMut(&ParamRef) -> Result<f64, String>,
) -> Result<f64, ExprError> {
    Ok(match n {
        Node::Num(v) => *v,
        Node::X => match x {
            Some(v) => v,
            None => return err("uses x but the parameter has no raw value"),
        },
        Node::Ref(r) => lookup(r).map_err(ExprError)?,
        Node::Neg(a) => -eval(a, x, lookup)?,
        Node::Bin(op, a, b) => {
            let a = eval(a, x, lookup)?;
            let b = eval(b, x, lookup)?;
            match op {
                b'+' => a + b,
                b'-' => a - b,
                b'*' => a * b,
                b'/' => {
                    if b == 0.0 {
                        return err("division by zero");
                    }
                    a / b
                }
                _ => a.powf(b),
            }
        }
        Node::Call(f, args) => {
            let mut v = Vec::with_capacity(args.len());
            for a in args {
                v.push(eval(a, x, lookup)?);
            }
            match f {
                Func::Abs => v[0].abs(),
                Func::Sqrt => v[0].sqrt(),
                Func::Exp => v[0].exp(),
                Func::Ln => v[0].ln(),
                Func::Log10 => v[0].log10(),
                Func::Floor => v[0].floor(),
                Func::Ceil => v[0].ceil(),
                Func::Round => v[0].round(),
                Func::Min => v[0].min(v[1]),
                Func::Max => v[0].max(v[1]),
                Func::Pow => v[0].powf(v[1]),
            }
        }
    })
    .and_then(|v: f64| {
        if v.is_nan() {
            err("result is not a number")
        } else {
            Ok(v)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(s: &str, x: f64) -> f64 {
        LogExpr::parse(s).unwrap().eval_x(x).unwrap()
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(ev("x-40", 130.0), 90.0);
        assert_eq!(ev("32+9*(x-40)/5", 140.0), 212.0);
        assert_eq!(ev("(x*.3515625)-45", 0.0), -45.0);
        assert_eq!(ev("14.7/(1+x)", 0.0), 14.7);
        assert_eq!(ev("1/x", 4.0), 0.25);
        assert_eq!(ev("-2^2", 0.0), -4.0);
        assert_eq!(ev("2^-1", 0.0), 0.5);
        assert_eq!(ev("2^3^2", 0.0), 512.0);
        assert_eq!(ev("X*0x10", 2.0), 32.0);
        assert_eq!(ev("x*1e-3", 2000.0), 2.0);
        assert_eq!(ev("max(x, 10) + abs(-1)", 3.0), 11.0);
        assert_eq!(ev("", 7.0), 7.0);
        assert_eq!(ev("[value]*2", 7.0), 14.0);
    }

    #[test]
    fn references_are_collected() {
        let e = LogExpr::parse("([P9:km/h]/3600)/((P12/[P58:AFR])/761)").unwrap();
        let refs: Vec<String> = e.refs().iter().map(ToString::to_string).collect();
        assert_eq!(refs, vec!["[P9:km/h]", "P12", "[P58:AFR]"]);
        assert!(!e.uses_x());
        let e = LogExpr::parse("[P229:µs]/(2777.77/(P8/60))").unwrap();
        assert_eq!(e.ref_ids(), vec!["P229".to_string(), "P8".to_string()]);
        let e = LogExpr::parse("GetLogParam(\"Engine Speed\")*(X/1200)").unwrap();
        assert_eq!(e.ref_ids(), vec!["Engine Speed".to_string()]);
        assert!(e.uses_x());
    }

    #[test]
    fn evaluates_references_through_lookup() {
        let e = LogExpr::parse("(P8*P21)/1200").unwrap();
        let v = e
            .eval(None, &mut |r| match r.id.as_str() {
                "P8" => Ok(3000.0),
                "P21" => Ok(4.0),
                _ => Err("?".into()),
            })
            .unwrap();
        assert_eq!(v, 10.0);
        let missing = e.eval(None, &mut |_| Err("no value this round".into()));
        assert_eq!(missing.unwrap_err().0, "no value this round");
    }

    #[test]
    fn refuses_rather_than_guesses() {
        for bad in [
            "x > 1",
            "if(x,1,2)",
            "foo(x)",
            "x +",
            "(x",
            "x)",
            "\"str\"",
            "[P1",
            "[bad id:u]",
            "min(x)",
        ] {
            assert!(LogExpr::parse(bad).is_err(), "{bad} should not parse");
        }
        let deep = format!("{}x{}", "(".repeat(200), ")".repeat(200));
        assert!(LogExpr::parse(&deep).is_err());
        assert!(LogExpr::parse("1/x").unwrap().eval_x(0.0).is_err());
        assert!(LogExpr::parse("sqrt(x)").unwrap().eval_x(-1.0).is_err());
        assert!(LogExpr::parse("x")
            .unwrap()
            .eval(None, &mut |_| Ok(0.0))
            .is_err());
    }
}
