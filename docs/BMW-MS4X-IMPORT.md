# BMW MS4x (Siemens MS41 / MS42 / MS43) definition import

This note records where the BMW petrol `bmw-ms4x-*` ECU definitions came from,
what was and was not imported, and the base/offset conventions to keep straight.

## Source and license

The map and identity definitions were imported from the **ms4x-net** project,
whose TunerPro XDF definition files are released under **GPL-3.0** — compatible
with OpenTune's GPL-3-or-later. Only the **definitions** (XDF) were used. The
`firmware/` directories in those repos (stock ROM binaries) were **not** cloned
or imported; ROM rights are separate and out of scope.

| ecudef | Upstream repo | Commit | XDF file | Software |
|---|---|---|---|---|
| `bmw-ms4x-ms41` | https://github.com/ms4x-net/ms41 | `c3763f812c9da19d29d175ce8eb7484314c18edb` | `definitions/Siemens_MS41_1429861_ENG_256K_v1.0.xdf` | 1429861 |
| `bmw-ms4x-ms42` | https://github.com/ms4x-net/ms42 | `4b3f591ccb96399f06dd8bb13a2a9dcb9ca56c17` | `definitions/Siemens_MS42_0110C6_ENG_512K_v1.3.xdf` | 0110C6 |
| `bmw-ms4x-ms43` | https://github.com/ms4x-net/ms43 | `45b8db5a092f5a9512902186f6b6b2187238035b` | `definitions/Siemens_MS43_MS430069_ENG_512K_v1.16.xdf` | 430069 |

The same provenance (repo, commit, file, software, base offset) is recorded in
each imported map's `notes` as a `CONFIDENCE: MEDIUM (source: ...)` prefix, so
attribution survives even if a single map is copied out of the file.

All three upstream repos ship `LICENSE` = GNU GPL v3. No file was skipped for
licensing reasons. The English (`_ENG_`) XDFs were used; the German (`_GER_`)
MS41 variants carry the same data with DAMOS-German names and were not imported.

## What was imported

Run per ECU (definitions cloned to a scratch dir **outside** this repo, e.g.
`/opt/nvme-fw/ms4x-src/`, which is **not** committed):

```
opentune import-xdf <full-image.xdf>                       # confirm it parses
opentune import-xdf <full-image.xdf> --into ecudefs/bmw-ms4x-<ecu>.json
```

Then every imported map's `notes` was given the mandatory confidence label
(`CONFIDENCE: MEDIUM`, with the source row above).

| ecudef | Maps imported |
|---|---|
| `bmw-ms4x-ms41` | 437 |
| `bmw-ms4x-ms42` | 1002 |
| `bmw-ms4x-ms43` | 1469 |

"Maps" includes the linked axis tables (`ldp_…` single-use, `ldpm_…` shared)
that the upstream XDFs model as separate 1-D tables. OpenTune does not resolve
TunerPro linked-axis object references, so maps that referenced a shared axis
import with an undefined axis (validator `axis.undefined` **warning**, not an
error); the axis data itself is present as its own table.

## Full image vs calibration — do not mix bases

The imported map addresses are **file offsets into the FULL image** (the
`_512K` / `_256K` XDF), i.e. they already include the full-image base offset.
The upstream `_64K` / `_32K` / `_24K` calibration-only XDFs use base `0x0` and
would give different (smaller) offsets. Keep them separate.

| ecudef | `flash_size` (full) | Calibration region (in the full image) | Full-image base |
|---|---|---|---|
| `bmw-ms4x-ms41` | 262144 (256 KiB) | `0x14000`, 24576 (24 KiB) | `0x14000` |
| `bmw-ms4x-ms42` | 524288 (512 KiB) | `0x48000`, 32768 (32 KiB) | `0x48000` |
| `bmw-ms4x-ms43` | 524288 (512 KiB) | `0x70000`, 65536 (64 KiB) | `0x70000` |

The `regions[]` in each ecudef record the read-only `full` image and the
writable `calibration` sub-region. All imported map addresses land inside
`flash_size` (checked by `opentune ecudefs validate`).

## Scope limits / confirm-per-unit

- **Protocol / bus fields are placeholders.** BMW MS4x diagnose over K-line
  (BMW DS2 / KWP2000); a full read and any write need a bench/boot (BDM) tool —
  stock MS4x is not OBD-flashable (`full_read: bench_boot`). `protocol`,
  `diag_layer`, `bitrate`, `tx_id`, `rx_id` must be confirmed per unit.
- **Addresses are per-software.** They are valid for the exact software numbers
  above at the stated full-image base. Confirm against your own read before any
  read or write.
- **Checksum is out of scope.** `checksum: null`. BMW MS4x checksum handling is
  not implemented here — only the map/identity definitions were imported.
