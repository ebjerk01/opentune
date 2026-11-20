//! The built-in view registry: every built-in [`NativeView`], rendered by the
//! app through the same [`Host`](crate::host::Host) as a plugin view (jobs,
//! confirms, adapter, seed keys, target).
//!
//! Each view module exports `TIER` and `create()`. A feature batch replaces
//! its own module (the scaffold stub) and keeps those two names; it never
//! touches this file, `views/mod.rs` or `app.rs`. A view graduates from
//! Bench to Public by changing its module's `TIER` (and `tier()`).
//!
//! The tier is decided HERE, at registration: on a stable channel a Bench
//! view is never constructed, so it has no sidebar entry, no `ui()` and no
//! adapter access, whatever the UI does.

use super::{
    actuator, adaptation, analyze, autoscan, candump, coding, compare, dashboard, debug, drive,
    guide, haldex, inspection, livelog, sa2, service, ts, utilities, wizard,
};
use crate::host::{bench_channel, BuildChannel, NativeView, Tier};

type Create = fn() -> Box<dyn NativeView>;

/// Every built-in view, as `(tier, constructor)`. Order does not matter: the
/// sidebar follows [`super::NAV_ORDER`].
const REGISTRY: &[(Tier, Create)] = &[
    (analyze::TIER, analyze::create),
    (compare::TIER, compare::create),
    (drive::TIER, drive::create),
    (coding::TIER, coding::create),
    (autoscan::TIER, autoscan::create),
    (adaptation::TIER, adaptation::create),
    (actuator::TIER, actuator::create),
    (service::TIER, service::create),
    (livelog::TIER, livelog::create),
    (dashboard::TIER, dashboard::create),
    (inspection::TIER, inspection::create),
    (candump::TIER, candump::create),
    (haldex::TIER, haldex::create),
    (ts::TIER, ts::create),
    (utilities::TIER, utilities::create),
    (sa2::TIER, sa2::create),
    (guide::TIER, guide::create),
    (debug::TIER, debug::create),
    (wizard::TIER, wizard::create),
];

/// Is a view of `tier` registered on `channel`?
pub fn admitted(tier: Tier, channel: &BuildChannel) -> bool {
    tier == Tier::Public || bench_channel(channel)
}

/// The built-in views this build registers on `channel`.
pub fn views(channel: &BuildChannel) -> Vec<Box<dyn NativeView>> {
    REGISTRY
        .iter()
        .filter(|(tier, _)| admitted(*tier, channel))
        .map(|(_, create)| create())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn ch(name: &str) -> BuildChannel {
        BuildChannel::new(name)
    }

    #[test]
    fn every_builtin_is_consistent_unique_and_placed() {
        let all = views(&ch(BuildChannel::BENCH));
        assert_eq!(all.len(), REGISTRY.len());
        let mut ids = BTreeSet::new();
        for (v, (tier, _)) in all.iter().zip(REGISTRY) {
            assert_eq!(v.tier(), *tier, "{}: TIER and tier() disagree", v.id());
            assert!(ids.insert(v.id().to_string()), "duplicate id {}", v.id());
            assert!(
                super::super::NAV_ORDER.contains(&v.id()),
                "{} has no place in NAV_ORDER",
                v.id()
            );
            assert!(
                super::super::View::parse(v.id()).is_none(),
                "{} shadows a core view",
                v.id()
            );
        }
    }

    #[test]
    fn bench_views_are_not_registered_on_stable() {
        let stable = views(&ch(BuildChannel::STABLE));
        assert!(stable.iter().all(|v| v.tier() == Tier::Public));
        let ids: Vec<&str> = stable.iter().map(|v| v.id()).collect();
        for public in ["analyze", "compare", "drive", "utilities", "sa2", "guide"] {
            assert!(ids.contains(&public), "{public}");
        }
        for bench in [
            "debug", "coding", "actuator", "livelog", "candump", "wizard",
        ] {
            assert!(!ids.contains(&bench), "{bench} registered on stable");
        }
        // An embedder's own channel name is not a Bench channel.
        assert_eq!(views(&ch("beta")).len(), stable.len());
        assert_eq!(views(&ch(BuildChannel::ALPHA)).len(), REGISTRY.len());
    }
}
