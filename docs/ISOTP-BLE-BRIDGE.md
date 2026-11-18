# ESP32 ISO-TP BLE bridge — host protocol note

This note describes the host-side protocol of the open-source
**esp32-isotp-ble-bridge** firmware, as implemented by OpenTune's `isotp-ble`
driver ([`crates/drivers/isotp-ble-bridge`](../crates/drivers/isotp-ble-bridge/)).
It is written in our own words from the firmware's public source and README;
no firmware code is copied into this repository. How the firmware relates to
OpenTune's own ESP32-S3 firmware, and how to flash it, is in
[`hardware.md`](hardware.md#companion-ble-firmware-esp32-isotp-ble-bridge).

> **Validation state: unverified.** The driver is tested against an
> in-process simulation of the firmware (`mock.rs`) with an independent UDS
> server as the ECU. It has not been run against a real bridge or a car.

## Source, licence and attribution

| | |
|---|---|
| Project | [Switchleg1/esp32-isotp-ble-bridge](https://github.com/Switchleg1/esp32-isotp-ble-bridge), branch `BridgeLEG` |
| Pinned | tag `v1.03`, commit `276e97c04881a7bc1ac71a39e0ffe97cdfd144ae` (2024-06-30). The branch head `e7c95845fcafa76fc74641316fb33e1504ddf121` differs only in its README. |
| Licence | **MIT**, © 2021 Brian Ledbetter; bundles [lishen2/isotp-c](https://github.com/lishen2/isotp-c) (MIT, © 2019 Shen Li). GitHub shows "Other" because the LICENSE file carries both notices. MIT is compatible with OpenTune's GPL-3.0-or-later. |
| Original | [bri3d/esp32-isotp-ble-bridge](https://github.com/bri3d/esp32-isotp-ble-bridge) (same MIT licence). Switchleg1's repository is a GitHub fork of it. |
| Used by | SimosTools (Android) and [bri3d/VW_Flash](https://github.com/bri3d/VW_Flash) (BSD-2-Clause; its `usb_isotp_connection.py` speaks the same frame format over USB). |

**Which fork.** bri3d's repository is the original, but its `main` branch was
last changed in December 2021 and speaks an older, different framing (4-byte
ids written straight to the data characteristic, commands on a separate
characteristic). Switchleg1's fork is the maintained one: tagged releases
v0.1 … v1.03 (2022–2024), the protocol below, and the one SimosTools and
VW_Flash use today. OpenTune targets Switchleg1 v1.03.

## The device

* **MCU:** classic ESP32 (`CONFIG_IDF_TARGET="esp32"`, 2 MB flash), Bluedroid
  BLE stack; built with ESP-IDF 4.4 (it uses the v4 task-watchdog API).
  Reference hardware is the Macchina A0 or the DIY "AMAleg" clone.
* **CAN:** the ESP32 TWAI controller at a fixed **500 kbit/s**, 11-bit ids,
  accept-all filter; transceiver on TX = GPIO 5, RX = GPIO 4, transceiver
  standby pin GPIO 21 held low.
* **ISO-TP:** done on the device (isotp-c): padding byte `0xAA`, block size 8
  and STmin 0 in the flow control it sends, 100 ms N_Bs/N_Cr timeouts.
* **Links.** The device serves only a fixed table of tester/ECU id pairs;
  a frame for any other pair is dropped silently:

  | name | tester → ECU | ECU → tester | largest message |
  |---|---|---|---|
  | ecu | `0x7E0` | `0x7E8` | 4095 bytes (4096-byte buffer) |
  | tcu | `0x7E1` | `0x7E9` | 4095 bytes |
  | haldex | `0x7E5` | `0x7ED` | 4095 bytes |
  | dtc | `0x700` | `0x7E8` | 511 bytes |

  So it reaches UDS-on-CAN modules only: no VW TP2.0 (the Passat CC's Gen4
  Haldex, older VAG modules), no K-line, no raw CAN capture, no 29-bit ids.

## BLE GATT

| UUID (16-bit) | Role | Properties |
|---|---|---|
| `0xABF0` | primary service | advertised in the advertising data |
| `0xABF1` | host → device frames | write without response (value ≤ 512 bytes; long writes also accepted) |
| `0xABF2` | device → host frames | notify; the host must write `01 00` to its CCC |
| `0xABF3` | "command" | writable, ignored by the firmware |
| `0xABF4` | "status" | notify, never sent |

The 16-bit UUIDs expand to `0000xxxx-0000-1000-8000-00805F9B34FB`. The device
advertises as **`BLE_TO_ISOTP20`** unless renamed (setting 8, stored in NVS).
It uses the ATT MTU the central negotiates (default 23); every value it sends
is at most MTU − 3 bytes.

## Frames

Everything — ISO-TP payloads, settings, persist commands — travels as frames
of an 8-byte little-endian header plus `len` bytes:

| offset | size | field | meaning |
|---:|---:|---|---|
| 0 | 1 | `0xF1` | header id |
| 1 | 1 | flags | `0` for ISO-TP data; see below |
| 2 | 2 | rx id | the CAN id on the other side of this payload |
| 4 | 2 | tx id | the CAN id that carries (or carried) this payload |
| 6 | 2 | len | payload length |

A UDS request to the engine ECU, `22 F1 90`:

```
F1 00 E8 07 E0 07 03 00 22 F1 90
      ^^^^^ ^^^^^ ^^^^^
      rx    tx    len      (tx 0x7E0 = request id, rx 0x7E8 = answer id)
```

The ECU's answer comes back with the ids swapped (tx `0x7E8`, rx `0x7E0`), so
in both directions *tx id* is the CAN id of the bytes in the frame. Several
frames may be concatenated in one BLE value; the device packs queued answers
into one notification while they fit ("multisend").

### Split frames (BLE)

A frame longer than one ATT value is sent in pieces:

1. the first MTU − 3 bytes of the encoded frame, with flag `0x08` OR-ed into
   the flags byte;
2. then `F2 <seq> <data…>` pieces of up to MTU − 5 data bytes, `seq` counting
   from 1 and wrapping as a byte.

The receiver appends pieces until it holds `len` payload bytes; a piece out of
order, or any value that does not start with `F2`, abandons the frame. The
scheme is the same in both directions. A split is only recognised when the
first piece's header announces more bytes than that piece carries.

Because the split flag is OR-ed into the flags byte, a **settings** frame must
not be split unless its setting number already has bit 3 set (only setting 8,
the GAP name, does): the device would otherwise file it under the wrong
setting. The driver refuses such a frame at a too-small MTU.

### Flags

| bit | name | meaning (host → device) |
|---|---|---|
| `0x01` | persist enable | start persist mode |
| `0x02` | persist clear | clear the persist list |
| `0x04` | persist add | add this payload to its link's persist list |
| `0x08` | split | first piece of a split frame |
| `0x40` | settings get | with `0x80`: read a setting (empty payload) |
| `0x80` | settings | low bits = setting number; payload = new value |

**Quirk:** the BLE send path never copies flags into the frames it notifies:
every notified frame has flags `0` (plus `0x08` on the first piece of a split
frame). Over the serial port the flags are kept. A settings reply over BLE is
therefore recognised by its ids only (`0/0` for global settings, the link's
ids for STmin — the same ids as ECU traffic, so read STmin while the link is
idle).

### Settings

| # | setting | value | scope | kept over reboot |
|---|---|---|---|---|
| 1 | ISO-TP STmin override for the frames the device sends | `u16` µs (0 = obey the ECU) | per link (frame ids select it) | no |
| 2 | status LED colour | `u32` `0x00RRGGBB` | global | no |
| 3 | persist message delay | `u16` ms | global | no |
| 4 | persist per-queued-message delay | `u16` ms | global | no |
| 5 | BLE delay after each notification burst | `u16` FreeRTOS ticks | global | no |
| 6 | BLE wait for more answers to pack | `u16` ticks | global | no |
| 7 | password | string ≤ 64 | global | yes (NVS) |
| 8 | advertised name | ASCII ≤ 14 | global | yes (NVS, re-advertises) |

A set is never acknowledged; a value of the wrong length is ignored. Settings
1–6 and 8 can be read back. The password check is compiled out of the
published firmware (`PASSWORD_CHECK` is not defined), so setting 7 does
nothing there.

### Persist mode

For logging: the host queues requests with *persist add* frames (up to 64 per
link, ECU and TCU links only), then sets *persist enable*. The device replays
each link's list round-robin, sending the next request once the previous
answer arrived, spaced by setting 3 plus setting 4 × the notifications
waiting. Answers carry a **millisecond timestamp instead of ids**: rx id = the
high 16 bits, tx id = the low 16 bits. While persist mode is on the device
accepts only settings and a frame without the enable flag, which ends it
(with flag `0x02` it also clears the list). Persist state is cleared on every
connect and disconnect. The OpenTune driver encodes these frames
(`Packet::persist_*`) but does not drive a logging session with them yet.

### What the device does not tell you

There is no acknowledgement or error frame. Unknown id pairs, payloads larger
than the link buffer, a full queue and ISO-TP failures (an ECU that never
sends its flow control or consecutive frames) all produce silence; the host's
own timeout is the only signal. The driver therefore refuses unknown id pairs
at `open` (option `any_ids=1` overrides that for a firmware with a different
table) and caps a download block at 4095 bytes.

## USB serial variant

The same frames run over the board's USB-UART (UART0, **250000 baud 8N1**),
without the split scheme: the device reads a header, then waits for `len`
bytes (a frame must complete within about 1 s). The device listens there only
while no BLE central is connected; the first valid frame claims the serial
side for 120 s of inactivity and stops BLE advertising. Common ESP32 boards
wire DTR/RTS to reset/boot, so hold both lines at the same level when
opening the port (VW_Flash does the same).

## OpenTune driver (`isotp-ble`)

* `codec.rs` — the frame format, split/reassembly, settings and persist
  frames; std only, no dependencies, so the Android app can reuse it.
* `link.rs` — the `Link` byte-transport trait and the serial link.
* `bridge.rs` — a session: sends frames (split by MTU), reassembles
  notifications, matches an answer to its request (right CAN id, positive
  response to the same service or a negative response naming it), skips stale
  answers and drains `7F xx 78` responsePending frames.
* `ble.rs` — the BLE link on `btleplug`, behind the optional `btleplug` cargo
  feature (off by default: default builds and CI pull no BLE/D-Bus crates).
* `mock.rs` — the simulated firmware used by the tests.

The driver implements `Interface`: `diag()` hands out the bridge as an
ISO-TP-level `DiagTransport` for `UdsClient` and the service engines;
`read_ecu_id`, block read (RequestUpload) and block write (RequestDownload)
run UDS over it, with the optional seed/key hook. Connect options:

| option | meaning | default |
|---|---|---|
| `transport` | `serial` or `ble` | `serial` when `port` is set, else `ble` |
| `port` | serial device | — |
| `baud` | serial baud | `250000` |
| `name` | BLE advertised name to connect to | `BLE_TO_ISOTP20` |
| `address` | BLE address / platform id (wins over `name`) | — |
| `scan_ms` | BLE scan time | `5000` |
| `att_mtu` | ATT MTU to assume instead of the negotiated one | negotiated |
| `stmin_us` | send setting 1 for the link on open | not sent |
| `any_ids` | allow id pairs outside the firmware's table | off |
| `seedkey`, `security`, `seedkey_level` | security access before block read/write, as in the other drivers | — |

`ConnectConfig` must say ISO-TP at 500 kbit/s with 11-bit ids from the table
above (default 7E0/7E8). Example: `opentune info --driver isotp-ble --port
/dev/ttyUSB0`.
