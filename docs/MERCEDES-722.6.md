# Mercedes 722.6 (NAG1) — EGS51

The 722.6 five-speed automatic (Mercedes "5G-Tronic", Chrysler "NAG1") with
its external Siemens TCU (EGS). Definition:
[`ctrldefs/mercedes-egs5x-7226.json`](../ctrldefs/mercedes-egs5x-7226.json)
(format: [CONTROLLER-DEFS.md](CONTROLLER-DEFS.md)); bench pinout:
`database/units/mercedes-egs52-7226.json` (pinout only).

**Status in one line:** passive CAN decode of the pre-2000 **EGS / EGS51**
frames (TCU ↔ engine / ABS / selector / climate), the Ultimate-NAG52 map
structures and the public gear ratios — all **synthetic-tested** (frames
built from the published layout). Nothing has been checked against a real
car yet. OpenTune does not transmit to this TCU, read or write its flash, or
know its checksum.

**EGS52 is deliberately not included.** The frame layout of the 2000+ CAN-C
cars (EGS52) is not part of OpenTune. It will be re-derived from a
listen-only capture of the owner's own car and added only from that
evidence.

## EGS51 or EGS52? Telling them apart from a capture

A W210 E-Class (1996-2002) can have either generation, depending on build
year and TCU. Both buses run at 500 kbit/s. Capture the bus (see below) and
look at which ids are present and how they behave:

| Clue | EGS / EGS51 car | EGS52 car |
|---|---|---|
| Engine torque frame | `0x310` present | `0x310` absent; engine torque frames at `0x212` and `0x312` instead |
| ESP torque-request frame | — | `0x300` present (from the ESP) |
| TCU speed frame | — | `0x338` present (from the TCU) |
| TCU gear frame `0x218` | follows the EGS51 coding below when you move the lever P → R → N → D | does not follow it |

EGS51 gear coding in `0x218` (`GZC` target, `GIC` actual): N = 0, 1..5 =
D1..D5, R = 6, R2 = 7, P = 8, 15 = not available.

`opentune controller decode 722.6 capture.log` decodes the EGS51 set; on an
EGS52 car it will decode the ids the two generations share, but those values
are not validated for EGS52 and must not be trusted.

EGS53 (later 722.6 / 722.9 cars) is not covered either.

## Frames (EGS51 set)

TCU = frames the EGS sends; the rest are what it listens to. Signal names
are the upstream names (German abbreviations) so they can be cross-checked;
the definition carries an English note for each.

| Id | Frame | From | What OpenTune decodes | Scaling source |
|---|---|---|---|---|
| `0x200` | `BS_200` | ESP/ABS | brake switch `BLS`, front wheel speeds + directions, counter | upstream code (rear), front assumed same |
| `0x208` | `BS_208` | ESP/ABS | rear wheel speeds `DHL`/`DHR` (½ × raw rpm, `0x3FFF` n/a) + directions, ESP brake intervention | upstream `wheel_*_2x_rpm` |
| `0x210` | `MS_210` | engine | pedal (raw × 0.4 %), ESP torque (raw × 3 Nm) | upstream code |
| `0x308` | `MS_308` | engine | engine speed `NMOT` (rpm), oil temperature (raw − 40 °C), TCC open request, starter | upstream code |
| `0x310` | `MS_310` | engine | indicated / drag / max / min torque (raw × 3 Nm), max-torque factor | upstream code |
| `0x608` | `MS_608` | engine | coolant and intake temperature (raw − 40 °C), fuel flow (raw × 0.868 µl/250 ms) | upstream code |
| `0x230` | `EWM_230` | selector | lever position `WHC` (D, N, R, P, +, −, intermediates), kickdown, programme switch/button, shift lock | upstream code |
| `0x410` | `KLA_410` | climate | A/C compressor torque (raw/4 Nm), compressor on | upstream code |
| `0x218` | `GS_218` | TCU | torque request (raw × 3 Nm, `0xFE` = none), request enable, gear (coding above), TCC open/slip/closed, big/small box, limp, shift active, kickdown, start enable | upstream code |
| `0x418` | `GS_418` | TCU | display gear and programme letters, ATF temperature (raw − 50 °C), target/actual gear, lever position, gearbox variant, loss torque (raw/4 Nm), 4WD/FWD flags | upstream code |

Upstream's EGS51 layer does not fill `GS_418` `T_GET` (it is the
replacement TCU's encoding), so on a car with the original EGS51 that byte
may mean something else until a capture shows otherwise.

The definition's **live channels** pick the useful ones (engine speed,
ATF temperature, pedal, torques, gears, lever, TCC state, limp);
`opentune controller decode` reports their ranges.

### Bit convention

Upstream's `can_data.txt` gives `OFFSET` counted from the most-significant
bit of the frame read as a big-endian 64-bit word. That is a Motorola signal;
the definition stores the DBC start bit
`(OFFSET / 8) * 8 + (7 - OFFSET % 8)` with `byte_order: big`. The tests in
`crates/opentune-ctrldef/tests/mercedes_7226.rs` build their frames the
upstream way, independently of that conversion.

## Gear ratios

From the public ratio table (facts; the 2004 large-NAG row matches the
ratio table in ultimate-nag52-fw-rs `egs_pc_sim`):

| Variant | 1 | 2 | 3 | 4 | 5 | R1 | R2 |
|---|---|---|---|---|---|---|---|
| small NAG W5A 280/300/330 (1996) | 3.932 | 2.408 | 1.486 | 1.000 | 0.830 | −3.100 | −1.899 |
| large NAG W5A 400/580/900 (1996) | 3.588 | 2.186 | 1.405 | 1.000 | 0.831 | −3.160 | −1.926 |
| small NAG W5A 330 (2004) | 3.951 | 2.423 | 1.486 | 1.000 | 0.833 | −3.147 | −1.930 |
| large NAG W5A 580 (2004) | 3.595 | 2.186 | 1.405 | 1.000 | 0.831 | −3.167 | −1.926 |

`GS_418` `MECH` and `GS_218` `GEARBOX_BIG` report large/small.

## Map structures (Ultimate-NAG52 firmware)

These are the calibration tables of the **open Ultimate-NAG52 replacement
TCU** — their shape and axes, as `structure_only` tables. They are not the
stock EGS flash layout (not publicly documented), and no values are shipped.

| Table | Size | X axis | Y axis | Cells | Profiles |
|---|---|---|---|---|---|
| upshift schedule | 4 × 11 | pedal 0..100 % (10 % steps) | 1-2, 2-3, 3-4, 4-5 | input-shaft rpm | S, C, A, M (× petrol/diesel) |
| downshift schedule | 4 × 11 | pedal 0..100 % | 2-1, 3-2, 4-3, 5-4 | input-shaft rpm | S, C, A, M |
| upshift / downshift time | 5 × 6 | pedal 0..100 % (20 % steps) | 5 input-speed points, 1000 rpm .. redline | target overlap ms | S, C, A, M, W, R |
| clutch fill time | 5 × 4 | ATF −20, 5, 25, 60 °C | K1, K2, K3, B1, B2 | ms | small / large NAG |
| clutch fill pressure | 1 × 6 | K1, K2, K3, B1, B2, B3 | — | mbar | |
| clutch low fill pressure | 1 × 5 | K1, K2, K3, B1, B2 | — | mbar | |
| TCC PWM | 5 × 7 | TCC pressure 0..15000 mbar | ATF 0..120 °C | duty /4096 | |
| TCC slip target | 8 × 11 | pedal 0..100 % | input 1000..6000 rpm | slip rpm (≤ 10 closed, > 100 open) | |
| TCC pressure adaptation | 6 × 6 | engine load 0..100 % | ATF −20..120 °C | mbar | one per gear 1-5 |
| shift torque adaptation (pulling / pushing) | 5 × 4 / 4 × 4 | input 1000..4000 rpm | load % | Nm | one per shift |
| prefill time / shift pressure offset adaptation | 1 × 8 | 1-2 .. 2-1 | — | ms / mbar | |

The adaptation entries point at these tables; the stock EGS adaptation
services are not public, so no service is declared.

## Diagnostics

Pre-2000 units are diagnosed on the K-line (C1 pin 1, database unit) with a
protocol the cited sources do not describe, so no diagnostic channel is
declared.

## Capturing your own car (planned, read-only)

1. Find the powertrain CAN: the `C2` connector `H`/`L` pins at the TCU
   (database unit, community confidence) or the car's wiring diagram. Whether
   it reaches the OBD socket on a given car is to be checked, not assumed.
2. Use a raw-CAN adapter in **listen-only** mode at 500 kbit/s, ignition on,
   car parked:
   `opentune candump --driver slcan --port /dev/ttyACM0 --out w210.log`
   (see [LIVE-LOGGING.md](LIVE-LOGGING.md) for the capture options).
3. Move the selector P → R → N → D, start the engine, let it warm.
4. Check the generation with the table above, then
   `opentune controller decode 722.6 w210.log` (ATF temperature plausible,
   `GIC` follows the lever, engine speed matches the tachometer).

On an EGS51 car, the frames the capture confirms can move to
`real-image-tested` in the definition (per-frame `validation`). On an EGS52
car, the capture is the evidence an EGS52 set will be derived from.

## Validation states

| Part | State |
|---|---|
| EGS51 frame layouts and scaling | synthetic-tested (frames built from the published layout) |
| Live channels, `opentune controller decode` | synthetic-tested |
| EGS52 frames | not included (to be derived from an own capture) |
| Map structures | structure only (documented upstream; no values) |
| Gear ratios | public facts |
| Diagnostics, flash, checksum, adaptations on the stock EGS | none |

## Safety

- The TCU sets line, shift and lock-up pressures; bad maps or adaptations
  burn clutches and can make the car move unexpectedly or lose drive.
- OpenTune only listens. Never replay captured frames onto a running car:
  `0x218` carries the TCU's engine-torque request and the engine acts on it.
- Wire into the bus with the engine off; capture listen-only.

## Sources and provenance

- [rnd-ash/ultimate-nag52-fw](https://github.com/rnd-ash/ultimate-nag52-fw)
  (GPL-3.0, checked 2026-11-18) — its hand-made EGS51 CAN matrix
  (`lib/egs51_ecus/can_data.txt`) and the scaling in its CAN layers, and the
  map structures and axes. Layouts are transcribed into data; no code was
  copied.
- [rnd-ash/ultimate-nag52-fw-rs](https://github.com/rnd-ash/ultimate-nag52-fw-rs)
  (GPL-3.0) — a cross-check copy of the EGS51 matrix and the large-NAG
  ratio table.
- Gear ratios: the public 5G-Tronic ratio table (Wikipedia, facts).
- Not used: any OEM CAN-matrix-derived layout, DAS / Xentry / CBF content,
  EGS calibration data, seed/key.

## Offline EGS52 coding CRC

The 42-byte variant coding record belongs to the coding engine and its
`opentune coding` CLI. Its CRC covers the first 40 bytes and occupies the
last two bytes, least significant byte first. The EGS52 flash checksum
algorithm remains unknown.

    opentune coding egs52-crc coding.bin
    opentune coding egs52-crc coding.bin --out candidate.bin --allow-unconfirmed

Verify reports stored and calculated values and exits unsuccessfully on a
mismatch. Recompute creates a new 42-byte binary file. Both operate offline.
The Rust helper accepts the polynomial, initial value, input/output reflection
and final XOR. The CLI exposes these as `--poly`, `--init`, `--refin true|false`,
`--refout true|false` and `--xorout`. It defaults to the CRC-16/ARC candidate (normal polynomial
`0x8005`, reflected `0xa001`, initial value and final XOR zero, both
reflections enabled). Status **implemented** for EGS52, unconfirmed on a real
EGS. The standard check vector and synthetic records exercise the calculation.
[Sources and licences](PRIOR-ART.md#egs52-groundwork).
