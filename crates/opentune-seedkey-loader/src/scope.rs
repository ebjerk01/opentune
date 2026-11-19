//! The hard refuse-list: definitions in the immobiliser / anti-theft class are
//! never loaded, even when the user supplies them.
//!
//! OpenTune's scope (CONTRIBUTING.md, "Scope boundaries") excludes immobiliser
//! removal or defeat. Seed/key definitions for ignition switches, steering
//! locks, immobilisers or keyless entry belong to that class, so the loader
//! refuses any definition whose `EcuName`, one of its `Aliases`, its `Origin`
//! or its `Provider` matches a term below. Refused entries are reported (index,
//! ECU name, field, matched term) and never run.
//!
//! ## Matching ("word-ish", case-insensitive)
//!
//! A field is split into alphanumeric runs (anything else separates), and each
//! run into **tokens** at camel-case and letter/digit boundaries
//! (`EZS167` → `EZS`, `167`; `KeylessGo` → `Keyless`, `Go`; `ImmoBox` →
//! `Immo`, `Box`). The candidates checked are every token, every pair of
//! adjacent tokens joined (`Key`+`Less` → `keyless`) and the whole run, all
//! lower-cased. A field is refused when a candidate:
//!
//! * **equals** one of [`EXACT`]: `ezs`, `esl`, `elv`, `wfs`, `immo`, `kessy`;
//! * **starts with** one of [`PREFIX`]: `immobili` (immobiliser/-izer),
//!   `keyless`, `entriegel` (entriegeln, Entriegelung), `unlock`,
//!   `wegfahrsperre`, `kessy`;
//! * **contains** one of [`CONTAINS`]: `theft` (anti-theft, antitheft).
//!
//! The list errs on the side of refusing: a false positive costs the user an
//! edit of their own file; a false negative would put OpenTune out of scope.

/// Terms a candidate must equal.
pub const EXACT: &[&str] = &["ezs", "esl", "elv", "wfs", "immo", "kessy"];
/// Terms a candidate must start with.
pub const PREFIX: &[&str] = &[
    "immobili",
    "keyless",
    "entriegel",
    "unlock",
    "wegfahrsperre",
    "kessy",
];
/// Terms a candidate must contain.
pub const CONTAINS: &[&str] = &["theft"];

/// The message every refusal carries.
pub const SCOPE_NOTE: &str = "refused: immobiliser / anti-theft class, outside OpenTune's scope \
     (CONTRIBUTING.md, \"Scope boundaries\": no immobiliser removal or defeat)";

/// Split one alphanumeric run into camel-case / letter-digit tokens.
fn tokens(run: &str) -> Vec<String> {
    let chars: Vec<char> = run.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if let Some(&p) = i.checked_sub(1).and_then(|j| chars.get(j)) {
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            let boundary = (p.is_lowercase() && c.is_uppercase())
                || (p.is_alphabetic() != c.is_alphabetic())
                || (p.is_uppercase() && c.is_uppercase() && next_lower);
            if boundary && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn matches(candidate: &str) -> Option<&'static str> {
    EXACT
        .iter()
        .find(|t| candidate == **t)
        .or_else(|| PREFIX.iter().find(|t| candidate.starts_with(**t)))
        .or_else(|| CONTAINS.iter().find(|t| candidate.contains(**t)))
        .copied()
}

/// The refuse-list term `text` matches, if any (see the module docs).
pub fn refused_term(text: &str) -> Option<&'static str> {
    for run in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|r| !r.is_empty())
    {
        let toks: Vec<String> = tokens(run).iter().map(|t| t.to_lowercase()).collect();
        let pairs = toks.windows(2).map(|w| format!("{}{}", w[0], w[1]));
        let whole = std::iter::once(run.to_lowercase());
        if let Some(t) = toks
            .iter()
            .cloned()
            .chain(pairs)
            .chain(whole)
            .find_map(|c| matches(&c))
        {
            return Some(t);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenises_camel_case_and_digits() {
        assert_eq!(tokens("EZS167"), ["EZS", "167"]);
        assert_eq!(tokens("EZSKeyless"), ["EZS", "Keyless"]);
        assert_eq!(tokens("ImmoBox"), ["Immo", "Box"]);
        assert_eq!(tokens("me97"), ["me", "97"]);
    }

    #[test]
    fn refuses_every_listed_class() {
        for (text, term) in [
            ("EZS", "ezs"),
            ("ezs_w211", "ezs"),
            ("EZS167", "ezs"),
            ("MyEZS", "ezs"),
            ("ESL", "esl"),
            ("elv-2", "elv"),
            ("WFS4", "wfs"),
            ("IMMO3", "immo"),
            ("ImmoBox", "immo"),
            ("Immobilizer", "immobili"),
            ("engine immobiliser", "immobili"),
            ("KESSY", "kessy"),
            ("KessyModule", "kessy"),
            ("Keyless Go", "keyless"),
            ("KeyLess", "keyless"),
            ("keylessentry", "keyless"),
            ("Lenkschloss entriegeln", "entriegel"),
            ("Entriegelung", "entriegel"),
            ("Unlock", "unlock"),
            ("unlocking tool", "unlock"),
            ("UnlockECU", "unlock"),
            ("Wegfahrsperre", "wegfahrsperre"),
            ("anti-theft", "theft"),
            ("AntiTheftModule", "theft"),
            ("antitheft", "theft"),
        ] {
            assert_eq!(refused_term(text), Some(term), "{text}");
        }
    }

    #[test]
    fn ordinary_names_pass() {
        for text in [
            "ME97",
            "SIM-ECU",
            "PowertrainDemo",
            "Gearbox 0x11",
            "Diesel EDC",
            "TestOrigin_1",
            "VolkswagenSA2",
            "Elevation", // starts with "el", not "elv" as a token
            "Wheels",
            "Lock",
            "",
        ] {
            assert_eq!(refused_term(text), None, "{text}");
        }
    }
}
