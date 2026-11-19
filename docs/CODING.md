# Module coding

The `opentune-coding` engine reads a module's configuration bytes over UDS,
decodes them with a **coding-label set** (a `CodingDef`: named bit fields with
options), previews an edit as a byte and parameter diff, and writes it back
safely: it re-reads the module first (that read is the backup), merges only the
bytes you changed, writes the changed blocks and reads them back to verify.
Label sets are data: they ship in definition packs (`coding/*.json`, see
[DEFINITION-PACKS.md](DEFINITION-PACKS.md)) or come from a file you load.

This page covers the brand label sets and formats: what ships, where each fact
comes from, what was tested, and what is missing.

## Field convention

A parameter is `byte`, `bit`, `len` inside one block (DID). Bit 0 is the
least-significant bit of `byte`; wider fields continue into higher bits and
higher bytes. Label tables usually give a byte and a **mask**: a mask such as
`0x30` is `bit = 4, len = 2`, and option values are the field value *after*
shifting (`0x20` masked = option `2`). `opentune_coding::mask_field` does that
conversion; a mask with a hole (`0x05`) is not one field and is refused.

## What ships, by brand

| Brand | Shipped | Source of the labels | Evidence |
|---|---|---|---|
| VAG | Pack [`vag-mib2-coding`](../packs/vag-mib2-coding/): the long coding of the **MIB2 High** infotainment unit (module 5F), 118 parameters over bytes 0-24, DID `0x0600` | [jilleb/mib2-toolbox](https://github.com/jilleb/mib2-toolbox) `Toolbox/GEM/mqb-coding.esd` @ `a8e2f08` (**MIT**, (c) 2019 Chillout); DID `0x0600` "VW Coding Value" from [bri3d/VW_Flash](https://github.com/bri3d/VW_Flash) `lib/constants.py` @ `293f46d` (BSD-2-Clause) | `synthetic-tested` |
| VAG, other modules | nothing | no openly licensed byte/bit table found (see below) | — |
| Ford As-Built | **format only**: line parser, checksum, `.ab` reader, block ↔ DID mapping (`opentune_coding::asbuilt`, `opentune coding asbuilt`) — **no labels** | format: [carp3/AsBuilt-Browser](https://github.com/carp3/AsBuilt-Browser) @ `bbcff56` (**MIT**) and [consp/apim-asbuilt-decode](https://github.com/consp/apim-asbuilt-decode) @ `32ac874` (**GPL-3.0**) | `synthetic-tested` |
| Ford/Volvo/JLR CCF / CCC | **importer only** for a parameter descriptor *you* supply (`opentune_coding::ccf`, `opentune coding import-ccf`) — **no descriptor ships** | structure as read by [5555l/ccf_parser](https://github.com/5555l/ccf_parser) @ `479ac82` (**GPL-3.0**) | `synthetic-tested` |

The [`vcar-demo`](../packs/vcar-demo/) pack also has coding sets, for the
virtual car only ([VIRTUAL-CAR.md](VIRTUAL-CAR.md)).

### VAG: MIB2 High long coding (`vag-mib2-coding`)

The MIB2 Toolbox's "Coding" engineering-menu screen edits the unit's long
coding through persistence keys `0x5018BBMM`, where `BB / 4` is the coding byte
and `MM` the mask. Each screen entry became one parameter, with its label and
option list unchanged (a few labels are German, e.g. "AM sperre"); the two
entries both labelled "SDS" are named "SDS (byte 22)" and "SDS (byte 24)".
Every parameter's `source` names its key. The MIT notice is in
[`packs/vag-mib2-coding/LICENSE-MIT.txt`](../packs/vag-mib2-coding/LICENSE-MIT.txt).

Verified: the table converts without overlap or gaps in the masks; every
option and both states of every flag encode and decode back to themselves and
change nothing else; read / decode / edit / write / read-back round trips run
over `opentune-udssim` and over the `vcar` virtual car, and through the CLI's
`mock` store — all with **invented** coding bytes.

**Not verified (assumptions):**

- that the bytes the toolbox edits through persistence are, in the same order,
  the data of UDS DID `0x0600` (the toolbox says the screen changes "the long
  coding of module 5F"; no open source shows the UDS read);
- that writing needs no SecurityAccess (`requires_security: false`): no open
  source states it. A module that wants a login answers `0x33` and nothing is
  written;
- that every MIB2 software train uses all 25 bytes the same way.

Never run against a real unit. Keep the backup the write report prints.

### Ford As-Built

An As-Built line is `MMM-BB-LL DDDD DDDD DDCC`: module request id, block, line,
up to five data bytes and a checksum byte. The checksum is the low byte of
`MM_hi + MM_lo + BB + LL + data bytes` (both sources compute the same sum;
AsBuilt Browser's test vector `726-02-01 1E1E 0000 00` → `6C` is a unit
test). Block `n` is UDS DID `0xDE00 + n - 1`, its data being the block's lines
concatenated five bytes at a time (apim-asbuilt-decode: "Block 1 (7D0-01 or
DE00)"). A Ford `.ab` file is XML: `<DATA LABEL="MMM-BB-LL">` records with
`<CODE>` words under `AS_BUILT_DATA`; only those records are read (the VIN and
part numbers are ignored).

- `opentune coding asbuilt <file>` checks every line's checksum (`.ab` or text
  lines; `;`/`#` comment lines are skipped) and exits non-zero on a bad one,
  naming the expected byte.
- `--module 726 --store-out store.json` also writes that module's blocks as a
  `mock` coding store, so a coding definition you wrote can decode a saved file
  offline: `opentune coding decode --def my-bcm.json --driver mock --store store.json`.
- In code, `asbuilt::to_coding` / `from_coding` convert between lines and the
  engine's blocks, and `asbuilt::line_offset(line, byte)` places a field given
  in As-Built notation (line, byte within the line, mask).

Open points: the two public tools disagree on how `BB`/`LL` of 10 and above are
written (apim-asbuilt-decode writes hex, AsBuilt Browser reads decimal); OpenTune
reads hex, and a checksum mismatch on such a line may be that, so it is
reported, never "fixed". The DID mapping and `.ab` reading are tested on
synthetic data only; no Ford module has been read.

### CCF / CCC parameter descriptors

The car configuration of EUCD-era Ford, Volvo and JLR modules is one byte
string; a parameter table says which bytes and bits mean what. No such table is
openly licensed, so OpenTune ships none. `opentune coding import-ccf
<descriptor.xml> --did <DID> --out def.json` turns a table you have into a
coding definition. The expected structure (the one ccf_parser reads):

```xml
<block name="CCF">
  <group start="5" stop="5">
    <title><tm>Group title</tm></title>
    <parameter id="005_005" name="P_NAME" mask="0x30" type="ENUM">
      <parameter_title><tm>Readable title</tm></parameter_title>
      <select>
        <option value="0x00" name="OPT_A"><tm>Option A</tm></option>
        <option value="0x10" name="OPT_B"><tm>Option B</tm></option>
      </select>
    </parameter>
  </group>
</block>
```

`id` is the start and end byte; `mask` applies to those bytes read as one
big-endian number; option values are masked, unshifted. `--block` picks the
`<block name>` (default `CCF`). Parameters the engine cannot express are listed
with the reason and left out: `ASCII` text, masks with holes, fields that cross
a byte of the big-endian number, and option values outside the mask. Where the
configuration lives on the bus is platform-specific and in no open source, so
`--did` is yours to give; byte offsets are taken from the start of that DID's
data. The output file is never overwritten.

A file named `ccc_descr.xml` was asked about for this work; no public copy or
description of it was found, so the importer follows ccf_parser's structure. A
descriptor in another structure is rejected with the block names it did find.

## Sources checked and not used

| Source | Why not |
|---|---|
| consp/apim-asbuilt-decode **labels** (`src/statics.py`, GPL-3.0) | Its README says the labels were "collected from code (extracting debug information from the QNX applications)" and from forum sheets: reverse-engineered OEM data plus unlicensed tables. Only its *format* code is used. |
| jakka351/FG-Falcon (CC-BY-SA-4.0) | The repository also redistributes FORScan spreadsheets and OEM firmware; its as-built notes cannot be separated from those. |
| jakka351/ABS-Config-ELM327 | No licence. |
| Korni22/VAG-Coding (GPL-3.0) | Two adaptation-channel notes copied from a forum; no long-coding table. |
| Cyanlabs As-Built database, Ross-Tech wiki, FORScan forum, motor-talk, OBDeleven / Carista hubs, Scribd/Google-Sheets coding lists | No open licence (all rights reserved). Cross-check only; nothing copied. |
| VCDS label files, ODIS data, FORScan files, OBDeleven/Carista content | Excluded by policy, never read. |

## Adding a label set

A label set needs a source whose licence allows redistribution in this GPL-3.0
repository (MIT, BSD, GPL-compatible, CC-BY, CC0, or your own captures of your
own module), recorded per pack (`sources`) and per parameter (`source`). Mark
evidence honestly (`synthetic-tested` until it has coded a real module) and add
a round trip under `crates/opentune-coding/tests/label_sets.rs`. A wrong label
can disable a function in a car; a smaller, sourced set is better than a larger
guessed one.
