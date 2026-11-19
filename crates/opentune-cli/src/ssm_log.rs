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
use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::ssm::{self, SsmKLine};
use opentune_core::protocol::ssmsim::{SsmSim, SsmSimConfig};
use opentune_core::{ConnectConfig, RawDiag, Registry};
use opentune_logdef::{FastLogConfig, LogDef, SsmPollConfig, SsmTarget};

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
        match self.ssm_target.trim().to_ascii_lowercase().as_str() {
            "ecu" | "engine" => Ok(SsmTarget::Ecu),
            "tcu" | "transmission" => Ok(SsmTarget::Tcu),
            other => bail!("--ssm-target {other}: expected ecu or tcu"),
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
    if args.transport == LogTransport::Uds {
        if driver == SIM_DRIVER {
            bail!("--driver {SIM_DRIVER} speaks SSM: add --transport ssm or ssm-can");
        }
        return Ok(None);
    }
    let target = args.target()?;
    let addr = target.address();
    let kline = args.transport == LogTransport::Ssm;

    if driver == SIM_DRIVER {
        let mut cfg = SsmSimConfig::demo();
        cfg.address = addr;
        let mut sim = SsmSim::new(cfg);
        let diag: Box<dyn RawDiag> = if kline {
            Box::new(move |req: &[u8]| -> Result<Vec<u8>, String> {
                SsmKLine::new(&mut sim, addr)
                    .request(req, timeout_ms)
                    .map_err(|e| e.to_string())
            })
        } else {
            Box::new(move |req: &[u8]| -> Result<Vec<u8>, String> {
                DiagTransport::request(&mut sim, req, timeout_ms).map_err(|e| e.to_string())
            })
        };
        return Ok(Some(diag));
    }

    let mut drv = reg
        .make(driver)
        .ok_or_else(|| anyhow!("unknown driver '{driver}'"))?;
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
    if kline {
        // SSM2 needs no wake-up and always runs at 4800 baud.
        cfg.bitrate = ssm::BAUD;
        cfg.tx_id = u32::from(addr);
        cfg.options.insert("init".into(), "none".into());
        cfg.options.insert("baud".into(), ssm::BAUD.to_string());
    } else {
        (cfg.tx_id, cfg.rx_id) = target.can_ids();
        if let Ok(baud) = std::env::var("OPENTUNE_BAUD") {
            if !baud.trim().is_empty() {
                cfg.options.insert("baud".into(), baud);
            }
        }
    }
    drv.open(&cfg).map_err(|e| anyhow!("open {driver}: {e}"))?;

    let diag: Box<dyn RawDiag> = if kline {
        let link = drv.kline().ok_or_else(|| {
            anyhow!(
                "driver '{driver}' offers no raw K-line; SSM over K-line needs kkl or \
                 kline-bridge (or --driver {SIM_DRIVER})"
            )
        })?;
        link.set_baud(ssm::BAUD)
            .and_then(|()| link.clear_input())
            .map_err(|e| anyhow!("prepare the K-line for SSM: {e}"))?;
        Box::new(move |req: &[u8]| -> Result<Vec<u8>, String> {
            let link = drv.kline().ok_or("the K-line is closed")?;
            SsmKLine::new(link, addr)
                .request(req, timeout_ms)
                .map_err(|e| e.to_string())
        })
    } else {
        if drv.diag().is_none() {
            bail!("driver '{driver}' does not expose a diagnostic transport for SSM over CAN");
        }
        Box::new(move |req: &[u8]| -> Result<Vec<u8>, String> {
            drv.diag()
                .ok_or("the transport is closed")?
                .request(req, timeout_ms)
                .map_err(|e| e.to_string())
        })
    };
    Ok(Some(diag))
}

/// For SSM: identify the module, keep the parameters it supports and switch
/// the session to batched `A8` reads. `--transport uds` passes through.
pub fn prepare(
    diag: &mut dyn RawDiag,
    def: LogDef,
    mut fast: FastLogConfig,
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
    let init = opentune_logdef::ssm::identify(diag)
        .map_err(|e| anyhow!("SSM init (BF) to the {target:?} failed: {e}"))?;
    eprintln!(
        "ssm: {target:?} SSM ID {} ROM ID {} ({} capability byte(s))",
        init.ssm_id_hex(),
        init.rom_id_hex(),
        init.capabilities.len()
    );
    let sel = opentune_logdef::ssm::select(&def, &init, target);
    for n in &sel.notes {
        eprintln!("  note: {n}");
    }
    for id in ids {
        if !sel.def.params().any(|(_, _, p)| p.id == *id) {
            eprintln!("  warning: '{id}' is not logged (no supported SSM parameter by that id)");
        }
    }
    if sel.is_empty() {
        bail!("no SSM parameter in the definition is supported by this module");
    }
    fast.ssm = Some(SsmPollConfig {
        max_packet_len: args.ssm_max_packet,
    });
    fast.validate()
        .map_err(|e| anyhow!("--ssm-max-packet: {e}"))?;
    Ok((sel.def, fast))
}
