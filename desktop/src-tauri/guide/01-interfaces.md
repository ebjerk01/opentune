# Hardware & interfaces

OpenTune talks to a car through an **interface driver**. The whole offline
workflow uses the **mock / file driver**, so you need no hardware to read this
app. For live work, these drivers ship in every build (the **Drivers** panel
lists them and is the authority for your build):

| Driver | Transport | Buses |
| --- | --- | --- |
| `mock` | file on disk | — (offline) |
| `openport2` | Tactrix OpenPort 2.0 (CDC-ACM `at…`) | CAN |
| `socketcan` | Linux SocketCAN (`can0`) | CAN (ISO-TP / TP2.0) |
| `slcan` | serial-line CAN (CANable / CANtact, CH340 USB-CAN) | CAN |
| `gsusb` | gs_usb / candleLight USB-CAN | CAN |
| `efp02c` | EFI Technology EFP02C USB-CAN, direct USB (WinUSB on Windows); unverified | CAN at 1 Mbit/s only |
| `elm327` | ELM327 AT commands over serial | CAN **and** K-line |
| `elm327obd` | ELM327 over serial, read diagnostics only | CAN |
| `elm327net` | ELM327 over Wi-Fi (TCP) or Bluetooth | CAN **and** K-line |
| `j2534` | J2534 PassThru through the vendor library | CAN |
| `kkl` | raw K-line (KKL 409.1) | K-line (ISO 9141 / 14230) |

Buses OpenTune targets: **K-line** (ISO 9141 / ISO 14230 KWP2000) and **CAN**
(ISO 15765 / VW TP2.0). There is no CAN-FD or DoIP support yet.

## Cheap interfaces that work

You do not need an expensive tool. A good sub-$50 kit for older VAG / Japanese
cars:

- **Tactrix OpenPort 2.0 clone (~$15)** — the cheapest interface that can actually reprogram a CAN ECU. Driven by `openport2`.
  - **Never run a firmware "update" on a clone** — a serial-number blacklist can erase the device. Use it as shipped.
- **KKL 409.1 cable (~$5)** — a K-line reader/flasher for 1996–~2005 cars. Driven by `kkl`. Prefer a genuine **FTDI FT232** unit over a CH340 clone for the most robust Linux support.
- **CANable / CANtact (~$10–20)** — with candleLight/gs_usb firmware it is a native SocketCAN device (`socketcan`, or `gsusb` directly over USB); with stock slcan firmware use the `slcan` driver.

## What to avoid

- **ELM327 clones** read fine but are too slow and command-limited to flash reliably. Genuine **OBDLink / STN** silicon is the dependable ELM-class pipe.
- **VCDS "HEX" clones** run locked firmware and expose no open command set — not driveable.
- **J2534 passthru** is a Windows DLL API. The `j2534` driver needs the vendor's PassThru DLL, so it only works on Windows; on Linux use one of the other drivers.

## The OpenTune reference board

The project's own open interface (design target) runs the timing-critical
transports on an **ESP32-S3** (USB-CDC to the host), with a **TJA1051** CAN
transceiver, an **L9637D** K-line transceiver, and a battery-sense divider so the
flash safety gate reads real voltage. Any of the drivers above still work; the
board exists for the transports a general-purpose adapter can't time reliably.
