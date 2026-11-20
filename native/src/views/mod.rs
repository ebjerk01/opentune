//! The sidebar destinations.
//!
//! * Core views (`View::Flash` .. `View::Controllers`) have their own `ui`
//!   signatures and are routed by `app.rs`.
//! * Built-in [`NativeView`](crate::host::NativeView)s come from
//!   [`builtin::views`] (`View::Builtin(i)`): one module each, rendered with a
//!   `Host` like plugin views. Not yet ported ones are scaffold stubs.
//! * Plugin views come from the embedding build (`View::Plugin(i)`).
//!
//! Sidebar groups run Calibration, Vehicle, Reference, Help, custom groups,
//! then Bench; inside a group, entries follow [`NAV_ORDER`].

pub mod builtin;
pub mod controllers;
pub mod drivers;
pub mod ecudb;
pub mod flash;
pub mod logview;
pub mod maps;
pub mod project;
pub mod vault;

// Built-in NativeViews (one module each; see `builtin`).
pub mod actuator;
pub mod adaptation;
pub mod analyze;
pub mod autoscan;
pub mod candump;
pub mod coding;
pub mod compare;
pub mod dashboard;
pub mod debug;
pub mod drive;
pub mod guide;
pub mod haldex;
pub mod inspection;
pub mod livelog;
pub mod sa2;
pub mod service;
pub mod ts;
pub mod utilities;
pub mod wizard;

use crate::host::ViewGroup;
use crate::icons;

/// Sidebar order by view id (core `--view` names and built-in ids). An id
/// not listed goes last in its group (plugin views keep their own order).
pub const NAV_ORDER: &[&str] = &[
    // Calibration
    "flash",
    "analyze",
    "maps",
    "compare",
    "project",
    "vault",
    "drive",
    "log",
    // Vehicle (Bench until vehicle-tested)
    "dashboard",
    "inspection",
    "livelog",
    "autoscan",
    "coding",
    "adaptation",
    "actuator",
    "service",
    "haldex",
    "candump",
    "ts",
    "wizard",
    // Reference
    "ecudb",
    "drivers",
    "controllers",
    "utilities",
    "sa2",
    // Help
    "guide",
    // Bench only
    "debug",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum View {
    #[default]
    Flash,
    Maps,
    Project,
    Vault,
    Log,
    EcuDb,
    Drivers,
    Controllers,
    /// A built-in `NativeView` (index into the registered built-ins).
    Builtin(u16),
    /// A view supplied by an embedding build (index into its view list).
    Plugin(u16),
}

impl View {
    /// The core views, in no particular order (the sidebar uses NAV_ORDER).
    pub const CORE: [View; 8] = [
        View::Flash,
        View::Maps,
        View::Project,
        View::Vault,
        View::Log,
        View::EcuDb,
        View::Drivers,
        View::Controllers,
    ];

    /// `--view` names of the core views (built-ins are looked up by id).
    pub fn parse(s: &str) -> Option<View> {
        View::CORE.into_iter().find(|v| v.id() == s)
    }

    /// The core view's id (`--view` name).
    pub fn id(self) -> &'static str {
        match self {
            View::Flash => "flash",
            View::Maps => "maps",
            View::Project => "project",
            View::Vault => "vault",
            View::Log => "log",
            View::EcuDb => "ecudb",
            View::Drivers => "drivers",
            View::Controllers => "controllers",
            View::Builtin(_) | View::Plugin(_) => "",
        }
    }

    /// The sidebar group of a core view.
    pub fn group(self) -> ViewGroup {
        match self {
            View::EcuDb | View::Drivers | View::Controllers => ViewGroup::Reference,
            _ => ViewGroup::Calibration,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            View::Flash => "Flash dump",
            View::Maps => "Maps",
            View::Project => "Project",
            View::Vault => "Vault",
            View::Log => "Log viewer",
            View::EcuDb => "ECU database",
            View::Drivers => "Drivers",
            View::Controllers => "Controllers",
            View::Builtin(_) | View::Plugin(_) => "Plugin",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            View::Flash => icons::ROWS,
            View::Maps => icons::STACK,
            View::Project => icons::KANBAN,
            View::Vault => icons::LOCK,
            View::Log => icons::LIST_DASHES,
            View::EcuDb => icons::CPU,
            View::Drivers => icons::WRENCH,
            View::Controllers => icons::CIRCUITRY,
            View::Builtin(_) | View::Plugin(_) => icons::SQUARES_FOUR,
        }
    }
}

/// Sidebar group order (custom groups go between Help and Bench).
pub fn group_rank(g: &ViewGroup) -> usize {
    match g {
        ViewGroup::Workspace | ViewGroup::Calibration => 0,
        ViewGroup::Vehicle => 1,
        ViewGroup::Reference => 2,
        ViewGroup::Help => 3,
        ViewGroup::Custom(_) => 4,
        ViewGroup::Bench => 5,
    }
}

/// Position of `id` in [`NAV_ORDER`] (unlisted ids sort last).
pub fn nav_rank(id: &str) -> usize {
    NAV_ORDER
        .iter()
        .position(|x| *x == id)
        .unwrap_or(NAV_ORDER.len())
}
