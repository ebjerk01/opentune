# VAG DSG transmission control units

What OpenTune knows about the VAG dual-clutch (DSG / S tronic) TCUs, what is
validated, and what is deliberately left out. Map details live in
[`MAP-LIBRARY.md`](MAP-LIBRARY.md#vag-dsg-tcus-dq250-02e); the checksum code is
`crates/opentune-checksum/src/dsg.rs` (02E) and `src/tcu.rs` (MQB, DQ381).

Validation words follow [`DEVELOPMENT.md` §7](DEVELOPMENT.md#7-validation-state-vocabulary).

## Families

| Family | Gearbox code | TCU | Definition | Checksum | Maps | State |
|---|---|---|---|---|---|---|
| DQ250 (PQ35/PQ46), PowerPC software | `02E` (sw `02E300…`) | Temic, big-endian PowerPC, 0x68000 read | `temic-dq250-02e` | `dq250-02e` | 30 shift tables (raw, MEDIUM) | checksum **real-image-tested**; maps real-image-checked |
| DQ250 (PQ), TriCore software | `02E` (e.g. `02E300013N`) | Temic, Infineon TriCore, 0xB0000 read | `temic-dq250-02e-tricore` | none known | 20 shift tables (raw, MEDIUM) | identify + maps; no checksum |
| DQ250-MQB | `0D9` (sw `0D9300…`) | Temic, TC1766 | `temic-dq250-mqb` | `dq250` (JAMCRC) | none | identify; checksum synthetic-tested |
| DQ381 | `0GC` | Bosch, SH72549 | `bosch-dq381` | `dq381` | none | checksum real-image-tested (earlier work) |
| DQ200 | `0AM`, `0CW` | supplier unconfirmed | — (database unit `vag-dq200`) | none | none | identify only |
| DL382 | `0CK`, `0CL`, `0CJ` | supplier unconfirmed | — (database unit `vag-dl382`) | none | none | identify only |

`opentune-scan` recognises all of these from the part numbers in an image
(`<code>300<ddd>[A-Z]{0,2}` software, `<code>927<ddd>…` control unit). The 02E
ident record `<nn>02E300xxxX  \x01GSG DSG … DCT` (and DQ381's `BOSCH TCU` /
`…_DQ380` markers) raise a detection to the high tier; a bare DQ200/DL382 part
number is medium. The extracted part number also drives definition matching.

No DQ250-MQB, DQ200 or DL382 read was available, so those entries claim only
what the open references document. Nothing is guessed to fill the gaps: no
flash size, layout or checksum is set for DQ200/DL382.

## DQ250 02E — PowerPC generation

The software seen as a 0x68000-byte (416 KiB) read:

```text
0x00000 .. 0x5FFFF   code: twelve 32 KiB sectors S00..S11 (big-endian PowerPC)
0x60000 .. 0x67FFF   calibration sector
  0x60000+           ident record  "<nn>02E300xxxX  \x01GSG DSG ... DCT"
                     (<nn> = the two-digit dataset index; 0x61000+ on newer software)
  0x67FC0            version tag  "v069<vvvv>..__getriebe_DSG_XXXX"
  0x67FE0            00 00
  0x67FE2            13 x u16 BE  CRC table (S00..S11, CAL)
  0x67FFC            00 00 FF FF
```

Software that shares one code build carries several calibration datasets
(e.g. `02E300041N` A3 2.0 TDI and `02E300046K` TT 2.0 TFSI): their code sectors
are byte-identical and only the CAL sector differs.

### Checksum `dq250-02e` (real-image-tested)

Each footer word `i` is the **CRC-16/MCRF4XX** (polynomial `0x1021` reflected
= `0x8408`, init `0xFFFF`, reflected in/out, no final XOR) of its sector:
`S00..S11` are `[i·0x8000, (i+1)·0x8000)`; the CAL word covers
`[0x60000, 0x67FE0)` — the whole CAL sector up to the footer. Stored
big-endian.

Found by OpenTune on real reads (no third-party source): all 13 words match on
six images of five software versions (`02E300011BL`, `02E300011C`, `02E300041N`,
`02E300042F`, `02E300042G`, `02E300046K`) — 78 of 78. A seventh read
(`02E300042M`) has its reset vectors and footer erased (a partial or
tool-exported read) and is refused. The fixture tests also edit a calibration
byte, correct, and check that only the CAL word changed.

The DQ250-MQB **JAMCRC** (`dq250`) does not hold on any 02E read (it also needs
a 0x180000-byte image, so it never runs on one).

Anti-brick: a full image is recognised only with the footer framing present and
at least 7 of the 12 code sectors already validating; correction rewrites only
the words of sectors that mismatch. The 0x8000-byte CAL sector on its own is
handled from its data-independent framing (`00 00`, `00 00 FF FF`,
`getriebe_DSG`), and only its CAL word is written.

Not validated yet: that the TCU accepts a corrected image (bench/vehicle).

## DQ250 02E — TriCore generation

Later 02E software (seen: `02E300013N`, Golf VI GTI / R datasets) is
little-endian TriCore code. The available reads are 0xB0000 bytes with the
64 KiB CAL first (ident record near `0x3668`, version tag at `0xFFC0`) and the
code from `0x10000`. Neither the 02E sector table nor the MQB JAMCRC matches,
and no other trailer was found, so the definition has `checksum: null`:
OpenTune can show and compare these maps but cannot correct an edited image.

## DQ250-MQB (0D9)

From the open flashing tools (VW_Flash, mqb-flash-rs, DQ250-MQB-Bench-Flash):
UDS on CAN `0x7E1`/`0x7E9`; a 0x180000-byte bin file with DRIVER (0x80E) at
`0x0`, CAL (0x20000) at `0x30000`, ASW (0x130000) at `0x50000`; box code at
CAL+`0x1FFC0`, software version at CAL+`0x1FFE0`; each of CAL/ASW ends with its
JAMCRC (`!crc32`, little-endian). Factory containers use a 256-byte
substitution cipher over LZSS. Full reads go through the SBOOT bench path. The
`dq250` checksum refuses to write until one block's stored JAMCRC validates on
the image in hand — it stays **synthetic-tested** until a real 0D9 read appears.

## Flashing (later)

OpenTune does not flash any DSG TCU yet. The plan is the shared UDS
module-flash path (request download / transfer data / checksum routine) once
that path is validated on hardware; for 02E the transport and programming
session still have to be established on a car. FRF/ODX containers for these
families (method `0x11`: table cipher + compression, per vag-tcu-tools) are not
decoded for DSG yet.

## Safety

- A DSG TCU controls clutch pressure. Wrong shift or clutch data can damage the
  clutches or the gearbox and can make a car move unexpectedly. Edit by ratio,
  keep downshift points below their upshift points, and keep a verified stock
  read.
- Only calibration editing is in scope: no launch-control, torque-limiter or
  protection defeat.
- Bench reads: current-limited supply, fused +12 V, CAN terminated (see the
  database units' bench notes).

## Sources and licences

- [bri3d/VW_Flash](https://github.com/bri3d/VW_Flash) (BSD-2-Clause) —
  DQ250-MQB layout, JAMCRC, DSG cipher.
- [bri3d/mqb-flash-rs](https://github.com/bri3d/mqb-flash-rs) (MIT) —
  DQ250-MQB block table (lengths, binfile layout, box-code location).
- [TheFlashBold/DQ250-MQB-Bench-Flash](https://github.com/TheFlashBold/DQ250-MQB-Bench-Flash)
  (GPL-3.0) — TC1766, SBOOT bench read, JAMCRC per block.
- [dspl1236/vag-tcu-tools](https://github.com/dspl1236/vag-tcu-tools) (MIT) —
  DQ381 CRC-32 (earlier work), FRF method `0x11` for DQ200/DQ250/DL382.
- The 02E layout, checksum and maps: OpenTune's own analysis of real reads.
  No commercial tool output, A2L/DAMOS or map pack was used; the reads are
  private and never committed.
