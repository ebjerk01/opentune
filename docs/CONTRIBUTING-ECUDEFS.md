# Contributing an ECU definition + map library entry

This is the practical, end-to-end guide for adding support for an ECU: the JSON
you write, the identify keys that make OpenTune recognise it, and the honesty bar
every entry must clear. It builds on the format references and the project-wide
rules:

- [`docs/ECU-DEFS.md`](ECU-DEFS.md) — the full `EcuDef` field reference.
- [`docs/MAP-DEFS.md`](MAP-DEFS.md) — the `MapDef` / axis format.
- [`docs/MAP-LIBRARY.md`](MAP-LIBRARY.md) — the shipped map libraries (EDC16U34, EDC15C6, PCR2.1) and their confidence.
- [`docs/IDENTIFICATION.md`](IDENTIFICATION.md) — identity → definition matching.
- [`CONTRIBUTING.md`](../CONTRIBUTING.md) — scope boundaries, DCO sign-off, CI.

If you only remember one thing: **contribute what you can prove, label how sure
you are, and never ship proprietary data or a value you have not seen decode
sanely.**

---

## 1. What you are contributing

One `ecudefs/<maker>-<family>[-<variant>].json` file. It bundles three things:

1. **Talk-to-it facts** — protocol, diagnostic layer, bitrate, CAN ids, the
   checksum/seed-key handler ids. Public, family-level facts only.
2. **Identify keys** — the part/software numbers that let OpenTune match a dump
   or a live ECU to this definition (section 3).
3. **A map library** — the `maps[]` array: the calibration tables, their
   addresses, scalings and axes (section 4). This is the "map library entry".

A definition with no maps is valid and useful (identify + checksum still work).
Maps can be added later in a follow-up PR.

A tiny, fully synthetic worked example lives in
[`samples/`](../samples/) (`opentune-demo-edc16.json` + its `.bin`) — open it to
see a complete, decoding definition end to end.

## 2. The JSON schema (quick shape)

See [`ECU-DEFS.md`](ECU-DEFS.md) for every field and enum; the skeleton is:

```json
{
  "id": "bosch-edc16u34",
  "name": "Bosch EDC16U34",
  "maker": "Bosch",
  "family": "EDC16",
  "provenance": {
    "sources": ["public family facts (ISO 15765-4)", "own stock read (not committed): maps diff-derived"],
    "license": "GPL-3.0-or-later",
    "evidence": "real-image-tested"
  },
  "notes": "Public family facts. Addresses are placeholders until confirmed per software variant.",
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
  "full_read": "obd",
  "maps": [ /* section 4 */ ],
  "identify": { /* section 3 */ }
}
```

Rules that trip people up:

- **Numbers are decimal.** JSON carries no hex; write `524288`, not `0x80000`.
- **`checksum`** must be an id `opentune-checksum` knows: `crc32-tail`, `edc15c6`,
  `edc16`, `edc17`, `pcr21`, `me7`, `dq250`, `dq250-02e`, `dq381`, `tcu-ccitt`, `zf8hp`, `haldex`,
  `simos-crc` (run
  `opentune checksum algos` for the current list). Unknown-checksum ECUs use `null`.
- **Addresses are byte offsets into the image the apps load** (the full read),
  not CPU addresses. If your family is normally read as a 2 MiB image, your map
  and region offsets are offsets into that 2 MiB.
- Every named region must fit inside the declared `flash_size`; the parse test
  (section 6) enforces it.
- **`provenance`** is required for every new or changed definition (CI runs
  `ecudefs validate --require-provenance`):
  - `sources` — one citation per source: a standard by name; an open project
    with its path/commit **and licence**; or "own read" / "own capture" for
    your own data (the data itself is never committed). Output of commercial
    or OEM tools (their map packs, label/coding files, DAMOS/A2L) is not a
    source and is rejected.
  - `license` — the SPDX id the definition data is shipped under, normally
    `GPL-3.0-or-later` (or the stricter upstream licence of imported
    definitions, e.g. `GPL-3.0-only`). Proprietary, non-commercial or
    "unknown" is an error; an id outside the known GPL-compatible list is a
    warning for the reviewer.
  - `evidence` — the validation state the definition as a whole has reached,
    using the project vocabulary ([`DEVELOPMENT.md`](DEVELOPMENT.md) §7):
    `example`, `implemented`, `synthetic-tested`, `real-image-tested`,
    `bench-tested`, `vehicle-tested`. `example` marks placeholder data (a worked
    example, not a real unit): it never matches an identity, and its `name` must
    end in "Example — placeholder data". Upgrade it once an own read backs it. When parts differ, pick the lower state and explain in
    `notes`; per-map confidence stays in each map's `CONFIDENCE:` label.

  Older definitions without the block still load everywhere and only get a
  `provenance.missing` warning; every shipped file carries one. To migrate a
  local file, add the block after `maker` and re-run the validator with
  `--require-provenance`.

## 3. Identify keys — how matching works

Matching is implemented in `crates/opentune-core/src/ecudef.rs` and fed by the
offline string scanner in `crates/opentune-scan/src/extract.rs`. You provide:

```json
"identify": {
  "part_numbers": ["03G906"],
  "sw_numbers": ["1037516722"]
}
```

- **`part_numbers`** — hardware / spare-part numbers *or prefixes*
  (e.g. `03G906`, `0281011`). Matched against the ECU's reported `hw_number`.
- **`sw_numbers`** — software / calibration numbers *or prefixes*
  (e.g. `1037516722`). Matched against `sw_number`.

Matching is **case- and whitespace-insensitive**, and a key hits if the reported
value **starts with** or **contains** it. So a short family prefix like `03G906`
covers every `03G906021xx` variant — prefer the shortest key that is still
specific to the family, so one definition covers a family without colliding with
others.

A key must be at least **6 characters** (whitespace not counted). A shorter key
is a generic prefix, not an identity — every Bosch software number starts with
`1037` — so the matcher ignores it and `ecudefs validate` rejects it
(`identify.key_short`). When several definitions match, the one whose **longest
hitting key** is longest wins (a full software number beats a hardware family
prefix); on a tie the first definition by id wins.

Where the numbers come from: the scanner reads part/software numbers straight out
of a `.bin` (the EPROM identity block, the ASAM/EPK descriptor), and a live ECU
reports them over UDS/KWP identification DIDs. You can see what a dump exposes
with `opentune identify` (live) or by inspecting the image.

For a broad "what car is this?" net without a full definition, add a
`CatalogEntry` instead — see [`IDENTIFICATION.md`](IDENTIFICATION.md).

## 4. The map library entry

Each `maps[]` element is a `MapDef` ([`MAP-DEFS.md`](MAP-DEFS.md)). Minimum: a
`name` and `address`; everything else defaults (`u8`, big-endian, 1×1, factor 1).
A real 2-D table looks like:

```json
{
  "name": "injection quantity limiter (smoke / air-mass limiter)",
  "notes": "CONFIDENCE: HIGH. Source: diff-derived from owner's stock vs tune + in-image header. Confirm per software variant before any write.",
  "address": 1913020,
  "data_type": "u16",
  "endian": "big",
  "rows": 16,
  "cols": 13,
  "scaling": { "factor": 0.01, "offset": 0.0 },
  "unit": "mg/stroke",
  "x_axis": { "name": "air mass",     "unit": "mg/stroke", "count": 13, "address": 1912994, "data_type": "u16", "endian": "big", "scaling": { "factor": 0.1 } },
  "y_axis": { "name": "engine speed", "unit": "rpm",       "count": 16, "address": 1912962, "data_type": "u16", "endian": "big", "scaling": { "factor": 1.0 } },
  "min": 0.0,
  "max": 80.0
}
```

Guidelines:

- `rows` = Y count, `cols` = X count; `rows*cols` cells of `data_type` at
  `address`. An axis `count` must equal the map's size on that dimension.
- `scaling` is linear only: `physical = raw*factor + offset` (the DBC/cantools
  model). A non-linear OEM `COMPU_METHOD` is out of scope — store it pre-scaled
  and say so in `notes`, don't invent an expression.
- Axes can be **stored** (`address`+`data_type`), **inline** (`values`), or
  **shared** (`axis_ref` to another axis' `name`). See `MAP-DEFS.md`.
- Set `min`/`max` to the physical safety bounds for edited cells (UI hint/clamp).
- You can import maps from a TunerPro **XDF** (`opentune import-xdf`) or an ASAM
  **A2L** (`opentune import-a2l --base <addr>`) and merge them into a definition,
  then hand-check them against the bar below.

## 5. The honesty / validation bar

OpenTune's credibility is that its numbers are real. An entry must clear all of:

1. **Public + personal data only.** No manufacturer firmware, DAMOS/A2L dumps,
   seed/key secrets, or other proprietary content. No real VINs, plates, or
   personal data. Build definitions from public family facts and your **own** ECU
   reads. (Enforced in review — see `CONTRIBUTING.md` — and partly by machine:
   the validator rejects commercial-tool sources and non-open licences in
   `provenance`, and runs the privacy scan for VIN-like ids, e-mail addresses
   and serial numbers over the file.)
2. **Label every map's confidence** in its `notes`, with its source. Use the tags
   already in the shipped library:
   - `CONFIDENCE: HIGH` — diff-confirmed against a real stock/tune pair **and**
     cross-checked against a known LSB / in-image header.
   - `CONFIDENCE: MEDIUM` — structure-derived (in-image map header, sane axes and
     range) but not diff-confirmed.
   - `CONFIDENCE: LOW` / `EXAMPLE ONLY` — placeholder address for the family;
     must be confirmed per variant before any read or write. Say exactly that.
3. **Placeholders are allowed, but must announce themselves.** If an address is a
   family guess, the map/definition `notes` must say the address needs
   per-variant confirmation. Never present a guess as a fact.
4. **It must decode sanely.** Before you submit, decode the map on a real image
   and sanity-check it:
   - stored axes are **monotonic** and in a plausible physical range;
   - cells land in a believable range for the unit (e.g. a diesel smoke limiter
     in tens of mg/stroke, not thousands);
   - `endian` and `data_type` are right (wrong endianness shows as garbage).
5. **Checksums must round-trip.** If you set a `checksum` id, verify it against
   your own untouched stock dump:
   `opentune checksum verify --algo <id> stock.bin` should report every block
   **OK**. A corrupt-then-`fix`-then-verify round trip must come back clean.

## 6. Validate, then submit

Run the same checks CI runs:

```sh
# Validate your file against the format, honesty and scope rules (the machine
# half of this guide). Exits non-zero on any error; add --warnings to see
# advisories, and --image <stock.bin> to decode the maps against a real dump.
# A directory validates every ecudefs/*.json; --require-provenance makes a
# missing provenance block an error (this is the CI gate).
opentune ecudefs validate ecudefs/<your-id>.json --require-provenance
opentune ecudefs validate ecudefs/<your-id>.json --image stock.bin --warnings

# The same privacy scan CI runs over every data directory:
opentune privacy-scan ecudefs/<your-id>.json

# The ecudef parse/consistency test loads every ecudefs/*.json and fails the
# build if one does not parse or a region runs past the flash image.
cargo test -p opentune-core --test ecudef_parse

# Decode your maps against a real image (compare a stock to a tune, or an
# image to itself just to confirm the maps load):
opentune compare --ecudefs ecudefs --def <your-id> stock.bin tuned.bin

# Checksum round-trip on your own stock dump:
opentune checksum verify --algo <id> stock.bin

# Project-wide gates:
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Then open a PR:

- Keep ECU data in `ecudefs/`; a broad recognition record goes in the catalog.
- Sign off every commit (`git commit -s`) — the project uses the DCO.
- Respect the scope boundaries in [`CONTRIBUTING.md`](../CONTRIBUTING.md):
  calibration only, no immobiliser/odometer/emissions-defeat features.

Thank you — a well-sourced, honestly-labelled definition is one of the most
valuable things you can add.
