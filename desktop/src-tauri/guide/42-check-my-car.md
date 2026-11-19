# Check my car

A guided check of a car for someone who is not a diagnostics expert. Six
steps, one at a time, each with a short instruction. The wizard is
**read-only**: it only asks the car's control units questions. It never clears
a trouble code and never changes a setting.

This panel is **Bench tier** (alpha / bench builds) until it has been tested on
real cars.

## What you need

- An **adapter with a diagnostic link** (CAN or DoIP), set in the sidebar
  **Adapter** box. On a dongle with an **HS/MS-CAN switch**, set it to **HS**.
- **Ignition on.** The engine can stay off until the live check.
- Optional: the vehicle's **module list** (from a definition pack) for a scan of
  every module, not just the OBD ones.

No car at hand? Press **Use the virtual car** or **Use the OBD simulator** in
the first step: the whole check then runs offline.

## The steps

1. **Connect** — checks the adapter you chose in the sidebar (the same probe as
   **Detect adapter**) and reads the battery voltage when the adapter can.
2. **Identify** — reads the **VIN** and the engine computer's name,
   calibration IDs and verification numbers (OBD mode 09; when mode 09 has no
   VIN, the UDS identification `22 F190` is tried). The VIN is broken down into
   manufacturer and model year where possible.
3. **Scan** — asks every OBD address (7E0–7E7) for the **check-engine lamp
   (MIL)**, the **readiness monitors** and the **stored, pending and permanent
   trouble codes**. With a module list chosen, it also runs the **Auto-scan**
   over every module in the list.
4. **Explain** — the results in plain words (see below).
5. **Live check** — start the engine and let it idle with the car parked. The
   wizard takes six readings, half a second apart, of coolant temperature,
   engine speed, vehicle speed, fuel trims and voltage.
6. **Report** — a summary of every step. **Save HTML…**, **Save Markdown…** or
   **Print…** it.

Every step can be **skipped** or **retried**. A step that fails says what went
wrong in plain language; **Technical detail** shows exactly what the app
received. **Next** is available once a step is done or skipped, and the step
bar at the top goes back to any earlier step.

## How the results are explained

The wizard says only what the car reported. It does not guess causes.

- **Trouble codes**: the description comes from OpenTune's trouble-code table
  (the same table as the DTC lookup in Utilities). A code names the test that
  failed; it does not by itself say which part is at fault. A code that is not
  in the table is shown as manufacturer-specific or unknown. Where the code was
  seen is listed too:
  - **stored (confirmed)** over OBD — the fault was detected often enough for
    the control unit to store it;
  - **pending** over OBD — seen in the current or last drive cycle, not
    confirmed yet;
  - **permanent** over OBD — stays until the control unit sees the system pass
    again; clearing codes does not remove it;
  - **module fault memory** (module scan) — with the status bits in words:
    failing at the moment of the scan, pending, confirmed, failed since the
    codes were last cleared, warning lamp requested.
- **Check-engine lamp**: on or off, and which module commands it.
- **Readiness**: an *incomplete* monitor means the engine computer has not
  finished that self-test since its fault memory was last cleared or the
  battery was disconnected. That is not a fault by itself; monitors finish on
  their own during normal driving (some need particular conditions). Some
  emissions inspections require them to be complete. *Not supported* monitors
  are tests this car does not run.

## Ratings

- **Live values** use the Dashboard's own warning / critical marks (for
  example coolant temperature). A value without such a mark — engine speed,
  fuel trims — is shown without a rating.
- **Battery voltage** (measured by the adapter at the OBD socket) uses these
  bands:

| Reading | Rating | Meaning |
| --- | --- | --- |
| below 11.0 V | attention | low or no car power (the Auto-debug panel's limit) |
| 11.0 – 12.0 V | warning | below the 12.0 V OpenTune requires before any write; reading still works |
| 12.0 – 13.0 V | OK | the usual range with the engine off |
| 13.0 – 15.0 V | OK | the usual charging range with the engine running |
| above 15.0 V | warning | above the usual charging range |

## Clearing codes

Clearing codes is not part of this check. After a repair, the **Explain** step
links to the **Auto-scan** panel, which clears codes per module and asks for
confirmation first.

## Common errors

- **Another panel is using the adapter** — stop the Dashboard, a live log or
  any other running panel, then **Retry**.
- **The adapter could not be opened** — check the cable and the port in the
  sidebar, and that no other program has the port open.
- **The car did not answer** — ignition on? Adapter firmly in the socket? HS/MS
  switch on HS?
- **This adapter cannot carry diagnostic requests** — a K-line-only or
  file-only driver; choose a CAN or DoIP adapter.
- **The vehicle module list could not be read** — choose another list, or
  continue without the full module scan.
