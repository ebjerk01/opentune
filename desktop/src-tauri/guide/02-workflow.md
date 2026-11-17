# The workflow

A calibration session in OpenTune follows the same shape whether you are offline
on a file or live on a car. The panels are laid out in that order.

## 1. Open the image

In **Image**, open a flash dump. Size and a paged hex view appear, and every
other panel then works on this file. Use the prev/next buttons or type an offset
(e.g. `0x070000`) to jump around.

## 2. Identify

In **Identify**, run *Identify opened image*. Offline, the mock driver reports
only a name and size, so use **Match a part / software number** to type a real
HW or SW number. OpenTune matches it against:

- the bundled **ecudef** definitions (the detailed, supported ECUs), and
- the **opendbc vehicle catalog** (a wide net of ~1000 vehicles) for candidate make / model / family even when there is no full definition yet.

## 3. Verify (and fix) checksums

An ECU refuses an image whose integrity fields are wrong. In **Checksum**, pick
the algorithm family and **Verify**: each protected block shows green **OK** or
red **BAD**. **Fix & save** recomputes and writes the correct values. If the file
is read-only, the app tells you and offers **Save As…** so you can write a copy
you own. Always re-verify after a fix.

## 4. Find and read maps

In **Maps**:

- **Find maps** diffs a stock file against a tuned file and lists the changed regions, with a stride hint (`~16x16 u16?`) to suggest table shapes — a fast way to locate what a tune touched.
- The **map viewer** decodes a *defined* calibration map (from an ecudef) out of the opened image into a blue→red heat grid, with the real X/Y axis breakpoints, units and scaling applied. It is also an editor: changed values are written back with a full backup first (see the **Maps** page).

## 5. Flash (safety-gated)

In **Flash**, pick a driver, a target image, a data file to write and an address.
The write always runs the safety sequence: **voltage gate → verified backup →
write → read-back verify**. Offline this runs through the mock driver so you can
see the full report without touching a car.

## 6. Keep backups

Every read should be archived. The **Vault** panel browses a backup folder where
each dump is a `.bin` plus a `.meta` sidecar carrying a CRC-32. Never work from a
single unsaved buffer — keep two backups before any write.
