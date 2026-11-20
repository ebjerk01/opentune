//! Small shared widgets the feature panels reuse (so two panels never grow
//! two copies): the definition pack picker, the GPS card, capability gating
//! and the CSV recorder. Each is generic: it knows nothing about the panel
//! that hosts it.

pub mod caps;
pub mod csv;
pub mod gps;
pub mod packs;
