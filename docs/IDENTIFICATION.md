# ECU identification in OpenTune

OpenTune identifies an ECU in two stages, from broad to precise:

1. **Read the ECU's identity.** Over the diagnostic bus (UDS / KWP2000), OpenTune
   reads the standard identification DIDs — hardware number, software number,
   supplier part number, etc. — into an `EcuId` (`crates/opentune-core/src/types.rs`),
   whose key fields are `hw_number` and `sw_number`.

2. **Match the identity against known data.**
   - First against the detailed definitions in `ecudefs/` — these describe ECUs
     OpenTune fully supports (memory layout, checksum, flash routine).
   - Then against the **catalog**: a wide, lightweight net that recognises an ECU
     by its reported firmware/part numbers even when no detailed `ecudef`
     exists yet. This is what `opentune identify --catalog <dir>` loads.

The catalog answers "what car / ECU is this?"; the `ecudefs` answer "and here is
how to read and flash it."

## The `CatalogEntry` format

The catalog schema is fixed in `crates/opentune-core/src/catalog.rs`. Each
catalog file is a JSON **array** of `CatalogEntry` objects (a single object is
also accepted). Fields:

| Field | Type | Meaning |
| --- | --- | --- |
| `make` | string (required) | Vehicle make, e.g. `"Volkswagen"`, `"Audi"`, `"Toyota"`. |
| `models` | array of strings | Models/variants this platform covers, e.g. `"Volkswagen Golf GTI"`. |
| `years` | string, optional | Model-year span, e.g. `"2015-2021"`. |
| `ecu` | string, optional | ECU type, e.g. `"engine"`, `"transmission"`, `"abs"`, `"eps"`. |
| `addresses` | array of strings | Diagnostic CAN addresses the ECU answers on, hex, e.g. `"0x7e0"`. |
| `fw_versions` | array of strings | Firmware/version strings the ECU reports (see decoding below). |
| `part_numbers` | array of strings | Hardware/part numbers extracted from the firmware strings. |
| `family` | string, optional | ECU family (e.g. `"EDC17"`, `"PCR2.1"`) when known. |
| `ecudef_id` | string, optional | Link to a detailed `ecudefs/` definition, when one exists. |
| `source` | string, optional | Where the record came from, e.g. `"opendbc"`. |

### How matching works

`CatalogEntry::matches_id` normalises every `fw_versions` and `part_numbers`
string (strips whitespace, upper-cases) and checks whether the ECU's reported
`hw_number` / `sw_number` is a substring of any of them (or vice-versa). So the
ASCII part number embedded in a firmware string — e.g. `04E906023AN` inside
`\xf1\x8704E906023AN\xf1\x893695` — is enough to match a reported
`hw_number` of `04E906023AN`.

## The opendbc catalog (`catalog/opendbc-*.json`)

The files in `catalog/opendbc-*.json` (split by make/port, e.g.
`opendbc-volkswagen.json`, `opendbc-toyota.json`, `opendbc-ford.json`) are
**generated** from comma.ai's
open-source [opendbc](https://github.com/commaai/opendbc) dataset. opendbc ships
`FW_VERSIONS` tables for hundreds of cars: for each vehicle platform it lists,
per ECU `(type, CAN address, subaddress)`, the exact firmware byte strings that
real cars report. That is a ready-made ECU-identity fingerprint database, which
the harvester converts into `CatalogEntry` arrays.

Conversion notes:

- **One entry per (platform, ECU type).** `fw_versions` holds every firmware
  string opendbc records for that ECU; `addresses` holds its CAN address(es).
- **Firmware decoding is consistent:** each firmware byte string is decoded with
  printable ASCII bytes kept verbatim and every other byte shown as `\xNN`
  (so `04E906023AN` stays readable while non-printable bytes remain exact and
  reversible).
- **`part_numbers`** are the alphanumeric runs (length ≥ 6, containing a digit)
  pulled out of those byte strings — e.g. VW `04E906023AN`, Ford `LX6C-2D053-RD`,
  Toyota `F152676144`.
- **`make` / `models` / `years`** come from opendbc's `CarDocs` entries in each
  port's `values.py`. When a platform has no `CarDocs`, `models` falls back to
  the raw opendbc platform token (e.g. `HONDA_CIVIC_BOSCH_DIESEL`) so the
  fingerprint group keeps an identifier.
- **`ecudef_id`** is left `null` — it is reserved for a link to a detailed
  `ecudefs/` definition and is filled in only if/when one is written for the
  matched ECU.
- `family` is left `null`: opendbc records the vehicle platform (MQB, TSS2, …),
  not the ECU hardware family (EDC17, …), so it is populated from `ecudefs/`
  instead when a detailed definition exists.

### Licence and attribution

opendbc is distributed by **comma.ai** under the **MIT License**. OpenTune is
GPL-3.0; the MIT licence is compatible with reuse in a GPL-3.0 project. The
harvested data and this catalog are derived from opendbc; credit for the
underlying firmware-fingerprint dataset belongs to **comma.ai and the opendbc
contributors** (https://github.com/commaai/opendbc, MIT). No OpenTune code is
copied from opendbc — only the public firmware-identification data is reused.

### Regenerating the catalog

The catalog is reproducible from a fresh opendbc checkout:

```sh
# 1. Clone opendbc read-only (treated as untrusted data; never executed)
git clone --depth 1 https://github.com/commaai/opendbc.git /path/to/opendbc

# 2. Harvest. The script PARSES opendbc's Python with Python's `ast` module as
#    text — it never imports or runs opendbc code. Always use `python3 -I`.
#    Arg 1 = the opendbc checkout; arg 2 = the output directory (the catalog/).
python3 -I scripts/harvest_opendbc.py /path/to/opendbc catalog
```

The script (`scripts/harvest_opendbc.py`) reads each
`opendbc/car/<brand>/fingerprints.py` (`FW_VERSIONS`) and `values.py`
(`class CAR` / `CarDocs`), and writes one `catalog/opendbc-<brand>.json` per
make. It is pure-Python with no third-party dependencies. It assumes the
opendbc firmware tables live under `<opendbc>/opendbc/car/<brand>/`, the layout
of current opendbc checkouts.

### Verifying

Every generated file is a valid `Vec<CatalogEntry>`: it is loaded by the real
Rust loader, `opentune_core::catalog::load_catalog`, which is exactly what
`opentune identify --catalog catalog` uses at runtime. Brands whose opendbc
`FW_VERSIONS` table is empty (currently `gm`, `byd`) and makes absent from
opendbc entirely (BMW, Mercedes-Benz) produce no file.
