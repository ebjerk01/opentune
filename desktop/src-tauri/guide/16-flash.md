# Flash

Demonstrates the safety-gated write sequence: **backup → write → read-back
verify**. **File target only in this build:** writing to a real ECU is not
validated, so the write always goes through the **mock / file driver** into an
image **file**, not into an ECU. There is no driver choice.

## What you need

- A **target image** file (the opened image, or pick one).
- A **data file** holding exactly the bytes to write.
- No adapter and no car.

## Step by step

1. **Target image…** — the file to write into. If you pick none, the opened image is used.
2. **Data file…** — the bytes to write. Its length sets the region length.
3. **Address** — where to write, as a hex offset in the target (for a plain dump this is the file offset).
4. Press **Write to file**.

A file has no battery, so the voltage gate is skipped. On a real ECU it is the
first check.

## Reading the results

- **region** — address and length written.
- **bytes written** — should equal the data file size.
- **voltage gate** — skipped for a file target.
- **backup** — the path of the copy of the whole target file as it was before the write, in the `opentune-vault` folder beside it.
- **target** — the file that was written.
- **verified** — `true` when the read-back matched byte for byte.

## Common errors

- **Write past end of image** — address + data length is beyond the end of the target file. Nothing was written.
- **The data file is empty** — pick a data file with the bytes to write.
- **Could not write the safety backup** — the `opentune-vault` folder beside the target can't be created or written. Nothing was written.

## Safety

- **The target file is changed on disk.** The whole file is archived to the vault first; to undo, copy that backup back.
- On a real ECU the same sequence applies, but a failed write can brick the module. Use a stable supply, never let the battery drop, and keep two backups in the **Vault**.
