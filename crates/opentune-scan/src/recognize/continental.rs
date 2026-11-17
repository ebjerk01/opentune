//! Siemens / Siemens VDO / Continental detectors: Simos, SID80x/SID20x, PPD,
//! MS4x, Simtec and supplier-only part-number prefixes.
//!
//! The family literals and part prefixes (`5WP4` Simos, `5WS4…-T` SID,
//! `5WK9…--…MS4x` ident record, `PPD1.x`, `CASN`/`111SN`, `CAPM3`/`111PO`)
//! follow openremap-core's documented signatures (MIT, see `NOTICE`); the
//! tiering and the `6577` serial cross-check are OpenTune's own.

use super::{is_print, supplier, Detection, Img, Tier, UNKNOWN_FAMILY};

const C: &str = supplier::CONTINENTAL;

pub(crate) fn detect(img: &Img, out: &mut Vec<Detection>) {
    simos(img, out);
    sid(img, out);
    ppd(img, out);
    cal_header(img, out);
    ms4x(img, out);
    simtec(img, out);
    supplier_only(img, out);
}

/// `prefix` + at least `min` digits, left-bounded. Returns (offset, token).
fn part(img: &Img, prefix: &[u8], min: usize) -> Option<(usize, String)> {
    img.literal_iter(prefix).find_map(|o| {
        let d = img.run(o + prefix.len(), 6, |c| c.is_ascii_digit()).len();
        (d >= min).then(|| {
            let t = img.run(o, 16, |c| c.is_ascii_alphanumeric() || c == b'-');
            (o, String::from_utf8_lossy(t).into_owned())
        })
    })
}

/// Siemens serial-style number `6577dddddd` (seen in Simos, PPD, MS4x, Simtec
/// ident records); only used as corroboration, never on its own.
fn serial_6577(img: &Img, start: usize, end: usize) -> Option<usize> {
    let mut from = start;
    while let Some(o) = img.find_in(b"6577", from, end) {
        if img.digits_at(o + 4, 6) {
            return Some(o);
        }
        from = o + 1;
    }
    None
}

fn bosch_generation_present(img: &Img) -> bool {
    [
        &b"EDC17"[..],
        b"MEDC17",
        b"MED17",
        b"ME7.",
        b"MOTRONIC",
        b"SB_V",
    ]
    .iter()
    .any(|x| img.contains(x))
}

fn simos(img: &Img, out: &mut Vec<Detection>) {
    if bosch_generation_present(img) {
        return;
    }
    let lit = img
        .find_iter(b"SIMOS")
        .find(|&o| img.left_bound(o) || img.b.get(o - 1).is_some_and(|c| c.is_ascii_digit()));
    let pn = part(img, b"5WP4", 3);
    let cas = img
        .find(b"cas21")
        .or_else(|| img.find(b"111s21"))
        .or_else(|| img.find(b"s21_"));
    if lit.is_none() && pn.is_none() {
        return;
    }
    let tier = if lit.is_some() && (pn.is_some() || cas.is_some()) {
        Tier::High
    } else if lit.is_some() || (pn.is_some() && cas.is_some()) {
        Tier::Medium
    } else {
        Tier::Low
    };
    let mut d = Detection::new(C, "SIMOS", tier, "continental-simos");
    if let Some(o) = lit {
        let tok =
            String::from_utf8_lossy(img.run(o, 12, |c| c.is_ascii_alphanumeric() || c == b'.'))
                .into_owned();
        d = d.variant(Some(tok.clone())).ev(
            "SIMOS family literal",
            Some(o),
            Some(img.snippet(o, 24)),
        );
    }
    if let Some((o, t)) = pn {
        d = d.ev("Siemens Simos part number 5WP4…", Some(o), Some(t));
    }
    if let Some(o) = cas {
        d = d.ev(
            "Simos project/dataset tag",
            Some(o),
            Some(img.snippet(o, 16)),
        );
    }
    out.push(d);
}

fn sid(img: &Img, out: &mut Vec<Detection>) {
    if bosch_generation_present(img) {
        return;
    }
    // "SID801", "SID803A", "SID206", … as a whole token.
    let lit = img.literal_iter(b"SID").find(|&o| {
        img.digits_at(o + 3, 3)
            && matches!(img.byte(o + 3), Some(b'2') | Some(b'3') | Some(b'8'))
            && {
                let end = o + 6 + (img.byte(o + 6) == Some(b'A')) as usize;
                img.right_bound(end)
            }
    });
    // "5WS4dddd[X]-T" hardware ident.
    let hw = img.literal_iter(b"5WS4").find_map(|o| {
        let t = img.run(o, 14, |c| c.is_ascii_alphanumeric() || c == b'-');
        let s = String::from_utf8_lossy(t).into_owned();
        (s.ends_with("-T") && s.len() >= 10).then_some((o, s))
    });
    let pm3 = img.find(b"111PM3").or_else(|| img.find(b"CAPM3"));
    let po = img
        .find(b"111PO")
        .and_then(|a| img.find(b"CAPO").map(|_| a));
    if lit.is_none() && hw.is_none() && pm3.is_none() && po.is_none() {
        return;
    }
    let (variant, lit_ev) = match lit {
        Some(o) => {
            let t =
                String::from_utf8_lossy(img.run(o, 7, |c| c.is_ascii_alphanumeric())).into_owned();
            (Some(t.clone()), Some((o, t)))
        }
        None if pm3.is_some() => (Some("SID801".to_string()), None),
        None if po.is_some() => (Some("SID803".to_string()), None),
        None => (None, None),
    };
    let tier = match (lit_ev.is_some(), hw.is_some()) {
        (true, true) => Tier::High,
        (false, false) => Tier::Low,
        _ => Tier::Medium,
    };
    let mut d = Detection::new(C, "SID", tier, "continental-sid").variant(variant);
    if let Some((o, t)) = lit_ev {
        d = d.ev("SID family literal", Some(o), Some(t));
    }
    if let Some((o, t)) = hw {
        d = d.ev(
            "Siemens/Continental SID hardware ident 5WS4…-T",
            Some(o),
            Some(t),
        );
    }
    if let Some(o) = pm3 {
        d = d.ev(
            "SID801 project tag (PM3)",
            Some(o),
            Some(img.snippet(o, 12)),
        );
    }
    if let Some(o) = po {
        d = d.ev("SID803 project tag (PO)", Some(o), Some(img.snippet(o, 12)));
    }
    out.push(d);
}

fn ppd(img: &Img, out: &mut Vec<Detection>) {
    if bosch_generation_present(img) {
        return;
    }
    let lit = img
        .literal_iter(b"PPD1.")
        .find(|&o| img.byte(o + 5).is_some_and(|c| c.is_ascii_digit()));
    let casn = img.find(b"CASN");
    let sn = img.find(b"111SN");
    if lit.is_none() && !(casn.is_some() && sn.is_some()) {
        return;
    }
    let tier = if lit.is_some() && (casn.is_some() || sn.is_some()) {
        Tier::High
    } else {
        Tier::Medium
    };
    let mut d = Detection::new(C, "PPD", tier, "continental-ppd");
    if let Some(o) = lit {
        d = d.variant(Some(img.snippet(o, 6))).ev(
            "PPD family literal",
            Some(o),
            Some(img.snippet(o, 6)),
        );
    }
    if let Some(o) = casn {
        d = d.ev(
            "PPD calibration dataset tag CASN",
            Some(o),
            Some(img.snippet(o, 12)),
        );
    }
    if let Some(o) = sn {
        d = d.ev("PPD project block 111SN", Some(o), Some(img.snippet(o, 12)));
    }
    out.push(d);
}

/// Siemens/Continental calibration header: `CA<pp>…` followed by the project
/// block repeated as `111<pp>…111<pp>…` (`CASN`/`111SN` on PPD, `CAPM3`/
/// `111PM3` on SID801, `CAPO`/`111PO` on SID803, `CASM`/`111SM` on Simos
/// PCR2.1; the same layout with other project letters on SID20x/SID30x).
fn cal_header(img: &Img, out: &mut Vec<Detection>) {
    if bosch_generation_present(img) {
        return;
    }
    for o in img.find_iter(b"111") {
        let pp = match img.b.get(o + 3..o + 5) {
            Some(p) if p.iter().all(u8::is_ascii_uppercase) => [p[0], p[1]],
            _ => continue,
        };
        let proj = img.run(o + 5, 8, |c| c.is_ascii_alphanumeric());
        if proj.len() < 3 || !proj.iter().any(u8::is_ascii_digit) {
            continue;
        }
        let again = img
            .find_in(&[b'1', b'1', b'1', pp[0], pp[1]], o + 5, o + 64)
            .is_some();
        let ca = img.find_in(&[b'C', b'A', pp[0], pp[1]], o.saturating_sub(64), o);
        let Some(ca) = ca.filter(|_| again) else {
            continue;
        };
        let (fam, tier, variant): (&str, Tier, Option<String>) = match &pp {
            // Simos PCR2.1 (seen together with its checksum directory).
            b"SM" => ("PCR2.1", Tier::Medium, None),
            // SN/PM/PO are already reported by `ppd()`/`sid()` from the same bytes.
            b"SN" | b"PM" | b"PO" => return,
            _ => ("SID", Tier::Low, None),
        };
        out.push(
            Detection::new(C, fam, tier, "continental-cal-header")
                .variant(variant)
                .ev(
                    "Siemens/Continental calibration header CA<pp>…/111<pp>… (SID/PPD layout)",
                    Some(ca),
                    Some(img.snippet(ca, 48)),
                ),
        );
        return;
    }
}

fn ms4x(img: &Img, out: &mut Vec<Detection>) {
    for x in [
        &b"EDC17"[..],
        b"MED17",
        b"ME7.",
        b"PPD1.",
        b"SIMOS",
        b"5WS4",
    ] {
        if img.contains(x) {
            return;
        }
    }
    // Ident record "5WK9dddd--dddddddddd" + "MS4d" (MS41/MS42/MS43).
    let record = img.literal_iter(b"5WK9").find_map(|o| {
        let d = img.run(o + 4, 6, |c| c.is_ascii_digit()).len();
        let p = o + 4 + d;
        (d >= 4 && img.at(p, b"--") && img.digits_at(p + 2, 10) && img.at(p + 12, b"MS4"))
            .then_some((o, p + 12))
    });
    if let Some((o, m)) = record {
        let v = img.snippet(m, 4);
        out.push(
            Detection::new(C, "MS4x", Tier::High, "siemens-ms4x")
                .variant(Some(v))
                .ev(
                    "Siemens ident record 5WK9…--…MS4x",
                    Some(o),
                    Some(img.snippet(o, 24)),
                ),
        );
        return;
    }
    // "MS4d" next to a 6577 serial (e.g. MS40's numeric ident block).
    for o in img.find_iter(b"MS4") {
        if !img.byte(o + 3).is_some_and(|c| c.is_ascii_digit()) || !img.digits_at(o + 4, 4) {
            continue;
        }
        let lo = o.saturating_sub(64);
        if let Some(s) = serial_6577(img, lo, o + 64) {
            out.push(
                Detection::new(C, "MS4x", Tier::Medium, "siemens-ms4x")
                    .variant(Some(img.snippet(o, 4)))
                    .ev(
                        "MS4x family tag in a numeric ident block",
                        Some(o),
                        Some(img.snippet(o, 16)),
                    )
                    .ev(
                        "Siemens serial 6577… within 64 bytes",
                        Some(s),
                        Some(img.snippet(s, 10)),
                    ),
            );
            return;
        }
    }
}

fn simtec(img: &Img, out: &mut Vec<Detection>) {
    let Some(o) = img.literal_iter(b"SIMTEC").next() else {
        return;
    };
    // "SIMTEC76", "SIMTEC 56": the digits name the variant.
    let sp = (img.byte(o + 6) == Some(b' ')) as usize;
    let digits = img.run(o + 6 + sp, 3, |c| c.is_ascii_digit());
    let tok = (!digits.is_empty()).then(|| format!("SIMTEC{}", String::from_utf8_lossy(digits)));
    let corro = img
        .find(b"Siemens")
        .or_else(|| img.find(b"SIEMENS"))
        .or_else(|| part(img, b"5WK9", 4).map(|p| p.0))
        .or_else(|| serial_6577(img, 0, img.len()));
    let mut d = Detection::new(
        C,
        "SIMTEC",
        if corro.is_some() {
            Tier::High
        } else {
            Tier::Medium
        },
        "siemens-simtec",
    )
    .variant(tok)
    .ev("SIMTEC family literal", Some(o), Some(img.snippet(o, 16)));
    if let Some(c) = corro {
        d = d.ev(
            "Siemens corroboration (name / 5WK9 / 6577 serial)",
            Some(c),
            Some(img.snippet(c, 16)),
        );
    }
    out.push(d);
}

fn supplier_only(img: &Img, out: &mut Vec<Detection>) {
    let mut d = Detection::new(C, UNKNOWN_FAMILY, Tier::Low, "continental-supplier");
    let mut facts = 0;
    for (pat, what) in [
        (&b"Copyright Siemens"[..], "Siemens copyright string"),
        (b"Siemens VDO", "Siemens VDO name string"),
        (b"Continental Automotive", "Continental name string"),
    ] {
        if let Some(o) = img.find(pat) {
            d = d.ev(
                what,
                Some(o),
                Some(String::from_utf8_lossy(img.run(o, 40, is_print)).into_owned()),
            );
            facts += 2;
        }
    }
    for (pre, what) in [
        (&b"5WK9"[..], "Siemens part number 5WK9…"),
        (b"5WS4", "Siemens part number 5WS4…"),
        (b"5WP4", "Siemens part number 5WP4…"),
    ] {
        if let Some((o, t)) = part(img, pre, 3) {
            d = d.ev(what, Some(o), Some(t));
            facts += 1;
        }
    }
    if let Some(o) = serial_6577(img, 0, img.len()) {
        if facts > 0 {
            d = d.ev("Siemens serial 6577…", Some(o), Some(img.snippet(o, 10)));
            facts += 1;
        }
    }
    if facts >= 2 {
        out.push(d);
    }
}
