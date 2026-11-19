# Actuator tests

Drive one output of a module for a set time: a radiator fan, a warning lamp,
a solenoid. The test uses UDS **InputOutputControlByIdentifier (0x2F)** and
only works on the **actuator channels an adaptation definition declares**, with
the control options, value range and time limit the definition gives.

Control is handed back to the module (**returnControlToECU**) automatically:
at the end of the test, when you press **STOP**, on any error, when you leave
the panel, when the adapter disconnects and when the window closes.

This panel is **Bench tier**: live tests run on alpha / bench builds only.
**Plan** works in every build and sends nothing.

## What you need

- **Adapter: CAN**, connected and detected.
- An **adaptation definition** JSON for the module with at least one channel of
  kind `actuator`. It may include the module's CAN `req_id` / `resp_id`.
- The **target module address**: from the definition or the sidebar **tx / rx**.
  A test is refused when the address is unknown; it never defaults to the
  engine ECU.
- **Ignition on**, the dongle's **HS/MS-CAN switch** on the module's bus.
- For a module that needs **SecurityAccess**: a **seed/key id** from a plugin,
  or `sa2` with your own armed SA2 script. OpenTune ships no seed/key
  algorithm.

## Step by step

1. Choose the module's adaptation definition in the **Pack** picker (or
   **File…**), then **List actuator channels**. Value and routine channels are
   not listed: use the Adaptation panel for those.
2. Choose a **Channel**. Its DID, default state, value range and time limit are
   shown under it.
3. Choose an **Option**:
   - **shortTermAdjustment** drives the output to a value. Enter the **Value**
     in the channel's unit when the channel has one; otherwise its declared
     state is sent.
   - **resetToDefault** drives the output to its default state.
   - **freezeCurrentState** holds the output where it is.
   - **returnControlToECU** hands an output back right away (no duration). Use
     it after an interrupted session.
4. Set the **Duration** in seconds (at most the channel's limit).
5. **Plan** shows every request the test will send and checks the value and
   duration. Nothing is sent.
6. Tick the confirmation, press **Run test** and confirm once more. The
   confirmation clears after each run.
7. The progress bar counts down; the **Exchanges** list shows every request and
   response as it happens. **STOP** ends the test at once and hands control
   back.

## Reading the results

- **completed** / **stopped early** — the output was driven and control was
  handed back (**control returned yes**).
- **aborted — nothing was driven** — the session or SecurityAccess step failed
  before the control request.
- **control request refused** — the module said no (wrong session, conditions
  not met, security). Control was still handed back.
- **link failed during the test** — the adapter or the module stopped
  answering while the output was held. A hand-back was still attempted; check
  **control returned**.
- **CONTROL NOT RETURNED** — the module never acknowledged the hand-back,
  even after a retry. The output may still be forced: switch the ignition off
  and on to recover the module, and check the component before driving.

## Debug: free-form IO DID

Alpha / bench builds show a **Debug** card for bringing up a new definition:
it drives an IO DID **no definition declares**, with a raw state you type. It
needs the Target ids, a second confirmation for every run, and is limited to
5 seconds. Nothing checks the DID or the state, so prefer writing the channel
into a definition.

## Common errors

- **Refused: ack required** — tick the confirmation.
- **Bench-tier feature** — live tests are not available on stable builds.
- **outside the declared range** / **above this channel's limit** — change the
  value or the duration; the definition sets both.
- **not declared for this channel** — the definition does not allow that
  option on this channel.
- **CAN address is unknown** — set tx/rx or add `req_id` / `resp_id` to the
  definition.
- **needs SecurityAccess** — enter a seed/key id.
- **adapter busy** — another panel is using the adapter. During a test every
  other panel sees this until the test ends.

## Safety

- An actuator test **moves real parts**. Run it only on a **stationary vehicle,
  parking brake on, gear in park or neutral**, with hands, tools and loose
  clothing clear of the component (fans can start at full speed).
- Keep the test short. The time limit comes from the definition and is never
  more than 60 seconds.
- If the result says **control returned no**, recover the module before
  driving.

The same tests run from the command line:
`opentune adaptation --def <file> actuator list` and
`opentune adaptation --def <file> actuator run <channel> …` (a plan unless
`--ack` is given).
