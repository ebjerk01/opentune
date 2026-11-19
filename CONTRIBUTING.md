# Contributing to OpenTune

Thanks for your interest. OpenTune is an open-source car diagnostics, coding and
tuning suite built around a shared Rust core. Contributions of code,
documentation, ECU definitions, definition packs and hardware designs are all
welcome. Please read the [code of conduct](CODE_OF_CONDUCT.md) first.

| You want to… | Read |
|---|---|
| add or fix an ECU definition / map library entry | [`docs/CONTRIBUTING-ECUDEFS.md`](docs/CONTRIBUTING-ECUDEFS.md), [`docs/ECU-DEFS.md`](docs/ECU-DEFS.md), [`docs/MAP-DEFS.md`](docs/MAP-DEFS.md) |
| write and submit your first definition pack, step by step | [`docs/PACK-AUTHORING.md`](docs/PACK-AUTHORING.md) (worked example: [`packs/community-drivetrain/`](packs/community-drivetrain/)) |
| add a definition pack (coding, adaptation, service scripts, loggers, module lists, controllers) | [`docs/DEFINITION-PACKS.md`](docs/DEFINITION-PACKS.md), [`packs/README.md`](packs/README.md) |
| add a gearbox / AWD controller definition | [`docs/CONTROLLER-DEFS.md`](docs/CONTROLLER-DEFS.md) |
| add or fix a control unit in the ECU database (pinout, access profile: OBD / bench / boot / security) | [`docs/DATABASE.md`](docs/DATABASE.md), [`docs/UNIT-ACCESS-PROFILE.md`](docs/UNIT-ACCESS-PROFILE.md) |
| change code | [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md), [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md), [`docs/DRIVERS.md`](docs/DRIVERS.md) |
| request support for an ECU or report a bug | open an issue from the templates |

## Code or data?

**Data** is how most coverage grows: ECU definitions in [`ecudefs/`](ecudefs/),
definition packs in [`packs/`](packs/), controller definitions in
[`ctrldefs/`](ctrldefs/). It needs no Rust knowledge: copy the template, fill it
in, run the validators below, open a pull request. **Code** changes (engines,
drivers, apps) follow the working agreement in `docs/DEVELOPMENT.md` and must
pass every gate (`scripts/check.sh`).

## Contributing data

1. Fork the repository and start from the template: `ecudefs/TEMPLATE.json`,
   `packs/_template/` or an existing controller definition. For a pack,
   [`docs/PACK-AUTHORING.md`](docs/PACK-AUTHORING.md) walks through every
   step below on a real example.
2. Fill in the **provenance**: `sources` (one citation per source),
   `license` (SPDX id of the data, normally `GPL-3.0-or-later`) and `evidence`
   (the validation state you actually reached, see below). It goes in a
   `provenance` block of an ECU definition, and at the top level of `pack.json`.
3. Validate on your fork exactly as CI does (with a released `opentune` binary,
   or `cargo run -p opentune-cli --` in place of `opentune`):

   ```sh
   opentune ecudefs validate ecudefs --require-provenance   # ECU definitions
   opentune packs validate packs                            # definition packs
   opentune controller validate ctrldefs                    # controller definitions
   opentune db validate --require-access                    # ECU database units
   opentune privacy-scan ecudefs packs ctrldefs database    # VINs, e-mails, serials
   ```

   A new ECU database unit must carry an access profile; see
   [`docs/UNIT-ACCESS-PROFILE.md`](docs/UNIT-ACCESS-PROFILE.md) for the schema
   and which sources are allowed (public datasheets, pinout sites, forums and
   open projects; never commercial-tool internals, leaked manuals or
   immobiliser procedures).

4. Add a test that exercises the data on synthetic input (built from the
   source, not from your file) if you claim `synthetic-tested` or higher.
5. Open a pull request and tick the provenance checklist in the template.
   CI repeats these checks and also privacy-scans every data file the PR
   changes. Findings are printed redacted, so a mistake is not repeated in a
   public log; fix it with a new commit and, if a real identifier was pushed,
   say so in the PR so the maintainer can clean the history.

**Evidence levels** are the project's validation-state vocabulary, lowest to
highest: `implemented` (written, not exercised), `synthetic-tested` (tests on
generated data or a simulator), `real-image-tested` (checked against real dumps
or captures, offline), `bench-tested` (real hardware on a bench),
`vehicle-tested` (end-to-end in a car). Claim the state the data as a whole has
reached, never more; per-map confidence goes in each map's `CONFIDENCE:` label.

## Provenance and clean-room rules

These apply to code and data alike. A contribution that breaks them is closed,
however useful it is.

- **No OEM secrets.** No security-access / seed-key algorithms or keys, no
  immobiliser or component-protection secrets, no OEM scripts or bytecode.
- **No copied commercial or OEM material.** No firmware or calibration dumps,
  DAMOS/A2L files, map packs, or definitions/XDFs taken from commercial tools.
  A third-party definition may be imported only if it carries an open licence
  compatible with GPL-3.0 — cite the project, file, commit and licence (see
  [`docs/BMW-MS4X-IMPORT.md`](docs/BMW-MS4X-IMPORT.md) for a worked example).
- **No reverse-engineered proprietary code.** Do not copy or translate
  decompiled code. Algorithms are derived from your own data (clean-room) or
  from open sources cited in [`docs/PRIOR-ART.md`](docs/PRIOR-ART.md).
- **No personal data.** No VINs, registration plates, ECU or vehicle serial
  numbers, names, e-mail addresses, locations or anyone's ECU images. Use
  obvious placeholders (`WVWZZZ00000000000`, `name@example.com`).
- **Cite your sources and their licences**, and keep your own data (reads,
  captures) out of the repository: cite it as "own read" / "own capture".
- **Be honest about evidence.** Never present a guess as a fact; label
  placeholders as placeholders.

The validators catch part of this mechanically (missing or non-open licences,
commercial-tool sources, unknown evidence levels, VIN-/e-mail-/serial-like
strings); review covers the rest.

## Scope boundaries

OpenTune is a **diagnostics, coding and tuning** suite and deliberately stays a
legitimate project. We will **not** accept features or data for:

- immobiliser removal or defeat;
- odometer / mileage manipulation;
- emissions-control defeat (DPF, EGR, AdBlue/SCR "off", lambda/O2 defeat, and
  similar).

## Building and checks

OpenTune is a Cargo workspace; you need a recent stable Rust toolchain (see
[`rust-toolchain.toml`](rust-toolchain.toml)).

```sh
cargo build --workspace
cargo test  --workspace
cargo run -p opentune-cli -- --help
scripts/check.sh          # every CI gate, in CI order (--fast = fmt + clippy)
scripts/setup-hooks.sh    # once per clone: fast pre-commit hook (fmt + JS syntax)
```

The `mock`/file driver operates on local `.bin` files, so the whole
identify → read → diff → checksum → flash pipeline can be developed and tested
with no hardware attached.

## Coding standards

- Format with `rustfmt` (`cargo fmt`); keep `cargo clippy --all-targets` clean
  (CI denies warnings).
- Prefer small, well-documented modules. Public items get a doc comment.
- Errors use the shared `opentune_core::Error`/`Result` types; avoid `unwrap()`
  outside tests and examples.
- Safety rules (voltage gate, verified backup, read-back verify) live in
  `opentune-core`/`opentune-flash`, never in individual drivers. A driver must
  not be able to skip them.
- New interface drivers implement the `Interface` trait and, where they speak
  CAN, reuse the shared `IsoTp` + `UdsClient` stack. A driver that owns a
  `DiagTransport` **must override `Interface::diag()`** — see
  [`docs/DRIVERS.md`](docs/DRIVERS.md).
- Add tests for new behaviour. Offline-testable logic (parsing, checksums,
  framing, definition loading) ships with unit or integration tests.
- Features that talk to a car stay in the Bench tier until vehicle-tested
  (`docs/DEVELOPMENT.md`).

## Commit messages and sign-off (DCO)

This project uses the **Developer Certificate of Origin** (DCO). By signing off a
commit you certify that you wrote the change, or otherwise have the right to
submit it under the project's licence (see [the DCO text](https://developercertificate.org/)).
For data this includes the right to contribute every fact you cite.

Sign off every commit with `git commit -s`, which appends, using your name and
email:

```
Signed-off-by: Jane Developer <jane@example.com>
```

Write clear, imperative commit subjects (e.g. "core: fix ISO-TP flow-control
timeout"). Keep unrelated changes in separate commits.

## Licences

- Software, device firmware and definition data: **GPL-3.0-or-later** (unless a
  definition's `provenance.license` names a stricter compatible licence of the
  material it was imported from).
- Hardware design files: **CERN-OHL-S-2.0**.

By contributing you agree your contribution is licensed under the licence that
applies to the part of the repository you are changing.
