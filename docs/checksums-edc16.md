# Bosch EDC16 flash checksum — SOLVED (MPC56x family)

Reverse-engineering notes for the Bosch EDC16 flash checksum, as implemented in
`crates/opentune-checksum/src/edc16.rs`.

See `docs/BOSCH-CHECKSUM.md` for the general Bosch multi-block checksum structure
these findings sit inside.

## Headline

The EDC16 variants built on the **Freescale/Motorola MPC56x** core (PowerPC,
**big-endian**) — `EDC16C35` (BMW), `EDC16C39` (Kia/Hyundai) and siblings — were
**fully cracked** from complete (boot + program + calibration) dumps. The scheme
is a **block directory** plus a plain **32-bit big-endian additive checksum to a
fixed constant**, not a CRC and not an avalanche field:

> For every directory block, `sum32be(flash[start..=end]) == 0xD01FE500`, where
> `sum32be` is the sum of big-endian 32-bit words over the block, truncated to
> 32 bits. The **last dword of each block** is the compensation/checksum word:
> `last = (0xD01FE500 − sum_of_the_rest) mod 2^32`, stored big-endian.

This is `verify()`/`correct()` in the crate today, validated on **14 full dumps**
(7 stock/tuned pairs, 3 distinct ECUs).

## Reference material

Full dumps analysed (all private, not in the repo/CI; placed under
`$OPENTUNE_FIXTURES/edc16-full/<name>_ori.bin` / `_mod.bin`):

| fixture(s)                          | size   | ECU / core                       |
|-------------------------------------|--------|----------------------------------|
| `bmw_318d_376967`                   | 2 MiB  | BMW E46 318d, **EDC16C35** MPC562|
| `bmw_320d_361811`                   | 1 MiB  | BMW E46 320d, **EDC16C35** MPC562|
| `kia_sorento_{381743,388042,390051,394237,394261}` | 2 MiB | Kia Sorento CRDi, **EDC16C39** MPC562 |

ID strings confirm the core, e.g. `BOSCH EDC16C35/C (BMW) MPC562/RevA` and
`BOSCH EDC16C39/5.4x MPC562/RevD`.

Run the fixtures tests:

```
OPENTUNE_FIXTURES=/path cargo test -p opentune-checksum -- --ignored
```

`reference_mpc_directory` validates the full scheme end-to-end; the older
`reference_known_fields` / `reference_files` cover the calibration-only
clean-field facts.

## The directory

A directory of 16-byte records lives in the boot/program and partition-header
areas:

```
struct DirRecord {   // 16 bytes, big-endian
    uint32_t start;      // inclusive CPU address of block start
    uint32_t end;        // inclusive CPU address of block end
    uint32_t cs_slot;    // == 0xFADECAFE  (placeholder checksum)
    uint32_t cmpl_slot;  // == 0xCAFEAFFE  (placeholder complement)
};
```

* The `0xFADECAFE` / `0xCAFEAFFE` magic pair is the directory's structural
  fingerprint; `parse_directory()` scans for it and reads the two big-endian
  words immediately before it as `{start, end}`.
* **CPU → file offset**: mask with `len − 1` (images are 512 KiB / 1 MiB /
  2 MiB, powers of two). Equivalently, subtract the per-image ROM base — `0` for
  the 2 MiB BMW/Kia reads, `0x00800000` for the 1 MiB BMW 320d (whose directory
  stores `0x008E0180` etc.).
* Some directories carry an off-by-one **duplicate** record (`end+1`); these are
  rejected by requiring dword alignment (`start % 4 == 0 && (end+1) % 4 == 0`).

Each image carries several blocks (program sub-blocks, info blocks and the
calibration block); the walker treats them all uniformly. For a tune, only the
calibration block's compensation word needs rewriting.

## The checksum — VERIFIED on 14 images

For **every** directory block of **every** stock and tuned image:

```
sum32be(block) = Σ big-endian u32 words over [start, end] inclusive, mod 2^32
               = 0xD01FE500   (the fixed Bosch constant)
```

The constant `0xD01FE500` is identical across BMW C35, BMW C35 (1 MiB) and Kia
C39 — it is a family constant, not per-ECU. The compensation word is each block's
**last dword**, and re-deriving it as `0xD01FE500 − sum(block without last dword)`
reproduces the stored value **exactly** for all blocks (e.g. the BMW calibration
"clean field" below).

### The calibration "clean field" revisited

Earlier calibration-only analysis isolated a lone 32-bit field at the top of the
cal bank (BMW 318d `0xFDFF8`, 320d `0xFEFF8`). It is exactly the calibration
block's last-dword compensation word — read **big-endian** on this PowerPC core:

| ECU       | field     | stock (BE)   | tuned (BE)   | (same bytes, LE) |
|-----------|-----------|--------------|--------------|------------------|
| BMW 318d  | `0xFDFF8` | `0xD765195A` | `0x3935C470` | `0x5A1965D7` / `0x70C43539` |
| BMW 320d  | `0xFEFF8` | `0x8CC09C30` | `0xB42CB4A1` | `0x309CC08C` / `0xA1B42CB4` |
| Kia 381743| `0x1FDFFC`| `0xC96D875F` | `0x782E8453` |                  |

The BMW values match the prior pass's field exactly (it read them little-endian).

## Why the prior (calibration-only) pass could not solve it

The earlier pass had only the calibration partition and correctly concluded the
field was *not* a contiguous CRC and *not* `sum(range) == stored`. Both negatives
were right; the reasons it looked unsolvable were:

1. **The relationship is `sum(block including the field) == constant`**, not
   `sum(range) == stored`. A search for a range whose sum equals the field value
   can never find it.
2. **The field is big-endian** (MPC56x is a PowerPC core). Read little-endian the
   32-bit sums never line up.
3. **The directory lives in the boot/program area**, absent from a cal-only dump,
   so the `{start,end}` bounds and the "it's one block of many" structure were
   invisible.
4. It is an **additive checksum, not a CRC** — consistent with the prior GF(2)
   polynomial-GCD finding no degree-32 generator.

The VAG Audi/VW EDC16 variants noted earlier as a "high-entropy block" are a
separate (newer / signed) case and are not claimed solved here; the MPC56x BMW
and Kia families above are.

## What the implementation does (`edc16.rs`)

* `parse_directory(image) -> Vec<DirBlock>` — structural scan for the marker
  pair, CPU→offset mapping, alignment filter, de-dup, sorted by `end` ascending.
* `verify_directory(image)` — reports each block; `ok` iff it sums to
  `EDC16_MPC_SUM_CONST` (`0xD01FE500`).
* `correct_directory(image)` — rewrites each off block's last-dword compensation
  word, inner blocks first (so nested sub-blocks are fixed before their enclosing
  block; an enclosing block's compensation word always lies outside the inner
  blocks, so no fix is undone).
* `Edc16::verify` / `Edc16::correct` use the directory walker when a directory is
  present (a full dump), and otherwise fall back to surfacing the calibration
  field from `DEFAULT_BLOCKS` / `KNOWN_FIELDS` (and refusing to correct it, since
  a cal-only dump lacks the directory).

The generic data-driven `Algo`/`Block` engine (additive sums, reflected CRC-32
and the Bosch forward CRC-32 `Crc32Bosch`) is retained for other
variants/families and remains fully unit-tested.

## Validation

* `cargo test -p opentune-checksum` — synthetic tests cover the directory parser
  (incl. nested blocks and misaligned-duplicate rejection), `sum32be`, the
  compensation-word round-trip, corruption detect→correct, and the no-directory
  fallback, plus the pre-existing `Algo` coverage.
* `reference_mpc_directory` (`#[ignore]`d) proves, on all 7 full-dump pairs: the
  directory parses (multi-block); **every** block sums to the constant in both
  stock and tuned; valid images need no correction; the calibration block's
  stored compensation word equals the recorded big-endian value; corrupt→correct
  →verify round-trips; and zeroing the field then correcting **re-derives the
  exact original stored value**.

## Confidence summary

| item                                                   | confidence              |
|--------------------------------------------------------|-------------------------|
| MPC56x directory structure + marker fingerprint        | high (14 images)        |
| `sum32be(block) == 0xD01FE500` for every block          | high (14 images)        |
| last-dword compensation word, exact re-derivation      | high (all blocks)       |
| BMW cal field == prior clean field (read BE)           | high (exact match)      |
| applies to other MPC56x EDC16C3x (C31, …)              | medium (same family)    |
| VAG Audi/VW EDC16 (signed high-entropy block)          | **separate, unsolved**  |

## Open / next

* Confirm the constant and directory on further MPC56x variants (C31, C34, …).
* The Kia C39 also carries extra high-entropy data in the cal tail; the sum
  check and last-dword correction are valid, but if an independent signature is
  also enforced by the ECU it is out of scope here.
* Solve the VAG Audi/VW EDC16 signed container separately (different scheme).
