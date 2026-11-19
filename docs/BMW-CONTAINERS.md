# BMW PSdZData SWE flash containers

**Validation state: synthetic-tested.** The decoder has been exercised only on
fixtures generated inside the test suite (made-up identifiers, addresses and
payloads) and on one public NRV2B test vector. It has **not** been run against a
real BMW file. Treat its output as unverified until someone checks it against
their own data.

OpenTune reads (never writes) the flash containers that BMW's E-Sys and ISTA-P
use for F/G-series control units. The decoder is written from public material
only; the sources and their licences are listed at the end of this page and in
[PRIOR-ART.md](PRIOR-ART.md#bmw-psdzdata-swe-flash-containers).

## What the files are

The PSdZData tree holds one folder per SWE (*SoftWare-Einheit*) process class,
`psdzdata/swe/<class>/`. Each flash SWE is a **file pair**:

```text
swfl_0000beef.xml.001_002_003    descriptor
swfl_0000beef.bin.001_002_003    data
```

Name pattern: `<class>_<id>.<kind>.<v1>_<v2>_<v3>`. The class is lower-case,
the id is hexadecimal (normally 8 digits) and the version has three decimal
fields.

| Class | Meaning | OpenTune |
|---|---|---|
| `btld` | bootloader | decoded (`.bin` + `.xml`) |
| `swfl` | software | decoded (`.bin` + `.xml`) |
| `swfk`, `ibad` | other classes with the same `.bin` + `.xml` layout | decoded |
| `cafd` | coding data, a `.caf` file | **identified only, never decoded** (no public format description) |

OpenTune doesn't depend on the class name. A pair is decoded when its `.xml`
parses as a descriptor with a `FLASH-SEGMENT` table.

### The descriptor (`.xml`)

```xml
<BINARY-FLASHBLOCK>
  <SHORT-NAME>swfl_0000beef</SHORT-NAME>
  <SDGS>
    <SDG><SDG-CAPTION><SHORT-NAME>Ident</SHORT-NAME></SDG-CAPTION>
      <SD SI="ProcessClass">swfl</SD> <SD SI="Identifier">0000beef</SD>
      <SD SI="Version">001_002_003</SD></SDG>
    <SDG><SDG-CAPTION><SHORT-NAME>SignatureTable</SHORT-NAME></SDG-CAPTION>
      <SD SI="SignatureStatus">SIGNED</SD> <SD SI="SignatureKeyLength">2048</SD>
      <SD SI="SignatureHashMode">SHA256</SD> <SD SI="SignatureMode">rsa</SD></SDG>
    <SDG><SDG-CAPTION><SHORT-NAME>Checksum</SHORT-NAME></SDG-CAPTION>
      <SD SI="Mode">CRC16</SD> <SD SI="Value">1234</SD></SDG>
  </SDGS>
  <FLASH-SEGMENTS>
    <FLASH-SEGMENT COMPRESSION-STATUS="COMPRESSED">
      <SOURCE-START-ADDRESS>0000040</SOURCE-START-ADDRESS>   <!-- offset in the .bin -->
      <SOURCE-END-ADDRESS>0000FFF</SOURCE-END-ADDRESS>       <!-- inclusive -->
      <TARGET-START-ADDRESS>0800000</TARGET-START-ADDRESS>   <!-- ECU address -->
      <TARGET-END-ADDRESS>0802FFF</TARGET-END-ADDRESS>       <!-- inclusive -->
    </FLASH-SEGMENT>
  </FLASH-SEGMENTS>
</BINARY-FLASHBLOCK>
```

(The example values are synthetic.) Some files wrap this in a root element
with a default namespace, so OpenTune matches elements by local name. Addresses
are hex text, with or without `0x`. A segment can also carry an optional
`SHORT-NAME`, `LONG-NAME`, `COMPRESSION-METHOD` and `CHECKSUM`.

## What OpenTune does with them

```text
opentune container identify --in swfl_0000beef.bin.001_002_003
opentune container unpack   --in swfl_0000beef.bin.001_002_003 --outdir segs --flat image.bin
```

- `--in` can be either half of the pair. The other half is found by name in the
  same folder; use `--pair <file>` when it is somewhere else.
- `identify` prints the class, id and version, the Ident / SignatureTable /
  Checksum metadata, and the segment table with the codec chosen for each
  segment. It decompresses nothing.
- `unpack` decodes every segment. With `--outdir` it writes `segNN[_name].bin`,
  or `.partial.bin` when a segment is not flash-ready. With `--flat` it writes
  one image with every segment at its target address and gaps set to `--fill`
  (default `FF`). The flat image is refused if any segment is not flash-ready
  or two segments overlap.

### Segment rules

| `COMPRESSION-STATUS` | `COMPRESSION-METHOD` | Handling |
|---|---|---|
| `UNCOMPRESSED` (or absent) | none | Stored bytes. The source and target ranges must have the same length. |
| `COMPRESSED` | none, `nrv`, `ucl`, `nrv2b` | UCL **NRV2B** (8-bit), a raw stream with no header ([`nrv2b`](../crates/opentune-container/src/nrv2b.rs)) |
| `COMPRESSED` | `gz`, `gzip` | gzip |
| `UNCOMPRESSED` | any method other than none | Refused as contradictory |
| anything else (for example an encrypted form) | any | **Refused**: reported, raw bytes kept, nothing bypassed |

A decoded segment is **flash-ready** only when its length equals the
descriptor's target-range length and the range fits a 32-bit address. If the
lengths differ, the data is kept but the segment is marked not flash-ready,
because public tools disagree on whether the target range of a compressed
segment gives its decompressed size. Other problems are also reported per
segment without stopping the rest: a source range outside the `.bin`, a decode
error, or an NRV2B stream with bytes left after its end marker. Overlapping
target ranges are reported for the container as a whole.

The module flasher (`opentune-flash`) **refuses** BMW segments. How the decoded
bytes are sent over UDS (compressed or plain) and how the signature is handled
are not publicly documented. OpenTune doesn't flash BMW ECUs.

## Verified vs. not verified

| Item | State |
|---|---|
| File naming, pairing, descriptor elements, segment address semantics | From public sources. Synthetic-tested. |
| NRV2B decoder | Public vector from `uclcli` passes. Round-trips with OpenTune's own encoder. The reference library's safety checks are reproduced. |
| gzip segments | Synthetic-tested (flate2). |
| Target-range length vs. decompressed length | **Not uniform in public sources.** Reported, never assumed. |
| `Checksum` (CRC16) | **Not verified.** Its parameters and coverage are not publicly documented. Only the declared value is shown. |
| `SignatureTable` (RSA / SHA-256) | **Not verified.** Where the signature lives and which keys apply are not public. Only the declared metadata is shown. |
| CAFD `.caf` | **Not decoded.** |
| Inner payload formats (for example head-unit images with their own headers) | Out of scope. OpenTune stops at the container. |
| Real BMW files | **Untested.** |

The `nrv2b` module also has a small greedy **encoder**. It exists only to
generate test fixtures. Its output is valid NRV2B, but it is not byte-identical
to UCL's compressor, and OpenTune does not repack BMW containers.

## Sources

| Source | Licence | Used for |
|---|---|---|
| [I CAN Hack — OEM update files, BMW (SWFL)](https://icanhack.nl/knowledge-base/reverse-engineering/oem-update-files/) | No open licence identified (facts only) | Pair naming, the `BINARY-FLASHBLOCK` / SDG / `FLASH-SEGMENT` structure, `COMPRESSION-STATUS`, NRV |
| [afflux/uclcli](https://github.com/afflux/uclcli) (BMW Group) | GPL-3.0-or-later | The `ucl_nrv2b_decompress_safe_8` variant and the public test vector |
| [UCL](https://www.oberhumer.com/opensource/ucl/) (M. F. X. J. Oberhumer) | GPL-2.0-or-later | The NRV2B algorithm (implemented independently) |
| [JackieZ123430/psdzdata-swfl-inspector](https://github.com/JackieZ123430/psdzdata-swfl-inspector) | MIT | Folder layout, local-name matching, `0x` prefixes, the length rules, NRV2B for `COMPRESSED` |
| [speidy/BMW-SWE-extractor](https://github.com/speidy/BMW-SWE-extractor) | None (facts only) | `LONG-NAME`, `COMPRESSION-METHOD` (`gz`/`nrv`), `CHECKSUM`; the gzip length counter-example |
| [CDFN/bmw-virtual-reader](https://github.com/CDFN/bmw-virtual-reader) | None (facts only) | Flat image built from segments by target address |
| [LeonB1ham/svt-psdzdata-export](https://github.com/LeonB1ham/svt-psdzdata-export) | None (facts only) | Class folders; CAFD files are `.caf` |

No code was copied from any of these, and no BMW software file was read or used.
