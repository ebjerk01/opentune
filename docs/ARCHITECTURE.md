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
| `opentune read` | Read a memory region to a file; `--plan` prints the requests without sending them |
| `opentune diff` | Byte-diff two images and summarise changed regions |
| `opentune checksum` | Verify / correct checksums; `checksum algos` lists the algorithms |
| `opentune flash` | Write a region through the safety orchestrator (backup → write → verify) |
| `opentune dump` | Full read via a driver, archived to the backup vault with ECU id |
| `opentune findmaps` | Find candidate calibration maps in one image (scored, with evidence), or from the diff of two images |
| `opentune recipe` | Portable tune recipes: `create` from a stock/tuned pair, `apply` only to a matching image (checksum-fixed + attested), `show` |
| `opentune ecudefs` | List / validate ECU-definition files; `relocate` moves a definition's maps onto a sibling software's read by content signature |
| `opentune identify` | Read the connected ECU's identity and match it to a definition |
| `opentune db` | Control-unit reference database: list, show, validate |
| `opentune dtc` | Look up a diagnostic trouble code |
| `opentune vin` | Decode a VIN |
| `opentune decode-part` | Parse a VAG or Bosch ECU part number |
| `opentune compare` | Compare two calibrations map-by-map through an ECU definition |
| `opentune import-xdf` | Import a TunerPro XDF definition (per-object clean/warned/refused report, `-o` JSON, `--strict`) |
| `opentune import-ecuflash` | Import an EcuFlash / RomRaider XML ROM definition |
| `opentune import-fastecu` | Import a definition the way FastECU reads it (EcuFlash/RomRaider XML + FastECU extensions, or a WinOLS MapPack CSV; `--rom`, `--base-dir`, `-o`, `--strict`) |
| `opentune import-ols` | Import a WinOLS `.ols` project's map definitions |
| `opentune import-a2l` | Import an ASAM A2L description |
| `opentune a2l2xdf` | Convert an A2L straight into a TunerPro XDF |
| `opentune export-xdf` | Export an ECU definition's maps as a TunerPro XDF |
| `opentune export-a2l` | Export an ECU definition's maps as an ASAM A2L |
| `opentune packs` | Load definition packs and report their contents |
| `opentune vault` | List archived reads in a backup vault directory |
| `opentune scan` | Scan a directory of ECU dumps offline, emit identification JSON |
| `opentune container` | Decode a VAG flash container (FRF / ODX / ODX-F / SGO); lists the SA2 seed/key scripts it declares |
| `opentune seedkey` | Run a user-supplied seed/key script offline: `seedkey sa2` computes a VAG SA2 key from a script (`--script` hex, `--script-file`, or `--container` your own FRF/ODX/SGO) and a seed, `--disasm` lists it, `--trace` steps it ([`SEEDKEY.md`](SEEDKEY.md#vag-sa2-scripts)) |
| `opentune cbf` | Parse a Mercedes CBF diagnostic-description file |
| `opentune coding` | Module coding over UDS (read, decode, edit, write) |
| `opentune adaptation` | Adaptation channels: UDS value / routine / actuator, and VAG KWP2000 over TP2.0 or K-line (`kwp-read`, `kwp-basic`, `kwp-test`, `kwp-save`; [`ADAPTATION.md`](ADAPTATION.md)) |
| `opentune autoscan` | Scan every module in a vehicle map: presence, identity, DTCs |
| `opentune service` | Plan or run a service-function script |
| `opentune log` | Log live data from a RomRaider-style `logger.xml` to CSV (`--fast`: UDS `0x2C` dynamic DIDs; `--gps <port\|file>`: GPS channels + `speed_error_kmh`, see [`LIVE-LOGGING.md`](LIVE-LOGGING.md#gps-channels)) |
| `opentune gps` | GPS/GNSS receiver (NMEA 0183 over serial / USB CDC-ACM, e.g. u-blox): one CSV row per fix with NMEA line counters, replay a recorded NMEA file, `--list-ports` ([`LIVE-LOGGING.md`](LIVE-LOGGING.md#gps-channels)) |
| `opentune obd` | Generic OBD-II: mode-01 live data, readiness, freeze frame, mode-09 info (VIN/CALID/CVN), stored/pending/permanent DTCs |
| `opentune candump` | Capture a CAN bus to a candump / CSV / ASC log, listen-only where possible |
| `opentune simulate` | Exchange raw requests with the offline UDS conformance server |
| `opentune haldex` | Haldex AWD: `decode` Haldex CAN frames in a log, read-only `gen4` controller read (KWP2000/TP2.0), OpenHaldex `status` and `mode list`/`set` (see [`HALDEX.md`](HALDEX.md)) |
| `opentune dsg` | VAG DQ250 02E DSG gearbox, read-only, KWP2000 over VW TP2.0 at `0x02`: `identify` (0x1A), `dtcs` (0x18), `groups <n...>` (0x21, numbered raw/formula-decoded cells, meanings only from the `vag-dq250-02e` controller definition); `--sim`, `--json` (see [`DSG.md`](DSG.md#read-only-diagnostics-opentune-dsg)) |
| `opentune doip` | DoIP (ISO 13400) over Ethernet: `discover` entities by vehicle identification request (all / `--eid` / `--vin`, broadcast, unicast or IPv6 multicast) or `--listen` for announcements, optionally `--activate` routing on each; `status` (entity status) and `power` (diagnostic power mode) over UDP; `--sim` for the simulated entity, `--json` |
| `opentune controller` | Drivetrain controller definitions (TCU, AWD; [`ctrldefs/`](../ctrldefs/)): `list`, `show`, `validate`, `match` a part number / gearbox code, `decode` a CAN log with a definition's frame sets, e.g. Mercedes 722.6 EGS51 (see [`CONTROLLER-DEFS.md`](CONTROLLER-DEFS.md)) |
| `opentune ts` | TunerStudio-compatible aftermarket ECUs (Speeduino, rusEFI, MegaSquirt): `ini info`/`tables`, `msq show`/`diff`, `read`, `write` (dry run unless `--confirm`; backup, verify, burn), `log`; `--port sim` for the simulated ECU ([`AFTERMARKET-ECUS.md`](AFTERMARKET-ECUS.md)) |
| `opentune bsl` | C167/ST10 bootstrap-mode memory read for EDC15 boards: `presets`, `plan` (dry run, every byte exchange), `read` (unverified on hardware, needs `--i-understand`; `--port sim` for the simulated C167) ([`C167-BSL.md`](C167-BSL.md)) |

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
  - `vagmeas` — VAG measuring-block cells and the one shared formula table
    (used by `opentune-haldex` and `opentune-dsg`).
  - `canfd` — CAN-FD frames + `IsoTpFd` (ISO-TP over CAN-FD).
  - `doip` — `DoIpTransport` (ISO 13400 diagnostics over IP).
  - `doip_discovery` — `DoIpUdpClient`: UDP vehicle identification (all / by
    EID / by VIN, de-duplicated), announcement listening, entity status and
    power mode, strict header checks; `connect_entity` turns a discovered
    entity into an activated `DoIpTransport`. The `doip` driver
    (`opentune-driver-doip`) wraps both behind `Interface`.
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
  `opentune_flash::module` programs a control module over UDS from a decoded
  container (erase → 0x34/0x36/0x37 → checksum routine per block, resumable)
  under the same policy — see [FLASHING.md](FLASHING.md).

### Format and engine crates
Each is standalone (pure parsing / logic, tested offline) and pulled in by the
apps that need it.

| Crate | Role |
|---|---|
| `opentune-a2l`, `opentune-xdf`, `opentune-ecuflash`, `opentune-ols` | Import ASAM A2L, TunerPro XDF, EcuFlash/RomRaider XML (plus FastECU's reading of them and MapPack CSV, `opentune_ecuflash::fastecu`) and WinOLS `.ols` map definitions (A2L/XDF export too). `opentune_xdf::report` is the shared per-object clean/warned/refused report |
| `opentune-image` | Sparse addressed image model with Intel HEX / Motorola SREC I/O |
| `opentune-container` | VAG flash containers (FRF / ODX / ODX-F / SGO) |
| `opentune-cbf` | Mercedes CBF diagnostic-description parser |
| `opentune-coding` | Module coding engine (read / decode / edit / safe write-back) |
| `opentune-adaptation` | Adaptation channels, basic-settings routines, actuator tests over UDS; VAG KWP2000 measuring blocks, basic settings and adaptation read/test/save over TP2.0 or K-line (`kwp`, data-defined `kwp_*` channel kinds, gated writes, synthetic module in `sim`) ([`ADAPTATION.md`](ADAPTATION.md)) |
| `opentune-servicefn` | Declarative service-function scripts run by one safe executor |
| `opentune-autoscan` | Full-vehicle module scan: presence, identity, stored DTCs |
| `opentune-logdef` | RomRaider-style `logger.xml` parser + live logging: per-channel polling and fast mode over UDS `0x2C` dynamic DIDs ([LIVE-LOGGING.md](LIVE-LOGGING.md)) |
| `opentune-gps` | Optional GPS/GNSS source for live logging: own NMEA 0183 parser (`$--RMC`/`GGA`/`VTG`, any talker, checksum-verified, rejected lines counted), latest-fix tracker with staleness, background serial reader (u-blox style CDC-ACM, no gpsd) and deterministic file replay; `opentune_logdef::gps` turns it into `gps_*` channels ([LIVE-LOGGING.md](LIVE-LOGGING.md#gps-channels)) |
| `opentune-logview` | Log file reader (Live log + generic CSV), series decimation/stats, map-overlay binning |
| `opentune-mapfind` | Single-image map finder: counted-table headers + axis pairs, scored candidates with evidence |
| `opentune-recipe` | Portable tune recipes: anchored patches + source identity, strict/relocating apply |
| `opentune-packs` | Versioned definition packs (defs, labels, channels, scripts, …) |
| `opentune-sa2` | VAG SA2 seed→key script VM (real instruction set; the script is always the user's own), disassembler and assembler |
| `opentune-scan` | Offline ECU-file scanner (family + identity strings) |
| `opentune-vin`, `opentune-dtc` | VIN / part-number decoding; DTC database + decoder |
| `opentune-udssim` | In-memory UDS ECU + DoIP entity (TCP data side and UDP discovery / status side, loopback socket servers) for conformance tests; optional flashable-ECU model with fault injection |
| `opentune-attest` | Flash checksum attestation bound to an image's bytes |
| `opentune-db` | Control-unit reference database (ECU / TCU / AWD units in [`database/`](../database/)): load, search, validate |
| `opentune-candump` | Full CAN dump over a driver's `CanMonitor`: candump / CSV / Vector ASC writers, live stats, capture loop |
| `opentune-haldex` | Haldex AWD: read-only Gen4 KWP2000/TP2.0 engine, passive Haldex CAN decoder + candump/SavvyCAN log readers, OpenHaldex backend (modes, 0x6B0/0x6A0 frames, UDS DIDs, BLE status), simulated units ([`HALDEX.md`](HALDEX.md)) |
| `opentune-dsg` | VAG DQ250 02E read-only diagnostics engine: KWP2000 over a core TP2.0 channel to address `0x02` — identification, DTCs with KWP status-byte decode, measuring groups as unconfirmed numbered cells (shared VAG formula table in `opentune_core::protocol::vagmeas`), meanings hooked from the controller definition's `live[]`; scripted DQ250 responder ([`DSG.md`](DSG.md)) |
| `opentune-ctrldef` | Data-defined drivetrain controller definitions ([`ctrldefs/`](../ctrldefs/), schema `docs/ctrldef.schema.json`): identity, ecudef/unit links, diagnostic channels, DBC-style CAN frame sets, live channels, adaptations, tables with axes, gear ratios; loader, validation, CAN decoder + log summary ([`CONTROLLER-DEFS.md`](CONTROLLER-DEFS.md), [`MERCEDES-722.6.md`](MERCEDES-722.6.md)) |
| `opentune-tsini` | TunerStudio INI ECU definitions (pages, constants, output channels, table/curve editors, menus, `#if` conditions) and `.msq` tunes; constant ↔ page-byte mapping, realtime channel decoding |
| `opentune-tsproto` | Serial protocol for those ECUs from the INI's command strings: msEnvelope CRC32 or plain framing, chunked page read/write, burn, CRC32 check, realtime block; verified-write orchestration and a simulated ECU |
| `opentune-c167bsl` | C167/ST10 bootstrap-loader (BSL) client, read only: autobaud + ID byte, 32-byte loader, user-supplied monitor kernel, register setup with read-back, checksummed block reads; EDC15 presets as data, dry-run plan, simulated BSL responder ([`C167-BSL.md`](C167-BSL.md)) |
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
| `opentune-driver-efp02c` | `efp02c` | EFI Technology EFP02C USB-CAN, via `nusb` (unverified: 1 Mbit/s, 11-bit, no writes) |
| `opentune-driver-vcar` | `vcar` | none — a simulated six-module vehicle ([`VIRTUAL-CAR.md`](VIRTUAL-CAR.md)) |
| `opentune-driver-isotp-ble` | `isotp-ble` | ESP32 running the open-source esp32-isotp-ble-bridge firmware, over USB serial or BLE (optional `btleplug` feature); unverified ([`ISOTP-BLE-BRIDGE.md`](ISOTP-BLE-BRIDGE.md)) |
| `opentune-driver-doip` | `doip` | none — an Ethernet port to the vehicle's DoIP (ISO 13400) entity: UDS over TCP after routing activation, optional UDP discovery (exposes `diag()`; loopback-tested only, [`DRIVERS.md`](DRIVERS.md#3i-doip-uds-over-ethernet-iso-13400)) |

These fifteen are registered in the CLI's `Registry` (`opentune drivers`); the
desktop lists `isotp-ble` and `doip` in alpha builds only (Bench tier). The
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
