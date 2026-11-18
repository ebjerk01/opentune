#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Differential test of OpenTune's SA2 VM against the public reference.

The oracle is bri3d's `sa2-seed-key` (MIT, PyPI). Every program this script
feeds it is SYNTHETIC — hand-written cases plus programs generated here from a
seeded RNG. No OEM SA2 script is used, stored or emitted.

    # regenerate the committed fixture (deterministic for a given --rng-seed)
    python3 tools/sa2/sa2_differential.py generate \
        --out crates/opentune-sa2/tests/fixtures/sa2_vectors.txt

    # re-check the fixture against the reference, then run FRESH random
    # programs through both the reference and OpenTune's VM (sa2_eval example)
    python3 tools/sa2/sa2_differential.py check \
        --fixture crates/opentune-sa2/tests/fixtures/sa2_vectors.txt \
        --eval path/to/sa2_eval --fresh 2000

See README.md next to this file.
"""
import argparse
import random
import subprocess
import sys
import time
from collections import deque

from sa2_seed_key.sa2_seed_key import Sa2SeedKey

RSL, RSR, ADD, SUB, EOR = 0x81, 0x82, 0x93, 0x84, 0x87
LOOP, NEXT, BCC, BRA, FINISH = 0x68, 0x49, 0x4A, 0x6B, 0x4C
OPCODES = [RSL, RSR, ADD, SUB, EOR, LOOP, NEXT, BCC, BRA, FINISH]
OPERANDS = {RSL: 0, RSR: 0, ADD: 4, SUB: 4, EOR: 4, LOOP: 1, NEXT: 0, BCC: 1, BRA: 1, FINISH: 0}

# Steps the reference may take before a vector is discarded (the reference has
# no budget of its own; OpenTune's default is 1_000_000).
REF_STEP_LIMIT = 200_000

EDGE_WORDS = [0x00000000, 0xFFFFFFFF, 0x80000000, 0x7FFFFFFF, 0x00000001, 0xFFFFFFFE]


class StepLimit(Exception):
    pass


def reference(script: bytes, seed: int):
    """Run the reference; return the key (int) or None if it raised."""
    vm = Sa2SeedKey(bytearray(script), seed)
    # The reference keeps its loop stacks as *class* attributes; give every
    # run its own so one run can never leak loop state into the next.
    vm.for_pointers = deque()
    vm.for_iterations = deque()
    steps = [0]

    def counted(fn):
        def wrapper():
            steps[0] += 1
            if steps[0] > REF_STEP_LIMIT:
                raise StepLimit()
            fn()

        return wrapper

    for name in ("rsl", "rsr", "add", "sub", "eor", "for_loop", "next_loop", "bcc", "bra", "finish"):
        setattr(vm, name, counted(getattr(vm, name)))
    try:
        return vm.execute() & 0xFFFFFFFF
    except StepLimit:
        raise
    except Exception:  # KeyError (unknown opcode) / IndexError (truncated, bad next)
        return None


# --- program generation -------------------------------------------------------


def imm(rng):
    if rng.random() < 0.25:
        return rng.choice(EDGE_WORDS)
    return rng.getrandbits(32)


def word(v):
    return list(v.to_bytes(4, "big"))


def gen_block(rng, depth, max_items):
    """A well-formed block: balanced loops, aligned forward branches."""
    out = []
    for _ in range(rng.randint(1, max_items)):
        k = rng.random()
        if k < 0.16:
            out.append(RSL)
        elif k < 0.32:
            out.append(RSR)
        elif k < 0.46:
            out += [ADD] + word(imm(rng))
        elif k < 0.58:
            out += [SUB] + word(imm(rng))
        elif k < 0.70:
            out += [EOR] + word(imm(rng))
        elif k < 0.84 and depth < 3:
            count = rng.randint(1, 16 if depth == 0 else 5)
            if rng.random() < 0.05 and depth == 0:
                count = rng.choice([200, 255])
            out += [LOOP, count] + gen_block(rng, depth + 1, max(1, max_items // 2)) + [NEXT]
        else:
            op = BCC if rng.random() < 0.7 else BRA
            skipped = gen_block(rng, depth + 1, 3) if rng.random() < 0.9 else []
            if len(skipped) <= 255:
                out += [op, len(skipped)] + skipped
    return out


def gen_structured(rng):
    body = gen_block(rng, 0, rng.randint(2, 10))
    if rng.random() < 0.1:  # a branch that jumps straight past the end
        body += [rng.choice([BCC, BRA]), rng.randint(0, 20)]
    return bytes(body + [FINISH])


def gen_unstructured(rng):
    """Random instruction soup: unbalanced loops, misaligned branches,
    truncation, unknown bytes. Both VMs must agree, including on errors."""
    out = []
    for _ in range(rng.randint(1, 14)):
        r = rng.random()
        if r < 0.05:
            out.append(rng.randrange(256))  # maybe-unknown byte
            continue
        op = rng.choice([o for o in OPCODES if o != FINISH])
        out.append(op)
        n = OPERANDS[op]
        if op == LOOP:
            out.append(rng.randint(1, 4))
        elif op in (BCC, BRA):
            out.append(rng.randint(0, 12))
        else:
            out += word(imm(rng))[:n]
    if rng.random() < 0.3 and out:
        out = out[: rng.randint(1, len(out))]  # maybe truncate mid-operand
    if rng.random() < 0.7:
        out.append(FINISH)
    return bytes(out)


def acceptable(script: bytes) -> bool:
    """Exclude the two documented, deliberate divergences from the reference:
    `finish` (0x4C) anywhere but the last byte (OpenTune stops there, the
    reference falls through) and a loop count of 0 (OpenTune refuses it). The
    check is per byte, so misaligned branches cannot reach either."""
    if FINISH in script[:-1]:
        return False
    for i in range(len(script) - 1):
        if script[i] == LOOP and script[i + 1] == 0:
            return False
    return True


def seeds_for(rng, n):
    out = []
    for _ in range(n):
        out.append(rng.choice(EDGE_WORDS) if rng.random() < 0.2 else rng.getrandbits(32))
    return out


# Hand-written programs, each aimed at one behaviour. All synthetic.
def a(*parts):
    out = []
    for p in parts:
        out += p if isinstance(p, list) else [p]
    return bytes(out)


HAND = [
    ("rsl", a(RSL, FINISH)),
    ("rsr", a(RSR, FINISH)),
    ("rsl x31", a(LOOP, 31, RSL, NEXT, FINISH)),
    ("rsr x33", a(LOOP, 33, RSR, NEXT, FINISH)),
    ("add", a(ADD, word(0x01020304), FINISH)),
    ("add overflow", a(ADD, word(0xFFFFFFFF), FINISH)),
    ("sub", a(SUB, word(0x01020304), FINISH)),
    ("sub borrow", a(SUB, word(0xFFFFFFFF), FINISH)),
    ("eor", a(EOR, word(0xA5A5A5A5), FINISH)),
    ("eor keeps carry", a(ADD, word(0xFFFFFFFF), EOR, word(0x11111111), BCC, 1, RSL, FINISH)),
    ("rsl carry -> bcc", a(RSL, BCC, 5, EOR, word(0xDEADBEEF), FINISH)),
    ("rsr carry -> bcc", a(RSR, BCC, 5, EOR, word(0xDEADBEEF), FINISH)),
    ("add carry -> bcc", a(ADD, word(0x80000000), BCC, 5, EOR, word(0x0F0F0F0F), FINISH)),
    ("sub borrow -> bcc", a(SUB, word(0x80000000), BCC, 5, EOR, word(0x0F0F0F0F), FINISH)),
    ("bcc +0", a(RSL, BCC, 0, RSR, FINISH)),
    ("bra over", a(BRA, 5, EOR, word(0xFFFFFFFF), RSL, FINISH)),
    ("bra +0", a(BRA, 0, RSL, FINISH)),
    ("bra past end", a(RSL, BRA, 40)),
    ("bcc past end", a(RSR, BCC, 40, RSL)),
    ("loop 1", a(LOOP, 1, RSL, NEXT, FINISH)),
    ("loop 255", a(LOOP, 255, ADD, word(0x01000193), RSL, NEXT, FINISH)),
    ("nested loops", a(LOOP, 3, RSL, LOOP, 4, ADD, word(7), NEXT, RSR, NEXT, FINISH)),
    ("triple nested", a(LOOP, 5, LOOP, 6, LOOP, 7, RSL, NEXT, EOR, word(0x13579BDF), NEXT, NEXT, FINISH)),
    ("conditional in loop", a(LOOP, 8, RSL, BCC, 5, EOR, word(0x04C11DB7), NEXT, FINISH)),
    ("skip loop body tail", a(LOOP, 4, RSR, BCC, 1, RSL, ADD, word(3), NEXT, FINISH)),
    ("branch over a loop", a(RSL, BCC, 5, LOOP, 3, RSL, RSL, NEXT, FINISH)),
    ("no finish", a(RSL, ADD, word(5))),
    ("empty-ish finish", a(FINISH)),
    ("open loop at end", a(LOOP, 3, RSL, FINISH)),
    ("misaligned branch", a(BRA, 1, EOR, word(0x81818181), FINISH)),
    ("unknown opcode", a(RSL, 0x00, FINISH)),
    ("unknown after branch skip", a(BRA, 1, 0x00, RSL, FINISH)),
    ("truncated add", a(RSL, ADD, 0x01, 0x02)),
    ("truncated loop", a(RSL, LOOP)),
    ("truncated bcc", a(RSL, BCC)),
    ("next without loop", a(RSL, NEXT, FINISH)),
    ("extra next", a(LOOP, 2, RSL, NEXT, NEXT, FINISH)),
]


def build_vectors(rng_seed, n_structured, n_unstructured):
    rng = random.Random(rng_seed)
    vectors = []  # (tag, script, seed)
    for tag, script in HAND:
        assert acceptable(script), tag
        for s in [0x00000000, 0xFFFFFFFF, 0x80000001, 0x12345678]:
            vectors.append(("hand: " + tag, script, s))
    made = 0
    while made < n_structured:
        script = gen_structured(rng)
        if not acceptable(script):
            continue
        made += 1
        for s in seeds_for(rng, 3):
            vectors.append(("structured", script, s))
    made = 0
    while made < n_unstructured:
        script = gen_unstructured(rng)
        if not script or not acceptable(script):
            continue
        made += 1
        for s in seeds_for(rng, 2):
            vectors.append(("unstructured", script, s))
    return vectors


def evaluate_reference(vectors):
    out = []
    for tag, script, seed in vectors:
        try:
            key = reference(script, seed)
        except StepLimit:
            continue
        out.append((tag, script, seed, key))
    return out


def fmt_line(tag, script, seed, key):
    exp = "ERR" if key is None else f"{key:08X}"
    return f"{script.hex().upper()} {seed:08X} {exp} # {tag}"


def parse_fixture(path):
    rows = []
    with open(path) as f:
        for line in f:
            body, _, tag = line.partition("#")
            parts = body.split()
            if len(parts) != 3:
                continue
            script, seed, exp = parts
            rows.append((tag.strip(), bytes.fromhex(script), int(seed, 16), None if exp == "ERR" else int(exp, 16)))
    return rows


def run_ours(eval_bin, rows):
    feed = "".join(f"{s.hex()} {seed:08X}\n" for _, s, seed, _ in rows)
    res = subprocess.run([eval_bin], input=feed, capture_output=True, text=True, check=True)
    out = []
    for line in res.stdout.splitlines():
        out.append(None if line.startswith("ERR") else int(line, 16))
    if len(out) != len(rows):
        raise SystemExit(f"sa2_eval returned {len(out)} lines for {len(rows)} inputs")
    return out


def compare(label, rows, theirs, ours):
    bad = 0
    for (tag, script, seed, _), t, o in zip(rows, theirs, ours):
        if t != o:
            bad += 1
            if bad <= 10:
                ts = "ERR" if t is None else f"{t:08X}"
                os_ = "ERR" if o is None else f"{o:08X}"
                print(f"  MISMATCH [{tag}] script={script.hex()} seed={seed:08X} reference={ts} opentune={os_}")
    print(f"{label}: {len(rows) - bad}/{len(rows)} agree")
    return bad


def cmd_generate(args):
    rows = evaluate_reference(build_vectors(args.rng_seed, args.structured, args.unstructured))
    with open(args.out, "w") as f:
        f.write(
            "# SA2 differential vectors: <script-hex> <seed> <key|ERR> # <tag>\n"
            "# Generated by tools/sa2/sa2_differential.py from SYNTHETIC programs only\n"
            f"# (rng seed {args.rng_seed}); expected values come from the public reference\n"
            "# bri3d/sa2-seed-key (MIT). No OEM script is in this file.\n"
        )
        for r in rows:
            f.write(fmt_line(*r) + "\n")
    errs = sum(1 for r in rows if r[3] is None)
    print(f"wrote {len(rows)} vectors ({errs} expected errors) to {args.out}")
    return 0


def cmd_check(args):
    bad = 0
    rows = parse_fixture(args.fixture)
    theirs = []
    for _, script, seed, _ in rows:
        theirs.append(reference(script, seed))
    bad += compare("fixture vs reference", rows, theirs, [r[3] for r in rows])
    if args.eval:
        bad += compare("fixture vs opentune", rows, [r[3] for r in rows], run_ours(args.eval, rows))
        if args.fresh:
            seed = args.rng_seed if args.rng_seed is not None else int(time.time())
            print(f"fresh programs: rng seed {seed}")
            fresh = evaluate_reference(
                build_vectors(seed, args.fresh * 3 // 4, args.fresh - args.fresh * 3 // 4)
            )
            bad += compare(
                "fresh: reference vs opentune", fresh, [r[3] for r in fresh], run_ours(args.eval, fresh)
            )
    return 1 if bad else 0


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    g = sub.add_parser("generate", help="write the fixture from synthetic programs + the reference")
    g.add_argument("--out", required=True)
    g.add_argument("--rng-seed", type=int, default=20261118)
    g.add_argument("--structured", type=int, default=320)
    g.add_argument("--unstructured", type=int, default=160)
    c = sub.add_parser("check", help="re-check the fixture; optionally diff fresh programs against OpenTune")
    c.add_argument("--fixture", required=True)
    c.add_argument("--eval", help="path to the built sa2_eval example")
    c.add_argument("--fresh", type=int, default=0, help="also run this many fresh random programs")
    c.add_argument("--rng-seed", type=int, help="rng seed for --fresh (default: time)")
    args = p.parse_args()
    sys.exit(cmd_generate(args) if args.cmd == "generate" else cmd_check(args))


if __name__ == "__main__":
    main()
