# Inspection

The data an emissions or periodic vehicle inspection reads over generic OBD-II
(SAE J1979 / ISO 15031-5), on one screen:

- **Check-engine lamp (MIL)** and the number of stored emissions codes.
- **Readiness monitors**: whether each self-test the ECU runs has completed since codes were last cleared, and during this drive cycle.
- **Trouble codes**: stored (mode 03), pending (mode 07) and permanent (mode 0A).
- **Freeze frame**: the engine values saved when a code was stored, and which code stored them.
- **Vehicle information**: VIN, ECU name, calibration IDs (CALID) and calibration verification numbers (CVN).

## What you need

- **Adapter: CAN**, connected and detected.
- **Ignition on.** The engine does not have to run.
- The dongle's **HS/MS-CAN switch on HS**. OBD-II modules are on HS-CAN.

## Step by step

1. Set the adapter in the sidebar and press **Detect adapter**.
2. Open **Inspection** and press **Read all**. Use the section buttons to refresh just one part.
3. Tick **All OBD modules (7E0–7E7)** to ask every emissions-related module (engine, transmission, …). Each module that answers gets a tab. This is slower: the adapter is set up again for each address.
4. With a target set in the sidebar, only that module is read.

## Reading the results

- **MIL on** means the ECU has a confirmed emissions fault. The stored-codes count comes from the same answer (PID 01).
- **Readiness**: *complete* means the monitor's self-test has run and passed. *Incomplete* means it has not finished yet. That is normal for a while after codes were cleared or the battery was disconnected; most inspections reject a car with too many incomplete monitors. *Not supported* means this engine has no such monitor. Petrol (spark) and diesel (compression) engines have different monitor sets.
- **This drive cycle** (PID 41) shows the same monitors for the current trip only; *disabled* means the monitor will not run this trip (for example, wrong temperature).
- **Pending** codes are faults seen once and not confirmed yet. **Permanent** codes cannot be cleared with a scan tool: the ECU erases them itself after the monitor passes again. Older ECUs do not support permanent codes (shown as not available).
- **Freeze frame**: values such as RPM, load, coolant temperature and speed at the moment the listed code was stored. PIDs the value table does not know are shown as raw hex.
- **CALID / CVN**: each calibration ID names a calibration in the ECU; the CVN next to it is a checksum the ECU computes over that calibration. Compare the CVN with the stock value for that CALID (manufacturer, inspection databases, a read of a known-stock car). **A CVN that differs from the stock value indicates a modified calibration.** A matching CVN is what inspections check.

## Common errors

- **No response from this module** — ignition off, wrong bus, or no module at that address.
- **Not available** next to a code list or section — the ECU does not support that request (normal for permanent codes on older cars).
- **Adapter busy** — another panel is using the adapter. Stop it first.

## Safety

- Read-only. Nothing is written or cleared. Clear codes in the Auto-scan panel, and only after fixing the cause: clearing resets every readiness monitor to incomplete.
