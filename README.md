# OpenTune

**An open-source car diagnostics, coding and tuning suite — read fault codes, live data, code and adapt modules, flash ECUs and edit calibrations, over cheap and open hardware you already own.**

> ⚠️ **Early alpha — not ready for use on a real car.** The offline tooling (working on a `.bin` or a definition file) is solid and tested; the on-car paths — live diagnostics, coding, adaptation and flashing — are **implemented but unproven on a vehicle**, and the desktop/Android apps are **unreleased alpha builds**. Reading, coding, or reflashing a control unit can permanently damage it. **Do not use this on a real vehicle yet.** Expect breaking changes. Use entirely at your own risk — see [Safety](#safety).

---

## What OpenTune is

OpenTune began as a clean-room ECU **calibration + checksum** toolkit. It's growing into a complete, local, open-source **diagnostics and tuning suite** — the kind of thing people reach for a mix of closed tools to do today — built on one Rust core:

> **identify → scan → read fault codes → live data → code → adapt → service functions → flash → tune**

Every feature is a *service* built on a shared protocol layer, so it works across every supported interface; new vehicles are added as **data** (definitions, label packs), not new code. The whole thing is GPL-3.0, ships **no manufacturer firmware or keys**, and refuses to guess where guessing could brick a module.

## Capabilities

### Working today (offline — on a `.bin` or definition file, fully tested)

- **Identify** an ECU from its reported part/SW numbers against ECU definitions + a **1034-entry catalog** (comma.ai opendbc + hand-curated European entries across VAG / Mercedes / BMW / Ford / PSA / Opel / Fiat / Volvo / Renault).
- **Checksums** — a `ChecksumAlgo` engine with **Bosch EDC16 (MPC56x), EDC17 (TriCore), Continental Simos PCR2.1, VAG EDC15, Bosch ME7, and the VAG DQ381 DSG (TCU)** solved and validated against real full dumps. `fix` corrects an uncorrected tune; `fix --reference` bootstraps a firmware-specific constant from your own stock dump; it **refuses to guess** on anything unvalidated (anti-brick), and reports *recognised* vs merely *mathematically consistent* results honestly.
- **Flash containers** — decode **FRF / ODX / ODX-F / SGO** (VAG) and **VBF** (Ford / Volvo / Mazda), with per-block and file checksums verified. Caller supplies any keys; none are bundled.
- **Calibration maps** — map-by-map **compare** in physical units, auto **map-finder** from a stock/tuned diff, and import/export of the big community definition libraries: **TunerPro XDF**, **ASAM A2L**, and **EcuFlash / RomRaider XML** (Subaru/Mitsubishi) — with lossy conversions flagged, never guessed.
- **Module coding** — a data-driven coding engine: read a module's configuration, decode it to human-readable parameters via coding-label definitions, edit a parameter (range/option validated), preview the before→after diff, and write it back safely (read-fresh → merge → read-back verify, with dry-run).
- **Diagnostics data** — an **11,140-code DTC database + decoder**, VIN and VAG/Bosch part-number decoders, and an ECU-library scanner.
- A **CLI** (`opentune`) exposing all of the above: `identify · info · read · dump · diff · findmaps · compare · checksum verify|fix · flash · coding · vault · ecudefs · import-xdf|a2l|ecuflash · export-xdf|a2l · vin · decode-part · scan · dtc · container`.

### Implemented, awaiting on-car validation

- **Live diagnostics over ELM327** — a serial ELM327 OBD driver (HS-CAN / MS-CAN): identify, **read/clear DTCs** (UDS `0x19` + OBD mode 03), and **live data** (OBD PIDs) — unit-tested against scripted responses, not yet run on a vehicle.
- **Interface drivers** — OpenPort 2.0 (J2534/CDC-ACM), generic J2534 PassThru, SocketCAN, slcan, gs_usb/candleLight, KKL 409.1 (raw K-line), an Arduino K-line bridge and an ESP32-S3 open interface (both with OpenTune firmware).
- **Protocol layer** — ISO-TP (hardened), UDS, KWP2000, VW TP2.0, CAN-FD (+ ISO-TP-FD) and **DoIP** (ISO 13400), with security access (`0x27`) wired into every write path — *you supply the seed/key algorithm; OpenTune ships none*.
- **Flash orchestrator** — voltage gate → verified backup → write → read-back verify, resumable/recoverable, with a backup vault and a full session transcript.
- **Apps** — a Tauri **desktop** app (identify, checksums, map viewer/editor, compare, flash, vault) and an **Android** app over the same core (UniFFI).

## Architecture

One core, many interfaces, data-driven features:

```
  Apps        CLI  ·  Desktop (Tauri)  ·  Android            ← one shared core
  ───────────────────────────────────────────────────────
  Services    Diagnostics · Live data · Coding · Adaptation ·
              Service functions · Flashing · Tuning          ← features, written once
  ───────────────────────────────────────────────────────
  Knowledge   ECU defs · coding labels · adaptation maps ·
  (DATA)      DTC DB · map packs · checksum & container defs ← new vehicles = new data
  ───────────────────────────────────────────────────────
  Protocol    ISO-TP · UDS · KWP2000 · TP2.0 · OBD-II · DoIP
  ───────────────────────────────────────────────────────
  Transport   ELM327 · J2534/OpenPort · SocketCAN · K-line ·
              gs_usb · slcan · ESP32-S3 / Arduino (open HW)
```

Because a feature is a service over the protocol layer, it works on any interface; because coverage is data, the suite grows by contributing definitions, not by editing code.

## Where it's heading

The next phase turns the suite's remaining pillars on, mostly buildable and testable offline, with the live paths gated on a vehicle. See **[ROADMAP.md](ROADMAP.md)** for the full plan.

- **Full-vehicle diagnostics** — auto-scan every module, read all fault codes + identities, freeze-frame and readiness.
- **Adaptation & service functions** — adaptation channels and actuator/output tests (`0x31`/`0x2F`), and declarative, safe service-function scripts (DPF regen, service reset, SAS calibration, injector coding, …) — all data-driven.
- **Datalogging** — OBD/UDS periodic data + address-based and Subaru-SSM logging, to file and dashboards.
- **Module & ECU flashing** — finish the UDS flash path (`0x34`/`0x36`/`0x37` + security + erase/checksum) end-to-end, container-driven, behind the flash-safety policy.
- **Broader coverage** — drivetrain/TCU and AWD controllers, more checksum and definition formats (WinOLS `.ols`, logger definitions, HEX/SREC), real DoIP on hardware, and a growing community definition & map library.
- **Interfaces to the max** — CAN-FD, DoIP and J2534 flashing, validated on hardware.

Sensitive, brick- or security-adjacent capabilities (OEM seed/key algorithms, protected calibration data) are **never bundled** — OpenTune takes them as caller-supplied inputs or leaves them out of scope, so the open project stays clean and hostable.

## Scope — what OpenTune will *not* do

To stay a legitimate, hostable project, OpenTune deliberately leaves out:

- immobiliser removal / defeat
- odometer manipulation
- emissions-control defeat (DPF/EGR/AdBlue "off")

No manufacturer firmware, calibration data or security keys are included in this repository.

## Building

```sh
# Rust stable (see rust-toolchain.toml)
cargo build --workspace
cargo test  --workspace
cargo run -p opentune-cli -- --help
```

## Safety

- OpenTune **always** takes a verified backup before any write, checks battery voltage, and reads back and verifies afterwards — these rules live in the core, not in individual drivers — and the checksum and coding engines **refuse rather than guess** when a result isn't validated.
- A failed flash or a wrong coding can leave a module non-functional. Keep a battery charger connected, never interrupt a write, and keep your original backup safe.
- You are responsible for complying with the vehicle, emissions and road-traffic laws where you live.

## Licence

- Software and firmware: **GPL-3.0-or-later** (see [LICENSE](LICENSE)).
- Hardware design files (when added): **CERN-OHL-S-2.0**.

## Status & roadmap

Early alpha, moving fast. The full plan lives in **[ROADMAP.md](ROADMAP.md)**; the
short version:

**Landed**
- ✅ Checksum correction — EDC15/16/17, Simos PCR2.1, ME7, DQ250/DQ381 TCU, ZF-8HP, Haldex
- ✅ Flash containers — FRF / ODX / SGO / VBF, plus a sparse HEX / SREC image model
- ✅ Definition import — TunerPro XDF, ASAM A2L, EcuFlash / RomRaider XML, WinOLS `.ols`
- ✅ Loadable **definition packs** — grow coverage by data, not code
- ✅ Module **coding** — bit-field decode/edit with safe, baseline-aware writes
- ✅ **Adaptation** channels and actuator / output tests
- ✅ Declarative **service functions** — DPF regen, service reset, SAS calibration, …
- ✅ Full-vehicle **auto-scan** — every module, DTCs, identities, freeze-frame, readiness
- ✅ **Datalogging** definitions — OBD/UDS, address-based and Subaru-SSM
- ✅ Protocol stack — ISO-TP, UDS, KWP2000, VW TP2.0, OBD-II, CAN-FD, DoIP
- ✅ Diagnostics driver — ELM327 / OBD, with a UDS conformance simulator for offline tests
- ✅ SecurityAccess SA2 bytecode VM (runs caller-supplied bytecode)
- ✅ Mercedes CBF diagnostic descriptions

**Next**
- 🔲 On-car validation of the live diagnostic, adaptation and logging paths
- 🔲 End-to-end UDS **flashing** (`0x34`/`0x36`/`0x37` + erase/checksum), behind the flash-safety policy
- 🔲 Desktop & Android UX — guided wizards over the engines
- 🔲 A growing community definition & pack library, and broader drivetrain/AWD coverage
- 🔲 CAN-FD, DoIP and J2534 flashing validated on hardware

OEM seed/key algorithms and protected calibration data are never bundled — OpenTune
takes them as caller-supplied inputs or leaves them out of scope.
