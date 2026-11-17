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

## Offline image recognition (`opentune-scan`)

Everything above identifies an ECU over the bus. `opentune-scan` does the same
from a **raw flash image** with no hardware: `opentune scan <dir|file>` (JSON,
or `--human`), `opentune identify --driver mock --file <bin>` (prints an
"Offline image recognition" block after the reported identity) and the desktop
**Analyze** panel (`analyze_image`) all use it.

### Families

Recognition runs two kinds of detector and merges their claims:

* **Checksum structure** (`crate::family`, unchanged): EDC15 / EDC16 / EDC17
  / PCR2.1 checksum directories or located checksum fields.
* **Signatures** (`crate::recognize`): markers the suppliers' toolchains leave
  in every image.

| Supplier | Family (`family`) | Main markers |
| --- | --- | --- |
| Bosch | any of the below | project descriptor `NN/1/<FAMILY>/…//<project>/…` (e.g. `41/1/ME7.5/51/4518.07//…`, `35/1/EDC17_CP14/…`, `53/1/VM17/147/ME17.9.11//…`) |
| Bosch | `ME7` | `ZZ` ident block at 0x10000 (non-printable 3rd byte), `ME7.x` literal, `MOTRONIC ME7` label |
| Bosch | `ME9` / `MED9` | `Bosch.Common.RamLoader.Me9` anchor; `MED9x` literal |
| Bosch | `EDC17` / `MEDC17` / `MED17` / `MEVD17` / `ME17` / `MD1` / `MG1` | family literal + `SB_V…` / `Customer.` / `YYYYMMDDNR…` serial |
| Bosch | `EDC16` | `DE CA FE` at active-section start + 0x3D (size table), SW at +0x10, `EDC16Cxx/…` string |
| Bosch | `EDC15` | `TSW Vx.yy DDMMYY CCCC <token>` toolchain line (variant from `C7/ESB…` → `EDC15C7`), C167 `PP22..00` header + 0281 HW, VAG `… EDC  SG` ident on 512 KiB |
| Bosch | `M7`, `MP3`/`MP7`/`MP9`, `M1.x`…`M5.x`, `ME1.5.5` | project descriptor only |
| Siemens/Continental | `SIMOS` | `SIMOS…` literal, `5WP4…` part, `cas21`/`s21_` tags, VAG ident (`06A906033AT SIMOS72 1.6l 2V`) |
| Siemens/Continental | `SID` | `SID80x`/`SID20x`/`SID30x` literal, `5WS4…-T` ident, `111PM3`/`CAPM3` (SID801), `111PO`+`CAPO` (SID803), `CA<pp>…111<pp>…` calibration header (low) |
| Siemens/Continental | `PPD` | `PPD1.x` literal, `CASN` + `111SN`, VAG ident `03G906018xx R4 2.0l PPD1.2` |
| Siemens/Continental | `PCR2.1` | checksum directory; `CASM…`/`111SM…` calibration header |
| Siemens | `MS4x` | `5WK9…--<10 digits>MS4x` ident record; `MS4x` tag next to a `6577…` serial |
| Siemens | `SIMTEC` | `SIMTECnn` literal (+ Siemens name / `5WK9` / `6577` serial) |
| Delphi | `MT`, `DCM`, `Multec` | `MTnn.n` tag, `DCMn.n` literal, `0x11 "DEL"` header; `Delphi Technologies` copyright (supplier-only otherwise) |
| Magneti Marelli | `MJD`, `IAW` | `MJD6JFHW01x`-style tag (+ `MAG  01246…`), lower-case `iawNN…` tag; `MARELLI`/byte-swapped `AMERLL` (supplier-only) |
| Denso | `Denso SH` | `Copr.DENSO`/`Cpyr.DENSO`/`CopyrightDENSO` + SuperH reset-vector table (PC,SP,PC,SP with SP in 0xFFFFxxxx) |

`DCM` is implemented from the family's naming convention but **no real DCM
image was available to validate it**; treat its results accordingly.

### Confidence tiers and evidence

Every detection reports `supplier`, `family`, `variant` (only when a string
in the image names it), `confidence` and `evidence` — the marker, the byte
`offset` and the matched bytes. There are no numeric scores.

* **high** — a family named inside a structured record (descriptor, ident
  record, toolchain line), a checksum structure, or two independent detectors
  agreeing at medium or better;
* **medium** — one family literal or one supplier-specific anchor;
* **low** — a weak anchor alone (e.g. `ZZ` at 0x10000, an SMS-Soft header
  tag), or supplier-only knowledge (family `Unknown`).

A purely structural claim (e.g. the EDC17 checksum directory, shared by the
whole EDC17/MED17/ME17 generation) is folded into a string detection of the
same generation. Two incompatible families at the same top tier set
`contested: true`.

Before any detector runs, the file is classified (`image_kind`): `raw`,
`empty`, `blank`, `text` (DCM/DAMOS/A2L — ≥85 % printable), `winols-project`,
`zip-archive`, `ecm-titanium-driver`, `sms-soft-container`. Only `raw` images
are searched; an SMS-Soft header tag (`BschM797`) is reported at low.

### Identity

`identity` lists identifiers with the offset of their first occurrence:
`bosch_hw` (0261/0281/0285…), `bosch_sw` (103…), `bosch_number`, `vag_part`
(control-unit main groups 906/907/927/997/998), `mercedes_part`, `ford_part`
(`6G91-12B684-DA` style), `continental_part` (`5WK9`/`5WS4…-T`/`5WP4`), `vin`
(validated), and from structured records `bosch_descriptor`, `dataset`
(`4420.01`), `calibration_id` (the project field after `//`),
`oem_ident_block` and `engine` (`2.0l R4` — only what the ident block states).

### JSON compatibility

`ScanRecord` keeps every previous key. `ecu_family` still holds the
checksum-structure family when one is found (it selects the checksum engine),
otherwise the recognised family; new keys are `ecu_supplier`, `ecu_variant`,
`family_confidence`, `recognition` (`image_kind`, `primary`, `others`,
`contested`) and `identity`. The summary adds `family_by_checksum_structure`,
`family_by_signature_only`, `supplier_only`, `contested`,
`recognized_family_breakdown`, `family_confidence_breakdown` and
`image_kind_breakdown` (`family_by_structure` now counts any known family).
`analyze_image` adds `supplier`, `variant`, `family_confidence`,
`recognition` and `identity`.

### Testing

`opentune_scan::synth` builds minimal synthetic images (synthetic numbers
only) carrying each family's markers; `tests/recognize.rs` checks family,
variant, tier, evidence offsets, contested handling and false positives
(noise, blank, text, WinOLS). The real-dump check is `#[ignore]`d and reads
`OPENTUNE_FIXTURES`. Dumps are never committed.

### Attribution

Part of the marker knowledge is re-implemented from
[openremap-core](https://github.com/v-arapidis/openremap-core) (MIT) — see
`crates/opentune-scan/NOTICE` for exactly what was and was not used.
