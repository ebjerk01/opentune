# Auto-scan

Walk a vehicle's module list, check which modules answer, read each module's
identity, and read its stored fault codes (DTCs). The scan is read-only. A
separate button clears codes.

## What you need

- **Adapter: CAN** (any driver with the CAN capability), connected and detected.
- A **module list** JSON file for the vehicle: a `vehicle` name and a `modules` list where each entry has a `name`, `addr`, CAN `req_id` / `resp_id` and `protocol`.
- **Ignition on.** Engine off is fine.
- The dongle's **HS/MS-CAN switch** and the sidebar bus setting on the bus the modules live on.

## Step by step

1. Set the adapter in the sidebar and press **Detect adapter**.
2. Press **Choose modules JSON…** and pick the vehicle's list.
3. Press **Scan vehicle**. Each module is addressed on its own CAN ids.
4. Read the table and the DTC details below it.
5. Only if you mean to: **Clear all DTCs**, confirm, then scan again.

## Reading the results

- The summary shows present modules out of the list, and the total stored DTCs.
- **Present / absent** — whether the module answered. Absent is not an error: the car may not have that module, or it is on the other bus.
- **VIN / identity** — the VIN and identification values the module returned.
- **#DTCs** — red count when codes are stored, green 0 when none.
- **Error** — a non-fatal problem with that module (for example a negative response).
- The DTC list gives code, description, system and the status byte.

## Common errors

- **Every module absent** — wrong bus (HS/MS switch), ignition off, wrong module list for this car, or the adapter is not connected.
- **Adapter busy** — another panel (for example a live poll) is using the adapter. Stop it and try again.

## Safety

- Scanning only reads.
- **Clear all DTCs writes to every present module.** It erases the stored codes and their freeze-frame data, and can reset readiness monitors. Write the codes down (or save a scan) before clearing. Clearing does not fix the fault.
- Keep the battery healthy during long scans; use a charger if the ignition is on for a long time.
