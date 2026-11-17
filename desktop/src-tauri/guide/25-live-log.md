# Live log

Stream values from the ECU using a RomRaider-style `logger.xml` definition,
watch them in a live table, and save the samples as CSV.

## What you need

- **Adapter: CAN**, connected and detected.
- A `logger.xml` definition for your ECU, or a definition pack's logger JSON. The
  bundled `generic-obd2/loggers/obd2-mode01.json` works on any OBD-II car on CAN.
- **Ignition on**; engine running for most values.
- The dongle's **HS/MS-CAN switch** on the bus of the module you log.
- To log a module other than the engine, set its **tx / rx** CAN ids in the sidebar. Blank means the engine ECU (7E0 / 7E8).

## Step by step

1. **Choose logger.xml…** (the picker opens in the bundled packs folder), then press **Load**. The parameters are listed.
2. Tick the parameters to log, or use **Select all** / **Select none**. With none ticked, every parameter is logged.
3. Set the **Interval** in ms (minimum 50, default 500).
4. Press **Start**. The table updates in place.
5. Press **Stop**, then **Save CSV**.

## Reading the results

- **Value** and **Units** per parameter. **Note** explains a missing value.
- UDS parameters (service `0x22`, read by identifier), OBD-II mode-01 PIDs and UDS `0x23` memory reads are read. SSM and other protocols come back with a note instead of a value.
- The CSV has one row per parameter per tick: timestamp (ms), parameter id, name, raw value, scaled value, units and note.

## Common errors

- **Slow updates** — an ELM327 manages roughly 1–3 parameters per second. Log fewer parameters or use a faster adapter. Ticks are skipped while the adapter is still busy with the previous one.
- **Poll error: adapter busy** — another panel is using the adapter.
- **All values blank** — wrong bus, wrong target ids, or the definition does not match this ECU.

## Safety

Read-only on the vehicle. Have a passenger run the laptop if you log while
driving.
