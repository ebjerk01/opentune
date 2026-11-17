//! Delphi, Magneti Marelli and Denso detectors.
//!
//! Marelli MJD (`MJDxxxHWnnX` tags, `MAG  01246…` part) and IAW (`iawNN…`
//! tags), Delphi Multec's `\x11DEL` header and the Denso copyright anchors
//! follow signatures documented by openremap-core (MIT, see `NOTICE`). The
//! Delphi copyright / `MTnn.n` / `DCMn.n` literals and the SuperH reset-vector
//! check are OpenTune's own.

use super::{is_print, supplier, Detection, Img, Tier, UNKNOWN_FAMILY};

pub(crate) fn detect(img: &Img, out: &mut Vec<Detection>) {
    delphi(img, out);
    marelli(img, out);
    denso(img, out);
}

fn delphi(img: &Img, out: &mut Vec<Detection>) {
    const S: &str = supplier::DELPHI;
    let copyright = img
        .find(b"Delphi Technologies")
        .or_else(|| img.find(b"DELPHI"));
    // "MT80.1", "MT22.1" — Delphi petrol controller family tags.
    let mt = img.find_iter(b"MT").find(|&o| {
        img.digits_at(o + 2, 2)
            && img.byte(o + 4) == Some(b'.')
            && img.digits_at(o + 5, 1)
            && img.right_bound(o + 6)
    });
    // "DCM3.5", "DCM6.2A" — Delphi common-rail diesel family literal.
    let dcm = img.literal_iter(b"DCM").find(|&o| {
        img.digits_at(o + 3, 1) && img.byte(o + 4) == Some(b'.') && img.digits_at(o + 5, 1)
    });
    // Delco/Delphi Multec header: 0x11 "DEL" at offset 0.
    let multec = img.at(0, b"\x11DEL");

    let tok = |o: usize, n: usize| {
        String::from_utf8_lossy(img.run(o, n, |c| c.is_ascii_alphanumeric() || c == b'.'))
            .into_owned()
    };
    let mut emitted = false;
    if let Some(o) = dcm {
        let t = tok(o, 8);
        let mut d = Detection::new(
            S,
            "DCM",
            if copyright.is_some() {
                Tier::High
            } else {
                Tier::Medium
            },
            "delphi",
        )
        .variant(Some(t.clone()))
        .ev("Delphi DCM family literal", Some(o), Some(t));
        if let Some(c) = copyright {
            d = d.ev("Delphi name/copyright", Some(c), Some(img.snippet(c, 40)));
        }
        out.push(d);
        emitted = true;
    }
    if let Some(o) = mt {
        let t = tok(o, 6);
        let mut d = Detection::new(
            S,
            "MT",
            if copyright.is_some() {
                Tier::High
            } else {
                Tier::Medium
            },
            "delphi",
        )
        .variant(Some(t.clone()))
        .ev("Delphi MTnn.n family tag", Some(o), Some(t));
        if let Some(c) = copyright {
            d = d.ev("Delphi name/copyright", Some(c), Some(img.snippet(c, 40)));
        }
        out.push(d);
        emitted = true;
    }
    if multec {
        out.push(Detection::new(S, "Multec", Tier::Medium, "delphi").ev(
            "Delco/Delphi Multec header 0x11 'DEL'",
            Some(0),
            Some(img.snippet(0, 16)),
        ));
        emitted = true;
    }
    if !emitted {
        if let Some(c) = img.find(b"Delphi Technologies") {
            out.push(
                Detection::new(S, UNKNOWN_FAMILY, Tier::Medium, "delphi").ev(
                    "Delphi copyright string",
                    Some(c),
                    Some(img.snippet(c, 48)),
                ),
            );
        }
    }
}

fn marelli(img: &Img, out: &mut Vec<Detection>) {
    const S: &str = supplier::MARELLI;
    // "MJD6JFHW01C", "MJD6F3HW03P": family + hardware tag.
    // (no left-boundary check: the tag is often glued to preceding bytes)
    let mjd = img.find_iter(b"MJD").find(|&o| {
        let t = img.run(o, 12, |c| c.is_ascii_alphanumeric());
        t.len() >= 10
            && t[3].is_ascii_digit()
            && &t[6..8] == b"HW"
            && t[8].is_ascii_digit()
            && t[9].is_ascii_digit()
    });
    let mag = img.find_iter(b"MAG  ").find(|&o| img.digits_at(o + 5, 5));
    if let Some(o) = mjd {
        let t = img.run(o, 12, |c| c.is_ascii_alphanumeric());
        let mut d = Detection::new(S, "MJD", Tier::High, "marelli")
            .variant(Some(String::from_utf8_lossy(&t[..6]).into_owned()))
            .ev(
                "Marelli MJD family/hardware tag",
                Some(o),
                Some(String::from_utf8_lossy(t).into_owned()),
            );
        if let Some(m) = mag {
            d = d.ev(
                "Marelli 'MAG' part record",
                Some(m),
                Some(img.snippet(m, 20)),
            );
        }
        out.push(d);
        return;
    }
    // Lower-case "iaw16", "iaw1afb0", "iaw08": IAW family tag.
    let iaw = img.find_iter(b"iaw").find(|&o| {
        img.left_bound(o) && img.byte(o + 3).is_some_and(|c| c.is_ascii_digit()) && {
            let t = img.run(o, 10, |c| c.is_ascii_alphanumeric());
            (5..=8).contains(&t.len()) && img.right_bound(o + t.len())
        }
    });
    if let Some(o) = iaw {
        let t = String::from_utf8_lossy(img.run(o, 10, |c| c.is_ascii_alphanumeric())).into_owned();
        out.push(
            Detection::new(S, "IAW", Tier::Medium, "marelli")
                .variant(Some(t.to_ascii_uppercase()))
                .ev("Marelli IAW family tag", Some(o), Some(t)),
        );
        return;
    }
    let name = img.find(b"MARELLI");
    let swapped = img.find(b"AMERLL");
    if mag.is_some() || name.is_some() || swapped.is_some() {
        let mut d = Detection::new(S, UNKNOWN_FAMILY, Tier::Low, "marelli");
        if let Some(m) = mag {
            d = d.ev(
                "Marelli 'MAG' part record",
                Some(m),
                Some(img.snippet(m, 20)),
            );
        }
        if let Some(m) = name {
            d = d.ev("MARELLI name string", Some(m), Some(img.snippet(m, 24)));
        }
        if let Some(m) = swapped {
            d = d.ev(
                "byte-swapped 'MARELLI' (16-bit swapped image)",
                Some(m),
                Some(img.snippet(m, 8)),
            );
        }
        out.push(d);
    }
}

/// SuperH reset vectors: power-on PC/SP then manual-reset PC/SP, big-endian.
/// PC must point inside the image; SP into on-chip RAM (0xFFFFxxxx).
fn superh_vectors(img: &Img) -> bool {
    let w = |o: usize| {
        img.b
            .get(o..o + 4)
            .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    };
    match (w(0), w(4), w(8), w(12)) {
        (Some(pc), Some(sp), Some(pc2), Some(sp2)) => {
            pc != 0
                && (pc as usize) < img.len()
                && pc % 2 == 0
                && sp >> 16 == 0xFFFF
                && pc2 == pc
                && sp2 == sp
        }
        _ => false,
    }
}

fn denso(img: &Img, out: &mut Vec<Detection>) {
    let anchor = [&b"Copr.DENSO"[..], b"Cpyr.DENSO", b"CopyrightDENSO"]
        .iter()
        .find_map(|p| img.find(p));
    let Some(a) = anchor else {
        return;
    };
    let ev = String::from_utf8_lossy(img.run(a, 20, is_print)).into_owned();
    if superh_vectors(img) {
        out.push(
            Detection::new(supplier::DENSO, "Denso SH", Tier::High, "denso")
                .ev("Denso copyright anchor", Some(a), Some(ev))
                .ev(
                    "SuperH reset vector table (PC,SP,PC,SP; SP in 0xFFFFxxxx)",
                    Some(0),
                    Some(format!("{:02X?}", &img.b[..16])),
                ),
        );
    } else {
        out.push(
            Detection::new(supplier::DENSO, UNKNOWN_FAMILY, Tier::Medium, "denso").ev(
                "Denso copyright anchor",
                Some(a),
                Some(ev),
            ),
        );
    }
}
