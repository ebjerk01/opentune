# OpenTune — notes for Claude

GPL-3.0 Rust car diagnostics / coding / tuning suite. A **local** app (no web), built
as data-driven "service" engines over a shared protocol layer. Cargo workspace.

## Commands

```bash
cargo build --workspace
cargo test  --workspace
cargo run -p opentune-cli -- --help
# desktop app builds STANDALONE (not a workspace member): see desktop/
cd desktop && cargo tauri build --features alpha --bundles appimage
```

Before committing, run the same gates CI runs, in CI's order (CI stops at the first
failure, so a fmt diff hides everything after it):

```bash
cargo fmt --all                                            # CI: cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Agent worktrees: run `cargo fmt --all` *before* merging back — unformatted merges kept
`main` CI red for a whole day (2026-11-17).

## Layout

- `crates/opentune-core` — the `Interface`/`DiagTransport` traits, UDS/KWP/TP2.0/ISO-TP
  protocols, `RawDiag` + `CoreDiag` (the one request→response seam every engine runs on),
  ECU defs, the flash safety contract.
- `crates/drivers/*` — interface drivers (mock, socketcan, slcan, gsusb, j2534,
  openport2, kkl, the ELM327 variants). Each implements `Interface`.
- Service engines: `opentune-coding`, `-adaptation`, `-autoscan`, `-servicefn`,
  `-logdef` — data-driven, run over `RawDiag`. `opentune-packs` loads definition packs.
- `opentune-checksum`, `-container` (FRF/ODX/SGO/VBF), `-flash`, `-cbf`, def importers
  (`-xdf`/`-a2l`/`-ecuflash`/`-ols`), `opentune-udssim` (conformance server).
- `desktop/` — Tauri 2 app. `src-tauri/` (Rust), `ui/` (plain JS, no bundler). The
  `alpha` cargo feature gates the Debug + live-engine panels.

## Gotchas (learned the hard way)

- **A driver MUST override `Interface::diag()`** to return its `DiagTransport`. The core
  default returns `None`, and then *every* engine (coding/adaptation/autoscan/service/log)
  and the raw-request console silently fails with "no diagnostic transport" — even though
  direct inherent-method paths (e.g. OBD live reads) still work. This bit elm327obd.
- `RawDiag`/`CoreDiag` live in `opentune-core` and are re-exported by each engine, so
  **one** `CoreDiag` adapter drives them all. Don't re-declare `RawDiag` per crate.
- **Tauri v2 runs sync commands on the UI thread** → a blocking serial read freezes the
  GUI (GNOME "not responding"). Make serial-I/O commands `async fn` (and use
  `tauri::State<'_, T>` lifetimes); also guard polling loops against overlapping ticks.
- The `desktop/` crate is **not** in the workspace (opts out with its own `[workspace]`),
  so build it from `desktop/`, and pass `--features alpha` to get the engine panels.
- Test engines against `opentune-udssim` (`SimTransport` + `CoreDiag`), not just
  hand-rolled fakes — see `crates/opentune-coding/tests/sim.rs` for the pattern.

## Clean-room / scope

OpenTune ships **no OEM seed/key algorithms and no protected calibration data** — it
takes them as caller-supplied inputs or leaves them out of scope, so the project stays
open and hostable. The checksum/coding engines **refuse rather than guess** when a result
isn't validated. Do not add vendor-derived keys, protected tables, or RE'd proprietary
data to this repo. `.gitignore` already blocks `*.bin`/`*.exe`/`vendor/`.
