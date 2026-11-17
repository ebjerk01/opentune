# Checksum families

A control unit will not run a flash image whose **integrity fields** don't match
its contents. Edit a map and those fields go stale, so the ECU rejects the image
until they are recomputed. The **Checksum** panel verifies and corrects them.

This edition ships four public algorithm families:

## `edc16` — Bosch EDC16

The common EDC16 variants on the Freescale/Motorola **MPC56x** (PowerPC,
big-endian) core protect the flash with a **block directory** plus a 32-bit
big-endian **additive checksum to a fixed constant**. When a full dump carries
the directory, OpenTune walks each block and checks that the sum of its
big-endian 32-bit words equals the Bosch constant; the last dword of each block
is that block's compensation word, rewritten innermost-block-first on a fix. On a
calibration-only dump (no directory) it surfaces the known calibration field.

## `edc17` — Bosch EDC17

EDC17 uses the Bosch/VAG **forward CRC-32** (polynomial `0x04C11DB7`) over the
checksum areas. OpenTune reuses a single, reference-tested forward CRC engine so
the EDC17 block tables and other Bosch families share one validated
implementation.

## `pcr21` — Siemens/Continental Simos PCR2.1

PCR2.1 protects its calibration with a CRC over the cal area. OpenTune corrects
that cal CRC and **never touches the RSA signature block** — signed regions are
out of scope.

## `crc32-tail` — generic

A plain CRC-32 (IEEE 802.3, reflected) stored at the tail of the image. A simple,
well-defined scheme useful as a reference and for custom layouts.

## Using the panel

1. Pick the family that matches the ECU (Identify shows a definition's `checksum` id).
2. **Verify** — read the block table; green **OK**, red **BAD**.
3. **Fix & save** — recompute and write the correct values.
4. **Verify again** — confirm every block is OK before the image goes near a car.

> If verify reports *no protected blocks*, the chosen algorithm did not recognise
> a layout in this image — check you picked the right family for the ECU.
