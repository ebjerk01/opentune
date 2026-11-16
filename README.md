# OpenTune

**Open-source ECU reading, flashing and calibration for people who want to tune their own cars with cheap, open hardware.**

> ⚠️ **Early alpha — not ready for use on a real car.** OpenTune is in active, early development.
> The offline calibration tooling (on a `.bin`) works today (see below); the hardware drivers are
> implemented but **unproven on a car** and the desktop/Android apps are **unreleased alpha builds**
> that have not been runtime-hardened. Reading, writing or reflashing an engine or transmission
> control unit can permanently damage it — **do not flash a real vehicle with this yet.** Expect
> breaking changes. Use entirely at your own risk — see [Safety](#safety).

## What works today (offline, on a `.bin`)

- **Identify** an ECU from its reported part/SW numbers against ECU definitions + a **1034-entry
  catalog** (998 from comma.ai opendbc + 36 hand-curated European entries across
  VAG/Mercedes/BMW/Ford/PSA/Opel/Fiat/Volvo/Renault, with VAG part-number match keys).
- **Checksums** — a `ChecksumAlgo` engine with **Bosch EDC16 (MPC56x), EDC17 (TriCore) and
  Continental Simos PCR2.1 solved** (directory-walking verify + correct, validated against real
  full dumps) plus **VAG EDC15** (verify + correct, clean-room from GPL sources; Mercedes EDC15C6
  proven *not* derivable from the application dump — its checksum runs in the tester-uploaded flash
  programming driver, not the flash itself — so the module **refuses** it); `fix` corrects an uncorrected tune, `fix --reference`
  bootstraps EDC17's firmware-specific constant from your own stock dump, and it refuses to
  guess on anything unvalidated (anti-brick).
- **Find maps** by clustering the diff of a stock/tuned pair into candidate calibration regions.
- **Flash orchestrator** — voltage gate → verified backup → write → read-back verify, with a
  backup vault, all exercised through the mock/file driver.
- **Compare** two calibrations **map-by-map** through a definition's `MapDef`s — decoded to
  physical units (mg/stroke, °, …) with a before→after grid, not just a byte diff.
- **Import and export** the large community library of **TunerPro XDF** definitions
  (`import-xdf` / `export-xdf`, modern XML, round-trip validated).
- **Import factory ASAM A2L** calibration descriptions (`import-a2l`) — precise map
  addresses/axes/scaling straight from the OEM description; validated at scale on real
  EDC15 / Siemens PPD / BMW MSV80 files (thousands of maps each), linear scaling recovered
  from RAT_FUNC and non-linear conversions flagged, never guessed.
- A **CLI** (`opentune`): `drivers`, `info`, `identify`, `read`, `dump`, `diff`, `findmaps`,
  `compare`, `checksum verify|fix`, `flash`, `vault`, `ecudefs`, `import-xdf`, `export-xdf`,
  `import-a2l`, `export-a2l`, `vin`, `decode-part`, `scan`, `dtc`.

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
| Bosch **EDC15 / EDC15C6** | VAG 1.9 TDI (EDC15), Mercedes CDI (EDC15C6) | VAG EDC15 **solved** (verify + correct, clean-room from GPL-3.0 sources); Mercedes CR1 located-only and **refuses** (never guesses) | K-line/OBD |
| Bosch **EDC17** (TriCore) | later common-rail diesels (incl. VAG) | **solved** — verify + correct; a firmware-specific VAG block constant is bootstrapped from your own stock dump (`checksum fix --reference`) | OBD (bench/boot for full read) |
| Continental/Siemens **Simos PCR2.1** | VAG 1.6 TDI CR | **solved** (verify + correct; self-contained on-image directory) | OBD (bench/boot) |

The checksums are validated against real full dumps. Checksum work derived from any
proprietary tool is re-derived and validated independently before it ships here. EDC17's
per-block target constant is firmware-specific; rather than guess, OpenTune recovers it from
a known-good reference image (your own untouched read) and refuses when the reference doesn't
match — so a wrong file can never silently corrupt the image.

The OBD-only interfaces move data in and out; obtaining a complete original from a
readout-protected ECU may require a bench/boot tool, which OpenTune does not provide.

## Where it's heading

OpenTune is expanding beyond engine ECUs. Planned, reference-driven directions (see
[ROADMAP.md](ROADMAP.md) *Phase 4* and [`docs/IDEAS.md`](docs/IDEAS.md)):

- **Drivetrain control units** — transmission (TCU) and AWD/4WD controllers as first-class targets:
  VAG DSG (DQ250/DQ381/DL382) + Haldex, Mercedes 722.6 (NAG1), then a generalized controller-def
  schema so BMW/Ford/Mitsubishi/Volvo units drop in.
- **More checksum families** — Bosch **ME7** (petrol) and the VAG TCU schemes (CRC32-VW, JAMCRC, …).
- **Security access** — an optional, *user-enabled* seed/key (SA2) plugin set (never auto-applied).
- **Real transports** — **DoIP** (ISO-13400) beyond today's mock, and **FRF / ODX / SGO** flash-container decode.

These build on a survey of 43 open-source projects. OpenTune reuses only license-compatible code,
re-derives anything that came from proprietary tools, and ships no manufacturer firmware or keys.

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
