# Service functions

Run service procedures such as DPF forced regeneration, service / oil interval
reset, steering-angle sensor (SAS) calibration or electronic parking brake (EPB)
service mode. Each procedure is a **ServiceScript** JSON: an ordered list of UDS
steps.

## What you need

- **Adapter: CAN**, connected and detected.
- A **ServiceScript** JSON for the procedure and the car.
- The **target module address**: from the script or the sidebar **tx / rx**. **Run is refused unless the target is known.**
- The car in the state the procedure needs (often ignition on, engine off; DPF regeneration needs the engine running and warm).
- The dongle's **HS/MS-CAN switch** on the module's bus.

## Step by step

1. In the **Pack** picker choose a pack and the script (or **File…** for a single ServiceScript JSON).
2. Set the **Per-request timeout** if needed (default 2000 ms).
3. Press **Plan**. This validates the script and lists every step. **Nothing is sent.**
4. Read the plan. Check the module and every step.
5. Tick **I understand Run sends real writes / actions to the module**, then press **Run**.

## Reading the results

- The outcome pill: **planned**, **completed**, or **failed @ step N**.
- Each step shows its number, operation, status (ok, failed, planned, skipped), a detail line and the request/response bytes.
- Run **stops at the first failed step**. Later steps are not sent.
- **This script needs UDS SecurityAccess** — OpenTune ships no OEM seed/key, so such a script can be planned but not run. A plugin build can supply the algorithm: the script then names it in a `seedkey` field, and the plan says which plugin-supplied algorithm will answer the SecurityAccess step. If the named algorithm is not available, the note gives its id.

## Common errors

- **Run disabled** — no CAN adapter is detected, or the confirm box is not ticked. Plan works without an adapter.
- **Refusing to run: tick the confirmation box** — tick it.
- **Target unknown** — set tx/rx or add the module address to the script.
- **Failed @ step N** with a negative response — the module refused, often because a precondition is not met (engine state, temperature, speed).

## Safety

- Service procedures **move real actuators and change stored data**. An EPB service mode retracts the brake pistons: chock the wheels and do not rely on the parking brake until the procedure is finished.
- A **DPF regeneration** makes the exhaust very hot. Run it outdoors, away from anything flammable, and do not leave the car.
- A failed run stops part-way. The module may be left in a non-default state; finish or reverse the procedure before driving.
- Keep a charger on the battery for long procedures.
