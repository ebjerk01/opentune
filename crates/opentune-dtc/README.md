# opentune-dtc

Diagnostic Trouble Code (DTC) database and decoder for [OpenTune](https://github.com/ebjerk01/opentune).

It does two things:

- **Describe a code.** `lookup("P0101")` resolves a code to its human-readable
  description from a bundled table.
- **Decode a UDS response.** `decode_uds(&bytes)` parses the record list of an
  ISO 14229-1 `ReadDTCInformation` response (3-byte DTC + 1 status byte per
  record) into decoded codes and their status bytes.

The crate has **no dependencies beyond `std`**. The DTC table is embedded at
compile time (`include_str!`) and parsed once on first use (`OnceLock` +
binary search), so lookups are allocation-light and the data ships inside the
binary.

## API

```rust
use opentune_dtc::{lookup, decode_uds, format_code, Dtc, DtcRecord, System};

// Describe a code (case-insensitive).
let dtc: Dtc = lookup("P0101").unwrap();
assert_eq!(dtc.description, "Mass or Volume Air Flow Circuit Range/Performance");
assert_eq!(dtc.system, System::Powertrain);

// A well-formed code that is not in the table still decodes structurally.
let unknown = lookup("C1234").unwrap();
assert_eq!(unknown.system, System::Chassis);
assert!(unknown.description.contains("unknown"));

// Malformed input returns None.
assert!(lookup("X9999").is_none());

// Build a code string from raw UDS DTC bytes (ISO 15031-6).
assert_eq!(format_code(0x01, 0x01, 0x00), "P0101");

// Decode a ReadDTCInformation record list: [high, mid, low, status] * N.
let records: Vec<DtcRecord> = decode_uds(&[0x01, 0x01, 0x00, 0x08]);
assert_eq!(records[0].dtc.code, "P0101");
assert_eq!(records[0].status, 0x08);
```

### Types

- `System` — `Powertrain` (`P`) / `Chassis` (`C`) / `Body` (`B`) /
  `Network` (`U`), from a code's first letter.
- `Dtc { code: String, description: String, system: System }`.
- `DtcRecord { dtc: Dtc, status: u8 }`.

### Functions

- `lookup(code: &str) -> Option<Dtc>` — `None` only for malformed codes; a
  well-formed but unlisted code falls back to a generic
  "manufacturer-specific / unknown" description with the decoded `System`.
- `decode_uds(raw: &[u8]) -> Vec<DtcRecord>` — 4-byte records; trailing partial
  records are ignored. Strip the service id (`0x59`), sub-function, and
  `DTCStatusAvailabilityMask` byte before calling.
- `format_code(high, mid, low) -> String` — canonical 5-character code from the
  3 DTC bytes. The `low` byte is the ISO 14229-1 failure-type byte and is not
  part of the 5-character SAE code.
- `table_len() -> usize` — number of bundled entries.

## Code byte format (ISO 15031-6 / SAE J2012)

First DTC byte:

| bits | meaning |
|------|---------|
| 7-6  | letter: `00`=P, `01`=C, `10`=B, `11`=U |
| 5-4  | first digit (0-3) |
| 3-0  | second digit (hex 0-F) |

Second DTC byte: third digit (bits 7-4) and fourth digit (bits 3-0), both hex.

So `0x01 0x01` → `P0101`.

## Data source & provenance

`data/dtc.tsv` is a tab-separated `CODE<TAB>DESCRIPTION` table of **11,140**
public powertrain (`P`, 9,830 entries) and network (`U`, 1,310 entries)
diagnostic trouble codes, sorted by code and de-duplicated.

These are the standardised **SAE J2012 / ISO 15031-6** trouble-code
definitions — short factual fault descriptions (e.g. "Mass or Volume Air Flow
Circuit Range/Performance") that are published in the OBD-II/EOBD standards and
widely reproduced by scan-tool and ECU documentation. They are facts about what
a code means, not creative content. No proprietary manufacturer lookup tables,
flash data, or copyrighted repair text are included.

Chassis (`C`) and body (`B`) codes are not in the bundled table; a well-formed
`C`/`B` code still decodes to its `System` with a generic description, so the
decoder degrades gracefully.

## Status

Part of the workspace and used by the CLI (`opentune dtc <code>`), the
auto-scan engine and the desktop app. The crate exposes only the library API and
ships its own data, so it can be built and tested on its own:

```sh
cargo test -p opentune-dtc
```

## Licence

GPL-3.0-or-later, same as the rest of OpenTune.
