# Image

Open a flash dump and look at its raw bytes. The file you open here is the
**opened image**. Analyze bin, Identify, Checksum and the Maps editor use it
unless you pick a different file in those panels.

## What you need

- A flash dump file (`.bin`, `.ori`, `.dat`, `.eep` and similar, or any file).
- No adapter and no car.

## Step by step

1. Press **Open flash dump…** and pick the file.
2. The card shows the file name, size (bytes and KiB) and full path.
3. **Hex preview** shows 256 bytes at a time: address, 16 hex bytes per row, and the printable ASCII on the right.
4. Use **◀ prev** / **next ▶** to move 256 bytes back or forward.
5. To jump, type an offset in hex (for example `0x070000`) in the box and press Enter or click away.

## Reading the results

- Addresses are **file offsets**, starting at `0x00000000`. They are not the ECU's CPU addresses. A definition or A2L may use a different base.
- Readable text in the ASCII column (part numbers, software versions, `EPK` strings) is often what Identify and Analyze bin look for.
- The file is read as raw bytes. Container formats (Intel HEX text, `.frf`, vendor archives) are not unpacked here.

## Common errors

- **Nothing happens after picking a file** — check the file still exists and that you have read permission.
- **Offset ignored** — the box expects hex. `70000` and `0x70000` both work; decimal does not.

## Safety

This panel only reads. It never changes the file. Before you edit anything in
other panels, copy the original dump somewhere safe and keep it untouched.
