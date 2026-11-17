# EDC16U34 calibration map library

This document records the real calibration maps OpenTune ships for the **Bosch
EDC16U34** (VAG 1.9 TDI PD, spare-part family `03G906…`) in
`ecudefs/bosch-edc16u34.json`, how each address was sourced, how confident we are
in it, and how to add maps for other ECUs. It replaces the earlier *placeholder*
maps with entries that decode to sane physical values on a real stock image.

All facts here come from **public** material and from an owner's own ECU reads —
no proprietary A2L/DAMOS content and no VINs.

## Target and reference images

The EDC16U34 PD is read as a **2 MiB** full-flash image. Addresses in the map
definitions are **byte offsets into that 2 MiB read**, which is what the viewer
loads. The reference images used to establish and validate the maps are the
owner's own Golf 1.9 TDI PD reads:

| File | Role |
|---|---|
| `stock.bin` | factory calibration (validation target) |
| `tuned_uncorrected.bin` | Wiktor "squeeze" tune, checksums *not* re-fixed |
| `tuned_fixed.bin` | same edits, checksums corrected |

> Note on `flash_size`. The shipped `ecudefs/bosch-edc16u34.json` keeps its
> existing `flash_size`/`regions` fields unchanged; the map addresses above are
> offsets into the full **2 MiB** read that real EDC16U34 PD dumps (and the
> fixtures) use. If you align the memory map to a 2 MiB image later, the map
> addresses already match it.

## How the addresses were sourced

Two independent methods were combined; where they agree, confidence is high.

### 1. Diff-derived (stock vs tune)

`opentune findmaps stock.bin tuned_uncorrected.bin` (and a byte-level diff)
report that the Wiktor tune changed **exactly two** regions — a fuel-only PD
squeeze (driver demand + smoke limiter raised, boost/timing left stock):

```
map0  0x001D172F..0x001D17C8   153 bytes
map1  0x001D30BD..0x001D325C   415 bytes
```

Those changed byte ranges fall **inside** the two injection-quantity maps defined
below, so the viewer highlights the real tuned cells.

### 2. In-image map headers

VAG EDC16 stores each table with a small header immediately before its data:

```
[rows:u16][cols:u16][Y-axis: rows × u16][X-axis: cols × u16][ (pad) ][data: rows*cols × u16]
```

All values are **big-endian u16** (Motorola). Reading the header at the start of
each changed region gave the exact dimensions and the stored axis tables, which
were then decoded and checked for monotonic breakpoints and a plausible physical
range. The standard EDC16 injected-quantity LSB is **0.01 mg/stroke**; air-mass
axes decode with **0.1 mg/stroke**; engine speed is **1.0 rpm**.

No EDC16U34-specific TunerPro **XDF** or ASAM **A2L** was available in the
reference tuning library (which holds many EDC16U34 PD *binaries* but no matching
`.xdf`/`.a2l`/DAMOS), so the definitions are header- and diff-derived rather than
XDF-imported. This is called out in each map's
`notes`.

## The maps

| # | Map | Addr (hex) | Shape | Axes | Unit | Confidence | Source |
|---|---|---|---|---|---|---|---|
| 1 | Injection-quantity limiter (smoke / air-mass limiter) | `0x1D30BC` | 16×13 | rpm × air mass | mg/stroke | **HIGH** | diff + header |
| 2 | Injection quantity — driver demand / full-load | `0x1D172E` | 4×20 | (Y pressure/idx) × rpm | mg/stroke | **MED-HIGH** | diff + header |
| 3 | Injection quantity vs air mass (base limiter family) | `0x1C23A4` | 16×8 | rpm × air mass | mg/stroke | **MEDIUM** | header only |

Validated decode of `stock.bin` (both axes monotonic in every case):

- **Map 1** — data `13.9 … 59.1 mg/stroke`. Full-load smoke limit ≈ 59 mg,
  tapering at high rpm; rises with air mass. Y = rpm `861…5000`, X = air mass
  `300…1000 mg`. The tune raised these cells to release fuel.
- **Map 2** — data `0 … 31.6 mg/stroke`, a humped curve over rpm with fuel cut
  (0) at the top three rpm breakpoints — textbook fuel-demand shape. X = rpm
  `0…5100`. The 4-point Y axis `[600,700,890,930]` is stored but its unit is
  **unconfirmed** (likely ambient/charge pressure in mbar/hPa, or an altitude
  index). The tune raised every active cell ~5–11 %.
- **Map 3** — data `0 … 42.9 mg/stroke`. One of ~7 near-identical 16×8 maps at
  `0x1C2370…0x1C2A1E` that share the same rpm/air-mass axes (most likely slices
  of a 3-D limiter indexed by a third variable). Structure-identified, **not**
  diff-confirmed.

### Maps intentionally *not* shipped yet

- **Boost-pressure target / boost limiter, injection timing, torque limiter** —
  this particular tune did not touch them (fuel-only), so there is no diff to
  anchor them, and the calibration area holds dozens of same-shaped header
  structures whose output quantity cannot be named unambiguously without a DAMOS/
  A2L. Several clean candidates were found as leads (e.g. a 16×10 rpm-indexed map
  at header `0x1D2F06`, and rpm×load maps around `0x1C42D6`/`0x1C4BDA`) but are
  withheld rather than guessed. Add them once an XDF/A2L or a boost/timing-
  changing tune pins them down.
- **Rail-pressure target / limit — not applicable.** The 1.9 TDI PD uses
  *unit injectors* (Pumpe-Düse), not a common rail, so there is no rail-pressure
  map on this ECU. The field is listed in the task spec generically; for this
  engine it is intentionally omitted.

## Reproducing / validating

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo build -p opentune-cli
opentune findmaps stock.bin tuned_uncorrected.bin --gap 32 --min-len 2
cargo test -p opentune-core --test ecudef_parse   # JSON must match the schema
```

A map decodes "sensibly" when its stored axes are monotonic and its cells land in
the expected physical band (IQ in mg/stroke, boost in mbar/hPa, etc.). Each map
above was decoded from `stock.bin` and checked against those criteria before
being committed.

## Adding maps for another ECU

1. **Find the structure.** `opentune findmaps <stock> <tuned>` locates the
   regions a known tune changed; a byte diff plus a header scan gives exact
   dimensions. Prefer a public TunerPro **XDF** or ASAM **A2L** when one exists —
   it gives address, rows/cols, axis and scaling directly, and should be cited
   with higher confidence than a diff-derived guess.
2. **Read the header.** For Bosch EDC16/EDC17, expect
   `[rows][cols][Y-axis][X-axis][data]`, big-endian `u16`. Decode the axes and
   confirm they are monotonic.
3. **Pick the scaling.** Use the family's known LSBs (EDC16 IQ = `0.01
   mg/stroke`, air mass = `0.1 mg/stroke`, rpm = `1.0`). Keep scaling **linear**
   (`factor`/`offset`); capture anything non-linear as a pre-scaled map plus a
   note (see `docs/MAP-DEFS.md`).
4. **Add the entry** to the `maps` array of `ecudefs/<ecu>.json`: `address`,
   `data_type` + `endian`, `rows` × `cols`, `scaling` + `unit`, `x_axis`/`y_axis`
   (inline `values`, or `address` + `data_type` + `scaling`), and a `notes`
   field stating the **confidence** and **source** (XDF vs A2L vs diff-derived)
   and any per-variant caveat.
5. **Validate.** Decode the map from a real stock image, confirm monotonic axes
   and a plausible physical range, and keep `cargo test -p opentune-core
   --test ecudef_parse` green.

See `docs/MAP-DEFS.md` for the full `MapDef` schema and its relationship to A2L,
XDF and DBC.
