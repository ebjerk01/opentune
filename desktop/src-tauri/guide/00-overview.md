# OpenTune

OpenTune is an **open-source, calibration-only** ECU toolkit. This desktop app
is the free edition: it works entirely **offline** on a flash-image file, so you
can learn the whole workflow with no hardware and no car connected.

Everything here operates on a **flash dump** (a `.bin` you read from an ECU or
downloaded). Open one in the **Image** panel and the rest of the app works on it.

## What this edition does

- **Image** — open a dump and inspect it as paged hex.
- **Identify** — read the ECU identity, match it to an OpenTune definition, and
  list vehicle-catalog candidates (998-vehicle opendbc catalog).
- **Checksum** — verify the integrity fields the ECU requires, and correct them.
- **Maps** — diff two dumps to find changed regions, and decode a defined
  calibration map into a heat grid with real axis labels and units.
- **Flash** — a safety-gated write, demonstrated through the offline mock driver:
  voltage gate → verified backup → write → read-back verify.
- **Vault** — browse a folder of CRC-tagged backups.
- **Drivers** — see every interface driver this build can talk to.

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
