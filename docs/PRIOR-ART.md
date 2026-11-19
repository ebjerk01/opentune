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
| [bri3d/sa2_seed_key](https://github.com/bri3d/sa2_seed_key) (PyPI `sa2-seed-key`) | — | MIT | The public reference implementation of the VAG SA2 seed/key VM: the ten opcodes, operand widths, the register/carry model, counted loops and forward byte-skip branches. `opentune-sa2` is written from those semantics (not translated), and the package is the oracle of its differential test ([tools/sa2](../tools/sa2/README.md)): synthetic programs only, test-only, never shipped. |
| [jpbede/sa2go](https://github.com/jpbede/sa2go) | — | MIT | A Go port of `sa2_seed_key`; cross-check of the SA2 semantics (it differs on `loop 0`, which OpenTune therefore refuses). |
| [jglim/UnlockECU](https://github.com/jglim/UnlockECU) | — | MIT | Its `VolkswagenSA2` provider is a C# port of `sa2_seed_key`; cross-check only (its `sub` never sets carry on a borrow, unlike the Python and Go versions OpenTune follows). |
| [mercedes-benz/odxtools](https://github.com/mercedes-benz/odxtools) | — | MIT | The ODX-F `SESSION/SECURITYS/SECURITY` schema (`SECURITY-METHOD`, `FW-SIGNATURE`, `FW-CHECKSUM`, `VALIDITY-FOR`) behind `opentune_container::odx::sa2_scripts`. |
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
| [rnd-ash/ultimate-nag52-fw](https://github.com/rnd-ash/ultimate-nag52-fw) | ~150 | GPL-3.0 | Open replacement TCU firmware for the Mercedes 722.6. Licence checked 2026-11-18 (GPL-3.0 `LICENSE`). Source of the `mercedes-egs5x-7226` controller definition, for three things only: its hand-made EGS51 CAN set (`lib/egs51_ecus/can_data.txt`, with the scaling in its CAN layers), the shape/axes of its shift, shift-time, fill, TCC and adaptation maps (structure only, no values), and the gear-ratio cross-check. Layouts transcribed into data; no code copied. Its other CAN matrices are not used (see [MERCEDES-722.6.md](MERCEDES-722.6.md#sources-and-provenance)). GPL-3.0 → compatible. |
| [rnd-ash/ultimate-nag52-fw-rs](https://github.com/rnd-ash/ultimate-nag52-fw-rs) | — | GPL-3.0 | Rust port of the same TCU (`egs_can`, `diag_common`, `flasher`, `bootloader`, `egs_pc_sim`). Used as a cross-check of the EGS51 matrix and for the large-NAG ratio table; its embedded `no_std` crates are not used. GPL-3.0 → compatible. |
| [pylessard/python-udsoncan](https://github.com/pylessard/python-udsoncan) | ~650 | MIT | The reference UDS (ISO 14229) client implementation; our service IDs, sub-functions, NRC handling and DID conventions follow its well-tested shape. Also the independent client in the `opentune-udssim` conformance harness ([tools/conformance](../tools/conformance/README.md)). MIT → reusable. |
| [jacobschaer/python-doipclient](https://github.com/jacobschaer/python-doipclient) | ~200 | MIT | A DoIP (ISO 13400-2) client; drives the simulator's DoIP entity (routing activation, alive check, NACKs) in the conformance harness. Test-only, not shipped. MIT → reusable. |
| [pylessard/python-can-isotp](https://github.com/pylessard/python-can-isotp) | ~430 | MIT | The reference ISO 15765-2 (ISO-TP) implementation — single/first/consecutive/flow-control frames, block size and STmin. Directly informed `protocol/isotp.rs`. MIT → reusable. |
| [fenugrec/freediag](https://github.com/fenugrec/freediag) | ~160 | GPL-2.0-or-later | A long-standing K-line / ISO 9141 / ISO 14230 diagnostic stack; a reference for L-line init, timing and the older K-line protocols behind our `iso14230_kline`/`iso9141` transports. GPL-2.0-**or-later** → compatible with GPL-3.0. |
| [fenugrec/nisprog](https://github.com/fenugrec/nisprog) | ~160 | GPL-3.0 | Nissan ECU read/reflash over K-line; a worked example of a vendor-specific reprogramming sequence and its seed/key handling. GPL-3.0 → compatible. |

## VAG KWP2000 adaptation and basic settings

| Project | Stars (approx) | Licence | What OpenTune learned |
|---|---|---|---|
| [gmenounos/kw1281test](https://github.com/gmenounos/kw1281test) | — | MIT | Licence checked 2026-11-19 (MIT `LICENSE.txt`). The VAG adaptation workflow (read / test / save of a 16-bit channel value, save with a workshop code; KWP1281 block titles `0x21`/`0x22`/`0x2A`, basic settings `0x28`) and, for KWP2000, the startRoutineByLocalIdentifier family behind the `kwp_*` defaults of `opentune-adaptation`: routine ids `B8` (enter, `32 B8` exits), `B9` (select channel / test value), `BA` (read), `BB` (save + 6-byte workshop code), the `01 03` adaptation and `00 <group>` basic-settings selectors, the read-record layouts, and SecurityAccess level `03`/`04`. Observed there on EDC16 over K-line, so **unconfirmed** for other modules. Its command set also informs the **basic settings without login** default: `BasicSetting GROUP` and `GroupRead GROUP` take no login, while `AdaptationRead`/`AdaptationTest`/`AdaptationSave` take an optional `LOGIN` (README usage, checked 2026-11-19). OpenTune therefore tries basic settings without SecurityAccess and logs in only on NRC `0x33`. That choice is OpenTune's own and is untested on a real car. Facts only, no code copied; its key computation is **not** used (OpenTune ships no VAG login algorithm). See [ADAPTATION.md](ADAPTATION.md#kwp2000-vag-tp20--k-line). |
| [michaeldove/vag-diag-sim](https://github.com/michaeldove/vag-diag-sim) | ~12 | MIT | A TP2.0 KWP2000 module simulator that answers session `10 89` and the `31 B8` routine ("basic measuring blocks"): a second, independent sighting of the session byte and the `B8` routine id. |

## Subaru Select Monitor (SSM2)

| Project | Stars (approx) | Licence | What OpenTune learned |
|---|---|---|---|
| [RomRaider/RomRaider](https://github.com/RomRaider/RomRaider) | ~350 | GPL-2.0 | The SSM2 logging protocol as its logger uses it (the `80 <dst> <src> <len>` packet with an 8-bit sum checksum, tester `F0` / ECU `10` / TCU `18`, 4800 baud, `BF` init, `A8` address reads, the K-line echo) and the `logger.xml` selection rules `opentune-logdef` follows: `ecubyteindex`/`ecubit` index the init reply's data from the SSM ID, so the capability bitmap starts at index 8; `<ecuparam>` addresses are chosen by ROM ID; `target` 1/2/3 = ECU/TCU/both. GPL-2.0-only → **learn-only**: the protocol and format facts only, own implementation, no code read across. |
| [Comer352L/FreeSSM](https://github.com/Comer352L/FreeSSM) | — | GPL-3.0 | An independent SSM2 implementation (K-line and CAN): a second description of the packet layout, the `BF`/`A8`/`A0`/`B8` commands and their `+0x40` replies, and SSM payloads carried over ISO 15765 on `7E0`/`7E8`. Facts only, no code copied. GPL-3.0 → compatible. |

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
| [commaai/opendbc](https://github.com/commaai/opendbc) `vw_pq.dbc` / `vw_mqb.dbc` | — | MIT | Signal layouts of `Allrad_1`, `Bremse_4`, `Bremse_3`, `Motor_1` and `ESP_19` for the passive Haldex decoder (and the Haldex controller definitions in `ctrldefs/`). |

Gear ratios in `ctrldefs/` (722.6, ZF 8HP) are public mechanical facts from the
Wikipedia transmission articles; no text is reproduced.
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

## C166 / C167 bootstrap loader (EDC15 bootstrap-mode read)

| Project / document | Stars (approx) | Licence | What OpenTune learned |
|---|---|---|---|
| Siemens *SAB 80C166 Family ApNotes — Bootstrap Loader C165/C167* (ed. 1.94, 1996) | — | public vendor document | The BSL itself: entry by `P0L.4` low through a hardware reset (≈ 8 kΩ pull-down; `P0L.2/3/5` high), the zero byte the BSL times for its baud rate, identification bytes `55h`/`A5h`/`B5h`, exactly 32 bytes stored at `00'FA40h..00'FA5Fh` then executed, the register state in BSL mode (watchdog off, `SYSCON` `0E00h`), the `T6`/`S0BRL` autobaud formula and the ±2.5 % deviation limit (`protocol::baud_check` reproduces the note's worked example), the `ROMS1`/`ROMEN` mapping, and its warning about half-duplex links. Facts only; its example assembly is not used. |
| Infineon C167CR derivatives user manual V3.1, ch. 14 (as quoted on comp.arch.embedded, [embeddedrelated.com](https://www.embeddedrelated.com/showthread/comp.arch.embedded/7562-1.php)) | — | public vendor document | The later identification bytes: `C5h` = C167 derivatives, `D5h` = devices with identification registers. |
| [EcuProg7/C167BootTool](https://github.com/EcuProg7/C167BootTool) (`ME7BootTool.py`) | — | GPL-3.0 | The monitor-kernel protocol `opentune-c167bsl` speaks: command bytes (`0x93` test, `0x82` write word, `0xCD` read word, `0x85` read block, `0x33` block checksum), acknowledges `AAh`/`EAh`, reply shapes, the XOR block checksum, the K-line echo handling, `AAh` on the hello meaning "kernel already running", the `01h`/`03h` loader/kernel started bytes, the SFR addresses, and the register values of the presets (internal-ROM read: `SYSCON F604h`, buses off, read from `01'0000h`; EDC15 external flash: `ADDRSEL3 4008h`, `BUSCON3 848Eh`, `BUSCON0 04ADh`, `SYSCON E204h`). Fresh Rust implementation; no code copied. GPL-3.0 → compatible. |
| [NefMoto/NefMotoOpenSource](https://github.com/NefMoto/NefMotoOpenSource) | — | GPL-3.0 | Bootmode derived from C167BootTool; its EDC15 external-flash read setup (register order, `BUSCON1 040Dh`) and docs give the EDC15 flash base `0x400000`, the `FDh` NAK seen with CH340 cables, the bench entry practice (strap at power-up, release after 2–3 s), the 57600/38400 baud advice for KKL cables, and the observation that a lone hello echo means no BSL answered. GPL-3.0 → compatible. |

Both GPL-3 tools bundle **Infineon MiniMon**'s K-line primary loader and monitor kernel (`LOADK.bin`, 32 bytes; `MINIMONK.bin`, 394 bytes) as binaries only. MiniMon is Infineon freeware, not GPL and without source, so OpenTune does **not** ship it: the loader and kernel are user-supplied files, recognised by SHA-256 only (`protocol::KNOWN_PAYLOADS`). See [C167-BSL.md](C167-BSL.md).

## Licence compatibility

OpenTune is **GPL-3.0-or-later**. The rules we follow when drawing on the work
above:

- **MIT / BSD (permissive)** — code and patterns may be reused in OpenTune,
  keeping the original copyright/licence notice. Covers VW_Flash, ME7Sum,
  moto-ecu-checksum, OpenVehicleDiag, python-udsoncan, python-can-isotp,
  cantools, SavvyCAN, esp32-isotp-ble-bridge, kw1281test, vag-diag-sim.
- **GPL-3.0 / GPL-2.0-or-later** — compatible with our licence; code may be
  incorporated under GPL-3.0. Covers ecu_diagnostics, ultimate-nag52-fw, nisprog,
  FastECU, freediag (2-or-later), caringcaribou, rusEFI, msqur, C167BootTool,
  NefMotoOpenSource, FreeSSM.
- **GPL-2.0-only** — **not** compatible with GPL-3.0; these are **learn-only**.
  We study the format/algorithm and write our own implementation. Covers
  RomRaider, pyA2L, Speeduino and LibreTune.

In every case the design was informed by reading these projects; where a file in
OpenTune is a close adaptation of permissively-licensed or GPL-compatible code,
that is noted at the top of the file. Any resemblance to GPL-2.0-only projects is
conceptual only — their source was not copied.

## EGS52 groundwork

| Source | Licence | Use |
|---|---|---|
| [rnd-ash/EGS52_emu memory implementation](https://github.com/rnd-ash/EGS52_emu/blob/7c21a27f1cf7bbff844939acc1fa20451c8f8f15/src/c167/mod.rs#L399) | GPL-3.0 | C167 emulator flash base. No hardware setup values taken. |
| [AutoKeyProg EGS52 support list](https://autokeyprog.com.mx/actualizacion-agosto/), [board memory report](https://autoprogs.ru/diagnostika-mercedes-remont-elektroniki-doosnaschenie-170/kodirovka-egs52-7617/index18.html) | Public product listing and forum post, no open reuse licence identified | CPU, flash/EEPROM part names and capacities only. |
| [EGS52 coding discussion](https://www.smartland.am/threads/1126/) | Public forum, no open reuse licence identified | Reports CRC-16 recalculation for variant coding. The 40+2 layout and ARC hypothesis follow the project brief. No coding records or attachments used. |
| [Greg Cook, Catalogue of parametrised CRC algorithms](https://reveng.sourceforge.io/crc-catalogue/) | Public reference, no open licence identified for the catalogue text | Numerical CRC parameters and standard check values only. |
| [OpenTune Bosch checksum notes](BOSCH-CHECKSUM.md) and [VAG EDC15 loop](../crates/opentune-checksum/src/edc15.rs) | GPL-3.0-or-later | Public VAG loop, two accumulator seeds and finalisation for the generic search. |
