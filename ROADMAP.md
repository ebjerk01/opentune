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
- [ ] GitHub Actions CI (fmt + clippy + test)
- [~] Unit tests for core (NRC parsing, CRC32 done; ISO-TP framing next)

## M1 — Offline calibration tooling
Everything you can do with a `.bin` and no car.

- [ ] Binary model: load/save, region map, metadata
- [~] **Checksum engine**: trait + verify + correct
  - [x] generic (CRC32-tail example + CRC32 helper)
  - [ ] Bosch EDC16 blocks (validated against reference files)
  - [ ] EDC17, PCR2.1 (later)
- [x] Map diff (compare stock vs modified, byte and region level)
- [~] ECU definition format (JSON): id, protocol, memory map, checksum type, seed/key id
- [~] `opentune-cli`: `info`, `read`, `diff`, `checksum verify|fix`, `flash` (mock) done

## M2 — Read path (hardware)
Identify and read a real ECU.

- [ ] `opentune-core` protocol layer: ISO-TP, UDS, KWP2000, VW TP2.0
- [ ] **OpenPort 2.0 driver** (CDC-ACM `at…` protocol, J2534-style)
- [ ] **SocketCAN driver**
- [ ] **K-line / ELM327 driver** (+ init: 5-baud / fast)
- [ ] ECU identify (part/SW number, VIN)
- [ ] Full OBD read — Bosch EDC16 first
- [ ] Battery-voltage read, exposed as a safety gate

## M3 — Flash path
Write a calibration back, safely.

- [ ] Programming session + security access (seed/key plugin interface)
- [ ] Erase / `requestDownload` / `transferData` / `transferExit`
- [ ] Calibration-only write (write just the map region where supported)
- [ ] Read-back verify after write
- [ ] EDC16 end-to-end on the bench, then in-car

## M4 — Safety & recovery
- [x] Flash orchestrator state machine (backup → write → verify)
- [x] Voltage gate (refuse to flash below threshold)
- [ ] Resumable / recoverable flash
- [ ] Backup vault (every read archived with ECU id + timestamp + hash)
- [ ] Structured logging of every session

## M5 — Desktop app (Tauri)
- [ ] App shell + core binding
- [ ] Connect / identify / read / backup
- [ ] Map viewer (read-only first)
- [ ] Flash wizard with the safety checklist
- [ ] **Auto-update from GitHub Releases** (signed)

## M6 — More ECUs
- [ ] Bosch EDC17 (checksum + OBD flash)
- [ ] Continental/Siemens Simos PCR2.1
- [ ] Bosch EDC15C6 over K-line (Mercedes CR1 family)
- [ ] Growing community ECU-definition library

## M7 — Android app
- [ ] Expose the Rust core via UniFFI
- [ ] Kotlin/Compose shell: connect (USB-OTG / BLE), read, flash
- [ ] Reuse the same safety rules and ECU defs

## M8 — Open hardware interface
- [ ] ESP32-S3 + CAN + K-line reference board (KiCad, CERN-OHL-S)
- [ ] Device firmware (ISO-TP, TP2.0, K-line init, raw UART for boot mode)
- [ ] USB CDC + optional WiFi/BLE bridge
- [ ] OpenTune driver for the device

## Ongoing
- [ ] Documentation (architecture, driver authoring, ECU-def authoring)
- [ ] Test fixtures and CI coverage
- [ ] Reproducible, signed release builds for Linux / Windows / macOS
