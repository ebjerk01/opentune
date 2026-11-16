//! ASAM MCD-2 MC (**A2L**) → OpenTune map-definition importer.
//!
//! An A2L file is the factory calibration description that ships (or is derived
//! from) an ECU's software. Unlike a community heuristic map-finder, an A2L
//! *names* every `CHARACTERISTIC` (map / curve / scalar), gives its exact
//! `ECU_ADDRESS`, its dimensions, its `RECORD_LAYOUT` (element data type and byte
//! order) and its `COMPU_METHOD` (raw→physical conversion), plus the `AXIS_PTS`
//! breakpoint tables. This crate parses an A2L and emits
//! [`opentune_core::ecudef::MapDef`] values ready to merge into an OpenTune ECU
//! definition — the precise counterpart to the XDF importer.
//!
//! # Honesty / linearity (same bar as `opentune-xdf`)
//!
//! OpenTune [`Scaling`] is strictly linear (`physical = raw * factor + offset`).
//!
//! * A `RAT_FUNC`/`LINEAR`/`IDENTITY` `COMPU_METHOD` that *is* affine → its exact
//!   `factor`+`offset` are recovered (see [`compu`]).
//! * A `TAB_INTP`/`TAB_NOINTP`/`TAB_VERB`/`FORM` or a non-affine `RAT_FUNC` →
//!   imported **pre-scaled** (`factor = 1`, `offset = 0`) with the conversion
//!   recorded in `notes`. It is **never** guessed.
//! * Every defaulted or uncertain field — a missing/odd record layout, an
//!   assumed endianness, an unresolved axis reference, a dropped higher
//!   dimension — is flagged in the map's `notes`.
//!
//! # Address spaces
//!
//! An A2L `ECU_ADDRESS` is a **CPU/logical address**, not necessarily a file
//! offset into a flash dump. Reaching the byte in a `.bin` may require
//! subtracting a base (the flash region's load address) or adding a segment
//! offset. This crate copies the address through **verbatim** (masked to 32-bit)
//! and does not invent an offset; see the README for how to apply one.

use opentune_core::ecudef::{AxisDef, DataType, Endian, MapDef, Scaling};
use std::collections::HashMap;
use std::fmt;

pub mod compu;
pub mod export;
pub mod lex;
pub mod num;

pub use export::export_a2l;

use compu::{parse_compu_method, CompuMethod};
use lex::Block;
use num::{parse_f64, parse_u32};

/// Result of importing one A2L file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct A2lImport {
    /// `PROJECT` identifier, if present.
    pub project: Option<String>,
    /// `MODULE` identifier, if present.
    pub module: Option<String>,
    /// The imported maps, one per convertible `CHARACTERISTIC`.
    pub maps: Vec<MapDef>,
}

/// Errors from importing an A2L.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// No `MODULE` block was found (not a usable A2L).
    NoModule,
    /// The file parsed but contained no convertible `CHARACTERISTIC`.
    Empty,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoModule => write!(f, "A2L contained no MODULE block"),
            Error::Empty => write!(f, "A2L contained no importable CHARACTERISTIC"),
        }
    }
}

impl std::error::Error for Error {}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Parse an A2L document into an [`A2lImport`].
pub fn parse_a2l(src: &str) -> Result<A2lImport> {
    let root = lex::parse_tree(src);

    let project = root.find_descendant("PROJECT");
    let module = project
        .and_then(|p| p.child("MODULE"))
        .or_else(|| root.find_descendant("MODULE"))
        .ok_or(Error::NoModule)?;

    let project_name = project
        .and_then(|p| p.word(0))
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let module_name = module.word(0).filter(|s| !s.is_empty()).map(str::to_string);

    // Index the supporting blocks by name.
    let record_layouts: HashMap<&str, RecordLayout> = collect(module, "RECORD_LAYOUT")
        .into_iter()
        .map(|b| (b.word(0).unwrap_or_default(), parse_record_layout(b)))
        .collect();
    let compu_methods: HashMap<&str, CompuMethod> = collect(module, "COMPU_METHOD")
        .into_iter()
        .map(|b| {
            let cm = parse_compu_method(b);
            (b.word(0).unwrap_or_default(), cm)
        })
        .collect();
    let axis_pts: HashMap<&str, AxisPts> = collect(module, "AXIS_PTS")
        .into_iter()
        .map(|b| (b.word(0).unwrap_or_default(), parse_axis_pts(b)))
        .collect();

    // MOD_COMMON BYTE_ORDER is the module-wide endianness default.
    let mod_common_endian = module.find_descendant("MOD_COMMON").and_then(byte_order_of);

    let ctx = Ctx {
        record_layouts: &record_layouts,
        compu_methods: &compu_methods,
        axis_pts: &axis_pts,
        default_endian: mod_common_endian,
    };

    let mut maps = Vec::new();
    for ch in collect(module, "CHARACTERISTIC") {
        maps.extend(map_characteristic(ch, &ctx));
    }

    if maps.is_empty() {
        return Err(Error::Empty);
    }

    Ok(A2lImport {
        project: project_name,
        module: module_name,
        maps,
    })
}

/// Gather every descendant block with the given tag.
fn collect<'a>(block: &'a Block, tag: &str) -> Vec<&'a Block> {
    let mut v = Vec::new();
    block.descendants_tagged(tag, &mut v);
    v
}

// ---------------------------------------------------------------------------
// Supporting-block models
// ---------------------------------------------------------------------------

/// The fields of a `RECORD_LAYOUT` this crate cares about.
#[derive(Debug, Clone, Default)]
struct RecordLayout {
    /// `FNC_VALUES` element data type (the map/value storage type).
    fnc: Option<(DataType, bool)>,
    /// `AXIS_PTS_X` element data type (stored X breakpoints).
    axis_x: Option<(DataType, bool)>,
    /// `AXIS_PTS_Y` element data type (stored Y breakpoints).
    axis_y: Option<(DataType, bool)>,
    /// A record-layout-level `BYTE_ORDER`, if any (overrides the module default).
    byte_order: Option<Endian>,
    /// Layout *position* of `FNC_VALUES` (the number after the keyword). Positions
    /// order the components in memory and are what the inline-offset computation
    /// in [`compute_embedded`] walks.
    fnc_pos: Option<u32>,
    /// Layout position of `AXIS_PTS_X`.
    axis_x_pos: Option<u32>,
    /// Layout position of `AXIS_PTS_Y`.
    axis_y_pos: Option<u32>,
    /// `NO_AXIS_PTS_X` `(position, stored count element type)` — a stored
    /// axis-point-count field that occupies bytes ahead of the data.
    no_axis_x: Option<(u32, DataType)>,
    /// `NO_AXIS_PTS_Y` `(position, stored count element type)`.
    no_axis_y: Option<(u32, DataType)>,
    /// True when the layout contains a positioned component whose byte size this
    /// crate does not model (alignment padding, rescale tables, src/rip address
    /// words, a Z/4/5 axis …). The inline-offset computation then refuses to
    /// guess rather than return a wrong address.
    has_unmodeled: bool,
}

/// Positioned `RECORD_LAYOUT` components whose byte footprint this crate does
/// **not** model. Their presence makes an inline (STD_AXIS / embedded FNC_VALUES)
/// offset uncertain, so [`compute_embedded`] declines to compute one — honesty
/// over a plausible-but-wrong address.
const UNMODELED_LAYOUT: &[&str] = &[
    "ALIGNMENT_BYTE",
    "ALIGNMENT_WORD",
    "ALIGNMENT_LONG",
    "ALIGNMENT_INT64",
    "ALIGNMENT_FLOAT16_IEEE",
    "ALIGNMENT_FLOAT32_IEEE",
    "ALIGNMENT_FLOAT64_IEEE",
    "AXIS_PTS_Z",
    "AXIS_PTS_4",
    "AXIS_PTS_5",
    "AXIS_RESCALE_X",
    "AXIS_RESCALE_Y",
    "AXIS_RESCALE_Z",
    "AXIS_RESCALE_4",
    "AXIS_RESCALE_5",
    "DIST_OP_X",
    "DIST_OP_Y",
    "DIST_OP_Z",
    "DIST_OP_4",
    "DIST_OP_5",
    "IDENTIFICATION",
    "NO_AXIS_PTS_Z",
    "NO_AXIS_PTS_4",
    "NO_AXIS_PTS_5",
    "NO_RESCALE_X",
    "NO_RESCALE_Y",
    "NO_RESCALE_Z",
    "NO_RESCALE_4",
    "NO_RESCALE_5",
    "OFFSET_X",
    "OFFSET_Y",
    "OFFSET_Z",
    "OFFSET_4",
    "OFFSET_5",
    "RESERVED",
    "RIP_ADDR_W",
    "RIP_ADDR_X",
    "RIP_ADDR_Y",
    "RIP_ADDR_Z",
    "RIP_ADDR_4",
    "RIP_ADDR_5",
    "SHIFT_OP_X",
    "SHIFT_OP_Y",
    "SHIFT_OP_Z",
    "SHIFT_OP_4",
    "SHIFT_OP_5",
    "SRC_ADDR_X",
    "SRC_ADDR_Y",
    "SRC_ADDR_Z",
    "SRC_ADDR_4",
    "SRC_ADDR_5",
];

/// Parse a `RECORD_LAYOUT` block. The relevant entries are `FNC_VALUES`,
/// `AXIS_PTS_X`/`AXIS_PTS_Y` (each `<keyword> <position> <datatype> …`), the
/// `NO_AXIS_PTS_X`/`_Y` count fields and an optional `BYTE_ORDER`.
fn parse_record_layout(block: &Block) -> RecordLayout {
    let dt_after = |kw: &str| block.after_keyword(kw, 2).and_then(datatype_of);
    let pos_after = |kw: &str| block.after_keyword(kw, 1).and_then(parse_u32);
    let no_axis = |kw: &str| -> Option<(u32, DataType)> {
        let pos = block.after_keyword(kw, 1).and_then(parse_u32)?;
        // `NO_AXIS_PTS_X <position> <datatype>`: a single stored count element.
        let dt = block
            .after_keyword(kw, 2)
            .and_then(datatype_of)
            .map(|(d, _)| d)
            .unwrap_or(DataType::U8);
        Some((pos, dt))
    };
    RecordLayout {
        fnc: dt_after("FNC_VALUES"),
        axis_x: dt_after("AXIS_PTS_X"),
        axis_y: dt_after("AXIS_PTS_Y"),
        byte_order: byte_order_of(block),
        fnc_pos: pos_after("FNC_VALUES"),
        axis_x_pos: pos_after("AXIS_PTS_X"),
        axis_y_pos: pos_after("AXIS_PTS_Y"),
        no_axis_x: no_axis("NO_AXIS_PTS_X"),
        no_axis_y: no_axis("NO_AXIS_PTS_Y"),
        has_unmodeled: block
            .words
            .iter()
            .any(|t| t.text().is_some_and(|w| UNMODELED_LAYOUT.contains(&w))),
    }
}

/// Byte offsets, from a characteristic's base (`ECU_ADDRESS`), of the inline
/// components of its record layout — computed **only** when every component that
/// precedes the target can be sized exactly. A `None` field means "absent or not
/// computable"; the caller must never substitute a guess.
#[derive(Debug, Clone, Copy, Default)]
struct Embedded {
    /// Offset of the `FNC_VALUES` (table data) block.
    fnc: Option<u32>,
    /// Offset of inline `AXIS_PTS_X` breakpoints (a STD_AXIS X axis).
    axis_x: Option<u32>,
    /// Offset of inline `AXIS_PTS_Y` breakpoints (a STD_AXIS Y axis).
    axis_y: Option<u32>,
}

/// A characteristic's base address paired with its computed inline offsets, so a
/// STD_AXIS can resolve its embedded breakpoint address.
#[derive(Debug, Clone, Copy)]
struct Layout {
    base: u32,
    emb: Embedded,
}

/// Walk a record layout's positioned, sizeable components in position order and
/// return the byte offset at which each of `FNC_VALUES`, `AXIS_PTS_X` and
/// `AXIS_PTS_Y` begins. `rows`/`cols` give the element counts (Y/X). Returns all
/// `None` when the layout has an unmodeled component (see [`UNMODELED_LAYOUT`]).
fn compute_embedded(rl: &RecordLayout, rows: u32, cols: u32) -> Embedded {
    if rl.has_unmodeled {
        return Embedded::default();
    }
    /// Which component a layout entry is (for recording its start offset).
    #[derive(Clone, Copy)]
    enum Comp {
        Fnc,
        AxX,
        AxY,
        /// A sized component that occupies bytes but is not a target (a count field).
        Filler,
    }
    let mut entries: Vec<(u32, u32, Comp)> = Vec::new();
    if let (Some(pos), Some((dt, _))) = (rl.fnc_pos, rl.fnc) {
        entries.push((
            pos,
            rows.saturating_mul(cols).saturating_mul(dt.byte_len()),
            Comp::Fnc,
        ));
    }
    if let (Some(pos), Some((dt, _))) = (rl.axis_x_pos, rl.axis_x) {
        entries.push((pos, cols.saturating_mul(dt.byte_len()), Comp::AxX));
    }
    if let (Some(pos), Some((dt, _))) = (rl.axis_y_pos, rl.axis_y) {
        entries.push((pos, rows.saturating_mul(dt.byte_len()), Comp::AxY));
    }
    if let Some((pos, dt)) = rl.no_axis_x {
        entries.push((pos, dt.byte_len(), Comp::Filler));
    }
    if let Some((pos, dt)) = rl.no_axis_y {
        entries.push((pos, dt.byte_len(), Comp::Filler));
    }
    entries.sort_by_key(|e| e.0);
    let mut off = 0u32;
    let mut out = Embedded::default();
    for (_pos, size, comp) in entries {
        match comp {
            Comp::Fnc => out.fnc = Some(off),
            Comp::AxX => out.axis_x = Some(off),
            Comp::AxY => out.axis_y = Some(off),
            Comp::Filler => {}
        }
        off = off.saturating_add(size);
    }
    out
}

/// Apply the inline `FNC_VALUES` offset to a characteristic's base address so the
/// returned address points at the table data even when axis points are stored
/// ahead of it in the record. A zero or uncomputable offset leaves it unchanged.
fn adjust_fnc_address(base: u32, embedded: Option<&Embedded>, notes: &mut Vec<String>) -> u32 {
    if let Some(off) = embedded.and_then(|e| e.fnc) {
        if off > 0 {
            notes.push(format!(
                "table data at ECU_ADDRESS + 0x{off:X} (axis points stored ahead of FNC_VALUES)"
            ));
            return base.wrapping_add(off);
        }
    }
    base
}

/// An `AXIS_PTS` block: a shared (`COM_AXIS`) breakpoint table.
#[derive(Debug, Clone, Default)]
struct AxisPts {
    address: Option<u32>,
    /// Deposit (`RECORD_LAYOUT`) reference — gives the stored element type.
    deposit: String,
    /// `MaxAxisPoints` (point count).
    count: u32,
}

/// Parse an `AXIS_PTS` block. Positional: `Name "Long" Address InputQuantity
/// Deposit MaxDiff Conversion MaxAxisPoints LowerLimit UpperLimit`.
fn parse_axis_pts(block: &Block) -> AxisPts {
    AxisPts {
        address: block.word(2).and_then(parse_u32),
        deposit: block.word(4).unwrap_or_default().to_string(),
        count: block.word(7).and_then(parse_u32).unwrap_or(0),
    }
}

/// Read a `BYTE_ORDER` keyword from a block (`MSB_FIRST` → big, `MSB_LAST` →
/// little). Also accepts the numeric A2L forms.
fn byte_order_of(block: &Block) -> Option<Endian> {
    match block.after_keyword("BYTE_ORDER", 1)? {
        "MSB_FIRST" | "BIG_ENDIAN" => Some(Endian::Big),
        "MSB_LAST" | "LITTLE_ENDIAN" => Some(Endian::Little),
        _ => None,
    }
}

/// Map an A2L deposit data-type keyword to an OpenTune [`DataType`]. The bool is
/// `true` when the mapping is exact and `false` when it was approximated (a type
/// OpenTune cannot represent natively, e.g. a 64-bit type → 32-bit).
fn datatype_of(name: &str) -> Option<(DataType, bool)> {
    Some(match name {
        "UBYTE" => (DataType::U8, true),
        "SBYTE" => (DataType::S8, true),
        "UWORD" => (DataType::U16, true),
        "SWORD" => (DataType::S16, true),
        "ULONG" => (DataType::U32, true),
        "SLONG" => (DataType::S32, true),
        "FLOAT32_IEEE" => (DataType::F32, true),
        // 64-bit types have no native OpenTune representation; approximate and
        // flag. (Rare in calibration maps.)
        "A_UINT64" => (DataType::U32, false),
        "A_INT64" => (DataType::S32, false),
        "FLOAT64_IEEE" => (DataType::F32, false),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// CHARACTERISTIC -> MapDef
// ---------------------------------------------------------------------------

struct Ctx<'a> {
    record_layouts: &'a HashMap<&'a str, RecordLayout>,
    compu_methods: &'a HashMap<&'a str, CompuMethod>,
    axis_pts: &'a HashMap<&'a str, AxisPts>,
    default_endian: Option<Endian>,
}

/// Convert one `CHARACTERISTIC` into one or more [`MapDef`]s. A normal
/// scalar/curve/map yields a single `MapDef`; a `CUBOID`/`CUBE_4`/`CUBE_5` yields
/// one `MapDef` per X×Y slice (see below). An `ASCII` string yields none.
fn map_characteristic(ch: &Block, ctx: &Ctx) -> Vec<MapDef> {
    let mut notes: Vec<String> = Vec::new();

    let name = match ch.word(0).filter(|s| !s.is_empty()) {
        Some(n) => n.to_string(),
        None => return Vec::new(),
    };
    let long_id = ch.word(1).unwrap_or_default();
    let kind = ch.word(2).unwrap_or_default().to_string();
    let char_addr = match ch.word(3).and_then(parse_u32) {
        Some(a) => a,
        None => return Vec::new(),
    };
    let rl_ref = ch.word(4).unwrap_or_default();
    let cm_ref = ch.word(6).unwrap_or_default();
    let lower = ch.word(7).and_then(parse_f64);
    let upper = ch.word(8).and_then(parse_f64);

    if kind == "ASCII" {
        return Vec::new(); // A string, not a numeric calibration table.
    }

    if !long_id.is_empty() {
        notes.push(long_id.to_string());
    }

    // Element data type + endianness from the referenced RECORD_LAYOUT.
    let rl = ctx.record_layouts.get(rl_ref);
    let (data_type, endian) = resolve_value_type(rl, rl_ref, ctx, &mut notes);

    // Scaling + unit from the referenced COMPU_METHOD.
    let (scaling, unit) = resolve_conversion(cm_ref, ctx, &mut notes);

    // Axis descriptions (ordered: first = X, second = Y, then higher dims).
    let axis_descrs: Vec<&Block> = ch.children_tagged("AXIS_DESCR").collect();

    // The X×Y face dimensions are always the first two axes.
    let cols = axis_count(axis_descrs.first(), ctx).max(1);
    let rows = axis_count(axis_descrs.get(1), ctx).max(1);

    // A cuboid: slice the higher dimensions into separate 2-D maps.
    let is_cuboid = matches!(kind.as_str(), "CUBOID" | "CUBE_4" | "CUBE_5");
    if is_cuboid && axis_descrs.len() > 2 {
        return cuboid_slices(CuboidArgs {
            name: &name,
            kind: &kind,
            char_addr,
            rl,
            axes: &axis_descrs,
            rows,
            cols,
            data_type,
            endian,
            scaling,
            unit: &unit,
            lower,
            upper,
            notes,
            ctx,
        });
    }

    // Dimensions from the kind + axes (handles VALUE/VAL_BLK/CURVE/MAP and a
    // cuboid that degenerates to a 2-D face).
    let (rows, cols) = dimensions(&kind, ch, &axis_descrs, ctx, &mut notes);

    let embedded = rl.map(|r| compute_embedded(r, rows, cols));
    let address = adjust_fnc_address(char_addr, embedded.as_ref(), &mut notes);
    let layout = embedded.map(|emb| Layout {
        base: char_addr,
        emb,
    });

    // Build axis definitions where we can.
    let x_axis = axis_descrs
        .first()
        .and_then(|a| build_axis(a, rl, true, layout.as_ref(), ctx, &mut notes));
    let y_axis = axis_descrs
        .get(1)
        .and_then(|a| build_axis(a, rl, false, layout.as_ref(), ctx, &mut notes));

    vec![MapDef {
        name,
        notes: notes.join("; "),
        address,
        data_type,
        endian,
        rows,
        cols,
        scaling,
        unit,
        x_axis,
        y_axis,
        min: lower,
        max: upper,
    }]
}

/// Inputs to [`cuboid_slices`], grouped to keep the signature readable.
struct CuboidArgs<'a> {
    name: &'a str,
    kind: &'a str,
    char_addr: u32,
    rl: Option<&'a RecordLayout>,
    axes: &'a [&'a Block],
    rows: u32,
    cols: u32,
    data_type: DataType,
    endian: Endian,
    scaling: Scaling,
    unit: &'a str,
    lower: Option<f64>,
    upper: Option<f64>,
    notes: Vec<String>,
    ctx: &'a Ctx<'a>,
}

/// Maximum number of cuboid slices imported, to keep a large table from
/// exploding into thousands of maps. Any beyond this are flagged, not dropped
/// silently.
const CUBOID_SLICE_CAP: u32 = 64;

/// Flatten a >2-D `CUBOID`/`CUBE_4`/`CUBE_5` into one 2-D `MapDef` per X×Y face.
///
/// The ASAM storage order is X innermost (fastest), then Y, then the higher
/// axes, so face *f* lives at `base + f · (rows·cols·elementsize)` and the higher
/// axes advance with the lower one varying fastest. This contiguous-storage
/// assumption is stated in every slice's `notes`; the scaling stays strictly
/// linear and is never guessed.
fn cuboid_slices(a: CuboidArgs) -> Vec<MapDef> {
    let mut notes = a.notes;

    // Higher (sliced) dimensions beyond X and Y, with a human label each.
    let outer: Vec<(u32, String)> = a
        .axes
        .iter()
        .skip(2)
        .enumerate()
        .map(|(k, ax)| {
            let count = axis_count(Some(ax), a.ctx).max(1);
            let label = ax
                .word(1)
                .filter(|s| !s.is_empty() && *s != "NO_INPUT_QUANTITY" && *s != "dummy")
                .map(str::to_string)
                .unwrap_or_else(|| format!("d{}", k + 3));
            (count, label)
        })
        .collect();

    let embedded = a.rl.map(|r| compute_embedded(r, a.rows, a.cols));
    let data_address = adjust_fnc_address(a.char_addr, embedded.as_ref(), &mut notes);
    let layout = embedded.map(|emb| Layout {
        base: a.char_addr,
        emb,
    });

    let face_bytes = a
        .rows
        .saturating_mul(a.cols)
        .saturating_mul(a.data_type.byte_len());
    let total_faces: u64 = outer.iter().map(|(c, _)| *c as u64).product();
    let total_faces = total_faces.max(1);

    let dims_desc = {
        let mut d = vec![a.cols.to_string(), a.rows.to_string()];
        d.extend(outer.iter().map(|(c, l)| format!("{c}({l})")));
        d.join("x")
    };
    notes.push(format!(
        "{kind} {ndim}-D table [{dims_desc}] flattened to {total_faces} X*Y face(s); \
         each imported as a separate 2-D map at base + slice*{face_bytes}B \
         (assumes contiguous storage, X fastest)",
        kind = a.kind,
        ndim = 2 + outer.len(),
    ));

    let emit = total_faces.min(CUBOID_SLICE_CAP as u64) as u32;
    if total_faces > emit as u64 {
        notes.push(format!(
            "only first {emit} of {total_faces} slices imported ({} omitted)",
            total_faces - emit as u64
        ));
    }

    // The X and Y face axes (shared by every slice).
    let x_axis = a
        .axes
        .first()
        .and_then(|ax| build_axis(ax, a.rl, true, layout.as_ref(), a.ctx, &mut notes));
    let y_axis = a
        .axes
        .get(1)
        .and_then(|ax| build_axis(ax, a.rl, false, layout.as_ref(), a.ctx, &mut notes));

    let base_notes = notes.join("; ");
    (0..emit)
        .map(|f| {
            let suffix = slice_label(f, &outer);
            MapDef {
                name: format!("{} [{suffix}]", a.name),
                notes: base_notes.clone(),
                address: data_address.wrapping_add(f.saturating_mul(face_bytes)),
                data_type: a.data_type,
                endian: a.endian,
                rows: a.rows,
                cols: a.cols,
                scaling: a.scaling,
                unit: a.unit.to_string(),
                x_axis: x_axis.clone(),
                y_axis: y_axis.clone(),
                min: a.lower,
                max: a.upper,
            }
        })
        .collect()
}

/// Human label for slice `f` of the outer (higher) dimensions, lowest dimension
/// varying fastest: e.g. `"z=0, w=1"`.
fn slice_label(f: u32, outer: &[(u32, String)]) -> String {
    let mut rem = f;
    let mut parts = Vec::with_capacity(outer.len());
    for (count, label) in outer {
        let c = (*count).max(1);
        parts.push(format!("{label}={}", rem % c));
        rem /= c;
    }
    parts.join(", ")
}

/// Resolve the stored value element type and byte order for a CHARACTERISTIC.
fn resolve_value_type(
    rl: Option<&RecordLayout>,
    rl_ref: &str,
    ctx: &Ctx,
    notes: &mut Vec<String>,
) -> (DataType, Endian) {
    let data_type = match rl.and_then(|r| r.fnc) {
        Some((dt, true)) => dt,
        Some((dt, false)) => {
            notes.push(format!(
                "FNC_VALUES type has no exact OpenTune equivalent; approximated to {dt:?}"
            ));
            dt
        }
        None => {
            if rl.is_none() {
                notes.push(format!(
                    "RECORD_LAYOUT '{rl_ref}' not found; element type defaulted to u8"
                ));
            } else {
                notes.push("RECORD_LAYOUT has no FNC_VALUES; element type defaulted to u8".into());
            }
            DataType::default()
        }
    };
    let endian = resolve_endian(rl.and_then(|r| r.byte_order), ctx, notes);
    (data_type, endian)
}

/// Resolve byte order: a record-layout override, else the MOD_COMMON default,
/// else big-endian with a note.
fn resolve_endian(rl_order: Option<Endian>, ctx: &Ctx, notes: &mut Vec<String>) -> Endian {
    rl_order.or(ctx.default_endian).unwrap_or_else(|| {
        notes.push("no BYTE_ORDER in RECORD_LAYOUT or MOD_COMMON; assumed big-endian".into());
        Endian::Big
    })
}

/// Resolve a COMPU_METHOD reference into a [`Scaling`] and a unit string.
fn resolve_conversion(cm_ref: &str, ctx: &Ctx, notes: &mut Vec<String>) -> (Scaling, String) {
    if cm_ref.is_empty() || cm_ref == "NO_COMPU_METHOD" {
        return (Scaling::default(), String::new());
    }
    match ctx.compu_methods.get(cm_ref) {
        Some(cm) => {
            let (scaling, note) = cm.conv.to_scaling();
            if let Some(n) = note {
                notes.push(format!("conversion {cm_ref}: {n}"));
            }
            (scaling, cm.unit.clone())
        }
        None => {
            notes.push(format!(
                "COMPU_METHOD '{cm_ref}' not found; imported pre-scaled"
            ));
            (Scaling::default(), String::new())
        }
    }
}

/// Compute `(rows, cols)` for a characteristic.
fn dimensions(
    kind: &str,
    ch: &Block,
    axes: &[&Block],
    ctx: &Ctx,
    notes: &mut Vec<String>,
) -> (u32, u32) {
    match kind {
        "VALUE" => (1, 1),
        "VAL_BLK" => val_blk_dims(ch, notes),
        "CURVE" => {
            let c = axis_count(axes.first(), ctx).max(1);
            (1, c)
        }
        "MAP" => {
            let cols = axis_count(axes.first(), ctx).max(1);
            let rows = axis_count(axes.get(1), ctx).max(1);
            (rows, cols)
        }
        "CUBOID" | "CUBE_4" | "CUBE_5" => {
            let cols = axis_count(axes.first(), ctx).max(1);
            let rows = axis_count(axes.get(1), ctx).max(1);
            let extra: Vec<u32> = axes
                .iter()
                .skip(2)
                .map(|a| axis_count(Some(a), ctx).max(1))
                .collect();
            if !extra.is_empty() {
                notes.push(format!(
                    "{kind}: only X*Y imported as a 2-D map; further axis dimensions {extra:?} dropped"
                ));
            }
            (rows, cols)
        }
        other => {
            notes.push(format!(
                "unhandled CHARACTERISTIC type {other}; treated as scalar"
            ));
            (1, 1)
        }
    }
}

/// VAL_BLK dimensions from `MATRIX_DIM` (preferred) or `NUMBER`.
fn val_blk_dims(ch: &Block, notes: &mut Vec<String>) -> (u32, u32) {
    if let Some(p) = ch.keyword_pos("MATRIX_DIM") {
        // Read up to three following numeric tokens (x [y [z]]).
        let dims: Vec<u32> = (1..=3)
            .filter_map(|k| ch.word(p + k).and_then(parse_u32))
            .collect();
        let cols = dims.first().copied().unwrap_or(1).max(1);
        let rows = dims.get(1).copied().unwrap_or(1).max(1);
        if dims.len() > 2 && dims[2] > 1 {
            notes.push(format!(
                "VAL_BLK MATRIX_DIM has a 3rd dimension {}; imported as {rows}x{cols}",
                dims[2]
            ));
        }
        return (rows, cols);
    }
    if let Some(n) = ch.after_keyword("NUMBER", 1).and_then(parse_u32) {
        return (1, n.max(1));
    }
    notes.push("VAL_BLK without MATRIX_DIM/NUMBER; defaulted to 1x1".into());
    (1, 1)
}

/// The point count of an axis description (resolving a COM_AXIS reference to its
/// AXIS_PTS where possible, else the AXIS_DESCR's MaxAxisPoints).
fn axis_count(ax: Option<&&Block>, ctx: &Ctx) -> u32 {
    let ax = match ax {
        Some(a) => *a,
        None => return 1,
    };
    // COM_AXIS: point count lives on the referenced AXIS_PTS.
    if let Some(r) = ax.after_keyword("AXIS_PTS_REF", 1) {
        if let Some(ap) = ctx.axis_pts.get(r) {
            if ap.count > 0 {
                return ap.count;
            }
        }
    }
    ax.word(3).and_then(parse_u32).unwrap_or(1)
}

/// Build an [`AxisDef`] for the first (`is_x`) or second axis of a characteristic.
fn build_axis(
    ax: &Block,
    char_rl: Option<&RecordLayout>,
    is_x: bool,
    layout: Option<&Layout>,
    ctx: &Ctx,
    notes: &mut Vec<String>,
) -> Option<AxisDef> {
    let axis_type = ax.word(0).unwrap_or_default().to_string();
    let input = ax.word(1).unwrap_or_default();
    let cm_ref = ax.word(2).unwrap_or_default();
    let count = axis_count(Some(&ax), ctx).max(1);

    let name = if input.is_empty() || input == "NO_INPUT_QUANTITY" || input == "dummy" {
        String::new()
    } else {
        input.to_string()
    };

    // Scaling + unit from the axis's own COMPU_METHOD.
    let (scaling, unit) = {
        let mut axis_notes = Vec::new();
        let (s, u) = resolve_conversion(cm_ref, ctx, &mut axis_notes);
        for n in axis_notes {
            notes.push(format!("{}-axis {}", if is_x { "x" } else { "y" }, n));
        }
        (s, u)
    };

    match axis_type.as_str() {
        "COM_AXIS" => {
            let r = ax.after_keyword("AXIS_PTS_REF", 1);
            match r.and_then(|r| ctx.axis_pts.get(r).map(|ap| (r, ap))) {
                Some((_r, ap)) => {
                    let dep = ctx.record_layouts.get(ap.deposit.as_str());
                    let (dt, endian) = axis_stored_type(dep, true, ctx, is_x);
                    Some(AxisDef {
                        name,
                        unit,
                        count: if ap.count > 0 { ap.count } else { count },
                        address: ap.address,
                        data_type: dt,
                        endian,
                        scaling,
                        ..Default::default()
                    })
                }
                None => {
                    // Unresolved shared-axis reference: keep the reference name.
                    let refname = r.unwrap_or("").to_string();
                    notes.push(format!(
                        "{}-axis COM_AXIS reference '{refname}' unresolved",
                        if is_x { "x" } else { "y" }
                    ));
                    Some(AxisDef {
                        name,
                        unit,
                        count,
                        scaling,
                        axis_ref: if refname.is_empty() {
                            None
                        } else {
                            Some(refname)
                        },
                        ..Default::default()
                    })
                }
            }
        }
        "STD_AXIS" => {
            // Stored inline in the characteristic's own record layout. If that
            // layout actually declares an AXIS_PTS_X/_Y entry and every preceding
            // component is sizeable, the breakpoint address is computed from the
            // record layout; otherwise it is left unresolved (never guessed).
            let (dt, endian) = axis_stored_type(char_rl, false, ctx, is_x);
            let axis = if is_x { "x" } else { "y" };
            let off = layout.and_then(|l| if is_x { l.emb.axis_x } else { l.emb.axis_y });
            match (off, layout) {
                (Some(o), Some(l)) => {
                    let addr = l.base.wrapping_add(o);
                    notes.push(format!(
                        "{axis}-axis STD_AXIS stored inline; address computed from record layout as ECU_ADDRESS + 0x{o:X}"
                    ));
                    Some(AxisDef {
                        name,
                        unit,
                        count,
                        address: Some(addr),
                        data_type: dt,
                        endian,
                        scaling,
                        ..Default::default()
                    })
                }
                _ => {
                    notes.push(format!(
                        "{axis}-axis STD_AXIS stored inline in the map block (address not computed)"
                    ));
                    Some(AxisDef {
                        name,
                        unit,
                        count,
                        data_type: dt,
                        endian,
                        scaling,
                        ..Default::default()
                    })
                }
            }
        }
        "FIX_AXIS" => {
            let values = fix_axis_values(ax, count);
            if values.is_empty() {
                notes.push(format!(
                    "{}-axis FIX_AXIS has no readable parameters",
                    if is_x { "x" } else { "y" }
                ));
            }
            Some(AxisDef {
                name,
                unit,
                count,
                values,
                scaling,
                ..Default::default()
            })
        }
        other => {
            // RES_AXIS / CURVE_AXIS and anything else: record the dimension but
            // flag that the breakpoints were not resolved.
            notes.push(format!(
                "{}-axis type {other} not resolved to breakpoints",
                if is_x { "x" } else { "y" }
            ));
            if count > 1 || !unit.is_empty() {
                Some(AxisDef {
                    name,
                    unit,
                    count,
                    scaling,
                    ..Default::default()
                })
            } else {
                None
            }
        }
    }
}

/// Stored breakpoint element type for an axis, from a record layout's
/// `AXIS_PTS_X`/`AXIS_PTS_Y` entry. `com` selects X (true) for a COM_AXIS's own
/// deposit layout; otherwise X or Y is chosen by `is_x` for an inline STD axis.
fn axis_stored_type(
    rl: Option<&RecordLayout>,
    com: bool,
    ctx: &Ctx,
    is_x: bool,
) -> (DataType, Endian) {
    let entry = if com || is_x {
        rl.and_then(|r| r.axis_x.or(r.axis_y))
    } else {
        rl.and_then(|r| r.axis_y.or(r.axis_x))
    };
    let dt = entry.map(|(dt, _)| dt).unwrap_or_default();
    // Axis-level endian notes are suppressed to avoid noise; the map-level note
    // already records a defaulted module endianness when one had to be assumed.
    let endian = rl
        .and_then(|r| r.byte_order)
        .or(ctx.default_endian)
        .unwrap_or(Endian::Big);
    (dt, endian)
}

/// Inline breakpoint values from a FIX_AXIS parameter form.
///
/// Supports `FIX_AXIS_PAR_LIST v0 v1 …`, `FIX_AXIS_PAR offset shift n`
/// (`v_i = offset + i·2^shift`) and `FIX_AXIS_PAR_DIST offset dist n`
/// (`v_i = offset + i·dist`).
fn fix_axis_values(ax: &Block, count: u32) -> Vec<f64> {
    if let Some(blk) = ax.child("FIX_AXIS_PAR_LIST") {
        return blk
            .words
            .iter()
            .filter_map(|t| t.text().and_then(parse_f64))
            .collect();
    }
    if let Some(p) = ax.keyword_pos("FIX_AXIS_PAR_LIST") {
        return ax.words[p + 1..]
            .iter()
            .filter_map(|t| t.text().and_then(parse_f64))
            .collect();
    }
    if let Some(p) = ax.keyword_pos("FIX_AXIS_PAR_DIST") {
        let offset = ax.word(p + 1).and_then(parse_f64);
        let dist = ax.word(p + 2).and_then(parse_f64);
        let n = ax.word(p + 3).and_then(parse_u32).unwrap_or(count);
        if let (Some(o), Some(d)) = (offset, dist) {
            return (0..n).map(|i| o + i as f64 * d).collect();
        }
    }
    if let Some(p) = ax.keyword_pos("FIX_AXIS_PAR") {
        let offset = ax.word(p + 1).and_then(parse_f64);
        let shift = ax.word(p + 2).and_then(parse_f64);
        let n = ax.word(p + 3).and_then(parse_u32).unwrap_or(count);
        if let (Some(o), Some(s)) = (offset, shift) {
            let step = 2f64.powf(s);
            return (0..n).map(|i| o + i as f64 * step).collect();
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests;
