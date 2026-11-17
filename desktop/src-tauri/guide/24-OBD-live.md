# OBD live

Four big readouts over generic OBD-II mode 01: RPM, coolant temperature, vehicle
speed and battery voltage. A quick check that the adapter and the engine ECU
talk. For more values, use **Dashboard**.

## What you need

- **Adapter: CAN**, connected and detected.
- **Ignition on**, engine running for RPM.
- The dongle's **HS/MS-CAN switch on HS**.

## Step by step

1. Set the adapter in the sidebar and press **Detect adapter**.
2. Set the **Interval** (minimum 100 ms, default 500 ms).
3. Press **Start**. The status pill shows **live** when reads succeed.
4. Press **Stop** when done.

## Reading the results

- **—** means the value was not returned. With no car connected every value except battery shows **—**, because the adapter reads battery voltage itself.
- One missing value (for example speed) does not stop the others.

## Common errors

- **Red status pill with a message** — the adapter could not be opened or did not answer. Check the port and driver, then **Detect adapter**. Polling keeps trying, so it recovers if the adapter comes back.
- **Adapter busy** — another panel is using the adapter.
- Polling stops by itself if the adapter loses its CAN capability.

## Safety

Read-only. Do not operate the app while driving.
