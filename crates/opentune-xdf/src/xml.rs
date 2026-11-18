//! Parser for the **modern XML** TunerPro XDF format (TunerPro v5).
//!
//! Every `<XDFTABLE>`, `<XDFFUNCTION>`, `<XDFCONSTANT>`, `<XDFFLAG>`,
//! `<XDFPATCH>` and `<XDFCHECKSUM>` ends in one [`ObjectReport`]: clean,
//! warned (imported with an explicit fidelity loss) or refused (with the
//! reason). The rules — which construct is faithful, warned or refused — are
//! documented in the crate README and `docs/MAP-DEFS.md`.
//!
//! Shape handled:
//! ```xml
//! <XDFFORMAT version="1.60">
//!   <XDFHEADER>
//!     <deftitle>...</deftitle>
//!     <BASEOFFSET offset="0" subtract="0"/>
//!     <DEFAULTS ... lsbfirst="0"/>
//!   </XDFHEADER>
//!   <XDFTABLE uniqueid="0x..">
//!     <title>...</title>
//!     <description>...</description>
//!     <XDFAXIS id="x"> ... </XDFAXIS>
//!     <XDFAXIS id="y"> ... </XDFAXIS>
//!     <XDFAXIS id="z">
//!       <EMBEDDEDDATA mmedaddress="0x288" mmedelementsizebits="16"
//!                     mmedrowcount="16" mmedcolcount="11" mmedtypeflags="0x02"/>
//!       <units>deg</units>
//!       <MATH equation="(45*X)/127-20"/>
//!       <min>..</min><max>..</max>
//!     </XDFAXIS>
//!   </XDFTABLE>
//! </XDFFORMAT>
//! ```

use crate::math::{linear_from_equation, LinearFit};
use crate::report::{kinds, ObjectKind, ObjectReport, Tracker};
use crate::{
    datatype_from, endian_from, parse_i64, parse_u32, typeflags, Error, Result, XdfConstant,
    XdfImport,
};
use opentune_core::ecudef::{AxisDef, DataType, Endian, MapDef, Scaling};
use roxmltree::{Document, Node};
use std::collections::HashMap;

/// A problem found while decoding: (issue kind, message). The caller decides
/// whether it is a warning, a refusal, or an axis-level loss.
type Problem = (&'static str, String);

/// Header-level context every object is decoded against.
struct Ctx<'a, 'i> {
    /// Signed offset added to every `mmedaddress` (`BASEOFFSET`).
    base: i64,
    /// `DEFAULTS lsbfirst`.
    default_lsb: bool,
    /// `DEFAULTS signed`.
    default_signed: bool,
    /// `DEFAULTS float`.
    default_float: bool,
    /// `DEFAULTS datasizeinbits`.
    default_size_bits: Option<u32>,
    /// Size of the binary image when the header declares a whole-file
    /// `REGION` starting at 0.
    image_size: Option<u64>,
    /// `CATEGORY index` → name.
    categories: HashMap<u32, String>,
    /// Objects by non-zero `uniqueid` (for linked axes); `None` = id is shared
    /// by several objects, so a link to it is ambiguous.
    by_id: HashMap<u32, Option<Node<'a, 'i>>>,
}

/// Decoded storage of one `<EMBEDDEDDATA>`.
#[derive(Debug, Clone, Copy)]
struct Cell {
    data_type: DataType,
    endian: Endian,
    column_major: bool,
    size_bits: u32,
}

impl Cell {
    fn bytes(&self) -> u64 {
        u64::from(self.size_bits / 8)
    }
}

/// Parse a modern XML XDF document.
pub fn parse(src: &str) -> Result<XdfImport> {
    let doc = Document::parse(src).map_err(|e| Error::Xml(e.to_string()))?;
    let root = doc.root_element();

    // The root is normally <XDFFORMAT>; tolerate the document element being the
    // table itself or a wrapper by searching descendants for objects regardless.
    let header = descendant(root, "XDFHEADER");

    let title = header
        .and_then(|h| child_text(h, "deftitle").or_else(|| child_text(h, "DefTitle")))
        .filter(|s| !s.is_empty());

    let (base_offset, base_subtract, base) = header_base(header);

    let defaults = header.and_then(|h| child(h, "DEFAULTS"));
    let dflag = |name: &str| {
        defaults
            .and_then(|d| attr(d, name))
            .and_then(parse_u32)
            .map(|v| v != 0)
            .unwrap_or(false)
    };
    let mut categories = HashMap::new();
    if let Some(h) = header {
        for c in h.children().filter(|n| has_tag(*n, "CATEGORY")) {
            if let (Some(i), Some(name)) = (attr(c, "index").and_then(parse_u32), attr(c, "name")) {
                categories.insert(i, name.trim().to_string());
            }
        }
    }
    let mut by_id: HashMap<u32, Option<Node>> = HashMap::new();
    for n in root.descendants().filter(|n| {
        has_tag(*n, "XDFTABLE") || has_tag(*n, "XDFFUNCTION") || has_tag(*n, "XDFCONSTANT")
    }) {
        if let Some(id) = attr(n, "uniqueid").and_then(parse_u32).filter(|i| *i != 0) {
            by_id.entry(id).and_modify(|e| *e = None).or_insert(Some(n));
        }
    }
    let ctx = Ctx {
        base,
        default_lsb: dflag("lsbfirst"),
        default_signed: dflag("signed"),
        default_float: dflag("float"),
        default_size_bits: defaults
            .and_then(|d| attr(d, "datasizeinbits"))
            .and_then(parse_u32)
            .filter(|v| *v > 0),
        image_size: header.and_then(image_region_size),
        categories,
        by_id,
    };

    let mut out = XdfImport {
        title,
        base_offset,
        base_subtract,
        ..Default::default()
    };
    let mut any_object = false;
    for n in root.descendants().filter(|n| n.is_element()) {
        let tag = n.tag_name().name().to_ascii_uppercase();
        let report = match tag.as_str() {
            "XDFTABLE" => {
                let (m, r) = parse_table(n, &ctx);
                out.maps.extend(m);
                r
            }
            "XDFFUNCTION" => {
                let (m, r) = parse_function(n, &ctx);
                out.maps.extend(m);
                r
            }
            "XDFCONSTANT" => {
                let (c, r) = parse_constant(n, &ctx);
                out.constants.extend(c);
                r
            }
            "XDFFLAG" => out_of_scope(n, ObjectKind::Flag, flag_reason(n)),
            "XDFPATCH" => out_of_scope(
                n,
                ObjectKind::Patch,
                format!(
                    "byte patch ({} entr(y/ies)) — a patch is a raw byte replacement, not a \
                     map definition; not imported",
                    n.children()
                        .filter(|c| has_tag(*c, "XDFPATCHENTRY"))
                        .count()
                ),
            ),
            "XDFCHECKSUM" => out_of_scope(
                n,
                ObjectKind::Checksum,
                "checksum descriptor — OpenTune corrects checksums only with its own validated \
                 per-ECU handlers (opentune-checksum); not imported"
                    .into(),
            ),
            _ => continue,
        };
        any_object = true;
        out.reports.push(report);
    }

    if !any_object {
        return Err(Error::Empty);
    }
    out.warnings = crate::warning_lines(&out.reports);
    Ok(out)
}

/// `<BASEOFFSET offset=".." subtract="0|1"/>` (TunerPro 5) or the older text
/// form `<baseoffset>-49152</baseoffset>` (signed). Returns
/// `(magnitude, subtract, signed offset)`.
fn header_base(header: Option<Node>) -> (u32, bool, i64) {
    let Some(b) = header.and_then(|h| child(h, "BASEOFFSET")) else {
        return (0, false, 0);
    };
    let signed = if let Some(off) = attr(b, "offset") {
        let mag = parse_i64(off).unwrap_or(0);
        let sub = attr(b, "subtract")
            .and_then(parse_u32)
            .map(|v| v != 0)
            .unwrap_or(false);
        if sub {
            -mag
        } else {
            mag
        }
    } else {
        b.text().and_then(parse_i64).unwrap_or(0)
    };
    let mag = signed.unsigned_abs().min(u64::from(u32::MAX)) as u32;
    (mag, signed < 0, signed)
}

/// The size of the whole-file `REGION` (the one TunerPro writes as
/// `type="0xFFFFFFFF" startaddress="0x0" size=".."`), if declared.
fn image_region_size(header: Node) -> Option<u64> {
    let regions: Vec<Node> = header
        .children()
        .filter(|n| has_tag(*n, "REGION"))
        .collect();
    let whole: Vec<&Node> = regions
        .iter()
        .filter(|r| {
            attr(**r, "type").and_then(parse_u32) == Some(0xFFFF_FFFF)
                && attr(**r, "startaddress").and_then(parse_u32).unwrap_or(0) == 0
        })
        .collect();
    match whole.as_slice() {
        [r] => attr(**r, "size")
            .and_then(parse_u32)
            .filter(|s| *s > 0)
            .map(u64::from),
        _ => None,
    }
}

/// Apply the header base offset with checked arithmetic.
fn rebase(mmed: u32, ctx: &Ctx) -> std::result::Result<u32, Problem> {
    let a = i64::from(mmed) + ctx.base;
    if a < 0 {
        Err((
            kinds::ADDRESS_REBASE,
            format!(
                "address 0x{mmed:X} could not be rebased against base offset {} (subtract \
                 underflow)",
                ctx.base
            ),
        ))
    } else if a > i64::from(u32::MAX) {
        Err((
            kinds::ADDRESS_REBASE,
            format!(
                "address 0x{mmed:X} could not be rebased against base offset {} (add overflow)",
                ctx.base
            ),
        ))
    } else {
        Ok(a as u32)
    }
}

/// Check `[addr, addr+len)` against the declared image size.
fn within_image(addr: u32, len: u64, ctx: &Ctx) -> std::result::Result<(), Problem> {
    match ctx.image_size {
        Some(size) if u64::from(addr) + len > size => Err((
            kinds::ADDRESS_OUTSIDE_IMAGE,
            format!(
                "data 0x{addr:X}..0x{:X} lies outside the {size:#X}-byte binary region the XDF \
                 declares",
                u64::from(addr) + len
            ),
        )),
        _ => Ok(()),
    }
}

/// Decode the element type / byte order / orientation of an `<EMBEDDEDDATA>`.
/// `Ok` carries warnings to record; `Err` means the cells cannot be decoded.
fn decode_storage(emb: Node, ctx: &Ctx) -> std::result::Result<(Cell, Vec<Problem>), Problem> {
    let mut warns = Vec::new();
    let size_bits = match attr(emb, "mmedelementsizebits") {
        Some(s) => parse_u32(s).ok_or((
            kinds::ELEMENT_SIZE_MISSING,
            format!("unparsable mmedelementsizebits `{s}`"),
        ))?,
        None => match ctx.default_size_bits {
            Some(d) => {
                warns.push((
                    kinds::ELEMENT_SIZE_DEFAULTED,
                    format!("no mmedelementsizebits; used the header default {d} bits"),
                ));
                d
            }
            None => {
                return Err((
                    kinds::ELEMENT_SIZE_MISSING,
                    "no mmedelementsizebits and no header default".into(),
                ))
            }
        },
    };
    let flags_raw = attr(emb, "mmedtypeflags");
    let flags =
        match flags_raw {
            Some(s) => parse_u32(s).ok_or((
                kinds::TYPEFLAGS_UNKNOWN,
                format!("unparsable mmedtypeflags `{s}`"),
            ))?,
            None => {
                // No flag word: fall back to the header defaults. That fallback is
                // ambiguous only where it changes the decoding.
                let mut f = 0;
                if ctx.default_signed {
                    f |= typeflags::SIGNED;
                }
                if ctx.default_float {
                    f |= typeflags::FLOAT;
                }
                if ctx.default_lsb && size_bits > 8 {
                    f |= typeflags::LSB_FIRST;
                }
                if f != 0 {
                    warns.push((
                        kinds::ENDIAN_DEFAULTED,
                        format!(
                        "no mmedtypeflags; header DEFAULTS applied ({}{}{}) — verify against a \
                         known image",
                        if f & typeflags::LSB_FIRST != 0 { "LSB first " } else { "" },
                        if f & typeflags::SIGNED != 0 { "signed " } else { "" },
                        if f & typeflags::FLOAT != 0 { "float" } else { "" },
                    )
                        .replace(" )", ")"),
                    ));
                }
                f
            }
        };
    let known = typeflags::SIGNED | typeflags::LSB_FIRST | typeflags::SWAP_XY | typeflags::FLOAT;
    if flags & !known != 0 {
        warns.push((
            kinds::TYPEFLAGS_UNKNOWN,
            format!(
                "mmedtypeflags 0x{flags:X} has unknown bit(s) 0x{:X}; ignored",
                flags & !known
            ),
        ));
    }
    let data_type = datatype_from(size_bits, flags).ok_or_else(|| {
        if flags & typeflags::FLOAT != 0 {
            (
                kinds::ELEMENT_SIZE_UNSUPPORTED,
                format!("{size_bits}-bit float; only 32-bit IEEE-754 floats are supported"),
            )
        } else {
            (
                kinds::ELEMENT_SIZE_UNSUPPORTED,
                format!("{size_bits}-bit elements cannot be stored exactly (8/16/32 only)"),
            )
        }
    })?;
    // A single byte has no byte order; report big-endian for it.
    let endian = if size_bits > 8 {
        endian_from(flags)
    } else {
        Endian::Big
    };
    Ok((
        Cell {
            data_type,
            endian,
            column_major: flags & typeflags::SWAP_XY != 0,
            size_bits,
        },
        warns,
    ))
}

/// Check `mmedmajorstridebits` / `mmedminorstridebits` for a block of
/// `n_fast` cells per line × `n_slow` lines.
///
/// TunerPro writes `0` for "packed". The exact meaning of a non-zero value
/// (which stride is the cell step and which the line pitch) is not publicly
/// specified, so a layout is accepted only when it is contiguous under the
/// reading that makes it so, and a 1-D vector only when every non-zero stride
/// equals the element size (contiguous under both readings). Anything else is
/// non-contiguous or reversed and cannot be expressed as a `MapDef`.
fn check_strides(
    emb: Node,
    size_bits: u32,
    n_fast: u32,
    n_slow: u32,
) -> std::result::Result<(), Problem> {
    let get = |n: &str| -> std::result::Result<i64, Problem> {
        match attr(emb, n) {
            None => Ok(0),
            Some(s) => {
                parse_i64(s).ok_or((kinds::STRIDE_NONCONTIGUOUS, format!("unparsable {n} `{s}`")))
            }
        }
    };
    let (major, minor) = (get("mmedmajorstridebits")?, get("mmedminorstridebits")?);
    if u64::from(n_fast) * u64::from(n_slow) <= 1 || (major == 0 && minor == 0) {
        return Ok(());
    }
    if major < 0 || minor < 0 {
        return Err((
            kinds::STRIDE_REVERSED,
            format!(
                "negative stride (major {major}, minor {minor} bits): reversed layout is not \
                 representable"
            ),
        ));
    }
    let sz = i64::from(size_bits);
    let ok = if n_fast == 1 || n_slow == 1 {
        [major, minor].iter().all(|s| *s == 0 || *s == sz)
    } else {
        let line = i64::from(n_fast) * sz;
        let a = (minor == 0 || minor == sz) && (major == 0 || major == line);
        let b = (major == 0 || major == sz) && (minor == 0 || minor == line);
        a || b
    };
    if ok {
        Ok(())
    } else {
        Err((
            kinds::STRIDE_NONCONTIGUOUS,
            format!(
                "strided layout (major {major}, minor {minor} bits, {size_bits}-bit cells, \
                 {n_slow}x{n_fast}) is not contiguous; interleaved data is not representable"
            ),
        ))
    }
}

/// Linear scaling from a node's `<MATH equation>`. `Err` carries the reason
/// the equation is not a linear function of `X` (the caller imports pre-scaled).
fn equation_scaling(node: Node) -> std::result::Result<Scaling, Problem> {
    let Some(m) = child(node, "MATH") else {
        return Ok(Scaling::default());
    };
    let Some(eq) = attr(m, "equation") else {
        return Ok(Scaling::default());
    };
    match linear_from_equation(eq) {
        LinearFit::Linear { factor, offset } => Ok(Scaling { factor, offset }),
        LinearFit::NonLinear | LinearFit::Unparsable => {
            let foreign: Vec<String> = m
                .children()
                .filter(|c| has_tag(*c, "VAR"))
                .filter_map(|v| {
                    let id = attr(v, "id")?.trim();
                    (!id.eq_ignore_ascii_case("x") && !id.eq_ignore_ascii_case("null")).then(|| {
                        match attr(v, "type") {
                            Some(t) => format!("{id} ({t})"),
                            None => id.to_string(),
                        }
                    })
                })
                .collect();
            if foreign.is_empty() {
                Err((
                    kinds::MATH_NONLINEAR,
                    format!(
                        "non-linear equation `{}` — imported pre-scaled (factor 1, offset 0)",
                        eq.trim()
                    ),
                ))
            } else {
                Err((
                    kinds::MATH_FOREIGN_VAR,
                    format!(
                        "equation `{}` uses variable(s) {} besides X — imported pre-scaled \
                         (factor 1, offset 0)",
                        eq.trim(),
                        foreign.join(", ")
                    ),
                ))
            }
        }
    }
}

/// Category names of an object (`CATEGORYMEM category` is 1-based).
fn category_names(n: Node, ctx: &Ctx) -> Vec<String> {
    n.children()
        .filter(|c| has_tag(*c, "CATEGORYMEM"))
        .filter_map(|c| attr(c, "category").and_then(parse_u32))
        .filter_map(|c| c.checked_sub(1))
        .filter_map(|i| ctx.categories.get(&i).cloned())
        .collect()
}

/// Object title, falling back to `<prefix>_<uniqueid>`.
fn title_of(n: Node, prefix: &str) -> String {
    child_text(n, "title")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| match attr(n, "uniqueid") {
            Some(id) => format!("{prefix}_{id}"),
            None => format!("unnamed {prefix}"),
        })
}

/// Base `notes` lines of an object: description + categories.
fn base_notes(n: Node, ctx: &Ctx) -> Vec<String> {
    let mut notes = Vec::new();
    if let Some(d) = child_text(n, "description").filter(|s| !s.is_empty()) {
        notes.push(d);
    }
    let cats = category_names(n, ctx);
    if !cats.is_empty() {
        notes.push(format!("category: {}", cats.join(" / ")));
    }
    notes
}

fn finish_notes(mut notes: Vec<String>, tr: &Tracker) -> String {
    notes.extend(tr.note_lines());
    notes.join("; ")
}

/// Everything decoded about a block of stored cells.
struct Block {
    address: u32,
    cell: Cell,
    scaling: Scaling,
}

/// Decode a stored block (`<EMBEDDEDDATA>` + the owner's `<MATH>`) of
/// `n_fast` × `n_slow` cells. Warnings go to `warns`; `Err` means the block
/// cannot be placed/decoded. A non-linear equation is reported via
/// `math_kind` as a warning, never an error.
fn decode_block(
    owner: Node,
    emb: Node,
    rows: u32,
    cols: u32,
    ctx: &Ctx,
    warns: &mut Vec<Problem>,
) -> std::result::Result<Block, Problem> {
    let mmed = attr(emb, "mmedaddress")
        .and_then(parse_u32)
        .ok_or((kinds::ADDRESS_MISSING, "no mmedaddress".to_string()))?;
    let (cell, w) = decode_storage(emb, ctx)?;
    warns.extend(w);
    let (n_fast, n_slow) = if cell.column_major {
        (rows, cols)
    } else {
        (cols, rows)
    };
    check_strides(emb, cell.size_bits, n_fast, n_slow)?;
    let address = rebase(mmed, ctx)?;
    within_image(
        address,
        u64::from(rows) * u64::from(cols) * cell.bytes(),
        ctx,
    )?;
    let scaling = match equation_scaling(owner) {
        Ok(s) => s,
        Err(p) => {
            warns.push(p);
            Scaling::default()
        }
    };
    Ok(Block {
        address,
        cell,
        scaling,
    })
}

/// Build the axis of dimension `dim` from an `<XDFAXIS id="x|y">`.
///
/// Source selection follows `<embedinfo type>`: `1` = embedded (stored in the
/// image at `mmedaddress`), `3` = linked to another object (`linkobjid`),
/// `0`/`2`/absent = manual `<LABEL>`s (or no breakpoints). Without
/// `embedinfo`, labels win; a bare addressed `EMBEDDEDDATA` is still read as
/// embedded (OpenTune's own exports, older files). Every loss is a warning on
/// the owning map — an axis problem never refuses the table data itself.
fn parse_axis(ax: Node, id: &str, dim: u32, ctx: &Ctx, tr: &mut Tracker) -> Option<AxisDef> {
    if dim <= 1 {
        return None; // a dimension of one needs no breakpoints
    }
    let unit = child_text(ax, "units").unwrap_or_default();
    let name = child_text(ax, "title")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| id.to_string());
    let count_only = |name: String, unit: String| AxisDef {
        name,
        unit,
        count: dim,
        ..Default::default()
    };

    if let Some(ic) = child_text(ax, "indexcount").and_then(|s| parse_u32(&s)) {
        if ic != dim {
            tr.warn(
                kinds::AXIS_COUNT_MISMATCH,
                format!(
                    "{id}-axis indexcount {ic} differs from the table dimension {dim}; using {dim}"
                ),
            );
        }
    }

    let source = child(ax, "embedinfo")
        .and_then(|e| attr(e, "type"))
        .and_then(parse_u32);
    let labels: Vec<&str> = ax
        .children()
        .filter(|n| has_tag(*n, "LABEL"))
        .filter_map(|l| attr(l, "value"))
        .map(str::trim)
        .collect();
    let emb = child(ax, "EMBEDDEDDATA");
    let emb_addr = emb.and_then(|e| attr(e, "mmedaddress")).and_then(parse_u32);

    let embedded = match source {
        Some(1) => true,
        Some(3) => return linked_axis(ax, id, dim, name, unit, ctx, tr),
        None => labels.is_empty() && emb_addr.unwrap_or(0) != 0,
        Some(0) | Some(2) => false,
        Some(t) => {
            if labels.is_empty() {
                tr.warn(
                    kinds::AXIS_UNRESOLVED,
                    format!(
                        "{id}-axis has unknown source (embedinfo type {t}); breakpoints dropped"
                    ),
                );
                return Some(count_only(name, unit));
            }
            false
        }
    };

    if embedded {
        let Some(e) = emb.filter(|_| emb_addr.unwrap_or(0) != 0) else {
            tr.warn(
                kinds::AXIS_UNRESOLVED,
                format!("{id}-axis is embedded but has no address (0x0); breakpoints unknown"),
            );
            return Some(count_only(name, unit));
        };
        let (rows, cols) = if id == "y" { (dim, 1) } else { (1, dim) };
        let mut warns = Vec::new();
        match decode_block(ax, e, rows, cols, ctx, &mut warns) {
            Ok(b) => {
                for (k, m) in warns {
                    let k = if k == kinds::MATH_NONLINEAR || k == kinds::MATH_FOREIGN_VAR {
                        kinds::AXIS_MATH_NONLINEAR
                    } else {
                        k
                    };
                    tr.warn(k, format!("{id}-axis: {m}"));
                }
                Some(AxisDef {
                    name,
                    unit,
                    count: dim,
                    address: Some(b.address),
                    data_type: b.cell.data_type,
                    endian: b.cell.endian,
                    scaling: b.scaling,
                    ..Default::default()
                })
            }
            Err((_, m)) => {
                tr.warn(
                    kinds::AXIS_STORAGE,
                    format!("{id}-axis storage unusable ({m}); breakpoint address dropped"),
                );
                Some(count_only(name, unit))
            }
        }
    } else if labels.is_empty() {
        // No breakpoints in the source either (TunerPro shows indices).
        Some(count_only(name, unit))
    } else {
        let parsed: Vec<Option<f64>> = labels.iter().map(|l| l.parse::<f64>().ok()).collect();
        if parsed.iter().any(Option::is_none) {
            let text: Vec<&str> = labels.to_vec();
            tr.warn(
                kinds::AXIS_CATEGORICAL,
                format!(
                    "{id}-axis has {} categorical label(s) not representable in a numeric axis: {}",
                    text.len(),
                    text.join(", ")
                ),
            );
            return Some(count_only(name, unit));
        }
        let values: Vec<f64> = parsed.into_iter().flatten().collect();
        if values.len() as u32 != dim {
            tr.warn(
                kinds::AXIS_COUNT_MISMATCH,
                format!(
                    "{id}-axis has {} label(s) for a dimension of {dim}; breakpoints dropped",
                    values.len()
                ),
            );
            return Some(count_only(name, unit));
        }
        if let Ok(s) = equation_scaling(ax) {
            if s != Scaling::default() {
                tr.warn(
                    kinds::AXIS_MATH_NONLINEAR,
                    format!(
                        "{id}-axis labels also carry MATH (x{} {:+}); labels imported verbatim — \
                         verify whether they are raw or physical",
                        s.factor, s.offset
                    ),
                );
            }
        }
        Some(AxisDef {
            name,
            unit,
            count: dim,
            values,
            ..Default::default()
        })
    }
}

/// An axis whose breakpoints come from another object (`embedinfo type="3"
/// linkobjid=".."`): resolved to a stored axis when the target is a 1-D table
/// of the right length.
fn linked_axis(
    ax: Node,
    id: &str,
    dim: u32,
    name: String,
    unit: String,
    ctx: &Ctx,
    tr: &mut Tracker,
) -> Option<AxisDef> {
    let link = child(ax, "embedinfo")
        .and_then(|e| attr(e, "linkobjid"))
        .and_then(parse_u32);
    let unresolved = |tr: &mut Tracker, why: String| {
        tr.warn(
            kinds::AXIS_UNRESOLVED,
            format!("{id}-axis is linked to another object but {why}; breakpoints dropped"),
        );
        Some(AxisDef {
            name: name.clone(),
            unit: unit.clone(),
            count: dim,
            ..Default::default()
        })
    };
    let Some(link) = link else {
        return unresolved(tr, "has no linkobjid".into());
    };
    let target = match ctx.by_id.get(&link) {
        None => return unresolved(tr, format!("object 0x{link:X} does not exist")),
        Some(None) => return unresolved(tr, format!("id 0x{link:X} is shared by several objects")),
        Some(Some(t)) => *t,
    };
    if !has_tag(target, "XDFTABLE") {
        return unresolved(
            tr,
            format!(
                "0x{link:X} is a <{}>, not a table",
                target.tag_name().name()
            ),
        );
    }
    let Some(z) = target.children().find(|n| {
        has_tag(*n, "XDFAXIS") && attr(*n, "id").is_some_and(|i| i.eq_ignore_ascii_case("z"))
    }) else {
        return unresolved(tr, format!("table 0x{link:X} has no data"));
    };
    let Some(e) = child(z, "EMBEDDEDDATA") else {
        return unresolved(tr, format!("table 0x{link:X} has no data"));
    };
    let r = attr(e, "mmedrowcount")
        .and_then(parse_u32)
        .unwrap_or(1)
        .max(1);
    let c = attr(e, "mmedcolcount")
        .and_then(parse_u32)
        .unwrap_or(1)
        .max(1);
    if r.min(c) != 1 || r.max(c) != dim {
        return unresolved(
            tr,
            format!("table 0x{link:X} is {r}x{c}, not a 1-D vector of {dim}"),
        );
    }
    let mut warns = Vec::new();
    match decode_block(z, e, r, c, ctx, &mut warns) {
        Ok(b) => {
            for (k, m) in warns {
                let k = if k == kinds::MATH_NONLINEAR || k == kinds::MATH_FOREIGN_VAR {
                    kinds::AXIS_MATH_NONLINEAR
                } else {
                    k
                };
                tr.warn(k, format!("{id}-axis (linked): {m}"));
            }
            let unit = if unit.is_empty() {
                child_text(z, "units").unwrap_or_default()
            } else {
                unit
            };
            Some(AxisDef {
                name: title_of(target, "table"),
                unit,
                count: dim,
                address: Some(b.address),
                data_type: b.cell.data_type,
                endian: b.cell.endian,
                scaling: b.scaling,
                ..Default::default()
            })
        }
        Err((_, m)) => unresolved(tr, format!("the linked table is unusable ({m})")),
    }
}

fn axis_node<'a, 'i>(n: Node<'a, 'i>, id: &str) -> Option<Node<'a, 'i>> {
    n.children().find(|c| {
        has_tag(*c, "XDFAXIS") && attr(*c, "id").is_some_and(|i| i.eq_ignore_ascii_case(id))
    })
}

fn bounds(n: Node) -> (Option<f64>, Option<f64>) {
    let num = |t: &str| child_text(n, t).and_then(|s| s.trim().parse::<f64>().ok());
    (
        num("min").or_else(|| num("rangelow")),
        num("max").or_else(|| num("rangehigh")),
    )
}

/// `<XDFTABLE>` → `MapDef`.
fn parse_table(table: Node, ctx: &Ctx) -> (Option<MapDef>, ObjectReport) {
    let name = title_of(table, "table");
    let mut tr = Tracker::new();
    let refuse = |mut tr: Tracker, (k, m): Problem| {
        tr.refuse(k, m);
        (None, tr.finish(name.clone(), ObjectKind::Table))
    };

    let Some((z, emb)) = axis_node(table, "z")
        .and_then(|z| child(z, "EMBEDDEDDATA").map(|e| (z, e)))
        .filter(|(_, e)| attr(*e, "mmedaddress").is_some())
    else {
        return (None, placeholder(table));
    };
    let rows = attr(emb, "mmedrowcount")
        .and_then(parse_u32)
        .unwrap_or(1)
        .max(1);
    let cols = attr(emb, "mmedcolcount")
        .and_then(parse_u32)
        .unwrap_or(1)
        .max(1);

    let mut warns = Vec::new();
    let block = match decode_block(z, emb, rows, cols, ctx, &mut warns) {
        Ok(b) => b,
        Err(p) => {
            for (k, m) in warns {
                tr.warn(k, m);
            }
            return refuse(tr, p);
        }
    };
    for (k, m) in warns {
        tr.warn(k, m);
    }

    let mut notes = base_notes(table, ctx);
    let mut x_axis = axis_node(table, "x").and_then(|a| parse_axis(a, "x", cols, ctx, &mut tr));
    let mut y_axis = axis_node(table, "y").and_then(|a| parse_axis(a, "y", rows, ctx, &mut tr));
    let (mut rows, mut cols) = (rows, cols);
    if block.cell.column_major && rows > 1 && cols > 1 {
        // Column-major storage (mmedtypeflags 0x04): the bytes of an R×C
        // column-major table are exactly a C×R row-major table with the axes
        // exchanged. Import that view: every cell keeps its value and its
        // breakpoints; only the on-screen orientation differs from TunerPro.
        std::mem::swap(&mut rows, &mut cols);
        std::mem::swap(&mut x_axis, &mut y_axis);
        notes.push(format!(
            "stored column-major (mmedtypeflags 0x04): imported transposed as {rows}x{cols} with \
             x/y axes exchanged relative to TunerPro's {cols}x{rows} view"
        ));
    }
    let (min, max) = bounds(z);
    let map = MapDef {
        name: name.clone(),
        notes: finish_notes(notes, &tr),
        address: block.address,
        data_type: block.cell.data_type,
        endian: block.cell.endian,
        rows,
        cols,
        scaling: block.scaling,
        unit: child_text(z, "units").unwrap_or_default(),
        x_axis,
        y_axis,
        min,
        max,
    };
    (Some(map), tr.finish(name, ObjectKind::Table))
}

/// `<XDFFUNCTION>` → a 1×N curve: the output (`y`) vector is the data, the
/// input (`x`) vector is its axis.
fn parse_function(f: Node, ctx: &Ctx) -> (Option<MapDef>, ObjectReport) {
    let name = title_of(f, "function");
    let mut tr = Tracker::new();
    let (Some(xn), Some(yn)) = (axis_node(f, "x"), axis_node(f, "y")) else {
        tr.refuse(kinds::ADDRESS_MISSING, "function lacks an x or y vector");
        return (None, tr.finish(name, ObjectKind::Function));
    };
    let Some(emb) = child(yn, "EMBEDDEDDATA").filter(|e| attr(*e, "mmedaddress").is_some()) else {
        return (None, placeholder(f));
    };
    let n = child_text(yn, "indexcount")
        .and_then(|s| parse_u32(&s))
        .or_else(|| attr(emb, "mmedcolcount").and_then(parse_u32))
        .or_else(|| attr(emb, "mmedrowcount").and_then(parse_u32))
        .unwrap_or(1)
        .max(1);
    let mut warns = Vec::new();
    let block = decode_block(yn, emb, 1, n, ctx, &mut warns);
    for (k, m) in warns {
        tr.warn(k, m);
    }
    let block = match block {
        Ok(b) => b,
        Err((k, m)) => {
            tr.refuse(k, m);
            return (None, tr.finish(name, ObjectKind::Function));
        }
    };
    let x_axis = parse_axis(xn, "x", n, ctx, &mut tr);
    let (min, max) = bounds(yn);
    let map = MapDef {
        name: name.clone(),
        notes: finish_notes(base_notes(f, ctx), &tr),
        address: block.address,
        data_type: block.cell.data_type,
        endian: block.cell.endian,
        rows: 1,
        cols: n,
        scaling: block.scaling,
        unit: child_text(yn, "units").unwrap_or_default(),
        x_axis,
        y_axis: None,
        min,
        max,
    };
    (Some(map), tr.finish(name, ObjectKind::Function))
}

/// `<XDFCONSTANT>` → [`XdfConstant`]. Only plain numeric scalars; nothing here
/// promotes a bitfield/switch to an unrestricted write.
fn parse_constant(node: Node, ctx: &Ctx) -> (Option<XdfConstant>, ObjectReport) {
    let name = title_of(node, "const");
    let mut tr = Tracker::new();
    let Some(emb) = child(node, "EMBEDDEDDATA").filter(|e| attr(*e, "mmedaddress").is_some())
    else {
        return (None, placeholder(node));
    };
    let mut warns = Vec::new();
    let block = decode_block(node, emb, 1, 1, ctx, &mut warns);
    for (k, m) in warns {
        tr.warn(k, m);
    }
    let block = match block {
        Ok(b) => b,
        Err((k, m)) => {
            tr.refuse(k, m);
            return (None, tr.finish(name, ObjectKind::Constant));
        }
    };
    let (min, max) = bounds(node);
    let c = XdfConstant {
        name: name.clone(),
        notes: finish_notes(base_notes(node, ctx), &tr),
        address: block.address,
        data_type: block.cell.data_type,
        endian: block.cell.endian,
        scaling: block.scaling,
        unit: child_text(node, "units").unwrap_or_default(),
        min,
        max,
    };
    (Some(c), tr.finish(name, ObjectKind::Constant))
}

fn flag_reason(n: Node) -> String {
    let mask = child_text(n, "mask").unwrap_or_else(|| "?".into());
    let addr = child(n, "EMBEDDEDDATA")
        .and_then(|e| attr(e, "mmedaddress"))
        .unwrap_or("?");
    format!(
        "bit flag (mask {mask} at {addr}) — OpenTune maps hold whole numeric cells; a single-bit \
         switch is not imported as a writable map"
    )
}

/// An object with no data address at all: a separator / information entry.
fn placeholder(n: Node) -> ObjectReport {
    out_of_scope(
        n,
        ObjectKind::Placeholder,
        "no data address (mmedaddress) — a separator or information entry; nothing to import"
            .into(),
    )
}

fn out_of_scope(n: Node, kind: ObjectKind, reason: String) -> ObjectReport {
    let mut tr = Tracker::new();
    tr.refuse(kinds::UNSUPPORTED_OBJECT, reason);
    tr.finish(title_of(n, kind.as_str()), kind)
}

// --- small roxmltree helpers (case-insensitive tag match) ---

fn has_tag(n: Node, tag: &str) -> bool {
    n.is_element() && n.tag_name().name().eq_ignore_ascii_case(tag)
}

fn child<'a, 'input>(n: Node<'a, 'input>, tag: &str) -> Option<Node<'a, 'input>> {
    n.children().find(|c| has_tag(*c, tag))
}

fn descendant<'a, 'input>(n: Node<'a, 'input>, tag: &str) -> Option<Node<'a, 'input>> {
    n.descendants().find(|c| has_tag(*c, tag))
}

fn child_text(n: Node, tag: &str) -> Option<String> {
    child(n, tag).map(|c| c.text().unwrap_or("").trim().to_string())
}

fn attr<'a>(n: Node<'a, '_>, name: &str) -> Option<&'a str> {
    n.attributes()
        .find(|a| a.name().eq_ignore_ascii_case(name))
        .map(|a| a.value())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Status;

    // A compact but realistic modern XML XDF: one 16x11 table, big-endian u8,
    // a non-trivial linear z equation, a label x-axis and a stored y-axis.
    const SAMPLE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<XDFFORMAT version="1.60">
  <XDFHEADER>
    <flags>0x1</flags>
    <deftitle>Sample ECU</deftitle>
    <description>synthetic test</description>
    <BASEOFFSET offset="0" subtract="0"/>
    <DEFAULTS datasizeinbits="8" sigdigits="2" outputtype="1" signed="0" lsbfirst="0" float="0"/>
    <CATEGORY index="0x0" name="Ignition"/>
    <CATEGORY index="0x1" name="Main"/>
  </XDFHEADER>
  <XDFTABLE uniqueid="0x1A29" flags="0x0">
    <title>Spark Advance</title>
    <description>main spark table</description>
    <CATEGORYMEM index="0" category="1"/>
    <CATEGORYMEM index="1" category="2"/>
    <XDFAXIS id="x">
      <units>MAP</units>
      <indexcount>11</indexcount>
      <LABEL index="0" value="0"/><LABEL index="1" value="10"/><LABEL index="2" value="20"/>
      <LABEL index="3" value="30"/><LABEL index="4" value="40"/><LABEL index="5" value="50"/>
      <LABEL index="6" value="60"/><LABEL index="7" value="70"/><LABEL index="8" value="80"/>
      <LABEL index="9" value="90"/><LABEL index="10" value="105"/>
      <MATH equation="X"/>
    </XDFAXIS>
    <XDFAXIS id="y">
      <units>rpm</units>
      <indexcount>16</indexcount>
      <EMBEDDEDDATA mmedaddress="0x200" mmedelementsizebits="16" mmedtypeflags="0x00"/>
      <MATH equation="X*40"/>
    </XDFAXIS>
    <XDFAXIS id="z">
      <units>deg</units>
      <EMBEDDEDDATA mmedaddress="0x288" mmedelementsizebits="8" mmedrowcount="16" mmedcolcount="11" mmedtypeflags="0x00"/>
      <min>-20</min>
      <max>60</max>
      <MATH equation="(45*X)/127-20"/>
    </XDFAXIS>
  </XDFTABLE>
</XDFFORMAT>"#;

    /// One table whose z EMBEDDEDDATA attributes / MATH / header extras are
    /// given; axes optional.
    fn one_table(header_extra: &str, z_emb: &str, z_math: &str, axes: &str) -> String {
        format!(
            r#"<XDFFORMAT version="1.60"><XDFHEADER><deftitle>T</deftitle>{header_extra}</XDFHEADER>
            <XDFTABLE uniqueid="0x10"><title>Tbl</title>{axes}<XDFAXIS id="z">
            <EMBEDDEDDATA {z_emb}/><MATH equation="{z_math}"/></XDFAXIS></XDFTABLE></XDFFORMAT>"#
        )
    }

    fn report<'a>(imp: &'a XdfImport, name: &str) -> &'a ObjectReport {
        imp.reports
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("no report for {name}: {:?}", imp.reports))
    }

    fn kinds_of(r: &ObjectReport) -> Vec<&'static str> {
        r.issues.iter().map(|i| i.kind).collect()
    }

    #[test]
    fn parses_sample_table() {
        let imp = parse(SAMPLE).expect("parse");
        assert_eq!(imp.title.as_deref(), Some("Sample ECU"));
        assert_eq!(imp.base_offset, 0);
        assert_eq!(imp.maps.len(), 1);

        let m = &imp.maps[0];
        assert_eq!(m.name, "Spark Advance");
        assert_eq!(m.address, 0x288);
        assert_eq!(m.rows, 16);
        assert_eq!(m.cols, 11);
        assert_eq!(m.data_type, DataType::U8);
        assert_eq!(m.endian, Endian::Big);
        assert_eq!(m.unit, "deg");
        assert_eq!(m.min, Some(-20.0));
        assert_eq!(m.max, Some(60.0));
        assert!((m.scaling.factor - 45.0 / 127.0).abs() < 1e-9);
        assert!((m.scaling.offset + 20.0).abs() < 1e-9);
        // Categories are imported faithfully into the notes.
        assert!(m.notes.contains("category: Ignition / Main"), "{}", m.notes);

        let x = m.x_axis.as_ref().expect("x axis");
        assert_eq!(x.unit, "MAP");
        assert_eq!(x.values.len(), 11);
        assert_eq!(x.values[10], 105.0);

        // y axis: stored (no embedinfo, addressed EMBEDDEDDATA), u16 BE, scaled.
        let y = m.y_axis.as_ref().expect("y axis");
        assert_eq!(y.address, Some(0x200));
        assert_eq!(y.data_type, DataType::U16);
        assert_eq!(y.count, 16);
        assert!((y.scaling.factor - 40.0).abs() < 1e-9);

        assert_eq!(report(&imp, "Spark Advance").status, Status::Clean);
        assert!(imp.strict_failures().is_empty());
    }

    #[test]
    fn base_offset_subtract_is_applied() {
        let xml = SAMPLE.replace(
            r#"<BASEOFFSET offset="0" subtract="0"/>"#,
            r#"<BASEOFFSET offset="0x100" subtract="1"/>"#,
        );
        let imp = parse(&xml).expect("parse");
        assert_eq!(imp.base_offset, 0x100);
        assert!(imp.base_subtract);
        assert_eq!(imp.maps[0].address, 0x188);
        // Stored axes are rebased too.
        assert_eq!(imp.maps[0].y_axis.as_ref().unwrap().address, Some(0x100));
    }

    #[test]
    fn text_form_negative_baseoffset_is_applied() {
        // TunerPro 1.5-era `<baseoffset>-49152</baseoffset>`: a 16 KiB bin whose
        // XDF addresses are CPU addresses 0xC000.. . Previously ignored (0).
        let xml = one_table(
            "<baseoffset>-49152</baseoffset>",
            r#"mmedaddress="0xCBD8" mmedelementsizebits="8" mmedcolcount="4""#,
            "X",
            "",
        );
        let imp = parse(&xml).expect("parse");
        assert_eq!(imp.maps[0].address, 0x0BD8);
        assert!(imp.base_subtract);
        assert_eq!(imp.base_offset, 0xC000);
    }

    #[test]
    fn signed_and_little_endian_flags() {
        let xml = SAMPLE.replace(
            r#"mmedelementsizebits="8" mmedrowcount="16" mmedcolcount="11" mmedtypeflags="0x00""#,
            r#"mmedelementsizebits="16" mmedrowcount="16" mmedcolcount="11" mmedtypeflags="0x03""#,
        );
        let imp = parse(&xml).expect("parse");
        let m = &imp.maps[0];
        assert_eq!(m.data_type, DataType::S16);
        assert_eq!(m.endian, Endian::Little);
    }

    #[test]
    fn float_flag_needs_32_bits() {
        let ok = one_table(
            "",
            r#"mmedaddress="0x10" mmedelementsizebits="32" mmedtypeflags="0x10002""#,
            "X",
            "",
        );
        let imp = parse(&ok).expect("parse");
        assert_eq!(imp.maps[0].data_type, DataType::F32);
        assert_eq!(imp.maps[0].endian, Endian::Little);

        let bad = ok.replace("\"32\"", "\"64\"");
        let imp = parse(&bad).expect("parse");
        assert!(imp.maps.is_empty());
        let r = report(&imp, "Tbl");
        assert_eq!(r.status, Status::Refused);
        assert_eq!(kinds_of(r), vec![kinds::ELEMENT_SIZE_UNSUPPORTED]);
    }

    #[test]
    fn odd_element_width_is_refused_not_approximated() {
        let xml = one_table(
            "",
            r#"mmedaddress="0x10" mmedelementsizebits="24" mmedcolcount="4" mmedtypeflags="0x0""#,
            "X",
            "",
        );
        let imp = parse(&xml).expect("parse");
        assert!(imp.maps.is_empty(), "24-bit must not be widened to u32");
        assert_eq!(
            report(&imp, "Tbl").issues[0].kind,
            kinds::ELEMENT_SIZE_UNSUPPORTED
        );
    }

    #[test]
    fn missing_element_size_uses_header_default_with_warning_or_refuses() {
        let with_default = one_table(
            r#"<DEFAULTS datasizeinbits="16" lsbfirst="0"/>"#,
            r#"mmedaddress="0x10" mmedcolcount="4" mmedtypeflags="0x0""#,
            "X",
            "",
        );
        let imp = parse(&with_default).expect("parse");
        assert_eq!(imp.maps[0].data_type, DataType::U16);
        assert_eq!(
            kinds_of(report(&imp, "Tbl")),
            vec![kinds::ELEMENT_SIZE_DEFAULTED]
        );

        let without = one_table("", r#"mmedaddress="0x10" mmedcolcount="4""#, "X", "");
        let imp = parse(&without).expect("parse");
        assert!(imp.maps.is_empty());
        assert_eq!(
            report(&imp, "Tbl").issues[0].kind,
            kinds::ELEMENT_SIZE_MISSING
        );
    }

    #[test]
    fn missing_typeflags_with_lsb_default_is_warned() {
        let lsb = one_table(
            r#"<DEFAULTS datasizeinbits="16" lsbfirst="1"/>"#,
            r#"mmedaddress="0x10" mmedelementsizebits="16" mmedcolcount="4""#,
            "X",
            "",
        );
        let imp = parse(&lsb).expect("parse");
        assert_eq!(imp.maps[0].endian, Endian::Little);
        assert_eq!(kinds_of(report(&imp, "Tbl")), vec![kinds::ENDIAN_DEFAULTED]);

        // Same with lsbfirst=0: both readings agree on big-endian -> clean.
        let msb = lsb.replace("lsbfirst=\"1\"", "lsbfirst=\"0\"");
        let imp = parse(&msb).expect("parse");
        assert_eq!(imp.maps[0].endian, Endian::Big);
        assert_eq!(report(&imp, "Tbl").status, Status::Clean);

        // 8-bit cells have no byte order: clean even with lsbfirst=1.
        let byte = lsb.replace("mmedelementsizebits=\"16\"", "mmedelementsizebits=\"8\"");
        let imp = parse(&byte).expect("parse");
        assert_eq!(report(&imp, "Tbl").status, Status::Clean);
    }

    #[test]
    fn unknown_typeflag_bits_are_warned() {
        let xml = one_table(
            "",
            r#"mmedaddress="0x10" mmedelementsizebits="8" mmedtypeflags="0x100""#,
            "X",
            "",
        );
        let imp = parse(&xml).expect("parse");
        assert_eq!(
            kinds_of(report(&imp, "Tbl")),
            vec![kinds::TYPEFLAGS_UNKNOWN]
        );
    }

    #[test]
    fn nonlinear_math_is_flagged_not_guessed() {
        let xml = SAMPLE.replace("(45*X)/127-20", "X*X+5");
        let imp = parse(&xml).expect("parse");
        let m = &imp.maps[0];
        assert_eq!(m.scaling, Scaling::default());
        assert!(m.notes.contains("non-linear"), "notes: {}", m.notes);
        assert_eq!(
            kinds_of(report(&imp, "Spark Advance")),
            vec![kinds::MATH_NONLINEAR]
        );
        assert_eq!(imp.strict_failures().len(), 1);
        assert!(matches!(imp.into_strict(), Err(Error::Strict(_))));
    }

    #[test]
    fn math_with_foreign_variable_is_prescaled_and_warned() {
        let xml = r#"<XDFFORMAT><XDFHEADER/><XDFTABLE><title>AFR</title><XDFAXIS id="z">
            <EMBEDDEDDATA mmedaddress="0x10" mmedelementsizebits="8"/>
            <MATH equation="X*A/128"><VAR id="X"/><VAR id="A" type="address" address="0x40" flags="0x2"/></MATH>
            </XDFAXIS></XDFTABLE></XDFFORMAT>"#
            .to_string();
        let imp = parse(&xml).expect("parse");
        assert_eq!(imp.maps[0].scaling, Scaling::default());
        let r = report(&imp, "AFR");
        assert_eq!(kinds_of(r), vec![kinds::MATH_FOREIGN_VAR]);
        assert!(r.issues[0].message.contains("A (address)"));

        // A declared-but-unused extra VAR with a linear X equation is clean.
        let unused = xml.replace("X*A/128", "X/128");
        let imp = parse(&unused).expect("parse");
        assert_eq!(report(&imp, "AFR").status, Status::Clean);
        assert!((imp.maps[0].scaling.factor - 1.0 / 128.0).abs() < 1e-12);
    }

    #[test]
    fn resolve_addr_checked_arithmetic() {
        let ctx = |base: i64| Ctx {
            base,
            default_lsb: false,
            default_signed: false,
            default_float: false,
            default_size_bits: None,
            image_size: None,
            categories: HashMap::new(),
            by_id: HashMap::new(),
        };
        assert!(rebase(0x10, &ctx(-0x100)).is_err());
        assert_eq!(rebase(0x100, &ctx(-0x100)), Ok(0));
        assert_eq!(rebase(0x288, &ctx(-0x100)), Ok(0x188));
        assert!(rebase(0xFFFF_FFFF, &ctx(0x10)).is_err());
        assert_eq!(rebase(0x184FC, &ctx(0x30000)), Ok(0x484FC));
    }

    /// Build a minimal single-table XDF with a given BASEOFFSET and z address.
    fn table_with_base(z_addr: &str, offset: &str, subtract: &str) -> String {
        format!(
            r#"<XDFFORMAT version="1.60"><XDFHEADER><deftitle>T</deftitle>
            <BASEOFFSET offset="{offset}" subtract="{subtract}"/></XDFHEADER>
            <XDFTABLE uniqueid="0x1"><title>Tbl</title><XDFAXIS id="z">
            <EMBEDDEDDATA mmedaddress="{z_addr}" mmedelementsizebits="8" mmedrowcount="2" mmedcolcount="2" mmedtypeflags="0x00"/>
            <MATH equation="X"/></XDFAXIS></XDFTABLE></XDFFORMAT>"#
        )
    }

    #[test]
    fn rebasing_below_base_is_refused_not_wrapped() {
        let xml = r#"<XDFFORMAT version="1.60"><XDFHEADER><deftitle>T</deftitle>
          <BASEOFFSET offset="0x100" subtract="1"/></XDFHEADER>
          <XDFTABLE uniqueid="0x1"><title>Bad</title><XDFAXIS id="z">
            <EMBEDDEDDATA mmedaddress="0x10" mmedelementsizebits="8" mmedrowcount="2" mmedcolcount="2" mmedtypeflags="0x00"/>
            <MATH equation="X"/></XDFAXIS></XDFTABLE>
          <XDFTABLE uniqueid="0x2"><title>Good</title><XDFAXIS id="z">
            <EMBEDDEDDATA mmedaddress="0x400" mmedelementsizebits="8" mmedrowcount="2" mmedcolcount="2" mmedtypeflags="0x00"/>
            <MATH equation="X"/></XDFAXIS></XDFTABLE></XDFFORMAT>"#;
        let imp = parse(xml).expect("parse");
        assert_eq!(imp.maps.len(), 1, "only the valid table imports");
        assert_eq!(imp.maps[0].name, "Good");
        assert_eq!(imp.maps[0].address, 0x300);
        assert!(
            imp.warnings
                .iter()
                .any(|w| w.contains("subtract underflow")),
            "expected an underflow warning, got {:?}",
            imp.warnings
        );
        assert_eq!(report(&imp, "Bad").status, Status::Refused);
        assert!(!imp.maps.iter().any(|m| m.address > 0xFF00_0000));
    }

    #[test]
    fn rebasing_at_base_yields_zero_address() {
        let xml = table_with_base("0x100", "0x100", "1");
        let imp = parse(&xml).expect("parse");
        assert_eq!(imp.maps.len(), 1);
        assert_eq!(imp.maps[0].address, 0);
        assert!(imp.warnings.is_empty(), "at-base must be clean");
    }

    #[test]
    fn rebasing_normal_add_base_preserved() {
        let xml = table_with_base("0x184FC", "196608", "0");
        let imp = parse(&xml).expect("parse");
        assert_eq!(imp.maps.len(), 1);
        assert_eq!(imp.maps[0].address, 0x484FC);
        assert!(imp.warnings.is_empty());
    }

    #[test]
    fn data_outside_declared_region_is_refused() {
        let region =
            r#"<REGION type="0xFFFFFFFF" startaddress="0x0" size="0x4000" name="Binary File"/>"#;
        let xml = one_table(
            region,
            r#"mmedaddress="0x3FFE" mmedelementsizebits="16" mmedcolcount="2" mmedtypeflags="0x0""#,
            "X",
            "",
        );
        let imp = parse(&xml).expect("parse");
        assert!(imp.maps.is_empty());
        assert_eq!(
            report(&imp, "Tbl").issues[0].kind,
            kinds::ADDRESS_OUTSIDE_IMAGE
        );
        // Exactly fitting is fine.
        let fits = xml.replace("0x3FFE", "0x3FFC");
        assert_eq!(parse(&fits).unwrap().maps.len(), 1);
    }

    #[test]
    fn interleaved_strides_are_refused() {
        // GM-style: two 33x2 column-major tables interleaved, 134-byte column
        // pitch. Not representable as contiguous cells.
        let xml = one_table(
            "",
            r#"mmedtypeflags="0x04" mmedaddress="0x16EC4" mmedelementsizebits="16" mmedrowcount="33" mmedcolcount="2" mmedmajorstridebits="0" mmedminorstridebits="1072""#,
            "X",
            "",
        );
        let imp = parse(&xml).expect("parse");
        assert!(imp.maps.is_empty());
        assert_eq!(
            report(&imp, "Tbl").issues[0].kind,
            kinds::STRIDE_NONCONTIGUOUS
        );

        // A 1-D strided vector (every 28 bytes) is refused too.
        let vec = one_table(
            "",
            r#"mmedaddress="0x100" mmedelementsizebits="16" mmedcolcount="8" mmedmajorstridebits="224""#,
            "X",
            "",
        );
        assert!(parse(&vec).unwrap().maps.is_empty());

        // Negative stride on stored data: reversed layout.
        let rev = vec.replace("\"224\"", "\"-16\"");
        let imp = parse(&rev).unwrap();
        assert_eq!(report(&imp, "Tbl").issues[0].kind, kinds::STRIDE_REVERSED);
    }

    #[test]
    fn packed_strides_are_accepted() {
        // Stride equal to the element size on a vector, or the natural row pitch
        // on a map, is just an explicit spelling of "packed".
        for emb in [
            r#"mmedaddress="0x100" mmedelementsizebits="16" mmedcolcount="8" mmedmajorstridebits="16""#,
            r#"mmedaddress="0x100" mmedelementsizebits="8" mmedrowcount="4" mmedcolcount="8" mmedminorstridebits="8" mmedmajorstridebits="64""#,
            r#"mmedaddress="0x100" mmedelementsizebits="8" mmedrowcount="4" mmedcolcount="8" mmedmajorstridebits="8""#,
        ] {
            let imp = parse(&one_table("", emb, "X", "")).unwrap();
            assert_eq!(imp.maps.len(), 1, "{emb}");
            assert_eq!(report(&imp, "Tbl").status, Status::Clean, "{emb}");
        }
    }

    #[test]
    fn label_axis_stride_is_irrelevant() {
        // TunerPro writes mmedmajorstridebits="-32" on label-only axes; nothing
        // is read from the image there, so it must not trip the stride check.
        let axes = r#"<XDFAXIS id="x"><EMBEDDEDDATA mmedelementsizebits="8" mmedmajorstridebits="-32"/>
            <indexcount>2</indexcount><LABEL index="0" value="1"/><LABEL index="1" value="2"/>
            <MATH equation="X"><VAR id="X"/></MATH></XDFAXIS>"#;
        let xml = one_table(
            "",
            r#"mmedaddress="0x10" mmedelementsizebits="8" mmedcolcount="2""#,
            "X",
            axes,
        );
        let imp = parse(&xml).unwrap();
        assert_eq!(report(&imp, "Tbl").status, Status::Clean);
        assert_eq!(imp.maps[0].x_axis.as_ref().unwrap().values, vec![1.0, 2.0]);
    }

    #[test]
    fn categorical_axis_labels_are_reported_not_silent() {
        let xml = r#"<XDFFORMAT version="1.60"><XDFHEADER><deftitle>TCU</deftitle>
          <BASEOFFSET offset="0" subtract="0"/></XDFHEADER>
          <XDFTABLE uniqueid="0x2"><title>Shift Times</title>
          <XDFAXIS id="x"><units>%</units><indexcount>2</indexcount>
            <LABEL index="0" value="0"/><LABEL index="1" value="100"/><MATH equation="X"/></XDFAXIS>
          <XDFAXIS id="y"><units>Gear</units><indexcount>5</indexcount>
            <LABEL index="0" value="1-&gt;2"/><LABEL index="1" value="2-&gt;3"/>
            <LABEL index="2" value="3-&gt;4"/><LABEL index="3" value="4-&gt;5"/>
            <LABEL index="4" value="5-&gt;6"/><MATH equation="X"/></XDFAXIS>
          <XDFAXIS id="z"><units>ms</units>
            <EMBEDDEDDATA mmedaddress="0x484fc" mmedelementsizebits="16" mmedrowcount="5" mmedcolcount="2" mmedtypeflags="0x02"/>
            <MATH equation="X"/></XDFAXIS></XDFTABLE></XDFFORMAT>"#;
        let imp = parse(xml).expect("parse");
        let m = &imp.maps[0];
        assert_eq!((m.rows, m.cols), (5, 2));
        let y = m.y_axis.as_ref().expect("y axis kept as a dimension");
        assert_eq!(y.count, 5);
        assert!(y.values.is_empty(), "text labels are not numeric values");
        assert!(m.notes.contains("categorical"), "{}", m.notes);
        let w = imp
            .warnings
            .iter()
            .find(|w| w.contains("categorical"))
            .expect("a categorical warning");
        assert!(w.contains("1->2") && w.contains("5->6"), "{w}");
        assert!(!m.notes.contains("x-axis has"));
        assert_eq!(
            kinds_of(report(&imp, "Shift Times")),
            vec![kinds::AXIS_CATEGORICAL]
        );
    }

    #[test]
    fn label_count_mismatch_drops_breakpoints_with_warning() {
        let axes = r#"<XDFAXIS id="x"><indexcount>4</indexcount><LABEL index="0" value="1"/>
            <LABEL index="1" value="2"/><MATH equation="X"/></XDFAXIS>"#;
        let xml = one_table(
            "",
            r#"mmedaddress="0x10" mmedelementsizebits="8" mmedcolcount="4""#,
            "X",
            axes,
        );
        let imp = parse(&xml).unwrap();
        let x = imp.maps[0].x_axis.as_ref().unwrap();
        assert!(x.values.is_empty());
        assert_eq!(x.count, 4);
        assert_eq!(
            kinds_of(report(&imp, "Tbl")),
            vec![kinds::AXIS_COUNT_MISMATCH]
        );
    }

    #[test]
    fn embedinfo_sources() {
        // type 1 with a real address -> stored axis; its non-linear MATH is a
        // warning on the map, and the axis stays raw.
        let stored = r#"<XDFAXIS id="x"><EMBEDDEDDATA mmedaddress="0x80" mmedelementsizebits="16" mmedcolcount="4" mmedtypeflags="0x02"/>
            <indexcount>4</indexcount><embedinfo type="1"/><MATH equation="1000/X"/></XDFAXIS>"#;
        let xml = one_table(
            "",
            r#"mmedaddress="0x10" mmedelementsizebits="8" mmedcolcount="4""#,
            "X",
            stored,
        );
        let imp = parse(&xml).unwrap();
        let x = imp.maps[0].x_axis.as_ref().unwrap();
        assert_eq!(x.address, Some(0x80));
        assert_eq!(x.endian, Endian::Little);
        assert_eq!(x.scaling, Scaling::default());
        assert_eq!(
            kinds_of(report(&imp, "Tbl")),
            vec![kinds::AXIS_MATH_NONLINEAR]
        );

        // type 1 at address 0 -> unresolved, count only.
        let zero = stored.replace("0x80", "0x0").replace("1000/X", "X");
        let imp = parse(&one_table(
            "",
            r#"mmedaddress="0x10" mmedelementsizebits="8" mmedcolcount="4""#,
            "X",
            &zero,
        ))
        .unwrap();
        let x = imp.maps[0].x_axis.as_ref().unwrap();
        assert_eq!((x.address, x.count), (None, 4));
        assert_eq!(kinds_of(report(&imp, "Tbl")), vec![kinds::AXIS_UNRESOLVED]);

        // type 2 without labels: the source itself has no breakpoints -> clean.
        let idx = r#"<XDFAXIS id="x"><EMBEDDEDDATA mmedelementsizebits="8"/><indexcount>4</indexcount>
            <embedinfo type="2"/><MATH equation="X"/></XDFAXIS>"#;
        let imp = parse(&one_table(
            "",
            r#"mmedaddress="0x10" mmedelementsizebits="8" mmedcolcount="4""#,
            "X",
            idx,
        ))
        .unwrap();
        assert_eq!(report(&imp, "Tbl").status, Status::Clean);
    }

    #[test]
    fn linked_axis_resolves_to_the_linked_vector() {
        let xml = r#"<XDFFORMAT><XDFHEADER/>
          <XDFTABLE uniqueid="0xBF2"><title>RPM breakpoints</title><XDFAXIS id="z"><units>rpm</units>
            <EMBEDDEDDATA mmedaddress="0x200" mmedelementsizebits="8" mmedcolcount="3"/>
            <MATH equation="X*50"/></XDFAXIS></XDFTABLE>
          <XDFTABLE uniqueid="0x5E04"><title>Enrichment</title>
            <XDFAXIS id="x"><indexcount>3</indexcount><embedinfo type="3" linkobjid="0xBF2"/></XDFAXIS>
            <XDFAXIS id="y"><indexcount>2</indexcount><embedinfo type="3" linkobjid="0x999"/></XDFAXIS>
            <XDFAXIS id="z"><EMBEDDEDDATA mmedaddress="0x300" mmedelementsizebits="8" mmedrowcount="2" mmedcolcount="3"/>
            <MATH equation="X"/></XDFAXIS></XDFTABLE></XDFFORMAT>"#;
        let imp = parse(xml).unwrap();
        let m = imp.maps.iter().find(|m| m.name == "Enrichment").unwrap();
        let x = m.x_axis.as_ref().unwrap();
        assert_eq!(x.address, Some(0x200));
        assert_eq!(x.unit, "rpm");
        assert!((x.scaling.factor - 50.0).abs() < 1e-12);
        assert_eq!(x.name, "RPM breakpoints");
        // The y link points nowhere: warned, count only.
        let y = m.y_axis.as_ref().unwrap();
        assert_eq!((y.address, y.count), (None, 2));
        assert_eq!(
            kinds_of(report(&imp, "Enrichment")),
            vec![kinds::AXIS_UNRESOLVED]
        );
    }

    #[test]
    fn scalar_constants_are_imported_separately() {
        let xml = r#"<XDFFORMAT version="1.60"><XDFHEADER><deftitle>TCU</deftitle>
          <BASEOFFSET offset="0" subtract="0"/></XDFHEADER>
          <XDFCONSTANT uniqueid="0xB"><title>Oil temp disable</title>
            <EMBEDDEDDATA mmedaddress="0x6B8EE" mmedelementsizebits="16" mmedtypeflags="0x00"/>
            <units>degC</units><rangehigh>150.0</rangehigh>
            <MATH equation="X*0.1"/></XDFCONSTANT>
          <XDFTABLE uniqueid="0x3"><title>A Table</title><XDFAXIS id="z">
            <EMBEDDEDDATA mmedaddress="0x1000" mmedelementsizebits="8" mmedrowcount="2" mmedcolcount="2" mmedtypeflags="0x00"/>
            <MATH equation="X"/></XDFAXIS></XDFTABLE></XDFFORMAT>"#;
        let imp = parse(xml).expect("parse");
        assert_eq!(imp.maps.len(), 1);
        assert_eq!(imp.constants.len(), 1);
        let c = &imp.constants[0];
        assert_eq!(c.name, "Oil temp disable");
        assert_eq!(c.address, 0x6B8EE);
        assert_eq!(c.data_type, DataType::U16);
        assert_eq!(c.endian, Endian::Big);
        assert_eq!(c.unit, "degC");
        assert_eq!(c.max, Some(150.0));
        assert!((c.scaling.factor - 0.1).abs() < 1e-12);
        assert_eq!(
            report(&imp, "Oil temp disable").object,
            ObjectKind::Constant
        );
    }

    #[test]
    fn constants_only_file_is_not_empty() {
        let xml = r#"<XDFFORMAT version="1.60"><XDFHEADER><deftitle>C</deftitle></XDFHEADER>
          <XDFCONSTANT uniqueid="0x1"><title>K</title>
            <EMBEDDEDDATA mmedaddress="0x40" mmedelementsizebits="8" mmedtypeflags="0x00"/>
            <MATH equation="X"/></XDFCONSTANT></XDFFORMAT>"#;
        let imp = parse(xml).expect("constants-only must parse");
        assert!(imp.maps.is_empty());
        assert_eq!(imp.constants.len(), 1);
    }

    #[test]
    fn constant_address_underflow_is_refused() {
        let xml = r#"<XDFFORMAT version="1.60"><XDFHEADER><deftitle>C</deftitle>
          <BASEOFFSET offset="0x100" subtract="1"/></XDFHEADER>
          <XDFCONSTANT uniqueid="0x1"><title>K</title>
            <EMBEDDEDDATA mmedaddress="0x10" mmedelementsizebits="8" mmedtypeflags="0x00"/>
            <MATH equation="X"/></XDFCONSTANT>
          <XDFTABLE uniqueid="0x3"><title>T</title><XDFAXIS id="z">
            <EMBEDDEDDATA mmedaddress="0x1000" mmedelementsizebits="8" mmedrowcount="2" mmedcolcount="2" mmedtypeflags="0x00"/>
            <MATH equation="X"/></XDFAXIS></XDFTABLE></XDFFORMAT>"#;
        let imp = parse(xml).expect("parse");
        assert!(imp.constants.is_empty(), "underflowing constant skipped");
        assert!(imp
            .warnings
            .iter()
            .any(|w| w.contains("constant") && w.contains("underflow")));
    }

    #[test]
    fn column_major_flag_imports_transposed_with_axes_swapped() {
        // Asymmetric 3x5 table stored column-major (0x04). The bytes are a 5x3
        // row-major table with the axes exchanged; import exactly that.
        let axes = r#"<XDFAXIS id="x"><indexcount>5</indexcount>
            <LABEL index="0" value="1"/><LABEL index="1" value="2"/><LABEL index="2" value="3"/>
            <LABEL index="3" value="4"/><LABEL index="4" value="5"/></XDFAXIS>
            <XDFAXIS id="y"><indexcount>3</indexcount>
            <LABEL index="0" value="10"/><LABEL index="1" value="20"/><LABEL index="2" value="30"/></XDFAXIS>"#;
        let xml = one_table(
            "",
            r#"mmedaddress="0x6F47E" mmedelementsizebits="16" mmedrowcount="3" mmedcolcount="5" mmedtypeflags="0x04""#,
            "X",
            axes,
        );
        let imp = parse(&xml).expect("parse");
        let m = &imp.maps[0];
        assert_eq!((m.rows, m.cols), (5, 3));
        assert_eq!(m.x_axis.as_ref().unwrap().values, vec![10.0, 20.0, 30.0]);
        assert_eq!(
            m.y_axis.as_ref().unwrap().values,
            vec![1.0, 2.0, 3.0, 4.0, 5.0]
        );
        assert_eq!(m.endian, Endian::Big);
        assert_eq!(m.data_type, DataType::U16);
        assert!(m.notes.contains("column-major"), "notes: {}", m.notes);
        // Faithful, so clean.
        assert_eq!(report(&imp, "Tbl").status, Status::Clean);

        // On a vector the flag changes nothing.
        let vec = one_table(
            "",
            r#"mmedaddress="0x10" mmedelementsizebits="16" mmedrowcount="1" mmedcolcount="5" mmedtypeflags="0x04""#,
            "X",
            "",
        );
        let imp = parse(&vec).unwrap();
        assert_eq!((imp.maps[0].rows, imp.maps[0].cols), (1, 5));
        assert!(!imp.maps[0].notes.contains("column-major"));
    }

    #[test]
    fn functions_import_as_curves() {
        let xml = r#"<XDFFORMAT><XDFHEADER/>
          <XDFFUNCTION uniqueid="0x5E"><title>MAF</title>
            <XDFAXIS id="x"><EMBEDDEDDATA mmedaddress="0xD226" mmedelementsizebits="16" mmedcolcount="3" mmedmajorstridebits="16"/>
              <units>V</units><indexcount>3</indexcount><embedinfo type="1"/><MATH equation="X/1023*5"><VAR id="x"/></MATH></XDFAXIS>
            <XDFAXIS id="y"><EMBEDDEDDATA mmedtypeflags="0x01" mmedaddress="0xD268" mmedelementsizebits="16" mmedcolcount="3" mmedmajorstridebits="16"/>
              <units>kg/h</units><indexcount>3</indexcount><embedinfo type="1"/><MATH equation="x/4"><VAR id="x"/></MATH></XDFAXIS>
          </XDFFUNCTION></XDFFORMAT>"#;
        let imp = parse(xml).unwrap();
        let m = &imp.maps[0];
        assert_eq!((m.rows, m.cols, m.address), (1, 3, 0xD268));
        assert_eq!(m.data_type, DataType::S16);
        assert_eq!(m.unit, "kg/h");
        assert!((m.scaling.factor - 0.25).abs() < 1e-12);
        let x = m.x_axis.as_ref().unwrap();
        assert_eq!(x.address, Some(0xD226));
        assert!((x.scaling.factor - 5.0 / 1023.0).abs() < 1e-12);
        let r = report(&imp, "MAF");
        assert_eq!((r.object, r.status), (ObjectKind::Function, Status::Clean));
    }

    #[test]
    fn flags_patches_checksums_are_refused_out_of_scope() {
        let xml = r#"<XDFFORMAT><XDFHEADER/>
          <XDFFLAG uniqueid="0x1"><title>Limp inhibit</title><mask>0x01</mask>
            <EMBEDDEDDATA mmedaddress="0x10" mmedelementsizebits="8"/></XDFFLAG>
          <XDFPATCH uniqueid="0x2"><title>Fuel pump</title>
            <XDFPATCHENTRY name="p" address="0x20" datasize="0x2" patchdata="0000"/></XDFPATCH>
          <XDFCHECKSUM uniqueid="0x3"><title>Main sum</title><REGION/></XDFCHECKSUM>
          <XDFTABLE><title>T</title><XDFAXIS id="z"><EMBEDDEDDATA mmedaddress="0x40" mmedelementsizebits="8"/></XDFAXIS></XDFTABLE>
        </XDFFORMAT>"#;
        let imp = parse(xml).unwrap();
        assert_eq!(imp.maps.len(), 1);
        for (name, kind) in [
            ("Limp inhibit", ObjectKind::Flag),
            ("Fuel pump", ObjectKind::Patch),
            ("Main sum", ObjectKind::Checksum),
        ] {
            let r = report(&imp, name);
            assert_eq!((r.object, r.status), (kind, Status::Refused));
            assert_eq!(r.issues[0].kind, kinds::UNSUPPORTED_OBJECT);
        }
        assert!(report(&imp, "Limp inhibit").issues[0]
            .message
            .contains("mask 0x01"));
        // Out-of-scope objects are listed but do not fail strict mode.
        assert!(imp.strict_failures().is_empty());
        assert_eq!(imp.summary().out_of_scope, 3);
    }

    #[test]
    fn table_without_data_is_a_placeholder() {
        // Separator headings / info entries carry no data address at all.
        let xml = r#"<XDFFORMAT><XDFTABLE><title>----IDLE----</title>
            <XDFAXIS id="z"><EMBEDDEDDATA mmedelementsizebits="8"/></XDFAXIS></XDFTABLE>
            <XDFCONSTANT><title>Info</title><EMBEDDEDDATA/></XDFCONSTANT></XDFFORMAT>"#;
        let imp = parse(xml).unwrap();
        assert!(imp.maps.is_empty() && imp.constants.is_empty());
        let r = report(&imp, "----IDLE----");
        assert_eq!(
            (r.object, r.status),
            (ObjectKind::Placeholder, Status::Refused)
        );
        assert_eq!(report(&imp, "Info").object, ObjectKind::Placeholder);
        assert!(
            imp.strict_failures().is_empty(),
            "nothing lost, nothing to refuse"
        );

        // An address that is present but unparsable is a real refusal.
        let bad = xml.replace(
            r#"<EMBEDDEDDATA mmedelementsizebits="8"/>"#,
            r#"<EMBEDDEDDATA mmedaddress="zz" mmedelementsizebits="8"/>"#,
        );
        let imp = parse(&bad).unwrap();
        let r = report(&imp, "----IDLE----");
        assert_eq!(
            (r.object, r.issues[0].kind),
            (ObjectKind::Table, kinds::ADDRESS_MISSING)
        );
    }
}
