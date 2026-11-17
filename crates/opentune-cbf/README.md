# opentune-cbf

Parser for Mercedes-Benz **CBF** (CAESAR diagnostic description) files.

A `.cbf` file ships inside Mercedes diagnostic software (DAS / Xentry). It
describes one or more ECU families: their diagnostic services, software variants
and — the focus of this crate — their **Diagnostic Trouble Code (DTC)**
definitions with human-readable descriptions.

## What it extracts

| Field | Meaning |
|-------|---------|
| `Ecu::dtcs` | The ECU-global DTC pool: every fault code + description + reference text. **The authoritative code→description source.** |
| `Ecu::services` | Diagnostic services (metadata: qualifier, name, description, type). |
| `Ecu::variants` | Concrete ECU software variants; each lists the DTCs and services it exposes plus identifying patterns (vendor id, protocol). |

The full CAESAR service record also carries request/response parameter layouts
(presentations / freeze-frame environments) used for *live* diagnostic sessions.
This crate deliberately omits those; they are not needed to build a DTC
database, and skipping them keeps the parser small and dependency-light.

## Usage

```rust
let cbf = opentune_cbf::parse_file("ECU.cbf")?;

for ecu in &cbf.ecus {
    println!("{}: {} DTCs", ecu.qualifier, ecu.dtcs.len());
}

// Flat code→description records, ready to feed a DTC database:
for entry in opentune_cbf::dtc_entries(&cbf) {
    println!("{}  {}", entry.code, entry.description);
}

// Or export the whole parsed model as JSON:
let json = opentune_cbf::to_json(&cbf)?;

// Or a normalised per-ECU diagnostics view (identity + protocols, DTCs,
// services classified by kind, variants, and explicit warnings for what the
// parser does not reconstruct):
for diag in opentune_cbf::diagnostics(&cbf) {
    println!("{} services, {} warnings", diag.services.len(), diag.warnings.len());
}
```

### CLI

```
opentune-cbf <FILE.cbf>           # summarise ECUs / DTC counts
opentune-cbf <FILE.cbf> --json    # full parsed model as JSON
opentune-cbf <FILE.cbf> --dtcs    # one `code<TAB>description` per line
```

The same is available from the main CLI as `opentune cbf <FILE.cbf>
[--json | --dtcs]`.

## DTC codes are Mercedes-specific

CBF DTC codes are **not** SAE J2012 `P/C/B/U` codes. Depending on the ECU they
look like Mercedes hex codes (`01F0EC` → "ECU Internal Error") or re-used
`P`-style strings with German/English text (`P0105` → "Ladedruckfühler"). When
feeding these into `opentune-dtc`, store them under a manufacturer ("MB")
namespace rather than mixing them into the generic SAE table.

## Building

Part of the root workspace:

```
cargo test -p opentune-cbf
```

## Attribution / license

Licensed **GPL-3.0-or-later**. The binary-format handling is adapted from the
**CBFParser** component of
[OpenVehicleDiag](https://github.com/rnd-ash/OpenVehicleDiag) (© Ashcon
Mohseninia and contributors, GPL-3.0). See [`NOTICE`](NOTICE) for details.

OpenVehicleDiag's SMR-D parser (removed upstream under a DMCA takedown) is **not**
reimplemented here. This crate handles the CBF format only.
