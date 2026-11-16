# Manual catalog entries

Hand-curated `CatalogEntry` records for vehicles the auto-harvested
`catalog/opendbc/` set does **not** cover. They use the **exact same JSON
schema** as the opendbc files (see `CatalogEntry` in
`crates/opentune-core/src/catalog.rs`), so the same loader reads them — the only
change needed is to point the loader at this directory too (see *Wiring*).

Current contents:

| File | Make | Entries | Focus |
|------|------|---------|-------|
| `manual-mercedes-diesel.json` | Mercedes-Benz | 6 | OM611/612/613/646/648/642 diesel engine ECUs |
| `manual-bmw-diesel.json` | BMW | 6 | M47 / M57 / N47 / N57 diesel engine ECUs |

opendbc ships **no Mercedes and no BMW** at all, so these fill a real gap.

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
    **not ECU-unique** — it will also match non-ECU parts of the same engine.
  - *BMW* entries ship **no** `part_numbers`. BMW part numbers do not encode the
    engine code, and we refuse to ship speculative prefixes. These entries
    therefore document the engine→family mapping but will **not auto-match** on a
    reported part number; they match by context (make/model/year/family), not by
    `matches_id`.
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
**silently ignored** — verified by loading all 12 entries through
`opentune_core::catalog::load_catalog`. They are kept in the JSON so a human (or
a future schema revision) can see the honesty flags inline. If/when the struct
grows these fields, nothing here needs to change.

## Wiring (what the integrator must still do)

`load_catalog` is non-recursive, so it does **not** descend into this
subdirectory on its own. To include these entries, have the catalog loader read
`catalog/manual/` in addition to `catalog/opendbc/` (either call `load_catalog`
on both directories and concatenate, or teach the loader to walk subdirectories).

## Adding more

1. Match the opendbc schema exactly (`make` is the only required field).
2. Cite a `source`. No source → it does not go in.
3. If you are not sure of the ECU family, say so in `notes` and set
   `confidence: "medium"` (or lower) — or leave the entry out entirely.
4. Prefer engine-scoped or empty `part_numbers` over an invented exact prefix.
