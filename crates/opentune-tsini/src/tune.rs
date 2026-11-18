//! [`Tune`]: the page bytes of a calibration, read and written by name.
use crate::model::{Ini, Kind};
use crate::value::{self, Scaling, Value};
use crate::{Error, Result};
use std::collections::BTreeMap;

/// The bytes of every page plus the PC-side variables of one tune.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Tune {
    /// One buffer per [`Ini::pages`] entry, each exactly the page size.
    pub pages: Vec<Vec<u8>>,
    /// `[PcVariables]` values (they live in the tune file, not the ECU).
    pub pc: BTreeMap<String, Value>,
}

impl Tune {
    /// All pages zero-filled.
    pub fn blank(ini: &Ini) -> Self {
        Self {
            pages: ini.pages.iter().map(|p| vec![0u8; p.size]).collect(),
            pc: BTreeMap::new(),
        }
    }

    /// Wrap page buffers, checking their count and sizes against `ini`.
    pub fn from_pages(ini: &Ini, pages: Vec<Vec<u8>>) -> Result<Self> {
        if pages.len() != ini.pages.len() {
            return Err(Error::Value(format!(
                "{} page buffers for {} pages",
                pages.len(),
                ini.pages.len()
            )));
        }
        for (p, d) in pages.iter().zip(&ini.pages) {
            if p.len() != d.size {
                return Err(Error::Value(format!(
                    "page {} is {} bytes, the INI says {}",
                    d.index + 1,
                    p.len(),
                    d.size
                )));
            }
        }
        Ok(Self {
            pages,
            pc: BTreeMap::new(),
        })
    }

    /// Resolve a name for an expression: scalar/bit constants with literal
    /// scaling and scalar PC variables. Expression-scaled constants are not
    /// resolved here (no recursion).
    pub fn var(&self, ini: &Ini, name: &str) -> Option<f64> {
        self.var_depth(ini, name, 0)
    }

    /// [`var`](Self::var), also evaluating `{ }` output channels that only
    /// depend on constants (Speeduino's `idleRes` scale helper, for example).
    fn var_depth(&self, ini: &Ini, name: &str, depth: usize) -> Option<f64> {
        if ini.constant(name).is_none() && !self.pc.contains_key(name) {
            let ch = ini.output_channel(name)?;
            let crate::model::ChannelKind::Expr { expr } = &ch.kind else {
                return None;
            };
            if depth >= 8 {
                return None;
            }
            return crate::expr::Expr::parse(expr)
                .ok()?
                .eval(&|n| self.var_depth(ini, n, depth + 1))
                .ok();
        }
        if let Some(v) = self.pc.get(name) {
            return match v {
                Value::Scalar(x) => Some(*x),
                Value::Bits(i) => Some(*i as f64),
                _ => None,
            };
        }
        let c = ini.constant(name)?;
        let page = self.pages.get(c.page?)?;
        let s = Scaling {
            scale: c.scale.lit()?,
            translate: c.translate.lit()?,
        };
        match value::decode(c, page, ini.endianness, &s).ok()? {
            Value::Scalar(x) => Some(x),
            Value::Bits(i) => Some(i as f64),
            _ => None,
        }
    }

    /// The scaling of constant `name`, evaluating expressions against this tune.
    pub fn scaling(&self, ini: &Ini, name: &str) -> Result<Scaling> {
        let c = ini
            .constant(name)
            .ok_or_else(|| Error::Unknown(name.into()))?;
        if matches!(c.kind, Kind::Bits { .. } | Kind::String { .. }) {
            return Ok(Scaling::IDENTITY);
        }
        value::scaling(c, &|n| self.var(ini, n))
    }

    /// Read constant `name` in user units.
    pub fn get(&self, ini: &Ini, name: &str) -> Result<Value> {
        let c = ini
            .constant(name)
            .ok_or_else(|| Error::Unknown(name.into()))?;
        let Some(p) = c.page else {
            return self
                .pc
                .get(name)
                .cloned()
                .ok_or_else(|| Error::Value(format!("PC variable '{name}' has no value")));
        };
        let page = self
            .pages
            .get(p)
            .ok_or_else(|| Error::Value(format!("no page {} in this tune", p + 1)))?;
        let s = self.scaling(ini, name)?;
        value::decode(c, page, ini.endianness, &s)
    }

    /// Write constant `name` from user units. The page is unchanged on error.
    pub fn set(&mut self, ini: &Ini, name: &str, v: &Value) -> Result<()> {
        let c = ini
            .constant(name)
            .ok_or_else(|| Error::Unknown(name.into()))?;
        let Some(p) = c.page else {
            self.pc.insert(name.to_owned(), v.clone());
            return Ok(());
        };
        let s = self.scaling(ini, name)?;
        let page = self
            .pages
            .get_mut(p)
            .ok_or_else(|| Error::Value(format!("no page {} in this tune", p + 1)))?;
        value::encode(c, v, page, ini.endianness, &s)
    }

    /// Every page constant's value, in definition order.
    pub fn values(&self, ini: &Ini) -> Vec<(String, Result<Value>)> {
        ini.constants
            .iter()
            .map(|c| (c.name.clone(), self.get(ini, &c.name)))
            .collect()
    }

    /// Byte ranges `(page, start, end)` that differ from `other`.
    pub fn diff_ranges(&self, other: &Tune) -> Vec<(usize, usize, usize)> {
        let mut out = Vec::new();
        for (pi, (a, b)) in self.pages.iter().zip(&other.pages).enumerate() {
            let mut i = 0;
            let n = a.len().min(b.len());
            while i < n {
                if a[i] != b[i] {
                    let start = i;
                    while i < n && a[i] != b[i] {
                        i += 1;
                    }
                    out.push((pi, start, i));
                } else {
                    i += 1;
                }
            }
            if a.len() != b.len() {
                out.push((pi, n, a.len().max(b.len())));
            }
        }
        out
    }
}
