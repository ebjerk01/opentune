//! TunerPro **XDF → OpenTune** map-definition importer.
//!
//! TunerPro's `.xdf` files are community-authored descriptions of where the
//! calibration tables live inside an ECU flash image and how to decode them.
//! There is a very large public library of them (EDC15/16/17, ME7, GM, etc.).
//! This crate reads an XDF and produces a list of
//! [`opentune_core::ecudef::MapDef`] values ready to be merged into an OpenTune
//! ECU definition.
//!
//! Two on-disk encodings exist in the wild and both are handled:
//!
//! * **Modern XML XDF** (TunerPro v5), the primary target of this crate:
//!   `<XDFFORMAT>` → `<XDFTABLE>` → `<XDFAXIS id="x|y|z">` with
//!   `<EMBEDDEDDATA .../>`, `<MATH equation="..."/>`, `<LABEL .../>`,
//!   `<units>`. Parsed by [`xml`].
//! * **Legacy text XDF** (`XDF\n1.11…`, the older `%%TABLE%%` key/value
//!   format). Still common in older community archives. Parsed by [`legacy`].
//!
//! [`parse_xdf`] sniffs the input and dispatches to the right parser.
//!
//! ## Scope / honesty
//!
//! OpenTune scaling is strictly linear (`physical = raw * factor + offset`).
//! A non-linear `<MATH>`/`ZEq` equation is imported **pre-scaled**
//! (`factor = 1`, `offset = 0`) with the original equation recorded in `notes`
//! — never guessed. Every table, function, constant, flag, patch and checksum
//! block ends in one [`report::ObjectReport`] (clean / warned / refused with a
//! reason) in [`XdfImport::reports`]; warnings are also written into the map's
//! `notes`. A construct that would otherwise produce a silently wrong address
//! or cell decoding (non-contiguous strides, unsupported widths, data outside
//! the declared image, an un-rebasable address) is refused, never
//! approximated. [`XdfImport::into_strict`] turns any warning or refusal on a
//! table/function/constant into an error.

use opentune_core::ecudef::{DataType, Endian, MapDef, Scaling};
use std::fmt;

pub mod export;
pub mod legacy;
pub mod math;
pub mod report;
pub mod xml;

pub use export::export_xdf;
pub use report::{ObjectKind, ObjectReport, Status, Summary};

/// Result of importing one XDF file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct XdfImport {
    /// Definition title from the XDF header, if present.
    pub title: Option<String>,
    /// Base offset declared in the XDF header. For a modern XDF this is the
    /// `<BASEOFFSET offset=.. subtract=..>` magnitude; for a legacy XDF it is
    /// `BaseOffset`. It has already been applied to every `MapDef.address`, and
    /// is kept here so the maintainer can see / re-derive the mapping.
    pub base_offset: u32,
    /// Whether [`base_offset`](Self::base_offset) is subtracted (a
    /// `BASEOFFSET subtract="1"` or a negative text-form `<baseoffset>`).
    pub base_subtract: bool,
    /// The imported maps, one per importable `<XDFTABLE>` / `<XDFFUNCTION>` /
    /// `%%TABLE%%` (functions become 1×N curves).
    pub maps: Vec<MapDef>,
    /// Scalar constants imported from `<XDFCONSTANT>` entries (modern XML only).
    /// These are single values, not tables, so they are kept in their own list
    /// rather than shoehorned into [`maps`](Self::maps). Empty for legacy files.
    pub constants: Vec<XdfConstant>,
    /// Structured, file-level import-loss / fidelity warnings that do not belong
    /// to any single map's `notes` — e.g. a table skipped because its address
    /// rebasing underflowed, categorical axis labels that the numeric axis model
    /// cannot store, or a swap-XY storage flag OpenTune does not yet model. A
    /// caller should surface these so a seemingly complete import does not hide a
    /// silent loss. Empty means nothing lossy was detected. One line per issue
    /// of every non-clean entry in [`reports`](Self::reports).
    pub warnings: Vec<String>,
    /// Per-object outcome (clean / warned / refused + reasons) for every
    /// table, function, constant, flag, patch and checksum block, in document
    /// order.
    pub reports: Vec<ObjectReport>,
}

impl XdfImport {
    /// Counts of clean / warned / refused objects and the issue-kind histogram.
    pub fn summary(&self) -> Summary {
        report::summarize(&self.reports)
    }

    /// Why a strict import would fail (one line per non-clean table, function
    /// or constant). Empty means the file imports cleanly.
    pub fn strict_failures(&self) -> Vec<String> {
        report::strict_failures(&self.reports)
    }

    /// Strict mode: `Err(Error::Strict)` unless every table, function and
    /// constant imported cleanly. Out-of-scope objects (flags, patches,
    /// checksums) are refused-with-reason in [`reports`](Self::reports) but do
    /// not fail strict mode.
    pub fn into_strict(self) -> Result<Self> {
        let f = self.strict_failures();
        if f.is_empty() {
            Ok(self)
        } else {
            Err(Error::Strict(f))
        }
    }
}

/// One warning line per issue of every non-clean report.
pub(crate) fn warning_lines(reports: &[ObjectReport]) -> Vec<String> {
    reports
        .iter()
        .filter(|r| r.status != Status::Clean)
        .flat_map(|r| {
            r.issues
                .iter()
                .map(move |i| format!("{} \"{}\": {i}", r.object, r.name))
        })
        .collect()
}

/// A scalar constant imported from an `<XDFCONSTANT>`.
///
/// Unlike a [`MapDef`], a constant is a single stored value (`rows == cols == 1`
/// conceptually): one address, one element, an optional linear scaling and unit.
/// Switches/bitfields are *not* imported as constants — only plain numeric
/// scalars — so this never silently turns a bitfield into an unrestricted write.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct XdfConstant {
    /// Constant name from `<title>` (falls back to `const_<uniqueid>`).
    pub name: String,
    /// Description + any uncertainty/limitation flags, same convention as a map.
    pub notes: String,
    /// Address of the value in the flash image, resolved through the base offset.
    pub address: u32,
    /// Stored element type.
    pub data_type: DataType,
    /// Byte order of the stored element.
    pub endian: Endian,
    /// Linear raw→physical scaling recovered from the `<MATH>` equation.
    pub scaling: Scaling,
    /// Physical unit, if any.
    pub unit: String,
    /// Optional lower physical bound (`<min>` / `<rangelow>`).
    pub min: Option<f64>,
    /// Optional upper physical bound (`<max>` / `<rangehigh>`).
    pub max: Option<f64>,
}

/// Errors from importing an XDF.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// The input is neither recognizable XML nor legacy text XDF.
    UnknownFormat,
    /// The XML did not parse.
    Xml(String),
    /// The document parsed but contained no usable table.
    Empty,
    /// A binary (pre-XML) TunerPro definition; only XML and the legacy text
    /// format are readable.
    Binary,
    /// Strict mode: at least one table/function/constant was warned or
    /// refused (one line each).
    Strict(Vec<String>),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnknownFormat => write!(f, "input is not a recognizable TunerPro XDF file"),
            Error::Xml(e) => write!(f, "XDF XML parse error: {e}"),
            Error::Empty => write!(f, "XDF contained no importable tables"),
            Error::Binary => write!(
                f,
                "binary TunerPro definition (pre-XML format) is not supported; open it in \
                 TunerPro and save it as an XML .xdf"
            ),
            Error::Strict(lines) => write!(
                f,
                "strict import refused: {} object(s) not imported cleanly:\n  {}",
                lines.len(),
                lines.join("\n  ")
            ),
        }
    }
}

impl std::error::Error for Error {}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Parse a TunerPro XDF document (modern XML **or** legacy text) into an
/// [`XdfImport`].
///
/// The format is detected from the content: a leading `<` / `<?xml` /
/// `<XDFFORMAT` is treated as XML; a leading `XDF` magic or a `%%TABLE%%`
/// marker is treated as the legacy text format.
pub fn parse_xdf(src: &str) -> Result<XdfImport> {
    let head = src.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']);
    let looks_xml = head.starts_with('<');
    let looks_legacy =
        head.starts_with("XDF") || head.contains("%%TABLE%%") || head.contains("%%HEADER%%");

    if looks_xml {
        xml::parse(src)
    } else if looks_legacy {
        legacy::parse(src)
    } else if head.contains("<XDFFORMAT") || head.contains("<XDFTABLE") {
        xml::parse(src)
    } else if head
        .chars()
        .take(64)
        .any(|c| c == '\0' || (c.is_control() && !c.is_whitespace()))
    {
        Err(Error::Binary)
    } else {
        Err(Error::UnknownFormat)
    }
}

/// [`parse_xdf`] in strict mode: refuses the whole file unless every table,
/// function and constant imports cleanly (see [`XdfImport::into_strict`]).
pub fn parse_xdf_strict(src: &str) -> Result<XdfImport> {
    parse_xdf(src)?.into_strict()
}

// ---------------------------------------------------------------------------
// Shared decoding helpers used by both the XML and the legacy parsers.
// ---------------------------------------------------------------------------

/// Parse a numeric string that may be decimal or `0x`-prefixed hex.
pub(crate) fn parse_num_u64(s: &str) -> Option<u64> {
    let t = s.trim();
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(h, 16).ok()
    } else {
        t.parse::<u64>().ok().or_else(|| {
            // Some files write signed or float-ish counts; fall back via f64.
            t.parse::<f64>().ok().map(|v| v as u64)
        })
    }
}

/// Parse a possibly-hex unsigned 32-bit value, saturating.
pub(crate) fn parse_u32(s: &str) -> Option<u32> {
    parse_num_u64(s).map(|v| v.min(u32::MAX as u64) as u32)
}

/// Parse a signed decimal or (`-`)`0x`-hex integer (base offsets, strides).
pub(crate) fn parse_i64(s: &str) -> Option<i64> {
    let t = s.trim();
    let (neg, body) = match t.strip_prefix('-') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let mag = if let Some(h) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        i64::from_str_radix(h, 16).ok()?
    } else {
        body.parse::<i64>().ok()?
    };
    Some(if neg { -mag } else { mag })
}

/// TunerPro `mmedtypeflags` bit meanings, as interpreted by this crate. These
/// are the community-documented readings; endianness in particular is a
/// best-effort interpretation (see the crate README) and is flagged in a map's
/// `notes` when it had to be defaulted.
pub mod typeflags {
    /// Stored value is signed (two's complement).
    pub const SIGNED: u32 = 0x01;
    /// Little-endian (LSB first / Intel). When clear, big-endian (Motorola),
    /// which is the XDF/Bosch default.
    pub const LSB_FIRST: u32 = 0x02;
    /// Stored value is an IEEE-754 float (used with a 32-bit element size).
    pub const FLOAT: u32 = 0x10000;
    /// Swap-XY: the table's data is stored transposed relative to the row-major
    /// (row = y, col = x, column-fastest) convention OpenTune reads with — i.e.
    /// the rows and columns are swapped in storage order. RomRaider's XDF
    /// converter honours this bit by setting `swapxy=true` on the table. OpenTune
    /// does not yet model a per-map storage orientation, so this crate *detects*
    /// and *flags* the bit (orientation unverified) but never silently
    /// transposes a map. See the README and the swap-flag test.
    pub const SWAP_XY: u32 = 0x04;
}

/// Derive a [`DataType`] from an element size in bits plus the type flags.
///
/// Only exactly representable cells map: 8/16/32-bit integers and 32-bit
/// IEEE floats. Anything else (24-bit, 64-bit, a float flag on a non-32-bit
/// cell, size 0) is `None` — the caller refuses rather than approximates.
pub(crate) fn datatype_from(size_bits: u32, flags: u32) -> Option<DataType> {
    let signed = flags & typeflags::SIGNED != 0;
    let is_float = flags & typeflags::FLOAT != 0;
    Some(match (size_bits, signed, is_float) {
        (32, _, true) => DataType::F32,
        (_, _, true) => return None,
        (8, false, _) => DataType::U8,
        (8, true, _) => DataType::S8,
        (16, false, _) => DataType::U16,
        (16, true, _) => DataType::S16,
        (32, false, _) => DataType::U32,
        (32, true, _) => DataType::S32,
        _ => return None,
    })
}

/// Derive an [`Endian`] from the type flags.
pub(crate) fn endian_from(flags: u32) -> Endian {
    if flags & typeflags::LSB_FIRST != 0 {
        Endian::Little
    } else {
        Endian::Big
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datatype_mapping() {
        assert_eq!(datatype_from(8, 0), Some(DataType::U8));
        assert_eq!(datatype_from(8, typeflags::SIGNED), Some(DataType::S8));
        assert_eq!(datatype_from(16, 0), Some(DataType::U16));
        assert_eq!(datatype_from(16, typeflags::SIGNED), Some(DataType::S16));
        assert_eq!(datatype_from(32, 0), Some(DataType::U32));
        assert_eq!(datatype_from(32, typeflags::SIGNED), Some(DataType::S32));
        assert_eq!(datatype_from(32, typeflags::FLOAT), Some(DataType::F32));
        assert_eq!(datatype_from(0, 0), None);
        assert_eq!(
            datatype_from(24, 0),
            None,
            "24-bit must not be approximated"
        );
        assert_eq!(datatype_from(64, typeflags::FLOAT), None);
        assert_eq!(datatype_from(16, typeflags::FLOAT), None);
    }

    #[test]
    fn signed_parsing() {
        assert_eq!(parse_i64("-49152"), Some(-49152));
        assert_eq!(parse_i64("0x10"), Some(16));
        assert_eq!(parse_i64("-0x10"), Some(-16));
        assert_eq!(parse_i64("+5"), Some(5));
        assert_eq!(parse_i64("x"), None);
    }

    #[test]
    fn binary_xdf_is_named_not_unknown() {
        let s = String::from_utf8_lossy(&[0x05, b'"', 0xE3, 0x00, 0x01, 0x02]).into_owned();
        assert_eq!(parse_xdf(&s), Err(Error::Binary));
    }

    #[test]
    fn endian_mapping() {
        assert_eq!(endian_from(0), Endian::Big);
        assert_eq!(endian_from(typeflags::LSB_FIRST), Endian::Little);
        assert_eq!(endian_from(typeflags::SIGNED), Endian::Big);
    }

    #[test]
    fn number_parsing() {
        assert_eq!(parse_u32("0x288"), Some(0x288));
        assert_eq!(parse_u32("648"), Some(648));
        assert_eq!(parse_num_u64("0X10"), Some(16));
    }

    #[test]
    fn unknown_format_rejected() {
        assert_eq!(parse_xdf("hello world"), Err(Error::UnknownFormat));
    }

    #[test]
    fn malformed_inputs_are_clean_errors() {
        // Truncated/garbage XML and legacy markers must return Err, not panic.
        for s in [
            "",
            "<",
            "<?xml",
            "<XDFFORMAT>",
            "<XDFFORMAT><XDFTABLE></XDFTABLE></XDFFORMAT>",
            "%%TABLE%%",
            "%%TABLE%%\n\t040100 Address =0x\n%%END%%",
            "XDF\n1.11\n%%TABLE%%",
        ] {
            let _ = parse_xdf(s); // must not panic
        }
    }

    #[test]
    fn fuzz_random_xdf_never_panics() {
        // Deterministic fuzz over both parser paths. Each input only needs to not
        // panic/overflow/hang; a few hundred iterations keep CI cheap.
        let mut state: u64 = 0x5844_4600_2026_1116;
        let mut rng = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let seeds: [&str; 2] = [
            r#"<XDFFORMAT version="1.60"><XDFTABLE uniqueid="0x1"><title>t</title><XDFAXIS id="z"><EMBEDDEDDATA mmedaddress="0x10" mmedelementsizebits="16" mmedrowcount="4" mmedcolcount="4"/><MATH equation="X*2+1"/></XDFAXIS></XDFTABLE></XDFFORMAT>"#,
            "XDF\n1.11\n%%TABLE%%\n\t040100 Address =0x288\n\t040300 Rows =0x10\n\t040200 ZEq =(45*x)/127,TH\n%%END%%\n",
        ];
        for _ in 0..512 {
            let seed = seeds[(rng() as usize) % seeds.len()];
            let mut buf = seed.as_bytes().to_vec();
            let edits = (rng() % 48) as usize;
            for _ in 0..edits {
                if buf.is_empty() {
                    break;
                }
                let idx = (rng() as usize) % buf.len();
                buf[idx] = (rng() & 0xff) as u8;
            }
            let s = String::from_utf8_lossy(&buf);
            let _ = parse_xdf(&s);
        }
    }
}
