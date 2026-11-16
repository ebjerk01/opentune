# OpenTune Roadmap

> ⚠️ **Status: early alpha.** The offline tooling works on a `.bin`; the apps are
> pre-release alpha builds and the hardware path is unproven on a car. Don't flash
> a real vehicle yet. Expect breaking changes.

**Where it stands:** the **offline toolkit** — identify, checksums, map viewing/editing,
calibration-definition import/export, diffing, and the CLI + desktop apps — works today on a
flash image and is largely feature-complete. The **frontier now is getting it onto a real car**:
the interface drivers are written but unproven on hardware, and the live read/flash path waits on
an adapter. Android and open hardware are the reach goals.

**Legend:** `[x]` done · `[~]` in progress · `[ ]` planned · 🔒 needs hardware

This is a living document; checked = done and on `main`.

---

## Phase 1 — Offline toolkit (no car needed) — ≈ complete
Everything here operates on a flash `.bin`.

### Core, CLI & project
- [x] Rust workspace; `opentune-core` (Interface driver trait, protocol layer, ECU identity, flash orchestrator, safety rules); driver registry; `mock`/file driver
- [x] **`opentune` CLI** — `identify · info · read · dump · diff · findmaps · compare · checksum verify|fix · flash` (mock, `+ --resume/--transcript`) `· vault · ecudefs · import-xdf · export-xdf · import-a2l · vin · decode-part · dtc`
- [x] GPL-3.0, README, CI (fmt + clippy + test + build), core unit tests

### Checksums (anti-brick: `correct` refuses to guess) — *see the coverage matrix*
- [x] `ChecksumAlgo` engine (verify + correct) + generic CRC32 helper
- [x] **EDC16 (MPC56x), EDC17 (TriCore, + `--reference` bootstrap), Simos PCR2.1, VAG EDC15** — all solved and validated against real dumps
- [~] **Mercedes EDC15C6 (CR1)** — clean-room constant recovery in progress; the module refuses until validated (never a guess)

### Maps & calibration
- [x] Map-definition format (`docs/MAP-DEFS.md`) + **real EDC16U34 map library** validated vs dumps
- [x] **Map-aware compare** (map-by-map, physical units) · byte/region diff · auto map-finder (`findmaps`)
- [x] **TunerPro XDF import + export** (round-trip validated) and factory **ASAM A2L import** (`opentune-a2l`; validated at scale on real EDC15 / Siemens-PPD / BMW-MSV80, thousands of maps)

### Identification
- [x] `opentune identify` + ECU-def `identify` keys + **998-car opendbc catalog** + hand-curated **Mercedes/BMW** entries
- [x] **VIN / VAG+Bosch part-number decoder** (`opentune-vin`)
- [ ] 🔒 read the real identity DIDs live from an ECU

### Safety, flash orchestration & diagnostics
- [x] Flash orchestrator state machine (voltage gate → verified backup → write → read-back verify)
- [x] **Resumable / recoverable flash** — chunked, SHA-256-checkpointed; resume without re-writing verified chunks, never apply the wrong payload
- [x] **Backup vault** (auto-archived with ECU id + CRC32; `vault` CLI lists/verifies) + **structured session transcript** (`.log`/`.jsonl`)
- [x] **DTC database + decoder** — 11,140 codes (`opentune dtc`)

### Apps
- [x] **Desktop (Tauri)** — identify, checksum verify/fix, findmaps, map viewer **+ editor** (edit/interpolate/smooth, write-back + checksum fix), **compare**, flash (mock), vault, offline guide
- [x] **Portable Linux builds** — glibc baseline fixed (`ubuntu-22.04` CI + a `debian:bookworm` Docker recipe in `desktop/portable-build/`); alpha AppImage + `.deb`
- [x] **Android `opentune-ffi`** — UniFFI binding over the core (compile-verified; Kotlin bindings generated) + a Jetpack Compose app (`android/`)
- [~] Desktop auto-update from GitHub Releases (configured; needs a signing key)
- [x] **Android APK built + verified on real hardware** (SM-P600): installs, launches to the UI, the UniFFI native lib loads & round-trips into the Rust core — no crash. Debug/alpha build.

---

## ECU coverage matrix

| Family | Example engines | Checksum | Identify | Real maps | Bus |
|---|---|---|---|---|---|
| **EDC16** (MPC56x) | VAG 1.9/2.0 TDI PD, BMW/KIA | ✅ solved | ✅ keys | ✅ EDC16U34 | OBD/CAN |
| **EDC17** (TriCore) | later CR diesels incl. VAG | ✅ solved (+`--reference`) | def | — | OBD (bench/boot for full read) |
| **Simos PCR2.1** | VAG 1.6 TDI CR | ✅ solved | def | — | OBD (bench/boot) |
| **EDC15** (VAG TDI) | VAG 1.9 TDI | ✅ solved (clean-room) | — | — | K-line/OBD |
| **EDC15C6** (Merc CR1) | Mercedes CDI (OM6xx) | 🔄 in progress | def | — | K-line |

The VAG *signed* high-entropy EDC16 block is a separate, unsolved scheme. Checksum work derived
from proprietary tools is re-derived and validated independently before it ships.

---

## Phase 2 — On the car 🔒 (hardware-gated — the frontier)
The code is written; **nothing here is proven on hardware yet.** Unblocks when an interface +
bench ECU are available.

- [x] Protocol layer: ISO-TP (BS/STmin/wait/overflow hardened), UDS, KWP2000, **VW TP2.0**
- [~] **Interface drivers implemented, awaiting on-car test** — OpenPort 2.0 (J2534/CDC-ACM), SocketCAN, slcan, ELM327, KKL 409.1 (raw K-line), Arduino K-line bridge
- [ ] **Generic J2534** driver (Mongoose/VNCI/SVCI clones) + verify cheap/clone adapters (OpenPort/ELM327 clones, CANable) — `docs/CHEAP-INTERFACES.md`
- [ ] **Live identify + full OBD read** (EDC16 first)
- [~] **Live DTC read/clear** (OBD mode 0x03/0x07, UDS 0x19) — decoder done, needs the live transport
- [x] Battery-voltage read exposed as a safety gate
- [x] Security access: `SeedKey` trait + the 0x27 handshake wired into the drivers (user supplies the algorithm)
- [ ] **Flash path**: `requestDownload`/`transferData`/`transferExit` → calibration-region write → read-back verify → **EDC16 end-to-end on the bench, then in-car**

---

## Phase 3 — Reach & ecosystem
- [~] **Android app on a phone** — APK built + verified running on a real device (above); USB-OTG / BLE adapter support still needs hardware
- [~] **Open hardware interface** — ESP32-S3 **firmware written** (`hardware/firmware/esp32-s3`: TWAI/CAN + ISO-TP + K-line + framed USB-CDC protocol; not yet compiled/flashed) **and its Rust host driver** (`crates/drivers/esp32-opentune`, 22 tests); Arduino K-line bridge firmware also written. Remaining: compile/flash + a KiCad board + on-bench test.
- [x] **CAN-FD** (+ ISO-TP-FD) **and DoIP** transport (`opentune-core`; ISO 15765-2:2016 FD + ISO 13400) — offline/mock-tested (incl. a localhost TCP DoIP round-trip); untested on real CAN-FD/DoIP silicon
- [ ] **More ECU families** + a growing **community ECU-definition & map library** (contribution format + review)

---

## Cross-cutting / tooling
- [x] Documentation — architecture, driver authoring, ECU defs, checksums, hardware, Android, ideas backlog (`docs/`)
- [~] **ECU-library scanner** (`opentune-scan`, prototyped) — structural ECU-family detection + string/part-number/VIN extraction as metadata enrichment; **not** reliable auto-sort-by-car (that needs better vehicle DBs — measured ~11% confident full-car on a messy library)
- [ ] **Signed, reproducible releases** for Linux / Windows / macOS (Linux portable build done; Windows + macOS + updater signing pending)
- [ ] More test fixtures + protocol-layer CI coverage (ISO-TP / TP2.0 / KWP edge cases)
