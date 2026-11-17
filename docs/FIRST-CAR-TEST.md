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
| Bosch EDC16 / EDC17, Simos PCR2.1, VAG DSG, Haldex | CAN (OBD pins 6/14) | ELM327, slcan/CANable, OpenPort 2.0, J2534 |
| Bosch EDC15 (incl. EDC15C6), older K-line ECUs | K-line (pin 7) | KKL 409.1 cable, OpenPort 2.0 — **not** a CAN-only ELM327 |
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
