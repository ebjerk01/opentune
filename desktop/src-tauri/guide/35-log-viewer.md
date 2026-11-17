# Log viewer

Open a CSV datalog, plot its channels against time, and overlay it on a
calibration map to see which cells the engine actually used. This closes the
tune loop: **log → see the cells that were used → edit them in Maps → compare**.
Everything is offline and read-only.

## What you need

- A **CSV log**. Any of:
  - a **Live log** CSV saved by OpenTune (one row per parameter per tick);
  - a CSV with one column per channel and a time column, from any logger.
- For the **map overlay**: an ECU definition with the map, and, when the map's
  axes are stored in the image (most bundled maps), the **image (.bin)** the
  definition belongs to.

A synthetic example ships in `samples/`: `opentune-demo-log.csv` overlays on
the demo map in `opentune-demo-edc16.json` + `opentune-demo-edc16.bin`.

## Step by step

1. Press **Open log…** and choose the CSV. The summary shows what was detected.
2. Read the **notes** under the summary. Anything the viewer had to infer
   (time unit, missing header, skipped lines, sorted samples) is listed there.
3. If the time unit is wrong, pick it in **Time unit**. The log is re-read.
4. Tick the channels to plot. **Lanes** gives each its own band; **Overlay**
   draws them on top of each other, each scaled to its own range.
5. Zoom with the **mouse wheel** (around the cursor) or **+ / −**, **drag** to
   pan, **double-click** or **Whole log** to see everything.
6. Move the mouse over the plot: the readout shows **every channel** at that
   sample, exactly as logged.
7. The **Statistics** table follows the visible window: min, max, mean,
   standard deviation, sample count, missing samples and when the maximum was
   reached.

### Map overlay

1. Choose a **Definition** (or **Definition file…** for a `.json` that is not
   bundled) and a **Map**. The X and Y labels show each axis's name and unit,
   and a channel with a matching unit or name is pre-selected.
2. Press **Image…** if the hint asks for it. The image opened in the Image
   panel is used when you do not choose one.
3. Check the **X** and **Y** channels. Optionally choose a channel under
   **Value range of** (for example injection quantity).
4. Choose **Nearest** (whole sample to the nearest cell) or **Bilinear**
   (split over the four surrounding cells by interpolation weight, like the
   ECU's lookup).
5. Tick **Visible window only** to overlay just the zoomed part of the log.
6. Press **Overlay**.

## Reading the results

- **Hits** colours each cell by how many samples used it, **Time** by how long
  the engine stayed there, **Value** by the mean of the chosen channel. Empty
  cells were never used. Hover a cell for all of it at once, including the
  value range and the map's own value.
- **Show map values** writes the map's cell values into the grid, so you see
  the numbers you would edit on top of where the engine ran.
- Samples **outside the axes** are counted in the edge cell (an ECU clamps its
  lookup there) and reported separately per side.
- Samples with a missing X or Y value are **skipped** and counted.
- Time in a cell is the time to the next sample, capped at five median sample
  steps so a pause in the log does not count as time in a cell.

## Units

The viewer compares the unit of each channel with the unit of the map axis it
is placed on. Spellings of the same unit match (`1/min` = `rpm`, `mbar` = `hPa`,
`mg/Hub` = `mg/stroke`). Anything else is a **mismatch**: the overlay is refused
with both units named. Pick the right channel, or tick **Accept unit mismatch**
to use the values exactly as logged. Units are never converted.

When a channel or an axis has no unit, the overlay runs and says the units
could not be checked.

## Supported CSV files

- Delimiters: comma, semicolon, tab, `|`. A decimal comma (`12,5`) is read
  when the delimiter is not a comma.
- Units in the header (`Engine speed [rpm]`, `Boost (hPa)`) or in a units row
  under the header.
- Time columns named like `time`, `t`, `timestamp`, `Zeit`, `elapsed`, with the
  unit in the name (`Time (ms)`, `time_s`) or inferred from the values (and
  then reported). `hh:mm:ss.fff` clock times work too. Unix timestamps are
  shown relative to the first sample. Without a time column the sample index
  is used.
- Lines starting with `#` are comments; text lines before the table are
  skipped; text-only columns are left out. All of this is listed in the notes.

## Common errors

- **"axis is stored in the image"** — choose the `.bin` the definition belongs
  to with **Image…**.
- **"unit mismatch"** — X and Y are probably swapped, or the log uses another
  unit (kPa vs hPa). Choose another channel.
- **"axis is not strictly monotonic"** — the axis decoded from this image is
  not a valid breakpoint list; the definition does not match this software
  variant.
- **"shared axis"** — the map reuses another axis by name, which the overlay
  does not resolve yet.
