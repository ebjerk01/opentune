# VAG DSG transmission control units

What OpenTune knows about the VAG dual-clutch (DSG / S tronic) TCUs, what is
validated, and what is deliberately left out. Map details live in
[`MAP-LIBRARY.md`](MAP-LIBRARY.md#vag-dsg-tcus-dq250-02e); the checksum code is
`crates/opentune-checksum/src/dsg.rs` (02E) and `src/tcu.rs` (MQB, DQ381).

Validation words follow [`DEVELOPMENT.md` §7](DEVELOPMENT.md#7-validation-state-vocabulary).

## Families

| Family | Gearbox code | TCU | Definition | Checksum | Maps | State |
|---|---|---|---|---|---|---|
| DQ250 (PQ35/PQ46), PowerPC software | `02E` (sw `02E300…`) | Temic, big-endian PowerPC, 0x68000 read | `temic-dq250-02e` | `dq250-02e` | 30 shift tables + drive-off table (raw, MEDIUM) | checksum **real-image-tested**; maps real-image-checked |
| DQ250 (PQ), TriCore software | `02E` (e.g. `02E300013N`) | Temic, Infineon TriCore, 0xB0000 read | `temic-dq250-02e-tricore` | none known | 20 shift tables + drive-off table (raw, MEDIUM) | identify + maps; no checksum |
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

The owner's reference library was searched again (2026-11-18) by file name
(`dsg`, `02E`, `0AM`, `0CW`, `0CK`, `0CL`, `DQ200`, `DL382`, `stage`/`mod`/`ori`
next to a TCU part number), by container type (`.frf`, `.sgo`, `.odx`, `.pdx`)
and by content (the `GSG DSG` / `getriebe_DSG` ident and `0AM300…`, `0CK300…`,
`0CL300…`, `0CW300…`, `02E300…` part numbers in every binary). Result: the nine
02E reads already used here, no tuned 02E file, and **no DQ200 or DL382 dump or
FRF/SGO container**. The only DQ200/DL382 material in the library sits inside
commercial WinOLS projects, A2L files and DCM calibration exports, which the
clean-room rules exclude, so they were not opened. DQ200 and DL382 therefore
stay identify-only.

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
(e.g. `02E300041N` A3 2.0 TDI, `02E300046K` TT 2.0 TFSI and `02E300042M` Golf
GTI 2.0 TFSI): their code sectors are (nearly) identical and only the CAL
sector differs. The CAL has no inline map headers; tables are fixed-size blocks,
sometimes with their breakpoint axis stored directly in front. In that code
build the region `0x62400..0x65000` is largely repeated (most words identical)
`0x2C00` bytes later (`0x65000..0x67C00`): two parameter sets of the same
layout, each holding one of the shift-schedule groups B and C. Which condition
selects a set is not established.

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

## Read-only diagnostics (`opentune dsg`)

`crates/opentune-dsg` reads a DQ250 02E over **KWP2000 on a VW TP2.0
channel to the gearbox, logical address `0x02`**, the way `opentune haldex
gen4` reads the Haldex (see [`HALDEX.md`](HALDEX.md)). It is **read-only**:
the only services it sends are `0x10` (session start), `0x1A`, `0x18` and
`0x21`. There is no clear-codes (`0x14`), coding, adaptation, basic-setting,
security-access, routine or flash service in the crate.

```text
opentune dsg identify --driver slcan --port /dev/ttyACM0 [--json]
opentune dsg dtcs     --driver socketcan --port can0
opentune dsg groups 1 2 3 --driver gsusb --count 10 --interval-ms 500
opentune dsg identify --sim        # scripted responder, synthetic data
```

Raw-CAN adapters only (socketcan, slcan, gsusb); `transport=tp20` and
`tp20_addr=0x02` are implied, `--opt tp20_keepalive_ms=...` etc. are passed
through. The channel setup, keep-alive (`A3`) and teardown (`A8`) are the
core's `Tp20Channel`. `--session` (default `0x89`, `none` to skip) is started
first; a refusal is reported and the reads continue. When a read fails like a
lost session (the channel broke and was set up again without the unit's
session, a timeout, or NRC `0x80`/`0x7E`/`0x7F`) the session is restarted and
that read is tried once more; the group's `notes` say so. A plain refusal
(`0x31`, `0x22`, ...) is recorded, never retried or guessed around.

| Read | Request | What is decoded | Source / confidence |
|---|---|---|---|
| Identification | `1A 9B` | part number (12 chars), software version (4), 10 binary bytes (hex, **not decoded**), component text | VAG record layout from the public OpenHaldex-C6 Gen4 capture; not yet seen from an 02E |
| | `1A 91`, `1A 97` | hardware number, system name (ASCII) | ISO 14230-3 option numbers; whether the 02E answers them is unknown |
| Trouble codes | `18 02 FF 00` | 16-bit code = VAG 5-digit fault number; status bits 6..5 storage state (not stored / stored-not-present / pending / present), bit 7 warning lamp, bit 4 readiness; bits 3..0 raw | KWP2000 status layout as rnd-ash/ecu_diagnostics decodes it (see [`PRIOR-ART.md`](PRIOR-ART.md)); community confidence |
| Measuring groups | `21 <n>` | 3-byte cells `(formula, a, b)`, scaled with the shared VAG formula table (`opentune_core::protocol::vagmeas`, the six formulas OpenHaldex-C6 checked on a Gen4); any other formula stays raw | formulas community confidence; **meanings: none** |

Each identification option is optional: a refused one is listed and the
others still decode. The coding value is **not** extracted — where it sits in
the `0x9B` record is not publicly documented, so the extra bytes are shown
raw.

### Measuring groups: meanings are parked

Which DQ250 group carries clutch pressures, temperatures or gear state is
deliberately **not** encoded: there is no licence-compatible public source and
no capture from a real 02E yet, and the owner parked the question until one
exists. Every cell is therefore reported as `group`, 0-based `cell`, formula,
`a`, `b`, the formula-scaled value (or raw) and state **`unconfirmed`**.

The hook for adding meanings later is data, not code: a `kwp_measuring_block`
entry in `live[]` of [`ctrldefs/vag-dq250-02e.json`](../ctrldefs/vag-dq250-02e.json)
(format in [`CONTROLLER-DEFS.md`](CONTROLLER-DEFS.md#live)) labels that
`(group, cell)`, which `opentune dsg groups` then shows with state
`from-definition` and the definition id. `--ctrldefs <dir>` points at another
definition directory (e.g. a private one built from your own capture). Its
`live[]` is empty today, and a test keeps it empty until a capture backs an
entry. `--count`/`--interval-ms` repeat the read (the TP2.0 keep-alive holds
the channel between samples) to record a capture with `--json`.

### Validation

**Synthetic-tested.** `crates/opentune-dsg/tests/` drive the reader over the
core TP2.0 stack (`Tp20Channel` on the core `Tp20Ecu` simulator, block size 2,
idle timeout with keep-alive, absent unit) and end to end through the slcan
driver's TP2.0 channel, against a scripted DQ250 responder with a made-up
identity (`02E000000X`, "SIMULATED DSG") — they assert that only read services
reach the unit and that the channel is set up once and torn down with `A8`.
Not tried on a car or bench yet: the first real read is the step that confirms
address `0x02`, session `0x89` and the `0x9B` layout for the 02E.

## DQ250-MQB (0D9)

From the open flashing tools (VW_Flash, mqb-flash-rs, DQ250-MQB-Bench-Flash):
UDS on CAN `0x7E1`/`0x7E9`; a 0x180000-byte bin file with DRIVER (0x80E) at
`0x0`, CAL (0x20000) at `0x30000`, ASW (0x130000) at `0x50000`; box code at
CAL+`0x1FFC0`, software version at CAL+`0x1FFE0`; each of CAL/ASW ends with its
JAMCRC (`!crc32`, little-endian). Factory containers use a 256-byte
substitution cipher over LZSS. Full reads go through the SBOOT bench path. The
`dq250` checksum refuses to write until one block's stored JAMCRC validates on
the image in hand — it stays **synthetic-tested** until a real 0D9 read appears.

## Controller definitions

Each family with an ecudef also has a controller definition in
[`ctrldefs/`](../ctrldefs/) (`vag-dq250-02e`, `vag-dq250-mqb`, `vag-dq381`;
format in [`CONTROLLER-DEFS.md`](CONTROLLER-DEFS.md)) that links the ecudefs
and adds the diagnostic channels (for 02E both the KWP2000-on-ISO-TP and the
TP2.0 `0x02` option, neither confirmed on a car; the TP2.0 one is what
`opentune dsg` uses, synthetic-tested) and identify keys. A
repository test keeps their part numbers and addressing in step with the
ecudefs. `opentune controller match 02E300042M` finds the definition.

## Flashing (later)

OpenTune does not flash any DSG TCU yet. The plan is the shared UDS
module-flash path (request download / transfer data / checksum routine) once
that path is validated on hardware; for 02E the transport and programming
session still have to be established on a car. If the 02E answers VW TP2.0,
the raw-CAN drivers can already open a KWP2000 channel to it for reads
(`opentune dsg`, above; or `--opt transport=tp20 --opt tp20_addr=0x02`, see
[`DRIVERS.md` §3f](DRIVERS.md#3f-vw-tp20-on-the-raw-can-drivers-socketcan-slcan-gsusb)); not tried on a car yet. FRF/ODX containers for these
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
- [Forbes-Automotive/OpenHaldex-C6](https://github.com/Forbes-Automotive/OpenHaldex-C6)
  (MIT) — the VAG `1A 9B` record layout, session `0x89` and the measuring-cell
  formulas (from a Haldex Gen4; reused for the 02E read, unconfirmed there).
- [rnd-ash/ecu_diagnostics](https://github.com/rnd-ash/ecu_diagnostics) — the
  KWP2000 DTC status-byte layout (storage state, warning lamp, readiness).
- [dspl1236/vag-tcu-tools](https://github.com/dspl1236/vag-tcu-tools) (MIT) —
  DQ381 CRC-32 (earlier work), FRF method `0x11` for DQ200/DQ250/DL382.
- The 02E layout, checksum and maps: OpenTune's own analysis of real reads.
  No commercial tool output, A2L/DAMOS or map pack was used; the reads are
  private and never committed.
