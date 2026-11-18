# opentune-xdf

Import **TunerPro XDF** map definitions into OpenTune ECU definitions.

TunerPro `.xdf` files are community-authored descriptions of where the
calibration tables live inside an ECU flash image and how to decode them. There
is a very large public library of them (Bosch EDC15/16/17, ME7, GM, Daewoo,
etc.). This crate reads an XDF and emits a list of
`opentune_core::ecudef::MapDef` ready to merge into an OpenTune ECU definition —
unlocking that library without re-deriving every address by hand.

## API

```rust
pub fn parse_xdf(src: &str) -> opentune_xdf::Result<XdfImport>;
pub fn parse_xdf_strict(src: &str) -> opentune_xdf::Result<XdfImport>; // Err(Strict) unless clean

pub struct XdfImport {
    pub title: Option<String>,   // definition title from the XDF header
    pub base_offset: u32,        // header base offset magnitude (already applied)
    pub base_subtract: bool,     // base offset is subtracted
    pub maps: Vec<opentune_core::ecudef::MapDef>,
    pub constants: Vec<XdfConstant>, // scalar <XDFCONSTANT> values (XML only)
    pub warnings: Vec<String>,   // one line per issue of every non-clean object
    pub reports: Vec<ObjectReport>, // per object: clean / warned / refused + issues
}
```

Every `<XDFTABLE>`, `<XDFFUNCTION>`, `<XDFCONSTANT>`, `<XDFFLAG>`,
`<XDFPATCH>` and `<XDFCHECKSUM>` ends in one `report::ObjectReport`:
**clean** (faithful), **warned** (imported with an explicit loss, also in the
map `notes` as `WARN[kind] …`) or **refused** (with the reason). Nothing is
imported with a silently wrong address, cell type or scale. The full
construct → outcome table is in [`docs/MAP-DEFS.md`](../../docs/MAP-DEFS.md#importing-other-definition-formats).
`XdfImport::into_strict()` / `--strict` refuse the file when any table,
function or constant is not clean (flags/patches/checksums/separators are
out of scope and don't count). The `report` module is shared with the
EcuFlash/RomRaider/FastECU importers.

`warnings` carries file-level fidelity losses that do not belong to a single
map's `notes` — a table/constant skipped because its address rebasing
underflowed, categorical axis labels the numeric axis model can't store, or a
swap-XY storage flag OpenTune does not model. Surface them: a non-empty `maps`
list does **not** mean the import was lossless.

`parse_xdf` auto-detects the encoding and dispatches. The `xml` and `legacy`
sub-modules are also public if you want to force one.

Each `<XDFTABLE>` / `%%TABLE%%` becomes one `MapDef`:

| MapDef field | Source |
|---|---|
| `name` | table `<title>` / `Title` (falls back to `table_<uniqueid>`) |
| `notes` | table description + any uncertainty/limitation flags (see below) |
| `address` | `mmedaddress` / `Address`, resolved through the header base offset |
| `rows`, `cols` | `mmedrowcount`/`mmedcolcount` (XML) or `Rows`/`Cols` (legacy) |
| `data_type` | element size bits + sign flag → `u8/s8/u16/s16/u32/s32`, or `f32` on the float flag |
| `endian` | `mmedtypeflags` LSB-first bit (default big-endian / Motorola) |
| `scaling` | linear `factor`/`offset` recovered from the z-axis `<MATH>` / `ZEq` |
| `unit` | z-axis `<units>` / `ZUnits` |
| `x_axis`, `y_axis` | inline `<LABEL>`/`*Labels`, or a stored (addressed) axis |
| `min`, `max` | z-axis `<min>`/`<max>` or legacy `RangeLow`/`RangeHigh` |

## Two on-disk formats

Both encodings seen in the community library are handled.

**Modern XML XDF** (TunerPro v5, the primary target):
`<XDFFORMAT>` → `<XDFHEADER>` (`<deftitle>`, `<BASEOFFSET offset subtract>`,
`<DEFAULTS lsbfirst>`) → `<XDFTABLE>` → up to three `<XDFAXIS id="x|y|z">`, each
with `<EMBEDDEDDATA mmedaddress mmedelementsizebits mmedrowcount mmedcolcount
mmedtypeflags mmed*stridebits>`, `<MATH equation>`, `<LABEL value>`, `<units>`,
`<indexcount>`, `<min>`, `<max>`.

**Legacy text XDF** (`XDF\n1.11…`, the older `%%TABLE%%` key/value format),
still common in older archives: header `BaseOffset`/`DefTitle`, table
`Title`/`Address`/`Rows`/`Cols`/`ZEq`/`ZUnits`/`XLabels`/`YLabels`/`RangeLow`/
`RangeHigh`.

## Scaling: linear only, non-linear never guessed

OpenTune scaling is strictly linear (`physical = raw * factor + offset`). The
MATH/ZEq equation is **evaluated symbolically** at several sample points and, if
it behaves as an affine function of `X`, its exact slope and intercept are
recovered. This handles every linear shape without pattern lists — `X`,
`a*X+b`, `(a*X)+b`, `X*a`, `X/d`, `(45*X)/127-20`, `0.351563*X + -35.16`, hex
constants, etc.

A **non-linear** equation (e.g. `1638/X`, `X*X`, `X^2`, a square root, an
unknown identifier) fails the linearity check and is imported **pre-scaled**
(`factor = 1`, `offset = 0`) with the original equation recorded in `notes` —
it is never approximated.

## `mmedtypeflags` interpretation

| Bit | Meaning here |
|---|---|
| `0x01` | signed (two's complement) |
| `0x02` | LSB first (little-endian / Intel); clear ⇒ big-endian (Motorola) |
| `0x04` | column-major storage. A 2-D table is imported **transposed** (rows/cols and x/y axes exchanged — the same bytes read the same way); a no-op on vectors |
| `0x10000` | IEEE-754 float (32-bit elements only; anything else is refused) |

Other bits are reported (`typeflags-unknown`). Without a flag word the header
`DEFAULTS` apply and the map is warned only where that changes the decoding
(e.g. `lsbfirst=1` on a multi-byte cell).

These are the community-documented readings. Endianness in XDF is historically
ambiguous (a per-table flag vs. a file-level `DEFAULTS lsbfirst`); when a table
carries no type flags the file default is used and the map's `notes` says so.
**Verify endianness against a bin of known byte order** before trusting a bulk
import.

## Limitations (be honest about these)

- **Legacy format cell type.** The legacy text format records a value's
  *display* type (`OutType`), not a reliable per-cell *storage* size for a
  table. The element type is therefore defaulted to `u8`/big-endian and every
  such map is flagged in `notes`. Confirm the real width/endianness against your
  readout.
- **Non-linear math** is pre-scaled + warned, not converted (see above); so is
  a `<MATH>` that uses `VAR`s other than `X` (`type="address"`/`"link"`).
- **Strides.** `mmedmajorstridebits`/`mmedminorstridebits` other than plain
  packing (interleaved GM tables, every-Nth-word vectors, negative = reversed)
  are **refused**: a `MapDef` is contiguous. Label-only axes carry `-32` strides
  that mean nothing (no data is read) and are ignored.
- **Non-standard element widths** (24-bit, 64-bit, non-32-bit floats) are
  **refused**, never approximated.
- **Axes.** `embedinfo type="1"` = embedded (stored axis), `type="3"
  linkobjid` = linked to another 1-D table (resolved to its storage), labels
  otherwise. An axis whose breakpoints can't be resolved keeps its count and
  the map is warned. A dimension of 1 gets no axis.
- **Categorical axis labels.** Real TCU/BMW XDFs carry non-numeric axis labels
  (gear/mode names such as `1->2`, `2->3`). OpenTune's `AxisDef` stores only
  numeric breakpoints, so these text labels cannot be kept as values. The axis
  dimension (`count`) and the table data are still imported; the dropped labels
  are listed in the map `notes` **and** in `XdfImport.warnings` rather than
  silently discarded. (If the core axis model gains a categorical/label field,
  switch to preserving the text directly.)
- **Column-major (`mmedtypeflags 0x04`).** The bytes of an R×C column-major
  table are exactly a C×R row-major table whose axes are exchanged, so that is
  what is imported (noted in `notes`). Every cell keeps its value and its
  breakpoints; `export_xdf` writes the row-major view, which TunerPro reads
  identically. (Earlier versions only flagged the bit.) Evidence: GM XDFs whose
  0x04 tables are interleaved column by column, and RomRaider's converter,
  which treats the bit as a storage-order swap.
- **Address rebasing is checked.** Both header forms are read: TunerPro 5
  `<BASEOFFSET offset subtract>` and the older signed text form
  `<baseoffset>-49152</baseoffset>` (previously ignored — every address of such
  a file was off by the base). Underflow/overflow refuses the object; never a
  wrapped address. A whole-file `REGION` bounds every object: data past it is
  refused.
- **Character encoding.** XDF payload text (titles, units) may be
  Windows-1251/Latin-1; pass the bytes in as a string (lossy UTF-8 is fine for
  structure). Non-ASCII units may come through as mojibake — that is a source
  encoding issue, not a parse error.
- **Scalar constants.** Modern-XML `<XDFCONSTANT>` entries are imported as
  [`XdfConstant`] scalars into `XdfImport.constants` (a distinct list, not folded
  into `maps`). Only plain numeric scalars are imported — a bitfield/switch is
  **not** promoted to an unrestricted scalar write.
- **Refused as out of scope** (listed in `reports`, never imported):
  `<XDFFLAG>`/`%%FLAG%%` bit flags, `<XDFPATCH>`, `<XDFCHECKSUM>`, and
  separator/info entries with no data address. DALINK (live-data links) is
  ignored. Legacy `%%TABLE%%` files expose only tables (no constants list).
- **Binary (pre-XML) XDFs** are refused with `Error::Binary` — re-save them
  as XML in TunerPro.

## CLI

Part of the workspace. The CLI exposes it as `opentune import-xdf <file>
[--into <ecudef.json>] [-o out.json] [--strict]` (prints the per-object
report and the maps, writes `{maps, constants, report}` JSON, or merges
`XdfImport.maps` into the target `EcuDef.maps`) and `opentune export-xdf`
for the reverse direction.

## Tested on

- Corpus run (ignored test `tests/corpus.rs`, `OPENTUNE_XDF_CORPUS=<dir>`)
  over 219 unique public community XDFs (BMW MSS54/MSD8x/6HP, GM LS1/E38,
  Alfa/Fiat Motronic, Moto Guzzi, Ducati …; not shipped). See the numbers in
  `docs/MAP-DEFS.md`.

- Synthetic inline XDF fixtures (XML and legacy) — the unit tests, so they pass
  without any external files.
- Two real community files from the tuning library:
  `Lanos DXUB3-14-3.xdf` (legacy, Daewoo) → 25 maps, and
  `94-95 LQ1 3.4 V6 … 16196401.xdf` (legacy, GM) → 94 maps, with addresses,
  dimensions and linear scaling recovered and the GM file's two `1638/X`
  reciprocal equations correctly flagged non-linear.
