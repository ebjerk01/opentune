# Control-unit database (`database/`)

The desktop **ECU database** panel and `opentune db list|show|validate` read
`database/units/*.json`: one file per ECU / TCU / AWD family or variant, saying what
it is, how it can be read (OBD / bench / boot), which OpenTune checksum applies,
which vehicles use it and - only where a public source can be cited - its bench
pinout. Schema: [`database-unit.schema.json`](database-unit.schema.json). Loader and
validator: `crates/opentune-db`.

## Rules (enforced by `cargo test -p opentune-db` / `opentune db validate`)

A wrong pin on the bench can destroy a control unit, so the pinout rules are strict:

- **Every pin cites a source** (`sources` = indices into the unit's `sources` list).
  Never guess, never infer a pin from "similar" units.
- **Confidence is honest.** `verified` = 2+ independent public sources agree (two
  pages of the same site count once); `community` = one credible source;
  `unverified` = no usable source or the sources disagree. A pin's `confidence`
  defaults to the unit's `pinout_confidence`.
- **Unverified pins are never wired.** `bench` lists may only reference
  `community`/`verified` pins, and the unit's `pinout_confidence` can never be higher
  than the weakest pin the bench wiring uses. When sources disagree on a
  safety-critical pin (e.g. ground), list both candidates as `unverified`, leave the
  bench list empty and explain it in `bench.notes`.
- **No pinout? Say so.** Leave `connectors` empty, `bench`/`pinout_confidence` null,
  and explain why in `notes`.
- Every unit with a pinout carries a `safety_note`.
- `checksum` must be an `opentune_checksum::get()` id; `ecudef` must exist in
  `ecudefs/`. `read_methods.obd` is `unknown` when no source says either way.
- **Access profile.** Every unit carries an `access` block (OBD / bench / boot /
  security, each with confidence and sources) that agrees with `read_methods`.
  `opentune db validate --require-access` (CI) fails on a unit without one
  unless it is listed in `database/units/.access-pending`, a list that only
  shrinks. Schema, rules and allowed sources:
  [UNIT-ACCESS-PROFILE.md](UNIT-ACCESS-PROFILE.md).

## Scope

Facts only, restated in your own words: no copied diagrams/images or long text from
commercial tool vendors, no seed/key algorithms, no OEM binaries or protected
calibration data, no private data (VINs, names, hostnames).
