//! Redaction of free text before it leaves the machine.
//!
//! [`crate::privacy`] *finds* personal data in contributed files; this module
//! *replaces* it in text that is about to be shown to the user for review or
//! sent somewhere (the desktop app's diagnostics upload). It is deliberately
//! aggressive: a false positive costs a little detail in a bug report, a miss
//! leaks an identifier.
//!
//! [`redact`] applies, in this order:
//!
//! | What | Becomes |
//! | --- | --- |
//! | caller-supplied secrets ([`RedactContext::secrets`]) | `<SECRET>` |
//! | absolute paths (Unix, `C:\…`, UNC, `file://`) | `<PATH>/basename` |
//! | e-mail addresses (not RFC 2606 example domains) | `<EMAIL>` |
//! | MAC addresses (`aa:bb:…`, `AA-BB-…`) | `<MAC>` |
//! | IPv6 and IPv4 addresses | `<IP>` |
//! | hex runs: SecurityAccess seeds/keys ([`mask_secrets`]) | `27 01 <masked: N bytes>` |
//! | hex runs: VINs, part numbers … decoded from ASCII bytes | `"<VIN>"`, `"<PART>"` … |
//! | hex runs longer than [`HEX_MAX_BYTES`] bytes | first 16 bytes `… <N bytes>` |
//! | VIN-like tokens (all, synthetic ones too) | `<VIN>` |
//! | ECU / part numbers: VAG `0xx 9xx xxx`, Bosch `0 281 …` / `1037…`, Siemens `5WP…`, Mercedes `A xxx xxx xx xx` | `<PART>` |
//! | labelled serial numbers (`S/N: …`, `serial number …`) | `<SERIAL>` |
//! | the user name and host name ([`RedactContext`]) | `<USER>`, `<HOST>` |
//!
//! [`redact_json`] applies [`redact`] to every string (and key) of a JSON value.

use crate::privacy;
use serde_json::Value;

/// Hex payloads longer than this many bytes are cut ...
pub const HEX_MAX_BYTES: usize = 64;
/// ... to their length and this many leading bytes.
pub const HEX_KEEP_BYTES: usize = 16;

/// Replacement markers.
pub const VIN: &str = "<VIN>";
pub const EMAIL: &str = "<EMAIL>";
pub const SERIAL: &str = "<SERIAL>";
pub const PART: &str = "<PART>";
pub const PATH: &str = "<PATH>";
pub const USER: &str = "<USER>";
pub const HOST: &str = "<HOST>";
pub const IP: &str = "<IP>";
pub const MAC: &str = "<MAC>";
pub const SECRET: &str = "<SECRET>";

/// What the redactor knows about this machine.
#[derive(Debug, Clone, Default)]
pub struct RedactContext {
    /// The user's home directory (removed wherever it appears).
    pub home: Option<String>,
    /// The login name (replaced as a whole word, 3+ characters).
    pub user: Option<String>,
    /// The machine's host name (replaced as a whole word, 3+ characters).
    pub host: Option<String>,
    /// Exact strings that must never appear (an access token, an endpoint).
    pub secrets: Vec<String>,
}

/// Redact `s` (see the module docs).
pub fn redact(s: &str, ctx: &RedactContext) -> String {
    let mut t = s.to_string();
    for sec in ctx.secrets.iter().filter(|x| x.len() >= 4) {
        if t.contains(sec.as_str()) {
            t = t.replace(sec.as_str(), SECRET);
        }
    }
    t = redact_paths(&t);
    if let Some(home) = ctx.home.as_deref().filter(|h| h.len() >= 3) {
        if t.contains(home) {
            t = t.replace(home, PATH);
        }
    }
    t = replace_spans(&t, email_spans(&t), EMAIL);
    t = redact_network(&t);
    t = redact_hex(&t, ctx);
    redact_plain(&t, ctx)
}

/// [`redact`] every string and object key in `v`.
pub fn redact_json(v: &mut Value, ctx: &RedactContext) {
    match v {
        Value::String(s) => *s = redact(s, ctx),
        Value::Array(a) => a.iter_mut().for_each(|x| redact_json(x, ctx)),
        Value::Object(o) => {
            let old = std::mem::take(o);
            for (k, mut x) in old {
                redact_json(&mut x, ctx);
                o.insert(redact(&k, ctx), x);
            }
        }
        _ => {}
    }
}

/// The text-only steps (no paths, network, hex): VINs, part numbers, labelled
/// serials, e-mail, user and host names. Used for ASCII decoded from hex too.
fn redact_plain(s: &str, ctx: &RedactContext) -> String {
    let mut t = replace_spans(s, vin_spans(s), VIN);
    t = replace_spans(&t, email_spans(&t), EMAIL);
    t = replace_spans(&t, part_spans(&t), PART);
    let serials = {
        let mut h = Vec::new();
        privacy::scan_serials(&t, &mut h);
        h.into_iter().map(|(_, a, b)| (a, b)).collect()
    };
    t = replace_spans(&t, serials, SERIAL);
    if let Some(u) = ctx.user.as_deref() {
        t = replace_word(&t, u, USER);
    }
    if let Some(h) = ctx.host.as_deref() {
        t = replace_word(&t, h, HOST);
        // `myhost.local`, `myhost.lan`: the short name is enough to find.
        if let Some(short) = h.split('.').next().filter(|x| *x != h) {
            t = replace_word(&t, short, HOST);
        }
    }
    t
}

/// Replace the byte ranges `spans` (sorted or not, overlaps merged) by `with`.
fn replace_spans(s: &str, mut spans: Vec<(usize, usize)>, with: &str) -> String {
    if spans.is_empty() {
        return s.to_string();
    }
    spans.sort_unstable();
    let mut out = String::with_capacity(s.len());
    let mut at = 0;
    for (a, b) in spans {
        if a < at {
            at = at.max(b);
            continue;
        }
        out.push_str(&s[at..a]);
        out.push_str(with);
        at = b;
    }
    out.push_str(&s[at..]);
    out
}

fn is_alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c >= 0x80
}

fn is_word(c: u8) -> bool {
    is_alnum(c) || c == b'_'
}

/// `word` (case-insensitive) as a whole word, not inside a marker.
fn replace_word(s: &str, word: &str, with: &str) -> String {
    if word.len() < 3 || !word.is_ascii() {
        return s.to_string();
    }
    let lower = s.to_ascii_lowercase();
    let w = word.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut spans = Vec::new();
    let mut from = 0;
    while let Some(p) = lower[from..].find(&w) {
        let a = from + p;
        let e = a + w.len();
        let left_ok = a == 0 || (!is_alnum(b[a - 1]) && b[a - 1] != b'<');
        let right_ok = e >= b.len() || (!is_alnum(b[e]) && b[e] != b'>');
        if left_ok && right_ok {
            spans.push((a, e));
        }
        from = e;
    }
    replace_spans(s, spans, with)
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn path_end_char(c: u8) -> bool {
    c.is_ascii_whitespace()
        || matches!(
            c,
            b'"' | b'\'' | b'`' | b')' | b']' | b'}' | b',' | b';' | b'<' | b'>' | b'|'
        )
}

fn path_start_boundary(c: u8) -> bool {
    c.is_ascii_whitespace()
        || matches!(
            c,
            b'"' | b'\'' | b'`' | b'(' | b'[' | b'{' | b'=' | b',' | b';' | b'<' | b'>' | b'|'
        )
}

/// Where the path starting at `i` ends.
fn path_end(b: &[u8], i: usize) -> usize {
    let mut j = i;
    while j < b.len() && !path_end_char(b[j]) {
        j += 1;
    }
    // A trailing full stop or colon belongs to the sentence.
    while j > i && matches!(b[j - 1], b'.' | b':') {
        j -= 1;
    }
    j
}

/// The last non-empty segment of a path (`/`- or `\`-separated).
fn basename(p: &str) -> &str {
    p.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
}

/// Absolute paths → `<PATH>/basename` (Windows: `<PATH>\\basename`).
fn redact_paths(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut at = 0;
    let mut i = 0;
    while i < b.len() {
        let prev = if i == 0 { None } else { Some(b[i - 1]) };
        let boundary = prev.map_or(true, path_start_boundary);
        let mut found: Option<(usize, &str)> = None;
        if boundary && s[i..].starts_with("file:///") {
            let e = path_end(b, i + 7);
            if s[i + 8..e].contains('/') {
                found = Some((e, "/"));
            }
        } else if b[i] == b'/' {
            let colon_ok = prev == Some(b':');
            let next_ok = b
                .get(i + 1)
                .is_some_and(|&c| c != b'/' && !path_end_char(c));
            if (boundary || colon_ok) && next_ok {
                let e = path_end(b, i);
                if s[i + 1..e].contains('/') {
                    found = Some((e, "/"));
                }
            }
        } else {
            // `C:\…` / `C:/…`, or a UNC path `\\server\share\…`.
            let drive = b[i].is_ascii_alphabetic()
                && b.get(i + 1) == Some(&b':')
                && matches!(b.get(i + 2), Some(b'\\' | b'/'))
                && b.get(i + 3).is_some_and(|&c| !path_end_char(c));
            let unc = b[i] == b'\\'
                && b.get(i + 1) == Some(&b'\\')
                && b.get(i + 2)
                    .is_some_and(|&c| c != b'\\' && !path_end_char(c));
            if boundary && (drive || unc) {
                found = Some((path_end(b, i), "\\"));
            }
        }
        match found {
            Some((e, sep)) => {
                out.push_str(&s[at..i]);
                out.push_str(PATH);
                out.push_str(sep);
                out.push_str(basename(&s[i..e]));
                at = e;
                i = e;
            }
            None => i += utf8_len(b[i]),
        }
    }
    out.push_str(&s[at.min(s.len())..]);
    out
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

// ---------------------------------------------------------------------------
// E-mail, MAC, IP
// ---------------------------------------------------------------------------

fn email_spans(s: &str) -> Vec<(usize, usize)> {
    let mut h = Vec::new();
    privacy::scan_emails(s, &mut h);
    h.into_iter().map(|(_, a, b)| (a, b)).collect()
}

fn redact_network(s: &str) -> String {
    let t = replace_spans(s, mac_spans(s), MAC);
    let t = replace_spans(&t, ipv6_spans(&t), IP);
    replace_spans(&t, ipv4_spans(&t), IP)
}

fn mac_spans(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 17 <= b.len() {
        if i > 0 && (is_word(b[i - 1]) || matches!(b[i - 1], b':' | b'-')) {
            i += 1;
            continue;
        }
        let sep = b[i + 2];
        let ok = matches!(sep, b':' | b'-')
            && (0..6).all(|g| {
                let p = i + g * 3;
                b[p].is_ascii_hexdigit()
                    && b[p + 1].is_ascii_hexdigit()
                    && (g == 5 || b[p + 2] == sep)
            })
            && b.get(i + 17).map_or(true, |&c| !is_word(c) && c != sep);
        if ok {
            out.push((i, i + 17));
            i += 17;
        } else {
            i += 1;
        }
    }
    out
}

fn ipv6_spans(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if !(c.is_ascii_hexdigit() || c == b':')
            || (i > 0 && (is_word(b[i - 1]) || b[i - 1] == b':'))
        {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < b.len() && (b[j].is_ascii_hexdigit() || b[j] == b':') {
            j += 1;
        }
        // A zone index (`fe80::1%eth0`).
        let mut e = j;
        if b.get(j) == Some(&b'%') {
            e = j + 1;
            while e < b.len() && is_word(b[e]) {
                e += 1;
            }
        }
        let tok = &s[i..j];
        let groups: Vec<&str> = tok.split(':').collect();
        let colons = groups.len() - 1;
        let well_formed = colons >= 2
            && groups.iter().all(|g| g.len() <= 4)
            && groups.iter().any(|g| !g.is_empty())
            && !tok.contains(":::")
            && (tok.matches("::").count() == 1 || groups.len() == 8);
        let end_ok = b.get(e).map_or(true, |&c| !is_word(c) && c != b'.');
        if well_formed && end_ok {
            out.push((i, e));
            i = e;
        } else {
            i = j.max(i + 1);
        }
    }
    out
}

fn ipv4_spans(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() || (i > 0 && (is_word(b[i - 1]) || b[i - 1] == b'.')) {
            i += 1;
            continue;
        }
        let mut j = i;
        let mut ok = true;
        for k in 0..4 {
            let st = j;
            while j < b.len() && b[j].is_ascii_digit() && j - st < 4 {
                j += 1;
            }
            let n = &s[st..j];
            if n.is_empty() || n.len() > 3 || !n.parse::<u16>().is_ok_and(|v| v <= 255) {
                ok = false;
                break;
            }
            if k < 3 {
                if b.get(j) != Some(&b'.') {
                    ok = false;
                    break;
                }
                j += 1;
            }
        }
        let end_ok = ok
            && b.get(j).map_or(true, |&c| !is_word(c))
            && !(b.get(j) == Some(&b'.') && b.get(j + 1).is_some_and(u8::is_ascii_digit));
        if end_ok {
            out.push((i, j));
            i = j;
        } else {
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// VINs and part numbers
// ---------------------------------------------------------------------------

fn vin_spans(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        let st = i;
        while i < b.len() && b[i].is_ascii_alphanumeric() {
            i += 1;
        }
        if privacy::is_vin_like(&s[st..i]) {
            out.push((st, i));
        }
    }
    out
}

/// A character class and an exact count.
type Group = (fn(u8) -> bool, usize);

/// Match `groups` (each: a character class and an exact count) from `i`,
/// with at most one separator from `seps` between groups. Returns the end.
fn match_groups(b: &[u8], i: usize, groups: &[Group], seps: &[u8]) -> Option<usize> {
    let mut j = i;
    for (k, &(class, n)) in groups.iter().enumerate() {
        if k > 0
            && b.get(j).is_some_and(|c| seps.contains(c))
            && b.get(j + 1).is_some_and(|&c| class(c))
        {
            j += 1;
        }
        for _ in 0..n {
            if !b.get(j).is_some_and(|&c| class(c)) {
                return None;
            }
            j += 1;
        }
    }
    Some(j)
}

fn up_or_digit(c: u8) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit()
}
fn digit(c: u8) -> bool {
    c.is_ascii_digit()
}
fn at_end(b: &[u8], e: usize) -> bool {
    b.get(e).map_or(true, |&c| !is_alnum(c))
}

/// VAG `TTT GGG SSS [V]` (type group with a digit; main group 9xx unless the
/// type group has a letter), checked with `opentune_vin`'s parser.
fn vag_part(b: &[u8], i: usize) -> Option<usize> {
    let e = match_groups(b, i, &[(up_or_digit, 3), (digit, 3), (digit, 3)], b" .-")?;
    let s = std::str::from_utf8(&b[i..e]).ok()?;
    let ty = &b[i..i + 3];
    let p = opentune_vin::part::parse_part_number(s);
    let main9 = p
        .vag_main_group
        .as_deref()
        .is_some_and(|g| g.starts_with('9'));
    let letter = ty.iter().any(u8::is_ascii_uppercase);
    if p.kind != opentune_vin::part::PartKind::Vag || !(main9 || letter) {
        return None;
    }
    // Optional version index: up to 3 capital letters, maybe after a space.
    let mut v = e;
    if b.get(v) == Some(&b' ') && b.get(v + 1).is_some_and(u8::is_ascii_uppercase) {
        v += 1;
    }
    let vs = v;
    while v < b.len() && v - vs < 3 && b[v].is_ascii_uppercase() {
        v += 1;
    }
    if v > vs && at_end(b, v) {
        return Some(v);
    }
    at_end(b, e).then_some(e)
}

/// Bosch hardware `0 2xx xxx xxx` and software `1037…` / `1039…` numbers:
/// 10 digits, separators allowed after digit 1, 4 and 7.
fn bosch_part(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i;
    let mut d = String::new();
    while d.len() < 10 {
        let c = *b.get(j)?;
        if c.is_ascii_digit() {
            d.push(c as char);
            j += 1;
        } else if matches!(c, b' ' | b'.' | b'-')
            && matches!(d.len(), 1 | 4 | 7)
            && b.get(j + 1).is_some_and(u8::is_ascii_digit)
        {
            j += 1;
        } else {
            return None;
        }
    }
    if !at_end(b, j) {
        return None;
    }
    let family = &d[..4];
    let hw = ["0281", "0261", "0260", "0280", "0265", "0986"].contains(&family)
        && opentune_vin::part::parse_part_number(&d).kind == opentune_vin::part::PartKind::Bosch;
    (hw || family == "1037" || family == "1039").then_some(j)
}

/// Siemens / Continental `5WP…`, `5WK…`, `5WS…` + 4 or more digits.
fn siemens_part(b: &[u8], i: usize) -> Option<usize> {
    let pre = b.get(i..i + 3)?;
    if !(pre[0] == b'5'
        && pre[1].eq_ignore_ascii_case(&b'W')
        && matches!(pre[2].to_ascii_uppercase(), b'P' | b'K' | b'S'))
    {
        return None;
    }
    let mut j = i + 3;
    if b.get(j) == Some(&b' ') {
        j += 1;
    }
    let ds = j;
    while j < b.len() && b[j].is_ascii_digit() {
        j += 1;
    }
    if j - ds < 4 {
        return None;
    }
    while j < b.len() && b[j].is_ascii_alphanumeric() {
        j += 1;
    }
    Some(j)
}

/// Mercedes `A xxx xxx xx xx`.
fn mercedes_part(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'A') {
        return None;
    }
    let mut j = i + 1;
    if b.get(j) == Some(&b' ') {
        j += 1;
    }
    let e = match_groups(
        b,
        j,
        &[(digit, 3), (digit, 3), (digit, 2), (digit, 2)],
        b" ",
    )?;
    at_end(b, e).then_some(e)
}

fn part_spans(s: &str) -> Vec<(usize, usize)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if i > 0 && is_alnum(b[i - 1]) {
            i += 1;
            continue;
        }
        let hit = mercedes_part(b, i)
            .or_else(|| siemens_part(b, i))
            .or_else(|| bosch_part(b, i))
            .or_else(|| vag_part(b, i));
        match hit {
            Some(e) => {
                out.push((i, e));
                i = e;
            }
            None => i += 1,
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Hex runs
// ---------------------------------------------------------------------------

/// One run of hex in a text: byte range in the text and the decoded bytes.
struct HexRun {
    start: usize,
    end: usize,
    bytes: Vec<u8>,
}

fn unhex(w: &[u8]) -> Vec<u8> {
    let nib = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
    let mut out = Vec::with_capacity(w.len().div_ceil(2));
    let mut it = w.iter().copied();
    if w.len() % 2 != 0 {
        out.push(nib(it.next().unwrap_or(b'0')));
    }
    while let (Some(a), Some(b)) = (it.next(), it.next()) {
        out.push(nib(a) << 4 | nib(b));
    }
    out
}

/// Upper-case hex bytes joined by spaces (`27 01 AA`).
pub fn hexs(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 3);
    for (i, x) in b.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{x:02X}"));
    }
    s
}

/// The hex runs in `s`: words of an even number of hex digits (`27`,
/// `2702AABB`) joined by single spaces (`27 02 AA BB`), and `0x…` words on
/// their own. Anything else (another separator, a non-hex word such as a
/// CAN id `[7E0]`) ends a run.
fn hex_runs(s: &str) -> Vec<HexRun> {
    let b = s.as_bytes();
    let mut runs = Vec::new();
    let mut cur: Option<HexRun> = None;
    let mut i = 0;
    while i < b.len() {
        if !is_word(b[i]) || (i > 0 && is_word(b[i - 1])) {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < b.len() && is_word(b[j]) {
            j += 1;
        }
        let w = &b[i..j];
        let prefixed = w.len() > 2 && (w[0] == b'0' && (w[1] | 0x20) == b'x');
        if prefixed && w[2..].iter().all(u8::is_ascii_hexdigit) {
            runs.extend(cur.take());
            runs.push(HexRun {
                start: i,
                end: j,
                bytes: unhex(&w[2..]),
            });
        } else if w.len() % 2 == 0 && w.iter().all(u8::is_ascii_hexdigit) {
            let bytes = unhex(w);
            match &mut cur {
                Some(r) if i == r.end + 1 && b[r.end] == b' ' => {
                    r.bytes.extend(bytes);
                    r.end = j;
                }
                _ => {
                    runs.extend(cur.take());
                    cur = Some(HexRun {
                        start: i,
                        end: j,
                        bytes,
                    });
                }
            }
        } else {
            runs.extend(cur.take());
        }
        i = j;
    }
    runs.extend(cur);
    runs
}

/// Where a SecurityAccess (0x27) request or its positive response (0x67)
/// starts in `b`: at the front, after an ISO-TP single/first-frame PCI, or
/// after a KWP2000 header.
fn security_access_at(b: &[u8]) -> Option<usize> {
    let sa = |i: usize| matches!(b.get(i), Some(0x27 | 0x67));
    let first = *b.first()?;
    if sa(0) {
        Some(0)
    } else if (0x01..=0x0F).contains(&first) && sa(1) {
        Some(1) // ISO-TP single frame / length prefix
    } else if first & 0xF0 == 0x10 && sa(2) {
        Some(2) // ISO-TP first frame
    } else if first & 0x80 != 0 && sa(3) {
        Some(3) // KWP2000: format, target, source
    } else if first & 0x80 != 0 && sa(4) {
        Some(4) // KWP2000 with a length byte
    } else {
        None
    }
}

/// The SecurityAccess / seed-key replacement for one run, if any.
fn mask_security(run: &HexRun, before: &str) -> Option<String> {
    let b = &run.bytes;
    // "seed: 1A2B3C4D", "key=0x11223344": the value after a seed/key label.
    let label = before
        .trim_end_matches([' ', ':', '=', '\t'])
        .to_ascii_lowercase();
    if ["seed", "seeds", "key", "keys"]
        .iter()
        .any(|w| label.ends_with(w))
    {
        return Some(format!("<masked: {} bytes>", b.len()));
    }
    if let Some(at) = security_access_at(b) {
        let keep = at + 2; // the service byte and the sub-function
        if b.len() > keep {
            return Some(format!(
                "{} <masked: {} bytes>",
                hexs(&b[..keep]),
                b.len() - keep
            ));
        }
    }
    None
}

/// The text just before a run (up to 12 characters), for the label check.
fn before_run<'a>(s: &'a str, r: &HexRun) -> &'a str {
    &s[s[..r.start]
        .char_indices()
        .rev()
        .nth(11)
        .map_or(0, |(i, _)| i)..r.start]
}

/// The write-time masker of the diagnostics log: SecurityAccess seeds and
/// keys (a 0x27 request / 0x67 response, also behind an ISO-TP PCI or a
/// KWP2000 header: everything after the sub-function becomes
/// `<masked: N bytes>`), a hex value right after a `seed` / `key` label, and
/// hex longer than [`HEX_MAX_BYTES`] bytes (kept: its first
/// [`HEX_KEEP_BYTES`] bytes and its length). Everything else is unchanged.
pub fn mask_secrets(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut at = 0;
    for r in &hex_runs(s) {
        let rep = mask_security(r, before_run(s, r)).or_else(|| {
            (r.bytes.len() > HEX_MAX_BYTES).then(|| {
                format!(
                    "{} … <{} bytes>",
                    hexs(&r.bytes[..HEX_KEEP_BYTES]),
                    r.bytes.len()
                )
            })
        });
        if let Some(rep) = rep {
            out.push_str(&s[at..r.start]);
            out.push_str(&rep);
            at = r.end;
        }
    }
    out.push_str(&s[at..]);
    out
}

/// One piece of a re-rendered hex run.
enum Piece {
    Bytes(Vec<u8>),
    /// Redacted text standing for `len` original bytes.
    Text(String, usize),
}

/// Printable ASCII inside a run that the text redactor changes (a VIN read
/// with 0x22 F190, a part number, a serial) becomes a quoted marker.
fn hex_pieces(bytes: &[u8], ctx: &RedactContext) -> Option<Vec<Piece>> {
    let mut pieces = Vec::new();
    let mut changed = false;
    let mut at = 0;
    let mut i = 0;
    while i < bytes.len() {
        if !(0x20..0x7F).contains(&bytes[i]) {
            i += 1;
            continue;
        }
        let st = i;
        while i < bytes.len() && (0x20..0x7F).contains(&bytes[i]) {
            i += 1;
        }
        if i - st < 6 {
            continue;
        }
        let text: String = bytes[st..i].iter().map(|&c| c as char).collect();
        let red = redact_plain(&text, ctx);
        if red != text {
            changed = true;
            if st > at {
                pieces.push(Piece::Bytes(bytes[at..st].to_vec()));
            }
            pieces.push(Piece::Text(format!("\"{red}\""), i - st));
            at = i;
        }
    }
    if !changed {
        return None;
    }
    if at < bytes.len() {
        pieces.push(Piece::Bytes(bytes[at..].to_vec()));
    }
    Some(pieces)
}

fn render_pieces(pieces: &[Piece], total: usize) -> String {
    let long = total > HEX_MAX_BYTES;
    let mut parts: Vec<String> = Vec::new();
    let mut used = 0;
    for p in pieces {
        if long && used >= HEX_KEEP_BYTES {
            break;
        }
        match p {
            Piece::Bytes(b) => {
                let take = if long {
                    b.len().min(HEX_KEEP_BYTES - used)
                } else {
                    b.len()
                };
                parts.push(hexs(&b[..take]));
                used += take;
            }
            Piece::Text(t, n) => {
                parts.push(t.clone());
                used += n;
            }
        }
    }
    let mut s = parts.join(" ");
    if long {
        s.push_str(&format!(" … <{total} bytes>"));
    }
    s
}

fn redact_hex(s: &str, ctx: &RedactContext) -> String {
    let mut out = String::with_capacity(s.len());
    let mut at = 0;
    for r in &hex_runs(s) {
        let rep = mask_security(r, before_run(s, r))
            .or_else(|| hex_pieces(&r.bytes, ctx).map(|p| render_pieces(&p, r.bytes.len())))
            .or_else(|| {
                (r.bytes.len() > HEX_MAX_BYTES).then(|| {
                    format!(
                        "{} … <{} bytes>",
                        hexs(&r.bytes[..HEX_KEEP_BYTES]),
                        r.bytes.len()
                    )
                })
            });
        if let Some(rep) = rep {
            out.push_str(&s[at..r.start]);
            out.push_str(&rep);
            at = r.end;
        }
    }
    out.push_str(&s[at..]);
    out
}

// ---------------------------------------------------------------------------
// Tests: a corpus with synthetic data only
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Identifiers are assembled at run time so this file stays clean under
    // the privacy scanner. "ZZZ" is not an assigned manufacturer code.
    fn vin() -> String {
        ["ZZZ", "7AB", "2CD", "3E", "5", "91", "746"].concat()
    }

    fn ctx() -> RedactContext {
        RedactContext {
            home: Some("/home/tuner".into()),
            user: Some("tuner".into()),
            host: Some("garagebox".into()),
            secrets: vec!["tok_synthetic_0123456789".into()],
        }
    }

    fn r(s: &str) -> String {
        redact(s, &ctx())
    }

    #[test]
    fn vins_in_text_hex_and_ascii_dumps() {
        let v = vin();
        assert_eq!(r(&format!("VIN {v} read")), "VIN <VIN> read");
        assert_eq!(r(&format!("vin={v};")), "vin=<VIN>;");
        // Synthetic-looking VINs are redacted too.
        let syn = ["WVWZZZ", "1KZ", "AW", "000001"].concat();
        assert_eq!(r(&syn), VIN);
        // A 0x22 F190 response in spaced and contiguous hex.
        let hex = hexs(v.as_bytes());
        let out = r(&format!("[7E8] <- 62 F1 90 {hex}  (9 ms)"));
        assert_eq!(out, "[7E8] <- 62 F1 90 \"<VIN>\"  (9 ms)");
        let contiguous: String = v.bytes().map(|c| format!("{c:02x}")).collect();
        let out = r(&format!("rx 62F190{contiguous}"));
        assert!(!out.contains(&contiguous) && out.contains("<VIN>"), "{out}");
        // Inside a long response: redacted, then truncated.
        let mut long = vec![0x62, 0xF1, 0x90];
        long.extend_from_slice(v.as_bytes());
        long.extend(std::iter::repeat(0xAA).take(80));
        let out = r(&hexs(&long));
        assert!(out.starts_with("62 F1 90 \"<VIN>\""), "{out}");
        assert!(out.ends_with(&format!("… <{} bytes>", long.len())), "{out}");
        for w in v.as_bytes().windows(4) {
            assert!(!out.contains(&hexs(w)), "VIN bytes leaked: {out}");
        }
        // An ASCII column of a hex dump.
        assert!(!r(&format!("0000: 57 56 |{v}|")).contains(&v));
    }

    #[test]
    fn part_numbers() {
        let cases = [
            "03L 906 018 AJ",
            "03L906018AJ",
            "03G906021",
            "8E0 910 557 C",
            "0 281 011 402",
            "0281011402",
            "1037382088",
            "1039S12345",
            "5WP40123",
            "5WK96789",
            "A 642 150 07 79",
            "A6421500779",
        ];
        for p in cases {
            let out = r(&format!("ECU {p} found"));
            if p == "1039S12345" {
                // Not a 10-digit number: left alone.
                assert_eq!(out, format!("ECU {p} found"));
                continue;
            }
            assert_eq!(out, "ECU <PART> found", "{p}");
        }
        // As ASCII inside a 0x22 F187 response.
        let part = ["03L", "906", "018"].concat();
        let out = r(&format!("<- 62 F1 87 {}", hexs(part.as_bytes())));
        assert_eq!(out, "<- 62 F1 87 \"<PART>\"");
        // Not part numbers.
        for s in [
            "took 100 200 300 ms",
            "version 0.7.0",
            "2026-11-19 15:04:05",
            "count 123456789",
            "frame 7E0 7E8",
        ] {
            assert_eq!(r(s), s, "{s}");
        }
    }

    #[test]
    fn emails_serials_secrets() {
        let mail = ["someone", "@", "mailhost", ".no"].concat();
        assert_eq!(r(&format!("from {mail}.")), "from <EMAIL>.");
        assert_eq!(r("see dev@example.com"), "see dev@example.com");
        assert_eq!(r("S/N: AB123456 ok"), "S/N: <SERIAL> ok");
        assert_eq!(r("serial number 9876543210X"), "serial number <SERIAL>");
        assert_eq!(
            r("Authorization failed for tok_synthetic_0123456789"),
            "Authorization failed for <SECRET>"
        );
    }

    #[test]
    fn paths_users_hosts() {
        assert_eq!(
            r("cannot open /home/tuner/cars/golf/stock.bin: denied"),
            "cannot open <PATH>/stock.bin: denied"
        );
        assert_eq!(
            r("panicked at /home/tuner/.cargo/registry/src/x/lib.rs:12:5"),
            "panicked at <PATH>/lib.rs:12:5"
        );
        assert_eq!(r("path=/var/tmp/x.log"), "path=<PATH>/x.log");
        assert_eq!(
            r(r"open C:\Users\Tuner\Desktop\ecu.bin"),
            r"open <PATH>\ecu.bin"
        );
        assert_eq!(r(r"share \\nas\dumps\a.bin"), r"share <PATH>\a.bin");
        assert_eq!(r("file:///home/tuner/a/b.bin"), "<PATH>/b.bin");
        assert_eq!(r("home is /home/tuner"), "home is <PATH>/<USER>");
        assert_eq!(r("user tuner on garagebox"), "user <USER> on <HOST>");
        assert_eq!(r("GARAGEBOX.local up"), "<HOST>.local up");
        // Relative paths, URLs and units stay.
        for s in [
            "src/app-core.js:12:3",
            "tauri://localhost/src/app.js",
            "120 km/h",
            "a/b",
            "/tmp",
        ] {
            assert_eq!(r(s), s, "{s}");
        }
        // Not inside a marker, not inside another word.
        assert_eq!(
            replace_word("<USER> user users", "user", USER),
            "<USER> <USER> users"
        );
    }

    #[test]
    fn network_addresses() {
        assert_eq!(r("adapter at 192.168.0.10:35000"), "adapter at <IP>:35000");
        assert_eq!(r("dns 8.8.8.8."), "dns <IP>.");
        assert_eq!(r("fe80::1%eth0 up"), "<IP> up");
        assert_eq!(r("[2001:db8::42]:13400"), "[<IP>]:13400");
        assert_eq!(r("mac 02:00:5E:10:00:01 seen"), "mac <MAC> seen");
        assert_eq!(r("mac 02-00-5E-10-00-01"), "mac <MAC>");
        for s in [
            "at 15:04:05.123",
            "version 1.2.3",
            "build 10.0.19045.1",
            "std::panic::catch_unwind",
            "Value::String",
        ] {
            assert_eq!(r(s), s, "{s}");
        }
    }

    #[test]
    fn security_access_and_long_payloads() {
        assert_eq!(
            r("[7E0] -> 27 01  <- 67 01 1A 2B 3C 4D  (12 ms)"),
            "[7E0] -> 27 01  <- 67 01 <masked: 4 bytes>  (12 ms)"
        );
        assert_eq!(r("key=DEADBEEF ok"), "key=<masked: 4 bytes> ok");
        let long: Vec<u8> = (0..100u8).map(|x| x | 0x80).collect();
        assert_eq!(
            r(&hexs(&long)),
            format!("{} … <100 bytes>", hexs(&long[..16]))
        );
        assert_eq!(
            mask_secrets("06 27 02 aa bb cc dd"),
            "06 27 02 <masked: 4 bytes>"
        );
    }

    #[test]
    fn json_values_and_keys() {
        let v = vin();
        let mut j = serde_json::json!({
            "msg": format!("read {v}"),
            "ctx": { "path": "/home/tuner/x/y.bin", "n": 3 },
            "list": ["192.168.1.2", true],
        });
        j["ctx"][v.as_str()] = serde_json::json!(1);
        redact_json(&mut j, &ctx());
        let s = j.to_string();
        assert!(
            !s.contains(&v) && !s.contains("tuner") && !s.contains("192.168"),
            "{s}"
        );
        assert_eq!(j["ctx"]["n"], 3);
        assert_eq!(j["ctx"]["path"], "<PATH>/y.bin");
    }

    /// A larger corpus: every line holds one identifier class; after redaction
    /// none of the identifiers survives and the surrounding text does.
    #[test]
    fn corpus_leaves_no_identifier() {
        let v = vin();
        let mail = ["a.person", "@", "mail", "host.no"].concat();
        let secrets: Vec<String> = vec![
            v.clone(),
            hexs(v.as_bytes()),
            mail.clone(),
            "03L906018".into(),
            "0281011402".into(),
            "1037382088".into(),
            "5WP40123".into(),
            "A6421500779".into(),
            "AB123456".into(),
            "tuner".into(),
            "garagebox".into(),
            "10.20.30.40".into(),
            "02:00:5E:10:00:01".into(),
            "1A 2B 3C 4D".into(),
            "tok_synthetic_0123456789".into(),
            "/home/tuner".into(),
        ];
        let lines = [
            format!("panic: VIN mismatch {v} vs <unknown>"),
            format!("[7E0] -> 22 F1 90  <- 62 F1 90 {}", hexs(v.as_bytes())),
            format!("contact {mail} for help"),
            "identified 03L906018 AJ (Bosch 0281011402, sw 1037382088)".into(),
            "TCU 5WP40123 / EGS A6421500779".into(),
            "label S/N: AB123456".into(),
            "cannot read /home/tuner/Documents/tuner-car.bin".into(),
            "connected to elm327 at 10.20.30.40:35000 from garagebox".into(),
            "ble 02:00:5E:10:00:01 rssi -60".into(),
            "-> 27 02 1A 2B 3C 4D".into(),
            "upload failed: 401 for tok_synthetic_0123456789".into(),
        ];
        for line in &lines {
            let out = r(line);
            for sec in &secrets {
                assert!(!out.contains(sec.as_str()), "{sec:?} survived in {out:?}");
            }
        }
        // The text around them is kept.
        assert_eq!(r(&lines[5]), "label S/N: <SERIAL>");
        assert_eq!(r(&lines[3]), "identified <PART> (Bosch <PART>, sw <PART>)");
    }
}
