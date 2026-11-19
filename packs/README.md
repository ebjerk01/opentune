# Definition packs

Data that the coding / adaptation / service / auto-scan / live-log engines run
on. Each sub-folder with a `pack.json` is one pack; see
[`docs/DEFINITION-PACKS.md`](../docs/DEFINITION-PACKS.md) for the format and a
"writing a pack" walkthrough. Inspect them with `opentune packs packs`. The
desktop app bundles this folder and opens its file pickers here.

| Pack | Contents | License | Sources |
|------|----------|---------|---------|
| [`generic-obd2`](generic-obd2/) | Auto-scan module lists (ISO 15765-4 physical `0x7E0`-`0x7E7` and functional `0x7DF`), a mode-01 logger for 34 common PIDs, a datalogging trigger set (WOT pull on PID `0x0C`/`0x11`, coolant over-temperature mark), 6 service scripts (readiness, stored / pending / permanent DTCs, VIN, **mode-04 clear = write**) | GPL-3.0-or-later | SAE J1979 / ISO 15031-5, ISO 15765-4 |
| [`iso14229-identification`](iso14229-identification/) | Logger def of the identification DIDs `0xF180`-`0xF19F` (shown as text / BCD dates / hex records in the live logger), 4 read-only DID scripts (VIN, active session, spare part number, ECU serial) | GPL-3.0-or-later | ISO 14229-1 Annex C; names cross-checked with [opendbc `uds.py`](https://github.com/commaai/opendbc/blob/master/opendbc/car/uds.py) (MIT) |
| [`vag-modules`](vag-modules/) | Auto-scan list of 7 VAG modules on UDS/CAN (engine, DSG, airbag, EPS, ACC radar, front camera, Haldex) | GPL-3.0-or-later | [opendbc](https://github.com/commaai/opendbc/tree/master/opendbc/car/volkswagen) (MIT, © comma.ai), [bri3d/VW_Flash](https://github.com/bri3d/VW_Flash) (BSD-2-Clause, © Brian Ledbetter and contributors) — cited per entry |
| [`vag-mib2-coding`](vag-mib2-coding/) | Coding-label set for the long coding of the MIB2 High infotainment unit (module 5F, DID `0x0600`): 118 parameters over bytes 0-24 ([`docs/CODING.md`](../docs/CODING.md)) | MIT (notice in the pack) | [jilleb/mib2-toolbox](https://github.com/jilleb/mib2-toolbox) `mqb-coding.esd` (MIT, © 2019 Chillout); DID from [bri3d/VW_Flash](https://github.com/bri3d/VW_Flash) (BSD-2-Clause) |
| [`_template`](_template/) | Copy-me scaffold (not loaded: `_`-prefixed folders are skipped) | GPL-3.0-or-later | — |

## Provenance rules

- Standards (ISO 14229, ISO 15031 / SAE J1979, ISO 15765-4) are used by their
  published identifiers and formulas only.
- Every non-standard fact (a module's CAN id, a DID's meaning) carries a
  `source` naming an open, license-compatible project and the file it came
  from. If there is no such source, the entry is left out — the VAG pack has
  no VAG "address words" for that reason.
- Nothing from commercial / OEM tools (their label and coding data is
  copyrighted or encrypted), no seed/key algorithms, no VINs or other personal
  data.
- Every `pack.json` records `sources`, `license` and `evidence` (the
  validation state, see [`docs/DEVELOPMENT.md`](../docs/DEVELOPMENT.md) §7).
  `opentune packs validate packs` checks all of this plus a privacy scan; CI
  runs it on every pull request.

## Notes on the shipped data

- `addr` in the module lists is the low byte of the 11-bit request id (11-bit
  normal addressing has no separate logical address). It is **not** a VAG
  address word.
- `vag-modules` is UDS on CAN only. opendbc's fingerprints show these ids
  answering UDS on MQB, MEB and late PQ cars; older modules that speak
  KWP2000 over VW TP2.0 are not listed because the auto-scan engine only
  speaks UDS and OBD. Its Haldex entry (`0x70F`/`0x779`) is the MQB **Gen5**
  unit; the PQ-era **Haldex Gen4** (e.g. Passat CC 4Motion) is KWP2000 over
  TP2.0 at logical address `0x0A`, which this module-list schema cannot
  express — read it with `opentune haldex gen4` instead (see
  [`docs/HALDEX.md`](../docs/HALDEX.md)).
- The generic OBD service scripts default to OBD ECU #1 (`req_id` `0x7E0` /
  `resp_id` `0x7E8`); the panel's Target fields override that. Mode 04 sent
  physically clears only the addressed ECU. Auto-scan → Clear with
  `obd2-physical.json` clears every OBD ECU.
- The identification logger reads each DID as a number (first 8 bytes). ASCII
  DIDs are easier to read through Auto-scan's identity block or the read-only
  service scripts, whose step report shows the response bytes.
