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

## Older VAG modules (KWP2000)

Channels of kind **KWP measuring block**, **KWP basic settings** and **KWP adaptation** talk KWP2000, over a VW TP2.0 channel (raw-CAN adapter: socketcan, slcan or gsusb) or the K-line (kkl, kline-bridge, elm327). They show the **KWP2000** card instead.

1. Enter the **Module address** (TP2.0 logical or K-line address, for example `0x01`), unless the definition carries one. Leave **Session** at `0x89` unless the module needs another.
2. **Read** a measuring block or an adaptation channel. Some modules want **SecurityAccess** even to read an adaptation channel: enter a **Seed/key id** from a plugin. OpenTune ships none.
3. **Plan** shows every request a write would send, and sends nothing.
4. **Test value** applies a raw value temporarily and reads it back; the module is expected to drop it when the adaptation is left. **Save value** stores it and needs the module's 6-byte **Workshop code**. **Run basic settings** starts basic settings for the group, reads the measuring block a few times, then stops. All three need the confirmation tick, a seed/key id, and confirm once more.
5. Tick **Simulated module** to try all of this against a synthetic module (seed/key id `kwpsim-test`). Its values are made up.

The VAG routine numbers and record layouts behind these channels are from public community work on one module family and are **unconfirmed** for most modules.

## Reading the results

- **routine ok** and an optional **result** — the module accepted the routine. **request results** only reads.
- Actuator: the outcome, **applied** (yes/no), **control returned** (yes/no), an optional status reading, and the raw response.
- **module … — assumed engine ECU** — no target was set for a read.

## Common errors

- **Refused: ack required** — tick the confirmation.
- **Target unknown** — set tx/rx or add `req_id` / `resp_id` to the definition.
- **Negative response** — the module refused: security access needed, wrong session, or conditions not met (speed, temperature, engine state).
- **KWP: session lost** — the TP2.0 channel broke and was set up again without the module's session. A read is retried once on its own. A test or save stops at once and sends nothing more: **read the channel again** before anything else.
- **Refused before sending anything** — a KWP write was not confirmed, or no seed/key id or workshop code was given.

## Safety

- **Actuator tests and routines command the module and can move real parts** (fans, valves, motors, injectors). Run them only on a **stationary vehicle, parking brake on, gear in park or neutral**, with hands and tools clear of the component.
- A routine can start a **DPF regeneration** (very hot exhaust; outdoors only, away from anything flammable) or a **relearn** that changes stored adaptation values.
- After an actuator test OpenTune always hands control back to the module. If the result says **control returned: no**, an output may be left forced. Recover the module (for example cycle the ignition) before driving.
- Keep the battery charged during long routines.
