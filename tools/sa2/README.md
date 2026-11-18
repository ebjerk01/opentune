# SA2 differential testing

`sa2_differential.py` checks OpenTune's SA2 seed/key VM (`crates/opentune-sa2`)
against an independent, public implementation: **bri3d/sa2_seed_key**
([GitHub](https://github.com/bri3d/sa2_seed_key), MIT, on PyPI as
`sa2-seed-key`). OpenTune's Rust is written from the instruction-set semantics;
the Python package is only the oracle and is never shipped.

**Every program is synthetic.** The script generates its own SA2 bytecode:

- ~37 hand-written programs, one per behaviour (each opcode, carry from
  `rsl`/`rsr`/`add`/`sub`, `eor` leaving carry alone, `bcc` both ways, `bra`,
  branches past the end and into an operand, `loop 1`/`loop 255`, nesting,
  conditionals inside loops, and the error cases: unknown opcode, truncated
  operands, `next` without `loop`);
- *structured* random programs (balanced loops up to three deep, aligned
  forward branches over sub-blocks, edge-value immediates);
- *unstructured* random instruction soup (unbalanced loops, misaligned
  branches, truncation, stray bytes) where both VMs must agree on the key **or**
  on failing.

No OEM SA2 script is used, stored or emitted. Two deliberate differences from
the reference are excluded from generation and covered by Rust unit tests
instead: OpenTune stops at `finish` (`0x4C`) where the reference falls through,
and refuses `loop 0`.

## Use

```sh
python3 -m venv ~/sa2venv && ~/sa2venv/bin/pip install -r tools/sa2/requirements.txt

# Regenerate the committed fixture (deterministic for a given --rng-seed):
~/sa2venv/bin/python -I tools/sa2/sa2_differential.py generate \
    --out crates/opentune-sa2/tests/fixtures/sa2_vectors.txt

# Re-check the fixture against the reference and diff fresh random programs
# through both VMs (sa2_eval is a tiny stdin/stdout example binary):
cargo build -p opentune-sa2 --example sa2_eval
~/sa2venv/bin/python -I tools/sa2/sa2_differential.py check \
    --fixture crates/opentune-sa2/tests/fixtures/sa2_vectors.txt \
    --eval target/debug/examples/sa2_eval --fresh 2000
```

`cargo test -p opentune-sa2` checks the committed fixture without Python.
`scripts/check.sh` runs the full differential check as an extra gate when
`OPENTUNE_SA2_PYTHON` names an interpreter that has `sa2-seed-key` installed
(`OPENTUNE_SA2_FRESH` sets the number of fresh programs, default 2000).

The reference keeps its loop stacks as class attributes, so the harness gives
every run fresh ones; it also counts reference steps and drops any vector over
200 000 steps (none of the default set comes close).
