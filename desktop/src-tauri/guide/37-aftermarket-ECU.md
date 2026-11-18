# Aftermarket ECU

Open aftermarket engine controllers — **Speeduino**, **rusEFI** and
**MegaSquirt** — describe themselves with a TunerStudio-format **INI** file and
keep their calibration in pages that a tuning tool reads, edits and burns over a
serial link. This panel speaks that format and those protocols, so the INIs and
`.msq` tune files the firmware projects publish work here. Details and sources:
`docs/AFTERMARKET-ECUS.md`.

This panel is in the **Bench** group. The file handling is tested on real INIs
and tunes; the serial side has only been tested against the simulated ECU.

## What you need

- The **INI of the firmware the ECU runs** (TunerStudio downloads it from the ECU,
  or take it from the firmware release). The signature must match.
- For online work: the ECU's USB serial port (`/dev/ttyACM0`, `/dev/ttyUSB0`,
  `COM3`). Speeduino, rusEFI and MS3 use 115200 baud. Close TunerStudio first: a
  serial port has one user at a time.
- No ECU? Use the port name **`sim`**: a simulated ECU that answers the INI's own
  commands. `samples/ts/opentune-demo.ini` is a synthetic INI to try everything.

## Offline: definition and tune

1. **Open INI…** and **Load**. Conditions for the INI's `#if` blocks (the
   setting groups, e.g. `LAMBDA`) go in **#set** / **#unset**. The summary shows
   the signature, pages, contents and any lines that could not be parsed.
2. The **working tune** starts blank. **Open .msq…** loads a tune into it (its
   saved settings become INI conditions), or read it from the ECU (below).
3. **Save .msq…** writes the working tune. **Diff with .msq…** lists every
   constant that differs between a file and the working tune.

## Editing

Pick a **table**, **curve** or **Settings** in the Edit card.

- Tables are heat grids (highest load at the top, like TunerStudio). Click a cell,
  type the value, press **Enter** (Esc cancels).
- Curves show a line chart and the cells; the first line is editable.
- Settings lists scalars, option fields and strings with their limits; click a
  value to change it. Option fields marked `INVALID` in the INI cannot be chosen.
- A value outside the INI's limits, or one the storage type cannot hold, is
  **refused**, never clamped.

## Online: read, write, burn

1. Enter the **Port** (or `sim`) and **Connect**. OpenTune checks the ECU's
   signature against the INI and refuses a mismatch unless you tick
   **ignore ECU signature**.
2. **Read from ECU** reads every page. That tune becomes the working tune and the
   base of the next write.
3. Edit, then **Plan write**: the changed bytes per page and every changed
   constant.
4. **Write to ECU…** shows the plan in a confirmation, then asks where to save the
   backup. The write then:
   - re-reads the ECU and stops if it no longer holds the tune the plan was
     made against;
   - saves the ECU's current tune as the backup `.msq` **and** a raw `.pages.bin`;
   - writes only the changed bytes, reads every page back and compares;
   - burns the verified pages when **burn after verify** is ticked (otherwise the
     change is only in RAM and is gone at the next power cycle), then compares
     the ECU's page CRC32 where the INI has a CRC command.
   A verify failure burns nothing.
5. **Live channels**: list output channels (e.g. `rpm, map, coolant`) and
   **Start** to read them five times a second. A channel the INI computes with an
   unsupported function shows a note instead of a value.

The serial port shares the app's one-operation-at-a-time adapter slot: while a
read, write or poll runs, vehicle panels report *adapter busy*, and the other
way round. **Disconnect** releases the port.

## Safety

- These ECUs run engines. A wrong fuel, ignition or boost value can destroy an
  engine in seconds. Check every changed value in the plan before confirming.
- Write with the **engine off** and ignition on, on a stable power supply.
- Keep the backup. To undo a write, open the backup `.msq` and write it back.
- Start the engine only after checking the changed values.

## Not supported yet

Tooth/composite loggers, SD-card logs, `[ControllerCommands]` buttons, rusEFI's
scattered high-speed output read and CAN pass-through to other `tsCanId`s.
