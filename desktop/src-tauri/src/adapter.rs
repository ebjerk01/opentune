//! Shared live-adapter access for every diagnostic panel.
//!
//! The hub, the reused session, module addressing and the UDS session /
//! seed-key helpers live in [`opentune_hub::adapter`], shared with other front
//! ends; see that module for the safety properties they enforce. This module
//! re-exports them so the `opentune_desktop::adapter::*` paths stay stable.

pub use opentune_hub::adapter::{
    apply_seed_key, def_addr, enter_extended_session, leave_session, parse_can_id, read_addr,
    resolve, write_addr, AdapterHub, Addr, ConnKey, Opener, PollLink, Session, IDLE_RELEASE,
};

#[cfg(test)]
pub(crate) use opentune_hub::adapter::testkit;
