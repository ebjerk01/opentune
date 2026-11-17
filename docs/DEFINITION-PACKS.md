# OpenTune definition packs

A **definition pack** is one versioned, loadable bundle of OpenTune's data-defined
knowledge for a vehicle or platform. It is how coverage grows by *data* instead of
code: drop in a pack and every service engine (coding, adaptation, service
functions, logging, auto-scan) picks up what it needs. Implemented by the
`opentune-packs` crate; load/inspect one with `opentune packs <dir>`.

## Layout

A pack is a directory with a `pack.json` manifest and conventional
sub-directories of `*.json` definition files — drop a file in the right folder and
it is loaded:

```
my-pack/
  pack.json            # the manifest (metadata)
  ecudefs/*.json       # EcuDef            — ECU identity, memory map, maps
  coding/*.json        # CodingDef         — module coding-label sets
  adaptation/*.json    # AdaptationDef     — adaptation channels / actuator tests
  services/*.json      # ServiceScript     — service-function scripts
  loggers/*.json       # LogDef            — live-data / logger definitions
  modules/*.json       # VehicleModules    — module lists for auto-scan
```

Any sub-directory is optional. Files named `TEMPLATE.json` or starting with `_`/`.`
are treated as scaffolding and skipped.

## Manifest (`pack.json`)

```json
{
  "id": "vag-mqb-community",
  "name": "VAG MQB (community)",
  "version": "1.2.0",
  "maker": "VAG",
  "platform": "MQB",
  "vehicles": ["Golf 7", "A3 8V"],
  "author": "…",
  "license": "GPL-3.0-or-later",
  "description": "Provenance / coverage notes."
}
```

Only `id` and `name` are required; unknown fields are ignored for forward
compatibility. **Record the license and provenance** of the definition data —
packs built from community/open sources keep their license; proprietary or
vendor-derived data does not belong in a public/community pack.

## Loading behaviour (honest, never silent)

`opentune_packs::load_pack(dir)` and `PackLibrary::load_all(root)`:

- error only when `pack.json` is missing or unparseable (that's what makes a
  directory a pack);
- load each definition file into its typed form and, where the type supports it
  (coding / adaptation / service scripts / ECU defs), **validate** it;
- a file that fails to parse or validate is recorded in the pack's `LoadReport`
  with the reason and **skipped** — it never aborts the pack or disappears
  silently.

`PackLibrary` merges several packs and offers simple cross-pack lookups
(`coding_by_id`, `service_by_id`, iterate all defs of a kind, per-kind counts).

## CLI

```
opentune packs <dir>
```

prints each pack's identity, license and per-kind definition counts, plus any
per-file load notes, and a total across all discovered packs.
