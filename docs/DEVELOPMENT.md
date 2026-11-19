# Development workflow

The working agreement for everyone changing OpenTune — human contributors and
coding agents alike. It exists because of real failures: unformatted code merged
to `main` kept CI red for a day, the desktop crate (outside the workspace)
piled up clippy errors CI never ran, and dozens of doc claims drifted from the
code. Each rule below closes one of those holes.

## 1. One command runs every gate

```sh
scripts/check.sh               # every gate CI runs, in CI order
scripts/check.sh --fast        # fmt + clippy only (workspace + desktop)
scripts/check.sh --no-desktop  # workspace + data gates only
```

It stops at the first failure and names the failing gate. Order:

1. workspace `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace` (includes the doc-drift tests, §5)
4. `opentune ecudefs validate ecudefs`
5. `opentune db validate --require-access`
   (then the `opentune-udssim` conformance harness against python-udsoncan /
   python-doipclient, [`tools/conformance/`](../tools/conformance/README.md) —
   run when `OPENTUNE_CONFORMANCE_PYTHON` or `python3` can import both
   packages, skipped with a notice otherwise)
6. `desktop/src-tauri`: `cargo fmt -- --check`
7. desktop clippy, default features and `--features alpha`, `-D warnings`
8. desktop `cargo test --features alpha`
9. `node --check` on every `desktop/ui/src/*.js` and `desktop/preview/*.js`
10. `node --test` on `desktop/ui/tests/` when that directory exists

CI runs the same gates as five jobs in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml):
`check` (1–3), `ecudefs` (4–5), `conformance` (the udsoncan harness),
`desktop` (6–8, on ubuntu-22.04) and `ui` (9–10).
If you add a gate, add it to both places.

Enable the tracked pre-commit hook once per clone — it catches the cheap things
(formatting of both crates, syntax of staged UI JavaScript) in seconds:

```sh
scripts/setup-hooks.sh         # git config core.hooksPath .githooks
```

## 2. Integration branch per batch

Work lands in batches. Each batch has an integration branch, `integrate/<name>`,
cut from `main`:

- Feature branches (one per task or agent) branch from the integration branch
  and merge back into it, not into `main`.
- After each merge, run `scripts/check.sh` **on the integration branch**. A merge
  that turns a gate red is fixed or reverted before the next one goes in.
- `main` only moves by **fast-forwarding** to a green integration branch. It is
  never the place where the merges happen.
- **Never push with a red gate** — not to `main`, not to an integration branch
  someone else is merging into. `--no-verify` is for emergencies, and CI will
  still catch it.
- Parallel workers use separate worktrees and touch distinct files where they
  can. Run `cargo fmt --all` (and `cargo fmt` in `desktop/src-tauri`) before
  handing a branch back.

## 3. Commit hygiene

- Imperative subject with a scope prefix (`core: …`, `desktop: …`, `docs: …`,
  `ci: …`), body explaining *why* when it isn't obvious.
- One logical change per commit; keep unrelated changes in separate commits.
  Formatting-only and refactor-only commits stay separate from behaviour changes.
- Every commit builds and passes at least `scripts/check.sh --fast`.
- Sign off (DCO) as described in [`CONTRIBUTING.md`](../CONTRIBUTING.md).
- Never commit build output, dumps, vendor binaries or secrets (`.gitignore`
  blocks the common ones — don't force-add around it).

## 4. Build output lives outside the repo

Set `CARGO_TARGET_DIR` to a directory **outside** the checkout, e.g.

```sh
export CARGO_TARGET_DIR=~/build/opentune
```

This keeps worktrees small, stops editors and indexers from crawling gigabytes
of artefacts, and lets several worktrees share nothing by accident (give each
worktree its own directory). The desktop crate is not a workspace member and
builds separately; `scripts/check.sh` puts it in `$CARGO_TARGET_DIR-desktop`
(or `$DESKTOP_TARGET_DIR` if you set that).

### Building on another machine

The gates are CPU-heavy. If your editing machine is small or shared, run them
on a faster build host instead: sync the worktree there (excluding `target/`,
`.git` and `node_modules/`), set a persistent `CARGO_TARGET_DIR` on that host
so rebuilds stay incremental, and run `scripts/check.sh` from the synced copy.
The script needs no git metadata and only reads `CARGO_TARGET_DIR` /
`DESKTOP_TARGET_DIR` from the environment, so a small `rsync` + `ssh` wrapper
is enough. `cargo fmt` and git work are cheap and can stay local.

## 5. Docs stay in step with the code

`crates/opentune-cli/tests/doc_drift.rs` runs with `cargo test --workspace` and
fails when:

- a top-level CLI subcommand is missing from the command table in
  [`ARCHITECTURE.md`](ARCHITECTURE.md) (or `README.md`);
- a driver id registered in the CLI is missing from [`DRIVERS.md`](DRIVERS.md);
- a checksum algorithm id is missing from the valid-id list in
  [`CONTRIBUTING-ECUDEFS.md`](CONTRIBUTING-ECUDEFS.md);
- a workspace crate is missing from [`ARCHITECTURE.md`](ARCHITECTURE.md).

Fix a failure by documenting the new item, not by weakening the test. Beyond
these checks: a claim in the docs about what works must match the validation
state in §7.

## 6. Clean-room rules (summary)

OpenTune is GPL-3.0 and must stay publishable. In short:

- **No OEM seed/key algorithms, protected calibration data, manufacturer
  firmware or vendor binaries** in the repo. Such inputs are caller-supplied at
  runtime or out of scope.
- **No code or tables derived from decompiling proprietary software.** Public
  facts (protocol standards, published documentation, interoperability
  behaviour observed on your own hardware) are fine; describe the source.
- Other open-source projects: respect their licence. GPL-2.0-only projects are
  learn-only. Credit what informed the design in [`PRIOR-ART.md`](PRIOR-ART.md).
- Engines **refuse rather than guess**: a checksum, coding or flash path that
  has not been validated reports that, and never writes a speculative result.
- No real VINs, plates or personal data in fixtures; synthetic samples only.
- Scope limits (no immobiliser, odometer or emissions-defeat features) are in
  [`CONTRIBUTING.md`](../CONTRIBUTING.md).

## 7. Validation-state vocabulary

Every feature, driver and algorithm is described with exactly one of these
states, in increasing order of evidence. Use the words as written in docs,
commit messages and release notes; never claim a higher state than you have
evidence for.

| State | Meaning |
|---|---|
| **example** | Data only: placeholder addresses/maps kept as a worked example. Never used to identify a real unit; shown as "Example — placeholder data". (Definitions and packs only.) |
| **implemented** | Code exists and compiles; behaviour not yet tested. |
| **synthetic-tested** | Unit/integration tests pass on synthetic data (generated images, the UDS simulator, canned frames). |
| **real-image-tested** | Verified against real dumps / definition files (offline). |
| **bench-tested** | Exercised against real hardware on the bench (adapter + ECU on a power supply). |
| **vehicle-tested** | Exercised end-to-end on a real vehicle. |

### Sidebar tier rule (desktop)

Desktop panels register with a tier (`desktop/ui/src/nav.js`):

- `"public"` — shown in every build.
- `"bench"` — shown only in alpha/bench builds, grouped under *Bench*.

**Anything that talks to a car stays tier `"bench"` until it is vehicle-tested.**
Offline tools (working on files only) may be `"public"` once real-image-tested.
Promoting a panel is a one-line tier change in its own commit that names the
evidence.

## 8. UI preview without Tauri

The desktop UI can be previewed in a plain browser with demo data — useful for
reviewing UI changes without building the app:

```sh
python3 -I desktop/preview/build.py /path/outside/repo/preview.html
```

It inlines `desktop/ui` (unchanged) plus `desktop/preview/mock.js`, which stands
in for the Tauri backend, and the per-panel `desktop/preview/fixtures-*.js`.
The vehicle panels answer from the `vcar` virtual car (its demo profile and
`packs/vcar-demo`, bundled by `build.py`); the calibration panels work on a
synthetic stock / tuned image pair and the log viewer on a synthetic 60 s log.
The pack picker lists every shipped pack from `packs/` (also bundled by
`build.py`), and its **Load pack…** opens a fictional community pack with one
skipped file (`fixtures-packs.js`).
Every command a panel invokes should have a fixture — a missing one shows as
"Preview: '<cmd>' has no demo data yet", and `desktop/ui/tests/preview.test.mjs`
fails when a core command in `lib.rs` has none. Keep the fixtures in step when a
command's shape changes; their JS is syntax-checked by the `ui` gate. Demo data
must be obviously fake (placeholder VINs such as `WVWZZZ00000000000`, the
virtual car's `TESTVCAR…`), never taken from a real car or dump. Never run `cargo tauri build` as a
routine check — the gates above cover compilation; bundles are built by the
release workflow.
