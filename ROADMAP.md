# OpenTune Roadmap

> ⚠️ **Status: early alpha.** The offline tooling works on a `.bin`; the apps are
> pre-release alpha builds and the hardware path is unproven on a car. Don't flash
> a real vehicle yet. Expect breaking changes.

Progress is tracked here. Checked = done and on `main`. This is a living document.

**Legend:** `[x]` done · `[~]` in progress · `[ ]` planned

---

## M0 — Foundation (in progress)
The compiling skeleton everything else builds on.

- [x] Cargo workspace, Rust toolchain pin, release profile
- [x] `opentune-core`: `Interface` driver trait, error/result types, capabilities
- [x] Core types: protocols, memory regions, ECU identity, progress reporting
- [x] Driver registry (enumerate + construct drivers by id)
- [x] `mock`/file driver (operate on local `.bin`, no hardware)
- [x] `opentune-cli` wired to the mock driver
- [x] GPL-3.0 licence, README, this roadmap
- [x] GitHub Actions CI (fmt + clippy + test + build)
- [x] Unit tests for core (ISO-TP framing, NRC parsing, CRC32)

## M1 — Offline calibration tooling
Everything you can do with a `.bin` and no car.

- [ ] Binary model: load/save, region map, metadata
- [x] **Checksum engine**: trait + verify + correct
  - [x] generic (CRC32-tail example + CRC32 helper)
  - [x] **Bosch EDC16 (MPC56x): SOLVED** — full directory-walking verify+correct, validated against real full dumps (BMW/KIA). VAG *signed* high-entropy block = separate, unsolved
  - [x] **Bosch EDC17 (TriCore): SOLVED** — directory-walked CRC32; the firmware-specific VAG per-block constant is bootstrapped from your own stock dump (`checksum fix --reference`)
  - [x] **Continental Simos PCR2.1: SOLVED** — forward CRC-32 + on-image directory, validated on real 1.6 TDI dumps
  - [~] **EDC15 / EDC15C6** — VAG variant solved clean-room from GPL sources; Mercedes CR1 constant-recovery in progress (stub refuses until validated)
- [x] Map diff (compare stock vs modified, byte and region level)
- [x] **Real map library** — EDC16U34 (Golf): smoke/IQ limiters validated vs real dumps (docs/MAP-LIBRARY.md)
- [x] **Calibration-definition import/export** — TunerPro **XDF** import+export (round-trip validated) and factory **ASAM A2L** import (`opentune-a2l`; validated on real EDC15/Siemens-PPD/BMW-MSV80, thousands of maps; linear scaling recovered, non-linear flagged)
- [x] **Map-aware compare** — diff two calibrations map-by-map in physical units (`compare`)
- [x] **VIN / part-number decoder** (`opentune-vin`): VIN (WMI/country/year/check-digit) + VAG/Bosch part parsing
- [x] **Mercedes + BMW catalog** entries (catalog/manual/) alongside the 998-car opendbc set
- [~] **ECU auto-identification** (`identify` command): read UDS/KWP DIDs → match `EcuDef.identify` keys + a vehicle catalog
  - [x] `identify` field on ECU defs + `opentune identify` (defs + catalog matching)
  - [x] harvest comma.ai opendbc FW fingerprints (MIT) into a catalog — 998 entries, 14 brands
  - [ ] read the real identity DIDs in the drivers (0xF187/F189/F190/F18A…)
- [x] ECU definition format (JSON) + 5 fleet defs + parse test
- [x] `opentune-cli`: `identify`, `info`, `read`, `dump`, `diff`, `findmaps`, `compare`, `checksum verify|fix`, `flash` (mock) + `--resume`/`--transcript`, `vault`, `ecudefs`, `import-xdf`, `export-xdf`, `import-a2l`, `vin`, `decode-part`, `dtc`
- [x] Calibration **map-definition format** (docs/MAP-DEFS.md) + `maps` in ECU defs
- [x] Prior-art survey + credits (docs/PRIOR-ART.md); flash sequences (docs/FLASHING-NOTES.md); Bosch checksum structure (docs/BOSCH-CHECKSUM.md)
- [x] **ECU identification / autoselect**: `opentune identify` (read DIDs → match ecudefs + catalog), `EcuDef.identify`, `catalog` module, and a **998-entry vehicle catalog harvested from opendbc** (14 makes, MIT); docs/IDENTIFICATION.md

## M2 — Read path (hardware)
Identify and read a real ECU.

- [x] `opentune-core` protocol layer: ISO-TP (BS/STmin/wait/overflow hardened), UDS, KWP2000, **VW TP2.0**
- [~] **OpenPort 2.0 driver** (CDC-ACM `at…` protocol) — implemented, awaiting hardware test
- [~] **SocketCAN driver** — implemented, awaiting hardware test
- [~] **K-line / ELM327 driver** — implemented, awaiting hardware test
- [~] **KKL 409.1 raw-K-line driver** — implemented (serial BREAK init, ISO14230 framing), wired into CLI, awaiting hardware
- [ ] **Cheap / AliExpress clone interface compatibility** (research: docs/CHEAP-INTERFACES.md)
  - [ ] OpenPort 2.0 clones (via the openport2 driver) — verified on a clone
  - [ ] ELM327 clones (read/log; most can't flash) via the elm327 driver
  - [x] **KKL 409.1 raw-K-line driver** (FTDI/CH340; real BREAK init, ISO14230 framing) — awaiting hardware test
  - [~] **slcan USB-CAN driver** (CANable/CANtact) — implemented, awaiting hardware
  - [ ] **Generic J2534 driver** (Mongoose/VNCI/SVCI/Scanmatik clones)
- [ ] ECU identify (part/SW number, VIN)
- [ ] Full OBD read — Bosch EDC16 first
- [x] Battery-voltage read, exposed as a safety gate

## M3 — Flash path
Write a calibration back, safely.

- [x] Security access: `SeedKey` plugin trait + all 3 drivers run the 0x27 seed/key handshake before flash (user supplies the algorithm)
- [ ] Erase / `requestDownload` / `transferData` / `transferExit`
- [ ] Calibration-only write (write just the map region where supported)
- [ ] Read-back verify after write
- [ ] EDC16 end-to-end on the bench, then in-car

## M4 — Safety & recovery
- [x] Flash orchestrator state machine (backup → write → verify)
- [x] Voltage gate (refuse to flash below threshold)
- [x] **Resumable / recoverable flash** — chunked write with a SHA-256-guarded checkpoint; resume after an interruption without re-writing already-verified chunks, and never apply the wrong payload to a region
- [x] Backup vault (every read archived with ECU id + timestamp + CRC32); `flash` auto-archives the pre-flash backup; `vault` CLI lists/verifies it
- [x] **Structured session transcript** — timestamped `.log` + `.jsonl` written beside the vault backup

## M5 — Desktop app (Tauri)
- [x] Desktop app (Tauri): identify, checksum verify/fix, findmaps, flash (mock), vault, offline guide
- [x] Map viewer (heat grid) **and editor** (edit/interpolate/smooth, write-back + checksum fix) + map-by-map compare
- [x] **Portable Linux builds** — glibc baseline fixed (ubuntu-22.04 CI + a `debian:bookworm` Docker recipe in `desktop/portable-build/`)
- [ ] Connect / read a real ECU + flash wizard (hardware)
- [~] **Auto-update from GitHub Releases** (configured; needs signing key)

## M6 — More ECUs
- [~] Bosch EDC17 — **checksum SOLVED**; OBD flash pending hardware
- [x] **Continental/Siemens Simos PCR2.1 checksum SOLVED** (forward CRC-32 + on-image directory; validated on real 1.6 TDI dumps; RSA signature block out of scope)
- [ ] Bosch EDC15C6 over K-line (Mercedes CR1 family)
- [x] DTC database + decoder (11,140 codes; `opentune dtc <code>`)
- [ ] Growing community ECU-definition library

## M7 — Android app
- [x] **opentune-ffi** — UniFFI binding over the core (compile-verified; Kotlin bindings generated)
- [~] Jetpack Compose app (`android/`) — offline identify/checksum/maps/flash on a `.bin`; needs Android SDK/NDK to build the APK, real adapters for USB-OTG/BLE
- [x] Reuses the same core, ECU defs and safety rules (one engine)

## M8 — Open hardware interface
- [~] ESP32-S3 + CAN + K-line interface spec (docs/hardware.md); KiCad next
- [~] **Arduino K-line bridge firmware** (ATmega328, hardware/firmware/kline-bridge) — written; a DIY cheap K-line adapter, host driver to follow
- [ ] Device firmware (ISO-TP, TP2.0, K-line init, raw UART for boot mode)
- [ ] USB CDC + optional WiFi/BLE bridge
- [ ] OpenTune driver for the device

## Ongoing
- [x] Documentation: architecture, driver authoring, ECU-defs, hardware, android
- [ ] Test fixtures and CI coverage
- [ ] Reproducible, signed release builds for Linux / Windows / macOS
