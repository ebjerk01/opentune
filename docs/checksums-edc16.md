# Bosch EDC16 flash checksum

Reverse-engineering notes for the EDC16 (Infineon C16x / ST10, little-endian)
calibration checksum, as implemented in `crates/opentune-checksum/src/edc16.rs`.

See `docs/BOSCH-CHECKSUM.md` for the general Bosch multi-block checksum structure
these findings sit inside.

## Reference material

Three independent, valid EDC16 stock/tuned pairs with a **clean** single
checksum field were analysed (plus several VAG pairs that use a different
scheme, see below). Each pair is two valid images of the same ECU — one
factory-stock, one professionally tuned and re-checksummed.

| fixture                         | size   | role                     |
|---------------------------------|--------|--------------------------|
| `yaris_90_stock.ori` / `.mod`   | 512 KiB| Toyota Yaris 90, EDC16    |
| `bmw_e46_318d_376967_2m_*`      | 2 MiB  | BMW E46 318d, EDC16       |
| `bmw_e46_320d_361811_2m_*`      | 1 MiB  | BMW E46 320d, EDC16       |

Yaris ECU identity string at `0x8000`: `TSW V0.70 120701 1025 C9/ESB/G43`.

These files are private and **not** in the repository or CI. The `#[ignore]`d
tests `edc16::tests::reference_known_fields` and `::reference_files` use them
when `OPENTUNE_FIXTURES` points at the directory holding them (Yaris at the
root, the harvested pairs under `edc16-pairs/<name>_ori.bin` / `_mod.bin`). Run:

```
OPENTUNE_FIXTURES=/path cargo test -p opentune-checksum -- --ignored
```

## The calibration checksum field — VERIFIED (3 pairs)

Byte-diffing each valid pair, the only non-calibration difference is a single
isolated 32-bit **little-endian** word — the calibration checksum the ECU
verifies. Location, width and byte order are reproduced exactly on all three
pairs (asserted by `reference_known_fields`):

| ECU       | offset    | stock (LE)   | tuned (LE)   |
|-----------|-----------|--------------|--------------|
| Yaris 90  | `0x7BD7C` | `0xDBCCED4D` | `0xDC88782E` |
| BMW 318d  | `0xFDFF8` | `0x5A1965D7` | `0x70C43539` |
| BMW 320d  | `0xFEFF8` | `0x309CC08C` | `0xA1B42CB4` |

In every case the field is a lone 32-bit word surrounded by fill bytes
(`0xC3` on Yaris, `0xFF`/`0x5A` on BMW), with **no** adjacent complement word
and **no** adjacent `{start,end}` descriptor. Confidence: **high.**

## VAG EDC16 uses a different scheme (out of scope here)

The Audi / VW / Mercedes / Land-Rover EDC16 pairs that were harvested do **not**
have a lone checksum word. Instead each has a ~128-byte **high-entropy block**
near the end of a bank (e.g. Audi 512 KiB at `0x7DF7C..0x7E000`, 2 MiB at
`0x1FDF7C..`) whose bytes change *wholesale* between stock and tuned. This is a
signed/obfuscated checksum container, not a plain 32-bit field, and is not
addressed by the field-based engine. Documented here so it is not mistaken for
the clean-field case.

## The algorithm — NOT yet determined (honest status)

The algorithm that produces the 32-bit value is **still unidentified**. It was
re-tested against **all three** clean pairs and **excluded**:

### Additive sums — excluded (magnitude + exhaustive search, 3 pairs)

Required stored deltas are large and avalanche-like:
`Yaris 0xBB8AE1`, `BMW318 0x16AACF62`, `BMW320 0x71178C28`.

* **Byte / 16-bit sums** are impossible by magnitude: the summed absolute
  per-word differences over the whole image are far smaller than the required
  stored delta (Yaris byte `0x9F3`, 16-bit `0x98FDB`).
* **32-bit word sums** (LE/BE, every alignment phase): an O(n) prefix-sum search
  — both an absolute `sum(S,E)==stored` sweep and a signed difference-window
  sweep — finds **only** the degenerate self-sum of the checksum field itself
  (it sums the fill zeros after it), never a real data range. Excluded.

### XOR checksum — excluded (3 pairs)

The XOR of every changed 32-bit word does **not** equal the stored-value XOR
(`stored_stock ^ stored_tuned`) for any pair — e.g. Yaris `0x03001900` vs the
required `0x07449563`. So it is not a 32-bit XOR fold. (This is now a locked
regression in `reference_known_fields`.)

### Standard / Bosch CRC-32 over a contiguous range — excluded

Using `crc(a) ^ crc(b) = crc_linear(a ^ b)` (independent of init / xorout) and a
direct parameter brute, the following were swept over every end offset, both
reflections, and 16-/32-bit byte-swapped feed orders:

* CRC-32, CRC-32C, CRC-32K (Koopman), CRC-32/Q, XFER, MPEG-2, POSIX, CRC-32/K2;
* the Bosch/VAG forward polynomial `0x04C11DB7` with init 0 and init
  `0xFFFFFFFF`.

**None matched** over a single contiguous span.

### Multi-area / proprietary CRC — the remaining shape

A GF(2) **polynomial-GCD** recovery was run on the within-pair *codeword
differences* (message ⧺ stored-word, forward and reflected framings). Because a
same-length pair's codeword difference is divisible by the CRC generator
independent of init/xorout, the GCD of valid codewords should expose a degree-32
generator. It does **not** (GCD degree 2–3). That rules out *any* CRC over a
single contiguous span, and is consistent with the Bosch design in which the CRC
runs over a **concatenation of areas** named by a directory (see
`docs/BOSCH-CHECKSUM.md`): concatenation moves bit positions, so the raw
contiguous bytes are not a valid codeword.

The area directory is **not** present in the calibration partition of these
clean-field images: scans for `{start,end,cs,~cs}` complement descriptors and for
32-bit pointers bracketing the calibration region found none. For EDC16 it lives
in the boot/program area, which is outside the harvested calibration dumps.

**Conclusion:** the field is a 32-bit avalanche checksum — most likely a CRC-32
over a multi-area concatenation (or a proprietary polynomial). It cannot be
pinned from these images alone; it needs the on-image area directory or the ECU
checksum routine (C166 boot code). Until then it stays `Algo::Unknown`.

## What the implementation does

`edc16.rs` ships a **data-driven** engine:

* `Algo` — `Sum8`, `Sum16Le/Be`, `Sum32Le/Be`, `Crc32` (reflected IEEE),
  `Crc32Bosch` (forward poly `0x04C11DB7`, init 0 — the Bosch/VAG main CRC,
  added as a reference-backed, reusable algorithm; see `crate::crc32_bosch`),
  and `Unknown`.
* `Block { name, sum_range, store_offset, store_len, store_le, algo }` — one
  checksum-protected block; the active map is a table (`DEFAULT_BLOCKS`).
* `KNOWN_FIELDS` — the three VERIFIED clean-field samples (fixture, length,
  offset, stored stock/tuned), used by the fixtures tests.
* `verify_blocks` / `correct_blocks` iterate the table.

`DEFAULT_BLOCKS` carries the one verified Yaris field with `algo: Algo::Unknown`:

* `verify` **surfaces** it (reports the stored value, `ok = false`);
* `correct` **refuses** (`Error::Unsupported`) rather than write a guessed value.

## Validation

* `cargo test -p opentune-checksum` — synthetic tests cover each `Algo`
  (including `Crc32Bosch` against the forward-engine MPEG-2 vector `0x0376E6E7`
  and the init-0 value `0x89A1897F`), width truncation, LE/BE storage,
  wrap-around, idempotent `correct`, corruption detection, and the `Unknown`
  surface/refuse behaviour.
* `reference_known_fields` (`#[ignore]`d) asserts the field location/width/order
  and both stored values on all three real pairs, and that none is a 32-bit XOR.
* `reference_files` (`#[ignore]`d) additionally proves the generic `Sum32Le`
  engine round-trips on the real Yaris bytes (correct → verify → corrupt →
  verify-fails → correct → verify-ok).

## Confidence summary

| item                                               | confidence            |
|----------------------------------------------------|-----------------------|
| checksum field location/width/LE (3 pairs)         | high (all reproduced) |
| additive-sum algorithms ruled out                  | high (proof + search) |
| 32-bit XOR ruled out                               | high (3 pairs)        |
| standard/Bosch CRC over contiguous range ruled out | high (linearity + GCD)|
| multi-area or proprietary CRC (shape)              | medium (by elimination)|
| exact algorithm                                    | **unresolved**        |

## To finish the reverse-engineering

1. Recover the **area directory** (the `{start,end}` list the ECU concatenates).
   It is not in the calibration partition; extract it from the boot/program area
   or from a full ECU dump, then CRC over the concatenation and brute the poly.
2. Or extract the checksum routine from the C166 code and read poly / seed /
   area list directly.
3. Then replace `Algo::Unknown` in `DEFAULT_BLOCKS` with the recovered algorithm
   (add e.g. `Crc32Custom { poly, init, xorout, refin, refout, areas }` if the
   existing `Crc32Bosch` parameters do not fit) and extend the fixtures tests to
   assert `stock` verifies OK and corrupt→correct reproduces the stored word.
