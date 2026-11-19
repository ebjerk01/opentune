# Dumping a Mercedes 722.6 TCU (EGS51 / EGS52 / EGS53)

The 722.6 automatic's transmission controller (EGS) exists in three
generations and they use **different processors and memories**, so work out
which one you have before you read it. OpenTune never needs the car running — the
dump is taken from the module on the bench.

> **Scope.** This page covers reading *your own* module, for inspection and as a
> recovery backup. OpenTune ships no OEM keys or protected calibrations.

## The three generations

| Unit | MCU | Flash | Serial EEPROM |
|------|-----|-------|---------------|
| **EGS51** | `SEC51C810` (8051 family) | small parallel flash (reported: `29F010` / `M27C1001`, ~128 KB — unconfirmed) | `93C56` (256 B, reported — unconfirmed) |
| **EGS52** | `SAK-C167` (Infineon C166 / C167CS) | `AM29F400BB` external, **512 KiB** | `95320` SPI (4 KiB) |
| **EGS53** | `ST10F276` | **internal** (~768 KiB) | `95640` SPI (8 KiB) |

EGS51's flash/EEPROM parts are community reports, not yet confirmed by OpenTune,
and may vary by board revision — check the chip markings on your board. The EGS52/53 figures come from public flasher references
(VF2/DFB) and rnd-ash's C167CS work. A full dump is the **flash *and* the serial
EEPROM**, kept as two files — the EEPROM holds the SCN/variant coding and adaptations
and must not be swapped between modules.

## 1. Identify the generation

- Read the TCU part number from the label and the board.
- **EGS51** has the 8051-class `SEC51C810` and a small parallel flash.
- **EGS52** has a C167 and a 44-pin `AM29F400BB`.
- **EGS53** has an `ST10F276` with internal flash (no external parallel flash chip).

## 2. Read it — by generation

**EGS51 (`SEC51C810`).** Chip-read both memories:
- `93C56` EEPROM: SOIC-8 clip on a CH341A (3.3 V) → save `egs51_eeprom.bin` (256 B).
- Parallel flash: usually desolder, read in a universal programmer (TL866II+/T56) as
  the exact device type → save `egs51_flash.bin`.

**EGS52 (`SAK-C167`).**
- **C167 bootstrap loader (no desolder):** enter the BSL on ASC0 by pulling `P0L.4`
  low at reset (the "one wire" trick) and read the `AM29F400BB` (512 KiB) with a C167
  boot tool. A correct CS/BUSCON preset for the `AM29F400BB` is required.
  OpenTune's `opentune bsl plan --preset egs52` shows the plan; its board bus registers
  are not publicly known yet, so `opentune bsl read` refuses until you supply them
  (see the C167 bootstrap guide).
- **Chip-off:** `AM29F400BB` on a TL866/T48; the `95320` SPI EEPROM via SPI clip or
  desolder + CH341A (25-series SPI mode).
- **Bench K-line** tools (FoxFlash/DFB) read EGS52/53 over K-line: **+12 V pin 29,
  GND pin 30, K-line pin 1**.

**EGS53 (`ST10F276`).** ST10 BSL for the internal flash; `95640` EEPROM separately.
A later target.

## 3. Verify the dump

- Read the flash **twice** and compare; the two reads must be byte-identical.
- The file size must match the device exactly — a short or half-size file means a
  wrong device type or a bad read; re-read.
- Keep each serial EEPROM with its own module; it carries the coding and adaptation.

## 4. What OpenTune does with the dump — and what it does not

- **Inspect / analyse:** open the flash in the image tools to look at the layout and
  the calibration region.
- **No checksum support.** OpenTune has **no EGS51/52/53 checksum** and will **not**
  correct an EGS checksum after an edit. No public algorithm, block layout or location
  exists; deriving one is clean-room research that first needs stock reads from owned
  units (ideally ≥2 images per generation). Until then, treat a dump as read-only
  reference, not something OpenTune can re-sign.
- **Coding record (EGS52):** `opentune coding egs52-crc` checks the CRC-16 on the
  42-byte variant-coding record. The CRC variant is **unconfirmed** on a real unit, so
  recomputing it needs `--allow-unconfirmed`.

Always keep the untouched original of **both** chips as your recovery copy before
making any change.
