# OpenTune Roadmap

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
  - [x] Bosch EDC15C6 checksum module (edc15.rs)
  - [ ] EDC17, PCR2.1 (later)
- [x] Map diff (compare stock vs modified, byte and region level)
- [~] **ECU auto-identification** (`identify` command): read UDS/KWP DIDs → match `EcuDef.identify` keys + a vehicle catalog
  - [x] `identify` field on ECU defs + `opentune identify` (defs + catalog matching)
  - [ ] harvest comma.ai opendbc/openpilot FW fingerprints (MIT) into a catalog
  - [ ] read the real identity DIDs in the drivers (0xF187/F189/F190/F18A…)
- [x] ECU definition format (JSON) + 5 fleet defs + parse test
- [~] `opentune-cli`: `info`, `read`, `diff`, `checksum verify|fix`, `flash` (mock) done
- [x] Calibration **map-definition format** (docs/MAP-DEFS.md) + `maps` in ECU defs
- [x] Prior-art survey + credits (docs/PRIOR-ART.md); flash sequences (docs/FLASHING-NOTES.md); Bosch checksum structure (docs/BOSCH-CHECKSUM.md)
- [~] **ECU identification / autoselect**: read DIDs → match ecudefs + a vehicle catalog; `opentune identify`, `EcuDef.identify`, `catalog` module done; opendbc harvest pending

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
  - [ ] **slcan / gs_usb USB-CAN driver** (CANable/CANtact/CH340 clones)
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
- [ ] Resumable / recoverable flash
- [x] Backup vault (every read archived with ECU id + timestamp + CRC32)
- [ ] Structured logging of every session

## M5 — Desktop app (Tauri)
- [~] App shell + core binding (Tauri scaffold)
- [ ] Connect / identify / read / backup
- [ ] Map viewer (read-only first)
- [ ] Flash wizard with the safety checklist
- [~] **Auto-update from GitHub Releases** (configured; needs signing key)

## M6 — More ECUs
- [ ] Bosch EDC17 (checksum + OBD flash)
- [ ] Continental/Siemens Simos PCR2.1
- [ ] Bosch EDC15C6 over K-line (Mercedes CR1 family)
- [ ] Growing community ECU-definition library

## M7 — Android app
- [~] UniFFI plan documented (docs/android.md)
- [ ] Kotlin/Compose shell: connect (USB-OTG / BLE), read, flash
- [ ] Reuse the same safety rules and ECU defs

## M8 — Open hardware interface
- [~] ESP32-S3 + CAN + K-line interface spec (docs/hardware.md); KiCad next
- [ ] Device firmware (ISO-TP, TP2.0, K-line init, raw UART for boot mode)
- [ ] USB CDC + optional WiFi/BLE bridge
- [ ] OpenTune driver for the device

## Ongoing
- [x] Documentation: architecture, driver authoring, ECU-defs, hardware, android
- [ ] Test fixtures and CI coverage
- [ ] Reproducible, signed release builds for Linux / Windows / macOS
