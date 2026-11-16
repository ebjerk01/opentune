# Manual catalog entries

Hand-curated `CatalogEntry` records for vehicles the auto-harvested
`catalog/opendbc/` set does **not** cover. They use the **exact same JSON
schema** as the opendbc files (see `CatalogEntry` in
`crates/opentune-core/src/catalog.rs`), so the same loader reads them — the only
change needed is to point the loader at this directory too (see *Wiring*).

Current contents:

| File | Make | Entries | Focus |
|------|------|---------|-------|
| `manual-mercedes-diesel.json` | Mercedes-Benz | 7 | OM611/612/613/646/648/642/651 diesel engine ECUs |
| `manual-bmw-diesel.json` | BMW | 6 | M47 / M57 / N47 / N57 diesel engine ECUs |
| `manual-vag-diesel.json` | VW / Audi / SEAT / Skoda | 6 | EA188/EA189/EA288 TDI → EDC15V/EDC15P/EDC16U34/EDC17/PCR2.1 |
| `manual-vag-petrol.json` | VW / Audi / SEAT / Skoda | 4 | 1.8T/2.0 FSI-TFSI/TSI → ME7.5/MED9/Simos-MED17 |
| `manual-ford-diesel.json` | Ford | 2 | 1.6 / 2.0-2.4 TDCi DuraTorq → EDC16/SID807/EDC17/DCM3.5 |
| `manual-psa-diesel.json` | Peugeot / Citroën / DS | 3 | 1.4-1.6 HDi, 2.0-2.2 HDi, BlueHDi → EDC15C2/16C34/SID807/EDC17 |
| `manual-opel-diesel.json` | Opel / Vauxhall | 2 | 1.3/1.7 CDTi, 1.9/2.0 CDTi → MJD/EDC16C39/EDC17 |
| `manual-fiat-diesel.json` | Fiat / Alfa / Lancia | 2 | 1.3 MultiJet, 1.9-2.4 JTD → MJD/EDC15C7/EDC16C39/EDC17 |
| `manual-volvo-diesel.json` | Volvo | 2 | D5 five-cyl, 1.6D/2.0D (PSA/Ford) → EDC16/EDC17/SID807/DCM3.5 |
| `manual-renault-diesel.json` | Renault / Dacia / Nissan | 2 | 1.5 dCi (K9K), 1.9/2.0/2.3 dCi → SID30x/DDCR/EDC15C3/EDC16/EDC17 |

opendbc is US/Asian-gasoline-biased: it ships **no Mercedes and no BMW**, and for VW/Audi only
newer MQB-era parts (04E906/04L906/05x906 and MQB chassis prefixes) — **none** of the big-volume
classic Euro diesels (EA188 PD `03G906`/`038906`/`028906`, EA189 CR `03L906`) or the older petrol
ECUs (`06A906`/`06F906`/`06J906`). These files fill that gap and feed the bilinfo regnr resolver.

## Provenance

Every entry carries a `source` string. These entries are built from **public,
widely-documented engine → ECU-family mappings** (which Bosch EDC generation a
given OM/M/N engine shipped with), as published across service literature and
community ECU/tuning databases. They are *not* scraped from a single dataset and
they are *not* vehicle-scan captures like opendbc's `fw_versions`.

## The honesty bar

This catalog is deliberately **small and conservative**. The rule was: *fewer
correct entries beat many guesses.* Concretely:

- **`family` is a candidate, not a promise.** Where an engine spanned more than
  one ECU generation over its life (e.g. OM642 = EDC16CP31 → EDC17CP; M57 =
  EDC15/EDC16/EDC17), the `family` field lists the **set** and the `notes` say
  the year/market split is **not** resolved. Do not treat these as a single
  pinned ECU.
- **Part numbers are scoped, or absent.**
  - *Mercedes* entries key `part_numbers` on the **engine-family prefix** (e.g.
    `A6111` = engine OM611). That scopes matching to the right engine but is
    **not ECU-unique** — it will also match non-ECU parts of the same engine. The
    OM651 entry ships **no** part key: its exact `A651x` ECU prefix (`A6511` vs
    `A6519`) could not be confirmed, so a speculative key was refused.
  - *VAG* entries key `part_numbers` on the VAG **engine-electronics prefix**:
    the 3-char platform code + `906` group (e.g. `03G906` = EA188 PD engine ECU,
    `03L906` = EA189 CR engine ECU, `06A906` = 1.8T ME7). This scopes matching to
    the right engine platform but is **not SW-unique** and matches all letter-
    suffixed variants. A bare platform code (`03L`) alone is **not** used — the
    `906` group is always included so the key lands on the engine ECU, not other
    `03L` parts. Note `03L906` is shared by the EA189 1.6-PCR2.1 and 2.0-EDC17
    entries (same platform) and does not by itself separate displacement.
  - *BMW, Ford, PSA, Opel, Fiat, Volvo, Renault* entries ship **no**
    `part_numbers`. Their ECU/part numbers encode supplier or chassis
    supersession, **not** the engine code, so no reliable engine-scoped prefix
    exists. These entries document the engine→family mapping but **do not
    auto-match** via `matches_id`; they match by context (make/model/year/family).
- **Bosch family prefixes are intentionally not used as match keys.** A bare
  `0281` matches *every* diesel Bosch ECU, so including it would cause mass false
  positives. The Bosch structural family is described in `crates/opentune-vin`
  instead.
- **Diagnostic addresses.** `0x7e0` (CAN UDS/OBD engine address) is included
  only for the EDC16-CAN / EDC17 era entries. EDC15 entries are K-line
  (ISO 9141 / KWP2000) and ship **no** address rather than a fabricated one.
- **Confidence is recorded per entry.** Each record has a `confidence` field
  (`high` / `medium`) plus a `notes` field explaining exactly what is and isn't
  pinned down.

### Non-schema fields (`confidence`, `notes`)

`confidence` and `notes` are **not** part of the `CatalogEntry` struct today.
The loader uses serde without `deny_unknown_fields`, so these extra keys are
**silently ignored** — verified by loading all 36 entries through
`opentune_core::catalog::load_catalog`. They are kept in the JSON so a human (or
a future schema revision) can see the honesty flags inline. If/when the struct
grows these fields, nothing here needs to change.

## Wiring

`load_catalog` now **recurses** into subdirectories (see `catalog.rs`), so
pointing it at `catalog/` picks up both the `opendbc-*.json` files and every
`manual-*.json` in this folder automatically — no extra wiring needed.

## Adding more

1. Match the opendbc schema exactly (`make` is the only required field).
2. Cite a `source`. No source → it does not go in.
3. If you are not sure of the ECU family, say so in `notes` and set
   `confidence: "medium"` (or lower) — or leave the entry out entirely.
4. Prefer engine-scoped or empty `part_numbers` over an invented exact prefix.
