//! Fleet identification helpers: **VIN decoding** and **part-number parsing**.
//!
//! This crate is deliberately dependency-free and does *structural* parsing
//! only. It decodes what the standards define (ISO 3779 VIN layout, the North
//! American check digit, documented Bosch/VAG part-number shapes) and refuses
//! to guess. Anything it cannot determine is returned as `None` / an empty
//! field and, where useful, called out in a human-readable flag rather than
//! filled in with a plausible-looking invention.
//!
//! Two entry points:
//!
//! * [`decode_vin`] — parse and validate a 17-character VIN, split it into
//!   WMI / VDS / VIS, look up the manufacturer and country from a shipped WMI
//!   table, and decode the model-year character (with the unavoidable 30-year
//!   ambiguity surfaced, not hidden).
//! * [`parse_part_number`] — recognise the *shape* of VAG (e.g. `03G906021LR`)
//!   and Bosch (e.g. `0281011402`) part numbers and extract the structurally
//!   knowable groups, flagging everything that is not derivable from the number
//!   alone.
//!
//! Neither function ever panics on arbitrary input; both report their findings
//! and their uncertainties.

pub mod part;
pub mod vin;
pub mod wmi;

pub use part::{parse_part_number, PartKind, PartNumber};
pub use vin::{decode_vin, ModelYear, Vin, VinError};
pub use wmi::{lookup_wmi, WmiInfo};
