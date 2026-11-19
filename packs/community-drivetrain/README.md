# community-drivetrain

The first community definition pack, and the worked example of
[`docs/PACK-AUTHORING.md`](../../docs/PACK-AUTHORING.md). It carries three
drivetrain controller definitions that are not in the built-in
[`ctrldefs/`](../../ctrldefs/) set:

| File | Unit | Frames | Diagnostics | Validation |
|---|---|---|---|---|
| `controllers/hyundai-kia-tcu.json` | Hyundai / Kia automatic TCU, classic (non-CAN-FD) CAN | `TCU11` `0x111`, `TCU12` `0x112` (current gear), `TCU13` `0x113` | UDS `0x7E1`/`0x7E9`, identification `22 F100` (implemented, untested) | synthetic-tested |
| `controllers/hyundai-kia-4wd.json` | Hyundai / Kia 4WD coupling controller, classic CAN | `_4WD11` `0x428`, `_4WD12` `0x429`, `_4WD13` `0x42A`, ABS `WHL_SPD11` `0x386` | none (no open source) | synthetic-tested |
| `controllers/subaru-global-tcu.json` | Subaru Global platform transmission | `Transmission` `0x048` (gear), `CVT` `0x049`, ABS `Wheel_Speeds` `0x13A` | UDS `0x7E1`/`0x7E9`, identification `22 F182` (implemented, untested) | synthetic-tested |

Try one offline on a CAN log (it never transmits):

```sh
opentune controller show packs/community-drivetrain/controllers/hyundai-kia-tcu.json --signals
opentune controller decode packs/community-drivetrain/controllers/hyundai-kia-tcu.json capture.log
```

## Provenance

- **Signal layouts, names, scales and value tables**: [commaai/opendbc](https://github.com/commaai/opendbc)
  at commit `27fa1c5d8eed3238a0a78c52ff5abb6cdc807bd2` —
  `opendbc/dbc/generator/hyundai/hyundai_can.dbc`,
  `opendbc/dbc/generator/subaru/subaru_global_2017.dbc` and
  `opendbc/dbc/generator/subaru/_subaru_global.dbc`. MIT licence,
  Copyright (c) 2020, Comma.ai, Inc. (the MIT notice applies to the
  transcribed facts; the pack as a whole is GPL-3.0-or-later).
- **Vehicles, request ids and identification requests**: opendbc
  `opendbc/car/hyundai/{values,fingerprints}.py` and
  `opendbc/car/subaru/{values,fingerprints}.py` (same commit, MIT).
- **Response ids and the bus rate**: ISO 15765-4 (response = request + 8,
  500 kbit/s), by identifier only.

Nothing comes from OEM or commercial tools, decompiled software or anyone's
car: no VINs, serial numbers, part numbers or captures.

## What is and is not claimed

- **synthetic-tested**: every shipped frame is decoded from synthetic payloads
  that `crates/opentune-packs/tests/community_drivetrain.rs` builds
  independently of the JSON (straight from the DBC start bits). Nothing has been
  checked against a real car or a real capture yet.
- The UDS channels are `implemented` only: the ids are opendbc's, never tried
  by OpenTune.
- Where the upstream DBC has only a name and a scale, the signal's note says
  "meaning from the DBC name" or "not documented in the source". Checksums and
  alive counters are decoded as plain numbers, not verified.
- `identify` is empty in all three files: no open source gives gearbox codes or
  part numbers for these units, so they are chosen by id, never guessed.
- Not covered: Hyundai / Kia CAN-FD platforms, Subaru hybrids and pre-Global
  Subarus (they use other DBCs).

To raise the evidence level, decode a capture from your own car with
`opentune controller decode … --json`, compare against the dash / a scan tool,
and open a pull request that cites it as "own capture" (the capture itself
stays out of the repository).
