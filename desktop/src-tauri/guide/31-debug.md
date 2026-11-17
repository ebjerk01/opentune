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

## Safety

- **Raw requests are sent exactly as typed.** There is no check, no backup and no confirmation. A write (`2E`), routine (`31`), reset (`11`) or clear (`14`) acts on the module immediately. Only send requests you understand.
- The session log and responses can contain the VIN and other identifying data. Review a log before sharing it.
