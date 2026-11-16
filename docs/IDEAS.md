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
  families drop in without a rebuild *(groundwork done: `AlgoInfo`/`algorithms()` descriptor
  registry + `correct_bad_blocks` hook; full dynamic/data-driven loading still TODO)*.
- `[S]` **Multi-block report** — per-block region, stored vs computed, and a one-click
  "fix only the BAD blocks" *(done: `checksum verify`/`report` + `fix --only-bad` + `checksum algos`)*.

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
  used by diagnostics and the standalone fault review *(done: `opentune-dtc`, 11,140 codes)*.
- `[S]` **Live DTC read/clear** — pull stored/pending fault codes over OBD-II (mode 0x03/0x07) or
  UDS (0x19 readDTCInformation), decode them with the built-in database, and show/clear them in
  the CLI + desktop/Android app. A nice, low-risk first on-car feature — read-only diagnostics, no
  flashing — once a read-capable interface is on the bench. The decoder already exists.
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

## Drivetrain control units — TCUs + AWD/4WD (scope expansion; user, 2026-11-16)
OpenTune's identify / checksum / maps / flash pipeline is **not engine-specific**. Make
**transmission (TCU) and all-wheel/4WD controllers first-class targets alongside engine ECUs**,
across every vehicle family this project has worked on. Each target just needs its own definitions +
checksum scheme; the flash/UDS transport is shared, and clutch-based-AWD maps are largely reusable
across brands. `[L]` for all of these.

Targets by family (what we've actually touched):
- **VAG** — DSG/TCU (Temic **DQ250/DQ200/DQ381/DL382**, Simos-based); **Haldex** AWD coupling
  (Gen1–5; Gen4/5 BorgWarner = the "Haldex performance" targets). Refs: bri3d/VW_Flash,
  TheFlashBold/DQ250-MQB-Bench-Flash, dspl1236/simos-suite.
- **Mercedes** — 722.9 (7G-Tronic) TCU; 4MATIC transfer-case / traction control (STAR-PC/XENTRY context).
- **BMW** — ZF 6HP/8HP TCU (EGS); xDrive transfer case (ATC).
- **Ford** — PowerShift / 6F35 TCU; Haldex-style AWD coupling.
- **Mitsubishi** — Pajero Super-Select 4WD + auto TCU.
- **Volvo** — Aisin TCU; Haldex AWD.
Per target the work is: controller defs (maps = clutch pressure/limits, torque limiters, shift
points, launch, AWD engagement/pre-load & lock curves), the checksum family, and the on-car flash
address. Build the VAG DSG + Haldex pair first (best open refs), then generalize the controller-def
schema so other brands drop in.

## External reference projects (surveyed 2026-11-16; licenses checked for GPL-3.0 reuse)
- **Flash kernels (also the EDC15C6 lead):** `fenugrec/npkern` (GPL-3.0 — RAM-resident reflash-kernel
  model: upload→execute→flash-ops→reset) + `nisprog`; `NefMoto/NefMotoOpenSource` (GPL-3.0 — EDC15
  bootmode + **C167/C166 bootstrap loaders**, directly relevant to EDC15C6's C167 class).
- **VAG flashing / flash-container decode:** `bri3d/VW_Flash` (BSD-2 — checksums, LZSS+AES, FRF/ODX
  decrypt, `a2l2xdf`); `dspl1236/simos-suite` (MIT — named algos: CRC32-VW, ECM3 64-bit, counter-XOR).
- **UDS reference:** `pylessard/python-udsoncan` (MIT — gold ISO-14229 behavior to validate our Rust UDS).
- **Hardware:** `Switchleg1/esp32-isotp-ble-bridge` (MIT — our ESP32-S3 / ISO-TP / BLE path as shippable fw).
- **Checksum breadth we lack:** OpenRemap detection schemes (ME7/NefMoto/IronFelix/MS43…), `nyetlabs/ME7Sum`.
- **Mercedes diagnostics:** `rnd-ash/OpenVehicleDiag` CBFParser (GPL-3.0 Rust — CBF→JSON; SMR-D parser was DMCA'd).
- **Map/def import:** `miikasyvanen/FastECU` ECUFlash + RomRaider definition parsers.
- Note: `tehfalcon-vw/Nexum-Studio` and `MichShav/NissECU` are NON-OSS (no code reuse); `bisera/openport-j2534` is dead (use `james-portman/j2534`).
