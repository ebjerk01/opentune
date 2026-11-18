# Calibration map library

This document records the real calibration maps OpenTune ships, how each address
was sourced, how confident we are in it, and how to add maps for other ECUs.
Sections:

- [Bosch EDC16U34](#bosch-edc16u34-vag-19-tdi-pd) — VAG 1.9 TDI PD (`ecudefs/bosch-edc16u34.json`)
- [Bosch EDC15C6](#bosch-edc15c6-mercedes-cdi) — Mercedes CDI (`ecudefs/bosch-edc15c6.json`)
- [Simos PCR2.1](#continentalsiemens-simos-pcr21-vag-16-tdi-cr) — VAG 1.6 TDI CR (`ecudefs/siemens-pcr21.json`)
- [Bosch EDC17CP14](#bosch-edc17cp14-vag-20-tdi-cr--no-maps-shipped) — no maps shipped (why)
- [VAG DSG TCUs (DQ250 02E)](#vag-dsg-tcus-dq250-02e) — shift schedules (`ecudefs/temic-dq250-02e.json`, `temic-dq250-02e-tricore.json`); family overview in [`DSG.md`](DSG.md)
- [Reproducing / validating](#reproducing--validating), [Adding maps for another ECU](#adding-maps-for-another-ecu)

All facts here come from **public** material and from reference stock/tuned ECU
reads — no proprietary A2L/DAMOS content, no commercial map packs, no VINs. The
dumps themselves are private and never committed.

## Confidence levels

Every map's `notes` starts with a `CONFIDENCE:` tag and its source:

| Tag | Meaning |
|---|---|
| **HIGH** | Diff-confirmed on a stock/tune pair **of the target software** and cross-checked against an in-image header / known LSB. |
| **MEDIUM-HIGH** | Diff-confirmed on a **sibling** software of the same project/family (identical header dimensions, input ids and axis breakpoints), then located in the target software by that structure. |
| **MEDIUM** | Structure only (in-image header, sane axes and range), or diff-confirmed location with an *inferred* role / unconfirmed scaling. |
| LOW | Not shipped. |

Where only the location is certain, the notes say so separately ("HIGH on
location/shape, MEDIUM on role"). Where the physical LSB is not confirmed, the
cells are shipped **raw** (`factor` 1, unit `raw (...)`) rather than with a
guessed scale — edit those by ratio, not by absolute value.

The common method for every ECU:

1. `opentune findmaps stock.bin tuned.bin` (or a byte diff) lists the regions a
   tune changed.
2. The table containing each region is identified from the in-image structure
   (header with dimensions and axis tables, or the row stride of the data).
3. Axes are decoded and checked for strict monotonicity and a plausible physical
   range; cell scaling uses the family's known LSBs where they exist.
4. Roles are assigned from axis quantities + data shape, and labelled as
   inferred where they are.
5. The fixture-gated test `crates/opentune-core/tests/fleet_maps_fixtures.rs`
   asserts every changed region of each reference tune falls inside a defined map
   and that every map decodes on the stock read without errors.

---

## Bosch EDC16U34 (VAG 1.9 TDI PD)

Target: software `1037382088` (project P447, `03G906021…`), read as a **2 MiB**
full-flash image; map addresses are byte offsets into that read. Inputs:

| Read | Role |
|---|---|
| `stock.bin` / `tuned_uncorrected.bin` | target stock + a fuel/torque-limit tune (checksums not re-fixed) |
| SW `1037381814` and `1037389289` (also P447) ori/mod pairs | sibling software: full stage-1 tunes (driver's wish, conversion, boost, duration) |

Layout: every table carries an inline header, big-endian u16:

```
[rows:u16][cols:u16][Y-axis: rows × u16][X-axis: cols × u16][data: rows*cols]
```

The target tune changed exactly two regions (`0x1D172F..0x1D17C8`,
`0x1D30BD..0x1D325C`); both lie inside shipped maps. The sibling tunes changed
tables whose header dimensions and axis breakpoints are identical to tables in
the target read, which is how those were located.

### Torque-based structure and units

EDC16 PD runs a torque structure: driver's wish → torque limiter → torque-to-IQ
conversion → smoke limiter. The units follow from the reads themselves:

- the conversion map's torque axis spans 0..336 Nm at 0.1 Nm/LSB and its cells
  0..72.4 mg/stroke at 0.01 mg/LSB;
- read at 0.1 Nm the torque limiter peaks at 316 Nm, which converts to ~56 mg at
  3000 rpm — just under the 59 mg smoke limit; read at 0.01 mg it would cap fuel
  at 31.6 mg, far below the smoke limit;
- one driver's-wish member holds −2.0 Nm drag-torque cells, impossible for a
  fuel quantity.

So driver's wish and torque limiter are **Nm (0.1 Nm/LSB, signed)**; the smoke
limiter and conversion output are **mg/stroke (0.01)**.

### Maps

| Map | Data addr | Shape | Axes (Y × X) | Unit | Confidence |
|---|---|---|---|---|---|
| Smoke limiter (IQ vs air mass) | `0x1D30BC` | 16×13 | rpm × air mass | mg/stroke | **HIGH** |
| Torque limiter | `0x1D172A` | 4×20 | atm. pressure × rpm | Nm | **HIGH** (unit MEDIUM-HIGH) |
| Driver's wish 1..7 | `0x1C23A4` + 0x156·k | 16×8 | rpm × pedal % | Nm | MEDIUM-HIGH |
| Driver's wish 8 (rpm to 6000) | `0x1C2CFE` | 16×8 | rpm × pedal % | Nm | MEDIUM-HIGH |
| Torque → IQ conversion | `0x1D3FE2` | 15×16 | rpm × torque | mg/stroke | MEDIUM-HIGH |
| Boost pressure target 1, 2 | `0x1E4744`, `0x1E48BC` | 16×10 | rpm × IQ | hPa | MEDIUM-HIGH |
| Boost pressure limiter | `0x1E4A2A` | 11×10 | rpm × atm. pressure | hPa | MEDIUM-HIGH |
| Injection duration 1..4 | `0x1DF1AA` … `0x1DF930` | 19×15 | rpm × IQ | raw (crank angle) | MEDIUM-HIGH (scale LOW) |

Corrections to earlier releases:

- **Torque limiter** was shipped as "injection quantity — driver demand" in
  mg/stroke at `0x1D172E`, 4 bytes late. The header puts data at `0x1D172A`; only
  there do the `0 / 550 / 551` rpm breakpoints and the 0 at 5100 rpm line up (the
  old offset produced the "fuel cut at the top three breakpoints" artefact). The
  Y axis `600/700/890/930` is atmospheric pressure in hPa.
- The 16×8 family at `0x1C2370…` was shipped as an "IQ vs air mass" limiter. Its
  X axis (`100…10000`) is accelerator pedal at 0.01 %, and the sibling tunes
  raised the full-pedal columns of every member: these are the **driver's wish**
  maps.
- The duration cells are signed and grow ~8× from 0.5 to 60 mg at every rpm
  (negative only at minimal quantity / high rpm), the shape of a
  quantity-to-duration map rather than start of injection. Bosch commonly uses
  3/128° per LSB, but that is not confirmed for this software, so they ship raw.

Not shipped: **start of injection** (several 10×10 / 12×10 candidates near
`0x1DFB6A`, untouched by every available tune), the 15×13 rpm×IQ air-mass
setpoint the sibling tunes changed (EGR-related, out of scope), and **rail
pressure** — the PD engine has unit injectors, no common rail.

---

## Bosch EDC15C6 (Mercedes CDI)

Target: software `1037351686` (`0281010826`), read as a **1 MiB** image. Inputs:
the target's stock/tuned pair plus three sibling MB EDC15C6 pairs
(`1037351637`, `1037351736`, `1037351737`).

Layout (C167, little-endian u16): each table is preceded by one descriptor per
axis, `[input-id:u16][count:u16][breakpoints…]`, Y first, then X, then the
row-major data. The id word names the input quantity and is consistent across
software (`0xC694` engine speed, `0xC646` boost pressure, `0xCA00` rail pressure,
`0xC8CA` pedal, `0xD084`/`0xD092`/`0xD094`/`0xD1F2` injection-quantity inputs).
`1037351686` and `1037351637` share the same layout at identical addresses.

The read holds the calibration **four times** (at +0, +0xC000, +0x18000 and
+0xE0000); the tune changed every copy identically. Maps point at the first copy;
apply the same edit to all four. The 4-byte words ending each copy
(`0x1BFFC…`) are checksums, not maps.

| Map | Data addr | Shape | Axes (Y × X) | Unit | Confidence |
|---|---|---|---|---|---|
| Smoke limiter (IQ vs boost) | `0x1645A` | 16×16 | rpm × boost hPa | mg/stroke | **HIGH** |
| Boost pressure target | `0x155B8` | 12×12 | rpm × IQ | hPa | MEDIUM-HIGH (sibling `351637` tune, same address) |
| Driver's wish | `0x16FB0` | 8×8 | rpm × pedal % | mg/stroke | MEDIUM (structure only) |
| Start of injection, main | `0x121FE` | 12×12 | rpm × IQ | raw signed | HIGH location, MEDIUM role |
| Injector energizing time 1..6 | `0x12978`, `0x12C48`, `0x1A2D0`, `0x1A5A0`, `0x1A870`, `0x1AB40` | 16×20 | rail pressure (bar) × IQ | raw (likely µs) | HIGH location, MEDIUM role |

Not shipped: rail-pressure target, torque limiter and rpm limiter — no available
tune touched them and the candidate tables cannot be named from structure alone.
The `checksum` field of this definition is owned by separate checksum work.

---

## Continental/Siemens Simos PCR2.1 (VAG 1.6 TDI CR)

Target: software `CASM2K30` (`03L906023A`), read as **2 MiB** (populated area
`0x180000..0x1FFFFF`). Input: one stock/tuned pair of that software. The tune
changed 47 regions, all inside the 47 shipped tables.

Layout: Simos stores the table data **without** an inline header; axes live
separately as count-prefixed breakpoint arrays (around `0x1A2000..0x1A6FFF`) and
the table-to-axis binding is in code. Dimensions were derived from the row stride
of the data and the 0x280-byte spacing of same-shaped tables. Consequently:

- **no axes** are shipped;
- cells are **raw little-endian u16** (no scaling is confirmed);
- roles are **inferred** from shape and stated as such in every name.

| Group | First data addr | Count × shape | Inferred role | Confidence |
|---|---|---|---|---|
| A | `0x1BF318` (+0x280·k) | 12 × 20×16 | boost pressure target (floor = ambient, ~2.4× peak) | MEDIUM |
| B | `0x1C1898` (+0x280·k) | 6 × 20×16 | boost pressure, second family (limit-like plateaus) | MEDIUM |
| C | `0x1CCC5C` (+0x360·k) | 2 × 23×18 | injector energizing time (falls with pressure, rises with quantity) | MEDIUM |
| D | `0x1D6398` (+0x280·k) | 27 × 20×16 | driver's wish (0 at column 0, rising per row; one per gear/mode) | MEDIUM |

Other PCR2.1 software uses **different offsets**: software `CASM2L95`
(`03L906023S`/`…MK`) keeps several of these tables byte-identical (e.g. group C
at `0x1CD254`, D8/D9 at `0x1D8030`/`0x1D82B0`) but most move. Locate tables by
content in your own read before any write. Rail pressure and start of injection
were not touched by the available tune and are not shipped.

---

## Bosch EDC17CP14 (VAG 2.0 TDI CR) — no maps shipped

The only EDC17CP14 reference reads available are Hyundai/Kia software (ix35,
Santa Fe, Sorento, Sportage). No VAG CP14 read — stock or tuned — was available,
and EDC17 map addresses are software-specific, so even structure-only maps
cannot be located for VAG software. Rather than ship addresses from a different
manufacturer's calibration in a family definition, `bosch-edc17cp14.json` still
ships no maps. Adding them needs a VAG CP14 stock read (ideally with a
tune of it) — then follow the method below.

---

## VAG DSG TCUs (DQ250 02E)

No stock/tuned pair of any DSG software was available, so no map here is
diff-confirmed against a tune and none is HIGH. Inputs: nine real 02E reads —
seven PowerPC-generation 0x68000-byte reads (five software versions) and two
TriCore-generation `02E300013N` reads (two datasets of one software). See
[`DSG.md`](DSG.md) for layouts and the checksum.

### Shift schedules

Both generations store their shift-speed schedules the same way, without an
inline header: per group, five consecutive **upshift** blocks followed by five
**downshift** blocks, each block **4 rows × 12 columns of u16** (0x60 bytes).
Every row rises monotonically from a flat low plateau to a flat high plateau,
and the values sit in the engine-speed band of the drivetrain the calibration
was built for. Evidence for the role, per definition:

- **PowerPC (`temic-dq250-02e`, big-endian)**: two datasets of the same code
  build — an A3 2.0 TDI and a TT 2.0 TFSI calibration — differ inside every
  shipped block, and nowhere in the code sectors; the TDI data tops out at
  4400, the TFSI data at 6220–6500, as diesel and petrol upshift speeds would.
  The first downshift block starts near standstill (70–450).
- **TriCore (`temic-dq250-02e-tricore`, little-endian)**: the same block shape
  and plateau pattern (upshifts to 6600–6700, downshifts from 150), located by
  structure only; the two available datasets do not differ inside these blocks.

What is inferred and therefore labelled so: that block *k* is the *k→k+1*
upshift (or *k+1→k* downshift), which group serves which driving programme, and
the unit (engine speed). The axes — 12 columns, probably accelerator pedal, and
4 rows — are not located, so the cells ship **raw** (factor 1, no axes). Edit by
ratio and keep every downshift below its upshift.

| Definition | Group | Upshift 1-2..5-6 | Downshift 2-1..6-5 | Confidence |
|---|---|---|---|---|
| `temic-dq250-02e` | A | `0x605DE` + 0x60·k | `0x607BE` + 0x60·k | MEDIUM |
| `temic-dq250-02e` | B | `0x6370A` + 0x60·k | `0x638EA` + 0x60·k | MEDIUM |
| `temic-dq250-02e` | C | `0x6630A` + 0x60·k | `0x664EA` + 0x60·k | MEDIUM |
| `temic-dq250-02e-tricore` | 1 | `0x1D34` + 0x60·k | `0x1F14` + 0x60·k | MEDIUM |
| `temic-dq250-02e-tricore` | 2 | `0x20F4` + 0x60·k | `0x22D4` + 0x60·k | MEDIUM |

Addresses are software-specific. The `temic-dq250-02e` addresses are for the
code build shared by `02E300041N`, `02E300046K` and `02E300042M` (tags
`v0690801`/`v0690807`); `02E300042F`/`042G` (`v0690918`/`v0690920`) hold group A
**0x15A6 bytes later** (`0x61B84` / `0x61D64`, checked by a fixture test), and
the `02E300011BL` R32 software holds a group at `0x61CCC` / `0x61EAC`. Locate the
tables by content in your own read before any write.

### Not shipped

Torque limits per gear, clutch pressure / torque-transfer characteristics,
clutch adaptation and launch tables: candidate structures exist (e.g. families
of 16-point rising curves near `0x61B70..0x62470` in the PowerPC CAL), but no
read pair or open reference identifies them, so they are left out rather than
named by guess. A stock/tuned pair of one 02E software would be the next step
(`opentune findmaps stock.bin tuned.bin`).

---

## Reproducing / validating

```bash
opentune findmaps stock.bin tuned.bin --gap 32 --min-len 2
opentune compare --ecudefs ecudefs --def <id> stock.bin tuned.bin
opentune ecudefs validate ecudefs/<id>.json --image stock.bin --warnings
cargo test -p opentune-core --test ecudef_parse
OPENTUNE_FIXTURES=/path/to/fixtures \
  cargo test -p opentune-core --test fleet_maps_fixtures -- --ignored
OPENTUNE_FIXTURES=/path/to/fixtures \
  cargo test -p opentune-core --test dsg_fixtures -- --ignored
```

A map decodes "sensibly" when its stored axes are monotonic and its cells land in
the expected physical band (IQ in mg/stroke, boost in hPa, etc.).

## Adding maps for another ECU

1. **Find the structure.** `opentune findmaps <stock> <tuned>` locates the
   regions a known tune changed; a byte diff plus a header scan gives exact
   dimensions. Prefer a public TunerPro **XDF** or ASAM **A2L** when one exists
   *and its licence allows it* — it gives address, rows/cols, axis and scaling
   directly.
2. **Read the header.** Bosch EDC16/EDC17: `[rows][cols][Y][X][data]`,
   big-endian. Bosch EDC15 (C167): `[id][n][Y…][id][m][X…][data]`,
   little-endian. Simos PCR: no header — use row stride and table spacing.
3. **Pick the scaling.** Use the family's known LSBs (Bosch IQ `0.01 mg/stroke`,
   air mass `0.1 mg/stroke`, rpm `1.0`, pedal `0.01 %`, torque `0.1 Nm` on
   torque-based EDC16). Unknown LSB → ship raw and say so.
4. **Add the entry** to `maps` in `ecudefs/<ecu>.json` (and the Android copy in
   `android/app/src/main/assets/ecudefs/`, which a drift guard keeps identical):
   `address`, `data_type` + `endian`, `rows` × `cols`, `scaling` + `unit`,
   axes, and `notes` with the **confidence** and **source**.
5. **Validate** with the commands above and add the tune pair to
   `fleet_maps_fixtures.rs` if you have one.

See `docs/MAP-DEFS.md` for the full `MapDef` schema and its relationship to A2L,
XDF and DBC.
