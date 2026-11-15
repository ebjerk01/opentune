# Bosch EDC16 flash checksum

Reverse-engineering notes for the EDC16 (Infineon C16x / ST10, little-endian)
calibration checksum, as implemented in `crates/opentune-checksum/src/edc16.rs`.

## Reference material

Two valid 512 KiB (`0x80000`-byte) flash images of the **same** EDC16 ECU:

| file                 | role                                             |
|----------------------|--------------------------------------------------|
| `yaris_90_stock.ori` | factory-stock dump (valid checksums)             |
| `yaris_90_tuned.mod` | professionally tuned, re-checksummed, **valid**  |

ECU identity string at `0x8000`: `TSW V0.70 120701 1025 C9/ESB/G43`.

These files are private and **not** in the repository or CI. The `#[ignore]`d
test `edc16::tests::reference_files` uses them when `OPENTUNE_FIXTURES` points at
the directory holding them.

## File layout (from a structural scan of the stock image)

| range                 | contents                                    |
|-----------------------|---------------------------------------------|
| `0x00000 .. 0x08000`  | `0xC3` fill (empty boot area)               |
| `0x08000 .. 0x4DFFF`  | program / code (ID string at `0x8000`)      |
| `0x4E000 .. 0x6DFFF`  | `0xC3` fill                                 |
| `0x70000 .. 0x7BD80`  | calibration / maps                          |
| `0x7BD7C .. 0x7BD80`  | **calibration checksum word** (4 bytes)     |
| `0x7BD80 .. 0x7C000`  | `0x00` padding                              |
| `0x7C000 .. end`      | `0xC3` fill + a small tail data block        |

A repeated descriptor at `0x7BBE0 / 0x7BBF4 / 0x7BC08` holds the triple
`{0x0004C57E, 0x0AAB08E9, 0x10550C9F}`; `0x0004C57E` is a file-offset pointer
just past the end of the code area. These bytes do **not** change between the
stock and tuned files (the code is untouched), so they are code-area metadata
and are not needed to correct a calibration tune. Their exact meaning
(`0x0AAB08E9` / `0x10550C9F`) was not pinned down and is not relied upon.

## The calibration checksum field — VERIFIED

Byte-diffing the two valid files, every differing byte except one group lies in
the calibration/map cluster `0x72899 .. 0x7A673`. The single isolated non-map
difference is:

| offset    | stock (LE) | tuned (LE) | width | byte order    |
|-----------|------------|------------|-------|---------------|
| `0x7BD7C` | `0xDBCCED4D` | `0xDC88782E` | 4 B | little-endian |

Because a *valid* tuned file must carry a recomputed checksum, and this is the
only non-map field that the tuner changed, **this 32-bit little-endian field at
`0x7BD7C` is the calibration checksum the ECU verifies.** Its location, width
and byte order are verified against both reference files and are asserted by the
`reference_files` test. Confidence: **high.**

## The algorithm — NOT yet determined (honest status)

The algorithm that produces the stored value was **not** recovered from a single
stock/tuned pair. The following were tested and **excluded**:

### Additive sums — excluded by magnitude and by exhaustive search

The required change in the stored value between the two files is
`0xDC88782E - 0xDBCCED4D = 0xBB8AE1` (12 290 785).

* **Byte sum** is impossible: the sum of the absolute per-byte differences over
  the *entire* file is only `0x9F3` (2 547) — far below `0xBB8AE1`.
* **16-bit word sum** is impossible: the sum of absolute per-word differences is
  only `0x98FDB` (626 139) — still far below `0xBB8AE1`.
* **32-bit word sum**: over any range that covers the map edits the delta is a
  byte-rotation of `{0x00,0x04,0x3B,0xDF}` in every alignment phase and for a
  half-word-swapped grouping — never `0xBB8AE1`. An O(n) prefix-sum search over
  **all** contiguous ranges (byte/16/32-bit, LE/BE, value and two's-complement)
  found no non-degenerate range reproducing **both** stored values.
* Treating the 4 bytes as **two independent 16-bit checksums** also fails: no
  contiguous range reproduces the low half (`0xED4D`/`0x782E`) for both files.

So the field is an **avalanche** function, not an additive checksum.

### Standard CRC-32 — excluded

Using the CRC linearity identity `crc(a) ^ crc(b) = crc_linear(a ^ b)` (which is
independent of the CRC's init and final-xor), the diff of the two files must
satisfy `crc_linear(stock ^ tuned) = 0xDBCCED4D ^ 0xDC88782E = 0x07449563` at the
correct end offset. This was tested, sweeping every end offset, for:

* polynomials CRC-32, CRC-32C, CRC-32K (Koopman), CRC-32/Q, XFER, MPEG-2, POSIX,
  CRC-32/K2, in **both** reflected and forward forms;
* normal, 16-bit-swapped and 32-bit-swapped byte feed orders.

**None matched.** The field is therefore consistent with a CRC-32 using a
**non-standard / proprietary polynomial** (common on Bosch EDC units), which
cannot be uniquely recovered from one difference codeword — it needs more
stock/tuned pairs for the same ECU, or the checksum routine from the boot code.

## What the implementation does

`edc16.rs` ships a **data-driven** engine, not a hard-coded hack:

* `Algo` — `Sum8`, `Sum16Le/Be`, `Sum32Le/Be`, `Crc32`, and `Unknown`.
* `Block { name, sum_range, store_offset, store_len, store_le, algo }` — one
  checksum-protected block; the active map is a table (`DEFAULT_BLOCKS`), so new
  EDC16 variants are added as data.
* `verify_blocks` / `correct_blocks` iterate the table. The `ChecksumAlgo` impl
  for `Edc16` uses `DEFAULT_BLOCKS`.

`DEFAULT_BLOCKS` contains the one verified-in-position calibration field with
`algo: Algo::Unknown`:

* `verify` **surfaces** it (reports the stored value, `ok = false`, since no
  algorithm can confirm it);
* `correct` **refuses** (`Error::Unsupported`) rather than write a guessed value
  that would brick the ECU.

The sum/CRC algorithms are real and fully exercised by synthetic unit tests. For
an EDC16 variant whose checksum *is* a plain sum, add a `Block` with the right
`Algo` and it verifies/corrects end-to-end with no code changes.

## Validation

* `cargo test -p opentune-checksum` — 11 synthetic tests cover each `Algo`, width
  truncation, LE/BE storage, wrap-around, idempotent `correct`, corruption
  detection, and the `Unknown` surface/refuse behaviour.
* `reference_files` (`#[ignore]`d, needs `OPENTUNE_FIXTURES`) asserts the exact
  stored constants at `0x7BD7C` in **both** real files, that the default map
  surfaces-but-will-not-correct the `Unknown` block, and that the generic
  `Sum32Le` engine round-trips (correct → verify → corrupt → verify-fails →
  correct → verify-ok) over the **real** calibration bytes.

## Confidence summary

| item                                             | confidence   |
|--------------------------------------------------|--------------|
| checksum field at `0x7BD7C`, 32-bit, LE          | high (both files) |
| calibration region ≈ `0x70000 .. 0x7BD7C`        | medium (structural) |
| additive-sum algorithms ruled out                | high (proof + search) |
| standard CRC-32 ruled out                        | high (linearity sweep) |
| exact algorithm (non-standard CRC/proprietary)   | **unresolved** |

## To finish the reverse-engineering

1. Obtain more stock/tuned pairs for this ECU (`1025 C9/ESB/G43`) to get several
   difference codewords and run a CRC-RevEng polynomial recovery.
2. Or extract the checksum routine from the code area (`0x8000 .. 0x4DFFF`) and
   read the polynomial / seed directly.
3. Then replace `Algo::Unknown` in `DEFAULT_BLOCKS` with the recovered algorithm
   (add a `Crc32Custom { poly, init, xorout, refin, refout }` variant if needed)
   and extend `reference_files` to assert `stock` verifies OK and
   corrupt→correct reproduces the stored word.
