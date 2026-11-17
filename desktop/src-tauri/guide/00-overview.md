# OpenTune

OpenTune is an **open-source, calibration-only** ECU toolkit. This desktop app
is the free edition. The calibration panels work entirely **offline** on a
flash-image file, so you can learn the whole workflow with no hardware and no
car connected. The diagnostics panels talk to a car through an adapter.

The calibration panels operate on a **flash dump** (a `.bin` you read from an
ECU or downloaded). Open one in the **Image** panel and the rest of the app
works on it.

## Panels and their guide pages

Each panel has its own page in this guide (pages 10 and up).

Offline, on a file:

- **Image** — open a dump and inspect it as paged hex.
- **Analyze Bin** — everything determinable from a raw dump, with honest confidence (early alpha).
- **Identify** — read the ECU identity, match an OpenTune definition and vehicle-catalog candidates.
- **Checksum** — verify the integrity fields the ECU requires, and correct them.
- **Maps** — diff two dumps, import XDF / A2L, and view or edit maps as a heat grid.
- **Compare** — per-map differences between two images, in physical units.
- **Flash** — the safety-gated write sequence, run through the offline file driver.
- **Vault** — browse a folder of CRC-tagged backups.
- **Drivers** — every interface driver in this build and what it can do.
- **Project** — save your map edits as a small `.otproj` file.
- **Utilities** — VIN, part-number and DTC decoders, and a generic seed/key calculator.
- **ECU Database** — reference data on ECUs, TCUs and AWD units, with bench pinouts.

Live, through an adapter:

- **Auto Scan** — find every module, read identity and fault codes, clear codes.
- **Dashboard** and **OBD Live** — live engine values over generic OBD-II.
- **Live Log** — log ECU values from a `logger.xml` definition to CSV.
- **CAN Dump** — capture all bus traffic to a file.
- **Coding**, **Adaptation**, **Service Functions** — change module settings, run tests and service procedures. These write to the car.
- **Auto Debug** — a guided self-test of adapter and car (alpha).
- **Debug** — raw requests and a UDS simulator (alpha builds only).

## Connecting an adapter

The **Adapter** box at the bottom of the sidebar sets the connection for every
live panel:

1. Pick the **driver** (see **Drivers**) and type the serial **port** if the adapter uses one.
2. Set **HS** or **MS** to match the bus of the module you want, and the dongle's physical HS/MS-CAN switch. Engine and gearbox are on HS; body and comfort modules are often on MS.
3. Optionally type the target module's CAN **tx / rx** ids (hex). Blank means: use the ids in the definition file; reads fall back to the engine ECU (7E0 / 7E8). Writes, routines and service runs are refused when no target is known.
4. Press **Detect adapter**. The badge shows the adapter name, firmware and battery voltage.

Only one panel can use the adapter at a time. If you get **adapter busy**, stop
the other panel's live poll and try again.

## Scope

This edition is **calibration-only by design**. It reads identity, checks and
fixes checksums, decodes and compares maps, and runs the safety-gated flash
sequence. It does **not** include immobiliser, EEPROM, or engine-standalone
tooling — those are deliberately out of scope here.

> Nothing in this app defeats an immobiliser or an emissions system. It is a tool
> for understanding and calibrating an ECU image you are allowed to work on.

## Offline first

Reads, checksum work, diffs and map decoding never need a car. Hardware drivers
(OpenPort 2.0, SocketCAN, SLCAN, K-line) ship for live reads and flashes, but the
mock / file driver powers everything you see here from a plain file on disk.
