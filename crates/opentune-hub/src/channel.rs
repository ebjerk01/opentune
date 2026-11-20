//! The build channel (stable / alpha / an embedder's own name) that gates
//! the debug / live-engine commands and the Bench-tier seed/key loader.

/// Build channel reported to the UI (managed state).
///
/// The public binary uses [`BuildChannel::compiled`]: `"alpha"` when compiled
/// with the `alpha` feature (the Debug panel is available), else `"stable"`. An
/// embedder may set its own channel name through `DesktopConfig::channel`.
/// Every channel other than `"stable"` enables the debug/live-engine commands,
/// and the UI shows its pre-release sections for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildChannel(String);

impl BuildChannel {
    /// The default release channel: debug commands are refused.
    pub const STABLE: &'static str = "stable";
    /// The public pre-release channel (`--features alpha`).
    pub const ALPHA: &'static str = "alpha";
    /// The channel of a plugin build that ships `tier: "bench"` panels (the UI
    /// shows its Bench section for `alpha` and `bench`).
    pub const BENCH: &'static str = "bench";

    /// A named channel (an embedder's own name, or one of the constants).
    pub fn new(name: impl Into<String>) -> Self {
        BuildChannel(name.into())
    }

    /// The channel this crate was compiled for: `alpha` with the `alpha`
    /// feature, else `stable`.
    pub fn compiled() -> Self {
        if cfg!(feature = "alpha") {
            Self::new(Self::ALPHA)
        } else {
            Self::new(Self::STABLE)
        }
    }

    /// The channel name as the UI sees it.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Debug / live-engine commands run on every channel except `stable`.
    pub fn debug_enabled(&self) -> bool {
        self.0 != Self::STABLE
    }
}

impl Default for BuildChannel {
    fn default() -> Self {
        Self::compiled()
    }
}
