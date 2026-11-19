//! The sidebar destinations. `Flash` is the ported workspace; the rest are
//! being ported (see each module).

pub mod drivers;
pub mod ecudb;
pub mod flash;
pub mod maps;
pub mod placeholder;

use crate::icons;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum View {
    #[default]
    Flash,
    Maps,
    Project,
    Vault,
    Drive,
    Log,
    EcuDb,
    Drivers,
    Controllers,
    Sa2,
}

impl View {
    pub const WORKSPACE: [View; 6] = [
        View::Flash,
        View::Maps,
        View::Project,
        View::Vault,
        View::Drive,
        View::Log,
    ];
    pub const REFERENCE: [View; 4] = [View::EcuDb, View::Drivers, View::Controllers, View::Sa2];

    /// `--view` names.
    pub fn parse(s: &str) -> Option<View> {
        Some(match s {
            "flash" => View::Flash,
            "maps" => View::Maps,
            "project" => View::Project,
            "vault" => View::Vault,
            "drive" => View::Drive,
            "log" => View::Log,
            "ecudb" => View::EcuDb,
            "drivers" => View::Drivers,
            "controllers" => View::Controllers,
            "sa2" => View::Sa2,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            View::Flash => "Flash dump",
            View::Maps => "Maps",
            View::Project => "Project",
            View::Vault => "Vault",
            View::Drive => "Drive view",
            View::Log => "Log viewer",
            View::EcuDb => "ECU database",
            View::Drivers => "Drivers",
            View::Controllers => "Controllers",
            View::Sa2 => "SA2 script",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            View::Flash => icons::ROWS,
            View::Maps => icons::STACK,
            View::Project => icons::KANBAN,
            View::Vault => icons::LOCK,
            View::Drive => icons::CHART_LINE,
            View::Log => icons::LIST_DASHES,
            View::EcuDb => icons::CPU,
            View::Drivers => icons::WRENCH,
            View::Controllers => icons::CIRCUITRY,
            View::Sa2 => icons::CODE,
        }
    }
}
