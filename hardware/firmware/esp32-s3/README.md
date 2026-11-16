# OpenTune ESP32-S3 interface firmware

The device side of OpenTune's own open interface (`docs/hardware.md`, roadmap
M8 / Phase 3). An ESP32-S3 runs the timing-critical transports — **CAN** (TWAI),
**ISO-TP** over CAN, and **K-line** (ISO 9141 / ISO 14230) — and presents a
**framed USB-CDC command protocol** to the host. The host-side Rust driver
`crates/drivers/esp32-opentune` speaks this protocol and implements
`opentune-core`'s `Interface` trait.

Toolchain: **ESP-IDF** (v5.1+). Chosen over Arduino-ESP32 for the native TWAI
driver, the built-in USB Serial/JTAG CDC (no external USB chip), and the oneshot
ADC with calibration.

> **Build status (honest): NOT compiled here.** This was developed offline with
> no ESP-IDF toolchain and no board. The code targets the documented ESP-IDF 5.x
> APIs and is structured to build with `idf.py`, but it has not been compiled or
> run on hardware. Nothing is tested against a real ECU or car. Treat the init
> timings, ADC divider scaling, and pin map as starting points to verify on the
> bench. See "Build & flash" below for the exact steps.

## Files

| File | Role |
|---|---|
| `main/wire_protocol.h/.c` | USB-CDC frame format, command IDs, status codes, CRC-8. |
| `main/usb_cdc.h/.c` | Host link over the ESP32-S3 USB Serial/JTAG controller. |
| `main/twai_can.h/.c` | Raw CAN via the TWAI peripheral (bitrate, send/recv 11/29-bit). |
| `main/isotp.h/.c` | ISO 15765-2 segmentation + flow control over CAN. |
| `main/kline.h/.c` | K-line UART with fast + 5-baud init, echo removal. |
| `main/battery.h/.c` | Battery-voltage ADC sense (divider + calibration). |
| `main/opentune_esp32.c` | `app_main`: frame reader + command dispatcher. |
| `main/pins.h` | Pinout (edit for your board). |

## USB-CDC wire protocol

Framing (both directions), an extension of the Arduino K-line bridge's
`0xB5 <cmd> <len> <payload>` with a 16-bit length (ISO-TP payloads reach 4095
bytes) and a trailing CRC-8:

```
+------+-----+--------+--------+--------------+------+
| 0xB5 | CMD | LEN_HI | LEN_LO | PAYLOAD[LEN] | CRC8 |
+------+-----+--------+--------+--------------+------+
```

* `CMD` — command id (host→device), or that id with **bit 7 set** in a reply.
* `LEN` — payload length, big-endian u16 (≤ 4096).
* `CRC8` — CRC-8 (poly `0x07`, init `0x00`, no reflection) over `CMD`, `LEN_HI`,
  `LEN_LO` and every payload byte (the `0xB5` sync is excluded).

A reply's payload starts with a **status** byte (`0x00` = OK); result bytes
follow on OK, or one optional detail byte on error.

### Commands (host → device)

| CMD | Name | Request payload | OK reply payload |
|---|---|---|---|
| 0x01 | PING | — | proto, major, minor, patch, caps, "OpenTune-ESP32S3" |
| 0x02 | CAN_SET_BITRATE | u32 bitrate (BE) | — |
| 0x03 | CAN_START | flags (b0 listen-only, b1 loopback) | — |
| 0x04 | CAN_STOP | — | — |
| 0x05 | CAN_SEND | id(4 BE), flags(1: b0 ext, b1 rtr), dlc(1), data | — |
| 0x06 | CAN_RECV | timeout_ms(2 BE) | id(4), flags(1), dlc(1), data |
| 0x07 | ISOTP_CONFIG | tx(4), rx(4), flags(1 b0 ext), bs(1), stmin(1), pad(1) | — |
| 0x08 | ISOTP_REQUEST | timeout_ms(2 BE), request bytes | full response payload |
| 0x09 | KLINE_SET_BAUD | u32 baud (BE) | — |
| 0x0A | KLINE_INIT_FAST | optional StartComm bytes | key bytes |
| 0x0B | KLINE_INIT_5BAUD | addr(1) | sync, kb1, kb2 |
| 0x0C | KLINE_TRANSCEIVE | timeout_ms(2 BE), bytes | reply bytes |
| 0x0D | BATTERY | — | mV(2 BE) |
| 0x0E | RESET | — | — |

Status codes: `0x00` OK, `0x01` bad-cmd, `0x02` bad-len, `0x03` crc, `0x04`
can-down, `0x05` can-tx, `0x06` timeout, `0x07` isotp, `0x08` kline-init,
`0x09` kline-nodata, `0x0A` busy, `0x0B` unsupported, `0x0C` overflow.

`caps` bitfield: `0x01` CAN, `0x02` ISO-TP, `0x04` K-line, `0x08` battery.

The protocol is synchronous: one request, one reply. `CMD_EVENT` (0x6E, sent as
`0xEE` with the reply bit) is reserved for future unsolicited frames; the host
driver tolerates and skips it.

## Pinout and transceivers

See `main/pins.h`. Defaults for an ESP32-S3-WROOM devkit:

| Signal | GPIO | Connects to |
|---|---|---|
| CAN TX | 4 | SN65HVD230 / TJA1051 TXD (D) |
| CAN RX | 5 | transceiver RXD (R) |
| CAN STBY | 6 | transceiver standby/Rs (optional) |
| K-line TX | 17 | L9637D TX (drives K) |
| K-line RX | 18 | L9637D RX (K-line level) |
| Battery ADC | 1 | OBD pin 16 → 47k/10k divider → GPIO1 |
| USB D-/D+ | 19/20 | fixed by the chip (native USB) |

CAN transceiver: **SN65HVD230** (3.3 V, listen-only via Rs) or **TJA1051**
(needs a clean 5 V supply; its logic is 3.3 V-tolerant on TXD). K-line
transceiver: **L9637D** (ISO 9141 / ISO 14230, built-in 12 V-side protection).
The L9637D's K pin goes to OBD pin 7, V_BAT to OBD 16, GND to OBD 4/5.

Battery divider: 47k (top) / 10k (bottom) puts 14.4 V at ~2.53 V, inside the ADC
11 dB range. Calibrate `DIV_NUM/DIV_DEN` in `battery.c` against a meter.

## Build & flash

Requires a working ESP-IDF 5.1+ install (`. $HOME/esp/esp-idf/export.sh`).

```sh
cd hardware/firmware/esp32-s3
idf.py set-target esp32s3
idf.py build
idf.py -p /dev/ttyACM0 flash monitor   # native USB enumerates as ttyACM*
```

Bring-up without a car:

* `idf.py build` alone verifies the firmware compiles.
* Send `CMD_PING` over the CDC port and check the version/caps reply.
* `CMD_CAN_START` with the loopback flag (b1) exercises TWAI with no bus.
* Jumper K-line TX→RX for a USB-only echo check of `KLINE_TRANSCEIVE`.
