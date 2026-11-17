# Flash

Demonstrates the safety-gated write sequence: **voltage gate → verified backup
→ write → read-back verify**. In this edition the write goes through the
**mock / file driver**, so it writes into an image **file**, not into an ECU.

## What you need

- A **target image** file (the opened image, or pick one).
- A **data file** holding exactly the bytes to write.
- No adapter and no car.

## Step by step

1. **Target image…** — the file to write into. If you pick none, the opened image is used.
2. **Data file…** — the bytes to write. Its length sets the region length.
3. **Address** — where to write, as a hex offset in the target (for a plain dump this is the file offset).
4. **min mV** — the voltage gate. The write is refused below this battery voltage. Default 12000.
5. Press **Run flash**.

The **Driver** list shows the drivers in this build, but the flash in this
edition always runs through the mock / file driver.

## Reading the results

- **region** — address and length written.
- **bytes written** — should equal the data file size.
- **battery** — the voltage the gate saw. The file driver always reports 12600 mV.
- **backup** — how many bytes were read back before writing.
- **verified** — `true` when the read-back matched byte for byte.

## Common errors

- **Voltage below minimum** — **min mV** is higher than the reported voltage. With the file driver, keep it at or below 12600.
- **Write past end of image** — address + data length is beyond the end of the target file.
- **Data length != region length** — the data file is empty or unreadable.

## Safety

- **The target file is changed on disk.** The backup taken here is kept in memory for the verify step; it is not saved as a file. Copy the target first.
- On a real ECU the same sequence applies, but a failed write can brick the module. Use a stable supply, never let the battery drop, and keep two backups in the **Vault**.
