# EDC15C6 on the bench (Mercedes CDI)

How to wire a Bosch EDC15C6 engine ECU (Mercedes CDI, CR1 / CR2) on the desk so
you can talk to it over K-line. The diagram below is drawn from the ECU database
entry `bosch-edc15c6-mercedes`, so it always matches the database.

> **Two pins are disputed between the public sources.** The ground pin (S1-5 or
> S1-6) and the second terminal-15 pin (S1-7 or S1-8). Find them on **your** unit
> with a multimeter before you apply power. Everything else below agrees between
> two independent sources.

## What you need

- A **current-limited** bench supply, 12–13.8 V, with a **fuse** in the +12 V lead.
  Start with a low current limit.
- A **K-line interface** (for example a KKL 409.1 cable, driver `kkl`).
- A multimeter, and the three EDC15C6 connector blocks (S1, S2, S3) or a breakout.

## Wiring diagram

```pinout bosch-edc15c6-mercedes
```

## Step by step

1. **Find ground first.** With the ECU unpowered, measure continuity from S1-5
   and S1-6 to the housing (and to each other). Use the pin that shows ground on
   your unit. The sources disagree: Transpondery says S1-5, ecupinouts says S1-6.
2. **Connect ground** to the supply negative.
3. **Permanent supply:** S2-13 (terminal 30) to +12 V through the fuse. Both
   sources agree.
4. **Ignition:** S1-1 (terminal 15) to the same +12 V. Both sources agree. A
   second terminal-15 pin is disputed (S1-7 per Transpondery, S1-8 per
   ecupinouts); you normally do not need it on the bench. Leave it unconnected
   unless the unit will not wake.
5. **K-line:** S3-28 to the K-line of your interface. Both sources agree.
6. Power up with the current limit low, then raise it once the current settles.
7. In OpenTune, **Identify** over K-line (KWP2000, ISO 14230) to confirm the ECU
   answers before doing anything else.

CAN (S2-11 / S2-12) comes from a single source and is not needed for K-line work.

## What this gets you — and what it does not

- **Diagnostics and identification** over K-line.
- **A full flash read** over K-line needs the ECU's programming-mode security
  access. OpenTune ships no OEM seed/key; you supply your own (the off-by-default
  seed/key loader, `docs/SEEDKEY-LOADER.md`) or use a boot-mode read.
- **Boot mode** is not on the connector: it needs the case opened and a
  board-level boot select. The ECU database entry's **Access** card lists
  what is publicly known (C167 bootstrap loader; a pull-down on the flash's
  DQ4 pin, *unverified* for this unit until you confirm by continuity that it
  reaches the CPU's P0L.4).
- Once you have a read, the **Checksum** panel verifies and corrects the
  EDC15C6 checksum.

## Safety

- Check **every** pin with a multimeter on your unit, whatever the diagram says.
  Hardware revisions can differ from the sources.
- **+12 V on a signal pin can destroy the ECU.** Keep the supply current-limited.
- "Verified" in the database means independent sources agree, not that a unit
  was measured.

Sources: Transpondery (Mercedes EDC15C5 / EDC15C6 pinout) and ecupinouts.com
(Bosch EDC15C6 Mercedes bench pinout); see the ECU database entry.
