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
VCDS + OpenPort + WinOLS in one). It's a single, local, open-source tool, built clean-room on open
standards: it ships no OEM keys or protected calibration data — sensitive seed/key algorithms are
caller-supplied, and anything that can't be implemented cleanly is left out of scope. Every feature
is a *service* on the shared protocol
layer, so it works across every interface; new vehicles are new **data** (defs/label packs), not
new code. See **Phase 5** below.

**Legend:** `[x]` done · `[~]` in progress · `[ ]` planned · 🔒 needs hardware

This is a living document; checked = done and on `main`.

---

## Phase 1 — Offline toolkit (no car needed) — ≈ complete
Everything here operates on a flash `.bin`.

### Core, CLI & project
- [x] Rust workspace; `opentune-core` (Interface driver trait, protocol layer, ECU identity, flash orchestrator, safety rules); driver registry; `mock`/file driver
- [x] **`opentune` CLI** — `drivers · identify · info · read · dump · diff · findmaps · compare · checksum verify|fix|algos · flash` (mock, `+ --resume/--transcript`) `· vault · ecudefs · import-xdf · export-xdf · import-a2l · export-a2l · a2l2xdf · import-ecuflash · import-ols · packs · vin · decode-part · scan · dtc · container · cbf · coding · adaptation · autoscan · service · log · simulate · obd · candump · db · recipe`; `checksum attest` derives the flash-policy attestation from a real verification (`opentune-attest`), never from a caller claim
- [x] **`opentune-image`** — addressed sparse-image model + Intel HEX / Motorola SREC read/write (checksums validated, holes and fill reported, never silently invented)
- [x] GPL-3.0, README, CI (fmt + clippy + test + build), core unit tests

### Checksums (anti-brick: `correct` refuses to guess) — *see the coverage matrix*
- [x] `ChecksumAlgo` engine (verify + correct) + generic CRC32 helper
- [x] **EDC16 (MPC56x), EDC17 (TriCore, + `--reference` bootstrap), Simos PCR2.1, VAG EDC15** — all solved and validated against real dumps
- [x] **Mercedes EDC15C6 (CR1)** — *solved (clean-room) and self-correcting.* Same loop structure as the public VAG EDC15 algorithm, Mercedes constants: seed `(0,0)` for every block, mid-loop add `0x3962`, finalisation `a += 0x8432`, `b ^= 0x4711`, stored LE as `(b<<16)|a`. Constants derived by exhaustive search against the owner's own stock and tuned dumps; exactly one parameter set reproduces all 15 slots of the Sprinter 2.2 CDI (`1037351686`) stock **and** ECU-flashed tuned image (incl. the four tune-changed calibration blocks) and all blocks of three further Mercedes stock/tuned pairs not used in the search (133/133 blocks). Block chain discovered from the data: each block = data (often opened by a `0x5555`/`0xA5A5`/`0xAAAA` magic) + halfword `0xC058` + 32-bit LE checksum ending on a 256-byte boundary; a block starts right after the previous stored word, or after fill at the next 4 KiB-aligned non-fill offset. Refuses unrecognised images and ambiguous block boundaries.

### Maps & calibration
- [x] Map-definition format (`docs/MAP-DEFS.md`) + **real EDC16U34 map library** validated vs dumps
- [x] **Map-aware compare** (map-by-map, physical units) · byte/region diff · auto map-finder (`findmaps`)
- [x] **Single-file map finder** (`opentune-mapfind`, `findmaps <image>`) — counted-table headers + axis pairs, scored candidates with evidence, hints worded as unverified; on the EDC16U34 stock read all defined maps are found, 91.5 % of tune-changed regions across 47 pairs are covered by a candidate. Approach credited to openremap-core (MIT), re-implemented.
- [x] **Portable tune recipes** (`opentune-recipe`, CLI `recipe create|apply|show`, format in `docs/RECIPES.md`) — anchored patches + source identity; `apply` refuses unless identity and every anchor match (explicit `--relocate` refuses ambiguous matches), then checksum-fixes and attests. Byte-identical on 24 of 45 real pairs; the rest are refused (never guessed).
- [x] **Log viewer + map overlay** (`opentune-logview`, desktop panel) — Live-log and generic CSV, zoom/pan plot with min/max decimation, cursor readout, per-window stats; overlay a log on a map as hits / time-in-cell / value heat grid, refusing unit mismatches unless accepted.
- [x] **TunerPro XDF and ASAM A2L import + export** — both round-trip validated; A2L validated at scale on real EDC15 / Siemens-PPD / BMW-MSV80 (thousands of maps), with STD_AXIS address computation and cuboid slicing

### Identification
- [x] `opentune identify` + ECU-def `identify` keys + a **1034-entry catalog** (998 opendbc + 36 hand-curated European entries: VAG/Mercedes/BMW/Ford/PSA/Opel/Fiat/Volvo/Renault, with VAG part-number match keys)
- [x] **VIN / VAG+Bosch part-number decoder** (`opentune-vin`)
- [x] **Signature-based recognition** (`opentune-scan` `recognize/`) — Bosch (ME7/ME9/MED9/EDC15/16/17, MED17/ME17/MD1/MG1, M7, MP, M1–M5 …), Siemens/Continental (SIMOS, SID, PPD, PCR2.1, MS4x, SIMTEC), Delphi, Magneti Marelli, Denso; each with a high/medium/low tier and byte evidence. Sample library: files with a known family 84 → 260 of 835, fixtures 133/133, 0 contested. Containers (DCM/DAMOS text, WinOLS projects, zip) are classified, never searched.
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
| **EDC16** (MPC56x) | VAG 1.9/2.0 TDI PD, BMW/KIA | ✅ solved | ✅ keys | ✅ EDC16U34 (18 maps) | OBD/CAN |
| **EDC17** (TriCore) | later CR diesels incl. VAG | ✅ solved (+`--reference`) | def | — | OBD (bench/boot for full read) |
| **Simos PCR2.1** | VAG 1.6 TDI CR | ✅ solved | def | 🔄 47 raw tables (diff-confirmed, no axes) | OBD (bench/boot) |
| **EDC15** (VAG TDI) | VAG 1.9 TDI | ✅ solved (clean-room) | — | — | K-line/OBD |
| **EDC15C6** (Merc CR1) | Mercedes CDI (OM6xx) | ✅ solved (clean-room) | def | 🔄 10 maps | K-line |
| **ME7** (Motronic) | VAG/Porsche petrol | ✅ real-image | def | — | OBD/CAN |
| **DQ381 TCU** (Bosch) | VAG 7-sp wet DSG | ✅ CAL, real-image | def | — | OBD/CAN |
| **DQ250 TCU** (MQB) | VAG 6-sp DSG | 🔄 JAMCRC, synthetic-only | — | — | OBD/CAN |
| **ZF-8HP TCU** (AL551) | BMW/VAG 8-sp auto | 🔄 CAL CRC-32, synthetic-only (`correct` needs a stock reference) | — | — | OBD/CAN |
| **Haldex Gen5** | VAG 4Motion AWD | 🔄 word-sum, synthetic-only | — | — | OBD/CAN |
| **Siemens MS41/42/43** | BMW petrol | — | def | ✅ imported (ms4x-net XDF, GPL-3) | OBD/K-line |

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
- [~] **ELM327 OBD read driver** (`opentune-driver-elm327-obd`, CLI `elm327obd`) — serial ELM init (HS-CAN `SP6` / MS-CAN `SPB` + physical switch), ISO-TP, and a UDS read client: live identify (VIN 0xF190 + part/HW/SW/supplier DIDs), DTC read (0x19 sub 0x02) + decode, clear (0x14), OBD mode-01 live PIDs (RPM/coolant/speed) + mode-03 fallback, battery (`ATRV`). 15 tests vs scripted ELM responses; **read-only** (no ECU writes). *First on-car session 2026-11-17 (auto-debug wizard): handshake, 13.2 V battery, live RPM + coolant PIDs **pass**; the transport-backed reads (supported PIDs, VIN, DTCs) failed with "no diagnostic transport" — a missing `Interface::diag()` override, fixed in `b0b7273`. Re-test pending with a rebuilt app.*
- [x] **Live-path hardening (2026-11-17, offline-tested)** — one shared ELM327 reply parser (`opentune_core::protocol::elm`: responsePending 0x78 in both adapter behaviours, truncated multi-frame rejected, `BUS BUSY`/`LV RESET`/`ERRxx`/`?` errors, adapter-reset detection, >7-byte CAN requests refused before sending); `Interface::diag()` on every driver; desktop **AdapterHub** (one adapter owner, app-wide busy lock, connection reuse); **per-module addressing** (scan/clear re-bind per module; writes, routines, actuators and service runs refuse an unknown target); writes run `10 03` → op → `10 01` in one connection with an explicit ack.
- [~] **Live identify + full OBD read** (EDC16 first) — read path now exists in the ELM327 driver above; needs on-car verification
- [~] **Live DTC read/clear** (OBD mode 0x03/0x07, UDS 0x19) — decoder + driver read path done, needs a live bus
- [x] Battery-voltage read exposed as a safety gate
- [x] Security access: `SeedKey` trait + the 0x27 handshake wired into **every driver's read/write path** — refuses to write unlocked when security is required but no algorithm is set (user supplies the algorithm)
- [ ] **Flash path**: `requestDownload`/`transferData`/`transferExit` → calibration-region write → read-back verify → **EDC16 end-to-end on the bench, then in-car**

---

## Phase 3 — Reach & ecosystem
- [~] **Android app on a phone** — APK built + verified running on a real device (above); USB-OTG / BLE adapter support still needs hardware
- [~] **Open hardware interface** — ESP32-S3 **firmware written** (`hardware/firmware/esp32-s3`: TWAI/CAN + ISO-TP + K-line + framed USB-CDC protocol; compile-only CI job on pinned ESP-IDF v5.3.2, never flashed) **and its Rust host driver** (`crates/drivers/esp32-opentune`, 27 tests; not yet in the CLI registry); Arduino K-line bridge firmware also written (compile-only CI). Remaining: flash + a KiCad board + on-bench test.
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
- [~] **ZF-8HP (AL551, method 0x22) CAL CRC-32** and **Haldex Gen5** per-block word-sum — implemented (refs: vag-tcu-tools MIT,
  VW_Flash BSD-2), **algorithm-vector + synthetic only** (no real dumps); ZF-8HP `correct` needs a validating stock reference.
- [ ] **Further family leads** (research-only until independent fixtures + provenance): Subaru M32R/SH7055/MH8104 (avoid the reproduced old-source carry/order bugs),
  Volvo/Aisin TF80-P2 (CRC-16/XMODEM) / AW55 (open) — keep each at its actual evidence level, no guessed correctors.
- [ ] Cross-validate our EDC15/EDC16 correctors against a native-Rust implementation. Ref: **LeZed97/ZedSuite** (GPL-3, Rust — directly liftable).
- [ ] Concrete VAG algos to compare/absorb: CRC32-VW, ECM3 64-bit summation, Simos counter-XOR. Ref: **dspl1236/simos-suite** (MIT; verify vs known-good dumps — AI-assisted).

### Seed/key (security access) — new plugin layer
- [~] **SA2 bytecode VM** shipped (`opentune-sa2`, implements `SeedKey` as `sa2`) — runs a caller-supplied SA2 program; **no OEM bytecode bundled**. Still open below: any optional per-brand plugin set.
- [ ] Ship an optional **SA2 seed/key** plugin set (user-enabled, never auto-applied — keeps the "you supply the algorithm" stance): **Daimler/Mercedes** (Standard/Mod/RefG), VW SA2+VGS, Bosch/Conti, Delphi. Ref: **jglim/UnlockECU** (MIT, C# → port to Rust). Daimler algos also feed the EDC15C6 programming-access follow-ups below.
- [ ] **Ford/Mazda module flashing** uses the optional, user-enabled `SeedKey` plugin layer — OpenTune ships no OEM seed/key algorithm; security access takes a caller-supplied key provider. The open-source FoCCCus (GPLv3) is a reference for the on-wire handshake format only.

### Flash-container decode
- [x] **FRF / ODX / ODX-F / SGO** unpack (`opentune-container`): FRF rolling-XOR → ZIP → ODX/SGO; LZSS10 / LZSS-DQ / ZF-8HP LZZ decompress; AES-CBC / repeating-XOR / SGO family-cipher block crypto — **caller supplies every key, none bundled** (`frf.key` legal caution respected). Refs: **lsdlsd88/TCU-FRF-SGO-UNPACKER** (MIT), **vag-tcu-tools** (MIT), **bri3d/VW_Flash** (BSD).
- [x] **VBF (Ford / Volvo / Mazda Versatile Binary Format)** (`opentune-container::vbf`) — plaintext module-flash container: comment-aware header parse + typed fields, binary blocks, **per-block CRC-16/CCITT-FALSE + CRC-32 `file_checksum` verify** (const tables, standard check-vectors). Validated against **33 real Ford module VBFs** (1–6 blocks each). Pairs with the ELM327/Ford suite work. Key-free.
- [x] **a2l2xdf** converter path (A2L→XDF, with a loss report). Ref: VW_Flash.
- [x] **WinOLS `.ols`** import (`opentune-ols`: ROM + maps; CLI `import-ols`). Refs: ZedSuite, **TheFlashBold/tune-editor** (GPL-3).

### DoIP — upgrade mock → real
- [~] Real **DoIP (ISO-13400)** transport: `DoIpTransport` client exercised against `opentune-udssim`'s loopback DoIP entity (routing activation, acks, interposed 0x78) — untested on a real vehicle; vehicle discovery still open. Refs: **zeloscloud/zelos-extension-uds** (MIT), **life-challenge/DiagTools** (MIT).
- [~] Validate our UDS state machine against **pylessard/python-udsoncan** (MIT) service-by-service — **`opentune-udssim`** conformance server (ISO 14229 catalogue, cross-checked vs udsoncan + iso14229, no code copied) with fault injection; coding/adaptation/servicefn/autoscan/logdef engines tested against it. CLI `simulate`.

### Drivetrain control units — TCU + AWD/4WD (new target class)
See `docs/IDEAS.md` "Drivetrain control units" for the per-brand target list. Each needs its own
defs + checksum; the flash/UDS transport is shared.
- [~] **Haldex Gen4** (BorgWarner, e.g. VW Passat CC 4Motion) — definitions + checksum + ECU-database entry, read-only first; validation = a real Gen4 read on the owner's car (joins the first car session). *(in progress)*
- [~] **OpenHaldex** support — a backend + CLI (panel after the desktop split) that talks to an OpenHaldex unit: lock modes, status, CAN log; clean-room from its docs and CAN behaviour (licence reported as custom: checked before reusing anything). Longer term: a Haldex controller mode on the OpenTune ESP32 interface (dual CAN, passthrough or modified lock request). Refs: openhaldex.pages.dev, Forbes-Automotive/OpenHaldex-C6, ABangingDonk/OpenHaldexDue + OpenHaldexT4. *(in progress)*
- [ ] **Haldex Gen5** word-sum — validate on a real dump when one appears (synthetic-only today).
- [~] **VAG DSG** definitions + map layouts — **DQ250 PQ `02E` first** (the owner's Passat CC 4Motion, if DSG), then DQ250 MQB, DQ200 (`0AM`), DL382; flash over the UDS module-flash path once that is validated. Refs: dspl1236/vag-tcu-tools (MIT), TheFlashBold/DQ250-MQB-Bench-Flash (GPL-3), bri3d/VW_Flash, bri3d/mqb-flash-rs. *(in progress)*
- [ ] **DQ250 JAMCRC** — validate on a real dump (synthetic-only today). DQ381 boot reads + TCU seed/key research stay in the private Bench plugin.
- [ ] **Mercedes 722.6 (NAG1) TCU** — control logic + maps (shift profiles, clutch-fill adaptation, per-gear TCC pressure) from **rnd-ash/ultimate-nag52-fw** (GPL-3, C++ — mine the logic/maps); reusable **Rust** crates (`egs_can`, `diag_common`, `flasher`, KWP2000-over-CAN `bootloader`) from **ultimate-nag52-fw-rs** / **-config-app** (GPL-3 — directly liftable).
- [ ] Generalize the controller-def schema so BMW ZF 8HP, Ford, Mitsubishi, Volvo TCUs/AWD drop in.

### EDC15C6 — follow-ups (checksum solved)
The block checksum is solved from the dumps (see above). Remaining, optional:
- [ ] (1) **Daimler SA2 seed/key** → enter programming mode: **jglim/UnlockECU**.
- [ ] (2) **C167/ST10 bootstrap-mode** read of internal ROM/flash (has EDC15VM/P+ chip-select presets): **EcuProg7/C167BootTool** (GPL-3) + **NefMoto** C167/C166 loaders (GPL-3).
- [ ] (3) Validate on more EDC15C6 software versions / other Mercedes CDI variants as dumps become available.

### Hardware 🔒 (mostly)
- [ ] Adopt **Switchleg1/esp32-isotp-ble-bridge** (MIT) as the companion ESP32-S3 BLE/ISO-TP firmware instead of writing it from scratch. *(porting is offline; compile/flash/on-bench is 🔒)*
- [ ] Reference ECUFlash/RomRaider definition parsers from **FastECU** (GPL-3) for broader def import. *(offline)*

---

## Phase 5 — Full diagnostic & service suite
Turn the toolkit into a complete suite. Every pillar is a **service engine** (code, written once)
over the existing protocol layer + **data-defined packs** (defs/labels/scripts), so brands/vehicles
drop in as data. Everything here is open and clean-room; OEM seed/keys and protected calibration
data are never bundled — they are caller-supplied or left out of scope. Most engines are buildable +
unit-testable offline; the live paths are 🔒 (need a car/adapter).

### Coding & adaptation
- [~] **Module coding engine** (`opentune-coding`) — data-defined coding-label sets (byte/bit→label with a documented LSB-first convention), `decode` / `get` / `set` (range + enum-option validated) / before-after **param & byte diff**, and a **safe write-back** over UDS 0x22/0x2E: read-first backup → write only changed blocks → **read-back verify**, plus **dry-run** plans and a `requires_security` flag. Engine runs over core's `RawDiag`/`CoreDiag`, tested against `opentune-udssim`; CLI `coding` + desktop panel. Next: live test, and **brand label packs** (VAG long-coding, Ford As-Built/CCC via `ccc_descr.xml`).
- [~] *(engine + CLI `adaptation` + desktop panel, udssim-tested; live 🔒)* **Adaptation / basic settings** — adaptation channels, `RoutineControl` (0x31), guided functions; data-defined channel maps; the 0x27 login gate reuses the existing `SeedKey` layer.
- [~] **Actuator / output tests** — `InputOutputControlByIdentifier` (0x2F) — available as a service-script step in `opentune-servicefn`; no dedicated actuator UI yet.

### Service & diagnostics
- [~] *(`opentune-servicefn` engine + CLI `service` (plan/dry-run) + desktop panel, udssim-tested; script library + live 🔒)* **Service functions** — declarative, safe UDS scripts (session → security → routine → verify): EPB, DPF regen, oil/service reset, SAS calibration, throttle align, injector coding, battery registration, brake bleed. Script library grows as data.
- [~] *(`opentune-autoscan` + CLI `autoscan` + desktop panel: data-defined module list, ping → identity → DTCs, read-only; live 🔒 open)* **Full-vehicle auto-scan** — enumerate all modules, read every module's DTCs + identity (ForScan/VCDS-style), freeze-frame + readiness.

- [~] **OBD inspection** (`opentune_core::obd::inspect`, CLI `obd readiness|freeze|info|dtcs`, desktop Inspection panel) — MIL + readiness monitors, freeze frame, mode 09 VIN/CALID/CVN/ECU name, stored/pending/permanent DTCs, per-module or all 7E0–7E7; `obdsim` answers all of it. Live 🔒.
- [x] **Virtual car** (`opentune-driver-vcar`, `packs/vcar-demo`, `docs/VIRTUAL-CAR.md`) — six simulated modules on a virtual CAN bus (engine, gearbox, ABS, airbag, body, cluster) with identity, DTCs, coding, adaptation, routines, IO control, OBD and synthetic CAN traffic; every vehicle panel works offline end to end.

### Live data & logging
- [~] **Dashboard** (`opentune_core::obd` + desktop panel + CLI `obd pids|supported|read`) — SAE J1979 mode-01 table (~40 PIDs incl. IAT, MAP + derived boost, MAF, rail pressure, EGT bank 1/2, DPF, torque), supported-PID discovery, up to 6 PIDs per request with single-PID fallback, gauges + tiles with warn/crit thresholds; `obdsim` simulated ECU. Live 🔒.
- [~] **CAN capture** (`CanMonitor` in core, `opentune-candump` writers, CLI `candump`, desktop panel) — candump log / CSV / Vector ASC, per-ID stats, listen-only reported per adapter (slcan `L`, SocketCAN/gs_usb listen-only, ELM327 `ATMA` + `ATCSM1`); holds the adapter lock for the whole capture. Live 🔒.
- [~] *(`opentune-logdef`: RomRaider `<logger>` + EcuFlash `<ecus>` parsing; live loop over UDS DIDs, OBD mode-01 PIDs and 0x23 memory reads; CLI `log`. SSM transport, dashboards/triggers + live 🔒 open)* **Datalogging engine** — periodic live data to file + dashboards/graphs + triggers, over OBD mode-01, UDS ReadDataByIdentifier (0x22 periodic), **UDS `mode23`/0x23 address logging**, and **Subaru SSM** — modelled on the Tactrix OpenPort logger (PID sorting, multi-request chunking). Reuse **RomRaider `logger.xml`** param definitions (data-defined, like coding). Ref: **RomRaider** (GPL-2), **FreeSSM** (GPL-3).

### Flashing to modules (not just engine ECUs)
- [ ] **UDS module flash** end-to-end — `0x34`/`0x36`/`0x37` + security + erase/checksum `RoutineControl`, **container-driven** (VBF/ODX/FRF/SGO/bin → RequestDownload address/length), behind the existing **flash-safety policy** (voltage → backup → write → read-back verify → recovery). 🔒 Ford module flashing (VBF + the user-supplied `SeedKey` plugin) is the first concrete target.
- [ ] 🔒 **Validate the J2534 drivers on an OpenPort 2.0** — `opentune-driver-j2534`/`j2534-openport` are written but hardware-unproven; the Tactrix OpenPort 2.0 is the reference PassThru box and the right interface for real engine-ECU flashing (vs the read-only ELM327). Target: EDC16/Subaru SH flash end-to-end on the bench.
- [ ] **Renesas SH boot mode** read/write (SH7055/SH7058/**SH725xx**) — bench/boot access for Subaru/Mitsubishi *and* the SH725xx DSG (DQ381) TCU. 🔒 bench. Refs: **fenugrec/npkern** (GPL-3, SH RAM kernel), EcuFlash SH boot-mode behavior, NefMoto loaders.

### Frontends (all local — no server)
- [x] **Plugin host** — desktop is a library (`opentune_desktop`) + thin binary; embedders add commands that share the adapter lock, extra checksum algorithms, a build channel, and UI overlay panels (`extensions.js` + `OTNav.register`). See `docs/PLUGINS.md`.
- [x] **Plugin hooks for unlock + CLI reuse** (OEM-free extension points; the embedder owns every algorithm, core ships none):
  - [x] `Interface::set_seed_key` — any registry-opened driver takes a caller-supplied `SeedKey` (10 drivers with an unlock step accept it; the rest return `false`).
  - [x] `DesktopConfig.seedkeys` resolver — plugin read/flash panels (`adapter::apply_seed_key`), service scripts (optional `seedkey` id) and the seed/key calculator (`source: "plugin"`) unlock through it.
  - [x] **CLI host API** — `opentune-cli` is a library (`opentune_cli`) + thin binary: `run_with(CliConfig { seedkeys, checksums, drivers, extra commands })`, a `CliContext` for extra commands, `--seedkey <id>` on `info/read/flash/dump/identify/service`, collisions refused at startup. See `docs/PLUGINS.md`.
- [~] **Desktop code split** — `lib.rs` / `app.js` into modules, `node --test` UI tests, a compile-time plugin-API test that pins every path plugins use. *(in progress, after the unlock hook)*
- [x] **Sidebar groups + tiers** — Calibration · Vehicle · Reference · Help · Bench. Car-facing tools start in the Bench section (alpha/bench builds only) and move up once tested on a real car.
- [x] **In-app help** — a guide page per panel plus a help strip on every panel (risk, what you need, three steps, open full guide). Guide renderer handles wrapped lists and links.
- [x] **ECU database** (`database/units`, `opentune-db`, CLI `db`, desktop panel) — 17 ECU/TCU/AWD units with read methods, checksum family, vehicles and sourced bench pinouts; a validator enforces per-pin sources and confidence (disputed pins are never part of bench wiring).
- [x] **Safer file writes** — checksum fix saves to a new file by default (overwrite archives the original first), flash archives the target to the vault, map write-back reports which checksum it used.
- [x] **Fast startup** — nothing heavy before the first paint: definitions/catalog parsed once on a background thread and cached, startup commands async, panels load on first show, WebKitGTK DMA-BUF renderer off by default on Linux; `OPENTUNE_TRACE_STARTUP=1` prints a timing breakdown (`docs/TROUBLESHOOTING-STARTUP.md`). ~1.1 s to first frame on a desktop test machine.
- [x] **Process gates** — `scripts/check.sh` (all CI gates incl. desktop + UI JS), tracked pre-commit hook, CI jobs for the desktop crate and UI, doc-drift tests (CLI commands, drivers, checksum ids, crates must be documented); `docs/DEVELOPMENT.md`.
- [~] *(desktop: adapter capability bar + capability-gated car-facing panels — coding/adaptation/autoscan/service/logging/OBD/dashboard/inspection/CAN capture — in the Bench tier until vehicle-tested; Debug console alpha-only)* Wire every pillar into the **Tauri desktop** app as the suite's primary face, with **guided wizards** (coding/adaptation/service/flash) + live dashboards; **Android** companion over USB-OTG/BLE; **CLI** for automation. Signed **OpenTune** AppImages from GitHub Releases.

### Definition ecosystem (the scaling lever)
- [~] New data formats — **coding-label**, **adaptation-channel map**, **service-function script**, **logger** and **vehicle-module list** all exist; **`opentune-packs`** bundles them as one versioned pack (`pack.json` + per-kind dirs, CLI `packs`). Shipped starter packs: `generic-obd2` (J1979 module lists, 34-PID logger, readiness/DTC/VIN scripts, mode-04 clear as the only write), `iso14229-identification`, `vag-modules` (7 UDS modules, each sourced from MIT/BSD projects), `vcar-demo`. Still open: the community library itself. A **community** definition/pack library, versioned files the app loads locally (no hosting).
- [~] **EcuFlash / RomRaider XML definition importer** (`opentune-ecuflash`, CLI `import-ecuflash`) — parses `<rom>` identity + reusable `<scaling>` + 1D/2D/3D `<table>` with axes → OpenTune `MapDef`s, `toexpr` fitted to a linear factor/offset via the XDF math (non-linear flagged + imported pre-scaled, never a silent wrong scale); fidelity warnings for unmerged `<include>` base defs, unknown `storagetype`, missing scaling refs. `<include>` / `<rom base>` base-def merging in the library (`parse_ecuflash_with_bases`; the CLI does not resolve bases yet). 26 tests + fixture-gated real-def test. **Next:** base resolution in the CLI, RomRaider attribute variants, `logger.xml` params (feeds the logging engine). Unlocks the community **Subaru/Mitsubishi** map library. Parse the open **format** + community-licensed defs; don't lift EcuFlash code. Refs: **RomRaider** (GPL-2), **FastECU** (GPL-3).
- [ ] **Subaru / Mitsubishi seed-key** tables as an optional, user-enabled plugin (published tables), same stance as the Ford/Daimler entries above.

---

## Suggested build order
Ranked by value, liftability and dependencies. **Offline** items need only a `.bin` and build today;
**🔒** items wait on an interface + bench ECU.

**Offline — buildable now**
*(Done since the last revision: ME7 checksum, FRF/ODX/SGO decode, TCU checksums, definition-format breadth, DoIP over a loopback entity, EDC15C6 checksum, map finder, tune recipes, log viewer, OBD inspection, virtual car, starter packs.)*
1. **Desktop split** — in progress (see Frontends). **ELM327 USB block read/write** (`elm327obd`: 0x23 / 0x35-0x36 upload, guarded 0x34-0x36 download, seed-key hook) — in progress.
2. **Fast logging via UDS 0x2C** (DynamicallyDefineDataIdentifier) — the logger bundles the selected channels (by DID and/or memory address) into one dynamic DID and polls that, falling back to plain 0x22/0x23 polling when the ECU refuses 0x2C. Tested on udssim. Ref: VW_Flash `simos_hsl` (approach).
3. **Open aftermarket ECUs — Speeduino / rusEFI / MegaSquirt (TunerStudio-compatible)** — parse the INI + `.msq` definitions, read/write tables over their serial protocols, log realtime data; clean-room from the open specs (LibreTune as a reference). Serves engine swaps / standalone setups.
4. **Recipes on EDC17** — every EDC17 stock/tuned pair is refused today (the corrected output fails attestation); find out why before recipes are trusted beyond EDC16/EDC15.
5. **Relocate maps by content signature** — apply a definition built for one software version to a sibling version's read (PCR2.1 CASM2K30 → CASM2L95 is the first case).
6. **Logger text/BCD decode** — identification DIDs (part numbers, serials) currently show as numbers in the live logger.
7. **Cross-validate EDC15/EDC16 vs ZedSuite (Rust)** — cheap hardening of what already ships.
8. **Validate the UDS state machine vs `python-udsoncan`** — conformance of the protocol layer (udssim server done).
9. **Generalized controller-def schema → drivetrain defs** — the data model for TCU/AWD units, then VAG DSG / Haldex / Mercedes 722.6 defs (flashing is 🔒).
10. **SA2 seed/key plugin set** — user-supplied, never auto-applied (sensitive; careful scoping).
11. **Small UI follow-ups** — Dashboard "Record", "open in Maps" from the log overlay, pack picker in the panels, UI preview fixtures for the new panels.

**Waiting on data**
- **EDC17CP14 maps** (Passat CC) — need a VAG CP14 stock read, ideally with a tune of it.
- **PCR2.1 CASM2L95 maps** — need a tune of that software (or the relocation helper above).
- **Slow start on one laptop (30–40 s)** — not reproducible on a desktop (~1.3 s); needs an `OPENTUNE_TRACE_STARTUP=1` log from the affected machine (`docs/TROUBLESHOOTING-STARTUP.md`).

**🔒 Hardware-gated — later**
- **First car session, read-only** (`docs/FIRST-CAR-TEST.md`) — then live identify + full OBD read (EDC16 first) · live DTC read/clear
- Vehicle-test each live panel (dashboard, inspection, CAN capture, auto-scan, coding, adaptation, service) so it can leave the Bench tier
- Flash path end-to-end on the bench, then in-car (EDC16 first)
- Verify cheap/clone adapters on real hardware
- DSG / Haldex / 722.6 **bench flashing**
- ESP32-S3 compile/flash + KiCad board; flash the Switchleg firmware
- EDC15C6 programming access (seed/key, bootstrap read)
- Android USB-OTG / BLE adapter support
