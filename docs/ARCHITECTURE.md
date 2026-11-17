# Architecture

OpenTune is layered so that a new app or a new piece of hardware is an additive
change, never a rewrite. The rule is one-directional: **apps depend on the core,
the core defines one driver trait, and drivers implement that trait.** Nothing
above the core talks to hardware, and no driver can bypass the safety rules.

```
          ┌───────────────────────────────────────────────┐
  apps    │   CLI        Desktop (Tauri)        Android     │
          │  opentune-cli   desktop/          android/      │
          └───────────────────────┬───────────────────────┘
                                  │  trait Interface (+ Registry)
          ┌───────────────────────┴───────────────────────┐
  core    │   opentune-core                                │
          │     Interface trait · Registry · types         │
          │     protocol: ISO-TP(-FD) · UDS · KWP2000 ·     │
          │       VW TP2.0 · CAN-FD · DoIP · seed/key       │
          │     diag (RawDiag/CoreDiag) · catalog · validate│
          │     ECU definitions (ecudef)                    │
          │                                                 │
          │   opentune-checksum   opentune-flash            │
          │     ChecksumAlgo        safety orchestrator     │
          │   + format / engine crates (see table below)    │
          └───────────────────────┬───────────────────────┘
                                  │  implements Interface (and/or CanLink)
   ┌───────────────┬─────────────┼───────────────┬──────────────────┐
 mock/file     OpenPort 2.0    SocketCAN     ELM327 / K-line    J2534 · slcan ·
 (offline)     (CDC-ACM)       (Linux CAN)   + own firmware     gs_usb · …
```

## Layers

### Apps
Thin shells that wire drivers into a `Registry` and drive core operations.

- **`opentune-cli`** — the reference app. Offline-capable operations (identify,
  read, diff, checksum) plus a safety-gated `flash`. With the `mock` driver
  everything works against local files. A library (`opentune_cli`: clap tree,
  driver registry, `commands/<area>.rs` handlers) plus a thin `opentune`
  binary; other front ends embed it through `run_with(CliConfig)` to add
  subcommands, drivers, checksum and seed-key resolvers (see
  [`PLUGINS.md`](PLUGINS.md#cli-host-api)).
- **`desktop/`** — a Tauri app (lives outside the workspace, builds on its own).
- **`android/`** — a Kotlin/Compose app over the same core via UniFFI
  (`opentune-ffi`); the APK builds in CI and has been run on a real device
  (see [`android.md`](android.md)).

#### CLI commands

`opentune --help` is authoritative; `opentune <command> --help` gives the
options. A doc-drift test (`crates/opentune-cli/tests/doc_drift.rs`) fails when
a top-level command is added without a row here.

| Command | Purpose |
|---|---|
| `opentune drivers` | List the interface drivers this build knows about |
| `opentune info` | Open a driver and print ECU identity + battery voltage |
| `opentune read` | Read a memory region to a file |
| `opentune diff` | Byte-diff two images and summarise changed regions |
| `opentune checksum` | Verify / correct checksums; `checksum algos` lists the algorithms |
| `opentune flash` | Write a region through the safety orchestrator (backup → write → verify) |
| `opentune dump` | Full read via a driver, archived to the backup vault with ECU id |
| `opentune findmaps` | Find candidate calibration maps in one image (scored, with evidence), or from the diff of two images |
| `opentune recipe` | Portable tune recipes: `create` from a stock/tuned pair, `apply` only to a matching image (checksum-fixed + attested), `show` |
| `opentune ecudefs` | List / validate ECU-definition files |
| `opentune identify` | Read the connected ECU's identity and match it to a definition |
| `opentune db` | Control-unit reference database: list, show, validate |
| `opentune dtc` | Look up a diagnostic trouble code |
| `opentune vin` | Decode a VIN |
| `opentune decode-part` | Parse a VAG or Bosch ECU part number |
| `opentune compare` | Compare two calibrations map-by-map through an ECU definition |
| `opentune import-xdf` | Import a TunerPro XDF definition |
| `opentune import-ecuflash` | Import an EcuFlash / RomRaider XML ROM definition |
| `opentune import-ols` | Import a WinOLS `.ols` project's map definitions |
| `opentune import-a2l` | Import an ASAM A2L description |
| `opentune a2l2xdf` | Convert an A2L straight into a TunerPro XDF |
| `opentune export-xdf` | Export an ECU definition's maps as a TunerPro XDF |
| `opentune export-a2l` | Export an ECU definition's maps as an ASAM A2L |
| `opentune packs` | Load definition packs and report their contents |
| `opentune vault` | List archived reads in a backup vault directory |
| `opentune scan` | Scan a directory of ECU dumps offline, emit identification JSON |
| `opentune container` | Decode a VAG flash container (FRF / ODX / ODX-F / SGO) |
| `opentune cbf` | Parse a Mercedes CBF diagnostic-description file |
| `opentune coding` | Module coding over UDS (read, decode, edit, write) |
| `opentune adaptation` | Adaptation channels (value / routine / actuator) |
| `opentune autoscan` | Scan every module in a vehicle map: presence, identity, DTCs |
| `opentune service` | Plan or run a service-function script |
| `opentune log` | Log live data from a RomRaider-style `logger.xml` to CSV (`--fast`: UDS `0x2C` dynamic DIDs) |
| `opentune obd` | Generic OBD-II: mode-01 live data, readiness, freeze frame, mode-09 info (VIN/CALID/CVN), stored/pending/permanent DTCs |
| `opentune candump` | Capture a CAN bus to a candump / CSV / ASC log, listen-only where possible |
| `opentune simulate` | Exchange raw requests with the offline UDS conformance server |

### Core — `opentune-core`
The abstraction every app talks to and every driver implements.

- `interface` — the `Interface` trait: `open`/`close`, `read_ecu_id`,
  `read_memory`, `write_memory`, `battery_millivolts`, plus `info` and
  `capabilities`, and `diag()` — which hands out the driver's `DiagTransport`
  (default `None`) for the service engines. This is the whole surface an app
  uses.
- `diag` — `RawDiag` (one `request(&[u8]) -> Vec<u8>`) and `CoreDiag`, the
  adapter that wraps a driver's `DiagTransport` so the coding / adaptation /
  service / auto-scan / logging engines run over any driver.
- `catalog` — the vehicle → ECU identification catalog behind `identify`.
- `validate` — machine review of ECU definitions (`ecudefs validate`).
- `registry` — `Registry`/`DriverEntry`: enumerate and construct drivers by id,
  so apps don't hard-code which drivers exist.
- `capabilities` — `Capabilities`: what a driver supports (CAN/CAN-FD, K-line,
  read/write memory, bench/boot, battery/programming voltage), so apps can grey
  out actions.
- `types` — shared values: `Protocol`, `DiagLayer`, `MemoryRegion`, `EcuId`,
  `ConnectConfig`, `Progress`/`Phase`.
- `error` — `Error`/`Result`, including structured `NegativeResponse` (NRC).
- `protocol` — the diagnostic stack reused by every CAN/K-line driver:
  - `isotp` — `CanLink` (raw-frame send/recv, provided by a driver) and `IsoTp`
    (ISO 15765-2 transport that implements `DiagTransport`).
  - `diag` — the `DiagTransport` request/response trait and NRC helpers.
  - `uds` — `UdsClient` (ISO 14229 services over a `DiagTransport`).
  - `kwp2000` — `Kwp2000Client`.
  - `vwtp20` — VW TP2.0 transport (KWP2000 over CAN on older VAG).
  - `canfd` — CAN-FD frames + `IsoTpFd` (ISO-TP over CAN-FD).
  - `doip` — `DoIpTransport` (ISO 13400 diagnostics over IP).
  - `seedkey` — the `SeedKey` plugin interface for security access.
- `ecudef` — `EcuDef` and `load_dir`: data describing a known ECU (protocol,
  memory map, checksum id, seed/key id), loaded from JSON in
  [`ecudefs/`](../ecudefs/). See [`ECU-DEFS.md`](ECU-DEFS.md).

### Core helpers — `opentune-checksum`, `opentune-flash`
Kept as separate crates so apps and drivers can pull in only what they need.

- **`opentune-checksum`** — the `ChecksumAlgo` trait (`verify`/`correct`) with a
  per-family lookup (`get`), a generic CRC32-tail example and the `edc15`,
  `edc16`, `edc17`, `pcr21`, `me7` and `tcu` (DQ250 / DQ381 / ZF 8HP / Haldex /
  CCITT trailer) modules. The id used here is what an ECU definition names in
  its `checksum` field (`opentune checksum algos` lists them).
- **`opentune-flash`** — the flash orchestrator. `flash_calibration` enforces the
  safety sequence **voltage gate → verified backup → write → read-back verify**.
  These rules live here, not in drivers, so no driver can skip them.

### Format and engine crates
Each is standalone (pure parsing / logic, tested offline) and pulled in by the
apps that need it.

| Crate | Role |
|---|---|
| `opentune-a2l`, `opentune-xdf`, `opentune-ecuflash`, `opentune-ols` | Import ASAM A2L, TunerPro XDF, EcuFlash/RomRaider XML and WinOLS `.ols` map definitions (A2L/XDF export too) |
| `opentune-image` | Sparse addressed image model with Intel HEX / Motorola SREC I/O |
| `opentune-container` | VAG flash containers (FRF / ODX / ODX-F / SGO) |
| `opentune-cbf` | Mercedes CBF diagnostic-description parser |
| `opentune-coding` | Module coding engine (read / decode / edit / safe write-back) |
| `opentune-adaptation` | Adaptation channels, basic-settings routines, actuator tests |
| `opentune-servicefn` | Declarative service-function scripts run by one safe executor |
| `opentune-autoscan` | Full-vehicle module scan: presence, identity, stored DTCs |
| `opentune-logdef` | RomRaider-style `logger.xml` parser + live logging: per-channel polling and fast mode over UDS `0x2C` dynamic DIDs ([LIVE-LOGGING.md](LIVE-LOGGING.md)) |
| `opentune-logview` | Log file reader (Live log + generic CSV), series decimation/stats, map-overlay binning |
| `opentune-mapfind` | Single-image map finder: counted-table headers + axis pairs, scored candidates with evidence |
| `opentune-recipe` | Portable tune recipes: anchored patches + source identity, strict/relocating apply |
| `opentune-packs` | Versioned definition packs (defs, labels, channels, scripts, …) |
| `opentune-sa2` | Generic SA2 seed→key bytecode VM (caller-supplied program) |
| `opentune-scan` | Offline ECU-file scanner (family + identity strings) |
| `opentune-vin`, `opentune-dtc` | VIN / part-number decoding; DTC database + decoder |
| `opentune-udssim` | In-memory UDS ECU + DoIP entity for conformance tests |
| `opentune-attest` | Flash checksum attestation bound to an image's bytes |
| `opentune-db` | Control-unit reference database (ECU / TCU / AWD units in [`database/`](../database/)): load, search, validate |
| `opentune-candump` | Full CAN dump over a driver's `CanMonitor`: candump / CSV / Vector ASC writers, live stats, capture loop |
| `opentune-ffi` | UniFFI binding used by the Android app |

### Drivers — `crates/drivers/*`
Each implements `Interface`. CAN drivers typically implement the lighter
`CanLink` trait and let the core's `IsoTp` + `UdsClient` do the protocol work.

| Crate | Driver id | Hardware |
|---|---|---|
| `opentune-driver-mock` | `mock` | none — local `.bin` files |
| `opentune-driver-openport2` | `openport2` | Tactrix OpenPort 2.0 (CDC-ACM) |
| `opentune-driver-socketcan` | `socketcan` | any Linux CAN interface |
| `opentune-driver-elm327` | `elm327` | ELM327 / OpenTune's own K-line firmware |
| `opentune-driver-kkl` | `kkl` | KKL 409.1 raw K-line cable (FTDI/CH340) |
| `opentune-driver-kline-bridge` | `kline-bridge` | Arduino K-line bridge (OpenTune firmware) |
| `opentune-driver-slcan` | `slcan` | slcan USB-CAN (CANable/CANtact) |
| `opentune-driver-j2534` | `j2534` | generic J2534 PassThru (Windows) |
| `opentune-driver-gsusb` | `gsusb` | gs_usb / candleLight USB-CAN |
| `opentune-driver-elm327net` | `elm327net` | ELM327 over WiFi / Bluetooth |
| `opentune-driver-elm327-obd` | `elm327obd` | ELM327 serial, OBD/UDS diagnostics (exposes `diag()`) |
| `opentune-driver-vcar` | `vcar` | none — a simulated six-module vehicle ([`VIRTUAL-CAR.md`](VIRTUAL-CAR.md)) |

These twelve are registered in the CLI's `Registry` (`opentune drivers`). The
`opentune-driver-esp32` crate (ESP32-S3 open interface) is in the workspace but
not yet in the CLI registry.

Writing a new one is covered in [`DRIVERS.md`](DRIVERS.md).

## Data flow of a flash

1. The app picks a driver from the `Registry` and `open`s it with a
   `ConnectConfig` derived from the chosen `EcuDef`.
2. `read_ecu_id` confirms identity; the app loads the matching `EcuDef`.
3. For a calibration change, the app calls `opentune_flash::flash_calibration`
   with the target `MemoryRegion` and new bytes.
4. The orchestrator reads battery voltage (gate), takes a verified backup via
   `read_memory`, writes via `write_memory`, then reads back and compares.
5. `opentune-checksum` corrects the family checksums on the new image before it
   is written, using the algorithm named in the `EcuDef`.

Every long operation reports `Progress { phase, done, total }` so any app can
show a meaningful progress bar without knowing driver internals.
