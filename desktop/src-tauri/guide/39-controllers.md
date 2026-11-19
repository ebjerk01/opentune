# Controllers

Drivetrain **controller definitions**: gearbox control units (DSG / automatic
TCUs) and AWD couplings (Haldex and similar), written down as data. Each
definition says how to recognise the unit (part / software numbers, gearbox
codes), how to reach it for diagnostics, which CAN frames it sends or listens
to and what their signals mean, its live channels, adaptations, shift /
pressure tables, gear ratios, safety notes and where every fact comes from.
Format and rules: `docs/CONTROLLER-DEFS.md`.

The panel is offline: it reads definition files and CAN logs, and never talks
to a car.

## Finding a definition

- The list holds the **built-in** set (the app's `ctrldefs/`) and the
  `controllers/` of every installed **definition pack**, plus every pack you
  loaded from disk. A badge shows where each one comes from.
- Search by family (`DQ250`, `722.6`), maker, car, or a number printed on the
  unit (part number such as `0AY907554C`, gearbox code such as `02E`).
  **TCU** / **AWD** narrow the list.
- **From a definition pack**: choose a pack and a definition, **Load pack…** to
  add a pack from disk (its `pack.json`), or **File…** to open one loose
  definition file. Files a pack could not load are listed under the list with
  the reason.

## Reading a definition

- The header shows the identity keys, linked ECU definitions and database
  units, the checksum, the file it came from, and the **validation state**:
  `valid`, or each error / warning with its rule (`docs/CONTROLLER-DEFS.md` →
  Validation rules). Links are checked against the bundled ECU definitions,
  the unit database and the checksum list (and a pack's own ECU definitions).
- The evidence word (`implemented`, `synthetic-tested`, `real-image-tested`,
  `bench-tested`, `vehicle-tested`) is shown for the definition and, where it
  differs, per frame, channel and adaptation. Treat anything below
  `real-image-tested` as unconfirmed.
- **CAN set** cards list each frame (id, sender, cycle time); open a frame to
  see its signals: start bit and length, Intel or Motorola order, scaling,
  unit, named values and "not available" markers.
- **Tables** marked *structure only* have known axes and size but no storage
  address yet.
- **Safety** notes apply before anything is done to the unit for real.

## Decoding a CAN log

1. Capture the bus the unit sits on (for example with **CAN capture**). The
   decoder reads candump logs, OpenTune CSV, SavvyCAN CSV and Vector ASC.
2. Pick the definition, **Open log…**, choose the **frame set** when the
   definition has several (e.g. PQ and MQB), and **Decode**.
3. The result shows how many frames were in the log and how many matched,
   other ids seen, each live channel's range and last value, a per-frame
   summary of every signal, and the first decoded frames.

This is the same decoder as `opentune controller decode <id> <log>`.

## Related tools

- **DQ250 (02E)**: `opentune dsg identify`, `dtcs` and `groups` read the
  gearbox over KWP2000 on VW TP2.0 (read-only, needs a raw-CAN adapter, not yet
  tested on a car). See `docs/DSG.md`.
- **Haldex**: the **Haldex AWD** panel (Bench group, alpha builds) reads
  OpenHaldex and the Gen4 live and has a Haldex-specific log decoder with
  charts. The definition's page links to it when the panel is available.
