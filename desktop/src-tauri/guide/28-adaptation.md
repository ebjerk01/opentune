# Adaptation

Read adaptation values, run basic-settings routines, and run actuator (output)
tests on a module, all from a data-defined channel set (an AdaptationDef JSON).

## What you need

- **Adapter: CAN**, connected and detected.
- An **adaptation definition** JSON for the module. It may include the module's CAN `req_id` / `resp_id`.
- The **target module address**: from the definition or the sidebar **tx / rx**. Reads fall back to the engine ECU. **Routine start/stop and actuator tests are refused unless the target is known.**
- **Ignition on.** Some routines need the engine running; the channel note says so when the definition includes it.
- The dongle's **HS/MS-CAN switch** on the module's bus.

## Step by step

1. Choose the module's adaptation definition in the **Pack** picker (a pack, then the definition; or **File…** for a single JSON), then **List channels**. The card shows the module, the number of channels, whether SecurityAccess is required, and the module address (or a warning that none is set).
2. Choose a **Channel**. Its kind is shown: **value**, **routine** or **actuator**.
3. **Value channel** — press **Read**. You get the raw value and the scaled value with its unit.
4. **Routine channel** — choose **start**, **stop** or **request results**, optionally enter **extra** data (hex), tick the confirmation and press **Run test**. Start and stop ask once more before sending.
5. **Actuator channel** — optionally enter a **state** (hex) to override the default, tick the confirmation and press **Run test**.

The confirmation box clears after each run. Tick it again for the next one.

## Reading the results

- **routine ok** and an optional **result** — the module accepted the routine. **request results** only reads.
- Actuator: the outcome, **applied** (yes/no), **control returned** (yes/no), an optional status reading, and the raw response.
- **module … — assumed engine ECU** — no target was set for a read.

## Common errors

- **Refused: ack required** — tick the confirmation.
- **Target unknown** — set tx/rx or add `req_id` / `resp_id` to the definition.
- **Negative response** — the module refused: security access needed, wrong session, or conditions not met (speed, temperature, engine state).

## Safety

- **Actuator tests and routines command the module and can move real parts** (fans, valves, motors, injectors). Run them only on a **stationary vehicle, parking brake on, gear in park or neutral**, with hands and tools clear of the component.
- A routine can start a **DPF regeneration** (very hot exhaust; outdoors only, away from anything flammable) or a **relearn** that changes stored adaptation values.
- After an actuator test OpenTune always hands control back to the module. If the result says **control returned: no**, an output may be left forced. Recover the module (for example cycle the ignition) before driving.
- Keep the battery charged during long routines.
