<!--
Thanks for contributing to OpenTune. Fill in the summary, then tick the checklist
that applies. PRs that add or change an ECU definition / map library entry
(ecudefs/*.json) must clear the honesty and scope bar below — CI runs
`opentune ecudefs validate ecudefs` and fails on any validation error.
-->

## Summary

<!-- What does this PR do and why? -->

## Type of change

- [ ] Bug fix
- [ ] New feature / code change
- [ ] New or updated ECU definition / map library entry (`ecudefs/*.json`)
- [ ] Docs only

## General checklist

- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] Every commit is signed off (`git commit -s`, DCO — see `CONTRIBUTING.md`)

## ECU-definition / map checklist (only if you touched `ecudefs/`)

Format & review (see `docs/CONTRIBUTING-ECUDEFS.md`, `docs/ECU-DEFS.md`, `docs/MAP-DEFS.md`):

- [ ] `opentune ecudefs validate ecudefs` reports **0 errors** for my file(s)
- [ ] Filename is `<maker>-<family>[-<variant>].json` and `id` matches the filename stem
- [ ] `identify` keys are the shortest prefixes still specific to the family (no empty/duplicate keys)
- [ ] Each map has a **confidence label** in `notes` — `CONFIDENCE: HIGH|MEDIUM|LOW` (with source) or `EXAMPLE ONLY` — and placeholder addresses say so
- [ ] Each map's `unit` is set, axis `count`s match the grid, and stored axes are monotonic
- [ ] If I set a `checksum` id, it **round-trips** against my own untouched stock dump (`opentune checksum verify --algo <id> stock.bin` → all OK), and I validated map addresses with `opentune ecudefs validate <file> --image stock.bin` where possible

Honesty & data (the OpenTune credibility bar):

- [ ] **Public + personal data only** — no manufacturer firmware, A2L/DAMOS, seed/key secrets, real VINs/plates or other proprietary content; built from public family facts and my **own** ECU reads
- [ ] No value is presented as fact unless I have seen it **decode sanely** on a real image

Scope (`README.md` "what OpenTune will NOT do" / `CONTRIBUTING.md`):

- [ ] This change is **calibration only** — it does **not** add or enable immobiliser removal/defeat, odometer/mileage correction, or emissions-control defeat (DPF/EGR/AdBlue/SCR/lambda "off")
