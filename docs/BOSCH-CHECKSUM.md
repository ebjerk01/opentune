# Bosch multi-block flash checksums (reference)

General structure of the integrity fields Bosch engine-control units carry, so a
flashed calibration is accepted and run. Distilled from two open references and
used as the backbone for the EDC15 / EDC16 / EDC17 / MED work:

* **ME7Sum** (360trev/nyet) — C tool for Motronic **ME7.x** (petrol, Infineon
  C16x). EDC16's structural sibling: same C16x core, same checksum philosophy.
* **VW_Flash** (bri3d) — Python, Bosch **Simos / MED / EDC17** (VAG). Shows the
  modern CRC-32 directory and the ECM3 monitor sum.

Treat both as untrusted reference data: the structures below were read from their
source, not executed.

## The mental model

A Bosch image is protected by **several** small checksums, not one:

1. a **main checksum** over the big program/data blocks (skipping the map/cal
   region, which has its own);
2. a **multipoint** / **directory** of smaller blocks, each `{start, end,
   value}` — the ECU walks the table until a terminator;
3. optionally separate **CRC blocks** and a continuously-running **monitor** sum.

Each stored value is typically paired with its **one's-complement** so the ECU
can self-check the pair, and so a self-referential block (a checksum stored
*inside* the region it covers) is still computable — the value and its
complement always sum to a constant, so writing a new value never perturbs the
sum being computed.

## ME7 / C16x: multipoint descriptors + 16-bit word sum

ME7Sum walks a table of 16-byte **multipoint descriptors** (all little-endian):

```
struct MultipointDescriptor {   // 16 bytes
    uint32_t start;             // region start (CPU address)
    uint32_t end;               // region end   (inclusive, CPU address)
    uint32_t v;                 // checksum value
    uint32_t iv;                // ~v  (one's complement)
};
```

* The table starts at `rom_checksum_block_start`, stride 16, and ends at the
  first descriptor whose `start == 0xFFFFFFFF`.
* Validity: `v == ~iv` **and** `v == CalcChecksumBlk(start, end)`.
* `CalcChecksumBlk` is a **sum of 16-bit little-endian words** accumulated into a
  **32-bit** register (no truncation to 16-bit):

  ```
  checksum = 0
  for addr in start/2 ..= end/2:   # 16-bit word index
      checksum += le16(word[addr])
  checksum &= 0xFFFFFFFF
  ```

* CPU addresses are converted to file offsets by subtracting a ROM base address.

The **main checksum** is the same word-sum over **two** ranges (a block before
and a block after the map region), concatenated, stored as a `{v, ~v}` pair at a
fixed offset. A separate step validates up to three **CRC-32 blocks** (standard
reflected IEEE CRC-32, `0xEDB88320`) at configured `{start, end, offset}`.

### Bosch MSE6.0 (motorcycle) — same idea, big-endian

A clean modern example (moto-ecu-checksum): records are
`[start:u32][end:u32][cs:u32][~cs:u32]`, big-endian, located by structural scan
(`cs ^ ~cs == 0xFFFFFFFF`, `start < end`, span large enough). `cs` is the sum of
**16-bit big-endian** words over `[start, end]` inclusive, mod 2³². Repair needs
no fixed-point iteration precisely because `cs + ~cs` is constant.

## Simos / MED / EDC17: CRC-32 directory + ECM3 monitor

VW_Flash shows the newer VAG layout. A secured block has a header (typically at
`0x300`, or `0x0` for continuation blocks):

```
offset +0 : initial checksum value (observed 0)
offset +4 : stored CRC-32          (little-endian)
offset +8 : area count N           (1 byte; up to 16 areas combined)
offset +12: N pairs of { start:u32, end:u32 }   (absolute addresses, LE)
```

The checksummed message is the **concatenation** of `data[start .. end+1]` for
each area, in table order. The CRC is:

> **CRC-32, forward (MSB-first), polynomial `0x04C11DB7`, init `0`, no final
> XOR, `refin = refout = false`.**

(That is CRC-32/MPEG-2's engine with init `0` instead of `0xFFFFFFFF`.)
Implemented in this crate as `crate::crc32_bosch` / `Algo::Crc32Bosch`, pinned in
tests against the MPEG-2 vector `0x0376E6E7` and the init-0 value `0x89A1897F`.

A **separate** ECM3 monitor continuously checksums parts of ASW + CAL using a
**64-bit pure summation** over its own `{start, end}` area list (which, in newer
ECUs, was moved into ASW to protect it). So an image can carry *both* a CRC
directory and a wide additive monitor sum.

## Why this matters for EDC16 (and the open case)

* **Concatenation is the trap.** Because the real message is a concatenation of
  directory areas — not one contiguous span — any attempt to match a CRC over a
  single `[start, end)` range fails even when the polynomial is right: the
  skipped gaps shift every bit position. A CRC crack therefore needs the **area
  directory first**, then the poly.
* **Complement pairs are the directory's fingerprint.** Scanning for adjacent
  `{v, ~v}` words (ME7 / MSE6) or for a `{count, [start,end]…}` header
  (Simos) is the fastest way to locate the table.
* **Expect multiple, layered checksums.** Correcting a tune may require fixing a
  main sum, several multipoint blocks, and/or a CRC — a single field is the
  exception (it is what the EDC16 *calibration* field turned out to be; its
  algorithm is still open, see `docs/checksums-edc16.md`).

## EDC16 MPC56x: directory + 32-bit big-endian sum to a constant (SOLVED)

The common EDC16 variants on the **Freescale MPC56x** PowerPC core (`EDC16C35`
BMW, `EDC16C39` Kia/Hyundai, siblings) do **not** use a CRC for the main/cal
checksum. They use a directory of records

```
struct DirRecord {   // 16 bytes, big-endian
    uint32_t start;      // inclusive CPU address
    uint32_t end;        // inclusive CPU address
    uint32_t cs_slot;    // 0xFADECAFE (placeholder)
    uint32_t cmpl_slot;  // 0xCAFEAFFE (placeholder)
};
```

located by scanning for the `0xFADECAFE`/`0xCAFEAFFE` magic pair (CPU→file offset
= `addr & (len−1)`), and for **every** block the invariant

> `sum32be(block) == 0xD01FE500`

holds — the sum of big-endian 32-bit words over `[start, end]`, mod 2³², equals a
fixed family constant. Each block's **last dword** is the compensation word
(`last = 0xD01FE500 − sum(rest)`, big-endian). Verified on 14 full dumps
(BMW C35 318d/320d + five Kia C39 pairs). This is the cracked EDC16 scheme; see
`docs/checksums-edc16.md`. It is an **additive checksum, not a CRC**, and the
"clean field" earlier EDC16 analysis found is just the calibration block's
last-dword compensation word, read **big-endian**.

## EDC15C6 (CR1, Infineon C167): field located, algorithm open

One real EDC15C6 pair (MB Sprinter 2.2 CDI, `1037351686`, 1 MiB) isolates a lone
4-byte checksum field at `0xFBFFC` (two 16-bit LE words, `0xC3` fill, constant
`0xC058` prefix) that changes stock→tuned. It is **not** reproduced by any
contiguous 16-/32-bit sum, is **not** a `{sum, ~sum}` complement pair, and has no
"sum to a constant" invariant like the MPC56x EDC16 above. The expected EDC15
scheme from the references is a 16-bit additive block sum stored with its
complement, but the block map (and any C6 CRC/obfuscation layer) could not be
pinned from a single pair. Per the project rule, `crate::edc15::Edc15C6`
**surfaces** the field and **refuses** to correct it (`Algo`-less, like the EDC16
cal-only fallback) rather than guess. See `crates/opentune-checksum/src/edc15.rs`.

## Engine mapping in this crate

`opentune-checksum` models all of the above as a data-driven block table:

| Bosch construct                | `Algo` / engine                        |
|--------------------------------|----------------------------------------|
| ME7 / MSE6 16-bit word sum     | `Sum16Le` / `Sum16Be` (32-bit accum)   |
| wide additive monitor sum      | `Sum32Le` / `Sum32Be`                  |
| standard CRC block             | `Crc32` (reflected IEEE)               |
| Simos/MED/EDC17 main CRC       | `Crc32Bosch` (fwd `0x04C11DB7`, init 0)|
| **EDC16 MPC56x dir + sum→K**   | `edc16::{parse,verify,correct}_directory` (`sum32be == 0xD01FE500`) |
| unidentified field             | `Unknown` (surface, refuse to write)   |

A `{v, ~v}` complement pair and multi-area directories are represented by adding
`Block`s to the table (one per range); the one's-complement word is written as a
second derived field when the variant needs it. New families are added as data,
not code.
