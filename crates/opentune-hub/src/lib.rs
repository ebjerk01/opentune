//! The adapter/session layer and the frontend-neutral helpers shared by
//! OpenTune's front ends (the Tauri desktop app embeds it; another front end
//! can use exactly the same code).
//!
//! * [`adapter`] — the app-wide adapter hub ([`adapter::AdapterHub`]: one
//!   worker thread owning the opened driver, one operation at a time), the
//!   reused [`adapter::Session`], module addressing ([`adapter::Addr`],
//!   [`adapter::ConnKey`], [`adapter::resolve`], [`adapter::read_addr`],
//!   [`adapter::write_addr`]) and the UDS session / seed-key helpers.
//! * [`drivers`] — the driver registry, opening a registry driver for a
//!   connection key or the mock/file driver for an offline image, and the
//!   adapter probe.
//! * [`seedkeys`] — plugin-supplied seed/key lookup ([`seedkeys::SeedKeys`])
//!   with the built-in SA2 interpreter and the optional definitions loader.
//! * [`sa2script`] — SA2 script text / file decoding and size caps.
//! * [`channel`] — the [`BuildChannel`] that gates debug / Bench-tier features.
//! * [`maps`] — map cell / axis decoding and single-cell encoding.
//! * [`vault`] — listing a backup vault directory.
//!
//! Features: `alpha` registers the Bench-tier drivers and makes
//! [`BuildChannel::compiled`] report `alpha`; `testkit` exposes the simulated
//! bus (`adapter::testkit`) and test-only seed/key resolvers
//! (`seedkeys::testkeys`) to a front end's own tests.

pub mod adapter;
pub mod channel;
pub mod drivers;
pub mod maps;
pub mod sa2script;
pub mod seedkeys;
pub mod vault;

pub use channel::BuildChannel;
