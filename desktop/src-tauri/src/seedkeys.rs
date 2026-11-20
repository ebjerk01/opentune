//! Plugin-supplied seed/key algorithms (UDS / KWP SecurityAccess).
//!
//! OpenTune ships **no** OEM seed/key algorithm, and this module adds none. It
//! is an extension point: an embedding app (a plugin build) passes resolvers in
//! [`DesktopConfig::seedkeys`](crate::DesktopConfig::seedkeys) and the core asks
//! them, by id, wherever it can unlock a module:
//!
//! * a service script that names a `seedkey` id ([`crate::feat_service`]);
//! * the Utilities seed/key calculator, for an id that is not one of its
//!   generic transforms (the result is labelled as plugin-supplied);
//! * a plugin's own read/flash commands, which attach a resolved handler to a
//!   hub-opened driver with [`crate::adapter::apply_seed_key`].
//!
//! The plugin owns the algorithm and its provenance; when no resolver knows an
//! id, every caller keeps the honest "plan only / not available" behaviour.
//!
//! One generic handler is built in: the VAG **SA2 interpreter**
//! (`opentune-sa2`), which computes nothing until the *user* supplies a
//! script. After the plugin resolvers, `sa2` resolves to the script the user
//! armed in the SA2 panel ([`SeedKeys::arm_sa2`], held in memory for this run
//! only), `sa2:<hex>` to the script given inline in the id, and `sa2:@<file>`
//! to a script file (raw bytes, hex or SA2 assembler, at most
//! [`MAX_SCRIPT_FILE`](opentune_hub::sa2script::MAX_SCRIPT_FILE) bytes) — the CLI's
//! `--seedkey sa2:<hex>` / `sa2:@<file>` forms. No script ships with OpenTune.
//!
//! Last, `unlockecu:<EcuName>` resolves through the user's UnlockECU-format
//! definitions loader when the embedder configured one
//! ([`DesktopConfig::seedkey_loader`](crate::DesktopConfig::seedkey_loader),
//! Bench tier: alpha / bench channels only; see `docs/SEEDKEY-LOADER.md`).
//!
//! The lookup itself lives in [`opentune_hub::seedkeys`], shared with other
//! front ends; this module re-exports it so the `opentune_desktop::seedkeys::*`
//! paths stay stable.

pub use opentune_hub::seedkeys::{
    loader_for, unavailable, ArmedSa2, SeedKeyResolver, SeedKeys, SA2_ID,
};

pub(crate) use opentune_hub::seedkeys::compute;

#[cfg(test)]
pub(crate) use opentune_hub::seedkeys::testkeys;
