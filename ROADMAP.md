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

**Suite direction (Phase 5):** OpenTune is growing from a tuning toolkit into a full **diagnostic
& service suite** — one local app that does, over any interface: identify → auto-scan → fault codes
→ live data → **coding** → **adaptation** → service functions → **flashing** → tuning (ForScan +
VCDS + OpenPort + WinOLS in one). It ships as **two local builds**: a **public** (clean-room, open
standards, user-supplied keys) and a **private "Bench"** (public *plus* brand seed-keys, private
checksums and protected definition packs). Every feature is a *service* on the shared protocol
layer, so it works across every interface; new vehicles are new **data** (defs/label packs), not
new code. See **Phase 5** below.

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
| **ME7** (Motronic) | VAG/Porsche petrol | ✅ real-image | def | — | OBD/CAN |
| **DQ381 TCU** (Bosch) | VAG 7-sp wet DSG | ✅ CAL, real-image | def | — | OBD/CAN |
| **DQ250 TCU** (MQB) | VAG 6-sp DSG | 🔄 JAMCRC, synthetic-only | — | — | OBD/CAN |
| **ZF-8HP TCU** | BMW/VAG 8-sp auto | ⚠️ CRC-CCITT primitive only | — | — | OBD/CAN |

**Validation states** (distinct — a primitive existing ≠ a family supported): *implemented* → *synthetic-tested* →
*real-image-tested* (verify==stored + correct round-trip on a genuine dump) → 🔒 *bench-tested* → 🔒 *vehicle-tested*.
Nothing here is past *real-image*. "✅ real-image" means the checksum is validated on a real dump; it does **not**
authorise flashing. The VAG *signed* high-entropy EDC16 block is a separate, unsolved scheme. Checksum work derived
from proprietary tools is re-derived and validated independently before it ships.

---

## Phase 2 — On the car 🔒 (hardware-gated — the frontier)
The code is written; **nothing here is proven on hardware yet.** Unblocks when an interface +
bench ECU are available.

- [x] Protocol layer: ISO-TP (BS/STmin/wait/overflow hardened), UDS, KWP2000, **VW TP2.0**
- [~] **Interface drivers implemented, awaiting on-car test** — OpenPort 2.0 (J2534/CDC-ACM), SocketCAN, slcan, ELM327, KKL 409.1 (raw K-line), Arduino K-line bridge, ESP32-S3, **generic J2534 PassThru** (Mongoose/VNCI/SVCI; Windows FFI gated), **gs_usb/candleLight USB-CAN**, **ELM327-over-WiFi/Bluetooth**
- [ ] Verify cheap/clone adapters on real hardware (OpenPort/ELM327 clones, CANable) — `docs/CHEAP-INTERFACES.md`
- [~] **ELM327 OBD read driver** (`opentune-driver-elm327-obd`, CLI `elm327obd`) — serial ELM init (HS-CAN `SP6` / MS-CAN `SPB` + physical switch), ISO-TP, and a UDS read client: live identify (VIN 0xF190 + part/HW/SW/supplier DIDs), DTC read (0x19 sub 0x02) + decode, clear (0x14), OBD mode-01 live PIDs (RPM/coolant/speed) + mode-03 fallback, battery (`ATRV`). 15 tests vs scripted ELM responses; **read-only** (no ECU writes). *Adapter verified live on hp-elite-x2; driver not yet exercised on a car.*
- [~] **Live identify + full OBD read** (EDC16 first) — read path now exists in the ELM327 driver above; needs on-car verification
- [~] **Live DTC read/clear** (OBD mode 0x03/0x07, UDS 0x19) — decoder + driver read path done, needs a live bus
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

### New checksum families — shipped (see validation states above)
- [x] **ME7** (Bosch Motronic petrol; VAG/Porsche) — verify+correct **real-image-validated** (33/33 blocks
  on a real Passat ME7.5): ROMSYS 16-bit word-sums + multipoint block descriptors, located by data-independent
  signature. The CAL-CRC32 block is firmware-specific — exposed as an extension point, **not** auto-located (documented limit). Ref: nyetlabs/ME7Sum (BSD).
- [~] **TCU checksums** — **DQ381 CRC32-VW real-image-validated** (CAL block on a real dump). Remaining:
  expose **BOOT/ASW/CAL separately** (all three independently confirmed via cross-check) + extracted-block support.
  **DQ250 JAMCRC: synthetic-only** (no MQB bench dump found). **CRC-CCITT: generic primitive, NOT** established ZF-8HP
  family support. Refs: dspl1236/vag-tcu-tools (MIT), TheFlashBold/DQ250-MQB-Bench-Flash (GPL-3).
- [ ] **Representation-aware checksum reports** — distinguish raw full image / extracted programming block /
  container segment, and embedded-vs-transfer checksums, and *recognised* vs *merely mathematically consistent* input.
- [ ] **Importer fidelity (XDF)** before more TCU maps: checked rebasing (refuse below-base underflow, not wrap),
  preserve categorical shift/mode axis labels (or report loss), import omitted scalar constants, explicit
  unsupported-layout reports, and verify swap-XY (0x04) semantics before changing storage order.
- [ ] **Further family leads** (research-only until independent fixtures + provenance): Haldex Gen5 (descriptor-driven,
  validate layout before correction), Subaru M32R/SH7055/MH8104 (avoid the reproduced old-source carry/order bugs),
  Volvo/Aisin TF80-P2 (CRC-16/XMODEM) / AW55 (open) — keep each at its actual evidence level, no guessed correctors.
- [ ] Cross-validate our EDC15/EDC16 correctors against a native-Rust implementation. Ref: **LeZed97/ZedSuite** (GPL-3, Rust — directly liftable).
- [ ] Concrete VAG algos to compare/absorb: CRC32-VW, ECM3 64-bit summation, Simos counter-XOR. Ref: **dspl1236/simos-suite** (MIT; verify vs known-good dumps — AI-assisted).

### Seed/key (security access) — new plugin layer
- [ ] Ship an optional **SA2 seed/key** plugin set (user-enabled, never auto-applied — keeps the "you supply the algorithm" stance): **Daimler/Mercedes** (Standard/Mod/RefG), VW SA2+VGS, Bosch/Conti, Delphi. Ref: **jglim/UnlockECU** (MIT, C# → port to Rust). Daimler algos also feed the EDC15C6 attack path below.
- [ ] 🔒 **Ford/Mazda seed→key** (24-bit LFSR + per-module secret table) — algorithm recovered from the open-source FoCCCus; it is the write-enabler, so it lives in the **private Bench** build only. Needs **one live bench seed/key capture** (the ELM327 can do it) to bit-validate the LFSR port before use. Public build keeps the user-supplies-the-algorithm stance.

### Flash-container decode
- [x] **FRF / ODX / ODX-F / SGO** unpack (`opentune-container`): FRF rolling-XOR → ZIP → ODX/SGO; LZSS10 / LZSS-DQ / ZF-8HP LZZ decompress; AES-CBC / repeating-XOR / SGO family-cipher block crypto — **caller supplies every key, none bundled** (`frf.key` legal caution respected). Refs: **lsdlsd88/TCU-FRF-SGO-UNPACKER** (MIT), **vag-tcu-tools** (MIT), **bri3d/VW_Flash** (BSD).
- [x] **VBF (Ford / Volvo / Mazda Versatile Binary Format)** (`opentune-container::vbf`) — plaintext module-flash container: comment-aware header parse + typed fields, binary blocks, **per-block CRC-16/CCITT-FALSE + CRC-32 `file_checksum` verify** (const tables, standard check-vectors). Validated against **33 real Ford module VBFs** (1–6 blocks each). Pairs with the ELM327/Ford suite work. Key-free.
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

## Phase 5 — Full diagnostic & service suite (public + private Bench)
Turn the toolkit into a complete suite. Every pillar is a **service engine** (code, written once)
over the existing protocol layer + **data-defined packs** (defs/labels/scripts), so brands/vehicles
drop in as data. Engines are **public**; sensitive data (brand seed-keys, protected label/map packs)
lives in the **private Bench** build. Most engines are buildable + unit-testable offline; the live
paths are 🔒 (need the car/adapter we now have).

### Coding & adaptation
- [~] **Module coding engine** (`opentune-coding`) — data-defined coding-label sets (byte/bit→label with a documented LSB-first convention), `decode` / `get` / `set` (range + enum-option validated) / before-after **param & byte diff**, and a **safe write-back** over UDS 0x22/0x2E: read-first backup → write only changed blocks → **read-back verify**, plus **dry-run** plans and a `requires_security` flag. Generic engine + 7 unit tests done (scripted transport). Next: adapt to core's `DiagTransport`, CLI/desktop surface, and **brand label packs** (VAG long-coding, Ford As-Built/CCC via `ccc_descr.xml`).
- [ ] **Adaptation / basic settings** — adaptation channels, `RoutineControl` (0x31), guided functions; data-defined channel maps; the 0x27 login gate reuses the existing `SeedKey` layer.
- [ ] **Actuator / output tests** — `InputOutputControlByIdentifier` (0x2F).

### Service & diagnostics
- [ ] **Service functions** — declarative, safe UDS scripts (session → security → routine → verify): EPB, DPF regen, oil/service reset, SAS calibration, throttle align, injector coding, battery registration, brake bleed. Script library grows as data.
- [ ] **Full-vehicle auto-scan** — enumerate all modules, read every module's DTCs + identity (ForScan/VCDS-style), freeze-frame + readiness.

### Live data & logging
- [ ] **Datalogging engine** — periodic live data to file + dashboards/graphs + triggers, over OBD mode-01, UDS ReadDataByIdentifier (0x22 periodic), **UDS `mode23`/0x23 address logging**, and **Subaru SSM** — modelled on the Tactrix OpenPort logger (PID sorting, multi-request chunking). Reuse **RomRaider `logger.xml`** param definitions (data-defined, like coding). Ref: **RomRaider** (GPL-2), **FreeSSM** (GPL-3).

### Flashing to modules (not just engine ECUs)
- [ ] **UDS module flash** end-to-end — `0x34`/`0x36`/`0x37` + security + erase/checksum `RoutineControl`, **container-driven** (VBF/ODX/FRF/SGO/bin → RequestDownload address/length), behind the existing **flash-safety policy** (voltage → backup → write → read-back verify → recovery). 🔒 Ford module flashing (VBF + Bench seed-key) is the first concrete target.
- [ ] 🔒 **Validate the J2534 drivers on an OpenPort 2.0** — `opentune-driver-j2534`/`j2534-openport` are written but hardware-unproven; the Tactrix OpenPort 2.0 is the reference PassThru box and the right interface for real engine-ECU flashing (vs the read-only ELM327). Target: EDC16/Subaru SH flash end-to-end on the bench.
- [ ] **Renesas SH boot mode** read/write (SH7055/SH7058/**SH725xx**) — bench/boot access for Subaru/Mitsubishi *and* the SH725xx DSG (DQ381) TCU. 🔒 bench. Refs: **fenugrec/npkern** (GPL-3, SH RAM kernel), EcuFlash SH boot-mode behavior, NefMoto loaders.

### Frontends (all local — no server)
- [ ] Wire every pillar into the **Tauri desktop** app as the suite's primary face, with **guided wizards** (coding/adaptation/service/flash) + live dashboards; **Android** companion over USB-OTG/BLE; **CLI** for automation. Public **OpenTune** AppImage + private **Bench** AppImage.

### Definition ecosystem (the scaling lever)
- [ ] New data formats — **coding-label** (done, in `opentune-coding`), **adaptation-channel map**, **service-function script**, **seed-key DB** (private). Public **community** pack + private **premium** pack, versioned files the app loads locally (no hosting).
- [~] **EcuFlash / RomRaider XML definition importer** (`opentune-ecuflash`, CLI `import-ecuflash`) — parses `<rom>` identity + reusable `<scaling>` + 1D/2D/3D `<table>` with axes → OpenTune `MapDef`s, `toexpr` fitted to a linear factor/offset via the XDF math (non-linear flagged + imported pre-scaled, never a silent wrong scale); fidelity warnings for unmerged `<include>` base defs, unknown `storagetype`, missing scaling refs. 6 tests + fixture-gated real-def test. **Next:** `<include>` base-def merging, RomRaider attribute variants, `logger.xml` params (feeds the logging engine). Unlocks the community **Subaru/Mitsubishi** map library. Parse the open **format** + community-licensed defs; don't lift EcuFlash code. Refs: **RomRaider** (GPL-2), **FastECU** (GPL-3).
- [ ] **Subaru / Mitsubishi seed-key** tables as an optional user-enabled plugin (published tables; Bench), same stance as the Ford/Daimler entries above.

---

## Suggested build order
Ranked by value, liftability and dependencies. **Offline** items need only a `.bin` and build today;
**🔒** items wait on an interface + bench ECU.

**Offline — buildable now**
1. **ME7 checksum** — a whole new petrol family; `ME7Sum` (BSD) is directly liftable, validatable against dumps. Biggest coverage win.
2. **FRF / ODX / SGO flash-container decode** — foundational for all VAG/DSG work (lets us open real flash containers); MIT refs.
3. **TCU checksums** (CRC32-VW, CRC-CCITT, JAMCRC) — prerequisite for DSG/TCU; small and self-contained.
4. **Definition-format breadth — EcuFlash/RomRaider XML + WinOLS `.ols` + `a2l2xdf`** — the EcuFlash/RomRaider importer unlocks the large community **Subaru/Mitsubishi** map + `logger.xml` set in one move (highest coverage win here, fully offline); `.ols` and `a2l2xdf` round out the map pipeline.
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
