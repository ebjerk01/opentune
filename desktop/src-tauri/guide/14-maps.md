# Maps

Find what changed between two dumps, import map definitions (XDF / A2L), turn a
region into a map, and view or edit a map as a heat grid with real axes and
units. Edits are written back with a full backup first.

## What you need

- **Find maps**: two dumps of the same ECU (a stock and a modified file).
- **Map editor**: a dump (the opened image, or pick one with **Image…**) and either a bundled definition, an imported XDF/A2L, or a region you describe.
- No adapter and no car.

## Find maps (diff two files)

1. Pick **Stock file…** and **Tuned file…**.
2. Set **gap** (default 16): differences closer than this many unchanged bytes are merged into one region.
3. Set **min len** (default 8): shorter regions are hidden.
4. Press **Run diff**. Each region shows start, end, length and a **stride hint** (for example `~16x16 u16?`), a guess at the table shape.
5. **Make map** opens the "make a map from a region" form with start and length filled in.

Only the overlapping length is compared. If the files differ in size, the extra
bytes at the end of the longer file are ignored.

## Import definitions and make maps

1. **Import XDF / A2L…** picks a TunerPro `.xdf` or ASAM `.a2l` file.
2. For an A2L that uses CPU addresses, enter **A2L base** (the CPU address of file offset 0, for example `0x80000000`). Leave it blank for XDF or for an A2L that already uses file offsets.
3. Press **Import**. Each map is listed; **Open** decodes it from the image.
4. **Make a map from a region**: give a name, start, length, rows, cols, data type (`u8` … `f32`), endianness, and scaling (`physical = raw × factor + offset`). Press **Make map → editor**.

## Map editor

1. Choose a **Definition** and a **Map**, or use an imported/promoted map.
2. **Image…** picks a different image; otherwise the opened image is used.
3. Press **Decode / open**. The grid shows physical values, blue (low) to red (high), with the X/Y axis breakpoints and unit.
4. Select cells: click-drag or shift-click for a rectangle, or **Select all**.
5. Edit the selection with the value box and **Set =**, **+**, **−**, **×%** (multiply by the value as a percentage: `105` adds 5 %), **Interpolate** (bilinear between the selection corners) or **Smooth 3×3**. Double-click a cell to type a value.
6. **Diff overlay** highlights cells that differ from the decoded values. **Undo**, **Redo** and **Reset** work on your edits.
7. **2D grid / 3D surface** switches the view. The 3D view is read-only; drag to rotate.
8. Write back with **Write to image…** (in place) or **Write → Save as…** (new file). Leave **fix checksum on write** ticked unless you have a reason not to.
9. **＋ Add to project** stores the edit in the current project (see **Project**).

## Reading the results

After a write the card shows how many cells changed, the output file, the
**safety backup** path, and the checksum result.

- Values are clamped to the definition's min/max where it has them, then rounded and clamped to the data type on write. A value you typed may be stored slightly differently.
- For a bundled definition the checksum family comes from the definition. For an imported or promoted map it comes from the **Algorithm** selected in the **Checksum** panel, so set that first.

## Common errors

- **Permission denied / read-only** — use **Write → Save as…**.
- **Could not write the safety backup** — the folder next to the image is not writable. Nothing was changed. Move the image to a writable folder.
- **Out of range / garbage values** — wrong address, base, data type or endianness. Check them against the definition source.

## Safety

- Every write first saves the whole original image to an `opentune-vault` folder next to the image. If that fails, nothing is written.
- Re-run **Checksum → Verify** after writing.
- A map that decodes cleanly is not proof it is the map you think it is. Compare against a known-good source before using a file in a car.
