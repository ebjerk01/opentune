<!--
Thanks for contributing to OpenTune. Fill in the summary, then tick the checklists
that apply (delete the ones that do not). Data PRs (ecudefs/, packs/, ctrldefs/)
must clear the provenance, honesty and scope bar below - CI runs
`opentune ecudefs validate ecudefs --require-provenance`, `opentune packs validate packs`,
`opentune controller validate ctrldefs` and `opentune privacy-scan` and fails on
any error. See CONTRIBUTING.md.
-->

## Summary

<!-- What does this PR do and why? -->

## Type of change

- [ ] Bug fix
- [ ] New feature / code change
- [ ] New or updated ECU definition / map library entry (`ecudefs/*.json`)
- [ ] New or updated definition pack (`packs/<id>/`)
- [ ] New or updated controller definition (`ctrldefs/*.json`)
- [ ] Docs only

## General checklist

- [ ] `scripts/check.sh` passes (or: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`)
- [ ] Every commit is signed off (`git commit -s`, DCO - see `CONTRIBUTING.md`)

## Provenance and privacy (every data PR, and code that embeds data)

- [ ] Every non-standard fact has a cited source with its licence (an open, GPL-3-compatible project, a standard, or my own read/capture), recorded in `provenance.sources` (ECU definitions) or `sources` in `pack.json`
- [ ] Nothing is copied from commercial / OEM tools (map packs, definitions/XDFs without an open licence, DAMOS/A2L, label or coding files) or from decompiled software
- [ ] No seed/key algorithms, keys or other security secrets
- [ ] No VINs, plates, serial numbers, e-mail addresses or other personal data, and no ECU images (`opentune privacy-scan <files>` is clean)
- [ ] The `evidence` level is what I actually tested (`implemented` / `synthetic-tested` / `real-image-tested` / `bench-tested` / `vehicle-tested`), not more

## ECU-definition / map checklist (only if you touched `ecudefs/`)

Format & review (see `docs/CONTRIBUTING-ECUDEFS.md`, `docs/ECU-DEFS.md`, `docs/MAP-DEFS.md`):

- [ ] `opentune ecudefs validate ecudefs --require-provenance` reports **0 errors** for my file(s)
- [ ] Filename is `<maker>-<family>[-<variant>].json` and `id` matches the filename stem
- [ ] `identify` keys are the shortest prefixes still specific to the family (no empty/duplicate keys)
- [ ] Each map has a **confidence label** in `notes` - `CONFIDENCE: HIGH|MEDIUM|LOW` (with source) or `EXAMPLE ONLY` - and placeholder addresses say so
- [ ] Each map's `unit` is set, axis `count`s match the grid, and stored axes are monotonic
- [ ] If I set a `checksum` id, it **round-trips** against my own untouched stock dump (`opentune checksum verify --algo <id> stock.bin` -> all OK), and I validated map addresses with `opentune ecudefs validate <file> --image stock.bin` where possible
- [ ] No value is presented as fact unless I have seen it **decode sanely** on a real image
- [ ] If I changed `ecudefs/`, I ran `android/sync-ecudefs.sh` so the Android copy matches

## Definition-pack checklist (only if you touched `packs/`)

- [ ] `opentune packs validate packs/<id>` reports **0 errors** (`id` = folder name, `version`, `license`, `sources`, `evidence` set; every file loads)
- [ ] Logger conversions: `expr` and `fit` describe the same line (`cargo test -p opentune-packs`)
- [ ] Controller definitions: `opentune controller validate packs/<id>/controllers --warnings` reports 0 errors
- [ ] `synthetic-tested` or higher is backed by a test that runs the definitions on synthetic data built from the source (see `docs/PACK-AUTHORING.md`)
- [ ] A row for the pack is added / updated in `packs/README.md`

## Scope (`README.md` "what OpenTune will NOT do" / `CONTRIBUTING.md`)

- [ ] This change is **calibration / diagnostics only** - it does **not** add or enable immobiliser removal/defeat, odometer/mileage correction, or emissions-control defeat (DPF/EGR/AdBlue/SCR/lambda "off")
