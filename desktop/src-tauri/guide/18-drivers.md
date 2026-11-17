# Drivers

Lists every interface driver in this build. Use it to find the driver id you
need in the sidebar **Adapter** box.

## What you need

Nothing.

## Reading the results

Each row shows the driver id, its name, and its mode: **offline** for `mock`,
**hardware** for the rest. What a driver can do (its capabilities) decides which
panels it can drive:

| Driver id | Adapter | Declares |
| --- | --- | --- |
| `mock` | file on disk | memory read/write, battery (fixed 12.6 V) |
| `elm327obd` | ELM327 serial (read diagnostics) | CAN, battery |
| `elm327` | ELM327 (CAN / K-line) | CAN, K-line, memory read/write, battery |
| `elm327net` | ELM327 over Wi-Fi / Bluetooth | CAN, K-line, memory read/write, battery |
| `openport2` | Tactrix OpenPort 2.0 | CAN, memory read/write, battery |
| `j2534` | J2534 PassThru (generic) | CAN, memory read/write |
| `socketcan` | Linux SocketCAN | CAN, memory read/write |
| `slcan` | slcan USB-CAN (CANable / CANtact) | CAN, memory read/write |
| `gsusb` | gs_usb / candleLight USB-CAN | CAN, memory read/write |
| `kkl` | KKL 409.1 K-line cable | K-line, memory read/write |
| `kline-bridge` | Arduino K-line bridge | K-line, memory read/write |
| `esp32-opentune` | OpenTune ESP32-S3 interface | CAN, K-line, memory read/write, battery |

The list in the panel is the authority for your build. A declared capability
means the driver supports it in principle; it does not mean every operation
has been tested on every car.

## Connecting

Pick the driver id in the sidebar **Adapter** box, set the port and HS/MS bus,
and press **Detect adapter**. The badge under it shows the adapter name,
firmware and battery voltage when it answers. Panels that need a capability the
adapter lacks show a yellow banner and disable their buttons.

## Safety

Read-only. See **Hardware & interfaces** for which cheap adapters are worth
buying and which to avoid.
