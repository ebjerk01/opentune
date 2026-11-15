# OpenTune

**Open-source ECU reading, flashing and calibration for people who want to tune their own cars with cheap, open hardware.**

> ⚠️ **Early work in progress.** The foundation is being built in the open. Nothing here is
> ready to touch a real car yet. Reading, writing or reflashing an engine or transmission
> control unit can permanently damage it. Use at your own risk — see [Safety](#safety).

OpenTune is a Rust core with pluggable **interface drivers** (so it works with hardware you
already own), a desktop app, and — later — an Android app, all sharing one engine. It aims to
cover the whole honest-tuning workflow: identify an ECU, make a verified backup, read it, edit
the calibration, correct the checksums, write it back, and verify.

## Design in one picture

```
            ┌──────────────────────────────────────────────┐
            │  Apps:  CLI  ·  Desktop (Tauri)  ·  Android    │
            └───────────────────────┬──────────────────────┘
                                    │
            ┌───────────────────────┴──────────────────────┐
            │  opentune-core                                │
            │   Interface trait · protocols (UDS/KWP/ISOTP) │
            │   ECU definitions · flash orchestrator        │
            │   checksum engine · safety rules              │
            └───────────────────────┬──────────────────────┘
                                    │  one driver trait
     ┌──────────────┬───────────────┼───────────────┬──────────────┐
   mock/file   OpenPort 2.0      SocketCAN      K-line (ELM327   …future:
   (offline)   (J2534/CDC-ACM)   (CANable/…)    + own firmware)  J2534 generic
```

## Supported interfaces (planned for v1)

| Driver | Hardware | Status |
|---|---|---|
| **mock/file** | none — operates on local `.bin` files | building |
| **OpenPort 2.0** | Tactrix OpenPort 2.0 (genuine or clone), CDC-ACM | building |
| **SocketCAN** | any Linux CAN (CANable, PCAN, …) | building |
| **K-line (ELM327)** | ELM327 or OpenTune's own ESP32/Arduino firmware | building |

## ECU coverage (first targets)

OpenTune is **calibration-focused**. First targets are the well-understood Bosch families:

| ECU | Example engines | Read | Flash (maps) |
|---|---|---|---|
| Bosch **EDC16** (incl. U34, C-series) | VAG 1.9 TDI PD, Toyota 1.4 D-4D | OBD | OBD |
| Bosch **EDC15C6** | older CDI diesels | K-line | K-line |
| Bosch **EDC17** | later common-rail diesels | bench/boot for full read | OBD |
| Continental/Siemens **Simos PCR2.1** | VAG 1.6 TDI CR | bench/boot | OBD |

The OBD-only interfaces move data in and out; obtaining a complete original from a
readout-protected ECU may require a bench/boot tool, which OpenTune does not provide.

## Scope — what OpenTune will *not* do

To stay a legitimate, hostable project, OpenTune deliberately leaves out:

- immobiliser removal / defeat
- odometer manipulation
- emissions-control defeat (DPF/EGR/AdBlue "off")

No manufacturer firmware or security files are included in this repository.

## Building

```sh
# Rust stable (see rust-toolchain.toml)
cargo build --workspace
cargo test --workspace
cargo run -p opentune-cli -- --help
```

## Safety

- OpenTune **always** takes a verified backup before any write, checks battery voltage, and
  reads back and verifies after writing. These rules live in the core, not in individual drivers.
- A failed flash can leave an ECU non-functional. Keep a battery charger connected, never
  interrupt a write, and keep your original backup safe.
- You are responsible for complying with the vehicle, emissions and road-traffic laws where you live.

## Licence

- Software and firmware: **GPL-3.0-or-later** (see [LICENSE](LICENSE)).
- Hardware design files (when added): **CERN-OHL-S-2.0**.

## Status & roadmap

See [ROADMAP.md](ROADMAP.md) — progress is tracked there.
