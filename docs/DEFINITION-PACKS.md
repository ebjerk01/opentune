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
are treated as scaffolding and skipped, and so are pack *directories* starting with
`_`/`.` (e.g. `packs/_template/`) during discovery.

The repository ships starter packs in [`packs/`](../packs/) (listed with their
sources in [`packs/README.md`](../packs/README.md)): `generic-obd2`,
`iso14229-identification` and `vag-modules`. The desktop app bundles that folder
as a resource; the Auto-scan, Service and Live-log file pickers open there.

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
per-file load notes, and a total across all discovered packs. The default `<dir>`
is `packs`, so `opentune packs` from the repository root lists the shipped packs.

## Writing a pack

1. **Copy the scaffold.** `cp -r packs/_template packs/<your-id>` and set `id`
   (= the folder name), `name`, `version`, `license` and `sources` in
   `pack.json`. Delete the sub-folders you do not need.
2. **Decide where each fact comes from before writing it.** Standards (ISO
   14229, SAE J1979 / ISO 15031, ISO 15765-4) may be used by their published
   identifiers. Anything else needs an open, license-compatible source, recorded
   per entry (`"source": "project path/file (license)"`). Unknown fields are
   ignored by the loaders, so `source` / `notes` keys can sit next to the data.
   No source → leave the entry out.
3. **Write the definitions.** Each file is the serde JSON of one engine type;
   the shipped packs are worked examples:

   - **Module list** (`modules/*.json`, `VehicleModules`) — see
     `packs/vag-modules/modules/vag-uds-can.json`:
     ```json
     { "vehicle": "…", "modules": [
       { "name": "Airbag / SRS", "addr": 21, "req_id": 1813, "resp_id": 1919,
         "protocol": "uds", "source": "opendbc (Ecu.srs 0x715, rx offset 0x6A)" } ] }
     ```
     `protocol` is `uds` or `obd`; Auto-scan re-binds the adapter to each
     module's `req_id`/`resp_id`.
   - **Logger** (`loggers/*.json`, `LogDef`) — see
     `packs/generic-obd2/loggers/obd2-mode01.json`. A parameter names its
     `protocol` (`obd2_mode01` / `uds_mode22`), its `addressing`
     (`{"addressing":"obd_pid","pid":12}` / `{"addressing":"uds_did","did":61840}`)
     and an optional linear `conversion` whose `expr` uses `X` for the raw value
     **and** whose `fit` gives the same line (`{"fit":"linear","factor":0.25,"offset":0}`
     for `X/4`). The repository test re-fits every `expr` and fails on a
     mismatch. The live engine decodes the whole response payload as one
     unsigned big-endian number, so only single-value PIDs/DIDs are useful.
   - **Service script** (`services/*.json`, `ServiceScript`) — see
     `packs/generic-obd2/services/`. Steps are tagged by `op`:
     `diagnostic_session`, `security_access`, `routine_control`, `io_control`,
     `write_data_by_identifier`, `read_data_by_identifier`, `tester_present`,
     `wait`, `expect_positive`, `verify`, and `obd_request` (SAE J1979 modes
     01/03/04/07/09/0A; mode 04 counts as a write). An optional top-level
     `req_id`/`resp_id` gives the default target. Every real run needs the
     operator's confirmation tick; writes/actions additionally need
     `requires_security` + a `security_access` step when the module demands it,
     and OpenTune ships no seed/key algorithms.
   - **Coding / adaptation / ECU defs** follow `opentune-coding`,
     `opentune-adaptation` and [`ECU-DEFS.md`](ECU-DEFS.md). No starter pack
     ships them yet: open, license-compatible coding data is rare, and vendor
     label files are off-limits.
4. **Check it loads cleanly.** `opentune packs packs/<your-id>` must show no
   load notes, and `cargo test -p opentune-packs` (the `repo_packs` test loads
   every pack under `packs/` and requires zero errors).
5. **Document it.** Add a row to `packs/README.md` with contents, license and
   sources.
