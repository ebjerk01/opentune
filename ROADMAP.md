# OpenTune Roadmap

> ⚠️ **Status: early alpha.** The offline tooling works on a `.bin`; the apps are
> pre-release alpha builds and the hardware path is unproven on a car. Don't flash
> a real vehicle yet. Expect breaking changes.

**Where it stands:** the **offline toolkit** — identify, checksums, map viewing/editing,
calibration-definition import/export, diffing, and the CLI + desktop apps — works today on a
flash image and is largely feature-complete. The **frontier now is getting it onto a real car**:
the interface drivers are written but unproven on hardware, and the live read/flash path waits on
an adapter. Android and open hardware are the reach goals. **Phase 4** then broadens coverage —
more checksum families (ME7, TCU), flash-container decode, real DoIP, and transmission/AWD control
units — most of it buildable offline; see the **build order** at the end.

**Legend:** `[x]` done · `[~]` in progress · `[ ]` planned · 🔒 needs hardware

This is a living document; checked = done and on `main`.

---

## Phase 1 — Offline toolkit (no car needed) — ≈ complete
Everything here operates on a flash `.bin`.

### Core, CLI & project
- [x] Rust workspace; `opentune-core` (Interface driver trait, protocol layer, ECU identity, flash orchestrator, safety rules); driver registry; `mock`/file driver
- [x] **`opentune` CLI** — `identify · info · read · dump · diff · findmaps · compare · checksum verify|fix · flash` (mock, `+ --resume/--transcript`) `· vault · ecudefs · import-xdf · export-xdf · import-a2l · export-a2l · vin · decode-part · scan · dtc`
- [x] GPL-3.0, README, CI (fmt + clippy + test + build), core unit tests

### Checksums (anti-brick: `correct` refuses to guess) — *see the coverage matrix*
- [x] `ChecksumAlgo` engine (verify + correct) + generic CRC32 helper
- [x] **EDC16 (MPC56x), EDC17 (TriCore, + `--reference` bootstrap), Simos PCR2.1, VAG EDC15** — all solved and validated against real dumps
- [ ] **Mercedes EDC15C6 (CR1)** — *not derivable from the application dump (proven); module refuses.* Full clean-room C166/C167 Ghidra RE completed (keyhana module, 540 functions seeded from genuine call targets). The **block format is fully characterized** — each checksummed block = 16-bit magic (`0x5555` cal-dir / `0xA5A5` code-patch / `0xAAAA` program-main) + data + halfword `0xC058` + 32-bit LE checksum (`0xC3` fill); 15 slots, full oracle captured (e.g. `[0x94000,0xe3ffc)→60A63BFD`). But **no resident checksum compute loop exists in the flash**: every C166 far-flash-read mechanism was searched (only 2 EXTS/EXTP+accumulate loops, both engine-control; the one SHA-1-like hash is the seed/key over RAM, off-limits). This matches Bosch EDC15 architecture — the block checksum is verified by the **tester-uploaded flash programming driver** during reflash, which is **not present in the application flash**. All standard algorithms (sum16/32, xor, CRC16/32 + poly scan, Adler-32, Fletcher-32, rotate-accumulate, and the VAG EDC15 loop) were **empirically ruled out across ≥3 independent blocks**. **To solve:** obtain the Bosch EDC15 flash programming driver / flashdaten for ECU `1037351686`, or dynamically capture the verify on a bench ECU during a reflash. Module **refuses** Mercedes (never guesses). Findings: `/opt/nvme-fw/edc15-cleanroom/work/edc15_RE_FINDINGS.txt` (217 lines).

### Maps & calibration
- [x] Map-definition format (`docs/MAP-DEFS.md`) + **real EDC16U34 map library** validated vs dumps
- [x] **Map-aware compare** (map-by-map, physical units) · byte/region diff · auto map-finder (`findmaps`)
- [x] **TunerPro XDF and ASAM A2L import + export** — both round-trip validated; A2L validated at scale on real EDC15 / Siemens-PPD / BMW-MSV80 (thousands of maps), with STD_AXIS address computation and cuboid slicing

### Identification
- [x] `opentune identify` + ECU-def `identify` keys + a **1034-entry catalog** (998 opendbc + 36 hand-curated European entries: VAG/Mercedes/BMW/Ford/PSA/Opel/Fiat/Volvo/Renault, with VAG part-number match keys)
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
| **EDC15C6** (Merc CR1) | Mercedes CDI (OM6xx) | ❌ not in app flash (needs flash driver) | def | — | K-line |

The VAG *signed* high-entropy EDC16 block is a separate, unsolved scheme. Checksum work derived
from proprietary tools is re-derived and validated independently before it ships.

---

## Phase 2 — On the car 🔒 (hardware-gated — the frontier)
The code is written; **nothing here is proven on hardware yet.** Unblocks when an interface +
bench ECU are available.

- [x] Protocol layer: ISO-TP (BS/STmin/wait/overflow hardened), UDS, KWP2000, **VW TP2.0**
- [~] **Interface drivers implemented, awaiting on-car test** — OpenPort 2.0 (J2534/CDC-ACM), SocketCAN, slcan, ELM327, KKL 409.1 (raw K-line), Arduino K-line bridge, ESP32-S3, **generic J2534 PassThru** (Mongoose/VNCI/SVCI; Windows FFI gated), **gs_usb/candleLight USB-CAN**, **ELM327-over-WiFi/Bluetooth**
- [ ] Verify cheap/clone adapters on real hardware (OpenPort/ELM327 clones, CANable) — `docs/CHEAP-INTERFACES.md`
- [ ] **Live identify + full OBD read** (EDC16 first)
- [~] **Live DTC read/clear** (OBD mode 0x03/0x07, UDS 0x19) — decoder done, needs the live transport
- [x] Battery-voltage read exposed as a safety gate
- [x] Security access: `SeedKey` trait + the 0x27 handshake wired into **every driver's read/write path** — refuses to write unlocked when security is required but no algorithm is set (user supplies the algorithm)
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
- [~] **ECU-library scanner** (`opentune-scan` crate + `opentune scan` CLI + the desktop Analyze-bin) — structural ECU-family detection + string/part-number/VIN extraction as metadata enrichment; **not** reliable auto-sort-by-car (needs better vehicle DBs — measured ~11% confident full-car on a messy library; the growing Euro catalog helps)
- [~] **Signed, reproducible releases** — Linux portable build + macOS/Windows bundle targets done; **Tauri updater signing key generated + wired** (needs the private key as a CI secret); Gradle wrapper committed; a synthetic demo `.bin` ships in `samples/`. App/installer code-signing (Apple/Windows certs) still pending.
- [x] Protocol-layer CI coverage — 20 ISO-TP / TP2.0 / KWP edge-case tests; `docs/CONTRIBUTING-ECUDEFS.md` for community defs

---

## Phase 4 — Coverage & scope expansion (reference-driven)
Concrete tasks drawn from a survey of 43 open-source ECU/tuning projects (2026-11-16; full ranked
notes in `docs/IDEAS.md`). Licenses were checked for GPL-3.0 reuse — repos marked **MIT/BSD** are
permissive (liftable with attribution); **GPL-3.0** combine fine; non-OSS repos are reference-only.
All `[ ]` = planned.

### New checksum families
- [ ] **ME7** (Bosch Motronic petrol; VAG/Porsche) — ROMSYS 16-bit word-sums + CAL **CRC32** +
  multipoint block descriptors, auto-located via the part-number block. Ref: **nyetlabs/ME7Sum** (BSD, C — lift directly).
- [ ] **TCU checksums** — CRC32-VW / CRC-CCITT (DSG), **JAMCRC** (DQ250). Refs: **dspl1236/vag-tcu-tools** (MIT), **TheFlashBold/DQ250-MQB-Bench-Flash** (GPL-3).
- [ ] Cross-validate our EDC15/EDC16 correctors against a native-Rust implementation. Ref: **LeZed97/ZedSuite** (GPL-3, Rust — directly liftable).
- [ ] Concrete VAG algos to compare/absorb: CRC32-VW, ECM3 64-bit summation, Simos counter-XOR. Ref: **dspl1236/simos-suite** (MIT; verify vs known-good dumps — AI-assisted).

### Seed/key (security access) — new plugin layer
- [ ] Ship an optional **SA2 seed/key** plugin set (user-enabled, never auto-applied — keeps the "you supply the algorithm" stance): **Daimler/Mercedes** (Standard/Mod/RefG), VW SA2+VGS, Bosch/Conti, Delphi. Ref: **jglim/UnlockECU** (MIT, C# → port to Rust). Daimler algos also feed the EDC15C6 attack path below.

### Flash-container decode — new
- [ ] **FRF / ODX-F / SGO** unpack+repack: LZSS10 + AES + SGO parsing (DQ200→AL1000, Simos). Refs: **lsdlsd88/TCU-FRF-SGO-UNPACKER** (MIT), **vag-tcu-tools** (MIT), **bri3d/VW_Flash** (BSD). ⚠️ never bundle recovered keys (`frf.key` legal caution).
- [ ] **a2l2xdf** converter path (A2L→XDF). Ref: VW_Flash.
- [ ] **WinOLS `.ols`** import. Refs: ZedSuite, **TheFlashBold/tune-editor** (GPL-3).

### DoIP — upgrade mock → real
- [ ] Real **DoIP (ISO-13400)** transport: TCP core + routing activation + vehicle discovery (we currently have a mock round-trip only). Refs: **zeloscloud/zelos-extension-uds** (MIT), **life-challenge/DiagTools** (MIT).
- [ ] Validate our UDS state machine against **pylessard/python-udsoncan** (MIT) service-by-service.

### Drivetrain control units — TCU + AWD/4WD (new target class)
See `docs/IDEAS.md` "Drivetrain control units" for the per-brand target list. Each needs its own
defs + checksum; the flash/UDS transport is shared.
- [ ] **VAG DSG/TCU** (Temic DQ250/DQ200/DQ381/DL382) defs + checksum + bench flash. Refs: VW_Flash, DQ250-MQB-Bench-Flash, vag-tcu-tools.
- [ ] **Haldex AWD** controller defs + checksum (Gen4/5 BorgWarner).
- [ ] **Mercedes 722.6 (NAG1) TCU** — control logic + maps (shift profiles, clutch-fill adaptation, per-gear TCC pressure) from **rnd-ash/ultimate-nag52-fw** (GPL-3, C++ — mine the logic/maps); reusable **Rust** crates (`egs_can`, `diag_common`, `flasher`, KWP2000-over-CAN `bootloader`) from **ultimate-nag52-fw-rs** / **-config-app** (GPL-3 — directly liftable).
- [ ] Generalize the controller-def schema so BMW ZF 8HP, Ford, Mitsubishi, Volvo TCUs/AWD drop in.

### EDC15C6 — open-source attack path 🔒 (supersedes "parked")
The checksum isn't in the application dump (proven); it runs in the tester-uploaded flash
programming driver. Concrete route to obtain/observe it:
- [ ] (1) **Daimler SA2 seed/key** → enter programming mode: **jglim/UnlockECU**.
- [ ] (2) **C167/ST10 bootstrap-mode** read of internal ROM/flash (has EDC15VM/P+ chip-select presets): **EcuProg7/C167BootTool** (GPL-3) + **NefMoto** C167/C166 loaders (GPL-3).
- [ ] (3) understand the **RAM flash-kernel** that computes the checksum: **fenugrec/npkern** (GPL-3) as the architectural model.
- [ ] (4) cross-reference adjacent-family real dumps/maps/seed-key: **Jeep WJ 2.7 CRD** repo (Bosch **EDC15C2**, not C6 — a data point, not a direct solve).

### Hardware 🔒 (mostly)
- [ ] Adopt **Switchleg1/esp32-isotp-ble-bridge** (MIT) as the companion ESP32-S3 BLE/ISO-TP firmware instead of writing it from scratch. *(porting is offline; compile/flash/on-bench is 🔒)*
- [ ] Reference ECUFlash/RomRaider definition parsers from **FastECU** (GPL-3) for broader def import. *(offline)*

---

## Suggested build order
Ranked by value, liftability and dependencies. **Offline** items need only a `.bin` and build today;
**🔒** items wait on an interface + bench ECU.

**Offline — buildable now**
1. **ME7 checksum** — a whole new petrol family; `ME7Sum` (BSD) is directly liftable, validatable against dumps. Biggest coverage win.
2. **FRF / ODX / SGO flash-container decode** — foundational for all VAG/DSG work (lets us open real flash containers); MIT refs.
3. **TCU checksums** (CRC32-VW, CRC-CCITT, JAMCRC) — prerequisite for DSG/TCU; small and self-contained.
4. **WinOLS `.ols` import + `a2l2xdf`** — definition-format breadth; feeds the map pipeline.
5. **Cross-validate EDC15/EDC16 vs ZedSuite (Rust)** — cheap hardening/confidence of what already ships.
6. **Validate UDS state machine vs `python-udsoncan`** — offline conformance of the protocol layer.
7. **Real DoIP (ISO-13400) transport** — code + mock-testable now; unblocks BMW/VAG-Ethernet later.
8. **Generalized controller-def schema** — the data model that lets TCU/AWD units drop in.
9. **Drivetrain defs** — VAG DSG + Haldex + Mercedes 722.6 maps/defs/checksums (the *def* groundwork; flashing is 🔒).
10. **CBFParser port → Mercedes diagnostics** — CBF → DTC/diagnostic defs (OpenVehicleDiag, GPL-3 Rust).
11. **SA2 seed/key plugin port** — user-supplied, never auto-applied (sensitive; careful scoping).

**🔒 Hardware-gated — later**
- Live identify + full OBD read (EDC16 first) · live DTC read/clear
- Flash path end-to-end on the bench, then in-car (EDC16 first)
- Verify cheap/clone adapters on real hardware
- DSG / Haldex / 722.6 **bench flashing**
- ESP32-S3 compile/flash + KiCad board; flash the Switchleg firmware
- EDC15C6 attack path (bootstrap / flash-driver / bench capture)
- Android USB-OTG / BLE adapter support
