# Auto-debug

A guided self-test for your adapter and car. It runs the tests your adapter
can do, tells you what it is doing, and asks you to do things in the car where
needed. The run can be saved as a JSON report. **Alpha.**

## What you need

- An adapter, set in the sidebar.
- A car you can start, with the adapter in the OBD-II port.
- The dongle's **HS/MS-CAN switch on HS** for the engine tests.

## Step by step

1. Optionally fill in **Vehicle under test** (make, model, year, engine, notes). It goes into the report.
2. Press **Start auto-debug**. The adapter is detected first, then only the tests your adapter's capabilities allow are run.
3. When a blue box says **Do this in the car**, do it, then press **Done — run test**, or **Skip**.
4. When it finishes, press **Save report…** to save a JSON file.

Tests include: adapter handshake, battery voltage, supported PIDs, VIN, stored
fault codes, engine start (RPM), throttle response, coolant temperature, two
observational bus-activity steps (lights, radio), and capability notes for
K-line and memory writing.

## Reading the results

- **PASS** — the check worked. **FAIL** — it did not; the detail says why. **INFO** — informational, or a step you skipped.
- The summary counts pass, fail and info. **Completed runs** keeps the last 50 runs on this computer.

## Common errors

- **Adapter handshake FAIL** — wrong driver or port, or the adapter is not plugged in.
- **Battery below 11 V** — ignition off or a weak battery.
- **Raw-request steps fail with "available only in alpha builds"** — the supported-PIDs, VIN and fault-code steps use the Debug panel's raw request, which only alpha builds allow.
- **No RPM** — engine not running, or the bus switch is on MS.

## Safety

- Every test only reads. No step writes to the car. The **Flash capability** step only reports what the adapter declares.
- Revving: do it with the car in park or neutral and the parking brake on.
- The saved report can contain the **VIN**. Remove it before posting a report publicly.
