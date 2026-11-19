# ECU database

A searchable reference of engine ECUs, gearbox controllers (TCUs) and AWD
controllers: what is inside them, how they can be read, which checksum family
they use, which vehicles they are fitted to, and bench pinouts where known.

## What you need

Nothing. The database is bundled with the app and works offline.

## Step by step

1. Type in the search box: a part number, ECU family (for example `EDC17`), CPU, or a vehicle.
2. Pick an entry to see its details.

## Reading the results

- **CPU** and **flash size** — the processor family and how much flash to expect in a full read.
- **How to read it** — **OBD** (through the diagnostic port), **bench** (ECU on the desk, wired to its connector), or **boot** (boot mode, usually with the case opened). If only bench or boot is listed, an OBD read is not expected to work.
- **Checksum family** — the algorithm to choose in the **Checksum** panel, when this edition supports it.
- **Vehicles** — cars the unit is known to be fitted to. One part number can be used in several cars, and one car can use several part numbers.
- **Bench pinout** — which connector pins carry power, ground, CAN or K-line, and boot pins, as a table and as a **wiring diagram** (pins on the left, wired to the supply and interface on the right). Each pinout has a **confidence level**: *verified* means two or more independent sources agree (not that a unit was measured), *community* means one source, and disputed pins are drawn dashed with a **?** — confirm them by continuity before wiring. Low confidence means the data comes from a single or unverified source.
- **Access** — how the unit is reached, part by part, each with its own confidence: **OBD** (what works through the diagnostic socket), **bench** (on the desk through its own connector, case closed), **boot mode** (the processor's boot loader: where to strap it, and **open case** when the points are on the board), and **security** (whether a full read or a write needs security access — OpenTune ships no OEM key). Boot points say where they are (a connector pin or a board pad) and are often *unverified*: confirm them by continuity first.
- Worked example: [EDC15C6 on the bench](#/guide/44-edc15c6-bench).
- Adding or fixing a unit, and which sources are allowed: `docs/UNIT-ACCESS-PROFILE.md` in the OpenTune repository.

## Common errors

- **No result** — try a shorter search (just the family or the last digits of a part number). The database does not cover every ECU.

## Safety

- **Verify every pinout with a multimeter before connecting power**, whatever its confidence level. Check ground, supply and communication pins against the actual board.
- Power a bench ECU from a **current-limited** bench supply. Start with a low current limit. A wrong pin with an unlimited supply can destroy the ECU.
- Boot-mode access usually means opening the case. Take ESD precautions.
