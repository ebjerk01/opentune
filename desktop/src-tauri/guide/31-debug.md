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

## Safety

- **Raw requests are sent exactly as typed.** There is no check, no backup and no confirmation. A write (`2E`), routine (`31`), reset (`11`) or clear (`14`) acts on the module immediately. Only send requests you understand.
- The session log and responses can contain the VIN and other identifying data. Review a log before sharing it.
