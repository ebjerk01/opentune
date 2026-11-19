# Drivetrain controller definitions

A **controller definition** describes one drivetrain control unit — an
automatic / dual-clutch **TCU**, an **AWD/4WD** coupling controller, a transfer
case or an active differential — as data, so a new brand drops in as a JSON
file instead of a new crate. The files live in [`ctrldefs/`](../ctrldefs/),
the schema is [`ctrldef.schema.json`](ctrldef.schema.json), and the crate is
`opentune-ctrldef` (types, loader, validation, CAN decoder). CLI:

```sh
opentune controller list                      # every definition
opentune controller show 722.6 [--signals]    # by id, part number or gearbox code
opentune controller validate [ctrldefs]       # structure, honesty and link rules
opentune controller match 0AY907554C          # which definitions match a number
opentune controller decode 722.6 capture.log [--bus egs51] [--frames] [--json]
```

`decode` reads the same log formats as `opentune haldex decode` (can-utils
candump, OpenTune CSV, SavvyCAN CSV, Vector ASC) and prints per-frame signal
ranges and the definition's live channels. It is offline and never transmits.

Definitions also ship in [definition packs](DEFINITION-PACKS.md): a pack's
`controllers/*.json` are the same files, loaded with the same validator (see
[In definition packs](#in-definition-packs)). In the desktop app the
**Controllers** panel lists the built-in set and every pack's controllers,
shows one definition with its validation state, and decodes a CAN log with it
(guide page *Controllers*).

Validation words are the vocabulary of
[`DEVELOPMENT.md` §7](DEVELOPMENT.md#7-validation-state-vocabulary).

## How it relates to the other definition files

A controller definition does **not** duplicate what already has a home:

| Fact | Lives in | The controller definition |
|---|---|---|
| Flash layout, maps, checksum of one software generation | `ecudefs/*.json` ([ECU-DEFS.md](ECU-DEFS.md)) | links it in `ecudefs` (one entry per software generation) |
| Pinout, bench wiring, bench safety | `database/units/*.json` ([DATABASE.md](DATABASE.md)) | links it in `units` |
| Checksum algorithm | `opentune-checksum` | names the id in `checksum` only when no linked ecudef does |
| Diagnostic channels, CAN broadcast frames, live channels, adaptations, shift/pressure/TCC tables with axes, gear ratios | — | **here** |

The repository test checks that every link resolves and that a controller
definition agrees with its linked ecudefs on part numbers and diagnostic
addressing. Shipped set:

| Id | Unit | Links | Validation |
|---|---|---|---|
| `vag-dq250-02e` | DQ250 6-speed DSG, 02E (PQ) | ecudefs `temic-dq250-02e`, `temic-dq250-02e-tricore` | real-image-tested (via the ecudef checksum) |
| `vag-dq250-mqb` | DQ250, 0D9 (MQB) | ecudef `temic-dq250-mqb` | synthetic-tested |
| `vag-dq381` | DQ381 7-speed wet DSG | ecudef `bosch-dq381` | real-image-tested (checksum) |
| `borgwarner-haldex-gen4` | Haldex Gen4 (PQ) | ecudef `borgwarner-haldex-gen4` | synthetic-tested; `Allrad_1` real-image-tested on public captures |
| `borgwarner-haldex-gen5` | Haldex Gen5 (MQB) | checksum `haldex` | synthetic-tested |
| `openhaldex-c6` | OpenHaldex controller | — | synthetic-tested |
| `zf-8hp-gen1` | ZF 8HP first generation | checksum `zf8hp`, unit `zf-8hp-gen1` | synthetic-tested |
| `mercedes-egs5x-7226` | Mercedes 722.6 (NAG1), EGS / EGS51 (EGS52 frames deliberately not included) | unit `mercedes-egs52-7226` (pinout) | synthetic-tested — see [MERCEDES-722.6.md](MERCEDES-722.6.md) |

The Haldex definitions are the hand-written `opentune-haldex` decoder written
down as data: `crates/opentune-haldex/tests/ctrldef_parity.rs` decodes 300+
frames both ways and requires identical values, so the two cannot drift.

## Fields

Top level. Required: `id`, `name`, `kind`, `maker`, `family`, `validation`,
`safety`, `sources`. Everything else is optional; unknown fields are
**rejected** (typos fail the load). CAN ids, DIDs and addresses may be JSON
numbers or `"0x…"` strings.

| Field | Meaning |
|---|---|
| `id` | `<maker>-<family>[-<variant>]`, lowercase `a-z0-9-`, equal to the file stem. |
| `name`, `maker`, `family` | Human name, controller supplier, family. |
| `kind` | `tcu`, `awd`, `transfer_case`, `differential`, `other`. |
| `vehicles` | Free-text applications. |
| `validation` | Evidence level of the definition as a whole. |
| `notes` | Scope, what is unknown, how to confirm. |
| `identify` | `part_numbers`, `sw_numbers`, `gearbox_codes`. A key without `*`/`?` matches as a case/space-insensitive prefix or substring; with wildcards it must match the whole value (`*` any run, `?` one character). |
| `ecudefs` | `ecudefs/` ids (flash layout, maps, checksum per software generation). |
| `units` | `database/units/` ids (pinouts, bench notes). |
| `diagnostics` | Diagnostic channels, see below. |
| `buses` | Named CAN frame sets, see below. |
| `live` | Live channels, see below. |
| `adaptations` | Adaptations, basic settings, routines, resets. |
| `tables` | Shift / pressure / TCC / adaptation tables with axes. |
| `gearsets` | Gear ratios per gearbox variant. |
| `checksum` | `opentune-checksum` id, or `null`. |
| `flash` | Inline flash layout (`size`, `regions`, `full_read`, `notes`) for a unit without an ecudef. `size` 0 = unknown. |
| `safety` | At least one safety note (shown by `show`). |
| `sources` | At least one `{title, url, license, used_for}`. |

### `diagnostics[]`

`name`, `transport` (`iso_tp`, `vwtp20`, `iso14230_kline`, `iso9141`,
`raw_can` — the ecudef vocabulary), `layer` (`uds`, `kwp2000`), `bitrate`,
`tx_id` / `rx_id` (ISO-TP pair; for TP2.0 `rx_id` is the id the tester asks
the unit to answer on), `extended_ids`, `tp20_address` (required for
`vwtp20`), `kline_address`, `session`, `identification` (request as hex, e.g.
`1A9B`), `validation`, `notes`. Declare only channels a cited source
documents; an unknown protocol is a note, not a guessed channel.

### `buses[]` and CAN signals

A bus set is `{name, description, bitrate, notes, frames}`; the set name is
what `--bus` selects. Two sets may define the same id with different layouts
(e.g. two platform generations); the decoder then refuses to guess and asks for
`--bus`. A frame is `{id, extended, name, sender, dlc, min_len, cycle_ms,
validation, notes, signals}` — a payload shorter than `min_len` (default 1)
is not decoded; a signal that does not fit a shorter payload is left out.

Signals use **DBC semantics** so they can be copied from a DBC:

| Field | Meaning |
|---|---|
| `start`, `len` | Start bit and length (1..64). |
| `byte_order` | `little` (Intel, DBC `@1`): `start` is the LSB, bit *n* = byte *n*/8 bit *n*%8. `big` (Motorola, DBC `@0`): `start` is the MSB in DBC numbering; the signal runs towards bit 0 of that byte, then bit 7 of the next. |
| `signed` | Two's complement. |
| `kind` | `number` (default), `bool` (1 bit), `enum` (needs `values`), `ascii` (8 bits, one character). |
| `factor`, `offset` | physical = raw × factor + offset. |
| `curve` | Piecewise-linear `[raw, physical]` points (raw ascending), clamped at the ends; replaces factor/offset (e.g. the Haldex lever scale). |
| `unit` | Physical unit. |
| `invalid` | Raw values meaning "not available". |
| `raw_range` | Valid raw `[min, max]`; outside it the value is invalid. |
| `values` | `[{raw, name}]` names for enums. |
| `notes` | Meaning, source, confidence. |

Two signals may share bits when a source gives two readings of the same
byte (e.g. `Allrad_1` byte 4: DBC stiffness or Gen4 temperature).

A source that counts bits from the MSB of the frame read as a big-endian
64-bit word (Ultimate-NAG52's `OFFSET`) converts to a Motorola `start` as
`(OFFSET / 8) * 8 + (7 - OFFSET % 8)`.

### `live[]`

`{name, unit, source, notes}` with `source.kind`:

- `can_signal` — `{bus, frame, signal}` of this definition;
- `kwp_measuring_block` — `{group, cell}` of a KWP2000 `0x21` block (VAG
  3-byte cells, scaled by the engine's formula table);
- `uds_did` — `{did, byte, len (1..4), signed, factor, offset}`.

Names are unique per bus set (the same channel may come from two alternative
sets) and among the non-CAN sources. `decode` reports the CAN ones.
`opentune dsg groups` labels the measuring-group cells named by
`kwp_measuring_block` entries (and reports every other cell as unconfirmed).

### `adaptations[]`

`{name, kind (adaptation | basic_setting | routine | reset), service,
writes, requires_security, table, validation, notes}`. `service` is text
(`UDS 0x2E 0xFDA2`) and stays empty when the stock service is not public;
`table` names the table that holds the learned values.

### `tables[]`

`{name, role, profiles, unit, rows, cols, x_axis, y_axis, storage,
ecudef_map, confidence, notes, source}`.

- `role`: `shift_up`, `shift_down`, `shift_time`, `shift_pressure`,
  `fill_time`, `fill_pressure`, `tcc_slip`, `tcc_pressure`, `torque_limit`,
  `adaptation`, `launch`, `coupling_lock`, `other`.
- An axis is `{name, unit}` plus exactly one of `values` (numeric
  breakpoints), `labels` (e.g. `1-2`, `K1`) or `derived` (a formula, with
  `count`). Its length must equal `cols` (x) / `rows` (y); a curve has
  `rows: 1` and may omit `y_axis`.
- `storage` (`address`, `data_type`, `endian`, `factor`, `offset`) when the
  table's place in an image is known, or `ecudef_map` `{ecudef, map}` when a
  linked ecudef already defines it.
- `confidence`: `high`, `medium`, `low`, or `structure_only` — shape and
  axes only. A table without `storage`/`ecudef_map` **must** be
  `structure_only`, and a `structure_only` table must not claim a location.

### `gearsets[]`

`{name, applies_to, gears: [{gear, ratio}], source, notes}`; reverse ratios
are negative.

## Validation rules

`opentune controller validate` (and the `repo_ctrldefs` test) apply these;
errors fail CI, warnings are shown with `--warnings`.

| Code | Severity | Rule |
|---|---|---|
| `id.format` | error | id is lowercase `a-z0-9` words joined by `-` (at least two). |
| `id.filename` | error | file stem equals the id. |
| `identity.empty` | error | `name`, `maker`, `family` are set. |
| `safety.missing` | error | at least one safety note. |
| `sources.missing` | error | at least one source. |
| `sources.incomplete` | error | every source has title, licence and use. |
| `identify.empty` | warn | no identify keys. |
| `checksum.empty` | error | `checksum` is an id or `null`. |
| `ecudefs.duplicate` | error | an ecudef listed twice. |
| `diag.name` | error | channel has a name. |
| `diag.bitrate` | error | bitrate > 0. |
| `diag.can_id` | error | ids fit 11 bits (29 with `extended_ids`). |
| `diag.isotp_ids` | error | `iso_tp` has `tx_id` and `rx_id`. |
| `diag.tp20_address` | error | `vwtp20` has `tp20_address`. |
| `diag.kline_address` | warn | K-line channel without an address. |
| `diag.identification` | error | identification request is even-length hex. |
| `bus.name` | error | set names are lowercase without spaces. |
| `bus.duplicate` | error | set defined twice. |
| `bus.bitrate` | error | bitrate > 0. |
| `frame.id` | error | id fits 11 / 29 bits. |
| `frame.duplicate_id` | error | id defined twice in one set. |
| `frame.duplicate_name` | error | frame name used twice in one set. |
| `frame.min_len` | error | `min_len` <= `dlc`. |
| `frame.empty` | warn | frame without signals. |
| `signal.duplicate` | error | signal name used twice in one frame. |
| `signal.layout` | error | the bits fit the frame (`dlc`, default 8 bytes). |
| `signal.kind` | error | `bool` is 1 bit, `ascii` 8 bits, `enum` has values. |
| `signal.scaling` | error | factor finite and non-zero, offset finite. |
| `signal.curve` | error | at least two points, raw ascending, not mixed with factor/offset. |
| `signal.value` | error | enum raws fit the width and are unique. |
| `signal.invalid` | error | invalid markers fit the width. |
| `signal.raw_range` | error | `[min, max]` ordered and within the width. |
| `signal.overlap` | warn | two signals of one frame share some, but not all, payload bits (the same bits under two names are an alternative view and fine). |
| `live.duplicate` | error | live name used twice for the same source set. |
| `live.ref` | error | a `can_signal` source names an existing bus/frame/signal. |
| `live.uds_len` | error | UDS value length 1..4. |
| `live.scaling` | error | UDS factor finite and non-zero. |
| `live.kwp_duplicate` | error | a measuring-block `(group, cell)` is named by one live entry only. |
| `adaptation.duplicate` | error | adaptation name used twice. |
| `adaptation.table` | error | `table` names an existing table. |
| `table.duplicate` | error | table name used twice. |
| `table.dims` | error | rows, cols >= 1. |
| `table.axis_kind` | error | exactly one of values / labels / derived. |
| `table.axis_count` | error | a derived axis has `count`. |
| `table.axis_len` | error | axis length equals the table dimension. |
| `table.axis_monotonic` | warn | numeric breakpoints strictly increasing. |
| `table.axis_value` | error | breakpoints are finite. |
| `table.y_axis` | error | `rows > 1` needs a y axis. |
| `table.confidence` | error | location claims and `structure_only` are consistent. |
| `table.storage` | error | storage factor finite and non-zero. |
| `table.ecudef_map` | error | `ecudef_map.ecudef` is listed in `ecudefs`. |
| `table.source` | warn | table has a source. |
| `gearset.empty` | error | gear set has gears. |
| `gearset.duplicate` | error | gear listed twice. |
| `gearset.ratio` | error | ratio finite and non-zero. |
| `gearset.order` | warn | forward ratios strictly decreasing. |
| `flash.region` | error | regions inside a known flash size. |
| `scope.empty` | warn | no diagnostics, buses, ecudefs or flash: nothing to act on. |
| `link.ecudef` | error | linked ecudef exists. |
| `link.unit` | error | linked database unit exists. |
| `link.checksum` | error | checksum id is known to `opentune-checksum`. |
| `link.checksum_mismatch` | warn | `checksum` differs from a linked ecudef's. |
| `link.ecudef_map` | error | the named map exists in that ecudef. |

## In definition packs

The repository's `ctrldefs/` is the **built-in set**. A
[definition pack](DEFINITION-PACKS.md) carries more under `controllers/`:

```
my-pack/
  pack.json
  controllers/acme-t1.json     # one ControllerDef, file named after its id
```

`opentune-packs` loads each file as a `ControllerDef` (unknown fields
rejected) and runs `validate()` plus the file-name rule; a file with an error
is skipped and listed in the pack's load report with the reasons, the others
become pack entries of kind `controller` (sub-directory `controllers`). The
`link.*` rules need the repository's ecudefs, database units and checksum ids,
so the consumer checks them: the desktop **Controllers** panel resolves a
pack definition's links against the bundled ecudefs plus the pack's own
`ecudefs/`. `opentune packs <dir>` counts a pack's controllers; to run the
CLI's `controller` commands on a pack definition, pass its path
(`opentune controller show packs/<id>/controllers/<file>.json`) or
`--dir packs/<id>/controllers`.

The first community pack, [`packs/community-drivetrain/`](../packs/community-drivetrain/),
carries Hyundai / Kia TCU and 4WD-coupling definitions and a Subaru
Global-platform transmission definition this way (CAN frames transcribed from
opendbc, synthetic-tested); [PACK-AUTHORING.md](PACK-AUTHORING.md) shows how it
was made.

## Adding a controller

1. Copy the closest file in `ctrldefs/` (a TCU with CAN frames:
   `mercedes-egs5x-7226.json`; one that only links ecudefs:
   `vag-dq381.json`), rename it to the new id.
2. Public facts only, each with a source and licence in `sources`. No OEM
   seed/key data, no OEM diagnostic databases, no protected calibration
   values. Unknown is written as unknown (in `notes`), never guessed.
3. Mark what is evidence: `validation` per definition, frame and channel;
   `confidence` per table.
4. Run `opentune controller validate --warnings` and
   `cargo test -p opentune-ctrldef` (the `repo_ctrldefs` test loads every
   file and checks its links). Add synthetic-frame tests for new CAN layouts,
   building the frames independently of the file (see
   `crates/opentune-ctrldef/tests/mercedes_7226.rs`).
