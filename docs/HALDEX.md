# Haldex AWD

BorgWarner (formerly Haldex Traction) AWD couplings on VAG 4Motion / quattro cars:
what is publicly known, what OpenTune does with them, and how far each part is
validated. Crate: [`opentune-haldex`](../crates/opentune-haldex); CLI:
`opentune haldex`.

**Status in one line:** passive CAN decode and the Gen4 identification/measuring
decode are checked against **public real captures**; the Gen4 read and the
OpenHaldex client are **synthetic-tested** (simulated units); nothing is
vehicle-tested yet; there is **no Gen4 checksum** and **no flashing**.

## Generations at a glance

| Generation | Cars (examples) | Controller | Diagnostics | Status frame | Lock request |
|---|---|---|---|---|---|
| Gen1/Gen2 | Golf Mk4 / Mk5 R32, A3 8P | `02D`, `7H0` / `1K0 907 554` | K-line (Gen1); KWP2000 over TP2.0 (Gen2) | `Allrad_1` 0x2C0 | `Bremse_4` 0x2A0 |
| **Gen4** | **Passat CC 4Motion**, Passat 4Motion (2009-), Tiguan, Golf Mk6 R, Yeti, Superb 4x4, Sharan/Alhambra, Q3, TT/TTS 8J, A3/S3 8P facelift | `0AY 907 554 C`, `0BS 907 554` (J492) | **KWP2000 over VW TP2.0, logical address 0x0A** | `Allrad_1` 0x2C0 | `Bremse_4` 0x2A0 |
| Gen5 (PQ) | some PQ cars | `0AY 907 554 F` | UDS (OpenHaldex) | `Allrad_1` 0x2C0 | — |
| Gen5 (MQB) | Golf 7 R, S3 8V, Tiguan AD, TT 8S | `0CQ 907 554` | UDS on `0x70F`/`0x779` | `Allrad_03` 0x118 | `ESP_14` 0x08A |

Sources: Dark Side Developments' Gen4 application list (Passat/CC 4Motion 2009,
coupling codes 0AY/0BR/0BY); OpenHaldex-C6 README ("Supported Platforms",
"Live Diagnostics", "Fault Codes") and source comments; opendbc `vw_pq.dbc`;
public part listings for `0BS 907 554` on the Passat CC (community). Identify a
unit by its **full part number including the suffix**: `0AY 907 554 C` is a
Gen4, `0AY 907 554 F` is a different unit that OpenHaldex treats as Gen5 (PQ).

## Haldex Gen4 (the owner's Passat CC)

### What is known

- **Diagnostics:** KWP2000 tunnelled over VW TP2.0. Channel setup on the
  broadcast id `0x200` to logical address **`0x0A`**; the unit answers on
  `0x20A` and hands the tester its data-channel id. Session `0x10 0x89`,
  identification `0x1A 0x9B`, trouble codes `0x18 02 FF 00`, measuring blocks
  `0x21 <group>`. All of this is visible in the public capture of an
  `0AY 907 554 C` in OpenHaldex-C6 (`documents/vehicle-logs/vw-pq/`), which
  identifies as `0AY907554C  0041 … Haldex 4Motion`. Scan tools show this module
  as address word 22 ("AWD"); OpenTune only uses the TP2.0 logical address.
- **Measuring groups** (OpenHaldex-C6, checked by its authors against a scan
  tool; community confidence): group 1 = oil temperature, clutch plate
  temperature, supply voltage; group 3 = oil pressure, estimated torque,
  clutch valve duty, clutch valve current. Formulas decoded: `0x06` V, `0x0E`
  bar, `0x18` A, `0x1A` °C, `0x21` %, `0x5E` Nm. Any other formula is shown
  raw. The decode matches the scan-tool values recorded in the public capture
  (27 / 5 / 4 bar, 2032 / 464 / 384 Nm, 40 / 21 / 18 %, 1.910 / 0.860 / 0.81 A)
  — see `gen4::tests::decodes_public_real_capture`.
- **CAN behaviour** (drivetrain CAN, 500 kbit/s): the ABS sends the lock
  request in `Bremse_4` 0x2A0 byte 0 (`0x7F`/`0x80` open, `0x80 + n` linear,
  `0xFE` full); the unit reports in `Allrad_1` 0x2C0 — status flags in byte 0,
  applied lock in byte 1 (same lever scale), fault-stored bit in byte 3 bit 7,
  and on Gen4 its temperature in byte 4 (`raw - 60` °C, `0xFF` invalid). The
  DBC names come from opendbc `vw_pq.dbc`; the Gen4 byte-4 temperature and the
  lever scale come from OpenHaldex-C6, which derived some meanings from its own
  analysis of the unit's firmware — community confidence. On the public capture
  the decoded temperature tracks the bench heating (23-56 °C).
- **Unknown:** the MCU, flash size and layout, any flash/read procedure, and the
  checksum. **No Gen4 dump** was found in the project's dump library (a
  filename and content search for Haldex / `0AY 907 554` / `0CQ 907 554` /
  `0BR` found none on 2026-11-18), and no public source describes one.
- **Connector:** OpenHaldex documents an 8-pin `1J0 973 814` in-line plug
  (terminal 15, ground, K-line, Haldex CAN-H/L) but its README also mentions a
  6-pin `1J0 973 713` for other generations. Treat the pinout as **to be
  checked on the car** before wiring anything (`database/units/haldex-gen4.json`).

### What OpenTune does

- `opentune haldex gen4` — a **read-only** pass: session, identification,
  stored codes, measuring groups (default 1, 3, 125, 126). It only sends
  `0x10`, `0x1A`, `0x18`, `0x21`: never clears codes, writes coding, adapts,
  runs basic settings or flashes. VAG fault numbers are printed as the 5-digit
  number; there is **no VAG fault-text decode** yet.
- **Adapter gap:** no shipped OpenTune driver opens a TP2.0 channel yet. The
  ELM327 drivers refuse TP2.0 (and many ELM327 clones cannot hold a TP2.0
  channel reliably anyway: it needs raw frames, per-block ACKs within ~100 ms
  and keep-alives); socketcan / slcan / gs_usb build ISO-TP only. Until a raw-CAN
  driver gets TP2.0 wiring (`Protocol::Vwtp20` -> `VwTp20` over its `CanLink`),
  `opentune haldex gen4 --driver …` refuses with that explanation; `--sim`
  runs the whole stack (real `VwTp20` + KWP2000 client) against a simulated unit.
- `opentune haldex decode <log>` — passive decode of candump / OpenTune CSV /
  SavvyCAN CSV captures (works on the public OpenHaldex logs).
- `ecudefs/borgwarner-haldex-gen4.json` — identify keys only (`0AY907554C`,
  `0BS907554`), protocol `vwtp20` / `kwp2000`, flash size unknown, no maps.
- Auto-scan cannot reach a Gen4: its module-list schema speaks UDS/OBD only.
  The `vag-modules` pack's Haldex entry is the MQB Gen5 (`0x70F`/`0x779`).

### Checksum

| Unit | Id | State |
|---|---|---|
| Gen4 | — | **None.** No dump, no public description. `checksum: null`; nothing is claimed or written. |
| Gen5 | `haldex` (`haldex-gen5`) | Per-block 16-bit word-sum from VW_Flash (BSD-2): **synthetic-tested only**. No real Gen5 dump was found in the library either, so it stays unvalidated. |

The Gen5 word-sum is **not** assumed to apply to Gen4.

## OpenHaldex

[OpenHaldex](https://github.com/Forbes-Automotive/OpenHaldex-C6) is an
open-source controller that sits between the car and the Haldex and rewrites the
lock request. **Licences checked 2026-11-18:** OpenHaldex-C6 is **MIT** (its
`LICENSE.md` is the MIT text plus a note pointing at `THIRD_PARTY_NOTICES.md`,
which is why GitHub shows "Other"); OpenHaldex-S3 (`dandwyer`, `meatro`) MIT;
OpenHaldexDue MIT; OpenHaldexApp MIT; OpenHaldexT4 has no licence file (facts
only). OpenTune's code is written fresh from the documented behaviour; nothing
was copied.

### Interfaces it exposes (and what OpenTune uses)

| Interface | Documented in | OpenTune |
|---|---|---|
| Modes: Stock, FWD, 50:50 (100 % lock), 60:40 (40 %), 75:25 (25 %), Expert (own map) | README "Modes" | `openhaldex::Mode` |
| CAN `0x6B0` state broadcast; `0x6A0` mode command (byte 0 = 1..6), both only with *Broadcast over CAN* on | README "CAN Broadcast / Change Mode" | decode both; `command_frame()` encodes `0x6A0` (no CLI sender: no driver exposes raw transmit) |
| UDS on `0x70F`/`0x779`: RDBI `0xFDA0` `[commanded %, engagement raw, mode]`, RDBI `0xFDA1` slip `[FL FR RL RR]` (i8, -128 = none), WDBI `0xFDA2` `[mode 0..5]` | `include/OpenHaldexC6_canID.h` | `OpenHaldexUds` (read + confirmed write) |
| BLE GATT (Mode / Controller / Status / Settings / Diag) | `documents/MOBILE_APP_OPENHALDEX.md` | `BleStatus::decode` (17-byte Status) |
| Wi-Fi web UI / HTTP (`/api/dashboard`, `/api/mode`) | source only, no contract | not used |

The UDS DIDs reach the unit from the OBD port only where the gateway routes
`0x70F` to the drivetrain CAN (MQB). On PQ cars put the adapter on the drivetrain
CAN, or use BLE / Wi-Fi.

### CLI

```sh
opentune haldex decode capture.log [--bus pq|mqb] [--frames] [--json]
opentune haldex gen4 --sim [--groups 1,3] [--no-dtcs] [--json]
opentune haldex status --driver socketcan --port can0 [--via can|uds] [--duration 3]
opentune haldex mode list
opentune haldex mode set 6040 --driver socketcan --port can0 --confirm
```

`status` is read-only (`--via can` listens, listen-only where the adapter allows;
`--via uds` sends only RDBI). `mode set` is a **write**: it refuses without
`--confirm` (and prints the exact request it would send), then reads `0xFDA0`
back and fails if the unit does not report the requested mode (a disabled
controller ignores mode changes).

### Backend API for a desktop panel

The panel (after the desktop split) should call these, all in
`opentune_haldex`, and add nothing protocol-specific of its own:

| Panel action | Call | Notes |
|---|---|---|
| Mode buttons (list) | `openhaldex::Mode::ALL`, `Mode::nominal_lock_pct()`, `Mode::as_str()` | offline |
| Live status over UDS | `OpenHaldexUds::new(&mut CoreDiag).read_lock()` / `.read_slip()` | driver opened with `tx_id 0x70F`, `rx_id 0x779`; poll ≤ 5 Hz; read-only |
| Set mode | `OpenHaldexUds::set_mode(mode)` -> `LockStatus` | tier `bench`; show a confirm dialog; display the returned (read-back) mode, not the requested one |
| Live status over CAN | `CanMonitor` + `can::decode_frame(id, ext, data, Bus::Auto)` | filter `0x6B0, 0x2C0, 0x2A0, 0x118`; `HaldexFrame` is `Serialize` |
| Log viewer | `canlog::parse_log(text)` + `canlog::summarize(&log, bus)` | returns `HaldexSummary` + decoded frames |
| Gen4 read | `gen4::read_gen4(&mut dyn DiagTransport, &Gen4ReadOptions)` -> `Gen4Report` | needs a TP2.0 transport (driver gap above) |
| BLE (mobile) | `openhaldex::BleStatus::decode(bytes)` | 17-byte Status, layout 1 |

Per the sidebar tier rule, a panel that talks to the car stays tier `bench`
until vehicle-tested; the log decoder may be `public`.

## Safety

- Changing the AWD lock changes how the car drives. Only change modes with the
  car stationary or in a safe place, and know how to return to **Stock**.
- Mode changes need `--confirm`; OpenTune never writes to the Haldex controller
  itself (no coding, adaptation, basic settings, code clearing or flashing).
- Bench work: see the pinout and bench-safety notes in
  `database/units/haldex-gen4.json` — and check the pinout on your own unit.

## Validation states

| Part | State |
|---|---|
| `Allrad_1` / `Bremse_4` / `Bremse_3` / `Motor_1` decode | `Allrad_1` checked on two public real Gen4 captures; others synthetic-tested |
| `Allrad_03` / `ESP_19` (MQB) decode | synthetic-tested |
| Log readers (candump, OpenTune CSV, SavvyCAN CSV) | synthetic-tested; SavvyCAN read on the public OpenHaldex captures |
| Gen4 identification + measuring-block decode | checked against public real capture bytes |
| Gen4 read engine over TP2.0 | synthetic-tested (`VwTp20` + simulated unit) |
| KWP2000 `0x21` / `0x18` / `0x14` client services | synthetic-tested |
| OpenHaldex UDS client | synthetic-tested (against `opentune-udssim` and a simulator) |
| Gen4 checksum | none |
| Gen5 `haldex` word-sum | synthetic-tested only |

The first real Gen4 read is planned in [`FIRST-CAR-TEST.md`](FIRST-CAR-TEST.md#stage-h--haldex-gen4-read-read-only).
