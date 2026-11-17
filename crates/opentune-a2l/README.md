# opentune-a2l

Import **ASAM MCD-2 MC (A2L)** calibration descriptions into OpenTune ECU
definitions.

An `.a2l` file is the factory (or factory-derived) description of an ECU's
calibration data. Unlike a community heuristic map-finder, an A2L *names* every
`CHARACTERISTIC` (map / curve / scalar), gives its exact `ECU_ADDRESS`, its
dimensions, its `RECORD_LAYOUT` (element data type and byte order) and its
`COMPU_METHOD` (raw→physical conversion), plus the `AXIS_PTS` breakpoint tables.
This crate parses an A2L and emits a list of `opentune_core::ecudef::MapDef`
ready to merge into an OpenTune ECU definition — the precise, factory-grade
counterpart to `opentune-xdf`.

The parser is **hand-written and dependency-free**: A2L is a small keyword/block
text grammar (`/begin X … /end X`, `"strings"`, `/* */` and `//` comments) and
is tokenized directly. The only dependency is `opentune-core`.

## API

```rust
pub fn parse_a2l(src: &str) -> opentune_a2l::Result<A2lImport>;

pub struct A2lImport {
    pub project: Option<String>, // PROJECT identifier
    pub module:  Option<String>, // MODULE identifier
    pub maps:    Vec<opentune_core::ecudef::MapDef>,
}

pub enum Error { NoModule, Empty }
```

Each convertible `CHARACTERISTIC` becomes one `MapDef`:

| MapDef field | Source |
|---|---|
| `name` | `CHARACTERISTIC` name |
| `notes` | long identifier + every uncertainty/limitation flag (see below) |
| `address` | `ECU_ADDRESS` (verbatim, masked to 32-bit — see *Address spaces*) |
| `rows`, `cols` | kind + axis point counts (`MAP` = Y×X, `CURVE` = 1×X, `VALUE` = 1×1, `VAL_BLK` = `MATRIX_DIM`/`NUMBER`) |
| `data_type` | referenced `RECORD_LAYOUT` → `FNC_VALUES` type (`UBYTE/SBYTE/UWORD/SWORD/ULONG/SLONG/FLOAT32_IEEE`) |
| `endian` | `RECORD_LAYOUT BYTE_ORDER`, else `MOD_COMMON BYTE_ORDER` (`MSB_FIRST`=big / `MSB_LAST`=little) |
| `scaling` | linear `factor`/`offset` from the referenced `COMPU_METHOD` |
| `unit` | `COMPU_METHOD` unit |
| `x_axis`, `y_axis` | `AXIS_DESCR` resolved via `COM_AXIS`→`AXIS_PTS`, `STD_AXIS`, or `FIX_AXIS` |
| `min`, `max` | `LOWER_LIMIT` / `UPPER_LIMIT` |

The CHARACTERISTIC types handled are `VALUE`, `VAL_BLK`, `CURVE`, `MAP`,
`CUBOID`/`CUBE_4`/`CUBE_5` (sliced into one 2-D X×Y `MapDef` per position of
the higher dimensions, each slice named and noted). `ASCII` characteristics (strings) are skipped.

## Scaling: linear only, non-linear never guessed

OpenTune `Scaling` is strictly linear (`physical = raw * factor + offset`).
`COMPU_METHOD` conversions are interpreted as follows:

- **`IDENTITY`** → `factor = 1`, `offset = 0`.
- **`LINEAR`** (`COEFFS_LINEAR a b`) → `physical = a·raw + b`. Exact.
- **`RAT_FUNC`** (`COEFFS a b c d e f`): A2L defines the **physical→internal**
  rational function `internal = (a·p² + b·p + c) / (d·p² + e·p + f)`. When
  `a = d = e = 0` and `b ≠ 0` this inverts to the **exact** affine
  `physical = (f/b)·raw − c/b`, which is recovered. Any other coefficient set is
  genuinely non-linear (e.g. a reciprocal like `[0 0 3333.3 0 0.1 0]`) and is
  imported **pre-scaled** (`factor = 1`, `offset = 0`) with the coefficients
  recorded in `notes`.
- **`TAB_INTP` / `TAB_NOINTP` / `TAB_VERB` / `FORM`** → not linear; imported
  pre-scaled with the conversion kind recorded in `notes`. Never approximated.

The physical **unit** is kept even for a pre-scaled map (the unit is known; only
the non-linear curve is dropped).

## Axes

- **`COM_AXIS`** (shared breakpoints) is resolved through its `AXIS_PTS_REF` to
  the `AXIS_PTS` block, giving the axis address, point count, stored element type
  (from the axis `RECORD_LAYOUT`'s `AXIS_PTS_X`) and linear scaling. An
  unresolved reference is kept as `axis_ref` and flagged.
- **`STD_AXIS`** breakpoints are stored inline inside the map's own data block.
  The element type, count and scaling are captured, and the axis's **embedded
  address is computed** from the `RECORD_LAYOUT` positions when every component
  that precedes it is modelled. If the layout contains components whose size is
  not modelled (alignment bytes, …) the address is left unset and flagged in
  `notes` rather than guessed.
- **`FIX_AXIS`** inline values are computed from `FIX_AXIS_PAR_LIST`,
  `FIX_AXIS_PAR` (`offset + i·2^shift`) or `FIX_AXIS_PAR_DIST`
  (`offset + i·dist`).
- `RES_AXIS` / `CURVE_AXIS` keep the dimension but flag that breakpoints were
  not resolved.

## Address spaces (A2L ↔ bin)

An A2L `ECU_ADDRESS` is a **CPU / logical address** as seen by the ECU's
processor, not necessarily a byte offset into a flash dump. For example a TriCore
MSV80 A2L addresses calibration at `0xA080xxxx`, and a C16x EDC15 at `0x3xxxx`;
reaching the matching byte in a `.bin` typically requires **subtracting the flash
region's load/base address** (and sometimes selecting the right segment). This
crate copies `ECU_ADDRESS` through **verbatim** (masked to 32-bit) and never
invents an offset. When merging into an OpenTune `EcuDef`, apply the known base
offset for that ECU's flash region to every `MapDef.address` (and to each stored
axis `address`) — do not assume the A2L address is already a file offset. The
same caveat applies to `opentune-xdf`, except that XDF carries its base offset in
the file; A2L does not, so the base must come from the ECU's memory map.

## Limitations (be honest about these)

- **Non-linear conversions** are pre-scaled + flagged, not converted (see above).
- **STD_AXIS embedded address** is computed only when the record layout is
  fully modelled; otherwise it is left unset and flagged.
- **CUBOID / CUBE_4 / CUBE_5** are sliced into separate 2-D X×Y maps (`MapDef`
  is a 2-D model), so a large cube yields many maps.
- **64-bit types** (`A_UINT64`, `A_INT64`, `FLOAT64_IEEE`) have no native
  OpenTune `DataType`; they are approximated to 32-bit and flagged. (Rare in
  calibration maps.)
- **Bit masks / `BIT_MASK` / `BIT_OPERATION`** on a record layout are not
  modeled; the value is taken at its element type.
- **Address offset.** A2L addresses are CPU addresses (see above) — the base
  offset to a bin is **not** applied here.
- **Character encoding.** A2L payload text (descriptions, units) is frequently
  Windows-1252 / Latin-1. Pass the bytes in as a `&str` (a lossy UTF-8 decode is
  fine for structure); non-ASCII units may come through as mojibake. That is a
  source-encoding issue, not a parse error.
- **Not imported:** `MEASUREMENT`, `AXIS_PTS` as standalone maps, `FUNCTION` /
  `GROUP` hierarchy, `IF_DATA` (ETK/XCP) blobs, `MOD_PAR` memory segments,
  `COMPU_VTAB` enum text, and `CHARACTERISTIC` dependencies. Only calibration
  tables (maps/curves/values) are converted.

## CLI

Part of the workspace. The CLI exposes it as `opentune import-a2l <file>
[--into <ecudef.json>]` (with `--base` to rebase CPU addresses to bin offsets —
see *Address spaces*), `opentune a2l2xdf` (A2L → TunerPro XDF with a
conversion-loss report) and `opentune export-a2l`.

## Tested on

- Synthetic inline A2L fixtures — the unit/integration tests, so they pass with
  no external files: a `MAP` with a `RAT_FUNC` conversion and a `COM_AXIS`, a
  `CURVE`, a scalar `VALUE`, `MSB_FIRST` vs `MSB_LAST`, a `TAB_INTP` that must be
  flagged (not guessed), `VAL_BLK MATRIX_DIM`, an unresolved `COM_AXIS`, and a
  comment/whitespace torture case.
- Three real library A2Ls (not committed):
  - Siemens **PPD1.2** (Skoda Octavia TDI): 1995 maps, 1503 multi-dimensional,
    all `COM_AXIS` breakpoint tables resolved to real addresses and element
    types.
  - Bosch **EDC15** (`A0810172`): 2974 maps; 5 genuinely non-linear `RAT_FUNC`
    reciprocal conversions correctly flagged and pre-scaled, the rest linear
    (`0.01`, `0.0625`, …) with units and `MSB_LAST`/little-endian recovered.
  - Bosch/Siemens **MSV80** (BMW, 24 MB): 10715 maps parsed, 550 non-linear
    flagged, TriCore `0xA080xxxx` CPU addresses carried through verbatim (a clear
    illustration of the base-offset caveat).
