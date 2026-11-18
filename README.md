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
- **Checksums** — a `ChecksumAlgo` engine with **Bosch EDC16 (MPC56x), EDC17 (TriCore), Continental Simos PCR2.1, VAG EDC15, Mercedes EDC15C6 (CR1, clean-room), Bosch ME7, and the VAG DQ381 DSG (TCU)** solved and validated against real full dumps; **DQ250, ZF 8HP and Haldex Gen5** are implemented but validated against algorithm vectors + synthetic images only (no real dump yet). `fix` corrects an uncorrected tune; `fix --reference` bootstraps a firmware-specific constant from your own stock dump; it **refuses to guess** on anything unvalidated (anti-brick), and reports *recognised* vs merely *mathematically consistent* results honestly.
- **Flash containers** — decode **FRF / ODX / ODX-F / SGO** (VAG) and **VBF** (Ford / Volvo / Mazda), with per-block and file checksums verified. Caller supplies any keys; none are bundled.
- **Calibration maps** — map-by-map **compare** in physical units, a **map-finder** for a single image (scored candidates with evidence) or a stock/tuned diff, portable **tune recipes** that only apply to a matching image (checksum-fixed and attested), a **log viewer** with map overlay, and import/export of the big community definition libraries: **TunerPro XDF**, **ASAM A2L**, and **EcuFlash / RomRaider XML** (Subaru/Mitsubishi), plus **WinOLS `.ols`** project import and a direct **A2L → XDF** converter — with lossy conversions flagged, never guessed.
- **Module coding** — a data-driven coding engine: read a module's configuration, decode it to human-readable parameters via coding-label definitions, edit a parameter (range/option validated), preview the before→after diff, and write it back safely (read-fresh → merge → read-back verify, with dry-run).
- **Diagnostics data** — an **11,140-code DTC database + decoder**, VIN and VAG/Bosch part-number decoders, and an ECU-library scanner.
- **Drivetrain** — VAG **DQ250 02E** DSG definitions with its sector checksum (validated on real reads) and shift-schedule maps; DQ250 MQB / DQ200 / DL382 identification; **Haldex Gen4** read-only identify and measuring groups over KWP2000/TP2.0 plus passive Haldex CAN decoding; **OpenHaldex** status and mode control; read-only **DQ250 02E** diagnostics over KWP2000/TP2.0 (`opentune dsg`); data-defined **controller definitions** for TCU/AWD units (DQ250, DQ381, Haldex, ZF 8HP, Mercedes 722.6 CAN decode).
- **Open aftermarket ECUs** — TunerStudio-compatible **Speeduino / rusEFI / MegaSquirt** support: INI definitions and `.msq` tunes, serial read/write with read-back verify before burn, live channels (`opentune ts`).
- A **CLI** (`opentune`) exposing all of the above: `drivers · identify · info · read · dump · diff · findmaps · compare · checksum verify|fix|algos|attest · flash · vault · ecudefs · packs · import-xdf|a2l|ecuflash|ols · a2l2xdf · export-xdf|a2l · vin · decode-part · scan · dtc · container · cbf · coding · adaptation · autoscan · service · log · simulate · controller · seedkey · doip · bsl · dsg · gps`.

### Implemented, awaiting on-car validation

- **Live diagnostics over ELM327** — a serial ELM327 OBD driver (HS-CAN / MS-CAN): identify, **read/clear DTCs** (UDS `0x19` + OBD mode 03), and **live data** (OBD PIDs) — unit-tested against scripted responses, not yet run on a vehicle.
- **Engines over the diagnostic transport** — adaptation / basic settings / actuator tests, service-function scripts, full-vehicle auto-scan (presence, identity, stored DTCs) and live datalogging (UDS `0x22` DIDs, OBD-II PIDs and `0x23` memory reads) — tested against the offline UDS conformance simulator (`opentune simulate`), not yet on a vehicle.
- **Interface drivers** — OpenPort 2.0 (J2534/CDC-ACM), generic J2534 PassThru, SocketCAN, slcan, gs_usb/candleLight, ELM327 (serial OBD, K-line, and WiFi/Bluetooth), KKL 409.1 (raw K-line), an Arduino K-line bridge and an ESP32-S3 open interface (both with OpenTune firmware), the EFI EFP02C USB-CAN box, the open ESP32 ISO-TP BLE bridge, and **DoIP** over Ethernet.
- **Protocol layer** — ISO-TP (hardened), UDS, KWP2000, VW TP2.0, CAN-FD (+ ISO-TP-FD) and **DoIP** (ISO 13400), with security access (`0x27`) wired into every write path — *you supply the seed/key algorithm; OpenTune ships none*.
- **Flash orchestrator** — voltage gate → verified backup → write → read-back verify, resumable/recoverable, with a backup vault and a full session transcript.
- **Apps** — a Tauri **desktop** app (identify, checksums, map viewer/editor, compare, flash, vault, log viewer, ECU database, in-app help, plus an adapter bar and capability-gated live panels for coding, auto-scan + DTCs, adaptation, service functions, logging, an OBD dashboard, OBD inspection and CAN capture — the live panels sit in the Bench section of alpha builds until they are vehicle-tested; plugins can add panels) — try every live panel offline against the built-in **virtual car** (`vcar` driver) and an **Android** app over the same core (UniFFI).
- **Plugin API** — the desktop app and the CLI are both libraries: embedders add panels, commands, drivers, checksum algorithms and caller-supplied seed/key handlers without forking (`docs/PLUGINS.md`). OpenTune itself ships no OEM seed/key algorithms.

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
  Protocol    ISO-TP (+FD) · UDS · KWP2000 · TP2.0 · OBD-II ·
              CAN-FD · DoIP
  ───────────────────────────────────────────────────────
  Transport   ELM327 · J2534/OpenPort · SocketCAN · K-line ·
              gs_usb · slcan · ESP32-S3 / Arduino (open HW)
```

Because a feature is a service over the protocol layer, it works on any interface; because coverage is data, the suite grows by contributing definitions, not by editing code.

## Where it's heading

Most pillars are now built and tested offline; what remains is mainly proving them on a vehicle. See **[ROADMAP.md](ROADMAP.md)** for the full plan.

- **On-car validation** — run the live diagnostic, coding, adaptation, service-function, auto-scan and logging paths on real vehicles.
- **Full-vehicle diagnostics** — validate OBD inspection (readiness, freeze frame, CALID/CVN) on real cars.
- **Datalogging** — Subaru SSM transport, plus dashboards and triggers.
- **Module & ECU flashing** — finish the UDS flash path (`0x34`/`0x36`/`0x37` + security + erase/checksum) end-to-end, container-driven, behind the flash-safety policy.
- **Broader coverage** — real dumps to validate the DQ250 / ZF 8HP / Haldex checksums, and a growing community definition & map library.
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

## Live diagnostics (experimental)

The engine subcommands drive a real adapter through a diagnostic transport.
With an ELM327-class USB dongle use the `elm327obd` driver (defaults to
`/dev/ttyUSB0 @38400`); point it at another port with `OPENTUNE_PORT` /
`OPENTUNE_BAUD`:

```sh
# raw UDS exchange against the offline conformance simulator (no hardware)
cargo run -p opentune-cli -- simulate --did F190=0102 22F190

# live, over an ELM327 dongle
OPENTUNE_PORT=/dev/ttyUSB0 \
  cargo run -p opentune-cli -- autoscan --modules modules.json --driver elm327obd
cargo run -p opentune-cli -- log --def logger.xml --driver elm327obd --rounds 20 --interval-ms 500
```

> These live paths are **not yet validated on a vehicle** — treat them as
> experimental, read-only first, and keep a backup before any write.

## Safety

- OpenTune **always** takes a verified backup before any write, checks battery voltage, and reads back and verifies afterwards — these rules live in the core, not in individual drivers — and the checksum and coding engines **refuse rather than guess** when a result isn't validated.
- A failed flash or a wrong coding can leave a module non-functional. Keep a battery charger connected, never interrupt a write, and keep your original backup safe.
- You are responsible for complying with the vehicle, emissions and road-traffic laws where you live.

## Licence

- Software and firmware: **GPL-3.0-or-later** (see [LICENSE](LICENSE)).
- Hardware design files (when added): **CERN-OHL-S-2.0**.

Releases: [GitHub Releases](https://github.com/ebjerk01/opentune/releases) ·
[Code signing policy](CODE_SIGNING_POLICY.md) (Windows builds are not Authenticode-signed yet) ·
[Code of conduct](CODE_OF_CONDUCT.md).
Privacy: the app sends nothing anywhere except the update check against GitHub Releases.

## Status & roadmap

Early alpha, moving fast. The full plan lives in **[ROADMAP.md](ROADMAP.md)**; the
short version:

**Landed**
- ✅ Checksum correction — EDC15/16/17, Mercedes EDC15C6, Simos PCR2.1, ME7, DQ381 TCU; DQ250, ZF-8HP and Haldex *(synthetic-validated only)*
- ✅ Flash containers — FRF / ODX / SGO / VBF, plus a sparse HEX / SREC image model
- ✅ Definition import — TunerPro XDF, ASAM A2L, EcuFlash / RomRaider XML, WinOLS `.ols`
- ✅ Loadable **definition packs** — grow coverage by data, not code
- ✅ Module **coding** — bit-field decode/edit with safe, baseline-aware writes
- ✅ **Adaptation** channels and actuator / output tests
- ✅ Declarative **service functions** — DPF regen, service reset, SAS calibration, …
- ✅ Full-vehicle **auto-scan** — every module, DTCs, identities
- ✅ **OBD inspection** — readiness monitors, freeze frame, VIN/CALID/CVN, stored/pending/permanent DTCs
- ✅ **Virtual car** — a simulated six-module vehicle so every live panel works offline
- ✅ **Fast logging** — UDS `0x2C` dynamic data identifiers bundle many channels into one request, with automatic fallback to polling
- ✅ **Datalogging** — logger.xml definitions + a live polling loop to CSV over UDS DIDs, OBD-II PIDs and `0x23` memory reads, with an optional **GPS** source (position, true ground speed, speedometer error) *(Subaru SSM still pending)*
- ✅ Protocol stack — ISO-TP, UDS, KWP2000, VW TP2.0, OBD-II, CAN-FD, DoIP
- ✅ Diagnostics driver — ELM327 / OBD, with a UDS conformance simulator for offline tests
- ✅ SecurityAccess SA2 — runs real VAG SA2 scripts from your own flash containers (none bundled)
- ✅ Mercedes CBF diagnostic descriptions
- ✅ **Drivetrain** — DQ250 02E checksum + shift maps, Haldex Gen4 read-only + OpenHaldex, DSG/TCU recognition, controller definitions incl. Mercedes 722.6
- ✅ **Aftermarket ECUs** — Speeduino / rusEFI / MegaSquirt (TunerStudio INI + `.msq`, serial read/write/burn, live data)
- ✅ **Plugin API** — desktop + CLI host APIs, seed/key and checksum resolvers

**Next**
- 🔲 Extend live **datalogging** to the Subaru SSM transport, plus dashboards and triggers
- 🔲 On-car validation of the live diagnostic, adaptation and logging paths
- 🔲 End-to-end UDS **flashing** (`0x34`/`0x36`/`0x37` + erase/checksum), behind the flash-safety policy
- 🔲 Desktop & Android UX — guided wizards over the engines
- 🔲 A growing community definition & pack library, and broader drivetrain/AWD coverage (DQ250 launch/torque maps, DQ200/DL382 maps, active Haldex Gen4 reads over TP2.0)
- 🔲 Signed releases with desktop auto-update
- 🔲 CAN-FD, DoIP and J2534 flashing validated on hardware

OEM seed/key algorithms and protected calibration data are never bundled — OpenTune
takes them as caller-supplied inputs or leaves them out of scope.
