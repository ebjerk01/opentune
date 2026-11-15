//! Diagnostic protocol layer used by CAN/K-line drivers.
//!
//! - [`isotp`]: ISO 15765-2 transport over a raw CAN link.
//! - [`diag`]: the request/response [`DiagTransport`] trait + NRC helpers.
//! - [`uds`] / [`kwp2000`]: service clients over a `DiagTransport`.
//! - [`seedkey`]: the seed→key plugin interface for security access.
pub mod diag;
pub mod isotp;
pub mod kwp2000;
pub mod seedkey;
pub mod uds;

pub use diag::{check_response, nrc_name, DiagTransport};
pub use isotp::{CanLink, IsoTp};
pub use kwp2000::Kwp2000Client;
pub use seedkey::SeedKey;
pub use uds::UdsClient;
