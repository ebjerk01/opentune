# Unit access profiles and wiring diagrams

Every control unit in the database (`database/units/*.json`, crate
`opentune-db`) describes how it can be read and wired. This page covers the
**access profile** (the `access` block: OBD, bench, boot mode, security) and
the **wiring diagram** that the app draws from the pin data. Schema:
[`database-unit.schema.json`](database-unit.schema.json); general database
rules: [`DATABASE.md`](DATABASE.md).

## What "confidence" means

Pins carry a confidence (their own, else the unit's `pinout_confidence`):

| Level | Meaning |
|---|---|
| `verified` | **Two or more independent sources agree.** It does **not** mean the pin was measured on a physical unit. The validator enforces at least two distinct sources. |
| `community` | One source. |
| `unverified` | Disputed between sources, or a single conflicting source. |

A unit's `pinout_confidence` may not be higher than its weakest bench pin.

The access profile uses the same three words for each of its parts, with the
same source rule (`verified` needs two distinct sources). Choose the level
for what the sources say about **this unit**: a fact that only a sibling unit
or a generic datasheet supports is `unverified` for this unit, however good
the datasheet is.

## Access profile (`access`)

Optional per unit in the schema, required by the CI gate for every unit not
listed in `database/units/.access-pending` (see [the gate](#the-ci-gate)).
Every part has `confidence` and `sources` (indices into the unit's `sources`
list, like pins); every part needs at least one source.

```jsonc
"access": {
  "obd":   { "mode": "read+write | read | write-only | none | unknown",
             "protocol": "KWP2000 (ISO 14230) on the K-line, pin 3-28" | null,
             "notes": "...", "confidence": "...", "sources": [0, 1] },
  "bench": { "possible": true, "how": "which pins, what it gives you",
             "confidence": "...", "sources": [0, 1] },
  "boot":  { "possible": true,
             "entry": "connector-pin | board-pad | bdm-jtag | none | unknown",
             "mcu_mode": "the processor's boot mode in plain words" | null,
             "points": [ { "name": "...",
                           "location": "connector:<connector>-<pin>" | "pcb:<label>",
                           "action": "pull-to-gnd | pull-to-vcc | bridge | connect",
                           "resistor_ohms": 8000,          // optional
                           "note": "...",                 // optional
                           "confidence": "..." } ],
             "reset_timing": "when to apply / release the strap" | null,
             "reachable": "external-connector | open-case | unknown",
             "notes": "...", "confidence": "...", "sources": [3, 4] },
  "security": { "diag_read": "required | not-required | unknown",
                "flash_write": "required | not-required | unknown",
                "notes": "... OpenTune ships no OEM key ...",
                "confidence": "...", "sources": [5] }
}
```

| Part | Meaning |
|---|---|
| `obd.mode` | What works through the vehicle's diagnostic socket: `read+write`, `read` (complete read, no programming), `write-only` (programming, but a complete original read needs bench or boot), `none`, `unknown` (no source either way, never guessed). |
| `bench` | Talking to the unit on the desk through **its own connector, case closed**. `how` says which pins and what it gives you. |
| `boot` | A processor boot / service mode. `possible` is a tri-state: `true` (a documented route), `false` (a source says there is **no** route; `entry: none`), or `"unknown"` (no public source documents one either way; `entry: unknown`). Prefer `"unknown"` over `false` when nobody says. `reachable: open-case` means the case must be opened. `bench.possible` takes the same three values. |
| `boot.points` | Where to strap. `connector:<connector>-<pin>` must name a `signal: boot` pin of the unit; `pcb:<label>` names a pad, component pin or test point. A point's confidence may not exceed the boot block's. `action`: `pull-to-gnd` / `pull-to-vcc` (a strap, optionally through `resistor_ohms`), `bridge` (short two points), or `connect` (attach a BDM / JTAG probe to a header or pads; only with `entry: bdm-jtag`, never as a pin `strap`). |
| `security` | Whether a complete **read over the diagnostic protocol** (`diag_read`) and **programming** (`flash_write`) need security access. Facts only. |

Related optional fields:

- `flash_note` (top level): a caveat on `flash_kib`, e.g. "29F400 (512 KiB)
  or 29F800 (1 MiB) by revision - confirm from the chip".
- `strap` on a `signal: boot` pin: `{ "action": "pull-to-gnd", "resistor_ohms": 4700 }`.
- `read_methods.obd` also accepts `"read"`.

### Validator rules (`opentune db validate`)

1. **`read_methods` agrees with `access`**: `obd` = `obd.mode`; `bench` and
   `boot` (booleans) are `true` exactly when `bench.possible` / `boot.possible`
   is `true` (`false` and `"unknown"` both mean `false` there). A mismatch is
   an error. Plain JSON booleans for `possible` are still accepted.
2. **Every part cites a source**, indices in range; `verified` needs two
   distinct sources (the same index twice counts once). A boot point marked
   `verified` needs two distinct boot sources.
3. **`boot.possible: true` needs** at least one point, a known `entry` (not
   `none`/`unknown`) and `reachable` other than `unknown`.
   **`false` needs** `entry: none`, no points and `reachable: unknown`.
   **`"unknown"` needs** `entry: unknown`, no points and `reachable: unknown`.
4. **Points resolve**: `connector:` to a boot pin of the unit, `pcb:` with a
   label, and a board point is never `external-connector`. Point confidence
   is not higher than the boot block's; `resistor_ohms` is positive.
5. **Security notes say "OpenTune ships no OEM key"** (case-insensitive).
   There is **no field for key material** at all: a `key`, `seed`,
   `algorithm` or any other unknown field fails to load
   (`deny_unknown_fields`).
6. `strap` only on `signal: boot` pins; texts not empty.

The pinout rules (sources per pin, `verified` = 2+ sources, unverified pins
never wired, `pinout_confidence` capped by the weakest bench pin) are
unchanged; the access profile does not recompute any confidence.

### The CI gate

`opentune db validate --require-access` runs in CI ("validate data" job) and
in `scripts/check.sh`:

- a unit **with** `access`: checked by the rules above;
- a unit **without** `access` that is listed in
  `database/units/.access-pending`: **warning**;
- a unit without `access` that is **not** listed (every new unit file):
  **error**;
- a listed unit that now has `access`, or a listed id with no unit file:
  **error** - remove it from the list.

`.access-pending` holds the units that predate the profile. **It only
shrinks**: remove an id in the same change that fills its profile, never add
one. A test (`crates/opentune-db/tests/access.rs`) caps its length; lower the
cap when you fill a unit.

### Sources: what is allowed

Public sources only, facts only, restated in your own words, each cited in
the unit's `sources` and referenced by index:

- **Datasheets and manuals of standard parts** (processor user manuals, data
  sheets and errata; flash / EEPROM / transceiver data sheets). They are the
  primary source for *how* a boot mode works (entry level, timing, pin
  numbers of the chip).
- **Public pinout sites** and service literature that is publicly hosted.
- **Public forums and blog posts**: facts only ("pin 24 of the flash to
  ground at power-up"), no copied images or long text.
- **Tool vendors' or retailers' public pages** (support lists, product pages)
  only as a **fallback**, when no datasheet or open project covers the fact:
  they may corroborate *that* an access path or chip exists, at `community` at
  most; their procedures and tool data are never used. When an open project
  covers the fact, cite the open project instead and drop the vendor page.
- **Open projects** with their licence noted (emulators, open replacement
  controllers, open flashing tools).
- OpenTune's own docs, for facts OpenTune itself established.

### Sources: what is NOT allowed

- **Commercial-tool internal data**: tool pin sheets, help files, wiring
  sheets or databases shipped inside a commercial tool, and tool output.
- **Leaked manuals** and **unauthorised copies**, e.g. Scribd or file-host
  uploads of tool sheets or dealer documents, even when publicly reachable.
- **OEM dealer data** (flash containers, diagnostic descriptions,
  calibration databases derived from them).
- **Private research** (the maintainers' non-public notes and reads) and
  non-clean-room code: facts from it may only enter once a public source says
  the same, and then that public source is cited.
- **Anything immobiliser-related**: no transponder, immobiliser, key-learning
  or component-protection procedures, and no seed/key, keys or algorithms.

### Worked example: Bosch EDC15C6 (`bosch-edc15c6-mercedes`)

| Part | Value | Confidence and why |
|---|---|---|
| `obd` | `read+write`, KWP2000 on the K-line, pin 3-28 | `unverified`: the K-line pin agrees between two pinout sites, but no cited source states read/write for this unit; carried over from OpenTune's definition, not yet tried. |
| `bench` | possible: terminal 30 on 2-13, terminal 15 on 1-1, K-line 3-28; ground disputed (1-5 / 1-6) | `community`, the unit's pinout confidence. |
| `boot` | possible, `board-pad`, `open-case`; C167 bootstrap loader on ASC0 (zero byte, ID `C5h`/`D5h`, 32-byte loader); strap P0L.4 low at the end of reset, P0L.5/.3/.2 high (Table 19-2), reset pulse at least 1032 TCL (errata CPU.7) | `unverified` for this unit: the mechanism is from the Infineon manual, the entry point (flash pin 24 to ground) from a retailer post about the sibling VAG EDC15P. |
| `boot.points[0]` | `pcb:flash DQ4 (pin 24 of the SO-44 29F400/29F800)`, `pull-to-gnd`, 8 kOhm | `unverified`: only valid if the board routes flash DQ4 to CPU AD4 (C167CR P-MQFP-144 pin 104 = P0L.4/AD4); confirm by continuity between flash pin 24 and CPU pin 104. |
| `security` | `diag_read` and `flash_write` required | `unverified`; OpenTune ships no OEM key. |
| `flash_note` | 29F400 (512 KiB) or 29F800 (1 MiB) by revision - confirm from the chip | |

Sources: Infineon C167CR user manual (ch. 14, Fig. 14-2, Table 19-2), C167CR
data sheet (pin 104), SAK-C167CR-LM errata sheet, AMD Am29F800B data sheet
(SO-44 pin 24 = DQ4), the OBD2Express retailer post on the EDC15P boot read
(`community` at most), OpenTune's [C167-BSL.md](C167-BSL.md). A Scribd copy of
a tool's boot sheet is deliberately **not** cited. The existing pin data and
both pin disputes are unchanged.

In the app the **ECU database** panel shows the profile in an **Access** card
(OBD / bench / boot / security with confidence pills, an "open case" marker
and the boot points). The read-only C167 bootstrap read itself is
[C167-BSL.md](C167-BSL.md).

## Wiring diagram (data-driven)

`desktop/ui/src/pinout-svg.js` (`OTPinout.svg(unit)`) turns a unit's
`connectors[].pins[]` into an inline SVG, so every unit with pins gets a diagram
for free and it never drifts from the data.

- **It is a wiring diagram, not a connector face.** Sources give pin numbers and
  functions, not physical positions, so the diagram does not pretend to know
  the layout.
- **Left:** one block per connector, pins in numeric order, labelled
  `<connector>-<pin>` with the function text (full text and the pin note in a
  tooltip).
- **Right:** the bench targets some pin needs, in a fixed order: +12 V
  permanent (KL30), +12 V ignition (KL15), ground, K-line, CAN high, CAN low,
  boot strap. Pins with signal `other` get no wire.
- **Wires** are coloured by signal (theme tokens, so light and dark mode both
  work; standalone it falls back to fixed colours).
- **Confidence:** a pin with two or more distinct sources shows "2 src ✓";
  an `unverified` pin is highlighted, marked **?** and its wire is **dashed**
  ("disputed — confirm by continuity"). The legend states the unit's level and
  that "verified" means sources agree.

Where it appears:

- **ECU database** panel: above the pin table of every unit with pins.
- **Guide pages:** a fenced block

  ````markdown
  ```pinout bosch-edc15c6-mercedes
  ```
  ````

  renders as that unit's diagram (`OTUtil.renderMarkdown` emits a placeholder,
  `OTPinout.hydrate` fills it from `db_get`). Unknown ids show a clear message.

## Sources

Public sources only (pinout sites, datasheets, public forums, open projects),
facts only, each cited in the unit's `sources` and referenced per pin. No data
from commercial-tool internals, leaked manuals or private research. The full
list is under [what is allowed](#sources-what-is-allowed) /
[not allowed](#sources-what-is-not-allowed) above.
