# OpenTune

**Open-source ECU reading, flashing and calibration for people who want to tune their own cars with cheap, open hardware.**

> ⚠️ **Work in progress.** The offline calibration tooling works today (see below); the
> hardware drivers are implemented but awaiting on-car testing. Reading, writing or reflashing
> an engine or transmission control unit can permanently damage it — don't flash a real car
> until a driver is proven on your hardware. Use at your own risk — see [Safety](#safety).

## What works today (offline, on a `.bin`)

- **Identify** an ECU from its reported part/SW numbers against ECU definitions + a **998-vehicle
  catalog** harvested from comma.ai opendbc.
- **Checksums** — a `ChecksumAlgo` engine with **Bosch EDC16 (MPC56x), EDC17 (TriCore) and
  Continental Simos PCR2.1 solved** (directory-walking verify + correct, validated against real
  full dumps) plus an EDC15 module; `fix` corrects an uncorrected tune, `fix --reference`
  bootstraps EDC17's firmware-specific constant from your own stock dump, and it refuses to
  guess on anything unvalidated (anti-brick).
- **Find maps** by clustering the diff of a stock/tuned pair into candidate calibration regions.
- **Flash orchestrator** — voltage gate → verified backup → write → read-back verify, with a
  backup vault, all exercised through the mock/file driver.
- **Compare** two calibrations **map-by-map** through a definition's `MapDef`s — decoded to
  physical units (mg/stroke, °, …) with a before→after grid, not just a byte diff.
- **Import and export** the large community library of **TunerPro XDF** definitions
  (`import-xdf` / `export-xdf`, modern XML, round-trip validated).
- A **CLI** (`opentune`): `drivers`, `info`, `identify`, `read`, `dump`, `diff`, `findmaps`,
  `compare`, `checksum verify|fix`, `flash`, `vault`, `ecudefs`, `import-xdf`, `export-xdf`, `dtc`.

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

## Interface drivers

| Driver | Hardware | Status |
|---|---|---|
| **mock/file** | none — operates on local `.bin` files | **working** |
| **OpenPort 2.0** | Tactrix OpenPort 2.0 (genuine or clone), CDC-ACM | implemented · awaiting hardware |
| **SocketCAN** | any Linux CAN (CANable, PCAN, …) | implemented · awaiting hardware |
| **K-line (ELM327)** | ELM327 (USB) | implemented · awaiting hardware |
| **KKL (raw K-line)** | FTDI/CH340 KKL 409.1 cable + transceiver | implemented · awaiting hardware |
| **Arduino K-line bridge** | an Arduino + L9637D running OpenTune firmware (`hardware/firmware/`) | implemented · awaiting hardware |
| **slcan** | CANable / CANtact USB-CAN | implemented · awaiting hardware |

See [`docs/CHEAP-INTERFACES.md`](docs/CHEAP-INTERFACES.md) for which cheap/clone adapters map to which driver.

## ECU coverage (first targets)

OpenTune is **calibration-focused**. First targets are the well-understood Bosch families:

| ECU | Example engines | Checksum | Bus |
|---|---|---|---|
| Bosch **EDC16** (MPC56x) | VAG 1.9/2.0 TDI PD (03G906…), BMW/KIA diesels | **solved** (verify + correct) | OBD/CAN |
| Bosch **EDC15C6** | older CDI diesels | field located (word-wise solve kept private) | K-line |
| Bosch **EDC17** (TriCore) | later common-rail diesels (incl. VAG) | **solved** — verify + correct; a firmware-specific VAG block constant is bootstrapped from your own stock dump (`checksum fix --reference`) | OBD (bench/boot for full read) |
| Continental/Siemens **Simos PCR2.1** | VAG 1.6 TDI CR | **solved** (verify + correct; self-contained on-image directory) | OBD (bench/boot) |

The checksums are validated against real full dumps. Checksum work derived from any
proprietary tool is re-derived and validated independently before it ships here. EDC17's
per-block target constant is firmware-specific; rather than guess, OpenTune recovers it from
a known-good reference image (your own untouched read) and refuses when the reference doesn't
match — so a wrong file can never silently corrupt the image.

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
