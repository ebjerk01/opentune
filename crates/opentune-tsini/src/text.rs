//! Low-level text handling for INI files: decoding, comment stripping, line
//! continuation, comma splitting and quoted strings.

/// Decode file bytes: UTF-8 (BOM stripped) when valid, else ISO-8859-1, which
/// is what older MegaSquirt definitions and `.msq` files are written in.
pub fn decode(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// One logical line: comment stripped, trimmed, continuations joined.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    /// 1-based number of the line's first physical line.
    pub no: usize,
    pub text: String,
}

/// Remove a `;` comment that is outside double quotes. A backslash escapes
/// the next character inside quotes.
pub fn strip_comment(line: &str) -> &str {
    let mut in_q = false;
    let mut esc = false;
    for (i, c) in line.char_indices() {
        if in_q {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_q = false;
            }
        } else if c == '"' {
            in_q = true;
        } else if c == ';' {
            return &line[..i];
        }
    }
    line
}

/// Split the source into logical lines. Empty lines are dropped. A line whose
/// (comment-stripped) text ends in a lone backslash continues on the next.
pub fn logical_lines(src: &str) -> Vec<Line> {
    let mut out = Vec::new();
    let mut pending: Option<Line> = None;
    for (i, raw) in src.lines().enumerate() {
        let no = i + 1;
        let t = strip_comment(raw).trim();
        let (t, cont) = match t.strip_suffix('\\') {
            // `\\` at the end is an escaped backslash, not a continuation.
            Some(rest) if !rest.ends_with('\\') => (rest.trim_end(), true),
            _ => (t, false),
        };
        let line = match pending.take() {
            Some(mut p) => {
                if !t.is_empty() {
                    if !p.text.is_empty() {
                        p.text.push(' ');
                    }
                    p.text.push_str(t);
                }
                p
            }
            None => Line {
                no,
                text: t.to_owned(),
            },
        };
        if cont {
            pending = Some(line);
        } else if !line.text.is_empty() {
            out.push(line);
        }
    }
    if let Some(p) = pending {
        if !p.text.is_empty() {
            out.push(p);
        }
    }
    out
}

/// Split `a, "b, c", {x, y}, [1x2]` on top-level commas. Each item is trimmed.
pub fn split_values(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut in_q = false;
    let mut esc = false;
    for c in s.chars() {
        if in_q {
            cur.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_q = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_q = true;
                cur.push(c);
            }
            '{' | '[' | '(' => {
                depth += 1;
                cur.push(c);
            }
            '}' | ']' | ')' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth <= 0 => {
                out.push(cur.trim().to_owned());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() || !out.is_empty() {
        out.push(cur.trim().to_owned());
    }
    out
}

/// Split `key = value` at the first `=` outside quotes. Returns trimmed parts.
pub fn split_key_value(s: &str) -> Option<(&str, &str)> {
    let mut in_q = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => in_q = !in_q,
            '=' if !in_q => return Some((s[..i].trim(), s[i + 1..].trim())),
            _ => {}
        }
    }
    None
}

/// The inner text of a `"..."` item, escapes left as written. `None` when the
/// item is not a quoted string.
pub fn quoted_inner(item: &str) -> Option<&str> {
    let t = item.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        Some(&t[1..t.len() - 1])
    } else {
        None
    }
}

/// A label/units string: the unescaped inner text of a quoted item, or the bare
/// item itself.
pub fn text_value(item: &str) -> String {
    match quoted_inner(item) {
        Some(inner) => unescape_text(inner),
        None => item.trim().to_owned(),
    }
}

/// Resolve `\"`, `\\`, `\n`, `\t` in display text; other escapes are kept.
pub fn unescape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// The inside of a `{ expr }` item, trimmed.
pub fn braced_inner(item: &str) -> Option<&str> {
    let t = item.trim();
    if t.starts_with('{') && t.ends_with('}') && t.len() >= 2 {
        Some(t[1..t.len() - 1].trim())
    } else {
        None
    }
}

/// Parse a number the way definitions write them: decimal, `0x` hex, a
/// leading `+`, or a leading `.`.
pub fn parse_number(s: &str) -> Option<f64> {
    let t = s.trim();
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return i64::from_str_radix(h, 16).ok().map(|v| v as f64);
    }
    let t = t.strip_prefix('+').unwrap_or(t);
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_respect_quotes() {
        assert_eq!(strip_comment(r#"a = "x;y" ; c"#), r#"a = "x;y" "#);
        assert_eq!(strip_comment(r#"a = "q\"; z" ;c"#), r#"a = "q\"; z" "#);
    }

    #[test]
    fn continuation_joins() {
        let l = logical_lines("a = 1, \\\n   2 ; c\n\n b = 3\n");
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].text, "a = 1, 2");
        assert_eq!(l[0].no, 1);
        assert_eq!(l[1].no, 4);
    }

    #[test]
    fn splitting() {
        assert_eq!(
            split_values(r#"array, U08, 0, [16x16], "a, b", { x, y }, 1"#),
            vec!["array", "U08", "0", "[16x16]", "\"a, b\"", "{ x, y }", "1"]
        );
        assert_eq!(split_key_value("k=\"a=b\""), Some(("k", "\"a=b\"")));
        assert_eq!(parse_number(".1"), Some(0.1));
        assert_eq!(parse_number("0x1F"), Some(31.0));
        assert_eq!(decode(&[0x41, 0xB0]), "A\u{B0}");
    }
}
