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

## Licence

Hardware design files (schematic, PCB, BOM, production files) for the OpenTune
open interface are released under the **CERN Open Hardware Licence, version 2 —
Strongly Reciprocal (CERN-OHL-S-2.0)**. Derivatives and manufactured boards must
make their design sources available under the same licence. The device firmware
remains under **GPL-3.0-or-later**.

Part names are given to specify the design; they are not endorsements, and
equivalents may be substituted where the spec allows.
