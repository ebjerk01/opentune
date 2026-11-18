//! A small evaluator for the `{ ... }` expressions definitions use in output
//! channels and scales: arithmetic, comparison, logic, bit operators, the
//! ternary, parentheses and a few math functions. Anything else (TunerStudio's
//! table lookups, smoothing, string functions, ...) is reported as
//! [`ExprError::Unsupported`] rather than guessed.
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Error)]
pub enum ExprError {
    #[error("expression syntax: {0}")]
    Parse(String),
    #[error("unknown name '{0}'")]
    Unknown(String),
    #[error("unsupported in expressions: {0}")]
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Ident(String),
    Op(&'static str),
    LParen,
    RParen,
    LBrack,
    RBrack,
    Comma,
    Question,
    Colon,
}

const OPS: [&str; 22] = [
    "<<", ">>", "<=", ">=", "==", "!=", "&&", "||", "+", "-", "*", "/", "%", "<", ">", "&", "|",
    "^", "!", "~", "=", "$",
];

fn lex(s: &str) -> Result<Vec<Tok>, ExprError> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let start = i;
            if c == b'0' && matches!(b.get(i + 1), Some(b'x' | b'X')) {
                i += 2;
                while i < b.len() && b[i].is_ascii_hexdigit() {
                    i += 1;
                }
                let v = i64::from_str_radix(&s[start + 2..i], 16)
                    .map_err(|e| ExprError::Parse(e.to_string()))?;
                out.push(Tok::Num(v as f64));
                continue;
            }
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                let save = i;
                i += 1;
                if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
                    i += 1;
                }
                if i < b.len() && b[i].is_ascii_digit() {
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                } else {
                    i = save;
                }
            }
            let v: f64 = s[start..i]
                .parse()
                .map_err(|_| ExprError::Parse(format!("bad number '{}'", &s[start..i])))?;
            out.push(Tok::Num(v));
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.') {
                i += 1;
            }
            out.push(Tok::Ident(s[start..i].to_owned()));
            continue;
        }
        match c {
            b'(' => out.push(Tok::LParen),
            b')' => out.push(Tok::RParen),
            b'[' => out.push(Tok::LBrack),
            b']' => out.push(Tok::RBrack),
            b',' => out.push(Tok::Comma),
            b'?' => out.push(Tok::Question),
            b':' => out.push(Tok::Colon),
            b'"' => return Err(ExprError::Unsupported("string literals".into())),
            _ => {
                let op = OPS
                    .iter()
                    .find(|o| s[i..].starts_with(**o))
                    .ok_or_else(|| ExprError::Parse(format!("unexpected '{}'", c as char)))?;
                if *op == "=" || *op == "$" {
                    return Err(ExprError::Unsupported(format!("operator '{op}'")));
                }
                out.push(Tok::Op(op));
                i += op.len();
                continue;
            }
        }
        i += 1;
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
enum Ast {
    Num(f64),
    Var(String),
    Index(String, Box<Ast>),
    Unary(&'static str, Box<Ast>),
    Bin(&'static str, Box<Ast>, Box<Ast>),
    Cond(Box<Ast>, Box<Ast>, Box<Ast>),
    Call(String, Vec<Ast>),
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

fn prec(op: &str) -> Option<u8> {
    Some(match op {
        "||" => 1,
        "&&" => 2,
        "|" => 3,
        "^" => 4,
        "&" => 5,
        "==" | "!=" => 6,
        "<" | "<=" | ">" | ">=" => 7,
        "<<" | ">>" => 8,
        "+" | "-" => 9,
        "*" | "/" | "%" => 10,
        _ => return None,
    })
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn expect(&mut self, t: Tok) -> Result<(), ExprError> {
        match self.next() {
            Some(x) if x == t => Ok(()),
            other => Err(ExprError::Parse(format!("expected {t:?}, got {other:?}"))),
        }
    }

    fn expr(&mut self) -> Result<Ast, ExprError> {
        let c = self.binary(0)?;
        if self.peek() == Some(&Tok::Question) {
            self.pos += 1;
            let a = self.expr()?;
            self.expect(Tok::Colon)?;
            let b = self.expr()?;
            return Ok(Ast::Cond(Box::new(c), Box::new(a), Box::new(b)));
        }
        Ok(c)
    }

    fn binary(&mut self, min: u8) -> Result<Ast, ExprError> {
        let mut lhs = self.unary()?;
        while let Some(Tok::Op(o)) = self.peek() {
            let op = *o;
            let p = match prec(op) {
                Some(p) if p > min => p,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.binary(p)?;
            lhs = Ast::Bin(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<Ast, ExprError> {
        match self.peek() {
            Some(Tok::Op(o)) if matches!(*o, "-" | "+" | "!" | "~") => {
                let o = *o;
                self.pos += 1;
                Ok(Ast::Unary(o, Box::new(self.unary()?)))
            }
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> Result<Ast, ExprError> {
        match self.next() {
            Some(Tok::Num(v)) => Ok(Ast::Num(v)),
            Some(Tok::LParen) => {
                let e = self.expr()?;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            Some(Tok::Ident(name)) => match self.peek() {
                Some(Tok::LParen) => {
                    self.pos += 1;
                    let mut args = Vec::new();
                    if self.peek() != Some(&Tok::RParen) {
                        loop {
                            args.push(self.expr()?);
                            match self.next() {
                                Some(Tok::Comma) => continue,
                                Some(Tok::RParen) => break,
                                other => {
                                    return Err(ExprError::Parse(format!(
                                        "bad call argument list: {other:?}"
                                    )))
                                }
                            }
                        }
                    } else {
                        self.pos += 1;
                    }
                    Ok(Ast::Call(name, args))
                }
                Some(Tok::LBrack) => {
                    self.pos += 1;
                    let i = self.expr()?;
                    self.expect(Tok::RBrack)?;
                    Ok(Ast::Index(name, Box::new(i)))
                }
                _ => Ok(Ast::Var(name)),
            },
            other => Err(ExprError::Parse(format!("unexpected {other:?}"))),
        }
    }
}

/// A parsed expression.
#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    ast: Ast,
}

fn truthy(v: f64) -> bool {
    v != 0.0 && !v.is_nan()
}

fn b2f(b: bool) -> f64 {
    if b {
        1.0
    } else {
        0.0
    }
}

impl Expr {
    pub fn parse(s: &str) -> Result<Self, ExprError> {
        let toks = lex(s)?;
        if toks.is_empty() {
            return Err(ExprError::Parse("empty expression".into()));
        }
        let mut p = Parser { toks, pos: 0 };
        let ast = p.expr()?;
        if p.pos != p.toks.len() {
            return Err(ExprError::Parse(format!(
                "trailing input at token {}",
                p.pos
            )));
        }
        Ok(Self { ast })
    }

    /// Every variable name the expression reads.
    pub fn names(&self) -> Vec<String> {
        fn walk(a: &Ast, out: &mut Vec<String>) {
            match a {
                Ast::Num(_) => {}
                Ast::Var(n) => out.push(n.clone()),
                Ast::Index(n, i) => {
                    out.push(n.clone());
                    walk(i, out);
                }
                Ast::Unary(_, x) => walk(x, out),
                Ast::Bin(_, a, b) => {
                    walk(a, out);
                    walk(b, out);
                }
                Ast::Cond(c, a, b) => {
                    walk(c, out);
                    walk(a, out);
                    walk(b, out);
                }
                Ast::Call(_, args) => args.iter().for_each(|x| walk(x, out)),
            }
        }
        let mut v = Vec::new();
        walk(&self.ast, &mut v);
        v
    }

    /// Evaluate with `vars` resolving names (`name` or `name[i]`).
    pub fn eval(&self, vars: &dyn Fn(&str) -> Option<f64>) -> Result<f64, ExprError> {
        eval(&self.ast, vars)
    }
}

fn int(v: f64) -> i64 {
    v as i64
}

fn eval(a: &Ast, vars: &dyn Fn(&str) -> Option<f64>) -> Result<f64, ExprError> {
    Ok(match a {
        Ast::Num(v) => *v,
        Ast::Var(n) => vars(n).ok_or_else(|| ExprError::Unknown(n.clone()))?,
        Ast::Index(n, i) => {
            let i = eval(i, vars)? as i64;
            let key = format!("{n}[{i}]");
            vars(&key).ok_or(ExprError::Unknown(key))?
        }
        Ast::Unary(op, x) => {
            let v = eval(x, vars)?;
            match *op {
                "-" => -v,
                "+" => v,
                "!" => b2f(!truthy(v)),
                _ => !int(v) as f64,
            }
        }
        Ast::Bin(op, l, r) => {
            if *op == "&&" {
                return Ok(b2f(truthy(eval(l, vars)?) && truthy(eval(r, vars)?)));
            }
            if *op == "||" {
                return Ok(b2f(truthy(eval(l, vars)?) || truthy(eval(r, vars)?)));
            }
            let (a, b) = (eval(l, vars)?, eval(r, vars)?);
            match *op {
                "+" => a + b,
                "-" => a - b,
                "*" => a * b,
                "/" => a / b,
                "%" => a % b,
                "<" => b2f(a < b),
                "<=" => b2f(a <= b),
                ">" => b2f(a > b),
                ">=" => b2f(a >= b),
                "==" => b2f(a == b),
                "!=" => b2f(a != b),
                "&" => (int(a) & int(b)) as f64,
                "|" => (int(a) | int(b)) as f64,
                "^" => (int(a) ^ int(b)) as f64,
                "<<" => int(a).checked_shl(int(b).clamp(0, 63) as u32).unwrap_or(0) as f64,
                ">>" => int(a).checked_shr(int(b).clamp(0, 63) as u32).unwrap_or(0) as f64,
                o => return Err(ExprError::Unsupported(format!("operator '{o}'"))),
            }
        }
        Ast::Cond(c, a, b) => {
            if truthy(eval(c, vars)?) {
                eval(a, vars)?
            } else {
                eval(b, vars)?
            }
        }
        Ast::Call(name, args) => {
            let v: Vec<f64> = args
                .iter()
                .map(|x| eval(x, vars))
                .collect::<Result<_, _>>()?;
            let one = |f: fn(f64) -> f64| -> Result<f64, ExprError> {
                match v.as_slice() {
                    [x] => Ok(f(*x)),
                    _ => Err(ExprError::Parse(format!("{name}() takes one argument"))),
                }
            };
            match name.as_str() {
                "abs" => one(f64::abs)?,
                "round" => one(f64::round)?,
                "floor" => one(f64::floor)?,
                "ceil" => one(f64::ceil)?,
                "sqrt" => one(f64::sqrt)?,
                "exp" => one(f64::exp)?,
                "log" => one(f64::ln)?,
                "log10" => one(f64::log10)?,
                "sin" => one(f64::sin)?,
                "cos" => one(f64::cos)?,
                "tan" => one(f64::tan)?,
                "isNaN" => one(|x| b2f(x.is_nan()))?,
                "pow" => match v.as_slice() {
                    [a, b] => a.powf(*b),
                    _ => return Err(ExprError::Parse("pow() takes two arguments".into())),
                },
                "min" if !v.is_empty() => v.iter().copied().fold(f64::INFINITY, f64::min),
                "max" if !v.is_empty() => v.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                other => return Err(ExprError::Unsupported(format!("function {other}()"))),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(s: &str) -> Result<f64, ExprError> {
        Expr::parse(s)?.eval(&|n| match n {
            "coolantRaw" => Some(130.0),
            "status" => Some(0b1010 as f64),
            "arr[2]" => Some(7.0),
            _ => None,
        })
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(ev("coolantRaw - 40").unwrap(), 90.0);
        assert_eq!(ev("(coolantRaw - 40) * 1.8 + 32").unwrap(), 194.0);
        assert_eq!(ev("1 + 2 * 3 - -1").unwrap(), 8.0);
        assert_eq!(ev("status & 2 ? 10 : 20").unwrap(), 10.0);
        assert_eq!(ev("(status >> 2) & 1").unwrap(), 0.0);
        assert_eq!(ev("!0 && 3 >= 2 || 0").unwrap(), 1.0);
        assert_eq!(ev("pow(2, 10) + abs(-1) + 0x10").unwrap(), 1041.0);
        assert_eq!(ev("arr[1 + 1] * 2").unwrap(), 14.0);
        assert_eq!(ev("1.5e2").unwrap(), 150.0);
    }

    #[test]
    fn errors_are_typed() {
        assert_eq!(ev("nope + 1"), Err(ExprError::Unknown("nope".into())));
        assert!(matches!(
            ev("table(coolantRaw, \"x.inc\")"),
            Err(ExprError::Unsupported(_))
        ));
        assert!(matches!(
            ev("smoothBasic(coolantRaw, 3)"),
            Err(ExprError::Unsupported(_))
        ));
        assert!(matches!(ev("1 +"), Err(ExprError::Parse(_))));
    }
}
