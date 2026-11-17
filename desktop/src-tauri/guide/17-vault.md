# Vault

Browse a backup folder. OpenTune stores each backup as a `.bin` file plus a
`.meta` sidecar that records a CRC-32 and identity details.

## What you need

- A folder with backups. Map write-backs create one called `opentune-vault` next to the image you edited.
- No adapter and no car.

## Step by step

1. Press **Choose backups folder…** and pick the folder.
2. The table lists every `.bin` file with its size, whether it has a sidecar, and the sidecar contents.
3. Press **Refresh** after new backups are written.

## Reading the results

- **Sidecar: present** — a `.meta` file exists. It holds the CRC-32 you can use to check the backup is intact.
- **Sidecar: none** — a plain `.bin` with no record. It may still be fine, but nothing proves it is unchanged.
- "No .bin backups in this folder" — the folder is empty or holds other files.

## Common errors

- **Empty list** — check you picked the folder itself, not its parent. Only files ending in `.bin` are listed.

## Safety

Read-only. Keep at least two copies of every original read, on different
disks, before you write anything to a car.
