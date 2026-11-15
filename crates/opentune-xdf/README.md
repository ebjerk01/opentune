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
}
```

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
- **Character encoding.** XDF payload text (titles, units) may be
  Windows-1251/Latin-1; pass the bytes in as a string (lossy UTF-8 is fine for
  structure). Non-ASCII units may come through as mojibake — that is a source
  encoding issue, not a parse error.
- **Not imported:** `%%FLAG%%` (bit flags), `%%CONSTANT%%`/`%%CHECKSUM%%`
  blocks, `<XDFCONSTANT>`/`<XDFFLAG>`, DAC/DALINK data-link scaling, and the
  checksum region descriptors. Only tables (maps) are converted.

## Wiring in (maintainer)

This crate is standalone (note the empty `[workspace]` table in its
`Cargo.toml`, and that it uses explicit package metadata rather than workspace
inheritance). To wire it into the workspace:

1. Add `"crates/opentune-xdf"` to the root `Cargo.toml` `members`, and remove
   the `[workspace]` table from this crate's `Cargo.toml` (optionally switching
   `version`/`edition`/… back to `*.workspace = true`).
2. Add an `opentune import-xdf <file> --into <ecudef.json>` CLI command that
   calls `parse_xdf`, then merges `XdfImport.maps` into the target
   `EcuDef.maps` (de-duplicating by name/address as you see fit).

## Tested on

- Synthetic inline XDF fixtures (XML and legacy) — the unit tests, so they pass
  without any external files.
- Two real community files from the tuning library:
  `Lanos DXUB3-14-3.xdf` (legacy, Daewoo) → 25 maps, and
  `94-95 LQ1 3.4 V6 … 16196401.xdf` (legacy, GM) → 94 maps, with addresses,
  dimensions and linear scaling recovered and the GM file's two `1638/X`
  reciprocal equations correctly flagged non-linear.
