//! VAG ident blocks: the ASCII record VW-group ECUs carry next to their
//! spare-part number, e.g. `03G906018DK R4 2.0l PPD1.2`,
//! `038906019BJ 1,9l R4 EDC  SG`, `06A906033AT SIMOS72 1.6l 2V` or
//! `06A906032QP … MOTRONIC ME7.5`. The family token inside the record names the
//! controller; the displacement/cylinder tokens give an engine descriptor.

use super::{
    bosch::canon_family, is_print, printable, supplier, Detection, Img, Tier, UNKNOWN_FAMILY,
};

/// One parsed VAG ident block.
#[derive(Debug, Clone)]
pub(crate) struct VagIdent {
    pub offset: usize,
    /// The part number, spaces removed (e.g. "03G906018DK").
    pub part: String,
    pub text: String,
    /// e.g. "2.0l R4", with its offset.
    pub engine: Option<(String, usize)>,
    /// (supplier, family, variant, token offset)
    pub family: Option<(&'static str, &'static str, Option<String>, usize)>,
}

/// Parse `ddd906ddd[AA]` / `ddd907ddd[AA]` part numbers followed by an ident text.
pub(crate) fn ident_blocks(img: &Img) -> Vec<VagIdent> {
    let mut out: Vec<VagIdent> = Vec::new();
    for group in [&b"906"[..], b"907"] {
        for g in img.find_iter(group) {
            if g < 3 {
                continue;
            }
            let s = g - 3;
            let pre = &img.b[s..g];
            if !pre[0].is_ascii_digit()
                || !pre
                    .iter()
                    .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
                || !img.left_bound(s)
                || !img.digits_at(g + 3, 3)
            {
                continue;
            }
            let mut e = g + 6;
            // optional single space before a 1-2 letter index
            let idx_start = if img.byte(e) == Some(b' ')
                && img.byte(e + 1).is_some_and(|c| c.is_ascii_uppercase())
            {
                e + 1
            } else {
                e
            };
            let idx = img.run(idx_start, 3, |c| c.is_ascii_uppercase()).len();
            if idx <= 2 {
                e = idx_start + idx;
            }
            if !img.right_bound(e) {
                continue;
            }
            let part: String = img.b[s..e]
                .iter()
                .filter(|c| **c != b' ')
                .map(|&c| c as char)
                .collect();
            let tail = img.run(e, 56, is_print);
            if tail.len() < 6 {
                continue;
            }
            let upper: Vec<u8> = tail.to_ascii_uppercase();
            let engine = displacement(tail).map(|(t, o)| (t, e + o));
            let family = family_token(img, &upper, e);
            if engine.is_none() && family.is_none() {
                continue;
            }
            let text = printable(&img.b[s..e + tail.len()]).trim_end().to_string();
            if out.iter().any(|v| v.text == text) {
                continue;
            }
            out.push(VagIdent {
                offset: s,
                part,
                text,
                engine,
                family,
            });
        }
    }
    out.sort_by_key(|v| v.offset);
    out
}

/// "1,9l"/"2.0l"/"1.6 l" (+ an adjacent "R4"/"V6"/"W8" token) → ("1.9l R4", offset).
fn displacement(tail: &[u8]) -> Option<(String, usize)> {
    for i in 0..tail.len().saturating_sub(3) {
        let w = &tail[i..];
        if w[0].is_ascii_digit()
            && (w[1] == b',' || w[1] == b'.')
            && w[2].is_ascii_digit()
            && (i == 0 || !tail[i - 1].is_ascii_alphanumeric())
        {
            let l = if w.get(3) == Some(&b' ') { 4 } else { 3 };
            if !matches!(w.get(l), Some(b'l') | Some(b'L')) {
                continue;
            }
            let mut s = format!("{}.{}l", w[0] as char, w[2] as char);
            // cylinder layout token right before or after
            let words: Vec<&[u8]> = tail
                .split(|&c| c == b' ')
                .filter(|x| !x.is_empty())
                .collect();
            if let Some(cyl) = words.iter().find(|x| {
                x.len() >= 2
                    && matches!(x[0], b'R' | b'V' | b'W')
                    && x[1].is_ascii_digit()
                    && (x.len() == 2 || x[2] == b'/')
            }) {
                s.push(' ');
                s.push_str(&String::from_utf8_lossy(cyl));
            }
            return Some((s, i));
        }
    }
    None
}

type FamilyHit = (&'static str, &'static str, Option<String>, usize);

fn family_token(img: &Img, upper: &[u8], base: usize) -> Option<FamilyHit> {
    let find = |p: &[u8]| memchr::memmem::find(upper, p);
    let tok_at = |o: usize| -> String {
        String::from_utf8_lossy(&img.b[base + o..])
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '.')
            .collect()
    };
    if let Some(o) = find(b"PPD1.") {
        return Some((supplier::CONTINENTAL, "PPD", Some(tok_at(o)), base + o));
    }
    if let Some(o) = find(b"SIMOS") {
        return Some((supplier::CONTINENTAL, "SIMOS", Some(tok_at(o)), base + o));
    }
    for p in [
        &b"MED17"[..],
        b"MEDC17",
        b"EDC17",
        b"EDC16",
        b"MED9",
        b"ME7.",
        b"ME9",
    ] {
        if let Some(o) = find(p) {
            let t = tok_at(o);
            if let Some((fam, var)) = canon_family(&t) {
                return Some((supplier::BOSCH, fam, var.or(Some(t)), base + o));
            }
        }
    }
    if let Some(o) = find(b" EDC ") {
        return Some((supplier::BOSCH, "EDC", None, base + o + 1));
    }
    None
}

pub(crate) fn detect(img: &Img, out: &mut Vec<Detection>) {
    for v in ident_blocks(img).into_iter().take(3) {
        let Some((sup, fam, variant, _token_offset)) = v.family.clone() else {
            continue;
        };
        let ev_text = Some(v.text.clone());
        let d = if fam == "EDC" {
            // The record says "EDC" but not which generation; only a full
            // 512 KiB image narrows it to EDC15 (EDC 3.x images are smaller).
            let n = img.len();
            if (0x7_F000..=0x8_0100).contains(&n) {
                Detection::new(sup, "EDC15", Tier::Medium, "vag-ident").ev(
                    "VAG ident block naming EDC (512 KiB image ⇒ EDC15 generation)",
                    Some(v.offset),
                    ev_text,
                )
            } else {
                Detection::new(sup, UNKNOWN_FAMILY, Tier::Low, "vag-ident").ev(
                    "VAG ident block naming EDC (generation not settled by size)",
                    Some(v.offset),
                    ev_text,
                )
            }
        } else {
            Detection::new(sup, fam, Tier::High, "vag-ident")
                .variant(variant)
                .ev("VAG ident block with family token", Some(v.offset), ev_text)
        };
        out.push(d);
    }
}
