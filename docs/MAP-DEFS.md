# Calibration map definition format

A **calibration map** (or *table*) is a block of tunable numbers inside an ECU's
flash image: a fuel/injection quantity surface, a torque limiter curve, a boost
target, a single threshold. OpenTune describes each one with a `MapDef` so the
tool (and a human) can locate it, decode the raw bytes into physical values, and
present it for editing.

Map definitions live **inside an ECU definition** (`ecudefs/*.json`) under an
optional `maps` array, next to the memory map and checksum handler that already
describe that ECU. They are parsed into
`opentune_core::ecudef::MapDef` and are fully optional: a definition with no
`maps` key still loads (back-compatible with every existing file).

> Map addresses and axis layouts vary by **software variant**, not just by ECU
> family. The maps OpenTune ships are family-level **placeholders** and say so in
> `notes`; confirm every address against your own readout before you read or
> write. Use **public** facts only — no proprietary A2L/DAMOS content, no real
> VINs.

## What describes a map

| Concept | Field(s) | Notes |
|---|---|---|
| Identity | `name`, `notes` | `name` is stable and human-readable; `notes` flags per-variant confirmation. |
| Location | `address` | Byte offset of the first cell in the flash image. |
| Storage | `data_type`, `endian` | How one cell is encoded. |
| Shape | `rows`, `cols` | `1×1` scalar, `1×N` curve, `M×N` map. |
| Scaling | `scaling.factor`, `scaling.offset`, `unit` | `physical = raw * factor + offset`. |
| Axes | `x_axis`, `y_axis` | Breakpoints along each dimension (optional). |
| Bounds | `min`, `max` | Optional physical safety clamp / UI hint. |

### `data_type`

Serialized in `snake_case`. Endianness is a **separate** field because the same
type appears in either byte order on different targets.

| Value | Meaning | Bytes |
|---|---|---|
| `u8`  | unsigned 8-bit | 1 |
| `s8`  | signed 8-bit (two's complement) | 1 |
| `u16` | unsigned 16-bit | 2 |
| `s16` | signed 16-bit | 2 |
| `u32` | unsigned 32-bit | 4 |
| `s32` | signed 32-bit | 4 |
| `f32` | IEEE-754 single float | 4 |

### `endian`

| Value | Meaning |
|---|---|
| `big` | most-significant byte first (Motorola) — common on Bosch EDC/ME. |
| `little` | least-significant byte first (Intel). |

### scaling

`scaling` is a linear transform applied to each raw cell:

```
physical = raw * factor + offset
```

- `factor` — the resolution / LSB weight (default `1.0`).
- `offset` — added after the multiply (default `0.0`).
- `unit` — a sibling of `scaling` on the map (e.g. `"mg/stroke"`, `"Nm"`, `"deg"`).

This is deliberately the **DBC/cantools** model (factor + offset + unit). It
covers the large majority of OEM maps. Non-linear OEM conversions (A2L
`RAT_FUNC`, interpolated `COMPU_TAB`) are **out of scope for v1**: capture such a
map as pre-scaled values plus a `notes` entry rather than embedding an arbitrary
expression language, which would be a code-injection and portability hazard.

### axes (`x_axis`, `y_axis`)

An axis gives the breakpoint values along one dimension. `x_axis` runs along the
columns, `y_axis` along the rows. Each axis can be described **three ways**,
mirroring A2L's `AXIS_DESCR`:

| Style | Set | A2L analogue |
|---|---|---|
| **inline** | `values: [..]` | `FIX_AXIS` — breakpoints hard-coded in the definition. |
| **stored** | `address` + `data_type` + `endian` + `scaling` | `STD_AXIS` — axis read from the image like a 1-D table. |
| **shared** | `axis_ref: "<name>"` | `COM_AXIS` — reuse one breakpoint table across several maps. |

Common axis fields: `name`, `unit`, `count` (number of breakpoints; should match
the map's `rows`/`cols` on that dimension).

## Comparison to A2L, XDF and DBC

OpenTune's `MapDef` is a small, deliberately-flat subset distilled from three
established models:

- **ASAM A2L / ASAP2** (OEM calibration description; studied via `pyA2L`).
  A2L is the richest: a `CHARACTERISTIC` (VALUE / CURVE / MAP / VAL_BLK) points at
  an address, a `RECORD_LAYOUT` (`deposit`) describing byte layout, a
  `COMPU_METHOD` for conversion, and one `AXIS_DESCR` per dimension that is either
  standalone (`STD_AXIS`), shared (`COM_AXIS` → `AXIS_PTS`) or computed
  (`FIX_AXIS`). OpenTune keeps the **three-way axis model** and the
  address/type/scaling separation, but folds `RECORD_LAYOUT` into the flat
  `data_type`+`endian`+`rows`+`cols` fields and restricts `COMPU_METHOD` to a
  linear `factor`/`offset`. A2L's power (and its licensing weight — pyA2L is
  GPL-2.0) is why we learn from it rather than depend on it.

- **TunerPro XDF / RomRaider ECU-def XML** (community table definitions).
  RomRaider models a `table` with `type` (2D/3D), `storagetype`
  (`uint8/int8/uint16/int16/float`), `endian`, `sizex`/`sizey`, `address`, and
  per-table X/Y sub-tables, each with a `scaling` (`expression` / `to_byte`,
  `units`, `min`/`max`, `format`). This is almost exactly OpenTune's shape — we
  adopt its storage-type-plus-endian cell model and its inline-or-addressed axes.
  The one thing we drop is the free-form `expression` string (OpenTune uses
  `factor`/`offset` instead), for the safety reason noted above.

- **DBC / cantools signal scaling** (CAN signal decoding, MIT).
  A DBC signal is `raw * scale + offset`, bounded by `minimum`/`maximum`, labelled
  with a `unit`, sized in bits with a byte order and signedness. OpenTune's
  `scaling` block and `min`/`max` are taken straight from this — the simplest,
  most portable scaling model that still round-trips raw↔physical exactly.

**Why this subset.** OpenTune targets reflashing and calibration edits, not a
full OEM measurement/calibration (MC) stack. A flat, linear, JSON-native model is
easy to hand-author, safe to parse (no embedded expressions), and friendly to
importers that *lower* other formats into `MapDef`s — the A2L, XDF, EcuFlash /
RomRaider XML and WinOLS `.ols` importers all do exactly that. Anything the linear
model cannot express is captured as a pre-scaled map plus a note, not by growing
the schema.

## Worked example — injection-quantity (smoke) limiter

A 16×16 smoke limiter on a Bosch EDC16: injected quantity (mg/stroke) indexed by
engine speed (rpm) on X and charge-air pressure (hPa) on Y. Cells are big-endian
`u16` with a `0.01 mg/stroke` LSB; the X axis is stored in the image, the Y axis
is stored too. (Addresses are placeholders — see the note at the top.)

```json
{
  "name": "smoke limiter (injection quantity vs rpm/boost)",
  "notes": "EXAMPLE ONLY. Placeholder address/axes inside the calibration region; confirm per software variant before any read or write.",
  "address": 458752,
  "data_type": "u16",
  "endian": "big",
  "rows": 16,
  "cols": 16,
  "scaling": { "factor": 0.01, "offset": 0.0 },
  "unit": "mg/stroke",
  "x_axis": {
    "name": "engine speed",
    "unit": "rpm",
    "count": 16,
    "address": 458240,
    "data_type": "u16",
    "endian": "big",
    "scaling": { "factor": 1.0, "offset": 0.0 }
  },
  "y_axis": {
    "name": "charge air pressure",
    "unit": "hPa",
    "count": 16,
    "address": 458496,
    "data_type": "u16",
    "endian": "big",
    "scaling": { "factor": 1.0, "offset": 0.0 }
  },
  "min": 0.0,
  "max": 80.0
}
```

Reading a cell: take the big-endian `u16` at `address + (row*cols + col)*2`,
multiply by `0.01` → mg/stroke. A raw `5000` becomes `50.00 mg/stroke`. The block
occupies `rows * cols * data_type_width = 16 * 16 * 2 = 512` bytes
(`MapDef::byte_len()`).

A **scalar** is just `rows: 1, cols: 1` with no axes; a **1-D curve** is
`rows: 1, cols: N` with only an `x_axis`.

## Importing other definition formats

The importers lower community definitions into `MapDef`s with one rule:
**every object ends faithful, warned, or refused — never silently wrong.**
Each table / function / constant gets a report (`opentune_xdf::report`):

- **clean** — imported exactly;
- **warned** — imported, but with an explicit loss written into the report
  *and* the map's `notes` (`WARN[kind] …`): correct but incomplete, e.g.
  pre-scaled because the equation is non-linear;
- **refused** — not imported, with the reason, whenever the only possible
  import would place or decode cells wrongly.

Object classes OpenTune does not model as maps (bit flags, patches, checksum
blocks, switch/selection tables, separator/info entries) are refused as
*out of scope* and listed. `--strict` (`import-xdf`, `import-fastecu`)
refuses the whole file — and writes nothing — if any table, function or
constant is warned or refused; out-of-scope objects do not trip it. `-o
out.json` writes `{maps, constants, report}`.

### TunerPro XDF (`import-xdf`)

| Construct | Outcome |
|---|---|
| `<XDFTABLE>` z data: address, rows/cols, 8/16/32-bit int, 32-bit float, sign / LSB-first flags | faithful |
| `mmedtypeflags 0x04` (column-major) on a 2-D table | faithful: imported transposed (rows/cols and x/y axes exchanged) — identical bytes, noted |
| `<BASEOFFSET offset subtract>`, text `<baseoffset>-49152</baseoffset>` (signed) | faithful; under/overflow → refused |
| header `REGION` (whole-file size) | data past it → refused (`address-outside-image`) |
| `CATEGORY` / `CATEGORYMEM` | faithful (`category: …` in notes) |
| `<XDFFUNCTION>` | faithful: a 1×N curve, input vector as stored x axis |
| `<XDFCONSTANT>` | faithful scalar constant (separate list) |
| linear `<MATH>` in `X` (incl. `^` constant powers, hex) | faithful (`factor`/`offset`) |
| non-linear `<MATH>`; `VAR`s other than `X` (`type=address`/`link`), `ROW()`/`COL()` | warned, pre-scaled (`math-nonlinear`, `math-foreign-var`) |
| axis `embedinfo type=1` (embedded) | stored axis; address 0 / unusable storage → warned, count only |
| axis `embedinfo type=3 linkobjid` | resolved to the linked 1-D table's storage; unresolvable → warned |
| axis `LABEL`s | numeric → inline values; text → warned (`axis-categorical`); wrong count → warned |
| no `mmedtypeflags` | header `DEFAULTS` used; warned (`endian-defaulted`) only where that changes decoding |
| no `mmedelementsizebits` | header default → warned; none → refused |
| 24/64-bit cells, non-32-bit float | refused (`element-size-unsupported`) |
| unknown `mmedtypeflags` bits | warned |
| `mmedmajor/minorstridebits` that are not plain packing (interleaved, strided, negative) | refused (`stride-noncontiguous` / `stride-reversed`) |
| `<XDFFLAG>` (bit mask), `<XDFPATCH>`, `<XDFCHECKSUM>`, entries without any address | refused, out of scope |
| legacy text XDF | imported; every table warned (`legacy-cell-type`: the format has no storage type) |
| binary (pre-XML) XDF | file refused — re-save as XML in TunerPro |

### EcuFlash / RomRaider XML as FastECU reads it (`import-fastecu`)

[FastECU](https://github.com/miikasyvanen/FastECU) has no format of its own:
it reads EcuFlash and RomRaider XML (and converts WinOLS MapPack CSV).
`import-fastecu` applies FastECU's interpretation with the same discipline
(`import-ecuflash` remains the plain EcuFlash/RomRaider importer):

| Construct | Outcome |
|---|---|
| `<include>` / `<rom base=>` inheritance (same file, then `--base-dir` by xmlid) | faithful; child attributes override base per table and per axis (EcuFlash positional `X`/`Y` axis overrides matched); missing base → file warning |
| base tables the calibration never mentions (no address) | not part of the ROM; counted, not listed |
| `startpos` (hex) with `interval` 1 | faithful address shift |
| `interval` > 1 | refused (interleaved) |
| `Switch` / `Selectable` / `MultiSelectable` / `bloblist` | refused, out of scope |
| `uint24` / `int24`, unknown `storagetype` | refused |
| scaling reference not found (storage unknown) | refused (`reference-missing`) |
| float with `endian="little"` | big-endian (FastECU); clean on a big-endian `memmodel` (SH/HC16/M32R/PPC), else warned |
| `wrx02` address past the ROM (`filesize`, else FastECU's 160 KiB) | folded by `0x8000`, warned (`address-folded`); still outside → refused |
| static axes (`<data>`) | inline values; text → warned |
| non-linear `toexpr`/`expression` | warned, pre-scaled |
| MapPack CSV `DataOrg` `eByte`/`eHiLo`/`eLoHi`/`eHiLoHiLo`/`eLoHiLoHi`/`eFloatHiLo`/`eFloatLoHi` | faithful; unknown `DataOrg` → refused (FastECU would default to `uint8`); `Fieldvalues.Offset` honoured; no signedness column → warned |

`import-ecuflash` additionally warns when a float declares `endian="little"`
on a big-endian memory model (it keeps the declared order).

**Corpus (2026-11-18, definition files only, offline — no images):** 219
unique public community XDFs: 202 parsed (14 binary, 2 malformed XML, 1 empty
refused as files); 171 941 objects clean, 6 582 warned, 228 refused, 3 176 out
of scope. Top warnings: text axis labels (4 091), non-linear MATH (2 033),
label/count mismatches (514), MATH with foreign VARs (225); refusals are
interleaved strides (204), data past the declared region (22) and unparsable
addresses. Subaru defs through `import-fastecu`: 331 EcuFlash calibration
roms (with `--base-dir`) — 32 512 clean, 20 563 warned, 0 refused; 332
RomRaider `ecu_defs.xml` roms — 42 376 clean, 10 868 warned, 0 refused
(switch tables out of scope). Warnings are text axes, non-linear equations
and, for EcuFlash `wrx02` roms, the address fold. Re-run with the ignored
corpus tests (`OPENTUNE_XDF_CORPUS`, `OPENTUNE_FASTECU_CORPUS`).

## Finding candidate maps in one image

`opentune findmaps <image>` (single file) runs `opentune-mapfind`: it scans for
Bosch-style count headers (`[n1][n2][axis1][axis2][data]`, `[n][axis][data]`;
16-bit BE/LE and 8-bit), plus header-less adjacent axis pairs, and ranks each
hit by header consistency, axis plausibility and data-surface smoothness. Each
candidate carries its evidence and *hints* about what its raw axes look like
(`rpm-like?`, `temperature-like?`, …) — never a claimed meaning. `--emit
out.json` writes the top candidates as `MapDef`s named `candidate_0x…` with
raw scaling and an "UNVERIFIED" note; `--def <id>` marks candidates that match
a definition's maps and prints the recall. With two files (`findmaps stock
tuned`) the older diff-clustering mode runs instead.

## Adding a map

1. Add entries to the `maps` array of the relevant `ecudefs/*.json`.
2. Use **public** facts only; keep addresses as placeholders and say so in
   `notes`.
3. Keep scaling linear (`factor`/`offset`); capture anything non-linear as a
   pre-scaled map plus a note.
4. `cargo test -p opentune-core` must stay green — `tests/ecudef_parse.rs` loads
   every `ecudefs/*.json`, and the `mapdef_json_round_trip` unit test guards the
   `MapDef` schema.
