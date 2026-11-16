# OpenTune open interface — hardware spec

OpenTune works with hardware you may already own (an OpenPort 2.0, a SocketCAN
adapter, an ELM327). This document specifies OpenTune's **own** open interface:
a small, cheap, fully documented board that any of those can't quite do on its
own — timing-critical K-line init, TP2.0, and a raw UART path for boot mode —
while presenting a simple USB serial port to the host.

> **Status: design target.** This is the reference board described in roadmap
> M8. Schematics and PCB (KiCad) are added under `hardware/` as they are drawn.

## Why a dedicated device

The host-side Rust core does the high-level protocol work, but some transports
need microsecond-level timing that is unreliable to do over a general-purpose
USB adapter: 5-baud K-line initialisation, KWP fast-init, ISO-TP flow control,
VW TP2.0 channel keep-alive, and a clean raw-UART mode for bootloader access.
OpenTune's interface runs those **timing-critical transports on the device** and
presents a **USB-CDC (virtual serial)** port to the host, so the desktop/CLI/
Android driver talks to it as a simple serial device.

## Core design

| Block | Part | Role |
|---|---|---|
| MCU | **ESP32-S3** | USB-CDC to host; runs the timed transports; optional BLE/WiFi bridge. |
| CAN transceiver | **TJA1051** | High-speed CAN (ISO 11898-2) physical layer. |
| K-line transceiver | **L9637D** | ISO 9141 / ISO 14230 K-line line driver + protection. |
| Level shift | around the L9637D K-line path | Match the ESP32-S3's 3.3 V logic to the transceiver / 12 V bus side. |
| Battery sense | resistor divider → ESP32-S3 ADC | Measure vehicle battery voltage for the safety gate. |
| Bench header | pin header | Direct UART / boot-mode and bench-supply access, separate from the OBD side. |

### Interfaces to the vehicle
- **CAN**: ESP32-S3 TWAI controller → TJA1051 → CAN_H / CAN_L. Supports the
  standard OBD CAN pins; split termination selectable for bench use.
- **K-line**: ESP32-S3 UART → L9637D → K-line (ISO 9141 / ISO 14230), with the
  level-shifting and protection the L9637D path needs for a 12 V bus.
- **Battery sense**: a divider from the OBD battery pin into an ESP32-S3 ADC
  channel, so `battery_millivolts()` reports a real reading and the flash
  orchestrator's voltage gate works. Calibrate the divider in firmware.
- **Bench header**: brings out raw UART (for bootloader / boot-mode access) and
  a bench power connection, kept separate from the OBD connector so bench work
  doesn't go through the car harness.

### Host interface
- **USB-C**, enumerating as **USB-CDC** (virtual serial). The matching OpenTune
  driver (roadmap M8) speaks a simple line protocol to the device.
- Optional **BLE / WiFi** bridge via the ESP32-S3 for a wireless link (e.g. to
  the Android app), carrying the same protocol.

## Firmware responsibilities (device side)
- K-line 5-baud and fast init, byte timing, inter-byte gaps.
- ISO-TP framing and flow control; VW TP2.0 channel setup + keep-alive.
- Raw UART pass-through for boot mode (no interpretation).
- Battery-voltage ADC sampling and reporting.
- A framed command/response protocol over USB-CDC for the host driver.

Firmware is licensed **GPL-3.0-or-later** like the rest of the software.

## Firmware architecture (implemented)

Reference firmware lives in [`hardware/firmware/esp32-s3/`](../hardware/firmware/esp32-s3/),
built with **ESP-IDF** (v5.1+). ESP-IDF is chosen over Arduino-ESP32 for the
native TWAI driver, the built-in USB Serial/JTAG CDC (no external USB chip), and
the oneshot ADC with calibration. It is split one-concern-per-file:

| File | Role |
|---|---|
| `main/wire_protocol.h/.c` | USB-CDC frame format, command IDs, status codes, CRC-8. |
| `main/usb_cdc.h/.c` | Host link over the ESP32-S3 USB Serial/JTAG controller. |
| `main/twai_can.h/.c` | Raw CAN via TWAI (bitrate, send/recv 11/29-bit frames). |
| `main/isotp.h/.c` | ISO 15765-2 segmentation + flow control over CAN. |
| `main/kline.h/.c` | K-line UART with fast + 5-baud init, half-duplex echo removal. |
| `main/battery.h/.c` | Battery-voltage ADC sense (divider + calibration). |
| `main/opentune_esp32.c` | `app_main`: frame reader + synchronous command dispatcher. |
| `main/pins.h` | Pinout (edit for your board). |

The device does **transport only**; the host keeps the diagnostic *service*
layer (UDS / KWP2000). The device-side ISO-TP in `isotp.c` deliberately mirrors
the host core's `opentune-core/src/protocol/isotp.rs` (single/first/consecutive
frames, Block Size / STmin flow control, WAIT/OVERFLOW handling, `0xF→0x0`
sequence-number wrap) so device- and host-side ISO-TP behave identically.

> **Build status (honest):** the firmware is written to the documented ESP-IDF
> 5.x APIs and structured to build with `idf.py`, but it was developed offline
> with **no ESP-IDF toolchain and no board**, so it has **not been compiled or
> run**. Nothing is tested against a real ECU or car. The init timings, ADC
> divider scaling and pin map are starting points to verify on the bench.

## USB-CDC wire protocol

Framing (both directions) extends the Arduino K-line bridge's
`0xB5 <cmd> <len> <payload>` idea with a **16-bit length** (ISO-TP payloads reach
4095 bytes) and a trailing **CRC-8** for line integrity:

```
+------+-----+--------+--------+--------------+------+
| 0xB5 | CMD | LEN_HI | LEN_LO | PAYLOAD[LEN] | CRC8 |
+------+-----+--------+--------+--------------+------+
```

* `CMD` — command id (host→device), or that id with **bit 7 set** in a reply, so
  a reply is matched to its request. `CMD_EVENT` (0x6E, seen as 0xEE) is reserved
  for unsolicited device frames; the host tolerates and skips them.
* `LEN` — payload length, big-endian u16 (≤ 4096).
* `CRC8` — CRC-8 (poly `0x07`, init `0x00`, no reflection) over `CMD`, `LEN_HI`,
  `LEN_LO` and the payload (the `0xB5` sync is excluded). The firmware's
  `ot_crc8` and the host driver's `proto::crc8` are byte-identical (shared test
  vector: `crc8("123456789") == 0xF4`).

A reply's payload is `status(1) || body`. `status == 0x00` is OK; on OK the
result bytes follow, on error `status` is non-zero (optionally one detail byte).

### Commands (host → device)

| CMD | Name | Request payload | OK reply payload |
|---|---|---|---|
| 0x01 | PING | — | proto, major, minor, patch, caps, `"OpenTune-ESP32S3"` |
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

The protocol is synchronous: one request, one reply. For K-line, `ISOTP_REQUEST`
has no equivalent — the host frames KWP2000/ISO-14230 itself and uses
`KLINE_TRANSCEIVE`, matching the `kline-bridge` split.

## Pinout (reference, ESP32-S3-WROOM devkit)

See `main/pins.h`; every pin except the fixed USB pair is a plain GPIO.

| Signal | GPIO | Connects to |
|---|---|---|
| CAN TX | 4 | SN65HVD230 / TJA1051 TXD (D) |
| CAN RX | 5 | transceiver RXD (R) |
| CAN STBY | 6 | transceiver standby / Rs (optional) |
| K-line TX | 17 | L9637D TX (drives K through the transceiver) |
| K-line RX | 18 | L9637D RX (K-line level, echoes our own TX) |
| Battery ADC | 1 | OBD pin 16 → 47k/10k divider → GPIO1 (ADC1_CH0) |
| USB D-/D+ | 19 / 20 | fixed by the chip (native USB) |

Transceiver notes:

* **CAN — SN65HVD230** (3.3 V native, has a listen-only mode via `Rs`), or
  **TJA1051** (needs a clean 5 V supply; its TXD is 3.3 V-tolerant). K pin →
  OBD 6/14, split termination selectable for bench use.
* **K-line — L9637D**: ISO 9141 / ISO 14230 line driver with built-in 12 V-side
  protection. K → OBD pin 7, `V_BAT` → OBD 16, `GND` → OBD 4/5. For the fast /
  5-baud init pulse the firmware detaches UART1 from the TX pin and bit-bangs it
  as a GPIO, then re-attaches the UART.
* **Battery divider** 47k (top) / 10k (bottom): 14.4 V → ~2.53 V, inside the ADC
  11 dB (~3.1 V) range. Calibrate `DIV_NUM/DIV_DEN` in `battery.c` to a meter.

## Build & flash

Requires a working ESP-IDF 5.1+ (`. $HOME/esp/esp-idf/export.sh`):

```sh
cd hardware/firmware/esp32-s3
idf.py set-target esp32s3
idf.py build
idf.py -p /dev/ttyACM0 flash monitor   # native USB enumerates as ttyACM*
```

Offline bring-up (no car):

* `idf.py build` alone verifies compilation.
* `CMD_PING` over the CDC port returns version + caps.
* `CMD_CAN_START` with the loopback flag exercises TWAI with no bus attached.
* Jumper K-line TX→RX for a USB-only echo test of `KLINE_TRANSCEIVE`.

## Host driver

[`crates/drivers/esp32-opentune/`](../crates/drivers/esp32-opentune/) is the Rust
host driver. It implements `opentune-core`'s `Interface` trait over this protocol
(via `serialport`, like the `slcan` / `kline-bridge` drivers):

* `proto.rs` — the frame codec (encode/parse, CRC-8, command/status constants),
  with unit tests mocking the serial layer.
* `device.rs` — `Device<S: Read + Write>`: one method per command; tests inject a
  mock link in place of a real serial port.
* `lib.rs` — `Esp32Driver`: for CAN it configures the device ISO-TP and drives
  `UdsClient` through an `IsoTpDiag` `DiagTransport`; for K-line it frames
  ISO-14230 and drives `Kwp2000Client` through a `KlineDiag`. A `CanRawLink`
  also exposes the device's raw CAN as a `CanLink`, so the host-side `IsoTp` can
  be used instead of the device's if wanted.

## Licence

Hardware design files (schematic, PCB, BOM, production files) for the OpenTune
open interface are released under the **CERN Open Hardware Licence, version 2 —
Strongly Reciprocal (CERN-OHL-S-2.0)**. Derivatives and manufactured boards must
make their design sources available under the same licence. The device firmware
remains under **GPL-3.0-or-later**.

Part names are given to specify the design; they are not endorsements, and
equivalents may be substituted where the spec allows.
