# Cheap / clone diagnostic interfaces for OpenTune

This note surveys the cheap AliExpress / eBay diagnostic interfaces a hobbyist is
likely to already own or to buy for a few dollars, and maps each one onto an
OpenTune driver — either an existing one or a driver that does not exist yet.

OpenTune today registers twelve interface drivers (`crates/drivers/*`,
`opentune drivers`):

| Driver | Transport | Buses |
| --- | --- | --- |
| `mock` | file on disk | — (offline test) |
| `openport2` (`j2534-openport`) | Tactrix OP2 CDC-ACM, `at…` ASCII protocol | CAN (raw/ISO-TP) |
| `socketcan` | Linux SocketCAN (`can0`) | CAN (ISO-TP / TP2.0) |
| `elm327` (`kline-elm327`) | serial AT commands | CAN **and** K-line (ISO9141 / ISO14230) |
| `elm327obd` (`elm327`) | serial AT commands, OBD/UDS diagnostics | CAN (HS / MS) |
| `elm327net` | ELM327 AT over TCP (WiFi) or Bluetooth RFCOMM | CAN + K-line |
| `kkl` | raw serial, software K-line init + framing | K-line |
| `kline-bridge` | Arduino bridge running OpenTune firmware | K-line |
| `slcan` | LAWICEL/slcan ASCII over serial | CAN |
| `gsusb` | gs_usb / candleLight (via kernel SocketCAN) | CAN |
| `j2534` | vendor PassThru DLL (Windows) | CAN (+ whatever the VCI offers) |

Buses OpenTune targets: **K-line** (ISO 9141 / ISO 14230 KWP2000) and **CAN**
(ISO 15765 / VW TP2.0). **CAN-FD** (ISO-TP-FD) and **DoIP** (ISO 13400) are
implemented in `opentune-core` and tested against mock/loopback peers, but no
registered driver exposes them on real hardware yet — so the newer "VCI" clones
are still limited to their legacy CAN channel in practice.

A note on terms: *reading* here means reading ECU identity and dumping flash via
the OBD diagnostic services (UDS / KWP2000). *Flashing* means the full
write-with-seed/key reprogramming flow. Many cheap tools can do the first and
not the second.

---

## 1. Compatibility matrix

| Interface | Typical chipset | ~Price | Bus(es) | What OpenTune would speak | OpenTune driver | Read? | Flash? | Key caveats |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **Tactrix OpenPort 2.0 clone** | LPC2364 (+ clone MCU) | $12–25 | CAN (K-line on genuine) | OP2 `at…` ASCII over CDC-ACM | `openport2` (existing) | Yes | **Yes** (CAN) | Firmware update can brick a clone (serial-number blacklist wipes flash); clone K-line path unreliable; USB id usually `0403:cc4d`. Best cheap flasher. |
| **ELM327 clone (USB)** | fake "v1.5/2.1" PIC/ARM clone, or genuine STN11xx | $3–10 fake / $20–40 STN | CAN + K-line | ELM AT commands over serial | `elm327` (existing) | Yes (often partial) | **No** in practice | Most "v1.5" are fakes (ELM never shipped 1.5); missing/garbled commands; too slow and command-limited to flash reliably. |
| **ELM327 (Bluetooth / WiFi)** | same clones | $5–15 | CAN + K-line | ELM AT over RFCOMM / TCP socket | `elm327net` (existing) | Yes (read/log) | No | Same AT dialect as the USB `elm327` driver, over WiFi (TCP 35000) or a Bluetooth RFCOMM serial device. |
| **OBDLink SX / EX / MX+** | genuine STN1110 / STN2120 | $25–60 | CAN + K-line | ELM AT (STN superset) over serial/BT | `elm327` (existing) | Yes (reliable) | Marginal | Genuine STN silicon = full command set, fastest ELM-class option; still an ELM-class pipe, not a J2534 flasher. Good "known-good ELM". |
| **KKL 409.1 / "VAG-COM" cable** | FTDI FT232 **or** CH340/CH341 + K-line transceiver (e.g. L9637) | $3–8 | **K-line only** | Raw bytes over serial; init + ISO14230/9141 framing in software | `kkl` (existing) | Yes | Yes (K-line ECUs) | **Not an ELM327** — it is a dumb serial↔K-line level shifter. Needs bit-level 5-baud/fast init done by OpenTune. CH340 vs FT232 affects which host driver/VID:PID. 1996–~2005 K-line cars only. |
| **CANable / CANable 2.0** | STM32 + candleLight (gs_usb) **or** slcan fw | $10–20 | CAN | SocketCAN (`can0`) or slcan serial | `socketcan` / `gsusb` / `slcan` (existing) | Yes | **Yes** (CAN) | With **candleLight/gs_usb** it is a native SocketCAN device, no extra driver. With stock **slcan** firmware use the `slcan` driver directly (no `slcand` bridge needed). Best cheap Linux CAN option. |
| **Macchina A0 / DIY ESP32 CAN bridge** | ESP32-WROOM + 3.3 V CAN transceiver, running [esp32-isotp-ble-bridge](ISOTP-BLE-BRIDGE.md) (MIT) | $10–60 | CAN only, 500 kbit/s | bridge frames over BLE or USB serial | `isotp-ble` (new, unverified) | Yes (UDS) | In principle (VW_Flash flashes over it) | Device does ISO-TP on four fixed id pairs only (engine, gearbox, 7E5/7ED, 700/7E8); no TP2.0, K-line or raw CAN. BLE needs the driver's `btleplug` build feature. |
| **CANtact / CANtact Pro** | STM32, slcan/candleLight | $20–60 | CAN (Pro adds CAN-FD/SWCAN) | SocketCAN | `socketcan` (existing) | Yes | Yes (CAN) | Same lineage as CANable; Pro's CAN-FD is unused by OpenTune today. |
| **"USB-CAN Analyzer"** | CH340 (HL-340) + MCP2515 | $8–15 | CAN | **Proprietary serial** framing | `slcan` (existing) if it speaks slcan ASCII; otherwise an out-of-tree kernel module | Yes (with work) | Yes (CAN) | Does **not** present as SocketCAN out of the box; uses a closed vendor protocol. Community `usb-can` kernel module (slcan-based) or a userspace driver is required. Avoid unless you already own one. |
| **Korlan USB2CAN (8devices)** | 8devices USB2CAN | $45–60 | CAN | SocketCAN (`can0`) | `socketcan` (existing) | Yes | Yes (CAN) | Mainline Linux `usb_8dev` kernel driver → native SocketCAN. Pricier but genuinely plug-and-play on Linux. |
| **J2534 passthru clone** (Mongoose/-Pro, VNCI, SVCI, Scanmatik, generic VCI) | various (FTDI + MCU, STM32…) | $20–120 | CAN (+ CAN-FD/DoIP on newer) | SAE J2534 PassThru API | `j2534` (existing, Windows-only) | Yes | Yes | J2534 is a **Windows 32-bit DLL** API; no official Linux support. A Linux shim exists only as community hacks. On Windows the generic `j2534` driver loads the vendor DLL; on Linux, skip. |
| **VCDS "HEX" clone** (HEX-V2 / HEX+CAN) | ATmega162 (+ CAN MCU) running cloned Ross-Tech fw | $15–40 | CAN + K-line | **None exposed** — speaks only to VCDS | **not driveable** | No | No | Runs cloned Ross-Tech firmware locked to the VCDS application; no open/generic command interface. Not a general interface for OpenTune. |
| **Bench / boot tools** (BDM frames, "ECU bench" cables, breakout boxes e.g. Godiag GT100/GT101) | passive wiring / BDM probe | $10–90 | n/a (bench) | n/a | **out of scope** | n/a | n/a | These are for **bench** reads (BDM/boot mode) or pin breakout, not OBD-port diagnostics. GT100/GT101 is a passive breakout box + bench power, not a VCI. Outside OpenTune's OBD scope. |

Prices are rough street prices on AliExpress/eBay in 2025–2026 and vary widely.

---

## 2. Recommended cheap kit for OpenTune v1

For the common case — an older VAG / Japanese car with a mix of K-line and
pre-CAN-FD CAN ECUs — a cheap, well-supported kit under ~$50 is:

1. **Tactrix OpenPort 2.0 clone (~$15)** — the flashing workhorse.
   - Works with the **existing `openport2` driver** (CAN only; audited against
     the public protocol notes, **hardware validation pending** — checklist in
     [DRIVERS.md §3l](DRIVERS.md#3l-openport2-tactrix-openport-20-capture-validated-k-line-and-transmit-pending)).
   - It is the cheapest interface that can actually *reprogram* a CAN ECU, and
     the driver handles its `at…` protocol and the seed/key flow.
   - Caveat: **never run a firmware "update"** on a clone — a serial-number
     blacklist check can erase the device. Use it as-is.

2. **KKL 409.1 cable (~$5)** — the K-line reader/flasher.
   - Covers 1996–~2005 K-line ECUs the OpenPort clone's K-line path handles
     poorly. These cars are exactly where a hobbyist open-source tool is most
     useful (no cheap alternative exists).
   - Uses the **`kkl` raw K-line driver** (added after this survey — see
     section 3). It unlocks a whole class of cheap hardware that nothing else in
     the ecosystem drives openly.
   - Prefer an **FTDI FT232** unit over CH340 if you can — genuine FTDI has the
     most robust Linux `ftdi_sio` support; CH340 works via `ch341` but counterfeit
     FTDI chips have historically been bricked by vendor "driver" updates.

3. **CANable (candleLight firmware) (~$12)** — clean Linux CAN.
   - Native **SocketCAN** via `gs_usb`, so it uses the **existing `socketcan`
     driver** with zero extra setup (`ip link set can0 up type can bitrate 500000`).
   - A better day-to-day CAN experience than the OpenPort clone for logging and
     reading; keep the OpenPort for flashing.

An ELM327 is deliberately *not* in the core kit: a cheap one is fine for a first
"can I even talk to this ECU" smoke test with the existing `elm327` driver, but
it cannot flash and the fakes are unreliable. If you want a dependable ELM-class
device, buy a **genuine STN-based OBDLink SX** rather than a $4 clone.

---

## 3. Driver gaps this implied (roadmap input — now all done)

These are the new drivers / transports the survey surfaced, in rough priority
order. All four have since been implemented (`kkl`, `slcan`, `elm327net`,
`j2534`); they are unit-tested but not yet validated on real hardware. The
original rationale is kept below for history.

1. ✅ **KKL raw K-line driver** (high value) — `kkl`.
   A dumb-serial driver for FT232/CH340 + K-line-transceiver cables: own the
   5-baud and fast (ISO 14230) init, the half-duplex byte timing, and the L-line
   wakeup, then run the existing `Kwp2000Client`/K-line service stack on top.
   Unlocks the cheapest hardware class, which nothing else drives openly.

2. ✅ **slcan / serial-CAN driver** (medium value) — `slcan`.
   A driver that speaks the LAWICEL/slcan ASCII protocol (and/or the cheap
   "USB-CAN Analyzer" binary framing) directly in OpenTune, so a CAN adapter
   works without relying on the host `slcand` bridge or an out-of-tree kernel
   module. Lets non-SocketCAN / non-Linux hosts use cheap CAN dongles.

3. ✅ **ELM327 Bluetooth / WiFi transport** (low/medium value) — `elm327net`.
   The `elm327` driver already speaks the AT dialect; it just opens a serial
   port. Add a BT-SPP (RFCOMM) and a WiFi (TCP, typically `192.168.0.10:35000`)
   transport so the ubiquitous wireless clones work for read/log.

4. ✅ **Generic J2534 driver** (Windows-only, lower priority) — `j2534`.
   A driver that loads a vendor J2534 PassThru DLL would reach Mongoose/VNCI/SVCI
   /Scanmatik/generic VCI clones — but only on Windows, since J2534 has no
   official Linux API. Treat as a Windows-host feature, not a Linux path.

Explicitly **not** worth a driver: VCDS HEX clones (locked cloned firmware, no
open command interface) and bench/BDM tools (outside the OBD scope).

---

## 4. Clone pitfalls

- **Firmware-update bricking.** The biggest trap. Running a vendor firmware
  "update" on a clone OpenPort 2.0 can trigger a serial-number blacklist that
  erases the device's flash and enables write protection, leaving it dead.
  Mini-VCI-style cables have likewise been bricked by firmware updates. **Rule:
  use clones at the firmware they ship with; never update.**

- **Fake ELM327 chipsets.** ELM Electronics never released firmware "v1.5", so
  every adapter reporting v1.5 is a clone. Cheap clones routinely miss or garble
  parts of the ELM327 command set, fail on specific ECU connection sequences, and
  are too slow/limited to reprogram. Genuine **ScanTool STN11xx** silicon (as in
  OBDLink) has the full command set and is the only ELM-class hardware worth
  trusting for anything beyond casual reads.

- **Counterfeit FTDI drivers.** Many "FTDI" KKL cables carry counterfeit FT232
  chips. Vendor Windows drivers have in the past deliberately bricked
  counterfeit chips (writing PID 0). On Linux `ftdi_sio` is more forgiving, but a
  CH340-based cable (VID:PID `1A86:7523`) is a safer bet than a fake FTDI; just
  make sure you load the matching host driver (`ch341` vs `ftdi_sio`).

- **CH340 "USB-CAN" adapters are not SocketCAN.** The $8 CH340+MCP2515 "USB-CAN
  Analyzer" uses a closed vendor serial protocol and needs either the vendor's
  Windows tool or a community `usb-can`/slcan-derived kernel module — it will not
  just appear as `can0`. Prefer a candleLight-flashed CANable or an 8devices
  Korlan for painless Linux CAN.

- **Quality variance.** "OpenPort 2.0" and "409.1" cover a wide range of build
  quality. Some OP2 clones need hardware mods (e.g. a resistor change on the
  LPC2364 reset line) to be safe to connect; KKL cables vary in whether the
  K-line transceiver and 12 V OBD feed are wired correctly, which shows up as
  "interface not found" even when the USB-serial chip enumerates fine. Buy from
  sellers with flashing/tuning feedback, and expect to test each unit.

---

## Sources

- HP Academy, *Tactrix Openport 2.0 Alternatives* — https://www.hpacademy.com/technical-articles/tactrix-openport-2-0-alternatives/
- Tactrix official (firmware/DLL, OP2 status) — https://www.tactrix.com/
- OBDII365 blog, *How to fix OpenPort 2.0 clone* and *clone high vs low quality* (clone blacklist/brick, reset-resistor mod) — https://obdii365.blogspot.com/2022/03/how-to-fix-openport-20-clone-to-allow.html , https://obdii365.blogspot.com/2022/03/openport-20-clone-high-quality-vs-low.html
- MiniVCI blog, *How to repair bricked Chinese Mini VCI* (firmware-update brick) — https://minivci.blogspot.com/2022/10/how-to-repair-bricked-chinese-mini-vci.html
- Multiecuscan forum, *Faulty ELM327 interfaces de-mystified* — https://www.multiecuscan.net/forum/viewtopic.php?p=16260
- FORScan forum, *Known problems with china clones of ELM327* — https://forum.forscan.org/viewtopic.php?p=5601
- CNX-Software, *Beware of fake ELM327 OBD-II adapters* — https://www.cnx-software.com/2022/08/27/beware-of-fake-elm327-obd-ii-bluetooth-adapters/
- ScanTool/OBD Solutions STN1110 product description (full ELM327 command set) — referenced via above
- CANable project (candleLight gs_usb vs slcan, CAN-FD note) — https://canable.io/ , https://foofoodamon.github.io/references/can2usb/Getting%20Started%20-%20CANable.pdf
- CNX-Software, *CANtact Pro open-source USB-to-CAN* — https://www.cnx-software.com/2020/07/24/cantact-pro-open-source-usb-to-can-device-enables-car-hacking/
- `kobolt/usb-can` and `SeeedDocument/USB-CAN-Analyzer` (CH340 "USB-CAN Analyzer" needs kernel module / closed vendor protocol) — https://github.com/kobolt/usb-can
- Linux SocketCAN setup notes — https://dokkev.github.io/socketcan
- RomRaider forum, Linux J2534 support thread (`j2534.so` shim, no official Linux J2534) — https://mailserver.romraider.com/forum/viewtopic.php?p=131692
- SAE J2534-5 (alternate platforms / non-Windows) — https://saemobilus.sae.org/standards/j2534-5_5_00-pass-thru-interface-alternate-platforms-api-version-0500
- `jakka351/OpenJ2534` and `dschultzca/j2534` (open/Linux J2534 resources) — https://github.com/jakka351/OpenJ2534
- Godiag GT100 breakout box listings (passive OBD breakout + bench power) — https://copperhilltech.com/blog/tag/OBDII+breakout+box
- Elektroda forum, KKL 409.1 CH340 vs FT232 VID:PID identification — https://www.elektroda.com/rtvforum/topic3065149.html
