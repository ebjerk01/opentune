//! VAG dual-clutch (DSG / S tronic) transmission control units.
//!
//! VAG TCU images carry their software/hardware part number as ASCII:
//! `<gearbox code>300<ddd>[A-Z]{0,2}` (software, e.g. `02E300041N`,
//! `0D9300042M`) or `<gearbox code>927<ddd>[A-Z]{0,2}` (control unit /
//! mechatronic, e.g. `0GC927373K`). The gearbox code names the transmission:
//!
//! | code | family | supplier |
//! |---|---|---|
//! | `02E` | DQ250 (PQ) | Temic |
//! | `0D9` | DQ250 (MQB) | Temic |
//! | `0GC` | DQ381 | Bosch |
//! | `0AM`, `0CW` | DQ200 | not confirmed |
//! | `0CK`, `0CL`, `0CJ` | DL382 | not confirmed |
//!
//! The 02E ident record is structured: `<nn>02E300xxxX  \x01GSG DSG ... DCT`
//! (a two-digit dataset index glued in front of the part number), and the
//! PowerPC-generation read ends its calibration sector with the
//! `v069…__getriebe_DSG_XXXX` tag and a CRC-16 sector table (`dq250-02e`). DQ381
//! images name their project (`…_VW_DQ380`) and supplier (`BOSCH TCU`).
//!
//! All markers here are OpenTune's own, taken from real reads and the public
//! gearbox-code / part-number conventions; see `docs/DSG.md`.

use super::{supplier, Detection, Img, Tier};
use opentune_checksum::ChecksumAlgo;

/// `(gearbox code, family, supplier)`.
const CODES: &[(&[u8; 3], &str, &str)] = &[
    (b"02E", "DQ250", supplier::TEMIC),
    (b"0D9", "DQ250", supplier::TEMIC),
    (b"0GC", "DQ381", supplier::BOSCH),
    (b"0AM", "DQ200", supplier::VAG_TCU),
    (b"0CW", "DQ200", supplier::VAG_TCU),
    (b"0CK", "DL382", supplier::VAG_TCU),
    (b"0CL", "DL382", supplier::VAG_TCU),
    (b"0CJ", "DL382", supplier::VAG_TCU),
];

/// One TCU part number found in an image.
#[derive(Debug, Clone)]
pub(crate) struct TcuPart {
    pub offset: usize,
    /// e.g. `02E300041N`.
    pub part: String,
    pub code: &'static str,
    pub family: &'static str,
    pub supplier: &'static str,
}

/// Left boundary: nothing alphanumeric before the code, or exactly a two-digit
/// dataset index (as in the 02E ident record `0102E300041N`).
fn left_ok(img: &Img, o: usize) -> bool {
    if img.left_bound(o) {
        return true;
    }
    o >= 2 && img.digits_at(o - 2, 2) && img.left_bound(o - 2)
}

/// Every `<code>{300|927}<ddd>[A-Z]{0,2}` part number, in image order.
pub(crate) fn tcu_parts(img: &Img) -> Vec<TcuPart> {
    let mut out: Vec<TcuPart> = Vec::new();
    for &(code, family, sup) in CODES {
        for o in img.find_iter(code) {
            let g = o + 3;
            if !(img.at(g, b"300") || img.at(g, b"927")) || !img.digits_at(g + 3, 3) {
                continue;
            }
            if !left_ok(img, o) {
                continue;
            }
            let idx = img.run(g + 6, 3, |c| c.is_ascii_uppercase()).len();
            if idx > 2 || !img.right_bound(g + 6 + idx) {
                continue;
            }
            let part = String::from_utf8_lossy(&img.b[o..g + 6 + idx]).into_owned();
            if out.iter().any(|p| p.part == part) {
                continue;
            }
            out.push(TcuPart {
                offset: o,
                part,
                code: std::str::from_utf8(code).unwrap_or(""),
                family,
                supplier: sup,
            });
        }
    }
    out.sort_by_key(|p| p.offset);
    out
}

pub(crate) fn detect(img: &Img, out: &mut Vec<Detection>) {
    let parts = tcu_parts(img);
    let mut done: Vec<&str> = Vec::new();
    for p in &parts {
        if done.contains(&p.family) {
            continue;
        }
        done.push(p.family);
        // The 02E ident record: part, two spaces, 0x01, "GSG DSG".
        let record = img
            .find_in(b"GSG DSG", p.offset, p.offset + 0x20)
            .filter(|_| p.code == "02E" || p.code == "0D9");
        let bosch_tcu = (p.family == "DQ381")
            .then(|| {
                img.find(b"BOSCH TCU")
                    .or_else(|| img.find(b"_DQ380"))
                    .or_else(|| img.find(b"DQ380_"))
            })
            .flatten();
        let tier = if record.is_some() || bosch_tcu.is_some() {
            Tier::High
        } else {
            Tier::Medium
        };
        let mut d = Detection::new(p.supplier, p.family, tier, "vag-tcu")
            .variant(Some(p.code.to_string()))
            .ev(
                "VAG transmission part number (gearbox code names the DSG family)",
                Some(p.offset),
                Some(p.part.clone()),
            );
        if let Some(r) = record {
            d = d.ev(
                "DSG ident record (GSG DSG) next to the part number",
                Some(r),
                Some(img.snippet(p.offset, 0x20).trim_end().to_string()),
            );
        }
        if let Some(b) = bosch_tcu {
            d = d.ev(
                "Bosch TCU / DQ380 project marker",
                Some(b),
                Some(img.snippet(b, 24)),
            );
        }
        if p.code == "02E" {
            d = dq250_02e_layout(img, d);
        }
        out.push(d);
    }
}

/// Layout facts for a 02E read (as evidence text, never as a variant).
fn dq250_02e_layout(img: &Img, mut d: Detection) -> Detection {
    use opentune_checksum::dsg::{Dq250PqCrc16, DQ250_02E_CAL_BASE, DQ250_02E_IMAGE_LEN};
    if img.len() == DQ250_02E_IMAGE_LEN {
        if let Some(t) = img.find_in(b"getriebe_DSG", DQ250_02E_CAL_BASE + 0x7FC0, img.len()) {
            d = d.ev(
                "02E PowerPC-generation layout: 0x68000 read, version tag at 0x67FC0",
                Some(t),
                Some(img.snippet(DQ250_02E_CAL_BASE + 0x7FC0, 32)),
            );
        }
        if let Ok(st) = Dq250PqCrc16.verify(img.b) {
            let ok = st.iter().filter(|b| b.ok).count();
            if ok > 0 {
                d = d.ev(
                    format!(
                        "dq250-02e CRC-16 sector table: {ok}/{} words match",
                        st.len()
                    ),
                    Some(DQ250_02E_CAL_BASE + 0x7FE2),
                    None,
                );
            }
        }
    } else if img.len() == 0xB_0000 && img.find_in(b"getriebe_DSG", 0xFFC0, 0x1_0000).is_some() {
        d = d.ev(
            "02E TriCore-generation layout: 0xB0000 read, 64 KiB CAL first, version tag at 0xFFC0",
            Some(0xFFC0),
            Some(img.snippet(0xFFC0, 32)),
        );
    }
    d
}
