//! What a command needs at run time: the driver registry.
use crate::cli::ConnectArgs;
use anyhow::{anyhow, Result};
use opentune_core::{ConnectConfig, Interface, Registry};

/// Run-time context handed to every command.
pub struct CliContext {
    registry: Registry,
}

impl CliContext {
    pub fn new(registry: Registry) -> Self {
        Self { registry }
    }

    /// The driver registry.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Open the driver named by `--driver` with the shared `--file`/`--base`
    /// options.
    pub fn open_connect(&self, conn: &ConnectArgs) -> Result<Box<dyn Interface>> {
        let mut drv = self
            .registry
            .make(&conn.driver)
            .ok_or_else(|| anyhow!("unknown driver '{}'", conn.driver))?;
        let mut cfg = ConnectConfig::default();
        if let Some(f) = &conn.file {
            cfg.options
                .insert("file".into(), f.to_string_lossy().into_owned());
        }
        cfg.options.insert("base".into(), conn.base.clone());
        drv.open(&cfg)
            .map_err(|e| anyhow!("open {}: {e}", conn.driver))?;
        Ok(drv)
    }
}
