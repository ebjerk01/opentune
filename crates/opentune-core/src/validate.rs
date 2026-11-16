//! Validation and review of ECU definitions.
//!
//! This is the machine half of the contribution-review process documented in
//! `docs/CONTRIBUTING-ECUDEFS.md`. It turns the honesty / scope / format rules a
//! human reviewer would otherwise check by hand into a set of deterministic
//! checks over a parsed [`EcuDef`], so CI can gate every `ecudefs/*.json` on the
//! same bar.
//!
//! The logic lives in the library (not the CLI) so it is unit-testable and can be
//! reused by the desktop/Android apps and by any importer. The CLI subcommand
//! `opentune ecudefs validate` is a thin front-end over [`EcuDef::validate`] (plus
//! [`EcuDef::validate_filename`] and [`EcuDef::validate_against_image`]).
//!
//! Severities:
//! - [`Severity::Error`] — the definition violates a hard rule (scope exclusion,
//!   out-of-image map, missing honesty label, malformed identity). CI fails.
//! - [`Severity::Warn`] — a quality / plausibility concern a reviewer should look
//!   at (missing unit, implausible scaling, non-monotonic axis). CI does not fail.
use crate::ecudef::{AxisDef, DataType, EcuDef, Endian, MapDef};

/// How serious a [`ValidationIssue`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A hard-rule violation. The definition must not be accepted as-is.
    Error,
    /// A quality / plausibility concern worth a reviewer's eyes, not a blocker.
    Warn,
}

impl Severity {
    /// Short uppercase tag for CLI output.
    pub fn tag(self) -> &'static str {
        match self {
            Severity::Error => "ERROR",
            Severity::Warn => "WARN",
        }
    }
}

/// One finding from validating an [`EcuDef`].
#[derive(Debug, Clone)]
pub struct ValidationIssue {
    /// Whether this blocks acceptance ([`Severity::Error`]) or is advisory.
    pub severity: Severity,
    /// Stable, greppable code for the rule, e.g. `"scope.emissions_defeat"`.
    pub code: &'static str,
    /// Where in the definition the issue is (e.g. `"map[2] 'boost target'"`),
    /// or an empty string for a definition-level issue.
    pub location: String,
    /// Human-readable explanation.
    pub message: String,
}

impl ValidationIssue {
    fn error(code: &'static str, location: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            code,
            location: location.into(),
            message: message.into(),
        }
    }

    fn warn(code: &'static str, location: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warn,
            code,
            location: location.into(),
            message: message.into(),
        }
    }
}

/// True if any issue in the slice is an [`Severity::Error`].
pub fn has_errors(issues: &[ValidationIssue]) -> bool {
    issues.iter().any(|i| i.severity == Severity::Error)
}

/// Number of [`Severity::Error`] issues in the slice.
pub fn error_count(issues: &[ValidationIssue]) -> usize {
    issues
        .iter()
        .filter(|i| i.severity == Severity::Error)
        .count()
}

/// Number of [`Severity::Warn`] issues in the slice.
pub fn warn_count(issues: &[ValidationIssue]) -> usize {
    issues
        .iter()
        .filter(|i| i.severity == Severity::Warn)
        .count()
}

// ---------------------------------------------------------------------------
// Scope enforcement (hard errors)
// ---------------------------------------------------------------------------

/// A forbidden capability (per `README.md` "what OpenTune will NOT do"): the
/// subject tokens that name a system, and the action tokens that turn a mention
/// of that system into a *defeat* of it. Bare `"egr"` / `"dpf"` are legitimate
/// calibration map names, so a hit requires an action word within a short window
/// *after* the subject (e.g. "egr off", "dpf delete", "immo bypass").
struct DefeatRule {
    code: &'static str,
    label: &'static str,
    subjects: &'static [&'static str],
    actions: &'static [&'static str],
}

const DEFEAT_RULES: &[DefeatRule] = &[
    DefeatRule {
        code: "scope.immobiliser_defeat",
        label: "immobiliser removal / defeat",
        subjects: &["immobiliser", "immobilizer", "immo", "wegfahrsperre"],
        actions: &[
            "off", "delete", "delet", "defeat", "remove", "removal", "disable", "bypass", "kill",
            "emulator", "aus",
        ],
    },
    DefeatRule {
        code: "scope.odometer_correction",
        label: "odometer / mileage correction",
        subjects: &[
            "odometer",
            "mileage",
            "kilometer",
            "kilometre",
            "laufleistung",
            "tacho",
            "odo ",
            "odo-",
        ],
        actions: &[
            "correct",
            "adjust",
            "rollback",
            "roll back",
            "reset",
            "recalibrat",
            "manipulat",
            "change",
            "edit",
            "reduce",
            "wind back",
        ],
    },
    DefeatRule {
        code: "scope.emissions_defeat",
        label: "emissions-control defeat (DPF/EGR/AdBlue/SCR off)",
        subjects: &[
            "dpf",
            "gpf",
            "fap",
            "egr",
            "agr",
            "adblue",
            "ad blue",
            "scr ",
            "scr-",
            "nox",
            "catalyst",
            "lambda delete",
            "o2 delete",
        ],
        actions: &[
            "off", "delete", "delet", "defeat", "remove", "removal", "disable", "bypass", "spoof",
            "emulat",
        ],
    },
];

/// Scan one lowercased text for any forbidden subject+action pairing.
fn scan_defeat(lower: &str) -> Vec<(&'static str, &'static str)> {
    let mut hits = Vec::new();
    for rule in DEFEAT_RULES {
        if rule_hits(lower, rule) {
            hits.push((rule.code, rule.label));
        }
    }
    hits
}

fn rule_hits(lower: &str, rule: &DefeatRule) -> bool {
    for subj in rule.subjects {
        let mut from = 0usize;
        while let Some(rel) = lower[from..].find(subj) {
            let start = from + rel;
            let win_end = (start + subj.len() + 14).min(lower.len());
            let window = &lower[start..win_end];
            if rule.actions.iter().any(|a| window.contains(a)) {
                return true;
            }
            from = start + subj.len();
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Embedded-blob detection
// ---------------------------------------------------------------------------

/// A string field longer than this is almost certainly not prose — a likely
/// vehicle for an embedded proprietary blob.
const MAX_STRING_LEN: usize = 8192;
/// A contiguous run of base64/hex characters this long (with no whitespace) in a
/// text field is treated as an embedded binary blob.
const MAX_B64_RUN: usize = 256;

fn longest_blobby_run(s: &str) -> usize {
    let mut best = 0usize;
    let mut cur = 0usize;
    for c in s.chars() {
        let blobby = c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-');
        if blobby {
            cur += 1;
            best = best.max(cur);
        } else {
            cur = 0;
        }
    }
    best
}

// ---------------------------------------------------------------------------
// Decode helpers (for --image validation and inline-axis checks)
// ---------------------------------------------------------------------------

fn read_cell(img: &[u8], off: usize, dt: DataType, endian: Endian) -> Option<f64> {
    let w = dt.byte_len() as usize;
    let b = img.get(off..off.checked_add(w)?)?;
    let le = matches!(endian, Endian::Little);
    let v = match dt {
        DataType::U8 => b[0] as f64,
        DataType::S8 => (b[0] as i8) as f64,
        DataType::U16 => {
            let a = [b[0], b[1]];
            (if le {
                u16::from_le_bytes(a)
            } else {
                u16::from_be_bytes(a)
            }) as f64
        }
        DataType::S16 => {
            let a = [b[0], b[1]];
            (if le {
                i16::from_le_bytes(a)
            } else {
                i16::from_be_bytes(a)
            }) as f64
        }
        DataType::U32 => {
            let a = [b[0], b[1], b[2], b[3]];
            (if le {
                u32::from_le_bytes(a)
            } else {
                u32::from_be_bytes(a)
            }) as f64
        }
        DataType::S32 => {
            let a = [b[0], b[1], b[2], b[3]];
            (if le {
                i32::from_le_bytes(a)
            } else {
                i32::from_be_bytes(a)
            }) as f64
        }
        DataType::F32 => {
            let a = [b[0], b[1], b[2], b[3]];
            (if le {
                f32::from_le_bytes(a)
            } else {
                f32::from_be_bytes(a)
            }) as f64
        }
    };
    Some(v)
}

/// Raw storage extremes of a data type (used to bound the physical range a map
/// can produce without reading an image). `f32` is unbounded, returns `None`.
fn raw_extremes(dt: DataType) -> Option<(f64, f64)> {
    Some(match dt {
        DataType::U8 => (0.0, u8::MAX as f64),
        DataType::S8 => (i8::MIN as f64, i8::MAX as f64),
        DataType::U16 => (0.0, u16::MAX as f64),
        DataType::S16 => (i16::MIN as f64, i16::MAX as f64),
        DataType::U32 => (0.0, u32::MAX as f64),
        DataType::S32 => (i32::MIN as f64, i32::MAX as f64),
        DataType::F32 => return None,
    })
}

/// Largest absolute physical value that is still considered plausible for a
/// calibration cell. Real maps stay well under this; a factor error (e.g. reading
/// a raw CRC word as a map) blows past it.
const IMPLAUSIBLE_ABS: f64 = 1.0e7;

fn norm_key(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Does `notes` carry one of the honesty tags from `CONTRIBUTING-ECUDEFS.md`?
fn has_confidence_label(notes: &str) -> bool {
    let u = notes.to_ascii_uppercase();
    u.contains("CONFIDENCE:")
        || u.contains("EXAMPLE ONLY")
        || u.contains("PLACEHOLDER")
        || u.contains("SYNTHETIC")
}

/// Is a sequence strictly monotonic (all increasing or all decreasing)?
fn is_strictly_monotonic(v: &[f64]) -> bool {
    if v.len() < 2 {
        return true;
    }
    let inc = v.windows(2).all(|w| w[1] > w[0]);
    let dec = v.windows(2).all(|w| w[1] < w[0]);
    inc || dec
}

impl EcuDef {
    /// Validate a definition against the format, honesty and scope rules.
    ///
    /// Covers everything checkable from the parsed definition alone: required
    /// fields, id format, identify-key hygiene, per-map geometry / scaling /
    /// honesty labels, embedded-blob detection and the hard scope exclusions.
    /// Filename-consistency and real-image checks are separate
    /// ([`EcuDef::validate_filename`], [`EcuDef::validate_against_image`]) because
    /// they need inputs beyond the definition.
    pub fn validate(&self) -> Vec<ValidationIssue> {
        let mut out = Vec::new();
        self.check_required(&mut out);
        self.check_id_format(&mut out);
        self.check_identify(&mut out);
        self.check_checksum_field(&mut out);
        self.check_regions(&mut out);
        self.check_maps(&mut out);
        self.check_scope(&mut out);
        self.check_blobs(&mut out);
        out
    }

    fn check_required(&self, out: &mut Vec<ValidationIssue>) {
        for (val, name) in [
            (&self.id, "id"),
            (&self.name, "name"),
            (&self.maker, "maker"),
            (&self.family, "family"),
        ] {
            if val.trim().is_empty() {
                out.push(ValidationIssue::error(
                    "required.empty_field",
                    "",
                    format!("required field '{name}' is empty"),
                ));
            }
        }
        if self.bitrate == 0 {
            out.push(ValidationIssue::warn(
                "required.bitrate_zero",
                "",
                "bitrate is 0 — set the CAN bit/s or K-line baud",
            ));
        }
    }

    fn check_id_format(&self, out: &mut Vec<ValidationIssue>) {
        let id = &self.id;
        if id.is_empty() {
            return; // already reported by check_required
        }
        let ok_chars = id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !ok_chars {
            out.push(ValidationIssue::error(
                "id.charset",
                "",
                format!(
                    "id '{id}' must be lowercase a-z, 0-9 and '-' only (convention: <maker>-<family>[-<variant>])"
                ),
            ));
        }
        let segments: Vec<&str> = id.split('-').filter(|s| !s.is_empty()).collect();
        if segments.len() < 2 {
            out.push(ValidationIssue::error(
                "id.shape",
                "",
                format!("id '{id}' must have at least <maker>-<family> segments"),
            ));
        }
        if id.starts_with('-') || id.ends_with('-') || id.contains("--") {
            out.push(ValidationIssue::error(
                "id.shape",
                "",
                format!("id '{id}' has empty '-' segments"),
            ));
        }
    }

    fn check_identify(&self, out: &mut Vec<ValidationIssue>) {
        let i = &self.identify;
        if i.part_numbers.is_empty() && i.sw_numbers.is_empty() {
            out.push(ValidationIssue::warn(
                "identify.empty",
                "identify",
                "no part_numbers or sw_numbers — this definition cannot be auto-matched to an ECU",
            ));
        }
        for (field, keys) in [
            ("part_numbers", &i.part_numbers),
            ("sw_numbers", &i.sw_numbers),
        ] {
            let mut seen: Vec<String> = Vec::new();
            for (idx, k) in keys.iter().enumerate() {
                let loc = format!("identify.{field}[{idx}]");
                let n = norm_key(k);
                if n.is_empty() {
                    out.push(ValidationIssue::error(
                        "identify.empty_key",
                        loc.clone(),
                        "identify key is empty / whitespace",
                    ));
                    continue;
                }
                if !n.chars().all(|c| c.is_ascii_alphanumeric()) {
                    out.push(ValidationIssue::warn(
                        "identify.key_charset",
                        loc.clone(),
                        format!("identify key '{k}' has non-alphanumeric characters"),
                    ));
                }
                if n.len() < 3 {
                    out.push(ValidationIssue::warn(
                        "identify.key_short",
                        loc.clone(),
                        format!("identify key '{k}' is very short and may match unrelated ECUs"),
                    ));
                }
                if seen.contains(&n) {
                    out.push(ValidationIssue::warn(
                        "identify.duplicate_key",
                        loc,
                        format!("duplicate identify key '{k}'"),
                    ));
                } else {
                    seen.push(n);
                }
            }
        }
    }

    fn check_checksum_field(&self, out: &mut Vec<ValidationIssue>) {
        if let Some(c) = &self.checksum {
            if c.trim().is_empty() {
                out.push(ValidationIssue::error(
                    "checksum.empty",
                    "",
                    "checksum is an empty string — use null for 'no known checksum'",
                ));
            }
        }
    }

    fn check_regions(&self, out: &mut Vec<ValidationIssue>) {
        if self.flash_size == 0 {
            return;
        }
        // Regions may be expressed as CPU/logical addresses (EDC17/PCR2.1 use a
        // non-zero base); normalise to offsets using the first region's address.
        let base = self.regions.first().map(|r| r.region.address).unwrap_or(0);
        let image_end = base as u64 + self.flash_size as u64;
        for r in &self.regions {
            let end = r.region.address as u64 + r.region.length as u64;
            if r.region.address < base {
                continue; // mixed addressing; don't guess
            }
            if end > image_end {
                out.push(ValidationIssue::error(
                    "region.out_of_image",
                    format!("region '{}'", r.name),
                    format!(
                        "region ends at {} but the flash image ends at {}",
                        end, image_end
                    ),
                ));
            }
        }
    }

    fn check_maps(&self, out: &mut Vec<ValidationIssue>) {
        for (idx, m) in self.maps.iter().enumerate() {
            let loc = format!("map[{idx}] '{}'", m.name);
            check_one_map(m, &loc, self.flash_size, out);
        }
    }

    fn check_scope(&self, out: &mut Vec<ValidationIssue>) {
        // Build the full searchable text of the definition.
        let mut texts: Vec<(String, String)> = vec![
            ("id".into(), self.id.clone()),
            ("name".into(), self.name.clone()),
            ("maker".into(), self.maker.clone()),
            ("family".into(), self.family.clone()),
            ("notes".into(), self.notes.clone()),
        ];
        for (idx, m) in self.maps.iter().enumerate() {
            texts.push((format!("map[{idx}].name"), m.name.clone()));
            texts.push((format!("map[{idx}].notes"), m.notes.clone()));
        }
        for (idx, k) in self.identify.part_numbers.iter().enumerate() {
            texts.push((format!("identify.part_numbers[{idx}]"), k.clone()));
        }
        for (idx, k) in self.identify.sw_numbers.iter().enumerate() {
            texts.push((format!("identify.sw_numbers[{idx}]"), k.clone()));
        }
        for (loc, text) in &texts {
            let lower = text.to_ascii_lowercase();
            for (code, label) in scan_defeat(&lower) {
                out.push(ValidationIssue::error(
                    code,
                    loc.clone(),
                    format!(
                        "content names a forbidden capability ({label}); OpenTune is calibration-only (see README scope)"
                    ),
                ));
            }
        }
    }

    fn check_blobs(&self, out: &mut Vec<ValidationIssue>) {
        let mut fields: Vec<(String, &str)> = vec![("notes".into(), self.notes.as_str())];
        for (idx, m) in self.maps.iter().enumerate() {
            fields.push((format!("map[{idx}].notes"), m.notes.as_str()));
            fields.push((format!("map[{idx}].name"), m.name.as_str()));
        }
        for (loc, s) in fields {
            if s.len() > MAX_STRING_LEN {
                out.push(ValidationIssue::error(
                    "blob.oversized_field",
                    loc.clone(),
                    format!(
                        "text field is {} bytes (>{MAX_STRING_LEN}); possible embedded data",
                        s.len()
                    ),
                ));
            }
            let run = longest_blobby_run(s);
            if run >= MAX_B64_RUN {
                out.push(ValidationIssue::error(
                    "blob.base64_run",
                    loc,
                    format!(
                        "text contains a {run}-char unbroken alphanumeric run; possible embedded binary blob"
                    ),
                ));
            }
        }
        // Guard against a huge inline axis masquerading as data.
        for (idx, m) in self.maps.iter().enumerate() {
            for (axname, ax) in axes_of(m) {
                if ax.values.len() > 4096 {
                    out.push(ValidationIssue::error(
                        "blob.oversized_axis",
                        format!("map[{idx}].{axname}"),
                        format!(
                            "inline axis has {} values; implausibly large",
                            ax.values.len()
                        ),
                    ));
                }
            }
        }
    }

    /// Check that the definition's `id` matches its filename
    /// (`<id>.json`), the `<maker>-<family>[-<variant>].json` convention.
    pub fn validate_filename(&self, filename: &str) -> Vec<ValidationIssue> {
        let mut out = Vec::new();
        let stem = std::path::Path::new(filename)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(filename);
        if stem != self.id {
            out.push(ValidationIssue::error(
                "filename.id_mismatch",
                "",
                format!(
                    "filename stem '{stem}' does not match id '{}' (file must be '{}.json')",
                    self.id, self.id
                ),
            ));
        }
        out
    }

    /// Validate map/axis addresses against a real dump: every cell and stored
    /// axis must land inside the image, stored axes should be monotonic, and
    /// decoded cells should sit inside any declared `[min, max]`.
    pub fn validate_against_image(&self, image: &[u8]) -> Vec<ValidationIssue> {
        let mut out = Vec::new();
        for (idx, m) in self.maps.iter().enumerate() {
            let loc = format!("map[{idx}] '{}'", m.name);
            let n = (m.rows as usize).saturating_mul(m.cols as usize);
            let w = m.data_type.byte_len() as usize;
            let base = m.address as usize;
            let mut vals = Vec::with_capacity(n);
            let mut off_end_ok = true;
            for i in 0..n {
                match read_cell(image, base + i * w, m.data_type, m.endian) {
                    Some(v) => vals.push(m.scaling.to_physical(v)),
                    None => {
                        off_end_ok = false;
                        break;
                    }
                }
            }
            if !off_end_ok {
                out.push(ValidationIssue::error(
                    "image.map_out_of_range",
                    loc.clone(),
                    format!(
                        "map data runs past the end of the {}-byte image",
                        image.len()
                    ),
                ));
                continue;
            }
            // Cells inside declared bounds?
            if let (Some(lo), Some(hi)) = (min_of(&vals), max_of(&vals)) {
                if let Some(mn) = m.min {
                    if lo < mn - 1e-6 {
                        out.push(ValidationIssue::warn(
                            "image.below_min",
                            loc.clone(),
                            format!("decoded minimum {lo:.3} is below declared min {mn:.3}"),
                        ));
                    }
                }
                if let Some(mx) = m.max {
                    if hi > mx + 1e-6 {
                        out.push(ValidationIssue::warn(
                            "image.above_max",
                            loc.clone(),
                            format!("decoded maximum {hi:.3} is above declared max {mx:.3}"),
                        ));
                    }
                }
            }
            // Stored axes: range + monotonicity.
            for (axname, ax) in axes_of(m) {
                if let Some(addr) = ax.address {
                    let aw = ax.data_type.byte_len() as usize;
                    let mut av = Vec::with_capacity(ax.count as usize);
                    let mut ok = true;
                    for i in 0..ax.count as usize {
                        match read_cell(image, addr as usize + i * aw, ax.data_type, ax.endian) {
                            Some(v) => av.push(ax.scaling.to_physical(v)),
                            None => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if !ok {
                        out.push(ValidationIssue::error(
                            "image.axis_out_of_range",
                            format!("{loc} {axname}"),
                            "stored axis runs past the end of the image",
                        ));
                    } else if !is_strictly_monotonic(&av) {
                        out.push(ValidationIssue::warn(
                            "image.axis_not_monotonic",
                            format!("{loc} {axname}"),
                            "stored axis breakpoints are not strictly monotonic (check endian/type/address)",
                        ));
                    }
                }
            }
        }
        out
    }
}

fn axes_of(m: &MapDef) -> Vec<(&'static str, &AxisDef)> {
    let mut v = Vec::new();
    if let Some(x) = &m.x_axis {
        v.push(("x_axis", x));
    }
    if let Some(y) = &m.y_axis {
        v.push(("y_axis", y));
    }
    v
}

fn min_of(v: &[f64]) -> Option<f64> {
    v.iter()
        .copied()
        .fold(None, |acc, x| Some(acc.map_or(x, |a: f64| a.min(x))))
}

fn max_of(v: &[f64]) -> Option<f64> {
    v.iter()
        .copied()
        .fold(None, |acc, x| Some(acc.map_or(x, |a: f64| a.max(x))))
}

fn check_one_map(m: &MapDef, loc: &str, flash_size: u32, out: &mut Vec<ValidationIssue>) {
    if m.name.trim().is_empty() {
        out.push(ValidationIssue::error(
            "map.empty_name",
            loc,
            "map has an empty name",
        ));
    }
    if m.rows == 0 || m.cols == 0 {
        out.push(ValidationIssue::error(
            "map.zero_dimension",
            loc,
            format!(
                "map has a zero dimension (rows={}, cols={})",
                m.rows, m.cols
            ),
        ));
    }

    // Honesty label is mandatory on every map.
    if !has_confidence_label(&m.notes) {
        out.push(ValidationIssue::error(
            "honesty.missing_confidence",
            loc,
            "map notes must carry a confidence label (CONFIDENCE: HIGH/MEDIUM/LOW or EXAMPLE ONLY) with its source, per CONTRIBUTING-ECUDEFS.md",
        ));
    }

    // Geometry: map data within the declared image.
    if flash_size > 0 {
        let end = m.address as u64 + m.byte_len() as u64;
        if end > flash_size as u64 {
            out.push(ValidationIssue::error(
                "map.out_of_image",
                loc,
                format!(
                    "map data ends at {end} but flash_size is {flash_size}; fix the address or flash_size"
                ),
            ));
        }
    }

    // Scaling sanity.
    if m.scaling.factor == 0.0 {
        out.push(ValidationIssue::error(
            "map.zero_factor",
            loc,
            "scaling factor is 0 — the map would decode every cell to the offset",
        ));
    }
    if !m.scaling.factor.is_finite() || !m.scaling.offset.is_finite() {
        out.push(ValidationIssue::error(
            "map.nonfinite_scaling",
            loc,
            "scaling factor/offset is not finite",
        ));
    }
    if let Some((lo, hi)) = raw_extremes(m.data_type) {
        let p0 = m.scaling.to_physical(lo);
        let p1 = m.scaling.to_physical(hi);
        let absmax = p0.abs().max(p1.abs());
        if m.scaling.factor.is_finite() && absmax > IMPLAUSIBLE_ABS {
            out.push(ValidationIssue::warn(
                "map.implausible_range",
                loc,
                format!(
                    "scaling spans physical values up to {absmax:.3e}{} — check factor/data_type",
                    unit_suffix(&m.unit)
                ),
            ));
        }
    }

    // Unit present on the cells.
    if m.unit.trim().is_empty() {
        out.push(ValidationIssue::warn(
            "map.missing_unit",
            loc,
            "map has no unit",
        ));
    }

    // min/max ordering.
    if let (Some(mn), Some(mx)) = (m.min, m.max) {
        if mn > mx {
            out.push(ValidationIssue::error(
                "map.min_gt_max",
                loc,
                format!("min ({mn}) is greater than max ({mx})"),
            ));
        }
    }

    // Axes.
    check_axis(m.x_axis.as_ref(), "x_axis", m.cols, loc, out);
    check_axis(m.y_axis.as_ref(), "y_axis", m.rows, loc, out);
}

fn check_axis(
    ax: Option<&AxisDef>,
    axname: &str,
    dim: u32,
    loc: &str,
    out: &mut Vec<ValidationIssue>,
) {
    let Some(ax) = ax else { return };
    let aloc = format!("{loc} {axname}");

    // The axis must describe itself exactly one way: inline, stored or shared.
    let has_values = !ax.values.is_empty();
    let has_addr = ax.address.is_some();
    let has_ref = ax.axis_ref.is_some();
    let ways = [has_values, has_addr, has_ref]
        .iter()
        .filter(|b| **b)
        .count();
    if ways == 0 {
        out.push(ValidationIssue::warn(
            "axis.undefined",
            aloc.clone(),
            "axis has no values, address or axis_ref — breakpoints are undefined",
        ));
    } else if ways > 1 {
        out.push(ValidationIssue::warn(
            "axis.overdefined",
            aloc.clone(),
            "axis sets more than one of values/address/axis_ref; only one is used",
        ));
    }

    // count must match the map's size on this dimension.
    if ax.count != 0 && dim != 0 && ax.count != dim {
        out.push(ValidationIssue::error(
            "axis.count_mismatch",
            aloc.clone(),
            format!(
                "axis count {} does not match the map's {axname} size {dim}",
                ax.count
            ),
        ));
    }

    // inline values length must match count.
    if has_values && ax.count != 0 && ax.values.len() as u32 != ax.count {
        out.push(ValidationIssue::error(
            "axis.values_count_mismatch",
            aloc.clone(),
            format!(
                "axis has {} inline values but count is {}",
                ax.values.len(),
                ax.count
            ),
        ));
    }

    // inline monotonicity is checkable without an image.
    if has_values && !is_strictly_monotonic(&ax.values) {
        out.push(ValidationIssue::warn(
            "axis.not_monotonic",
            aloc.clone(),
            "inline axis breakpoints are not strictly monotonic",
        ));
    }

    if ax.unit.trim().is_empty() {
        out.push(ValidationIssue::warn(
            "axis.missing_unit",
            aloc,
            "axis has no unit",
        ));
    }
}

fn unit_suffix(unit: &str) -> String {
    if unit.trim().is_empty() {
        String::new()
    } else {
        format!(" {unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_def() -> EcuDef {
        // A minimal, valid, map-free definition.
        EcuDef::from_json(
            r#"{
            "id": "bosch-demo", "name": "Demo", "maker": "Bosch", "family": "DEMO",
            "notes": "public family facts",
            "protocol": "iso_tp", "diag_layer": "uds", "bitrate": 500000,
            "flash_size": 524288,
            "checksum": "edc16",
            "identify": { "part_numbers": ["0281011"], "sw_numbers": ["1037"] }
        }"#,
        )
        .expect("base parses")
    }

    fn codes(issues: &[ValidationIssue]) -> Vec<&str> {
        issues.iter().map(|i| i.code).collect()
    }

    #[test]
    fn clean_def_has_no_errors() {
        let d = base_def();
        let issues = d.validate();
        assert!(
            !has_errors(&issues),
            "clean def should have no errors, got {issues:?}"
        );
    }

    #[test]
    fn empty_required_field_errors() {
        let mut d = base_def();
        d.name = "  ".into();
        let issues = d.validate();
        assert!(codes(&issues).contains(&"required.empty_field"));
        assert!(has_errors(&issues));
    }

    #[test]
    fn bad_id_charset_errors() {
        let mut d = base_def();
        d.id = "Bosch_EDC16".into();
        let issues = d.validate();
        assert!(codes(&issues).contains(&"id.charset"));
    }

    #[test]
    fn single_segment_id_errors() {
        let mut d = base_def();
        d.id = "bosch".into();
        let issues = d.validate();
        assert!(codes(&issues).contains(&"id.shape"));
    }

    #[test]
    fn filename_must_match_id() {
        let d = base_def();
        assert!(d.validate_filename("bosch-demo.json").is_empty());
        let mism = d.validate_filename("something-else.json");
        assert_eq!(mism.len(), 1);
        assert_eq!(mism[0].code, "filename.id_mismatch");
    }

    #[test]
    fn empty_and_duplicate_identify_keys() {
        let mut d = base_def();
        d.identify.part_numbers = vec!["0281011".into(), "0281011".into(), "  ".into()];
        let issues = d.validate();
        assert!(codes(&issues).contains(&"identify.empty_key"));
        assert!(codes(&issues).contains(&"identify.duplicate_key"));
    }

    #[test]
    fn no_identify_keys_warns_not_errors() {
        let mut d = base_def();
        d.identify.part_numbers.clear();
        d.identify.sw_numbers.clear();
        let issues = d.validate();
        assert!(codes(&issues).contains(&"identify.empty"));
        assert!(!has_errors(&issues));
    }

    #[test]
    fn empty_checksum_string_errors() {
        let mut d = base_def();
        d.checksum = Some("".into());
        let issues = d.validate();
        assert!(codes(&issues).contains(&"checksum.empty"));
    }

    #[test]
    fn region_past_image_errors() {
        let json = r#"{
            "id": "bosch-demo", "name": "Demo", "maker": "Bosch", "family": "DEMO",
            "protocol": "iso_tp", "diag_layer": "uds", "bitrate": 500000,
            "flash_size": 524288,
            "regions": [{ "name": "full", "region": { "address": 0, "length": 1048576 }, "writable": false }]
        }"#;
        let d = EcuDef::from_json(json).unwrap();
        let issues = d.validate();
        assert!(codes(&issues).contains(&"region.out_of_image"));
    }

    fn map_def_json(notes: &str, address: u32, extra: &str) -> String {
        format!(
            r#"{{
            "id": "bosch-demo", "name": "Demo", "maker": "Bosch", "family": "DEMO",
            "protocol": "iso_tp", "diag_layer": "uds", "bitrate": 500000,
            "flash_size": 524288,
            "identify": {{ "part_numbers": ["0281011"] }},
            "maps": [{{
                "name": "smoke limiter", "notes": "{notes}",
                "address": {address}, "data_type": "u16", "endian": "big",
                "rows": 16, "cols": 16,
                "scaling": {{ "factor": 0.01, "offset": 0.0 }}, "unit": "mg/stroke"
                {extra}
            }}]
        }}"#
        )
    }

    #[test]
    fn map_without_confidence_label_errors() {
        let d = EcuDef::from_json(&map_def_json("just a map", 400000, "")).unwrap();
        let issues = d.validate();
        assert!(codes(&issues).contains(&"honesty.missing_confidence"));
    }

    #[test]
    fn map_with_confidence_label_ok() {
        let d = EcuDef::from_json(&map_def_json(
            "CONFIDENCE: MEDIUM. in-image header",
            400000,
            "",
        ))
        .unwrap();
        let issues = d.validate();
        assert!(!codes(&issues).contains(&"honesty.missing_confidence"));
    }

    #[test]
    fn map_out_of_image_errors() {
        // 16*16*2 = 512 bytes; address near the end overflows 524288.
        let d = EcuDef::from_json(&map_def_json("EXAMPLE ONLY", 524000, "")).unwrap();
        let issues = d.validate();
        assert!(codes(&issues).contains(&"map.out_of_image"));
    }

    #[test]
    fn zero_factor_errors() {
        let json =
            map_def_json("EXAMPLE ONLY", 400000, "").replace("\"factor\": 0.01", "\"factor\": 0.0");
        let d = EcuDef::from_json(&json).unwrap();
        let issues = d.validate();
        assert!(codes(&issues).contains(&"map.zero_factor"));
    }

    #[test]
    fn implausible_scaling_warns() {
        // u16 * 1000 -> up to ~6.5e7 physical, above the plausibility ceiling.
        let json = map_def_json("EXAMPLE ONLY", 400000, "")
            .replace("\"factor\": 0.01", "\"factor\": 1000.0");
        let d = EcuDef::from_json(&json).unwrap();
        let issues = d.validate();
        assert!(codes(&issues).contains(&"map.implausible_range"));
        assert!(!has_errors(&issues)); // it's a warning
    }

    #[test]
    fn axis_count_mismatch_errors() {
        let extra = r#", "x_axis": { "name": "rpm", "unit": "rpm", "count": 12, "address": 390000, "data_type": "u16", "endian": "big" }"#;
        let d = EcuDef::from_json(&map_def_json("EXAMPLE ONLY", 400000, extra)).unwrap();
        let issues = d.validate();
        assert!(codes(&issues).contains(&"axis.count_mismatch"));
    }

    #[test]
    fn inline_axis_non_monotonic_warns() {
        let extra = r#", "x_axis": { "name": "rpm", "unit": "rpm", "count": 16, "values": [0,1,2,3,4,5,6,7,8,9,10,11,12,13,99,14] }"#;
        let d = EcuDef::from_json(&map_def_json("EXAMPLE ONLY", 400000, extra)).unwrap();
        let issues = d.validate();
        assert!(codes(&issues).contains(&"axis.not_monotonic"));
    }

    #[test]
    fn scope_immobiliser_defeat_errors() {
        let mut d = base_def();
        d.notes = "This definition enables IMMO OFF for the cluster".into();
        let issues = d.validate();
        assert!(codes(&issues).contains(&"scope.immobiliser_defeat"));
        assert!(has_errors(&issues));
    }

    #[test]
    fn scope_emissions_defeat_errors() {
        let mut d = base_def();
        d.name = "EDC17 DPF delete pack".into();
        let issues = d.validate();
        assert!(codes(&issues).contains(&"scope.emissions_defeat"));
    }

    #[test]
    fn scope_odometer_correction_errors() {
        let mut d = base_def();
        d.notes = "odometer correction supported".into();
        let issues = d.validate();
        assert!(codes(&issues).contains(&"scope.odometer_correction"));
    }

    #[test]
    fn legit_egr_map_name_is_not_a_scope_hit() {
        // An EGR *rate* calibration map is legal; only "egr off/delete" is not.
        let mut d = base_def();
        d.notes = "touches the gear/EGR/condition limiter family".into();
        let issues = d.validate();
        assert!(!codes(&issues).contains(&"scope.emissions_defeat"));
    }

    #[test]
    fn embedded_base64_blob_errors() {
        let mut d = base_def();
        d.notes = "A".repeat(300);
        let issues = d.validate();
        assert!(codes(&issues).contains(&"blob.base64_run"));
    }

    #[test]
    fn image_decode_out_of_range_errors() {
        let d = EcuDef::from_json(&map_def_json("EXAMPLE ONLY", 400000, "")).unwrap();
        // Image smaller than the map's extent.
        let small = vec![0u8; 1024];
        let issues = d.validate_against_image(&small);
        assert!(codes(&issues).contains(&"image.map_out_of_range"));
    }

    #[test]
    fn image_decode_monotonic_axis_ok() {
        // Build a 1x4 map with a stored, monotonic x-axis and decode it.
        let extra = r#", "x_axis": { "name": "rpm", "unit": "rpm", "count": 4, "address": 0, "data_type": "u16", "endian": "big" }"#;
        let json = format!(
            r#"{{
            "id": "bosch-demo", "name": "Demo", "maker": "Bosch", "family": "DEMO",
            "protocol": "iso_tp", "diag_layer": "uds", "bitrate": 500000,
            "flash_size": 64,
            "identify": {{ "part_numbers": ["0281011"] }},
            "maps": [{{
                "name": "curve", "notes": "EXAMPLE ONLY",
                "address": 8, "data_type": "u16", "endian": "big",
                "rows": 1, "cols": 4,
                "scaling": {{ "factor": 1.0 }}, "unit": "x"
                {extra}
            }}]
        }}"#
        );
        let d = EcuDef::from_json(&json).unwrap();
        // 64-byte image: x-axis (4 u16 at 0) = 0,1,2,3 monotonic; map cells at 8.
        let mut img = vec![0u8; 64];
        for (i, v) in [0u16, 1, 2, 3].iter().enumerate() {
            img[i * 2..i * 2 + 2].copy_from_slice(&v.to_be_bytes());
        }
        let issues = d.validate_against_image(&img);
        assert!(
            !has_errors(&issues),
            "monotonic in-range axis should be clean, got {issues:?}"
        );
        assert!(!codes(&issues).contains(&"image.axis_not_monotonic"));
    }
}
