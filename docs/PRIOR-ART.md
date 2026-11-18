# Prior art and acknowledgements

OpenTune stands on a large body of open-source ECU, diagnostics and CAN work.
This file credits the projects we surveyed while designing OpenTune's protocol
stack, checksum handlers, ECU-definition format and calibration-map format, and
records what we learned from each. None of their code is copied into this
GPL-3.0 tree except where the licence explicitly permits it (see
[Licence compatibility](#licence-compatibility)); GPL-2.0-only projects are
**learn-only** references.

Star counts are **approximate ballparks** at the time of writing — they move, and
they are here only to indicate relative reach, not to rank quality.

## VAG flashing & boot/bench tooling

| Project | Stars (approx) | Licence | What OpenTune learned |
|---|---|---|---|
| [bri3d/VW_Flash](https://github.com/bri3d/VW_Flash) | ~450 | BSD-2-Clause | The reference for Simos/MEDC17 UDS flashing: security access, request-download/transfer-data sequencing, compression/encryption blocks and block checksums. Directly informed our UDS flash state machine and the `bench_boot` vs OBD `full_read` distinction. Its `simos_hsl` logger (define an address list once, read it back in one request, slice by length) informed the `0x2C` fast-logging mode ([LIVE-LOGGING.md](LIVE-LOGGING.md)). BSD → patterns are reusable. |
| [bri3d/mqb-flash-rs](https://github.com/bri3d/mqb-flash-rs) | — | MIT | Rust port of the MQB flashing modules; its DQ250-MQB block table (lengths, bin-file layout, box-code and version locations) is the layout behind `temic-dq250-mqb`. See `docs/DSG.md`. |
| [TheFlashBold/DQ250-MQB-Bench-Flash](https://github.com/TheFlashBold/DQ250-MQB-Bench-Flash) | — | GPL-3.0 | DQ250-MQB SBOOT bench read/flash (TC1766) and the per-block JAMCRC behind the `dq250` checksum. |
| [dspl1236/vag-tcu-tools](https://github.com/dspl1236/vag-tcu-tools) | — | MIT | DQ381 / ZF 8HP CRC-32 layouts and the FRF method notes for DQ200/DQ250/DL382 containers. |
| [miikasyvanen/FastECU](https://github.com/miikasyvanen/FastECU) | ~250 | GPL-3.0 | A cross-platform open reflashing tool covering many OEM/protocol families; a model for how to structure a driver/protocol/definition split in a desktop tuning app. GPL-3.0 → compatible. |

## Checksums

| Project | Stars (approx) | Licence | What OpenTune learned |
|---|---|---|---|
| [360trev/ME7Sum](https://github.com/360trev/ME7Sum) | ~300 | BSD-style | The canonical write-up of Bosch ME7 multi-point/rom checksums (seed/csum pairs, region tables). Informed the structure of our checksum-handler abstraction and the ME-family notes. |
| [NetworkDir/moto-ecu-checksum](https://github.com/NetworkDir/moto-ecu-checksum) | ~60 | MIT | Compact, readable implementations of several ECU checksum variants; a cross-check for our EDC16 checksum logic. MIT → reusable. |
| [LeZed97/ZedSuite](https://github.com/LeZed97/ZedSuite) | — | GPL-3.0 | Independent EDC15 (VAG TDI v4.1, three layouts) and EDC16 (FADECAFE region, sum to `0xD01FE500`) checksum code. Run unmodified on our fixture dumps as a cross-validation of `edc15`/`edc16` (results in [BOSCH-CHECKSUM.md](BOSCH-CHECKSUM.md#cross-validation-against-zedsuite-2026-11-18)); no code taken. GPL-3.0 → compatible. |

## Diagnostic & protocol libraries

| Project | Stars (approx) | Licence | What OpenTune learned |
|---|---|---|---|
| [rnd-ash/ecu_diagnostics](https://github.com/rnd-ash/ecu_diagnostics) | ~120 | GPL-3.0-only | A Rust UDS/KWP2000/OBD stack — the closest neighbour to `opentune-core`'s protocol layer. Validated our KWP2000/UDS service split and error-code modelling. GPL-3.0 → compatible. |
| [rnd-ash/OpenVehicleDiag](https://github.com/rnd-ash/OpenVehicleDiag) | ~400 | GPL-3.0 | A diagnostics suite with a CDF/ODX-style definition importer; informed how we think about lowering OEM definition formats into a neutral internal model. GPL-3.0 → compatible with OpenTune (reuse under GPL-3 terms, with attribution). |
| [rnd-ash/ultimate-nag52-fw](https://github.com/rnd-ash/ultimate-nag52-fw) | ~150 | GPL-3.0 | Open transmission-controller firmware; a reference for real-world UDS routine/IO-control usage and CAN framing on a live module. GPL-3.0 → compatible. |
| [pylessard/python-udsoncan](https://github.com/pylessard/python-udsoncan) | ~650 | MIT | The reference UDS (ISO 14229) client implementation; our service IDs, sub-functions, NRC handling and DID conventions follow its well-tested shape. Also the independent client in the `opentune-udssim` conformance harness ([tools/conformance](../tools/conformance/README.md)). MIT → reusable. |
| [jacobschaer/python-doipclient](https://github.com/jacobschaer/python-doipclient) | ~200 | MIT | A DoIP (ISO 13400-2) client; drives the simulator's DoIP entity (routing activation, alive check, NACKs) in the conformance harness. Test-only, not shipped. MIT → reusable. |
| [pylessard/python-can-isotp](https://github.com/pylessard/python-can-isotp) | ~430 | MIT | The reference ISO 15765-2 (ISO-TP) implementation — single/first/consecutive/flow-control frames, block size and STmin. Directly informed `protocol/isotp.rs`. MIT → reusable. |
| [fenugrec/freediag](https://github.com/fenugrec/freediag) | ~160 | GPL-2.0-or-later | A long-standing K-line / ISO 9141 / ISO 14230 diagnostic stack; a reference for L-line init, timing and the older K-line protocols behind our `iso14230_kline`/`iso9141` transports. GPL-2.0-**or-later** → compatible with GPL-3.0. |
| [fenugrec/nisprog](https://github.com/fenugrec/nisprog) | ~160 | GPL-3.0 | Nissan ECU read/reflash over K-line; a worked example of a vendor-specific reprogramming sequence and its seed/key handling. GPL-3.0 → compatible. |

## Tuning suites & map/definition formats

| Project | Stars (approx) | Licence | What OpenTune learned |
|---|---|---|---|
| [RomRaider/RomRaider](https://github.com/RomRaider/RomRaider) | ~350 | GPL-2.0 | The community reference for an ECU-definition XML / TunerPro-XDF-style table model: `table` type (2D/3D), `storagetype`+`endian`, `sizex`/`sizey`, address, and per-axis sub-tables with scaling and min/max. The backbone of our `MapDef`/`AxisDef` shape. GPL-2.0-only → **learn-only**. |
| [miikasyvanen/FastECU](https://github.com/miikasyvanen/FastECU) | — | GPL-3.0 | Subaru flashing/tuning suite that reads EcuFlash and RomRaider definitions directly. Studied for how it interprets them — hex `startpos`/`interval` cell addressing, switch/selection tables, big-endian floats, the `wrx02` 0x8000 address fold, `<include>`/`base=` resolution — and its WinOLS MapPack-CSV column layout. `opentune_ecuflash::fastecu` is a fresh implementation of that behaviour; no source copied. GPL-3.0 → compatible. |
| [christoph2/pyA2L](https://github.com/christoph2/pyA2L) | ~200 | GPL-2.0 | An ASAM A2L/ASAP2 parser — the OEM calibration description format. Taught us the `CHARACTERISTIC` / `AXIS_PTS` / `COMPU_METHOD` model and the three-way axis description (`STD_AXIS`/`COM_AXIS`/`FIX_AXIS`) that our axis styles mirror. GPL-2.0-only → **learn-only**. |

## Haldex AWD

| Project | Stars (approx) | Licence | What OpenTune learned |
|---|---|---|---|
| [Forbes-Automotive/OpenHaldex-C6](https://github.com/Forbes-Automotive/OpenHaldex-C6) | — | MIT | The OpenHaldex controller: its mode set, CAN broadcast/command frames, BLE contract, supplier UDS DIDs, the Gen4 TP2.0 address and measuring groups, Gen4/Gen5 status-byte meanings, and public PQ/MQB CAN captures used as test vectors. `opentune-haldex` is written fresh from these facts (see [`HALDEX.md`](HALDEX.md)). |
| [dandwyer/OpenHaldex-S3](https://github.com/dandwyer/OpenHaldex-S3), [ABangingDonk/OpenHaldexDue](https://github.com/ABangingDonk/OpenHaldexDue) | — | MIT | Cross-checks of the same Haldex frame layouts and the project lineage. |
| [commaai/opendbc](https://github.com/commaai/opendbc) `vw_pq.dbc` / `vw_mqb.dbc` | — | MIT | Signal layouts of `Allrad_1`, `Bremse_4`, `Bremse_3`, `Motor_1` and `ESP_19` for the passive Haldex decoder. |
## Aftermarket ECUs (TunerStudio-compatible)

| Project | Stars (approx) | Licence | What OpenTune learned |
|---|---|---|---|
| [noisymime/speeduino](https://github.com/noisymime/speeduino) | ~1.5k | GPL-2.0 | The INI definition format as a large real example (`reference/speeduino.ini`: pages, `lastOffset` aliases, `#if` settings, expression channels) and, from `comms.cpp`, the msEnvelope framing facts (big-endian length and CRC32, status codes, `p`/`M`/`b`/`d`/`r` commands). Read for format/protocol facts only → **learn-only**; `opentune-tsini`/`-tsproto` are independent implementations. |
| [rusefi/rusefi](https://github.com/rusefi/rusefi) | ~1k | GPL-3.0 | Generated board INIs (large single-page layouts, F32 constants, empty burn commands) and the ECU side of the binary protocol in `tunerstudio*.cpp` (`S`/`R`/`C`/`B`/`k`/`O`, response codes, signature with NUL). GPL-3.0 → compatible; no code copied. |
| [nearwood/msqur](https://github.com/nearwood/msqur) | ~30 | GPL-3.0 | An MSQ viewer whose collection of MS1/MS2/MS2Extra/MS3 INIs served as parser test input (kept outside the repo). |
| [RallyPat/LibreTune](https://github.com/RallyPat/LibreTune) | ~50 | GPL-2.0 | An open TunerStudio-compatible tuner in Rust; surveyed as a behaviour reference for the overall feature set. GPL-2.0 → **learn-only**; no code used. |

TunerStudio (EFI Analytics) is proprietary: nothing from it is used; OpenTune
reads the INI and `.msq` formats as the open projects above document them.

## CAN tooling

| Project | Stars (approx) | Licence | What OpenTune learned |
|---|---|---|---|
| [cantools/cantools](https://github.com/cantools/cantools) | ~1.1k | MIT | DBC/signal handling: `scale`, `offset`, `minimum`, `maximum`, `unit`, byte order and signedness, with `physical = raw*scale + offset`. This is exactly the linear scaling model OpenTune adopted for map cells and axes. MIT → reusable. |
| [collin80/SavvyCAN](https://github.com/collin80/SavvyCAN) | ~1.1k | MIT | A CAN reverse-engineering/capture tool; a reference for frame capture workflows and signal discovery that sit upstream of writing a definition. MIT → reusable. |
| [Switchleg1/esp32-isotp-ble-bridge](https://github.com/Switchleg1/esp32-isotp-ble-bridge) (fork of [bri3d/esp32-isotp-ble-bridge](https://github.com/bri3d/esp32-isotp-ble-bridge)) | — | MIT (bundles lishen2/isotp-c, MIT) | ESP32 firmware that bridges BLE (or USB serial) to ISO-TP on CAN. Recommended as OpenTune's BLE companion firmware and **not vendored** (pinned at v1.03); the `isotp-ble` driver is an independent implementation of its host protocol, written up in [ISOTP-BLE-BRIDGE.md](ISOTP-BLE-BRIDGE.md). MIT → compatible. |
| [CaringCaribou/caringcaribou](https://github.com/CaringCaribou/caringcaribou) | ~800 | GPL-3.0 | A CAN/automotive security toolkit (UDS/ISO-TP discovery, DCM/XCM fuzzing); informed how we think about safely probing an unknown ECU before trusting a definition. GPL-3.0 → compatible. |

## Interface protocols from own-hardware captures

| Interface | Source | What OpenTune did |
|---|---|---|
| EFI Technology EFP02C USB-CAN (`efp02c` driver) | Captures of the project owner's own unit | The USB record layout was first identified during interoperability analysis of the vendor's Windows software, then implemented from captures of the owner's own EFP02C driven from Linux without the vendor software. No vendor code or data is included. Marked unverified until re-confirmed from a black-box capture of the vendor software; see [DRIVERS.md §3g](DRIVERS.md#3g-efp02c-efi-technology-efp02c-usb-can-unverified). |

The driver's USB access uses [kevinmehall/nusb](https://github.com/kevinmehall/nusb)
(Apache-2.0 OR MIT) as a library dependency.

## Licence compatibility

OpenTune is **GPL-3.0-or-later**. The rules we follow when drawing on the work
above:

- **MIT / BSD (permissive)** — code and patterns may be reused in OpenTune,
  keeping the original copyright/licence notice. Covers VW_Flash, ME7Sum,
  moto-ecu-checksum, OpenVehicleDiag, python-udsoncan, python-can-isotp,
  cantools, SavvyCAN, esp32-isotp-ble-bridge.
- **GPL-3.0 / GPL-2.0-or-later** — compatible with our licence; code may be
  incorporated under GPL-3.0. Covers ecu_diagnostics, ultimate-nag52-fw, nisprog,
  FastECU, freediag (2-or-later), caringcaribou, rusEFI, msqur.
- **GPL-2.0-only** — **not** compatible with GPL-3.0; these are **learn-only**.
  We study the format/algorithm and write our own implementation. Covers
  RomRaider, pyA2L, Speeduino and LibreTune.

In every case the design was informed by reading these projects; where a file in
OpenTune is a close adaptation of permissively-licensed or GPL-compatible code,
that is noted at the top of the file. Any resemblance to GPL-2.0-only projects is
conceptual only — their source was not copied.
