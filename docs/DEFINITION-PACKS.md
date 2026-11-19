# OpenTune definition packs

A **definition pack** is one versioned, loadable bundle of OpenTune's data-defined
knowledge for a vehicle or platform. It is how coverage grows by *data* instead of
code: drop in a pack and every service engine (coding, adaptation, service
functions, logging, logging triggers, auto-scan, drivetrain controller
definitions) picks up what it needs. Implemented by the
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
  controllers/*.json   # ControllerDef     — drivetrain TCU / AWD definitions
  triggers/*.json      # TriggerSet        — datalogging triggers (WOT pulls, marks)
```

`controllers/` holds the same files as the repository's built-in
[`ctrldefs/`](../ctrldefs/) ([CONTROLLER-DEFS.md](CONTROLLER-DEFS.md)); each file
is named after its `id`.

`triggers/` holds datalogging trigger files (`"format": "opentune-triggers"`)
for `opentune log --trigger` / `opentune obd read --trigger`
([LIVE-LOGGING.md](LIVE-LOGGING.md#triggers)). A trigger set with an `id` is
named after it. Its channel ids should be ones the pack's own `loggers/`
carry (the `repo_packs` tests check the shipped packs); list alternatives to
also match `obd read`'s channel keys.

Any sub-directory is optional. Files named `TEMPLATE.json` or starting with `_`/`.`
are treated as scaffolding and skipped, and so are pack *directories* starting with
`_`/`.` (e.g. `packs/_template/`) during discovery.

The repository ships starter packs in [`packs/`](../packs/) (listed with their
sources in [`packs/README.md`](../packs/README.md)): `generic-obd2`,
`iso14229-identification` and `vag-modules`. The desktop app bundles that folder
as a resource.

In the desktop app, the Coding, Adaptation, Service fn, Auto-scan and Live log
panels choose their definition with one **pack picker**: a pack (only those
that carry the panel's kind are offered), then the definition inside it. It
shows the pack's id, version, licence and validation state — a pack whose
loader skipped files says how many, and lists each file with the reason.
**Load pack…** opens a pack from anywhere on disk (its `pack.json`, its folder
or a definition inside it); the app remembers it and offers it in every panel.
**File…** still takes a single loose definition. Each panel remembers its last
choice. Backend: `packs_list` / `packs_open` (`desktop/src-tauri/src/packs.rs`).
The **Controllers** panel mounts the same picker for `controllers/`; its list
shows the built-in `ctrldefs/` plus the controllers of every installed pack and
of every pack loaded from disk, each marked with its pack and validation state.

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
  "description": "Provenance / coverage notes.",
  "sources": ["opendbc opendbc/car/volkswagen/values.py @ <commit> (MIT)"],
  "evidence": "synthetic-tested"
}
```

To **load**, only `id` and `name` are required; unknown fields are ignored for
forward compatibility. To be **accepted** (`opentune packs validate`, the CI
gate), a manifest also needs:

- `id` equal to the pack's folder name (lowercase, digits, `-`), a `name` and a
  `version` (`MAJOR.MINOR.PATCH`);
- `sources` — one citation per source: a standard by name, an open project with
  path/commit and licence, or "own capture" for your own data. No output of
  commercial / OEM tools;
- `license` — the SPDX id of the pack's data (normally `GPL-3.0-or-later`).
  Packs built from community/open sources keep their license; proprietary,
  non-commercial or vendor-derived data does not belong in a pack;
- `evidence` — the validation state the pack's data as a whole has reached:
  `implemented`, `synthetic-tested`, `real-image-tested`, `bench-tested` or
  `vehicle-tested` ([`DEVELOPMENT.md`](DEVELOPMENT.md) §7). If only part of it
  was exercised, use the lower state and say what was tested in `description`.

## Loading behaviour (honest, never silent)

`opentune_packs::load_pack(dir)` and `PackLibrary::load_all(root)`:

- error only when `pack.json` is missing or unparseable (that's what makes a
  directory a pack);
- load each definition file into its typed form and, where the type supports it
  (coding / adaptation / service scripts / ECU defs / controller defs),
  **validate** it. Controller definitions get `opentune-ctrldef`'s structure and
  honesty rules plus its file-name rule; their links (ecudefs, database units,
  checksum ids) need the repository and are checked by the consumer (see
  [CONTROLLER-DEFS.md](CONTROLLER-DEFS.md#in-definition-packs)). Trigger sets
  get `TriggerSet::validate` (format and version, unique names, non-empty
  `all`/`any`, finite values, hold and window limits, no pre/post window on
  a `mark`) and the file-name rule;
- a file that fails to parse or validate is recorded in the pack's `LoadReport`
  with the reason and **skipped** — it never aborts the pack or disappears
  silently.

`PackLibrary` merges several packs and offers simple cross-pack lookups
(`coding_by_id`, `service_by_id`, `controller_by_id`, iterate all defs of a
kind, e.g. `triggers()`, per-kind counts).

## CLI

```
opentune packs <dir>
```

prints each pack's identity, license and per-kind definition counts, plus any
per-file load notes, and a total across all discovered packs. The default `<dir>`
is `packs`, so `opentune packs` from the repository root lists the shipped packs.

```
opentune packs validate <dir> [--warnings] [--allow <file>]
```

is the contributor gate (CI runs `opentune packs validate packs`). For the pack
at `<dir>`, or every pack under it, it reports as errors: an incomplete
manifest (above), every file that fails to parse or validate, and every
privacy-scan finding (VIN-like identifiers, e-mail addresses outside the
reserved `example.*` / `.test` domains, serial-number-like values) in the
pack's `*.json` / `*.md` / `*.txt` / `*.csv` files, printed redacted. A
sub-directory the loader does not know (e.g. a misspelt `service/`) is a
warning. It exits non-zero on any error. `--allow` names a file of exact values
to ignore, one per line, for deliberate synthetic data the heuristics flag.

## Writing a pack

1. **Copy the scaffold.** `cp -r packs/_template packs/<your-id>` and set `id`
   (= the folder name), `name`, `version`, `license`, `sources` and `evidence`
   in `pack.json`. Delete the sub-folders you do not need.
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
     and OpenTune ships no seed/key algorithms. An optional `seedkey` id names
     the algorithm a host application (e.g. a desktop plugin) must supply for
     that step; without one, a secured script can be planned but not run.
   - **Controller definition** (`controllers/*.json`, `ControllerDef`) — copy
     the closest file in [`ctrldefs/`](../ctrldefs/) and follow
     [CONTROLLER-DEFS.md](CONTROLLER-DEFS.md) (public sources with licences,
     safety notes, evidence words). Check it with
     `opentune controller validate packs/<your-id>/controllers --warnings`.
   - **Trigger set** (`triggers/*.json`, `TriggerSet`) — copy
     `packs/generic-obd2/triggers/wot-pull.json` and follow
     [LIVE-LOGGING.md](LIVE-LOGGING.md#triggers); try it offline with
     `opentune obd read --driver obdsim --rounds 20 --out t.csv --trigger <file>`.
   - **Coding / adaptation / ECU defs** follow `opentune-coding`,
     `opentune-adaptation` ([`ADAPTATION.md`](ADAPTATION.md), UDS and VAG
     KWP2000 channel kinds) and [`ECU-DEFS.md`](ECU-DEFS.md). Open,
     license-compatible coding data is rare and vendor label files are
     off-limits; `vag-mib2-coding` is the one brand coding pack so far
     ([`CODING.md`](CODING.md) lists its sources and the ones rejected).
4. **Check it.** `opentune packs validate packs/<your-id>` must report 0
   errors (it also runs on a fork: point it at any pack folder), and
   `cargo test -p opentune-packs` (the `repo_packs` tests load and validate
   every pack under `packs/`).
5. **Document it.** Add a row to `packs/README.md` with contents, license and
   sources.
