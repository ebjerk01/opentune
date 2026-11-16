//! Filename / path signal. Many library files are already well-named, e.g.
//! `VW Passat 1.9 TDI PD 03G906021LR 1037382088.bin` or a tidy
//! `sorted/Volkswagen/Passat/1.9 TDI/...` folder tree. We parse (make, model,
//! engine, part#) out of the path so (a) those files can be assigned without any
//! DB and (b) a part#→(make,model,engine) index built from the well-named files
//! can rescue the badly-named ones that merely share a part number.

use serde::Serialize;

use crate::extract;

#[derive(Debug, Clone, Default, Serialize)]
pub struct FilenameParse {
    pub make: Option<String>,
    pub model: Option<String>,
    pub engine: Option<String>,
    pub part_numbers: Vec<String>,
}

impl FilenameParse {
    pub fn has_identity(&self) -> bool {
        self.make.is_some()
    }
    pub fn full_identity(&self) -> bool {
        self.make.is_some() && self.model.is_some() && self.engine.is_some()
    }
}

/// (canonical make, list of lowercase aliases that appear in filenames).
const MAKES: &[(&str, &[&str])] = &[
    ("Volkswagen", &["volkswagen", "vw", "vag"]),
    ("Audi", &["audi"]),
    ("Seat", &["seat"]),
    ("Skoda", &["skoda", "škoda"]),
    ("BMW", &["bmw"]),
    ("Mercedes", &["mercedes", "mercedes-benz", "merc", "mb"]),
    ("Opel", &["opel", "vauxhall"]),
    ("Ford", &["ford"]),
    ("Renault", &["renault"]),
    ("Peugeot", &["peugeot"]),
    ("Citroen", &["citroen", "citroën"]),
    ("Fiat", &["fiat"]),
    ("Alfa Romeo", &["alfa", "alfaromeo"]),
    ("Volvo", &["volvo"]),
    ("Toyota", &["toyota"]),
    ("Nissan", &["nissan"]),
    ("Honda", &["honda"]),
    ("Hyundai", &["hyundai"]),
    ("Kia", &["kia"]),
    ("Mazda", &["mazda"]),
    ("Mitsubishi", &["mitsubishi"]),
    ("Subaru", &["subaru"]),
    ("Land Rover", &["landrover", "land rover", "rover"]),
    ("Jaguar", &["jaguar"]),
    ("Porsche", &["porsche"]),
    ("Dacia", &["dacia"]),
    ("Chevrolet", &["chevrolet", "chevy"]),
    ("Iveco", &["iveco"]),
    ("Mini", &["mini"]),
    ("Smart", &["smart"]),
];

/// Engine fuel-system tags that mark a displacement token as an engine.
const ENGINE_TAGS: &[&str] = &[
    "TDI", "TSI", "FSI", "TFSI", "GTI", "PD", "CR", "HDI", "DCI", "CDI", "CRD", "JTD", "DTI",
    "CDTI", "DID", "D4D", "TDCI", "SDI", "MPI", "GDI", "VVT", "BLUETEC", "BLUEHDI", "CRDI", "VTI",
    "THP",
];

fn canon_make(tok: &str) -> Option<&'static str> {
    let t = tok.to_ascii_lowercase();
    for (canon, aliases) in MAKES {
        if aliases.iter().any(|a| *a == t) {
            return Some(canon);
        }
    }
    None
}

/// Parse the full relative path (folder names carry signal too).
pub fn parse(rel_path: &str) -> FilenameParse {
    let mut out = FilenameParse::default();

    // part numbers from the whole path string
    let bytes = rel_path.as_bytes();
    let ex = extract::extract(bytes);
    out.part_numbers = ex
        .vag_part_numbers
        .iter()
        .chain(ex.bosch_numbers.iter())
        .cloned()
        .collect();

    // Strip the final file extension, then tokenise on path separators and
    // punctuation but NOT '.', so a displacement like "1.9" stays intact.
    let stem = {
        let last_sep = rel_path.rfind('/').map(|i| i + 1).unwrap_or(0);
        match rel_path[last_sep..].rfind('.') {
            Some(d) => &rel_path[..last_sep + d],
            None => rel_path,
        }
    };
    let toks: Vec<String> = stem
        .split(|c: char| {
            c == '/' || c == ' ' || c == '_' || c == '-' || c == '(' || c == ')' || c == ','
        })
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    // make: first token (scanning left-to-right) that is a known make alias
    let mut make_idx = None;
    for (i, t) in toks.iter().enumerate() {
        if let Some(m) = canon_make(t) {
            out.make = Some(m.to_string());
            make_idx = Some(i);
            break;
        }
    }

    // engine: a displacement token (e.g. "1.9", "2.0", "1900") adjacent to an
    // engine tag, or a bare engine tag. Capture "<disp> <tag>".
    let upper: Vec<String> = toks.iter().map(|t| t.to_ascii_uppercase()).collect();
    for i in 0..toks.len() {
        if ENGINE_TAGS.contains(&upper[i].as_str()) {
            // look back for a displacement number in the previous 1-2 tokens
            let disp = (1..=2)
                .filter_map(|back| i.checked_sub(back))
                .map(|j| &toks[j])
                .find(|t| looks_like_displacement(t))
                .cloned();
            out.engine = Some(match disp {
                Some(d) => format!("{} {}", d, upper[i]),
                None => upper[i].clone(),
            });
            break;
        }
    }
    // displacement written joined with the tag, e.g. "1.9TDI", "20TDI"
    if out.engine.is_none() {
        for t in &toks {
            if let Some(e) = split_joined_engine(t) {
                out.engine = Some(e);
                break;
            }
        }
    }

    // model: the token right after the make, if it is wordy (not a number/tag).
    if let Some(mi) = make_idx {
        if let Some(cand) = toks.get(mi + 1) {
            let cu = cand.to_ascii_uppercase();
            let is_engineish = looks_like_displacement(cand)
                || ENGINE_TAGS.contains(&cu.as_str())
                || split_joined_engine(cand).is_some();
            let is_wordy = cand.chars().any(|c| c.is_ascii_alphabetic()) && cand.len() >= 2;
            if is_wordy && !is_engineish && canon_make(cand).is_none() {
                out.model = Some(cand.clone());
            }
        }
    }

    out
}

fn looks_like_displacement(t: &str) -> bool {
    // "1.9", "2.0", "1.6" or compact "19"/"20" (1-2 . 1 digit, or 3-4 digits cc)
    let b = t.as_bytes();
    if b.len() == 3 && b[0].is_ascii_digit() && b[1] == b'.' && b[2].is_ascii_digit() {
        return true;
    }
    // 3-4 digit cc like 1900/1968 (loose; only used adjacent to an engine tag)
    if (b.len() == 4) && b.iter().all(|c| c.is_ascii_digit()) {
        return t.starts_with('1') || t.starts_with('2') || t.starts_with('3');
    }
    false
}

/// "1.9TDI" / "2.0TDI" / "16HDI" split into "<disp> <TAG>".
fn split_joined_engine(t: &str) -> Option<String> {
    let up = t.to_ascii_uppercase();
    for tag in ENGINE_TAGS {
        if let Some(stripped) = up.strip_suffix(tag) {
            if !stripped.is_empty() && stripped.chars().all(|c| c.is_ascii_digit() || c == '.') {
                return Some(format!("{} {}", stripped, tag));
            }
        }
    }
    None
}
