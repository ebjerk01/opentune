# Checksum

Verify and correct the integrity fields an ECU checks before it will run an
image. After any map edit these fields are stale and must be recomputed. See
**Checksums** (general page) for how each family works.

## What you need

- A dump opened in **Image**.
- The right algorithm family for the ECU. **Identify** shows it on a matched definition; **Analyze bin** picks it from the detected family.
- No adapter and no car.

## Step by step

1. Pick the **Algorithm**: `edc16`, `edc17`, `pcr21` or `crc32-tail`.
2. Press **Verify**. Each protected block is listed with its region, the stored value and the computed value.
3. If any block is **BAD**, press **Fix → Save as…** and choose a new file name. The corrected copy becomes the opened image. **Fix & save** does the same but overwrites the opened file.
4. Press **Verify** again. Every block must be **OK**.

## Reading the results

- **All blocks OK** — the integrity fields match the contents.
- **One or more blocks are BAD** — stored and computed values differ. Either the file was edited, or you picked the wrong family.
- **No protected blocks** — the algorithm did not recognise a layout in this image. Check the family.
- After a fix, the list shows each change made. "Nothing to fix" means the checksums were already correct.

## Common errors

- **Permission denied / read-only** — the file can't be written in place. Use **Fix → Save as…** to write a copy you own.
- **BAD on a file you never edited** — likely the wrong family, or a partial or corrupt read. Do not fix it; find out why first.

## Safety

- **Fix & save overwrites the opened file and does not make a backup.** Prefer **Fix → Save as…**, or copy the original first.
- `pcr21` corrects the calibration CRC only. It never touches the RSA signature block. A signed region that was changed will still be rejected by the ECU.
- A correct checksum does not make a tune safe. It only means the ECU will accept the file.
