# Offline checksum search

Build the standalone tool with stable Rust. It has no dependencies.

```sh
rustc --edition=2021 -O tools/checksum-search/search.rs -o /tmp/checksum-search
/tmp/checksum-search diff --edited 0x10..0x20 stock.bin modified.bin
/tmp/checksum-search search --slot 0x100 --region 0x20..0x100 stock.bin modified.bin third.bin
rustc --edition=2021 --test tools/checksum-search/search.rs -o /tmp/checksum-search-tests
/tmp/checksum-search-tests
```

Supply equal-length images of the same layout at runtime. Keep images outside
the repository. All offsets are file offsets in decimal or hex. Range ends
are exclusive. Diff prints contiguous runs of changed bytes outside all
repeated `--edited` ranges. A run can start inside a stored field if some
bytes of the checksum stayed the same. Try nearby slot starts too.

Search tests each repeated `--region` independently. With a region and no
`--slot`, it scans all non-overlapping storage positions. With a slot alone,
it checks the prefix up to the slot. With neither option, it tries prefix
ends one, two and four bytes before EOF. Repeat `--slot` for alternatives.
Each match names the region, slot, storage width and byte order, and the
algorithm configuration. Numeric fields inside the algorithm description
use hexadecimal. Search narrows candidates with each successive image and
prints only configurations that reproduce every stored value. Empty output
means no match. Use independently changed inputs and a held-out image to
distinguish collisions, especially with short sums and uniform padding.

The CPU workers split configurations across native threads. Set `--jobs N`
to limit concurrency. `--gpu` is reserved and reports unimplemented.

| Family | Search space |
|---|---|
| Word reductions | 8/16/32-bit words, either byte order, 8/16/32-bit accumulators at least as wide as the word. Sum, one's/two's complement sum, XOR. Incomplete words are skipped as a configuration. |
| Fletcher and Adler | Fletcher-16 on bytes (initial sums zero, mod 255), Adler-32 (1,0, mod 65521). |
| CRC | Normal polynomials 0x8005 and 0x1021 (16 bits), 0x04c11db7 and 0x1edc6f41 (32 bits). Independently try zero/all-ones init and XOR-out, with reflection of both input/output on or off. |
| VAG EDC15-style accumulator loop | Four-byte multiples only. Try seed pairs (0,0) and (0x8631,0xefcd) with the documented VAG rotations, add/subtract, XOR and finalisation. |

The tool reports hypotheses for the selected regions and parameter grid.
It has no EGS flash profile or correction operation. [Sources and licences](../../docs/PRIOR-ART.md#egs52-groundwork).
