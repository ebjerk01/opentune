# Changelog

All notable changes to OpenTune. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

OpenTune is **early alpha**. Unless an entry says otherwise, a feature is tested
offline only (unit tests, simulators, synthetic fixtures) and is **not yet proven
on a real car or adapter**. Car-facing panels stay in the desktop app's Bench
section (alpha/bench builds) until they are vehicle-tested.

## [Unreleased]

## [0.7.0] - 2026-11-19

### Added
- **"Check my car" guided wizard** (desktop, Bench): a read-only walk through
  identify → scan → fault codes → live data, tested end to end on the virtual
  car and an OBD simulator.
- **Logging triggers**: start, stop and mark conditions for datalogging
  (`log --trigger`, `--split`, also on `obd read --out`), a `triggers/` pack kind
  and a generic-OBD2 wide-open-throttle pull trigger set.
- **Desktop Live log**: reconnect through the adapter hub with link-lost markers,
  Subaru SSM transports (K-line / CAN) and an offline SSM simulator; a negative
  response now counts as a live link in polling and on the Dashboard.
- Offline EGS52 coding CRC verification and opt-in recomputation. CRC-16/ARC
  is implemented and remains unconfirmed on a real EGS.
- EGS52 C167 flash read preset for the 512 KiB emulator window at `0x80000`.
  Plans name missing board registers. Reads require all values. Simulator coverage only.
- Standalone multithreaded checksum search and stock/modified diff tool under
  `tools/checksum-search/`, with generated synthetic test cases.

### Changed
- **Desktop: the sidebar and the content pane scroll separately.** The window is
  a fixed-height shell; scrolling a long panel no longer moves the sidebar and
  vice versa, and each panel keeps its own scroll position. Sidebar groups
  (Calibration, Vehicle, Reference, Help, Bench) collapse from their header
  button; the choice is remembered, and the open panel's entry stays visible.

### Fixed
- **Desktop: smooth scrolling in WebKitGTK.** Long panels (Maps, Log viewer,
  Inspection, Haldex, Controllers, ECU database) scrolled at 30–48 fps because
  every card's blurred shadow was repainted per frame; they now hold 60 fps.
  On alpha/bench builds the whole page scrolled as one (the sidebar stretched
  the layout to ~1800 px). The panel fade is opacity-only, the dashboard
  paints only while visible, and the dashboard no longer touches the GPS
  card's tiles.

## [0.6.0] - 2026-11-19

### Added
- **Read-only DSG diagnostics** (`opentune dsg identify|dtcs|groups`) for the VAG
  DQ250 02E over KWP2000 / VW TP2.0: identification, trouble codes and measuring
  groups. Group meanings are not assigned yet (raw, marked unconfirmed) until a
  real capture backs them. Never run against a real gearbox yet.
- **VAG KWP2000 adaptation** in the adaptation engine, over TP2.0 and K-line:
  measuring blocks, basic settings, adaptation read / test / save with read-back
  (`opentune adaptation kwp-read|kwp-basic|kwp-test|kwp-save`, desktop KWP card).
  Writes need a session, a caller-supplied login and an explicit confirmation;
  OpenTune ships no VAG login algorithm.
- **Actuator tests** (UDS `0x2F`): timed output tests with tester-present,
  automatic return of control to the ECU (also on stop, error, panel close or
  disconnect), desktop **Actuator tests** panel (Bench) and
  `opentune adaptation actuator list|run` (plan only unless `--ack`).
- **Controller definitions** (`opentune-ctrldef`, `ctrldefs/`): one data-defined
  format for gearbox and AWD units (DQ250 02E/MQB, DQ381, Haldex Gen4/Gen5,
  OpenHaldex, ZF 8HP, Mercedes 722.6 EGS51 CAN decode), `opentune controller
  list|show|validate|match|decode`, a `controllers/` definition-pack kind and a
  desktop **Controllers** panel with offline CAN-log decode (candump, CSV, Vector
  ASC).
- **SA2 seed/key scripts**: the interpreter now runs the real VAG SA2 instruction
  set (differential-tested against a public reference implementation), lists the
  scripts inside your own FRF/ODX/SGO files, `opentune seedkey sa2`, and a desktop
  **SA2 script** panel (disassembly, key preview). No scripts are bundled.
- **DoIP** (ISO 13400) as an interface: `--driver doip` with vehicle discovery
  (UDP identification by VIN/EID, announcements, entity status, power mode),
  routing activation, every engine over Ethernet, and a desktop **Discover**
  button (Bench). Tested against a loopback simulator only.
- **GPS logging**: an NMEA 0183 source (`opentune-gps`, RMC/GGA/VTG) for the
  datalogger (`log --gps <port|file>`) and the desktop Live log / Dashboard,
  with position, true ground speed and the speedometer error.
- **Resilient logging over any transport**: the log/OBD pipeline handles a quiet
  gateway bus and an adapter that drops and reopens mid-log (`--reconnect`,
  `--duration`).
- **Drive view**: an at-a-glance gauge view with peak hold, sparklines and offline
  replay of logs.
- **Subaru SSM2** transport, simulator and logging.
- **New interface drivers** (all unverified on hardware): EFI EFP02C USB-CAN
  (1 Mbit/s, 11-bit, writes disabled), the open ESP32 ISO-TP BLE bridge firmware
  (`isotp-ble`; Bluetooth backend behind an optional feature), DoIP, and the
  ESP32-S3 OpenTune interface now registered in the CLI and (Bench) desktop.
- **C167/ST10 bootstrap-loader client** for EDC15 boards (`opentune bsl
  presets|plan|read`): dry-run plan, read behind `--i-understand`, simulator with
  fault injection. You supply the loader files.
- **Contribution workflow**: `CONTRIBUTING.md`, issue and pull-request templates,
  source / licence / evidence metadata on every ECU definition and pack, a
  privacy scanner (`opentune privacy-scan`), `opentune packs validate` and a CI
  data gate.
- Desktop: a shared definition-pack picker in the engine panels; preview test data
  for every panel.
- An identification-only definition for a VW 1.6 TDI CR engine ECU (likely
  Continental Simos PCR2.1; no maps or checksum).

### Changed
- **VW TP2.0 hardened**: retransmit after a lost acknowledgement, receive-side
  recovery, automatic re-open after a channel break (the module's KWP session
  must then be restarted), bounded waits; a seeded fuzzer runs in every test run.
  It found and fixed two real bugs (sequence numbering after a lost first frame,
  a request executed twice with block size 0).
- KWP2000: full negative-response name table and echo checks on session,
  identification and security replies.
- "Response pending" (`0x78`) is accepted only when it echoes the request's
  service, in all drivers and in the ELM327 reply parser.
- DoIP requests honour the caller's timeout instead of waiting up to 60 s for a
  silent ECU.
- CLI log files gain a `ts` column, so they open in the desktop Log viewer.
- The ELM327 driver's first real-car read (identity over UDS) passed; its clone
  quirks (a receive filter that only `ATZ`/`ATAR`/`ATD` clear) are documented and
  pinned by a test.

### Fixed
- Block reads no longer return short images or shift later addresses when a
  module answers with fewer bytes than asked (slcan, ESP32, K-line bridge, KKL,
  ELM327 K-line drivers).
- A DSG measuring-group answer with no whole value is reported as an error
  instead of an empty group.

## [0.5.0] - 2026-11-18

First signed public release: desktop app (Linux AppImage/.deb, Windows installers)
with signed auto-update, and the `opentune` CLI for Linux, Windows and macOS. See
the [README](README.md) for the feature set at that point.

[Unreleased]: https://github.com/ebjerk01/opentune/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/ebjerk01/opentune/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/ebjerk01/opentune/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/ebjerk01/opentune/releases/tag/v0.5.0
