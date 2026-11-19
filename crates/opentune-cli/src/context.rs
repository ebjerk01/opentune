//! [`CliContext`]: what a command needs at run time.
use crate::cli::ConnectArgs;
use crate::{ChecksumResolver, CliConfig, SeedKeyResolver};
use anyhow::{anyhow, bail, Result};
use opentune_checksum::ChecksumAlgo;
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::{ConnectConfig, Interface, MemoryRegion, Registry};

/// Run-time context handed to every command, core or extra: the driver
/// registry (public drivers plus the embedder's) and the embedder's checksum
/// and seed-key resolvers.
pub struct CliContext {
    registry: Registry,
    checksums: Vec<ChecksumResolver>,
    seedkeys: Vec<SeedKeyResolver>,
}

impl CliContext {
    /// Build the context a [`CliConfig`] describes: the public registry with
    /// `cfg.drivers` appended, and `cfg`'s resolvers.
    pub fn from_config(cfg: &CliConfig) -> Self {
        let mut registry = crate::registry::public_registry();
        for &(id, name, make) in &cfg.drivers {
            registry.register(id, name, make);
        }
        Self {
            registry,
            checksums: cfg.checksums.clone(),
            seedkeys: cfg.seedkeys.clone(),
        }
    }

    /// The driver registry (public drivers, then the embedder's).
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Resolve a checksum algorithm id: the embedder's resolvers first, in
    /// order, then the public `opentune_checksum::get`.
    pub fn checksum(&self, id: &str) -> Option<Box<dyn ChecksumAlgo>> {
        self.checksums
            .iter()
            .find_map(|r| r(id))
            .or_else(|| opentune_checksum::get(id))
    }

    /// [`checksum`](Self::checksum), erroring on an unknown id.
    pub fn require_checksum(&self, id: &str) -> Result<Box<dyn ChecksumAlgo>> {
        self.checksum(id)
            .ok_or_else(|| anyhow!("unknown checksum algo '{id}'"))
    }

    /// Resolve a seed-key handler id through the embedder's resolvers, in
    /// order, then `sa2:<hex>` / `sa2:@<file>` (the SA2 VM running a script
    /// the user supplies in the id), then the simulators' test-only handlers
    /// (`udssim-test`, `seed XOR 0xA5`, and `kwpsim-test` for the synthetic
    /// KWP module — each unlocks only its simulator). The public build ships no
    /// algorithm or script.
    pub fn seed_key(&self, id: &str) -> Option<Box<dyn SeedKey>> {
        self.seedkeys
            .iter()
            .find_map(|r| r(id))
            .or_else(|| match crate::seedkey_cmd::sa2_from_id(id) {
                Some(Ok(sk)) => Some(Box::new(sk) as Box<dyn SeedKey>),
                _ => None,
            })
            .or_else(|| opentune_udssim::driver::test_seed_key(id))
            .or_else(|| opentune_adaptation::sim::test_seed_key(id))
    }

    /// [`seed_key`](Self::seed_key), erroring on an unknown id (or on an
    /// `sa2:` id whose script cannot be read).
    pub fn require_seed_key(&self, id: &str) -> Result<Box<dyn SeedKey>> {
        if let Some(sk) = self.seedkeys.iter().find_map(|r| r(id)) {
            return Ok(sk);
        }
        if let Some(sa2) = crate::seedkey_cmd::sa2_from_id(id) {
            return Ok(Box::new(sa2?));
        }
        self.seed_key(id).ok_or_else(|| {
            anyhow!(
                "unknown seed-key id '{id}': no seed-key resolver in this build provides it \
                 (OpenTune itself ships no seed/key algorithms)"
            )
        })
    }

    /// Resolve seed-key `id` and hand it to `drv` with
    /// [`Interface::set_seed_key`]. Errors when the id is unknown or the driver
    /// has no security-access unlock step (`set_seed_key` returned `false`).
    pub fn attach_seed_key(&self, drv: &mut dyn Interface, driver: &str, id: &str) -> Result<()> {
        let sk = self.require_seed_key(id)?;
        if !drv.set_seed_key(sk) {
            bail!(
                "driver '{driver}' has no security-access unlock step; \
                 seed key '{id}' cannot be attached to it"
            );
        }
        Ok(())
    }

    /// Make registry driver `driver`, attach seed key `seedkey` (if any) and
    /// open it with `cfg`. The seed key is resolved before the driver is made,
    /// so an unknown id fails without touching hardware.
    pub fn open_driver(
        &self,
        driver: &str,
        cfg: &ConnectConfig,
        seedkey: Option<&str>,
    ) -> Result<Box<dyn Interface>> {
        if let Some(id) = seedkey {
            self.require_seed_key(id)?;
        }
        let mut drv = self
            .registry
            .make(driver)
            .ok_or_else(|| anyhow!("unknown driver '{driver}'"))?;
        if let Some(id) = seedkey {
            self.attach_seed_key(drv.as_mut(), driver, id)?;
        }
        drv.open(cfg).map_err(|e| anyhow!("open {driver}: {e}"))?;
        Ok(drv)
    }

    /// Open the driver a [`ConnectArgs`] names (`--driver`, `--seedkey`) with
    /// the config [`ConnectArgs::config`] builds from its adapter flags
    /// (`--port --baud --bus --tx-id --rx-id --opt`, and `--file --base` for
    /// the mock). Bad flag values fail before any driver is made.
    pub fn open_connect(&self, conn: &ConnectArgs) -> Result<Box<dyn Interface>> {
        let cfg = conn.config()?;
        self.open_driver(&conn.driver, &cfg, conn.seedkey.as_deref())
    }

    /// Dry run of a block read on the opened `drv` (registry id `driver`, for
    /// the message): the exact lines [`Interface::plan_read_memory`] says the
    /// read would write, **without sending anything**. Errors when the driver
    /// cannot plan (`None`) or when the read would be refused before reaching
    /// the bus.
    pub fn plan_read(
        &self,
        drv: &mut dyn Interface,
        driver: &str,
        region: MemoryRegion,
    ) -> Result<Vec<String>> {
        match drv.plan_read_memory(region) {
            Some(plan) => plan.map_err(|e| anyhow!("plan read with {driver}: {e}")),
            None => bail!(
                "driver cannot plan: '{driver}' offers no dry run of a memory read \
                 (nothing was sent)"
            ),
        }
    }
}
