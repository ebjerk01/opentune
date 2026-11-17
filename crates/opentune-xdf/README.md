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

pub struct XdfImport {
    pub title: Option<String>,   // definition title from the XDF header
    pub base_offset: u32,        // header base offset (already applied to addresses)
    pub maps: Vec<opentune_core::ecudef::MapDef>,
    pub constants: Vec<XdfConstant>, // scalar <XDFCONSTANT> values (XML only)
    pub warnings: Vec<String>,   // structured import-loss / fidelity warnings
}
```

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
| `0x04` | swap-XY (storage transposed). **Detected and flagged, never applied** — see below |
| `0x10000` | IEEE-754 float (with a 32-bit element) |

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
- **Non-linear math** is pre-scaled + flagged, not converted (see above).
- **Exotic strides.** A negative `mmedmajorstridebits`/`mmedminorstridebits`
  (reversed or non-contiguous layout) is flagged in `notes` but the map is still
  shaped by `rows`×`cols`; the reversed layout itself is not modeled.
- **Non-standard element widths** (e.g. 24-bit) are approximated to the next
  standard type and flagged.
- **Categorical axis labels.** Real TCU/BMW XDFs carry non-numeric axis labels
  (gear/mode names such as `1->2`, `2->3`). OpenTune's `AxisDef` stores only
  numeric breakpoints, so these text labels cannot be kept as values. The axis
  dimension (`count`) and the table data are still imported; the dropped labels
  are listed in the map `notes` **and** in `XdfImport.warnings` rather than
  silently discarded. (If the core axis model gains a categorical/label field,
  switch to preserving the text directly.)
- **Swap-XY (`mmedtypeflags 0x04`).** Some tables (e.g. BMW 6HP shift maps)
  declare that their data is stored transposed relative to the row-major
  (column-fastest) order OpenTune reads with. RomRaider's XDF converter honours
  this by setting `swapxy=true`; its `Table3D` population swaps the inner/outer
  loop, i.e. it is a genuine **storage-order** transform, not mere presentation.
  OpenTune has no per-map storage-orientation field and no round-trippable way
  to record the flag (`export_xdf` would drop it and silently re-corrupt), and a
  blind transpose of an asymmetric map is itself dangerous — so this crate
  **does not transpose**. It detects the bit, leaves `rows`/`cols` as declared,
  and marks the orientation unverified in `notes` + `warnings` so a human can
  confirm it against a known image before trusting cell positions.
- **Address rebasing is checked.** A `BASEOFFSET subtract` that would take an
  address below the base (which previously wrapped to a giant offset like
  `0xFFFFFF10`), or an add that overflows `u32`, is refused: the table/constant
  is skipped and a reason recorded in `warnings`, never emitted as a wrapped
  address. A rebasing that lands at exactly `0` is valid and kept.
- **Character encoding.** XDF payload text (titles, units) may be
  Windows-1251/Latin-1; pass the bytes in as a string (lossy UTF-8 is fine for
  structure). Non-ASCII units may come through as mojibake — that is a source
  encoding issue, not a parse error.
- **Scalar constants.** Modern-XML `<XDFCONSTANT>` entries are imported as
  [`XdfConstant`] scalars into `XdfImport.constants` (a distinct list, not folded
  into `maps`). Only plain numeric scalars are imported — a bitfield/switch is
  **not** promoted to an unrestricted scalar write.
- **Not imported:** `%%FLAG%%` (bit flags), `%%CONSTANT%%`/`%%CHECKSUM%%`
  blocks, `<XDFFLAG>`, DAC/DALINK data-link scaling, and the checksum region
  descriptors. Legacy `%%TABLE%%` files expose only tables (no constants list).

## CLI

Part of the workspace. The CLI exposes it as `opentune import-xdf <file>
[--into <ecudef.json>]` (lists the maps, or merges `XdfImport.maps` into the
target `EcuDef.maps`) and `opentune export-xdf` for the reverse direction.

## Tested on

- Synthetic inline XDF fixtures (XML and legacy) — the unit tests, so they pass
  without any external files.
- Two real community files from the tuning library:
  `Lanos DXUB3-14-3.xdf` (legacy, Daewoo) → 25 maps, and
  `94-95 LQ1 3.4 V6 … 16196401.xdf` (legacy, GM) → 94 maps, with addresses,
  dimensions and linear scaling recovered and the GM file's two `1638/X`
  reciprocal equations correctly flagged non-linear.
