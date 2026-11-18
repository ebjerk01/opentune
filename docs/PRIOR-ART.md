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
| [bri3d/VW_Flash](https://github.com/bri3d/VW_Flash) | ~450 | BSD-2-Clause | The reference for Simos/MEDC17 UDS flashing: security access, request-download/transfer-data sequencing, compression/encryption blocks and block checksums. Directly informed our UDS flash state machine and the `bench_boot` vs OBD `full_read` distinction. BSD → patterns are reusable. |
| [miikasyvanen/FastECU](https://github.com/miikasyvanen/FastECU) | ~250 | GPL-3.0 | A cross-platform open reflashing tool covering many OEM/protocol families; a model for how to structure a driver/protocol/definition split in a desktop tuning app. GPL-3.0 → compatible. |

## Checksums

| Project | Stars (approx) | Licence | What OpenTune learned |
|---|---|---|---|
| [360trev/ME7Sum](https://github.com/360trev/ME7Sum) | ~300 | BSD-style | The canonical write-up of Bosch ME7 multi-point/rom checksums (seed/csum pairs, region tables). Informed the structure of our checksum-handler abstraction and the ME-family notes. |
| [NetworkDir/moto-ecu-checksum](https://github.com/NetworkDir/moto-ecu-checksum) | ~60 | MIT | Compact, readable implementations of several ECU checksum variants; a cross-check for our EDC16 checksum logic. MIT → reusable. |

## Diagnostic & protocol libraries

| Project | Stars (approx) | Licence | What OpenTune learned |
|---|---|---|---|
| [rnd-ash/ecu_diagnostics](https://github.com/rnd-ash/ecu_diagnostics) | ~120 | GPL-3.0-only | A Rust UDS/KWP2000/OBD stack — the closest neighbour to `opentune-core`'s protocol layer. Validated our KWP2000/UDS service split and error-code modelling. GPL-3.0 → compatible. |
| [rnd-ash/OpenVehicleDiag](https://github.com/rnd-ash/OpenVehicleDiag) | ~400 | GPL-3.0 | A diagnostics suite with a CDF/ODX-style definition importer; informed how we think about lowering OEM definition formats into a neutral internal model. GPL-3.0 → compatible with OpenTune (reuse under GPL-3 terms, with attribution). |
| [rnd-ash/ultimate-nag52-fw](https://github.com/rnd-ash/ultimate-nag52-fw) | ~150 | GPL-3.0 | Open transmission-controller firmware; a reference for real-world UDS routine/IO-control usage and CAN framing on a live module. GPL-3.0 → compatible. |
| [pylessard/python-udsoncan](https://github.com/pylessard/python-udsoncan) | ~650 | MIT | The reference UDS (ISO 14229) client implementation; our service IDs, sub-functions, NRC handling and DID conventions follow its well-tested shape. MIT → reusable. |
| [pylessard/python-can-isotp](https://github.com/pylessard/python-can-isotp) | ~430 | MIT | The reference ISO 15765-2 (ISO-TP) implementation — single/first/consecutive/flow-control frames, block size and STmin. Directly informed `protocol/isotp.rs`. MIT → reusable. |
| [fenugrec/freediag](https://github.com/fenugrec/freediag) | ~160 | GPL-2.0-or-later | A long-standing K-line / ISO 9141 / ISO 14230 diagnostic stack; a reference for L-line init, timing and the older K-line protocols behind our `iso14230_kline`/`iso9141` transports. GPL-2.0-**or-later** → compatible with GPL-3.0. |
| [fenugrec/nisprog](https://github.com/fenugrec/nisprog) | ~160 | GPL-3.0 | Nissan ECU read/reflash over K-line; a worked example of a vendor-specific reprogramming sequence and its seed/key handling. GPL-3.0 → compatible. |

## Tuning suites & map/definition formats

| Project | Stars (approx) | Licence | What OpenTune learned |
|---|---|---|---|
| [RomRaider/RomRaider](https://github.com/RomRaider/RomRaider) | ~350 | GPL-2.0 | The community reference for an ECU-definition XML / TunerPro-XDF-style table model: `table` type (2D/3D), `storagetype`+`endian`, `sizex`/`sizey`, address, and per-axis sub-tables with scaling and min/max. The backbone of our `MapDef`/`AxisDef` shape. GPL-2.0-only → **learn-only**. |
| [christoph2/pyA2L](https://github.com/christoph2/pyA2L) | ~200 | GPL-2.0 | An ASAM A2L/ASAP2 parser — the OEM calibration description format. Taught us the `CHARACTERISTIC` / `AXIS_PTS` / `COMPU_METHOD` model and the three-way axis description (`STD_AXIS`/`COM_AXIS`/`FIX_AXIS`) that our axis styles mirror. GPL-2.0-only → **learn-only**. |

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
| [CaringCaribou/caringcaribou](https://github.com/CaringCaribou/caringcaribou) | ~800 | GPL-3.0 | A CAN/automotive security toolkit (UDS/ISO-TP discovery, DCM/XCM fuzzing); informed how we think about safely probing an unknown ECU before trusting a definition. GPL-3.0 → compatible. |

## Licence compatibility

OpenTune is **GPL-3.0-or-later**. The rules we follow when drawing on the work
above:

- **MIT / BSD (permissive)** — code and patterns may be reused in OpenTune,
  keeping the original copyright/licence notice. Covers VW_Flash, ME7Sum,
  moto-ecu-checksum, OpenVehicleDiag, python-udsoncan, python-can-isotp,
  cantools, SavvyCAN.
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
