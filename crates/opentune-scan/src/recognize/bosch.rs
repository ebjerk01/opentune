//! Bosch detectors: project descriptors, EDC17/MED17-generation markers, ME7,
//! ME9/MED9, EDC16, EDC15 and supplier-only fall-backs.
//!
//! ME7 (`ZZ` block at 0x10000 with a non-printable third byte), ME9 (RAM-loader
//! anchor) and the EDC16 `DECAFE` offset table follow the layout facts
//! documented by openremap-core (MIT, see `NOTICE`); the descriptor parser and
//! the tiering are OpenTune's own.

use super::{
    find_bosch_number, is_print, is_token_byte, printable, supplier, Detection, Img, Tier,
    UNKNOWN_FAMILY,
};

const B: &str = supplier::BOSCH;

pub(crate) fn detect(img: &Img, out: &mut Vec<Detection>) {
    descriptor(img, out);
    edc17_generation(img, out);
    me7(img, out);
    me9(img, out);
    edc16(img, out);
    edc15(img, out);
    supplier_only(img, out);
}

// ---------------------------------------------------------------------------
// Project descriptor: "<rev>/1/<FAMILY>/<…>/<dataset>//<project>/…"
// ---------------------------------------------------------------------------

/// One parsed Bosch project descriptor.
#[derive(Debug, Clone)]
pub(crate) struct BoschDescriptor {
    pub offset: usize,
    pub text: String,
    pub family: &'static str,
    pub variant: Option<String>,
    /// Slash-separated fields (field 0 = revision, 1 = "1", 2 = family token).
    pub fields: Vec<String>,
    /// Byte offset of each field.
    pub field_offsets: Vec<usize>,
}

/// Map a descriptor/literal family token to (canonical family, variant).
pub(crate) fn canon_family(token: &str) -> Option<(&'static str, Option<String>)> {
    let t = token.to_ascii_uppercase();
    let t = t.trim_end_matches('.');
    let fam: &'static str = if t.starts_with("MEDC17") {
        "MEDC17"
    } else if t.starts_with("MEVD17") {
        "MEVD17"
    } else if t.starts_with("MED17") {
        "MED17"
    } else if t.starts_with("EDC17") {
        "EDC17"
    } else if t.starts_with("ME17") || t == "VM17" {
        "ME17"
    } else if t.starts_with("MED9") {
        "MED9"
    } else if t.starts_with("ME9") {
        "ME9"
    } else if t.starts_with("ME7") {
        "ME7"
    } else if t.starts_with("ME1.5") || t.starts_with("ME155") {
        "ME1.5.5"
    } else if t.starts_with("EDC16") {
        "EDC16"
    } else if t.starts_with("EDC15") {
        "EDC15"
    } else if t.starts_with("MD1C") {
        "MD1"
    } else if t.starts_with("MG1C") {
        "MG1"
    } else if t.starts_with("M7.") || t.starts_with("M79") || t.starts_with("M78") {
        "M7"
    } else if let Some(rest) = t.strip_prefix("MP") {
        match rest.as_bytes().first() {
            Some(b'3') => "MP3",
            Some(b'7') => "MP7",
            Some(b'9') => "MP9",
            _ => return None,
        }
    } else if t.len() >= 4 && t.as_bytes()[0] == b'M' && t.as_bytes()[2] == b'.' {
        match t.as_bytes()[1] {
            b'1' => "M1.x",
            b'2' => "M2.x",
            b'3' => "M3.x",
            b'4' => "M4.x",
            b'5' => "M5.x",
            _ => return None,
        }
    } else {
        return None;
    };
    let variant = if fam == "EDC17" || fam == "EDC16" || fam == "MED17" {
        // "EDC17_CP14" → "EDC17CP14"; "EDC17C49_C" → "EDC17C49"; "MED170_PBL" → "MED170"
        let mut v = t.replacen("EDC17_", "EDC17", 1);
        if let Some(i) = v.find('_') {
            v.truncate(i);
        }
        Some(v)
    } else if t == "VM17" {
        None
    } else {
        Some(t.to_string())
    };
    Some((fam, variant.filter(|v| v != fam)))
}

/// Every Bosch project descriptor in the image (deduplicated by text).
pub(crate) fn descriptors(img: &Img) -> Vec<BoschDescriptor> {
    let mut out: Vec<BoschDescriptor> = Vec::new();
    for pos in img.find_iter(b"/1/") {
        // 2-3 digit revision before "/1/"
        let mut s = pos;
        while s > 0 && pos - s < 4 && img.b[s - 1].is_ascii_digit() {
            s -= 1;
        }
        let nd = pos - s;
        if !(2..=3).contains(&nd)
            || (s > 0 && (img.b[s - 1].is_ascii_alphanumeric() || img.b[s - 1] == b'/'))
        {
            continue;
        }
        let tok = img.run(pos + 3, 16, is_token_byte);
        if tok.len() < 2 || img.byte(pos + 3 + tok.len()) != Some(b'/') {
            continue;
        }
        let Some((family, mut variant)) = canon_family(&String::from_utf8_lossy(tok)) else {
            continue;
        };
        let raw = img.run(s, 128, is_print);
        if raw.iter().filter(|&&c| c == b'/').count() < 4 {
            continue;
        }
        let text = printable(raw);
        if out.iter().any(|d| d.text == text) {
            continue;
        }
        let mut fields = Vec::new();
        let mut field_offsets = Vec::new();
        let mut off = s;
        for f in raw.split(|&c| c == b'/') {
            fields.push(String::from_utf8_lossy(f).trim().to_string());
            field_offsets.push(off);
            off += f.len() + 1;
        }
        // "53/1/VM17/147/ME17.9.11//…", "45/1/MED17/5/MED17.5//…": the precise
        // variant is a later field.
        if variant.is_none() {
            let want = format!("{family}.");
            variant = fields
                .iter()
                .skip(3)
                .find(|f| f.to_ascii_uppercase().starts_with(&want))
                .cloned();
        }
        out.push(BoschDescriptor {
            offset: s,
            text,
            family,
            variant,
            fields,
            field_offsets,
        });
    }
    out
}

fn descriptor(img: &Img, out: &mut Vec<Detection>) {
    for d in descriptors(img).into_iter().take(4) {
        out.push(
            Detection::new(B, d.family, Tier::High, "bosch-descriptor")
                .variant(d.variant)
                .ev("Bosch project descriptor", Some(d.offset), Some(d.text)),
        );
    }
}

// ---------------------------------------------------------------------------
// EDC17 / MEDC17 / MED17 / MEVD17 / ME17 / MD1 / MG1 generation
// ---------------------------------------------------------------------------

/// Generation literals, most specific first (a tie in count goes to the earlier one).
const GEN17: &[(&[u8], &str)] = &[
    (b"MEDC17", "MEDC17"),
    (b"MEVD17", "MEVD17"),
    (b"MED17", "MED17"),
    (b"EDC17", "EDC17"),
    (b"ME17", "ME17"),
    (b"MD1C", "MD1"),
    (b"MG1C", "MG1"),
];

fn edc17_generation(img: &Img, out: &mut Vec<Detection>) {
    // Supporting generation markers.
    let sbv = img
        .find_iter(b"SB_V")
        .find(|&o| img.byte(o + 4).is_some_and(|c| c.is_ascii_digit()));
    let cust = img.find(b"Customer.");
    let serial = img
        .find_iter(b"NR")
        .find(|&o| o >= 8 && img.digits_at(o - 8, 8) && img.digits_at(o + 2, 7));

    // Family literals: not glued to a preceding capital (so "EDC17" inside
    // "MEDC17" is skipped), except ME17's "VME17 SB_V…" header form; and the
    // next byte must be a token/terminator byte, not a lowercase letter.
    let mut best: Option<(&'static str, usize, usize, String)> = None; // fam, count, first, token
    for &(pat, fam) in GEN17 {
        let mut count = 0;
        let mut first = None;
        let mut token = String::new();
        for o in img.find_iter(pat) {
            let prev = if o == 0 { 0 } else { img.b[o - 1] };
            let glued = prev.is_ascii_uppercase() || prev.is_ascii_digit();
            let vme17 = fam == "ME17" && prev == b'V' && img.at(o + 4, b" SB_V");
            if glued && !vme17 {
                continue;
            }
            let next = img.byte(o + pat.len()).unwrap_or(0);
            if !(next == 0 || next == b' ' || next == b'/' || is_token_byte(next))
                || next.is_ascii_lowercase()
            {
                continue;
            }
            count += 1;
            if first.is_none() {
                first = Some(o);
                token = String::from_utf8_lossy(img.run(o, 24, is_token_byte)).into_owned();
            }
        }
        if let Some(f) = first {
            if best.as_ref().map_or(true, |b| count > b.1) {
                best = Some((fam, count, f, token));
            }
        }
    }

    let support = sbv.is_some() || cust.is_some() || serial.is_some();
    let add_support = |mut d: Detection| {
        if let Some(o) = sbv {
            d = d.ev(
                "Bosch SW base version (SB_V)",
                Some(o),
                Some(img.snippet(o, 24)),
            );
        }
        if let Some(o) = cust {
            d = d.ev("Bosch customer label", Some(o), Some(img.snippet(o, 24)));
        }
        if let Some(o) = serial {
            d = d.ev(
                "Bosch production serial (YYYYMMDDNR…)",
                Some(o - 8),
                Some(img.snippet(o - 8, 20)),
            );
        }
        d
    };
    match best {
        Some((fam, count, first, token)) => {
            let variant = canon_family(&token).and_then(|(_, v)| v);
            let tier = if support { Tier::High } else { Tier::Medium };
            let d = Detection::new(B, fam, tier, "bosch-17-literal")
                .variant(variant)
                .ev(
                    format!("{fam} family literal ({count}x)"),
                    Some(first),
                    Some(token),
                );
            out.push(add_support(d));
        }
        None if sbv.is_some() && (cust.is_some() || serial.is_some()) => {
            let d = Detection::new(B, UNKNOWN_FAMILY, Tier::Low, "bosch-17-literal").ev(
                "EDC17/MED17-generation markers without a family literal",
                None,
                None,
            );
            out.push(add_support(d));
        }
        None => {}
    }
}

// ---------------------------------------------------------------------------
// ME7.x
// ---------------------------------------------------------------------------

const ME7_ZZ: usize = 0x1_0000;

fn me7(img: &Img, out: &mut Vec<Detection>) {
    if img.len() < ME7_ZZ {
        return;
    }
    // Later generations never carry ME7 markers; their presence means a
    // coincidental "ME7"/"ZZ" (openremap-core uses the same exclusions).
    for x in [
        &b"EDC17"[..],
        b"MED17",
        b"MEDC17",
        b"EDC16",
        b"SB_V",
        b"Customer.",
    ] {
        if img.contains(x) {
            return;
        }
    }
    let zz = img.at(ME7_ZZ, b"ZZ") && img.byte(ME7_ZZ + 2).is_some_and(|c| !is_print(c));
    let lit = [&b"ME7."[..], b"ME71", b"ME731", b"ME 7."]
        .iter()
        .filter_map(|p| img.literal_iter(p).next())
        .min();
    let label = img.find_iter(b"MOTRONIC").find(|&o| {
        let sp = img.run(o + 8, 6, |c| c == b' ').len();
        sp > 0 && img.at(o + 8 + sp, b"ME7")
    });
    if !(zz || lit.is_some() || label.is_some()) {
        return;
    }
    let tier = match (zz, lit.is_some() || label.is_some()) {
        (true, true) => Tier::High,
        (false, true) => Tier::Medium,
        _ => Tier::Low,
    };
    let mut d = Detection::new(B, "ME7", tier, "bosch-me7");
    if let Some(o) = lit {
        let tok =
            String::from_utf8_lossy(img.run(o, 12, |c| c.is_ascii_alphanumeric() || c == b'.'));
        d = d
            .variant(Some(tok.trim_end_matches('.').replace("ME 7.", "ME7.")))
            .ev("ME7 family literal", Some(o), Some(tok.into_owned()));
    }
    if let Some(o) = label {
        d = d.ev("MOTRONIC ME7 label", Some(o), Some(img.snippet(o, 24)));
    }
    if zz {
        d = d.ev(
            "ZZ ident block at 0x10000 (non-printable 3rd byte, ME7 layout)",
            Some(ME7_ZZ),
            Some(format!("{:02X?}", &img.b[ME7_ZZ..ME7_ZZ + 4])),
        );
    }
    out.push(d);
}

// ---------------------------------------------------------------------------
// ME9 / MED9
// ---------------------------------------------------------------------------

fn me9(img: &Img, out: &mut Vec<Detection>) {
    let anchor = img.find_in(b"Bosch.Common.RamLoader.Me9", 0, 0x20_0000);
    let med9 = img.literal_iter(b"MED9").find(|&o| {
        img.byte(o + 4)
            .is_some_and(|c| c.is_ascii_digit() || c == b'.' || c == b'/')
    });
    match (anchor, med9) {
        (None, None) => {}
        (a, Some(m)) => {
            let tok = String::from_utf8_lossy(img.run(m, 12, is_token_byte)).into_owned();
            let mut d = Detection::new(
                B,
                "MED9",
                if a.is_some() {
                    Tier::High
                } else {
                    Tier::Medium
                },
                "bosch-me9",
            )
            .variant(Some(tok.clone()))
            .ev("MED9 family literal", Some(m), Some(tok));
            if let Some(a) = a {
                d = d.ev(
                    "ME9/MED9 RAM-loader anchor",
                    Some(a),
                    Some(img.snippet(a, 32)),
                );
            }
            out.push(d);
        }
        (Some(a), None) => {
            // Anchor alone: ME9. A Bosch 0261 HW + 1037 SW pair raises it.
            let hw = find_bosch_number(img, b"0261", 0, 0xA_0000);
            let sw = find_bosch_number(img, b"1037", 0, 0xA_0000);
            let tier = if hw.is_some() && sw.is_some() {
                Tier::High
            } else {
                Tier::Medium
            };
            let mut d = Detection::new(B, "ME9", tier, "bosch-me9").ev(
                "ME9 RAM-loader anchor (no MED9 literal)",
                Some(a),
                Some(img.snippet(a, 32)),
            );
            if let (Some(h), Some(s)) = (hw, sw) {
                d = d
                    .ev("Bosch HW number", Some(h), Some(img.snippet(h, 10)))
                    .ev("Bosch SW number", Some(s), Some(img.snippet(s, 10)));
            }
            out.push(d);
        }
    }
}

// ---------------------------------------------------------------------------
// EDC16
// ---------------------------------------------------------------------------

/// (nominal size, active-section starts) — DECAFE sits at start+0x3D and the
/// SW number at start+0x10 (layout documented by openremap-core).
const EDC16_LAYOUTS: &[(usize, &[usize])] = &[
    (
        0x10_0000,
        &[0x4_0000, 0xC_0000, 0xD_0000, 0xE_0000, 0x2_0000, 0x8_0000],
    ),
    (0x20_0000, &[0x4_0000, 0xC_0000, 0x1C_0000]),
    (0x8_0000, &[0x0]),
    (0x4_0000, &[0x0]),
    (0xF_0000, &[0x3_0000]),
];

fn edc16(img: &Img, out: &mut Vec<Detection>) {
    let n = img.len();
    let starts = EDC16_LAYOUTS
        .iter()
        .find(|(size, _)| *size >= n && size - n <= 0x1000)
        .map(|(_, s)| *s)
        .unwrap_or(&[]);
    let mut magic = None;
    let mut sw = None;
    for &st in starts {
        if img.at(st + 0x3D, b"\xDE\xCA\xFE") {
            magic.get_or_insert(st);
            let swo = st + 0x10;
            let ok = (img.at(swo, b"1037") || img.at(swo, b"1039"))
                && img
                    .b
                    .get(swo + 4..swo + 10)
                    .is_some_and(|s| s.iter().all(|c| c.is_ascii_hexdigit()));
            if ok && sw.is_none() {
                sw = Some(swo);
            }
        }
    }
    let lit = img.literal_iter(b"EDC16").find(|&o| {
        img.run(o, 16, |c| c.is_ascii_alphanumeric()).len() >= 6
            && img.byte(o + img.run(o, 16, |c| c.is_ascii_alphanumeric()).len()) == Some(b'/')
    });
    if magic.is_none() && lit.is_none() {
        return;
    }
    let tier = match (magic.is_some(), sw.is_some() || lit.is_some()) {
        (true, true) => Tier::High,
        _ => Tier::Medium,
    };
    let mut d = Detection::new(B, "EDC16", tier, "bosch-edc16");
    if let Some(st) = magic {
        d = d.ev(
            format!("DECAFE header magic at active-section start 0x{st:X} + 0x3D"),
            Some(st + 0x3D),
            Some("DE CA FE".into()),
        );
    }
    if let Some(o) = sw {
        d = d.ev(
            "SW number at active-section start + 0x10",
            Some(o),
            Some(img.snippet(o, 10)),
        );
    }
    if let Some(o) = lit {
        let tok =
            String::from_utf8_lossy(img.run(o, 16, |c| c.is_ascii_alphanumeric())).into_owned();
        d = d.variant(Some(tok.clone())).ev(
            "EDC16 family string",
            Some(o),
            Some(img.snippet(o, 40)),
        );
    }
    out.push(d);
}

// ---------------------------------------------------------------------------
// EDC15
// ---------------------------------------------------------------------------

/// Parse a Bosch `TSW Vx.yy DDMMYY CCCC token` toolchain line at `o`.
fn tsw_line(img: &Img, o: usize) -> Option<(usize, String)> {
    let mut p = o + 5; // after "TSW V"
    let ver = img.run(p, 6, |c| c.is_ascii_digit() || c == b'.');
    if ver.len() < 3 || !ver.contains(&b'.') {
        return None;
    }
    p += ver.len();
    let sep = |p: usize| img.run(p, 4, |c| c == b' ' || c == 0).len();
    let s = sep(p);
    if s == 0 || !img.digits_at(p + s, 6) {
        return None;
    }
    p += s + 6;
    let s = sep(p);
    let code = img.run(p + s, 5, |c| c.is_ascii_alphanumeric());
    if s == 0 || !(3..=4).contains(&code.len()) {
        return None;
    }
    p += s + code.len();
    let s = sep(p);
    let tok = img.run(p + s, 24, |c| {
        c.is_ascii_alphanumeric() || c == b'/' || c == b'+'
    });
    if s == 0 || tok.len() < 2 {
        return None;
    }
    Some((p + s, String::from_utf8_lossy(tok).into_owned()))
}

fn edc15_variant(tok: &str) -> Option<String> {
    if tok.starts_with("EDC15") {
        return Some(tok.split('/').next().unwrap_or(tok).to_string());
    }
    let first = tok.split('/').next()?;
    let b = first.as_bytes();
    (b.len() == 2 && b[0] == b'C' && b[1].is_ascii_digit()).then(|| format!("EDC15{first}"))
}

fn edc15(img: &Img, out: &mut Vec<Detection>) {
    for x in [
        &b"EDC16"[..],
        b"EDC17",
        b"MEDC17",
        b"MED17",
        b"SB_V",
        b"Customer.",
        b"ME7.",
        b"PPD1.",
        b"SIMOS",
    ] {
        if img.contains(x) {
            return;
        }
    }
    let n = img.len();
    let c3 = img.b.iter().filter(|&&c| c == 0xC3).count();
    let c3_pct = c3 * 100 / n.max(1);

    if let Some((o, tok)) = img
        .find_iter(b"TSW V")
        .find_map(|o| tsw_line(img, o).map(|(_, s)| (o, s)))
    {
        out.push(
            Detection::new(B, "EDC15", Tier::High, "bosch-edc15")
                .variant(edc15_variant(&tok))
                .ev(
                    "Bosch TSW toolchain line (EDC15 generation)",
                    Some(o),
                    Some(img.snippet(o, 48)),
                )
                .ev(format!("0xC3 fill = {c3_pct}% of image"), None, None),
        );
        return;
    }
    // C167 flash bootstrap header ("PP22..00" after a "UU" preamble).
    let pp22 = [4usize, 0x8004, 0x7_8004]
        .into_iter()
        .find(|&o| img.at(o, b"PP22..00"));
    if let Some(o) = pp22 {
        if let Some(h) = find_bosch_number(img, b"0281", 0, n) {
            out.push(
                Detection::new(B, "EDC15", Tier::Medium, "bosch-edc15")
                    .ev(
                        "C167 flash bootstrap header PP22..00",
                        Some(o),
                        Some(img.snippet(o, 8)),
                    )
                    .ev(
                        "Bosch diesel HW number 0281…",
                        Some(h),
                        Some(img.snippet(h, 10)),
                    )
                    .ev(format!("0xC3 fill = {c3_pct}% of image"), None, None),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Supplier-only and containers
// ---------------------------------------------------------------------------

fn supplier_only(img: &Img, out: &mut Vec<Detection>) {
    let hw = find_bosch_number(img, b"0261", 0, img.len())
        .map(|o| (o, "Bosch petrol HW number 0261…"))
        .or_else(|| {
            find_bosch_number(img, b"0281", 0, img.len())
                .map(|o| (o, "Bosch diesel HW number 0281…"))
        });
    let sw = find_bosch_number(img, b"1037", 0, img.len());
    let name = img.find(b"BOSCH").or_else(|| img.find(b"Bosch"));
    let label = img.find_iter(b"MOTRONIC").find(|&o| img.left_bound(o));
    if hw.is_none() && name.is_none() && label.is_none() {
        return;
    }
    // A HW number alone is a bare 10-digit run; require two facts.
    let facts =
        hw.is_some() as u8 + sw.is_some() as u8 + name.is_some() as u8 + label.is_some() as u8;
    if facts < 2 {
        return;
    }
    let mut d = Detection::new(B, UNKNOWN_FAMILY, Tier::Low, "bosch-supplier");
    if let Some((o, what)) = hw {
        d = d.ev(what, Some(o), Some(img.snippet(o, 10)));
    }
    if let Some(o) = sw {
        d = d.ev("Bosch SW number 1037…", Some(o), Some(img.snippet(o, 10)));
    }
    if let Some(o) = name {
        d = d.ev("Bosch name string", Some(o), Some(img.snippet(o, 16)));
    }
    if let Some(o) = label {
        d = d.ev("MOTRONIC label", Some(o), Some(img.snippet(o, 24)));
    }
    out.push(d);
}

/// SMS-Soft containers start with "SMS-Soft" + an 8-byte tag naming the ECU
/// (e.g. "BschM797", "BschME17"); the payload itself is packed.
pub(crate) fn sms_soft(img: &Img, out: &mut Vec<Detection>) {
    let tag = String::from_utf8_lossy(img.run(8, 8, is_print)).into_owned();
    let Some(rest) = tag.strip_prefix("Bsch") else {
        return;
    };
    let rest = rest.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
    let fam = match rest {
        r if r.starts_with("ME17") => Some(("ME17", None)),
        r if r.starts_with("M79") || r.starts_with("M7") => Some(("M7", Some(r.to_string()))),
        r => canon_family(r),
    };
    if let Some((fam, variant)) = fam {
        out.push(
            Detection::new(B, fam, Tier::Low, "sms-soft-header")
                .variant(variant)
                .ev(
                    "SMS-Soft container header tag (payload packed, not inspected)",
                    Some(8),
                    Some(tag),
                ),
        );
    }
}
