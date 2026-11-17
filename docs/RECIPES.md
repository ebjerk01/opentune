# Tune recipes (`.otrecipe`)

A **tune recipe** is a portable, reviewable description of a tune: the bytes a
tuner changed in a stock image, each anchored to the exact original bytes and a
little unchanged context, plus the identity of the image it was made from.
It is produced and consumed by `opentune-recipe` and the `opentune recipe` CLI.

A recipe is **not** a cross-firmware tune converter. It applies to the image it
was made from (typically: your own stock read of the same ECU, re-read later or
restored from a backup) and **refuses** — writing nothing — whenever it cannot
prove that. The one looser mode, relocation, must be asked for explicitly and
still refuses every ambiguous match.

```sh
# capture: stock + tuned -> recipe (names patches after a definition's maps)
opentune recipe create stock.bin tuned.bin --def bosch-edc16u34 -o tune.otrecipe

# inspect
opentune recipe show tune.otrecipe

# apply: only to a matching image; checksums fixed and attested on the way out
opentune recipe apply tune.otrecipe my_stock.bin -o my_tuned.bin
#   -> my_tuned.bin + my_tuned.bin.attestation.json  (or REFUSED, nothing written)

# explicit relocation mode (see below)
opentune recipe apply tune.otrecipe other_read.bin -o out.bin --relocate
```

`create` options: `--checksum <algo>` (otherwise taken from `--def`, else from
the structurally detected family), `--context N` (default 8 bytes per side),
`--gap N` (changes closer than N unchanged bytes merge into one patch, default
16), `--note`. `apply` options: `--relocate`, `--checksum <algo>` (override),
`--attestation <path>`, `--report <path>` (full per-patch report as JSON).

## File format (version 1)

UTF-8 JSON. Byte strings are lowercase hex. Unknown fields are ignored; a file
whose `format` is not `opentune-recipe`, or whose `version` is newer than the
reader supports, is rejected.

```json
{
  "format": "opentune-recipe",
  "version": 1,
  "tool": "opentune 0.1.0",
  "note": "stage 1 (example)",
  "source": {
    "size": 2097152,
    "family": "EDC16",
    "def_id": "bosch-edc16u34",
    "part_numbers": ["0281000000"],
    "sw_numbers": ["1037000000"],
    "sha256": "<sha-256 of the whole stock image>",
    "unchanged_sha256": "<sha-256 of every byte outside patches and checksum fields>"
  },
  "checksum": "edc16",
  "checksum_fields": [ { "start": 2097148, "end": 2097152 } ],
  "result_sha256": "<sha-256 of the tuned image>",
  "patches": [
    {
      "offset": 1048576,
      "original": "03e803e8",
      "new": "041a041a",
      "context_before": "0001000200030004",
      "context_after": "0005000600070008",
      "maps": ["example map"]
    }
  ]
}
```

(All values above are placeholders, not real part numbers or calibration data.)

| Field | Meaning |
|---|---|
| `source.size` | Image size; must match the target in every mode. |
| `source.family` | Structural family from `opentune-scan` (`EDC15`, `EDC16`, `EDC17`, `PCR2.1`, `Unknown`); must match. |
| `source.part_numbers` / `sw_numbers` | Identity strings found in the image (VAG/Bosch/Mercedes part numbers, `103…` software numbers). **VINs are never recorded.** |
| `source.unchanged_sha256` | SHA-256 over the stock image with every patch range and checksum field removed (the remaining bytes concatenated in order). Equal only for the same software *and* calibration. |
| `checksum` | `opentune-checksum` algorithm id used to correct the output; `null` if none was known (apply then needs `--checksum`). |
| `checksum_fields` | Ranges the checksum correction rewrote in the tuned image. They are **not** patches: the correction recomputes them for the target. Detected at create time by reverting each small changed range and checking that the algorithm reproduces the tuned bytes exactly. |
| `result_sha256` | Hash of the tuned image; `apply` reports when its output is byte-identical to it. |
| `patches[].offset` | Offset of the first changed byte in the source image. |
| `patches[].original` / `new` | Source bytes and replacement bytes (same length). Unchanged bytes inside a merged patch appear identically in both. |
| `patches[].context_before` / `context_after` | Up to `--context` unchanged source bytes on each side (shorter at the image edges). |
| `patches[].maps` | Map names (or `name (x axis)`) from `--def` that the patch touches. Informational. |

## Apply rules

`apply` works in four stages and stops at the first refusal; the target is
never partially patched.

1. **Identity.** Strict mode (default) requires: same size, same structural
   family, every recorded part number present, every recorded software number
   present, and the same `unchanged_sha256`. Relocation mode requires the same
   size, the same family and **at least one shared hardware part number**; the
   software numbers and the unchanged-region hash are reported but not required.
2. **Anchors.** Every patch is resolved against the *unmodified* target:
   - `anchored` — context + original bytes match exactly at `offset`;
   - `already applied` — context + new bytes match at `offset`;
   - strict mode: anything else is a `mismatch` (the first differing byte is
     reported);
   - relocation mode: the sequence `context_before + original + context_after`
     is searched across the whole target. Exactly one occurrence → `relocated`
     (old → new offset reported); none → `not found`; two or more →
     `ambiguous`. Relocated patches that would overlap are refused.
   Any unresolved patch refuses the recipe. A target that already carries
   *some but not all* patches is refused (a partial or different tune); one that
   carries *all* of them is a verified no-op.
3. **Checksums.** The new bytes are written and the checksum algorithm
   (`--checksum`, else the recipe's) corrects the image, using the untouched
   target as the reference image (needed by EDC17). No known algorithm →
   refused.
4. **Attestation.** `opentune-attest` verifies every checksum block of the
   output. Only a satisfied attestation, bound to the output's SHA-256, lets
   the image be written; it is saved next to it (`<out>.attestation.json`) for
   the flash safety policy.

Relocation is for re-reads whose layout shifted (e.g. a different read range
or a re-based dump) — **not** for moving a calibration onto different
software. Relocating a tune across software revisions can be dangerous even
when every byte pattern is found; review a relocated output before flashing.

## Credits

The portable-recipe idea and its strict → explicit-looser identity ladder were
learned from the described behaviour of
[openremap-core](https://github.com/v-arapidis/openremap-core) (MIT); OpenTune's
format and implementation are independent (see `crates/opentune-recipe/NOTICE`).
