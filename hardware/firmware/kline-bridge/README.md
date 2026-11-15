# OpenTune K-line bridge (Arduino / ATmega328)

A cheap DIY K-line interface: an Arduino Uno/Nano (16 MHz) acts as a **timed
K-line adapter** for OpenTune. The host speaks KWP2000/ISO-14230 and sends the
raw request bytes over USB; the firmware does the timing-critical part on-chip
(fast / 5-baud init via a BREAK, ISO-14230 inter-byte timing, half-duplex echo
removal) and streams the ECU's reply back. Protocol framing/checksums stay on
the host (`opentune-core`'s `Kwp2000Client`).

## Wiring (needs an L9637D K-line transceiver)
```
  Arduino D4 (KTX) ---> L9637D TXD
  Arduino D3 (KRX) <--- L9637D RXD
  L9637D Vbatt = car 12V (OBD pin 16)   GND = OBD pins 4/5   K = OBD pin 7
```
No transceiver yet? Jumper **D4 → D3** for a USB-only loopback bring-up: every
byte sent is echoed back, so you can exercise the host protocol end-to-end.

## Host protocol (USB serial @115200)
Binary frames: `0xB5 <cmd> <len> <payload…>`.

| Dir | cmd | meaning |
|---|---|---|
| H→D | 0x01 | fast init (ISO-14230) |
| H→D | 0x02 `<addr>` | 5-baud slow init |
| H→D | 0x03 `<hi> <lo>` | set K-line baud (default 10400) |
| H→D | 0x10 `<bytes…>` | transceive: send on K, stream reply back |
| D→H | 0x81 `<status>[keys]` | init result |
| D→H | 0x90 `<bytes…>` | received reply |
| D→H | 0xEE `<code>` | error |

## Build & upload
```sh
arduino-cli compile --fqbn arduino:avr:uno kline_bridge.ino
arduino-cli upload  --fqbn arduino:avr:uno -p /dev/ttyUSB0 kline_bridge.ino
```
A CH340 Nano clone uploads fine as `arduino:avr:uno` at 115200.

## Status
Firmware written; bring-up pending hardware access. An OpenTune host driver for
this device (`drivers/kline-bridge`) is the planned companion — it will speak
the frame protocol above and run `Kwp2000Client` on top, like the KKL driver.
This is the small-scale sibling of the ESP32-S3 interface in `docs/hardware.md`.
