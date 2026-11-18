# Haldex

Tools for BorgWarner Haldex AWD couplings on VAG 4Motion / quattro cars: the
open-source **OpenHaldex** controller, a read-only **Haldex Gen4** read, and an
offline decoder for **drivetrain-CAN captures**. Background, sources and
validation states: `docs/HALDEX.md`.

This panel is in the **Bench** group: nothing in it has been tested on a car yet.

## What you need

- **OpenHaldex**: an OpenHaldex unit and an adapter on a bus where it answers on
  `0x70F` / `0x779`. From the OBD port that works only where the gateway routes
  `0x70F` to the drivetrain CAN (MQB). On PQ cars, connect to the drivetrain CAN.
- **Gen4 read**: a driver that opens a VW TP2.0 channel. None does yet (see below).
- **CAN log decode**: a capture of the drivetrain CAN (candump log, OpenTune CSV
  or SavvyCAN CSV), for example from the **CAN capture** panel. No adapter needed.
- No hardware? Tick **Simulated unit** to try the OpenHaldex and Gen4 sections.

## OpenHaldex

1. Press **Read status**, or **Live (2 Hz)** to keep reading. Live polling stops
   when you leave the panel.
2. The tiles show the mode, the lock the unit commands (%), the engagement the
   Haldex reports (raw) and the per-wheel slip (— = no data).
3. To change the mode, click a mode button. OpenTune asks for confirmation and
   names the exact request (`2E FD A2 <mode>` to `0x70F`). After the write it reads
   the status back and shows the mode **the unit reports**, not the one requested.
   A unit whose controller is disabled ignores mode changes; that shows as an error.

| Mode | Lock |
|---|---|
| stock | the car decides (pass-through) |
| fwd | 0 % |
| 5050 | 100 % |
| 6040 | 40 % |
| 7525 | 25 % |
| expert | the unit's own map |

Reading status only sends `0x22` (read by identifier).

## Haldex Gen4 read

The Gen4 controller (`0AY 907 554 C`, `0BS 907 554`; e.g. Passat CC 4Motion,
Tiguan, Golf Mk6 R) speaks KWP2000 over **VW TP2.0**, not UDS. No OpenTune driver
opens a TP2.0 channel yet, so a read on a real car is **refused** with that reason.
With **Simulated unit** ticked the panel runs the full TP2.0 + KWP2000 stack
against a simulated controller (synthetic data) and shows identification, stored
codes (as 5-digit numbers, no fault text yet) and the measuring groups. Group 1
and 3 labels come from OpenHaldex-C6 (community confidence); other values are
shown raw.

The read never clears codes, writes coding, adapts or flashes.

## CAN log decode

1. **Open log…** and choose the frame set: **auto**, **PQ** (Gen1/2/4) or **MQB** (Gen5).
2. **Decode** shows a summary (lock request and actual, Gen4 temperature, flags,
   OpenHaldex modes) and charts over time: lock request, lock actual, clutch
   actual (in the DBC's Nm/min scale), Gen4 temperature and the front-rear
   wheel-speed difference. The first 200 decoded frames are listed below.
3. **Export CSV for Log viewer…** writes the series as a Live-log CSV; open it in
   the **Log viewer** panel to zoom and compare.

## Safety

- Changing the AWD lock changes how the car drives. Change modes only with the
  car stationary or in a safe place, and know how to return to **stock**.
- OpenTune never writes to the Haldex controller itself: no coding, adaptation,
  basic settings, code clearing or flashing.
- Check the connector pinout on your own unit before wiring a bench
  (`database/units/haldex-gen4.json`).
