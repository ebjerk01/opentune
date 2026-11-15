# Contributing to OpenTune

Thanks for your interest. OpenTune is an open-source ECU reading, flashing and
calibration toolkit built around a shared Rust core. Contributions of code,
documentation, ECU definitions and hardware designs are all welcome.

## Building

OpenTune is a Cargo workspace. You need a recent stable Rust toolchain (see
[`rust-toolchain.toml`](rust-toolchain.toml)).

```sh
cargo build --workspace
cargo test  --workspace
cargo run -p opentune-cli -- --help
```

The `mock`/file driver operates on local `.bin` files, so the whole
identify → read → diff → checksum → flash pipeline can be developed and tested
with no hardware attached.

Before opening a pull request, make sure the same checks CI runs pass locally:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Scope boundaries

OpenTune is **calibration-focused** and deliberately stays a legitimate project.
Contributions must keep it that way. We will **not** accept features for:

- immobiliser removal or defeat;
- odometer / mileage manipulation;
- emissions-control defeat (DPF, EGR, AdBlue/SCR "off", lambda/O2 defeat, and
  similar).

Also do not add to this repository:

- manufacturer firmware, calibration files, security-access seed/key secrets, or
  any other proprietary or copyrighted ECU data;
- real vehicle identifiers (VINs), registration plates, or other personal data.

ECU definitions in [`ecudefs/`](ecudefs/) must be built from **public** ECU
facts only (family, protocol, generic memory map) and should carry a `notes`
field stating that addresses need per-variant confirmation.

## Coding standards

- Format with `rustfmt` (`cargo fmt`); keep the default style.
- Keep `cargo clippy --all-targets` clean; the CI denies warnings.
- Prefer small, well-documented modules. Public items get a doc comment.
- Errors use the shared `opentune_core::Error`/`Result` types; avoid `unwrap()`
  outside tests and examples.
- Safety rules (voltage gate, verified backup, read-back verify) live in
  `opentune-core`/`opentune-flash`, never in individual drivers. A driver must
  not be able to skip them.
- New interface drivers implement the `Interface` trait and, where they speak
  CAN, reuse the shared `IsoTp` + `UdsClient` stack. See
  [`docs/DRIVERS.md`](docs/DRIVERS.md).
- Add tests for new behaviour. Offline-testable logic (parsing, checksums,
  framing, ECU-def loading) should ship with unit or integration tests.

## Commit messages and sign-off (DCO)

This project uses the **Developer Certificate of Origin** (DCO). By signing off a
commit you certify that you wrote the change, or otherwise have the right to
submit it under the project's licence (see [the DCO text](https://developercertificate.org/)).

Sign off every commit by adding a `Signed-off-by` line, which `git` adds for you
with `-s`:

```sh
git commit -s -m "drivers: add raw-CAN echo test"
```

This appends, using your real name and email:

```
Signed-off-by: Jane Developer <jane@example.com>
```

Write clear, imperative commit subjects (e.g. "core: fix ISO-TP flow-control
timeout"). Group related changes; keep unrelated changes in separate commits.

## Licences

- Software and device firmware: **GPL-3.0-or-later**.
- Hardware design files: **CERN-OHL-S-2.0**.

By contributing you agree your contribution is licensed under the licence that
applies to the part of the repository you are changing.
