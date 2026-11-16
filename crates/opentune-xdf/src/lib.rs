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
//! — never guessed. Uncertain or defaulted fields (missing element size,
//! defaulted endianness, exotic strides) are likewise flagged in `notes`.

use opentune_core::ecudef::{DataType, Endian, MapDef, Scaling};
use std::fmt;

pub mod export;
pub mod legacy;
pub mod math;
pub mod xml;

pub use export::export_xdf;

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
    /// The imported maps, one per `<XDFTABLE>` / `%%TABLE%%`.
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
    /// silent loss. Empty means nothing lossy was detected.
    pub warnings: Vec<String>,
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
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnknownFormat => write!(f, "input is not a recognizable TunerPro XDF file"),
            Error::Xml(e) => write!(f, "XDF XML parse error: {e}"),
            Error::Empty => write!(f, "XDF contained no importable tables"),
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
    } else {
        Err(Error::UnknownFormat)
    }
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
/// Unknown / zero sizes default to `U8`. A float flag on a 32-bit element
/// yields `F32`.
pub(crate) fn datatype_from(size_bits: u32, flags: u32) -> (DataType, bool) {
    let signed = flags & typeflags::SIGNED != 0;
    let is_float = flags & typeflags::FLOAT != 0;
    let dt = match (size_bits, signed, is_float) {
        (_, _, true) => DataType::F32,
        (0 | 8, false, _) => DataType::U8,
        (8, true, _) => DataType::S8,
        (16, false, _) => DataType::U16,
        (16, true, _) => DataType::S16,
        (32, false, _) => DataType::U32,
        (32, true, _) => DataType::S32,
        // Unusual width (24-bit etc.) — approximate by the next type up and let
        // the caller note it.
        (w, false, _) if w <= 16 => DataType::U16,
        (_, false, _) => DataType::U32,
        (w, true, _) if w <= 16 => DataType::S16,
        (_, true, _) => DataType::S32,
    };
    let exact = matches!(size_bits, 0 | 8 | 16 | 32);
    (dt, exact)
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
        assert_eq!(datatype_from(8, 0).0, DataType::U8);
        assert_eq!(datatype_from(8, typeflags::SIGNED).0, DataType::S8);
        assert_eq!(datatype_from(16, 0).0, DataType::U16);
        assert_eq!(datatype_from(16, typeflags::SIGNED).0, DataType::S16);
        assert_eq!(datatype_from(32, 0).0, DataType::U32);
        assert_eq!(datatype_from(32, typeflags::SIGNED).0, DataType::S32);
        assert_eq!(datatype_from(32, typeflags::FLOAT).0, DataType::F32);
        assert_eq!(datatype_from(0, 0).0, DataType::U8);
        assert!(!datatype_from(24, 0).1, "24-bit must be marked inexact");
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
