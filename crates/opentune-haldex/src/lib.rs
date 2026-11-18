//! BorgWarner **Haldex** AWD coupling support.
//!
//! Three independent pieces, all offline-testable:
//!
//! * [`gen4`] — a **read-only** engine for the Haldex Generation 4 controller
//!   (VAG PQ35/PQ46 era, e.g. VW Passat CC 4Motion, Tiguan, Golf Mk6 R):
//!   KWP2000 over VW TP2.0 at logical address `0x0A`. Identification,
//!   stored trouble codes and measuring-block groups. It never sends a write,
//!   coding, adaptation, routine or flash service.
//! * [`can`] + [`canlog`] — a passive decoder for the Haldex-related frames on
//!   the drivetrain CAN (lock request, lock actual, status flags, wheel
//!   speeds) and readers for can-utils `candump`, OpenTune CSV and SavvyCAN
//!   CSV logs, so captures can be summarised offline.
//! * [`openhaldex`] — a backend for the open-source **OpenHaldex** controller
//!   (the box that sits between the car and the Haldex and rewrites the lock
//!   request): its modes, its CAN broadcast/command frames, its BLE status
//!   record and its supplier-specific UDS identifiers.
//!
//! [`sim`] provides simulated units (a Gen4 TP2.0/KWP2000 controller and an
//! OpenHaldex UDS responder) for tests and the CLI's `--sim` mode.
//!
//! # Provenance and licences
//!
//! Written fresh for OpenTune (GPL-3.0-or-later); no code was copied. Facts
//! were taken from these public, licence-compatible sources (cited per item in
//! the modules and in `docs/HALDEX.md`):
//!
//! * **commaai/opendbc** `vw_pq.dbc` / `vw_mqb.dbc` (MIT, (c) 2020 Comma.ai) —
//!   PQ `Allrad_1` 0x2C0, `Bremse_4` 0x2A0, `Bremse_3` 0x4A0, `Motor_1` 0x280
//!   and MQB `ESP_19` 0x0B2 signal layouts.
//! * **Forbes-Automotive/OpenHaldex-C6** (MIT, (c) 2026 Forbes Automotive) —
//!   the published BLE contract (`documents/MOBILE_APP_OPENHALDEX.md`), the
//!   README's CAN broadcast/command frames (0x6B0 / 0x6A0), the mode set, the
//!   Gen4 TP2.0 logical address and measuring-group meanings, the Gen4/Gen5
//!   status-frame byte meanings and its public vehicle CAN logs. Some of those
//!   byte meanings come from **that project's own analysis** of the Haldex
//!   firmware; they are labelled as OpenHaldex-sourced and community
//!   confidence here, not verified by OpenTune.
//! * **dandwyer/OpenHaldex-S3** (MIT) and **ABangingDonk/OpenHaldexDue** (MIT)
//!   — cross-checks of the same frame layouts.
//!
//! None of these are real-vehicle validated by OpenTune yet; see the
//! validation states in `docs/HALDEX.md`.
pub mod can;
pub mod canlog;
pub mod gen4;
pub mod openhaldex;
pub mod sim;

pub use can::{decode_frame, Bus, HaldexFrame};
pub use canlog::{parse_log, LogFrame};
pub use gen4::{read_gen4, Gen4ReadOptions, Gen4Report};
pub use openhaldex::Mode;
