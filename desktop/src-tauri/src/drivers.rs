//! The driver registry: every driver a release can talk to, opening one for an
//! adapter binding or an offline image, and the sidebar adapter probe.

use crate::{adapter, state::DebugLog};
use serde::Serialize;
use serde_json::Value;

// The registry, the driver openers and the probe itself live in
// `opentune_hub::drivers`, shared with other front ends.
pub use opentune_hub::drivers::open_registry_driver;
pub(crate) use opentune_hub::drivers::{open_file, probe_json, registry, SSM_SIM_DRIVER};

/// Probe for an adapter: open the selected driver fresh (which, for
/// `elm327obd`, runs the ELM327 handshake) and report whether it answered, its
/// firmware banner, battery voltage and capabilities. A general app-wide command
/// (not debug-only): the sidebar shows the adapter status everywhere. When
/// another adapter operation is running it reports `busy` instead of probing.
#[tauri::command]
pub async fn probe_adapter(
    driver: String,
    port: Option<String>,
    baud: Option<String>,
    bus: Option<String>,
    log: tauri::State<'_, DebugLog>,
    hub: tauri::State<'_, adapter::AdapterHub>,
) -> Result<Value, String> {
    let key = adapter::ConnKey::new(
        &driver,
        port.as_deref(),
        baud.as_deref(),
        bus.as_deref(),
        adapter::Addr::ENGINE,
    );
    let (result, line) = probe_json(&hub, key, &driver);
    if let Some(line) = line {
        log.push(line);
    }
    Ok(result)
}

#[derive(Serialize)]
pub(crate) struct DriverInfo {
    id: String,
    name: String,
}

#[tauri::command]
pub fn list_drivers() -> Vec<DriverInfo> {
    registry()
        .list()
        .iter()
        .map(|e| DriverInfo {
            id: e.id.to_string(),
            name: e.display_name.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> Vec<String> {
        list_drivers().into_iter().map(|d| d.id).collect()
    }

    /// The Bench-tier drivers appear in alpha builds only.
    #[test]
    fn bench_tier_drivers_follow_the_alpha_feature() {
        let ids = ids();
        assert!(ids.iter().any(|i| i == "mock"));
        for bench in ["isotp-ble", "doip", "esp32-opentune"] {
            assert_eq!(
                ids.iter().any(|i| i == bench),
                cfg!(feature = "alpha"),
                "{bench} must be listed exactly when the alpha feature is on"
            );
        }
    }
}
