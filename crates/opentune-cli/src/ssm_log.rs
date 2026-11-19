//! `log --transport ssm|ssm-can`: Subaru SSM2 logging.
//!
//! `ssm` frames SSM2 packets on the driver's raw K-line
//! ([`Interface::kline`](opentune_core::Interface::kline): `kkl`,
//! `kline-bridge`), opened without a wake-up at 4800 baud. `ssm-can` sends
//! the SSM payloads over the driver's ISO-TP transport on `7E0`/`7E8`
//! (`7E1`/`7E9` for the TCU). `--driver ssmsim` runs either mode against the
//! simulated module in `opentune_core::protocol::ssmsim`.
//!
//! Before logging, `BF` identifies the module and its capability bitmap
//! selects the `logger.xml` parameters it supports (see
//! `opentune_logdef::ssm`). Nothing here writes to the module.
use anyhow::{anyhow, bail, Result};
use clap::{Args, ValueEnum};
use opentune_core::protocol::ssm;
use opentune_core::protocol::ssmsim::{SsmSim, SsmSimConfig};
use opentune_core::{ConnectConfig, Interface, RawDiag, Registry};
use opentune_logdef::ssm::SsmWire;
use opentune_logdef::{FastLogConfig, LogDef, SsmTarget};

/// The pseudo-driver id of the simulated SSM module.
pub const SIM_DRIVER: &str = "ssmsim";

/// What `log` speaks to the module.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum LogTransport {
    /// UDS / OBD-II over the driver's diagnostic transport.
    #[default]
    Uds,
    /// Subaru SSM2 over the driver's raw K-line.
    Ssm,
    /// Subaru SSM over ISO-TP (CAN 7E0/7E8).
    SsmCan,
}

/// The `log` flags that choose the wire protocol and tune SSM.
#[derive(Args, Debug, Clone)]
pub struct SsmLogArgs {
    /// Wire protocol: `uds` (UDS/OBD-II over the driver's diagnostic
    /// transport), `ssm` (Subaru SSM2 over the driver's raw K-line: `kkl`,
    /// `kline-bridge`) or `ssm-can` (SSM over ISO-TP 7E0/7E8). `--driver
    /// ssmsim` runs either SSM mode against a simulated module.
    #[arg(long, value_enum, default_value_t = LogTransport::Uds)]
    pub transport: LogTransport,
    /// SSM module: `ecu` (engine; K-line 0x10, CAN 7E0) or `tcu`
    /// (transmission; 0x18, 7E1).
    #[arg(long, value_name = "ecu|tcu", default_value = "ecu")]
    pub ssm_target: String,
    /// SSM: largest request/reply packet in bytes; A8 address reads split to
    /// stay under it (80 = 24 addresses per read, which fits the K-line
    /// bridge).
    #[arg(long, value_name = "BYTES", default_value_t = ssm::DEFAULT_MAX_PACKET_LEN)]
    pub ssm_max_packet: usize,
}

impl SsmLogArgs {
    /// The `--ssm-target` module.
    pub fn target(&self) -> Result<SsmTarget> {
        self.ssm_target
            .parse()
            .map_err(|e: String| anyhow!("--ssm-target: {e}"))
    }

    /// The wire for `--transport ssm|ssm-can` (`None` for `uds`).
    pub fn wire(&self) -> Option<SsmWire> {
        match self.transport {
            LogTransport::Uds => None,
            LogTransport::Ssm => Some(SsmWire::KLine),
            LogTransport::SsmCan => Some(SsmWire::Can),
        }
    }
}

/// Open the SSM request seam for `log`, owning the driver (or simulator).
/// `None` for `--transport uds`: the caller opens its UDS driver as before.
pub fn open(
    reg: &Registry,
    driver: &str,
    store: Option<&std::path::Path>,
    timeout_ms: u64,
    args: &SsmLogArgs,
) -> Result<Option<Box<dyn RawDiag>>> {
    let Some(wire) = args.wire() else {
        if driver == SIM_DRIVER {
            bail!("--driver {SIM_DRIVER} speaks SSM: add --transport ssm or ssm-can");
        }
        return Ok(None);
    };
    let target = args.target()?;

    let mut drv: Box<dyn Interface> = if driver == SIM_DRIVER {
        Box::new(SsmSim::new(SsmSimConfig::demo()))
    } else {
        reg.make(driver)
            .ok_or_else(|| anyhow!("unknown driver '{driver}'"))?
    };
    let mut cfg = ConnectConfig::default();
    if let Some(store) = store {
        cfg.options
            .insert("coding_store".into(), store.to_string_lossy().into_owned());
    }
    if let Ok(port) = std::env::var("OPENTUNE_PORT") {
        if !port.trim().is_empty() {
            cfg.options.insert("port".into(), port);
        }
    }
    if wire == SsmWire::Can {
        if let Ok(baud) = std::env::var("OPENTUNE_BAUD") {
            if !baud.trim().is_empty() {
                cfg.options.insert("baud".into(), baud);
            }
        }
    }
    // SSM2 on K-line needs no wake-up and always runs at 4800 baud.
    wire.configure(&mut cfg, target);
    drv.open(&cfg).map_err(|e| anyhow!("open {driver}: {e}"))?;
    wire.prepare(drv.as_mut()).map_err(|e| anyhow!("{e}"))?;
    Ok(Some(Box::new(
        move |req: &[u8]| -> Result<Vec<u8>, String> {
            wire.with_transport(drv.as_mut(), target, |t| {
                t.request(req, timeout_ms).map_err(|e| e.to_string())
            })?
        },
    )))
}

/// For SSM: identify the module, keep the parameters it supports and switch
/// the session to batched `A8` reads. `--transport uds` passes through.
pub fn prepare(
    diag: &mut dyn RawDiag,
    def: LogDef,
    fast: FastLogConfig,
    args: &SsmLogArgs,
    ids: &[&str],
) -> Result<(LogDef, FastLogConfig)> {
    if args.transport == LogTransport::Uds {
        return Ok((def, fast));
    }
    if fast.enabled {
        bail!("--fast is UDS 0x2C; SSM always batches its reads (see --ssm-max-packet)");
    }
    let target = args.target()?;
    let p = opentune_logdef::ssm::prepare(diag, &def, fast, target, args.ssm_max_packet, ids)
        .map_err(|e| anyhow!("{e}"))?;
    eprintln!(
        "ssm: {target:?} SSM ID {} ROM ID {} ({} capability byte(s))",
        p.init.ssm_id_hex(),
        p.init.rom_id_hex(),
        p.init.capabilities.len()
    );
    for n in &p.notes {
        eprintln!("  note: {n}");
    }
    for id in &p.not_logged {
        eprintln!("  warning: '{id}' is not logged (no supported SSM parameter by that id)");
    }
    Ok((p.def, p.cfg))
}
