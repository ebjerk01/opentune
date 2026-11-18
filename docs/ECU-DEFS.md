# ECU definition format

An **ECU definition** is a small JSON file describing one known ECU: how to talk
to it, where its memory is, and which checksum / seed-key handlers apply. The
files live in [`ecudefs/`](../ecudefs/) and are loaded by
`opentune_core::ecudef::load_dir`, which parses every `*.json` in a directory
into an `EcuDef`.

Definitions must be built from **public** ECU facts only. Memory addresses and
sizes vary by software variant, so the shipped definitions use sensible
family-level placeholders and say so in `notes`; confirm them against your own
readout before any read or write.

## Fields

| Field | JSON type | Required | Meaning |
|---|---|---|---|
| `id` | string | yes | Stable identifier (e.g. `"bosch-edc16u34"`). Used to select the definition. |
| `name` | string | yes | Human-readable name. |
| `maker` | string | yes | ECU manufacturer (e.g. `"Bosch"`, `"Continental"`). |
| `family` | string | yes | ECU family (e.g. `"EDC16"`, `"Simos PCR2.1"`). |
| `notes` | string | no (default `""`) | Free text; use it to flag that addresses need per-variant confirmation. |
| `protocol` | enum | yes | Transport the ECU speaks (see below). |
| `diag_layer` | enum | yes | Diagnostic service layer: `uds` or `kwp2000`. |
| `bitrate` | integer (u32) | yes | CAN bitrate in bit/s, or K-line baud. |
| `tx_id` | integer (u32) | no (default `0`) | Diagnostic request CAN id; for K-line, the ECU address. |
| `rx_id` | integer (u32) | no (default `0`) | Diagnostic response CAN id. |
| `flash_size` | integer (u32) | no (default `0`) | Total flash image size in bytes. |
| `regions` | array | no (default `[]`) | Named memory regions (see below). |
| `checksum` | string or null | no (default `null`) | Checksum algorithm id handled by `opentune-checksum` (e.g. `"edc16"`, `"edc17"`). |
| `seedkey` | string or null | no (default `null`) | Seed/key algorithm id handled by a seed/key plugin. |
| `full_read` | enum | no (default `obd`) | How a complete original is obtained: `obd` or `bench_boot`. |

### `protocol` values

Serialized in `snake_case`:

| Value | Meaning |
|---|---|
| `iso_tp` | ISO 15765-4: UDS/KWP over CAN with ISO-TP. |
| `vwtp20` | VW TP2.0: KWP2000 over CAN (VAG). |
| `iso14230_kline` | ISO 14230-4: KWP2000 over K-line. |
| `iso9141` | ISO 9141-2: K-line. |
| `raw_can` | Raw CAN frames (no transport). |

### `diag_layer` values

| Value | Meaning |
|---|---|
| `uds` | Unified Diagnostic Services (ISO 14229). |
| `kwp2000` | Keyword Protocol 2000 (ISO 14230). |

### `full_read` values

| Value | Meaning |
|---|---|
| `obd` | A complete original can be read over the OBD port. |
| `bench_boot` | A complete original needs a bench/boot tool (OpenTune does not provide one); calibration flashing may still be possible over OBD. |

### `regions[]` — named memory regions

Each entry is:

| Field | JSON type | Required | Meaning |
|---|---|---|---|
| `name` | string | yes | Region name (e.g. `"full"`, `"calibration"`). |
| `region.address` | integer (u32) | yes | Start address. |
| `region.length` | integer (u32) | yes | Length in bytes. |
| `writable` | boolean | no (default `false`) | Whether OpenTune may write this region. |

Addresses and lengths are plain integers in JSON. Hex is not expressed in the
file itself — convert to decimal. For example `0x80000` (512 KiB) is `524288`.

## Worked example

`ecudefs/bosch-edc16u34.json` — a Bosch EDC16U34 (VAG 1.9 TDI PD), diagnosed
over CAN. The `full` region is the whole 512 KiB image (read-only); the
`calibration` region is the writable map area:

```json
{
  "id": "bosch-edc16u34",
  "name": "Bosch EDC16U34",
  "maker": "Bosch",
  "family": "EDC16",
  "notes": "VAG 1.9 TDI PD. Diagnostics over CAN (ISO 15765-4); some vehicles answer KWP2000, others UDS - set diag_layer to match the car. Addresses and flash_size are placeholders for the family and must be confirmed per software variant before any read or write.",
  "protocol": "iso_tp",
  "diag_layer": "kwp2000",
  "bitrate": 500000,
  "tx_id": 2016,
  "rx_id": 2024,
  "flash_size": 524288,
  "regions": [
    { "name": "full",        "region": { "address": 0,      "length": 524288 }, "writable": false },
    { "name": "calibration", "region": { "address": 393216, "length": 131072 }, "writable": true  }
  ],
  "checksum": "edc16",
  "seedkey": null,
  "full_read": "obd"
}
```

Notes on the values above:

- `tx_id`/`rx_id` `2016`/`2024` are the common ISO 15765-4 diagnostic pair
  `0x7E0`/`0x7E8` in decimal.
- `bitrate` `500000` is 500 kbit/s CAN.
- `checksum: "edc16"` selects the EDC16 algorithm in `opentune-checksum`; a
  definition with no known checksum uses `null`.

## Drivetrain controllers

TCU and AWD units also get a **controller definition** in
[`ctrldefs/`](../ctrldefs/) that links one or more ecudefs (one per software
generation) and adds what an ecudef does not carry: diagnostic channels, CAN
broadcast frames, live channels, adaptations, shift/pressure tables with axes
and gear ratios. See [`CONTROLLER-DEFS.md`](CONTROLLER-DEFS.md).

## Adding a definition

1. Copy an existing file in `ecudefs/` and edit the fields.
2. Use **public** facts only — no real VINs or vehicle-specific data.
3. Keep addresses as placeholders and note it in `notes`.
4. The integration test `crates/opentune-core/tests/ecudef_parse.rs` loads every
   `ecudefs/*.json` and fails the build if one does not parse, so run
   `cargo test -p opentune-core --test ecudef_parse` after editing.
