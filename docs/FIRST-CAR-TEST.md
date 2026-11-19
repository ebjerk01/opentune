# First car test — plan

A step-by-step plan for the first sessions with a real adapter and car, so the time
in the car goes to learning, not to debugging basics. The first session is
**read-only**: nothing in it writes to a module. Writes come later, one at a time,
once every read step has passed.

## What to bring

- A build that includes the live-path fixes (shared ELM327 parser, adapter lock,
  per-module addressing). Check the About panel's version against the release notes.
- The adapter, and for ELM327 dongles with an HS/MS switch: set it to **HS-CAN** for
  engine/powertrain, **MS-CAN** only for body modules.
- A battery charger or maintainer if the ignition will be on for more than a few
  minutes. Readings below ~12.0 V make modules behave oddly.
- This page and a way to save files (session logs, captures, reports).

## Which adapter for which ECU

| ECU family | Bus | Works with |
|---|---|---|
| Bosch EDC16 / EDC17, Simos PCR2.1, VAG DSG (UDS), Haldex Gen5 (MQB) | CAN (OBD pins 6/14) | ELM327, slcan/CANable, OpenPort 2.0, J2534 |
| Haldex Gen4 (PQ, e.g. Passat CC 4Motion) | CAN, KWP2000 over VW TP2.0 | a raw-CAN adapter: CANable (slcan), candleLight (gs_usb) or SocketCAN, see Stage H; **not** the ELM327 |
| Bosch EDC15 (incl. EDC15C6), older K-line ECUs | K-line (pin 7) | KKL 409.1 cable — **not** a CAN-only ELM327, and not OpenTune's `openport2` driver (CAN only so far) |
| Body/comfort modules on MS-CAN (Ford, some others) | MS-CAN | ELM327 with MS-CAN switch, OpenPort |

## Stage 0 — at the desk, no car

| Step | Do | Pass when |
|---|---|---|
| 0.1 | Plug in the adapter. Sidebar adapter box: pick the driver, port, bus. **Detect**. | Badge turns green and shows the firmware (e.g. `ELM327 v1.5`). |
| 0.2 | Note the battery reading. | Reads ~0 V with no car (expected). |
| 0.3 | Bench section → Debug → save the session log. | A log file is written. |

If Detect fails: check the port (`/dev/ttyUSB*` / `/dev/ttyACM*`), the baud (38400
for most ELM327 clones), and that no other program holds the port.

## Stage 1 — ignition on, engine off (read-only)

| Step | Panel | Do | Pass when |
|---|---|---|---|
| 1.1 | Sidebar | Detect again | Battery reads ~12–12.8 V |
| 1.2 | Inspection | Read vehicle info | VIN matches the car; CALID/CVN listed |
| 1.3 | Inspection | Read readiness + DTCs | MIL state and DTC count match the dash |
| 1.4 | Dashboard | Start | Coolant/IAT plausible; RPM 0 |
| 1.5 | Debug | Raw request `22 F190` to the engine | Positive response with the VIN |

## Stage 2 — engine running (read-only)

| Step | Panel | Do | Pass when |
|---|---|---|---|
| 2.1 | Dashboard | Start, then blip the throttle twice | RPM rises and falls; boost/MAP follow on turbo engines |
| 2.2 | Dashboard | Let it run 2 min | No "adapter busy" storms; values update steadily |
| 2.3 | CAN capture | Capture 30 s to a file | Frames counted; Dropped stays low (ELM327 may drop some) |
| 2.4 | Live log | Log a few PIDs for 60 s, save CSV | CSV opens in the Log viewer |

## Stage 3 — whole-car scan (read-only)

| Step | Panel | Do | Pass when |
|---|---|---|---|
| 3.1 | Auto-scan | Load the module list for the car, Scan | Modules you know exist show as present with plausible identities |
| 3.2 | Auto-scan | Compare with the instrument cluster | Fault codes match what the car shows |

Do **not** press "Clear" in this session.

## Stage H — Haldex Gen4 read (read-only)

For 4Motion cars with the PQ-era Haldex Gen4 (the Passat CC 4Motion). Background,
sources and the CLI are in [`HALDEX.md`](HALDEX.md). Nothing in this stage writes:
no code clearing, no basic settings, no coding, no OpenHaldex mode changes.

**Adapter.** The Gen4 is diagnosed with KWP2000 over **VW TP2.0** (logical address
`0x0A`). OpenTune's ELM327 drivers refuse TP2.0, and an ELM327 v1.5 clone cannot
hold a TP2.0 channel (raw frames, per-block ACKs within ~100 ms, keep-alives). So
**with the ELM327 v1.5 only step H.1 runs** (passive). H.2-H.3 actively read the
unit and need a raw-CAN adapter: a CANable (`--driver slcan --port /dev/ttyACM0`),
a candleLight (`--driver gsusb --port can0`) or any SocketCAN interface
(`--driver socketcan --port can0`). These drivers open the TP2.0 channel
themselves (`opentune haldex gen4` implies `transport=tp20`, `tp20_addr=0x0A`);
the TP2.0 path is synthetic-tested only, so this is its first real run. For
gs_usb / SocketCAN bring the interface up first:
`sudo ip link set can0 up type can bitrate 500000`. Run
`opentune haldex gen4 --sim` at the desk first to see what the read will show.

| Step | Car state | Do | Save | Pass when |
|---|---|---|---|---|
| H.0 | desk | `opentune haldex gen4 --sim` | — | Prints a simulated identity, "no codes", groups 1/3 decoded |
| H.1 | ignition on, engine off | `opentune candump --driver elm327obd --bus hs --duration 30 --out H-1-obd.log`, then `opentune haldex decode H-1-obd.log` | `H-1-obd.log`, decode output | Capture runs. Note whether any `Allrad_1` 0x2C0 / `Bremse_4` 0x2A0 frames appear: at the OBD port they normally do **not** (the gateway keeps the drivetrain CAN off the diagnostic bus) — either answer is useful data |
| H.2 | ignition on, engine off | `opentune haldex gen4 --driver slcan --port /dev/ttyACM0 --json > H-2-gen4.json` (or `gsusb`/`socketcan --port can0`) | `H-2-gen4.json`, Debug log, the error text if the open fails | The TP2.0 channel opens (if not, the error names the step and CAN ids: no answer on `0x20A` = the unit is not reachable from the OBD port through the gateway, or the ignition/bitrate is wrong — record it). Identification answers: part number `0AY 907 554 …` or `0BS 907 554 …`, a software version and a "Haldex" component text; the code list is empty if the dash shows no AWD fault (and lists codes if it does); group 1 shows oil and plate temperature near ambient. If supply voltage reads implausibly (not ~12 V), record it — the voltage formula is community-sourced |
| H.3 | engine running, stationary, no throttle | repeat H.2 with `--groups 1,3 > H-3-gen4.json` | `H-3-gen4.json` | Group 3 values are plausible and steady (low oil pressure / duty at rest); no new codes appear |

Also write down the part number printed on the controller label if it is visible
without removing anything, so it can be compared with H.2. Do not wire anything
into the Haldex harness in this session: the connector pinout is still to be
checked on the car.

## Later sessions — one write at a time

Only after every read step has passed, and each on its own day:

1. Clear DTCs on **one** module you have just read (engine), then re-read.
2. Coding: read → preview a change → write → read back → restore the original.
3. Adaptation: read channels only, then one documented, harmless routine.
4. Flashing stays bench-only until the flash path is proven on a spare ECU.

## What to capture and send back

After each stage save: the Debug session log, any CAN capture, the Live log CSV, and
the Auto-debug report. Name files `<stage>-<step>-<what>` (e.g. `2-3-capture.log`) and
write the car (make, model, year, engine) and the bus switch position at the top of
your notes. A failing step is useful data — save the log before retrying.
