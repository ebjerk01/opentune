//! Raw bytes <-> user values for constants and channels.
//!
//! `user = (raw + translate) * scale` and `raw = user / scale - translate`, as
//! the definition format specifies. Integer types round to the nearest raw
//! value; a user value whose raw value does not fit the storage type is an
//! error, never clamped silently.
use crate::expr::Expr;
use crate::model::{Constant, DataType, Endian, Kind, Num};
use crate::Error;
use serde::Serialize;

/// A constant's value in user units.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Value {
    Scalar(f64),
    /// Index into the bit field's options.
    Bits(u32),
    /// Row-major, `rows * cols` values.
    Array(Vec<f64>),
    Text(String),
}

/// Resolved scale and translate of a constant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scaling {
    pub scale: f64,
    pub translate: f64,
}

impl Scaling {
    pub const IDENTITY: Scaling = Scaling {
        scale: 1.0,
        translate: 0.0,
    };

    pub fn to_user(&self, raw: f64) -> f64 {
        (raw + self.translate) * self.scale
    }

    pub fn to_raw(&self, user: f64) -> f64 {
        user / self.scale - self.translate
    }
}

/// Evaluate a [`Num`] with `vars` for any expression.
pub fn eval_num(n: &Num, vars: &dyn Fn(&str) -> Option<f64>) -> Result<f64, Error> {
    match n {
        Num::Lit(v) => Ok(*v),
        Num::Expr(e) => Ok(Expr::parse(e)?.eval(vars)?),
    }
}

/// Resolve a constant's scaling; `vars` supplies names its expressions use.
pub fn scaling(c: &Constant, vars: &dyn Fn(&str) -> Option<f64>) -> Result<Scaling, Error> {
    let s = Scaling {
        scale: eval_num(&c.scale, vars)?,
        translate: eval_num(&c.translate, vars)?,
    };
    if s.scale == 0.0 || !s.scale.is_finite() || !s.translate.is_finite() {
        return Err(Error::Value(format!(
            "'{}': unusable scale {}",
            c.name, s.scale
        )));
    }
    Ok(s)
}

/// Read one raw element of type `ty` at the start of `b`.
pub fn read_raw(b: &[u8], ty: DataType, endian: Endian) -> Option<f64> {
    let n = ty.size();
    let s = b.get(..n)?;
    let mut a = [0u8; 8];
    match endian {
        Endian::Big => a[8 - n..].copy_from_slice(s),
        Endian::Little => {
            for (i, x) in s.iter().enumerate() {
                a[7 - i] = *x;
            }
        }
    }
    let u = u64::from_be_bytes(a);
    Some(match ty {
        DataType::U08 | DataType::U16 | DataType::U32 | DataType::Ascii => u as f64,
        DataType::S08 => u as u8 as i8 as f64,
        DataType::S16 => u as u16 as i16 as f64,
        DataType::S32 => u as u32 as i32 as f64,
        DataType::F32 => f32::from_bits(u as u32) as f64,
        DataType::F64 => f64::from_bits(u),
    })
}

/// Read a raw unsigned word (for bit fields).
fn read_word(b: &[u8], ty: DataType, endian: Endian) -> Option<u64> {
    let n = ty.size();
    let s = b.get(..n)?;
    let mut v = 0u64;
    match endian {
        Endian::Big => s.iter().for_each(|&x| v = (v << 8) | x as u64),
        Endian::Little => s.iter().rev().for_each(|&x| v = (v << 8) | x as u64),
    }
    Some(v)
}

fn write_word(b: &mut [u8], ty: DataType, endian: Endian, v: u64) {
    let n = ty.size();
    for i in 0..n {
        let byte = (v >> (8 * i)) as u8;
        match endian {
            Endian::Little => b[i] = byte,
            Endian::Big => b[n - 1 - i] = byte,
        }
    }
}

/// Write one raw element. Integer raws must be whole and in range.
pub fn write_raw(b: &mut [u8], ty: DataType, endian: Endian, raw: f64) -> Result<(), String> {
    if b.len() < ty.size() {
        return Err("outside the page".into());
    }
    let bits: u64 = match ty {
        DataType::F32 => {
            let f = raw as f32;
            if raw.is_finite() && !f.is_finite() {
                return Err(format!("{raw} does not fit F32"));
            }
            f.to_bits() as u64
        }
        DataType::F64 => raw.to_bits(),
        _ => {
            if !raw.is_finite() || raw.fract() != 0.0 {
                return Err(format!("raw value {raw} is not a whole number"));
            }
            let (lo, hi) = ty.raw_range();
            if raw < lo || raw > hi {
                return Err(format!("raw value {raw} outside {ty:?} range {lo}..={hi}"));
            }
            (raw as i64) as u64
        }
    };
    let mask = if ty.size() >= 8 {
        u64::MAX
    } else {
        (1u64 << (8 * ty.size())) - 1
    };
    write_word(b, ty, endian, bits & mask);
    Ok(())
}

/// The raw value to store for `user`: rounded for integer types.
pub fn user_to_raw(ty: DataType, s: &Scaling, user: f64) -> f64 {
    let r = s.to_raw(user);
    if ty.is_float() {
        r
    } else {
        r.round()
    }
}

/// Decode a constant from its page bytes.
pub fn decode(c: &Constant, page: &[u8], endian: Endian, s: &Scaling) -> Result<Value, Error> {
    let bytes = page
        .get(c.offset..c.offset + c.byte_len())
        .ok_or_else(|| Error::Value(format!("'{}' lies outside its page", c.name)))?;
    Ok(match &c.kind {
        Kind::Scalar => Value::Scalar(s.to_user(read_raw(bytes, c.ty, endian).unwrap_or(0.0))),
        Kind::Array { .. } => Value::Array(
            bytes
                .chunks(c.ty.size())
                .map(|e| s.to_user(read_raw(e, c.ty, endian).unwrap_or(0.0)))
                .collect(),
        ),
        Kind::Bits { lo, hi, .. } => {
            let w = read_word(bytes, c.ty, endian).unwrap_or(0);
            let width = (hi - lo + 1) as u32;
            Value::Bits(((w >> lo) & ((1u64 << width) - 1)) as u32)
        }
        Kind::String { .. } => {
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            Value::Text(bytes[..end].iter().map(|&b| b as char).collect())
        }
    })
}

/// Encode `v` into the constant's bytes in `page`.
pub fn encode(
    c: &Constant,
    v: &Value,
    page: &mut [u8],
    endian: Endian,
    s: &Scaling,
) -> Result<(), Error> {
    let len = c.byte_len();
    let name = &c.name;
    let bytes = page
        .get_mut(c.offset..c.offset + len)
        .ok_or_else(|| Error::Value(format!("'{name}' lies outside its page")))?;
    let bad = |m: String| Error::Value(format!("'{name}': {m}"));
    match (&c.kind, v) {
        (Kind::Scalar, Value::Scalar(u)) => {
            write_raw(bytes, c.ty, endian, user_to_raw(c.ty, s, *u)).map_err(bad)?
        }
        (Kind::Array { .. }, Value::Array(vals)) => {
            if vals.len() != c.elements() {
                return Err(bad(format!(
                    "has {} values, the definition says {}",
                    vals.len(),
                    c.elements()
                )));
            }
            // Check every element before touching the page.
            let mut tmp = bytes.to_vec();
            for (chunk, u) in tmp.chunks_mut(c.ty.size()).zip(vals) {
                write_raw(chunk, c.ty, endian, user_to_raw(c.ty, s, *u)).map_err(bad)?;
            }
            bytes.copy_from_slice(&tmp);
        }
        (Kind::Bits { lo, hi, .. }, Value::Bits(i)) => {
            let width = (hi - lo + 1) as u32;
            let mask = (1u64 << width) - 1;
            if u64::from(*i) > mask {
                return Err(bad(format!("option {i} does not fit bits [{lo}:{hi}]")));
            }
            let w = read_word(bytes, c.ty, endian).unwrap_or(0);
            let w = (w & !(mask << lo)) | (u64::from(*i) << lo);
            write_word(bytes, c.ty, endian, w);
        }
        (Kind::String { len }, Value::Text(t)) => {
            let tb: Vec<u8> = t
                .chars()
                .map(|ch| {
                    u8::try_from(u32::from(ch))
                        .map_err(|_| bad(format!("non-Latin-1 character {ch:?}")))
                })
                .collect::<Result<_, _>>()?;
            if tb.len() > *len {
                return Err(bad(format!("text longer than {len} bytes")));
            }
            bytes.fill(0);
            bytes[..tb.len()].copy_from_slice(&tb);
        }
        (k, v) => return Err(bad(format!("value {v:?} does not match kind {k:?}"))),
    }
    Ok(())
}

/// Out-of-range check against the definition's `lo`/`hi` (user units). Returns
/// a message per offending element; expressions in limits are skipped.
pub fn range_issues(c: &Constant, v: &Value) -> Vec<String> {
    let (lo, hi) = (
        c.min.as_ref().and_then(Num::lit),
        c.max.as_ref().and_then(Num::lit),
    );
    let check = |u: f64| -> Option<String> {
        let eps = 1e-9 * (1.0 + u.abs());
        if lo.is_some_and(|l| u < l - eps) || hi.is_some_and(|h| u > h + eps) {
            Some(format!(
                "{} = {u} outside {}..{}",
                c.name,
                lo.map_or("-inf".into(), |x| x.to_string()),
                hi.map_or("inf".into(), |x| x.to_string())
            ))
        } else {
            None
        }
    };
    match v {
        Value::Scalar(u) => check(*u).into_iter().collect(),
        Value::Array(vals) => vals.iter().filter_map(|u| check(*u)).take(1).collect(),
        _ => Vec::new(),
    }
}
