# Debug

Low-level tools for developers and testers: send raw UDS / OBD requests, run
the offline UDS simulator, and keep a session log. **Only shown in alpha
builds.**

## What you need

- An alpha build of OpenTune.
- For raw requests: an adapter with a diagnostic transport, connected.
- For the simulator: nothing.

## Connection

Set **Driver**, **Port**, **Baud**, **CAN bus** (HS 500k / MS 125k) and
**Timeout ms**, then press **Detect adapter**. These mirror the sidebar
Adapter box. `elm327obd` defaults to `/dev/ttyUSB0` at 38400 baud;
`openport2` ignores port and baud. Match the bus to the dongle's HS/MS switch
and to the module (powertrain on HS, body/comfort often on MS).

## Raw UDS / OBD request

Type the request as hex (for example `22 F1 90`, read the VIN by identifier)
and press **Send**. The response appears below as hex. The request goes to the
sidebar **tx / rx** ids, or to the engine ECU (7E0 / 7E8) when they are blank.

## Offline UDS simulator

Runs requests against a simulated UDS server, no adapter needed.

1. **Seed DIDs** — values the simulator holds, as `ID=HEX` pairs separated by commas (for example `F190=0102, F1A0=AA`).
2. Requests — hex requests separated by commas (for example `22F190, 22F1A0`).
3. Press **Run sim**. The exchanges appear in the session log.

## Session log

Every raw exchange and adapter probe is logged with the target id and timing.
**Refresh**, **Clear**, or **Save…** it as a text file to attach to a bug
report.

## Render check

Shows how WebKitGTK draws on this machine and measures how smoothly panels
scroll. Use it when the app feels laggy.

- **Read environment** lists the rendering-related environment variables the
  app sees (`WEBKIT_DISABLE_COMPOSITING_MODE`, `WEBKIT_DISABLE_DMABUF_RENDERER`,
  `GDK_BACKEND`, `XDG_SESSION_TYPE`; whether a Wayland / X11 display and an
  AppImage are in use), the GPU render nodes and their driver, whether the
  DMA-BUF renderer is on and which rule turned it off (if any), what OpenTune
  set itself, and the WebGL renderer (`llvmpipe` means software rendering).
- **Measure scrolling**: pick a panel and press the button. The app switches
  to that panel; scroll it up and down for about 5 seconds. It then returns
  here with the frame rate, the median and 95th-percentile frame time and the
  number of long frames (over 50 ms). A smooth panel shows about 60 fps and
  17 ms frames on a 60 Hz screen.

Results are added to the session log. Start the app with `OPENTUNE_PERF=1` to
log the environment at startup and measure the first scroll after every panel
switch (printed to the terminal too). If `WEBKIT_DISABLE_COMPOSITING_MODE` is
set, the app shows a warning once: everything is drawn on the CPU and
scrolling will be slow. Remove it from whatever starts OpenTune.

## Diagnostics log

OpenTune keeps a local log file of crashes, errors in the app window, failed
commands and startup steps (the public app never sends it anywhere). The card shows the
last 100 records; pick a **Level** to show only warnings and errors.

- **Open folder** opens the folder with `opentune.log` (and up to five older
  `opentune.log.1` … `.5`).
- **Copy last 200 lines** puts the newest 200 records on the clipboard, as
  they are in the file, for a bug report.
- **Clear…** deletes the log after a confirmation.

SecurityAccess seeds and keys are masked and long hex payloads shortened
before anything is written; file contents are never recorded. Paths and a VIN
in an error message can still be there, so review the lines before sharing
them. Details: docs/DIAGNOSTICS.md.

## Diagnostics upload

Only in a build that configures an upload target (a plugin build, or
`OPENTUNE_DIAG_ENDPOINT` set by you) a **Diagnostics** card appears here and
under About & updates. In the public app it is hidden and nothing is sent.

- **Allow sending diagnostics** (off by default) must be on before anything
  can be sent by hand.
- **Send diagnostics…** opens a dialog: write an optional note, press
  **Preview** to see the exact text that will be sent (VINs, part numbers,
  paths, user and host names, IP addresses and seeds/keys are already replaced
  by markers such as `<VIN>`), then **Send**.
- After a crash the next start only offers to send.
- A plugin build may send automatically. It says so once at the first start
  and shows **Diagnostics: auto → host** in the sidebar; **Send diagnostics
  automatically** turns it off.
- The history lists the last 20 uploads; **Show payload** shows exactly what
  was sent.

Never sent: flash images, datalogs, definitions, CAN captures or any file you
opened. `OPENTUNE_DIAG=off` turns every upload off.

## Safety

- **Raw requests are sent exactly as typed.** There is no check, no backup and no confirmation. A write (`2E`), routine (`31`), reset (`11`) or clear (`14`) acts on the module immediately. Only send requests you understand.
- The session log and responses can contain the VIN and other identifying data. Review a log before sharing it.
