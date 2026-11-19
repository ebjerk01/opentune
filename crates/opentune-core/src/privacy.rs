//! Privacy scan for contributed data files.
//!
//! Definition data (ECU definitions, packs, controller definitions, catalog
//! and database records) must not carry personal data. This scanner looks for
//! the three kinds of identifier that most often slip in with a contribution:
//!
//! - **VIN-like** tokens: exactly 17 characters of the VIN alphabet (upper-case
//!   `A-Z` without `I`/`O`/`Q`, digits), at least one letter, and the last four
//!   characters numeric (ISO 3779). Clearly synthetic placeholders are allowed:
//!   a run of five identical characters (`WVWZZZ00000000000`), a run of six
//!   ascending characters (`…123456`), a marker word (`TEST`, `FAKE`, `DUMMY`,
//!   `EXAMPLE`, `SAMPLE`, `SYNTH`, `XXXX`), or the check-digit worked example
//!   `1M8GDM9AXKP042788` that VIN references publish.
//! - **e-mail addresses**, except the reserved example domains (RFC 2606 /
//!   RFC 6761: `example.com|net|org`, `*.example`, `*.test`, `*.invalid`,
//!   `*.localhost`).
//! - **serial-number-looking values**: a label such as `serial`, `serial number`,
//!   `S/N`, `Seriennummer` or `chassis number` followed by a value of 6+
//!   characters with at least four digits (`0x` hex literals, e.g. a DID number,
//!   are not values).
//!
//! It is a heuristic net, not a guarantee: a finding is printed redacted (so CI
//! logs do not repeat the identifier), and a reviewer still reads every
//! contribution. Deliberate synthetic values that the heuristics flag go in an
//! allow-list (see `opentune privacy-scan --allow`).

/// What a [`PrivacyFinding`] looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivacyKind {
    /// A 17-character VIN-like token.
    Vin,
    /// An e-mail address.
    Email,
    /// A labelled serial-number-like value.
    Serial,
}

impl PrivacyKind {
    /// Stable code, e.g. for a [`crate::ValidationIssue`].
    pub fn code(self) -> &'static str {
        match self {
            PrivacyKind::Vin => "privacy.vin",
            PrivacyKind::Email => "privacy.email",
            PrivacyKind::Serial => "privacy.serial",
        }
    }

    /// Short human label.
    pub fn label(self) -> &'static str {
        match self {
            PrivacyKind::Vin => "VIN-like identifier",
            PrivacyKind::Email => "e-mail address",
            PrivacyKind::Serial => "serial-number-like value",
        }
    }
}

/// One possible piece of personal data found in a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivacyFinding {
    /// What was found.
    pub kind: PrivacyKind,
    /// 1-based line.
    pub line: usize,
    /// 1-based column (in bytes).
    pub column: usize,
    /// The matched value itself. Do not print it; use [`PrivacyFinding::redacted`].
    pub value: String,
}

impl PrivacyFinding {
    /// The value with all but its first characters hidden, safe to print in a
    /// public CI log (e.g. `WVW… (17 chars)`).
    pub fn redacted(&self) -> String {
        redact(&self.value)
    }

    /// One-line description without the value, e.g. for a validation message.
    pub fn message(&self) -> String {
        format!(
            "{} at line {} col {} ({}): remove it or replace it with a clearly synthetic placeholder",
            self.kind.label(),
            self.line,
            self.column,
            self.redacted()
        )
    }
}

/// Keep the first three characters (a VIN's manufacturer code, an address's
/// first letters) and the length.
pub fn redact(value: &str) -> String {
    let head: String = value.chars().take(3).collect();
    format!("{head}… ({} chars)", value.chars().count())
}

/// Scan `text` for VIN-like tokens, e-mail addresses and serial-number-like
/// values. `allow` lists exact values to ignore (deliberate synthetic data).
pub fn scan(text: &str, allow: &[String]) -> Vec<PrivacyFinding> {
    let mut hits: Vec<(PrivacyKind, usize, usize)> = Vec::new();
    scan_vins(text, &mut hits);
    scan_emails(text, &mut hits);
    scan_serials(text, &mut hits);
    hits.sort_by_key(|h| (h.1, h.0 as u8));
    hits.dedup_by_key(|h| h.1);
    let mut out = Vec::new();
    for (kind, start, end) in hits {
        let value = &text[start..end];
        if allow.iter().any(|a| a == value) {
            continue;
        }
        let (line, column) = line_col(text, start);
        out.push(PrivacyFinding {
            kind,
            line,
            column,
            value: value.to_string(),
        });
    }
    out
}

/// Parse an allow-list file: one exact value per line, `#` starts a comment.
pub fn parse_allow_list(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

fn line_col(text: &str, off: usize) -> (usize, usize) {
    let before = &text.as_bytes()[..off];
    let line = before.iter().filter(|&&b| b == b'\n').count() + 1;
    let col = off
        - before
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |p| p + 1)
        + 1;
    (line, col)
}

// ---------------------------------------------------------------------------
// VINs
// ---------------------------------------------------------------------------

/// Published check-digit worked example (not a real vehicle of anyone's).
const DOCUMENTED_EXAMPLE_VINS: &[&str] = &["1M8GDM9AXKP042788"];

const SYNTHETIC_MARKERS: &[&str] = &[
    "TEST", "FAKE", "DUMMY", "EXAMPLE", "SAMPLE", "SYNTH", "XXXX",
];

fn is_vin_char(c: u8) -> bool {
    c.is_ascii_digit() || (c.is_ascii_uppercase() && !matches!(c, b'I' | b'O' | b'Q'))
}

/// Is this 17-character token VIN-shaped?
pub fn is_vin_like(t: &str) -> bool {
    let b = t.as_bytes();
    b.len() == 17
        && b.iter().all(|&c| is_vin_char(c))
        && b.iter().any(|c| c.is_ascii_uppercase())
        && b[13..].iter().all(|c| c.is_ascii_digit())
}

/// Is a VIN-shaped token clearly a placeholder rather than a real vehicle?
pub fn is_synthetic_vin(t: &str) -> bool {
    if DOCUMENTED_EXAMPLE_VINS.contains(&t) || SYNTHETIC_MARKERS.iter().any(|m| t.contains(m)) {
        return true;
    }
    let b = t.as_bytes();
    let (mut same, mut asc) = (1usize, 1usize);
    for w in b.windows(2) {
        same = if w[1] == w[0] { same + 1 } else { 1 };
        asc = if w[1] == w[0].wrapping_add(1) {
            asc + 1
        } else {
            1
        };
        if same >= 5 || asc >= 6 {
            return true;
        }
    }
    false
}

fn scan_vins(text: &str, hits: &mut Vec<(PrivacyKind, usize, usize)>) {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_alphanumeric() {
            i += 1;
        }
        let tok = &text[start..i];
        if is_vin_like(tok) && !is_synthetic_vin(tok) {
            hits.push((PrivacyKind::Vin, start, i));
        }
    }
}

// ---------------------------------------------------------------------------
// E-mail addresses
// ---------------------------------------------------------------------------

fn is_local_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'%' | b'+' | b'-')
}

fn is_domain_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-')
}

/// Reserved example / test domains (RFC 2606, RFC 6761).
pub fn is_reserved_domain(domain: &str) -> bool {
    let d = domain.to_ascii_lowercase();
    matches!(
        d.as_str(),
        "example.com" | "example.net" | "example.org" | "example" | "localhost"
    ) || [".example", ".test", ".invalid", ".localhost"]
        .iter()
        .any(|s| d.ends_with(s))
        || [".example.com", ".example.net", ".example.org"]
            .iter()
            .any(|s| d.ends_with(s))
}

/// "Top-level domains" that are really file extensions (`name@2x.png`).
const FILE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "svg", "webp", "ico", "bmp", "json", "bin", "txt", "csv", "md",
    "js", "mjs", "css", "html", "rs", "py", "toml", "yml", "yaml", "xml", "xdf", "pdf",
];

fn scan_emails(text: &str, hits: &mut Vec<(PrivacyKind, usize, usize)>) {
    let b = text.as_bytes();
    for (at, _) in text.match_indices('@') {
        let mut s = at;
        while s > 0 && is_local_char(b[s - 1]) {
            s -= 1;
        }
        let mut e = at + 1;
        while e < b.len() && is_domain_char(b[e]) {
            e += 1;
        }
        while e > at + 1 && matches!(b[e - 1], b'.' | b'-') {
            e -= 1;
        }
        if s == at {
            continue;
        }
        let domain = &text[at + 1..e];
        let labels: Vec<&str> = domain.split('.').collect();
        let tld = labels.last().copied().unwrap_or("");
        let well_formed = labels.len() >= 2
            && labels.iter().all(|l| !l.is_empty())
            && tld.len() >= 2
            && tld.bytes().all(|c| c.is_ascii_alphabetic());
        // `icon@2x.png`, `logo@3x.webp`: a file name, not an address.
        let file_like = FILE_EXTENSIONS.contains(&tld.to_ascii_lowercase().as_str());
        if well_formed && !file_like && !is_reserved_domain(domain) {
            hits.push((PrivacyKind::Email, s, e));
        }
    }
}

// ---------------------------------------------------------------------------
// Serial numbers
// ---------------------------------------------------------------------------

/// Labels (lower-case), longest first so `serial number` wins over `serial`.
/// The flag says whether the label is too generic to count without an
/// explicit `:` / `=` / `#` before the value.
const SERIAL_LABELS: &[(&str, bool)] = &[
    ("fahrgestellnummer", false),
    ("chassis number", false),
    ("chassis_number", false),
    ("chassis-number", false),
    ("serial number", false),
    ("serial_number", false),
    ("serial-number", false),
    ("serialnumber", false),
    ("seriennummer", false),
    ("chassis no.", false),
    ("chassis no", false),
    ("serial no.", false),
    ("serial no", false),
    ("serial nr.", false),
    ("serial nr", false),
    ("serial_no", false),
    ("ser.-nr.", false),
    ("ser.-nr", false),
    ("ser.nr.", false),
    ("ser.nr", false),
    ("serial", false),
    ("s/n", true),
    ("sn", true),
];

fn scan_serials(text: &str, hits: &mut Vec<(PrivacyKind, usize, usize)>) {
    let lower = text.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut i = 0;
    while i < b.len() {
        // A label starts at a word boundary.
        if i > 0 && b[i - 1].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        let Some(&(label, needs_delim)) = SERIAL_LABELS.iter().find(|(l, _)| {
            b[i..].starts_with(l.as_bytes())
                && !b
                    .get(i + l.len())
                    .is_some_and(|c| c.is_ascii_alphanumeric())
        }) else {
            i += 1;
            continue;
        };
        let mut j = i + label.len();
        let mut delim = false;
        let mut skipped = 0;
        while j < b.len()
            && skipped < 8
            && matches!(b[j], b' ' | b'\t' | b'"' | b'\'' | b':' | b'=' | b'#')
        {
            delim |= matches!(b[j], b':' | b'=' | b'#');
            j += 1;
            skipped += 1;
        }
        let vs = j;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'-' | b'/')) {
            j += 1;
        }
        let mut ve = j;
        while ve > vs && matches!(b[ve - 1], b'-' | b'/') {
            ve -= 1;
        }
        let value = &lower[vs..ve];
        let digits = value.bytes().filter(u8::is_ascii_digit).count();
        if (!needs_delim || delim) && value.len() >= 6 && digits >= 4 && !value.starts_with("0x") {
            hits.push((PrivacyKind::Serial, vs, ve));
        }
        i += label.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<PrivacyKind> {
        scan(text, &[]).into_iter().map(|f| f.kind).collect()
    }

    // Real-looking identifiers are assembled at run time so this source file
    // itself stays clean under the scanner. The WMI "ZZZ" is unassigned, so
    // this cannot be any real vehicle.
    fn vin_like() -> String {
        ["ZZZ", "7AB", "2CD", "3E", "5", "91", "746"].concat()
    }

    #[test]
    fn flags_a_vin_shaped_token() {
        let v = vin_like();
        assert!(is_vin_like(&v) && !is_synthetic_vin(&v));
        let text = format!("{{\n  \"note\": \"read from car {v} on the bench\"\n}}");
        let f = scan(&text, &[]);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, PrivacyKind::Vin);
        assert_eq!((f[0].line, f[0].column), (2, 26));
        assert_eq!(f[0].redacted(), "ZZZ… (17 chars)");
        assert!(!f[0].message().contains(&v));
    }

    #[test]
    fn synthetic_vins_are_not_flagged() {
        for v in [
            "WVWZZZ00000000000",
            "WVWZZZ1KZAW000001",
            "TESTVCAR0D0000001",
            "WDD2040081A123456",
            "1M8GDM9AXKP042788",
            "WVWZZZ1KZXXXX1234",
        ] {
            assert!(kinds(v).is_empty(), "{v}");
        }
    }

    #[test]
    fn vin_false_positive_cases() {
        // 17 digits (no letter), 17 hex chars ending in letters, lower case,
        // I/O/Q (not in the VIN alphabet), 16 and 18 characters, a substring of
        // a longer hash, part numbers and software ids: none are VINs.
        for t in [
            "12345678901234567",
            "0123456789ABCDEFA",
            "café serial: été — WVWZZZÉ",
            "zzz7ab2cd3e591746",
            "ZZZ7AB2CD3E59174O",
            "ZZZ7AI2CD3E591746",
            "ZZZ7AB2CD3E59174",
            "ZZZ7AB2CD3E5917460",
            "sha256 9F86D081884C7D659A2FEAA0C55AD015A3BF4F1B2B0B822CD15D6C15B0F00A08",
            "03L907425C 1037382088 0281011 03G906021AB",
            "\"sw\": \"1037516722\", \"hw\": \"0261206317\"",
        ] {
            assert!(kinds(t).is_empty(), "{t}");
        }
        // Underscore / punctuation are boundaries: an embedded VIN still hits.
        assert_eq!(
            kinds(&format!("car_{}_dump", vin_like())),
            vec![PrivacyKind::Vin]
        );
    }

    #[test]
    fn emails_and_reserved_domains() {
        let addr = ["jane.doe", "@", "mail", ".no"].concat();
        assert_eq!(kinds(&format!("contact {addr}.")), vec![PrivacyKind::Email]);
        let f = scan(&format!("by <{addr}>"), &[]);
        assert_eq!(f[0].value, addr, "angle brackets and trailing dot trimmed");
        for ok in [
            "jane@example.com",
            "ci@build.example.org",
            "user@host.test",
            "a@b.invalid",
            "root@localhost",
            "@scope/package",
            "user@host",
            "git@1.2.3",
            "see crate@1.2",
            "opendbc @ d714bb4",
            "icons/128x128@2x.png",
            "logo@3x.webp",
        ] {
            assert!(kinds(ok).is_empty(), "{ok}");
        }
    }

    #[test]
    fn serial_numbers_need_a_label_and_digits() {
        let s = ["VWZ7Z0", "A12345", "67"].concat();
        assert_eq!(
            kinds(&format!("\"serial\": \"{s}\"")),
            vec![PrivacyKind::Serial]
        );
        assert_eq!(
            kinds(&format!("Serial number {s}")),
            vec![PrivacyKind::Serial]
        );
        assert_eq!(
            kinds(&format!("S/N: 4471-{}", "20931")),
            vec![PrivacyKind::Serial]
        );
        assert_eq!(
            kinds(&format!("Seriennummer={}", "88213377")),
            vec![PrivacyKind::Serial]
        );
    }

    #[test]
    fn serial_false_positive_cases() {
        for t in [
            // label without a value, or a DID number after it
            "\"name\": \"ECU serial number\", \"did\": 61836",
            "ECU serial number (DID 0xF18C)",
            "serial number 0xF18C61836",
            // words that merely contain the label
            "serialize the record; deserialization; snapshot 123456789",
            // bare 'sn' / 's/n' without ':' '=' '#'
            "sn 12345678",
            "s/n 99887766",
            // too short / too few digits
            "serial: AB12",
            "serial: ABCDEFG1",
            // a sentence about serial communication
            "serial port at 115200 baud",
        ] {
            assert!(kinds(t).is_empty(), "{t}");
        }
    }

    #[test]
    fn allow_list_suppresses_exact_values() {
        let v = vin_like();
        let allow = parse_allow_list(&format!("# synthetic fixture\n{v}  # bench dummy\n\n"));
        assert_eq!(allow, vec![v.clone()]);
        assert!(scan(&v, &allow).is_empty());
        assert_eq!(scan(&v, &[]).len(), 1);
    }
}
