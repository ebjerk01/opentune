# OpenTune — ideas & directions

A living backlog of features worth building. Grouped by theme; rough effort in
`[S/M/L]`. Calibration-focused and own-hardware only — nothing here targets
emissions defeat, odometer fraud, or theft.

## Checksums & integrity
- `[M]` **EDC17** (TriCore) — your Passat CC's engine *(done: verify + correct; VAG constant
  bootstrapped from your own stock via `checksum fix --reference`)*.
- `[M]` **Simos PCR2.1** — VAG 1.6 TDI CR *(done: self-contained on-image directory)*.
- `[S]` **EDC16-CR in-situ** confirmation (constants validated, needs a live word-wise dump).
- `[M]` **Checksum plugin system** — load ECU checksum algorithms as data/plugins so new
  families drop in without a rebuild; a shared `correct_with_reference` boundary model.
- `[S]` **Multi-block report** in the CLI/app with per-block region, stored vs computed, and a
  one-click "fix only the blocks that are BAD".

## Maps & calibration
- `[L]` **Map editor** (not just the viewer) — edit cells in the heat grid, interpolate,
  copy/paste regions, undo, and write back with automatic checksum correction.
- `[L]` **A2L / DAMOS import** — parse the OEM ASAM calibration description to auto-populate
  precise map definitions (addresses, axes, scaling) for any ECU that has one *(done:
  `opentune-a2l` crate + `import-a2l` CLI, zero-dep parser; RAT_FUNC affine recovery, COM_AXIS
  resolution, non-linear flagged; validated on real EDC15/PPD/MSV80 A2Ls. `--base` rebases
  logical CPU addresses to bin offsets. STD_AXIS embedded address + >2-D cuboids still TODO)*.
- `[M]` **XDF / TunerPro import + export** — reuse the huge community library of definitions
  *(done: `opentune-xdf` crate, `import-xdf` modern XML + legacy text, `export-xdf` modern XML,
  round-trip validated)*.
- `[M]` **Real map library** for the common ECUs (EDC16U34 first) *(in progress)*.
- `[M]` **Auto map-finder** — cluster a stock/tuned diff, classify regions against known map
  shapes, and emit candidate `MapDef`s to seed a definition.
- `[M]` **Tune compare** — diff two calibrations map-by-map (not just bytes), overlaying the
  grids so you can see exactly what a tune changed and by how much *(done: `compare` CLI —
  per-map changed-cell count, Δmax/Δavg in physical units, and a before→after grid)*.
- `[S]` **Units & axis decoding** everywhere (mg/stroke, mbar, bar, °CA, Nm).

## Identification & data
- `[S]` **Richer ECU definitions** — part/SW number ranges, memory maps, seed/key ids.
- `[M]` **DTC database + decoder** — map DTC codes to descriptions (generic OBD + manufacturer),
  used by diagnostics and the standalone fault review.
- `[M]` **Grow the vehicle catalog** beyond opendbc; add Mercedes/BMW (not in opendbc) from
  public part-number data.
- `[S]` **VIN / part-number decoder** in the app.

## Interfaces & hardware
- `[M]` **slcan / gs_usb** polish + **generic J2534** (Windows) and **ELM327 BT/WiFi** transports.
- `[M]` **CAN FD + DoIP** — for newer ECUs (ESP32-S3 CAN-FD, BMW/VAG Ethernet diagnostics).
- `[L]` **Open hardware interface** — finish the ESP32-S3 + CAN + K-line board (KiCad,
  CERN-OHL-S) and its firmware; the Arduino K-line bridge is the small-scale sibling.
- `[M]` **On-device logging** — the interface streams framed messages so timing-critical work
  stays on the adapter.

## Flashing & safety
- `[M]` **Resumable / recoverable flash** — checkpoint block progress; resume after a drop.
- `[M]` **Flash wizard** — guided session with the safety checklist, battery-voltage watch,
  and an automatic pre-flash backup to the vault.
- `[S]` **Backup vault browser** — list/restore/compare archived reads *(done: `vault` CLI
  lists the vault and `--verify` re-checks each .bin CRC against its .meta; apps browse too)*.
- `[M]` **Security-access (seed/key) plugin UX** — supply your own algorithm; never shipped.

## Apps & ecosystem
- `[L]` **Android app** — reuse the Rust core via UniFFI; USB-OTG / BLE adapters; same safety
  rules and ECU defs.
- `[M]` **Public desktop app** (OSS edition) — the calibration-only counterpart of the private
  bench build.
- `[S]` **Structured logging** across core/flash/drivers with a session transcript.
- `[M]` **Signed, reproducible releases** for Linux/Windows/macOS + desktop auto-update *(in progress)*.
- `[M]` **In-app guide / wiki** *(in progress)*.
- `[S]` **Live-data gauges / dashboard** (once a read-capable interface is on the bench).

## Project
- `[S]` Test fixtures + CI coverage for the protocol layer (ISO-TP / TP2.0 / KWP edge cases).
- `[S]` Docs site + screenshots.
- `[M]` Community ECU-definition & map library (contribution format + review).
