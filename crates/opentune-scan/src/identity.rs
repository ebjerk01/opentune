//! Offset-bearing identity extraction: every identifier we report says where in
//! the image it was read, so a user can check it in a hex view.
//!
//! Sources: the frequency-ranked tokens from [`crate::extract`] (now with first
//! offsets), Bosch project descriptors (dataset / calibration fields), VAG
//! ident blocks (OEM record + engine descriptor), Siemens/Continental part
//! prefixes and Ford-style engineering part numbers.

use serde::Serialize;

use crate::extract::Extracted;
use crate::recognize::{self, Img};

/// One identifier read from the image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdField {
    /// `bosch_hw`, `bosch_sw`, `bosch_number`, `vag_part`, `mercedes_part`,
    /// `ford_part`, `continental_part`, `vin`, `dataset`, `calibration_id`,
    /// `bosch_descriptor`, `oem_ident_block`, `engine`.
    pub kind: &'static str,
    pub value: String,
    /// Offset of the first occurrence.
    pub offset: u64,
    /// How many times the value occurs (1 for record-derived fields).
    pub count: usize,
}

const PER_KIND: usize = 6;

/// VAG main groups used by control units (906 engine, 907 misc ECUs,
/// 927 gearbox, 997/998 exchange units).
const VAG_ECU_GROUPS: &[&str] = &["906", "907", "927", "997", "998"];

fn push(out: &mut Vec<IdField>, kind: &'static str, value: String, offset: usize, count: usize) {
    if out.iter().filter(|f| f.kind == kind).count() >= PER_KIND
        || out.iter().any(|f| f.kind == kind && f.value == value)
    {
        return;
    }
    out.push(IdField {
        kind,
        value,
        offset: offset as u64,
        count,
    });
}

/// Extract every identifier we can place in the image.
pub fn identity(image: &[u8], ex: &Extracted) -> Vec<IdField> {
    let mut out: Vec<IdField> = Vec::new();
    let off = |t: &String| ex.first_offset.get(t).copied().unwrap_or(0);
    let cnt = |t: &String| ex.counts.get(t).copied().unwrap_or(1);

    for t in &ex.bosch_numbers {
        let kind = if ["0261", "0281", "0285"].iter().any(|p| t.starts_with(p)) {
            "bosch_hw"
        } else {
            "bosch_number"
        };
        push(&mut out, kind, t.clone(), off(t), cnt(t));
    }
    for t in &ex.sw_numbers {
        push(&mut out, "bosch_sw", t.clone(), off(t), cnt(t));
    }
    for t in &ex.vag_part_numbers {
        let norm: String = t.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        let main = norm.get(3..6).unwrap_or("");
        if VAG_ECU_GROUPS.contains(&main) {
            push(&mut out, "vag_part", norm, off(t), cnt(t));
        }
    }
    for t in &ex.mercedes_numbers {
        push(&mut out, "mercedes_part", t.clone(), off(t), cnt(t));
    }
    for t in &ex.vins {
        push(&mut out, "vin", t.clone(), off(t), cnt(t));
    }

    let img = Img { b: image };
    if recognize::image_kind(image) != recognize::ImageKind::Raw {
        return out;
    }
    continental_parts(&img, &mut out);
    ford_parts(&img, &mut out);

    for d in recognize::descriptors(&img) {
        push(&mut out, "bosch_descriptor", d.text.clone(), d.offset, 1);
        // Field 4: dataset "6428.AA" / "4420.01".
        if let Some(f) = d.fields.get(4) {
            let b = f.as_bytes();
            if b.len() == 7 && b[..4].iter().all(u8::is_ascii_digit) && b[4] == b'.' {
                push(&mut out, "dataset", f.clone(), d.field_offsets[4], 1);
            }
        }
        // First non-empty field after the first empty one: the project /
        // calibration name ("D9158_N43J003", "P_842", "C598G3W1").
        if let Some(gap) = d.fields.iter().skip(3).position(|f| f.is_empty()) {
            let i = gap + 3;
            if let Some((j, f)) = d
                .fields
                .iter()
                .enumerate()
                .skip(i + 1)
                .find(|(_, f)| !f.is_empty())
            {
                if f.len() >= 4 && f.chars().any(|c| c.is_ascii_digit()) {
                    push(&mut out, "calibration_id", f.clone(), d.field_offsets[j], 1);
                }
            }
        }
    }
    for v in recognize::ident_blocks(&img) {
        push(&mut out, "vag_part", v.part.clone(), v.offset, 1);
        push(&mut out, "oem_ident_block", v.text.clone(), v.offset, 1);
        if let Some((e, o)) = v.engine {
            push(&mut out, "engine", e, o, 1);
        }
    }
    out
}

/// `5WK9…`, `5WS4…[-T]`, `5WP4…` Siemens/Continental part numbers.
fn continental_parts(img: &Img, out: &mut Vec<IdField>) {
    for pre in [&b"5WK9"[..], b"5WS4", b"5WP4"] {
        for o in img.literal_iter(pre) {
            let digits = img.run(o + 4, 6, |c| c.is_ascii_digit()).len();
            if digits < 3 {
                continue;
            }
            let mut t = img.run(o, 12, |c| c.is_ascii_alphanumeric());
            if img.at(o + t.len(), b"-T") {
                t = &img.b[o..o + t.len() + 2];
            }
            push(
                out,
                "continental_part",
                String::from_utf8_lossy(t).into_owned(),
                o,
                1,
            );
        }
    }
}

/// Ford engineering numbers: `6G91-12B684-DA`, `BV61-14C204-BRJ`.
fn ford_parts(img: &Img, out: &mut Vec<IdField>) {
    let alnum_mix = |s: &[u8]| {
        s.iter().all(u8::is_ascii_alphanumeric)
            && s.iter().any(u8::is_ascii_digit)
            && s.iter().any(u8::is_ascii_uppercase)
    };
    for o in img.find_iter(b"-") {
        if o < 4 || !img.left_bound(o - 4) {
            continue;
        }
        let a = &img.b[o - 4..o];
        let mid = img.run(o + 1, 7, |c| c.is_ascii_alphanumeric());
        if !alnum_mix(a) || !(5..=6).contains(&mid.len()) || !alnum_mix(mid) {
            continue;
        }
        let p = o + 1 + mid.len();
        if img.byte(p) != Some(b'-') {
            continue;
        }
        let suf = img.run(p + 1, 4, |c| c.is_ascii_uppercase());
        if !(2..=3).contains(&suf.len()) || !img.right_bound(p + 1 + suf.len()) {
            continue;
        }
        let v = String::from_utf8_lossy(&img.b[o - 4..p + 1 + suf.len()]).into_owned();
        push(out, "ford_part", v, o - 4, 1);
    }
}
