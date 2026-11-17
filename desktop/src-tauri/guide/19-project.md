# Project

Save a tuning session as a small `.otproj` file. It records the base image path
and a fingerprint of it, the chosen definition and checksum, and the map edits
you made. It stores **what you changed, not a copy of the dump**.

## What you need

- A base image file, and map edits made in **Maps → Map editor**.
- No adapter and no car.

## Step by step

1. **New** starts an empty project. Give it a **Name** and optional **Notes**.
2. **Base image…** picks the dump the edits apply to. If you skip this, the first captured edit's image is used.
3. In the Map editor, press **＋ Add to project**, or here press **Capture current map**, to store the map that is open in the editor.
4. **Save** or **Save as…** writes the `.otproj` file.
5. Later, **Open…** a project. Each edit's **Open** re-decodes that map from the base image and lays your values back on top in the editor.

## Reading the results

- Each edit row shows the map and how many cells changed against the originally decoded values.
- The image status warns when the base image is missing or its fingerprint no longer matches. That means the file changed since the project was saved.
- **Recent projects** is kept in the app's local storage.

## Common errors

- **Fingerprint mismatch** — the base image was modified (for example by a write-back or checksum fix) or replaced. Re-opened edits may land on different data. Point the project at the correct original.
- **Not a valid OpenTune project** — the file is not an `.otproj` or is damaged.

## Safety

Saving a project writes only the `.otproj` file. It never changes the image.
Writing edits into an image happens in the Map editor, with a backup first.
