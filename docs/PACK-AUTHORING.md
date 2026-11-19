# Write and submit a definition pack

A step-by-step guide from an empty folder to a merged pull request. It follows
how the first community pack,
[`packs/community-drivetrain/`](../packs/community-drivetrain/), was made, so
every command and file below is real and you can copy them. The format reference
(manifest fields, sub-directories, loader rules) is
[DEFINITION-PACKS.md](DEFINITION-PACKS.md); the rules every contribution
follows are in [CONTRIBUTING.md](../CONTRIBUTING.md).

You need a fork of the repository and either a released `opentune` binary or a
Rust toolchain (`cargo run -p opentune-cli --` works wherever this guide says
`opentune`). No car and no adapter are needed: everything here runs offline.

## 1. Choose what the pack adds

A pack is worth reviewing when it adds something the repository does not have.
Check first:

```sh
opentune controller list        # built-in controller definitions (ctrldefs/)
opentune packs packs            # every shipped pack and what it carries
```

*community-drivetrain*: the built-in set covered VAG, BorgWarner Haldex, ZF and
Mercedes units. The open opendbc project has CAN layouts for Hyundai / Kia TCU
and 4WD-coupling frames and for the Subaru Global-platform transmission, none of
which OpenTune had. That decided the scope: three controller definitions, nothing
else. A pack that only carries what you can source and test is better than a big
one with guesses.

## 2. Find a source you may use

Every fact that is not a published standard identifier needs an **open,
GPL-3.0-compatible source**, cited with the project, file, commit and licence.

| Usable | Not usable |
|---|---|
| Standards by their published identifiers (ISO 14229, ISO 15765-4, SAE J1979) | Output of commercial / OEM tools (label files, coding data, XDFs or definitions without an open licence, DAMOS / A2L) |
| Open projects under MIT, BSD, Apache-2.0, GPL, LGPL, MPL, CC0, CC-BY(-SA) | Anything without a licence, or with a non-commercial / "all rights reserved" one |
| Your own measurements, cited as "own capture" / "own read" (the data itself stays out of the repository) | Decompiled or reverse-engineered proprietary software, OEM documents, leaked files |

Pin the source to a commit, so a reviewer sees exactly what you read.
*community-drivetrain* uses [commaai/opendbc](https://github.com/commaai/opendbc)
at commit `27fa1c5d8eed3238a0a78c52ff5abb6cdc807bd2` (MIT, Copyright (c) 2020,
Comma.ai, Inc.):

- `opendbc/dbc/generator/hyundai/hyundai_can.dbc`: frames `TCU11`-`TCU13`,
  `_4WD11`-`_4WD13`, `WHL_SPD11`;
- `opendbc/dbc/generator/subaru/subaru_global_2017.dbc` and
  `_subaru_global.dbc`: frames `Transmission`, `CVT`, `Wheel_Speeds`;
- `opendbc/car/{hyundai,subaru}/values.py` and `fingerprints.py`: which cars use
  those DBCs, the transmission request id `0x7E1` and the identification
  requests (`22 F100`, `22 F182`);
- ISO 15765-4 for "response id = request id + 8" and the 500 kbit/s rate.

MIT data may be shipped in this GPL-3.0-or-later repository; keep its copyright
line in your citations (it is in every `sources` entry of the pack).

## 3. Scaffold the pack from the template

```sh
cp -r packs/_template packs/community-drivetrain
cd packs/community-drivetrain
rm -r loggers modules services      # the kinds this pack does not carry
mkdir controllers
```

The folder name is the pack id: lowercase letters, digits and `-`. Then edit
`pack.json`; the finished one is
[`packs/community-drivetrain/pack.json`](../packs/community-drivetrain/pack.json).
The fields the gate checks:

```json
{
  "id": "community-drivetrain",
  "name": "Community drivetrain controllers",
  "version": "0.1.0",
  "license": "GPL-3.0-or-later",
  "sources": [
    "commaai/opendbc opendbc/dbc/generator/hyundai/hyundai_can.dbc @ 27fa1c5d8eed (MIT, Copyright (c) 2020, Comma.ai, Inc.)",
    "ISO 15765-4 (response id = request id + 8, 500 kbit/s OBD-II CAN rate)"
  ],
  "evidence": "synthetic-tested"
}
```

`maker`, `platform`, `vehicles`, `author` and `description` are free text; use
`description` to say what was tested and what was not (step 8).

## 4. Write the definitions

Each kind has its own format (see
[DEFINITION-PACKS.md](DEFINITION-PACKS.md#writing-a-pack)). Controller
definitions follow [CONTROLLER-DEFS.md](CONTROLLER-DEFS.md); copy the closest
file from [`ctrldefs/`](../ctrldefs/) (here `borgwarner-haldex-gen5.json`, a unit
with a UDS channel and CAN frames) to `controllers/<id>.json`. The file name must
equal the `id`.

**CAN signals from a DBC** are a mechanical transcription. A DBC line

```
BO_ 274 TCU12: 8 TCU
 SG_ CUR_GR : 8|4@1+ (1.0,0.0) [0.0|15.0] ""  _4WD,EMS,ESC
VAL_ 274 CUR_GR 1 "D" 2 "D" ... 14 "R" 0 "P";
```

becomes

```json
{ "id": "0x112", "name": "TCU12", "sender": "TCU", "dlc": 8, "min_len": 8,
  "validation": "synthetic-tested", "notes": "TCU status 2 (hyundai_can.dbc).",
  "signals": [
    { "name": "CUR_GR", "start": 8, "len": 4, "kind": "enum",
      "values": [{"raw": 0, "name": "P"}, {"raw": 3, "name": "D3"}, {"raw": 14, "name": "R"}],
      "notes": "Current gear (opendbc's value table names 1-8 'D')." } ] }
```

| DBC | Controller definition |
|---|---|
| `BO_ 274 TCU12: 8 TCU` | `"id": "0x112"` (274 in hex), `"name"`, `"dlc": 8`, `"min_len": 8`, `"sender"` |
| `start\|len` | `"start"`, `"len"` (same numbers, DBC numbering) |
| `@1` / `@0` | `"byte_order": "little"` (the default) / `"big"` |
| `+` / `-` | unsigned (default) / `"signed": true` |
| `(factor,offset)` | `"factor"`, `"offset"` (leave out 1 and 0) |
| `"unit"` | `"unit"` (normalise: `deg` for a temperature becomes `degC`) |
| 1-bit signal | `"kind": "bool"` |
| `VAL_` table | `"kind": "enum"` and `"values"` (raws unique) |
| `[min\|max]` | only as `"raw_range"` when the source says values outside it are invalid |

The *community-drivetrain* files were transcribed with a throwaway script and
then annotated by hand. How you transcribe does not matter; that every line is
checked does (step 7 does it mechanically).

What the source does **not** say stays unsaid:

- **Meanings.** A DBC gives names and scales. Where it gives nothing more, the
  note says so ("meaning from the DBC name", "values not documented in the
  source"). Do not invent value tables.
- **Checksums and counters** are decoded as plain numbers; say they are not
  verified.
- **Diagnostics.** Declare a channel only when a source documents it
  (`hyundai-kia-4wd` has none). A channel nobody has tried is
  `"validation": "implemented"`.
- **Identification keys.** `identify` needs part numbers, software numbers or
  gearbox codes from a source. opendbc has none for these units, so `identify`
  stays empty and the validator warns `identify.empty`. A warning is acceptable;
  a guessed key that matches the wrong unit is not.
- **Rates.** opendbc does not state bus rates. The 500 kbit/s in the files is
  ISO 15765-4's OBD-II rate, and the bus `notes` say it is assumed.

Every definition also needs `safety` notes (what OpenTune does and does not do
with the unit) and its own `sources` list: `{title, url, license, used_for}`,
one per source, with the commit in the URL.

## 5. Validate

```sh
opentune packs validate packs/community-drivetrain --warnings
opentune controller validate packs/community-drivetrain/controllers --warnings
```

```
pack community-drivetrain (packs/community-drivetrain): OK (0 warning(s))
...
3 definition(s): 0 error(s), 3 warning(s)      # identify.empty, see step 4
```

`packs validate` is the CI gate. It checks the manifest (id = folder name,
version, licence, sources, evidence), that every file parses and passes its
engine's validation (a file that fails is skipped and reported, never silently
dropped), and runs the privacy scan on the pack. `controller validate` adds the
controller rules' warnings (see the table in
[CONTROLLER-DEFS.md](CONTROLLER-DEFS.md#validation-rules)). Both must report
0 errors.

## 6. Privacy scan

```sh
opentune privacy-scan packs/community-drivetrain
```

```
privacy-scan: 5 file(s) scanned, 0 non-text skipped, 0 finding(s).
```

It looks for VIN-like identifiers, e-mail addresses outside `example.*` /
`.test`, and labelled serial-number-like values, and prints findings redacted.
Use obvious placeholders (`WVWZZZ00000000000`, `name@example.com`). For
deliberate synthetic values the heuristics flag, list them in a file and pass
`--allow <file>`. CI also scans every data file a pull request changes.

## 7. Test it on synthetic data

A definition is `synthetic-tested` only when a test decodes it. The test must
build its frames **from the source, not from your JSON**, or it will repeat a
transcription mistake instead of catching it.
[`crates/opentune-packs/tests/community_drivetrain.rs`](../crates/opentune-packs/tests/community_drivetrain.rs)
packs each payload straight from the DBC start bits:

```rust
/// Build a little-endian (DBC `@1`) payload from `(start, len, raw)` triples.
fn intel(fields: &[(u32, u32, u64)]) -> [u8; 8] { /* raw << start, no overlap */ }

// TCU12: torque limit 200 Nm (100 x 2), gear D3 (CUR_GR 8|4 = 3)
let f = dec.decode(0x112, false, &intel(&[(0, 8, 100), (8, 4, 3)])).unwrap();
approx(&f, "ETL_TCU", 200.0);
assert_eq!(enum_name(&f, "CUR_GR").as_deref(), Some("D3"));
```

It also loads the pack and requires a clean load report, checks the ids are not
already in `ctrldefs/`, and lists every shipped frame id, so a frame added
without a test fails the build. Copy the file, rename it after your pack and
replace the frames. Run it with:

```sh
cargo test -p opentune-packs
```

(the `repo_packs` tests there also load and validate every pack under `packs/`).
To see a definition work end to end, decode a synthetic log with the CLI. These
five candump lines are a Subaru shifting P, R, N, D and one wheel-speed frame at
about 50 km/h:

```
(0.000000) can0 048#0000000400000000
(0.500000) can0 048#0001000300000000
(1.000000) can0 048#0002000200000000
(1.500000) can0 048#0003007900000000
(2.000000) can0 13A#00D036DA46DB681B
```

```sh
opentune controller decode packs/community-drivetrain/controllers/subaru-global-tcu.json synth.log
```

```
subaru-global-tcu (synthetic-tested), set global: 5 frame(s), 5 decoded

Live channels:
  gear                       D x1, N x1, P x1, R x1  last D
  wheel speed FL             49.989 .. 49.989 km/h  last 49.989
  ...
```

## 8. Set the evidence level honestly

`evidence` in `pack.json` (and `validation` in each controller definition) is
the state the data **as a whole** has reached ([DEVELOPMENT.md](DEVELOPMENT.md)
§7). If parts differ, take the lower one and say what was tested in
`description`.

| Level | What it would take for *community-drivetrain* |
|---|---|
| `implemented` | The files are written and validate. |
| `synthetic-tested` | **Reached**: every frame is decoded from synthetic payloads (step 7). |
| `real-image-tested` | Decode a real capture from a car and check the values against the dash or a scan tool. |
| `bench-tested` / `vehicle-tested` | Query the UDS channel on a real unit / in a car. |

The UDS channels stay `implemented` inside the files, because no test talks to
them; the pack `description` says so.

## 9. Document it

- **`packs/<id>/README.md`** (recommended): contents, provenance with the
  licence and copyright notices, what is and is not claimed. See
  [`packs/community-drivetrain/README.md`](../packs/community-drivetrain/README.md).
- **A row in [`packs/README.md`](../packs/README.md)**: contents, licence,
  sources.
- **`CHANGELOG.md`**: one line under `## [Unreleased]` / `### Added`.

## 10. Run the gate, commit, open the pull request

Run what CI runs: `scripts/check.sh` (all gates), or at least

```sh
opentune packs validate packs
opentune controller validate ctrldefs
opentune privacy-scan ecudefs packs ctrldefs
cargo test -p opentune-packs
```

Commit with a sign-off (`git commit -s`, see
[CONTRIBUTING.md](../CONTRIBUTING.md#commit-messages-and-sign-off-dco)) and open
a pull request. The template asks you to tick:

- *Type of change*: "New or updated definition pack";
- *Provenance and privacy*: every fact cited with its licence, nothing from
  commercial / OEM tools or decompiled software, no secrets, no personal data,
  the evidence level is what you tested;
- *Definition-pack checklist*: `packs validate` 0 errors, the controller /
  logger checks, a synthetic test, a row in `packs/README.md`.

Paste the `packs validate` and `privacy-scan` summaries into the description. If
you cannot open a pull request, or want to discuss the pack first, use the
*Definition pack submission* issue form instead.

## 11. After merge: raise the evidence

The most useful follow-up is real evidence. Log the bus of a car the pack
covers with a listen-only adapter, run
`opentune controller decode <file> capture.log --json`, compare the values with
the dashboard or a scan tool, and open a pull request that raises the frame's
`validation` and cites the capture as "own capture". The capture itself, which
can contain identifiers of your car, stays out of the repository.

## Checklist

- [ ] Scope checked against `opentune controller list` / `opentune packs packs`
- [ ] Every non-standard fact has an open, GPL-compatible source pinned to a commit
- [ ] `packs/<id>/` from `_template`, `id` = folder name, `version`, `license`, `sources`, `evidence`
- [ ] Unknowns written as unknown; no guessed meanings, keys or channels
- [ ] `opentune packs validate packs/<id>`: 0 errors
- [ ] Kind-specific validator clean (e.g. `opentune controller validate packs/<id>/controllers`)
- [ ] `opentune privacy-scan packs/<id>`: 0 findings
- [ ] A test that decodes / runs the definitions on synthetic data built from the source
- [ ] Evidence level = what the tests show, no more
- [ ] `packs/<id>/README.md`, a row in `packs/README.md`, a CHANGELOG line
- [ ] `scripts/check.sh` green, commits signed off, PR template ticked
