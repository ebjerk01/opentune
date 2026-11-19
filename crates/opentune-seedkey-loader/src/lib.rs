//! # opentune-seedkey-loader
//!
//! An **interop loader** for seed/key definition files in the format of
//! `jglim/UnlockECU` (MIT), wired into OpenTune's existing [`SeedKey`] hook
//! (`Interface::set_seed_key`, the CLI's `--seedkey`, the desktop's
//! `SeedKeys`).
//!
//! [`SeedKey`]: opentune_core::protocol::seedkey::SeedKey
//!
//! ## What OpenTune ships: no algorithm, no data
//!
//! This crate contains **no** seed/key algorithm ("provider") and **no**
//! definition, table or constant. It reads a definitions file the *user*
//! supplies ([`defs`]), refuses anything in the immobiliser / anti-theft class
//! ([`scope`]), and passes each computation to a provider the *user*
//! configures:
//!
//! * an **external provider program** the user installs ([`provider`]): run by
//!   absolute path with no shell, one JSON request on stdin, one JSON response
//!   on stdout, a timeout and bounded output; or
//! * `builtin:sa2`: the generic SA2 interpreter (`opentune-sa2`) for
//!   definitions whose `Provider` is `VolkswagenSA2`, running the script the
//!   user's definition carries in its `InstructionTape` parameter.
//!
//! ## Off by default
//!
//! Nothing is loaded unless the user names **both** a definitions file and a
//! provider ([`LoaderConfig`]); there is no search path and no auto-discovery.
//! The resulting [`Loader`] answers seed/key ids `unlockecu:<EcuName or
//! alias>` and picks the definition whose `AccessLevel` equals the level the
//! driver asks for.
//!
//! See `docs/SEEDKEY-LOADER.md`.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod defs;
pub mod loader;
pub mod provider;
pub mod scope;

pub use defs::{Definition, DefinitionSet, ParamValue, Parameter, Refusal, Rejection};
pub use loader::{
    LoadedSeedKey, Loader, LoaderConfig, ProviderSpec, BUILTIN_SA2, ID_PREFIX, SA2_PROVIDER,
    SA2_TAPE_PARAM,
};
pub use provider::{ExternalProvider, DEFAULT_TIMEOUT, MAX_TIMEOUT, PROTOCOL};
