# Compare

Decode every map of one definition from two images and see, per map, how many
cells changed and by how much, in physical units.

## What you need

- Two dumps of the same ECU (stock and modified).
- A bundled **Definition** that fits that ECU.
- No adapter and no car.

## Step by step

1. Choose the **Definition**.
2. Pick **Stock image…** and **Tuned image…**.
3. Press **Compare maps**.
4. For a changed map, press **grid** to see both versions side by side.

## Reading the results

- The summary shows how many maps changed, the total changed cells, and both file sizes.
- Per map: size, number of changed cells, **Δmax** (largest change) and **Δavg** (average change), in the map's unit.
- **Out of range — skipped** — the map's address is outside one of the files. The definition probably does not fit this file.
- The grid view uses one shared colour scale for both images. Changed cells are outlined.

## Common errors

- **Every map changed a lot** — the definition does not fit these files, or the two files are from different software versions. Compare only files with the same software number.
- **"Pick a definition"** — choose one first.

## Safety

Read-only. Nothing is written.
