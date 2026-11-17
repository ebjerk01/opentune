# CAN dump

Capture all traffic on a CAN bus to a file while showing a live table of the
IDs seen. Use it to learn what a car sends, or to record a session for later
analysis.

## What you need

- **Adapter: CAN**. A raw CAN adapter (SocketCAN, slcan, gs_usb) captures best. An ELM327 works on quiet buses only (see below).
- **Ignition on** (most buses are silent with the ignition off).
- The right bus and speed: the dongle's **HS/MS-CAN switch** and the sidebar bus setting must match the bus you tap.

## Step by step

1. Set the adapter in the sidebar and press **Detect adapter**.
2. Choose the output format (see **File formats** below) and the output file.
3. Press **Start**. Frames go to the file, and the table shows one row per CAN ID with how often it appears and its latest data.
4. Do the action you want to record (press a button, open a door, rev the engine).
5. Press **Stop** to close the file.

## File formats

- **candump log** — the Linux can-utils text format, one frame per line like `(time) can0 123#DEADBEEF`. Readable by `canplayer` and many analysis tools.
- **CSV** — one frame per row, for spreadsheets and scripts.
- **Vector ASC** — a text format read by Vector tools and many CAN viewers.

## Reading the results

- Each row in the table is one CAN ID. An ID whose data changes when you press something is a good candidate for that function.
- The frame count shows how busy an ID is. IDs that tick steadily and fast are usually periodic status messages.

## Common errors

- **ELM327 "BUFFER FULL" or dropped frames** — ELM327 monitor mode is slow and overflows on a busy bus. Capture a quieter bus, or use a raw CAN adapter.
- **No frames** — wrong bus or speed, ignition off, or the adapter is not connected.
- **Adapter busy** — another panel is using the adapter.

## Safety

- Capturing does not send diagnostic requests.
- Where the adapter supports it, capture runs in **listen-only** mode and puts nothing on the bus. An adapter without listen-only mode still acknowledges frames like any other node.
- A **wrong bus speed** on an adapter that is not listen-only can put error frames on the bus and upset other modules. Check the bus speed before you start.
- Capture files can contain identifying data such as the VIN. Check before you share one.
