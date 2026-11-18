//! Command templates such as `"p%2i%2o%2c"` or `"r\$tsCanId\x04%2o%2c"`.
//!
//! A template is a byte string with placeholders, as the open firmware
//! definitions write them:
//!
//! - `\xNN` hex byte, `\NNN` octal byte, `\\`, `\"`, `\n`, `\r`, `\t`;
//! - `$name` / `\$name` — a PC variable (e.g. `tsCanId`), one byte;
//! - `%Ni` page identifier (N bytes), `%No` offset, `%Nc` count (N bytes, in the
//!   definition's `endianness`; N defaults to 1), `%v` the value bytes;
//! - `%%` a literal `%`.
use crate::model::Endian;
use serde::Serialize;

/// One piece of a template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "part", content = "arg", rename_all = "snake_case")]
pub enum CmdPart {
    Bytes(Vec<u8>),
    Var(String),
    PageId(u8),
    Offset(u8),
    Count(u8),
    Value,
}

/// A parsed command template; `raw` is the text as written (without quotes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandTemplate {
    pub raw: String,
    pub parts: Vec<CmdPart>,
}

/// Inputs for [`CommandTemplate::render`].
pub struct RenderCtx<'a> {
    pub endian: Endian,
    /// Rendered page identifier bytes for `%i`.
    pub page_id: &'a [u8],
    pub offset: usize,
    pub count: usize,
    pub value: &'a [u8],
    /// Byte value of a `$name` variable.
    pub vars: &'a dyn Fn(&str) -> Option<u8>,
}

/// What a request matched against a template carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matched {
    pub offset: Option<usize>,
    pub count: Option<usize>,
    pub value: Vec<u8>,
}

fn push_bytes(parts: &mut Vec<CmdPart>, b: u8) {
    if let Some(CmdPart::Bytes(v)) = parts.last_mut() {
        v.push(b);
    } else {
        parts.push(CmdPart::Bytes(vec![b]));
    }
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn put_uint(out: &mut Vec<u8>, v: usize, width: u8, endian: Endian) -> Result<(), String> {
    let w = width as usize;
    if w == 0 || w > 4 {
        return Err(format!("unsupported field width {w}"));
    }
    if w < 4 && v >= 1usize << (8 * w) {
        return Err(format!("value {v} does not fit in {w} byte(s)"));
    }
    let be = (v as u32).to_be_bytes();
    let bytes = &be[4 - w..];
    match endian {
        Endian::Big => out.extend_from_slice(bytes),
        Endian::Little => out.extend(bytes.iter().rev()),
    }
    Ok(())
}

fn get_uint(b: &[u8], endian: Endian) -> usize {
    let mut v = 0usize;
    match endian {
        Endian::Big => b.iter().for_each(|&x| v = (v << 8) | x as usize),
        Endian::Little => b.iter().rev().for_each(|&x| v = (v << 8) | x as usize),
    }
    v
}

impl CommandTemplate {
    /// Parse the inner text of a quoted command string.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let b = raw.as_bytes();
        let mut parts = Vec::new();
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            match c {
                b'\\' => {
                    let n = *b.get(i + 1).ok_or("dangling backslash")?;
                    match n {
                        b'x' | b'X' => {
                            let hex: Vec<u8> = b[i + 2..]
                                .iter()
                                .take(2)
                                .take_while(|c| c.is_ascii_hexdigit())
                                .copied()
                                .collect();
                            if hex.is_empty() {
                                return Err(format!("bad \\x escape in {raw:?}"));
                            }
                            let s = std::str::from_utf8(&hex).map_err(|e| e.to_string())?;
                            push_bytes(
                                &mut parts,
                                u8::from_str_radix(s, 16).map_err(|e| e.to_string())?,
                            );
                            i += 2 + hex.len();
                        }
                        b'0'..=b'7' => {
                            let oct: Vec<u8> = b[i + 1..]
                                .iter()
                                .take(3)
                                .take_while(|c| (b'0'..=b'7').contains(c))
                                .copied()
                                .collect();
                            let s = std::str::from_utf8(&oct).map_err(|e| e.to_string())?;
                            let v = u16::from_str_radix(s, 8).map_err(|e| e.to_string())?;
                            push_bytes(&mut parts, (v & 0xFF) as u8);
                            i += 1 + oct.len();
                        }
                        b'$' => {
                            // `\$name` is a variable reference.
                            i += 1;
                            continue;
                        }
                        b'n' => {
                            push_bytes(&mut parts, b'\n');
                            i += 2;
                        }
                        b'r' => {
                            push_bytes(&mut parts, b'\r');
                            i += 2;
                        }
                        b't' => {
                            push_bytes(&mut parts, b'\t');
                            i += 2;
                        }
                        other => {
                            push_bytes(&mut parts, other);
                            i += 2;
                        }
                    }
                }
                b'$' if b.get(i + 1).is_some_and(|&c| is_ident_start(c)) => {
                    let start = i + 1;
                    let mut j = start;
                    while j < b.len() && is_ident(b[j]) {
                        j += 1;
                    }
                    parts.push(CmdPart::Var(raw[start..j].to_owned()));
                    i = j;
                }
                b'%' => {
                    let mut j = i + 1;
                    let mut width = 0u8;
                    while j < b.len() && b[j].is_ascii_digit() {
                        width = width.saturating_mul(10).saturating_add(b[j] - b'0');
                        j += 1;
                    }
                    let w = if width == 0 { 1 } else { width };
                    match b.get(j) {
                        Some(b'i') => parts.push(CmdPart::PageId(w)),
                        Some(b'o') => parts.push(CmdPart::Offset(w)),
                        Some(b'c') => parts.push(CmdPart::Count(w)),
                        Some(b'v') => parts.push(CmdPart::Value),
                        Some(b'%') if j == i + 1 => push_bytes(&mut parts, b'%'),
                        _ => return Err(format!("unknown placeholder in {raw:?}")),
                    }
                    i = j + 1;
                }
                _ => {
                    push_bytes(&mut parts, c);
                    i += 1;
                }
            }
        }
        Ok(Self {
            raw: raw.to_owned(),
            parts,
        })
    }

    pub fn has_offset(&self) -> bool {
        self.parts.iter().any(|p| matches!(p, CmdPart::Offset(_)))
    }

    pub fn has_count(&self) -> bool {
        self.parts.iter().any(|p| matches!(p, CmdPart::Count(_)))
    }

    pub fn has_value(&self) -> bool {
        self.parts.contains(&CmdPart::Value)
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// Render to the bytes sent on the wire.
    pub fn render(&self, ctx: &RenderCtx) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        for p in &self.parts {
            match p {
                CmdPart::Bytes(b) => out.extend_from_slice(b),
                CmdPart::Var(n) => out.push(
                    (ctx.vars)(n)
                        .ok_or_else(|| format!("unknown variable ${n} in {:?}", self.raw))?,
                ),
                CmdPart::PageId(w) => {
                    if ctx.page_id.len() != *w as usize {
                        return Err(format!(
                            "page identifier is {} byte(s) but {:?} expects {w}",
                            ctx.page_id.len(),
                            self.raw
                        ));
                    }
                    out.extend_from_slice(ctx.page_id);
                }
                CmdPart::Offset(w) => put_uint(&mut out, ctx.offset, *w, ctx.endian)?,
                CmdPart::Count(w) => put_uint(&mut out, ctx.count, *w, ctx.endian)?,
                CmdPart::Value => out.extend_from_slice(ctx.value),
            }
        }
        Ok(out)
    }

    /// Match a complete request against this template (used by simulators).
    /// `page_id` is the rendered identifier this template is for. A `%v` takes
    /// `count` bytes when the template has a count, else the rest.
    pub fn match_request(
        &self,
        req: &[u8],
        endian: Endian,
        page_id: &[u8],
        vars: &dyn Fn(&str) -> Option<u8>,
    ) -> Option<Matched> {
        let mut m = Matched {
            offset: None,
            count: None,
            value: Vec::new(),
        };
        let mut i = 0;
        for p in &self.parts {
            match p {
                CmdPart::Bytes(b) => {
                    if req.get(i..i + b.len())? != b.as_slice() {
                        return None;
                    }
                    i += b.len();
                }
                CmdPart::Var(n) => {
                    if *req.get(i)? != vars(n)? {
                        return None;
                    }
                    i += 1;
                }
                CmdPart::PageId(w) => {
                    let w = *w as usize;
                    if req.get(i..i + w)? != page_id {
                        return None;
                    }
                    i += w;
                }
                CmdPart::Offset(w) | CmdPart::Count(w) => {
                    let w = *w as usize;
                    let v = get_uint(req.get(i..i + w)?, endian);
                    if matches!(p, CmdPart::Offset(_)) {
                        m.offset = Some(v);
                    } else {
                        m.count = Some(v);
                    }
                    i += w;
                }
                CmdPart::Value => {
                    let rest = req.get(i..)?;
                    let n = m.count.unwrap_or(rest.len()).min(rest.len());
                    m.value = rest[..n].to_vec();
                    i += n;
                }
            }
        }
        (i == req.len()).then_some(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(n: &str) -> Option<u8> {
        (n == "tsCanId").then_some(0)
    }

    #[test]
    fn speeduino_read() {
        let t = CommandTemplate::parse("p%2i%2o%2c").unwrap();
        let id = CommandTemplate::parse("\\$tsCanId\\x02").unwrap();
        let page_id = id
            .render(&RenderCtx {
                endian: Endian::Little,
                page_id: &[],
                offset: 0,
                count: 0,
                value: &[],
                vars: &vars,
            })
            .unwrap();
        assert_eq!(page_id, vec![0, 2]);
        let r = t
            .render(&RenderCtx {
                endian: Endian::Little,
                page_id: &page_id,
                offset: 0x0102,
                count: 251,
                value: &[],
                vars: &vars,
            })
            .unwrap();
        assert_eq!(r, vec![b'p', 0, 2, 0x02, 0x01, 251, 0]);
        let m = t
            .match_request(&r, Endian::Little, &page_id, &vars)
            .unwrap();
        assert_eq!((m.offset, m.count), (Some(0x102), Some(251)));
    }

    #[test]
    fn ms_big_endian_and_octal() {
        let t = CommandTemplate::parse("r\\$tsCanId\\x04%2o%2c").unwrap();
        let r = t
            .render(&RenderCtx {
                endian: Endian::Big,
                page_id: &[],
                offset: 0x0102,
                count: 3,
                value: &[],
                vars: &vars,
            })
            .unwrap();
        assert_eq!(r, vec![b'r', 0, 4, 1, 2, 0, 3]);
        let p = CommandTemplate::parse("P\\001W%o%v").unwrap();
        assert_eq!(p.parts[0], CmdPart::Bytes(vec![b'P', 1, b'W']));
        assert_eq!(p.parts[1], CmdPart::Offset(1));
        let w = p
            .render(&RenderCtx {
                endian: Endian::Big,
                page_id: &[],
                offset: 300,
                count: 1,
                value: &[9],
                vars: &vars,
            })
            .unwrap_err();
        assert!(w.contains("does not fit"));
    }
}
