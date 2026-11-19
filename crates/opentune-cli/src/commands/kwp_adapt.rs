//! `opentune adaptation kwp-*`: the KWP2000 path of the adaptation engine
//! (VAG measuring blocks, basic settings, adaptation read / test / save) over
//! a VW TP2.0 channel (raw-CAN drivers), a K-line driver, or the built-in
//! synthetic module (`--sim`).
//!
//! Writes (basic settings, test, save) need `--ack` and a `--seedkey` handler
//! for SecurityAccess; OpenTune ships no VAG KWP login algorithm, so in the
//! public build only `--sim` (handler `kwpsim-test`) can run them. `--dry-run`
//! prints the request plan and sends nothing.
use crate::cli::{AdaptAction, ConnectArgs, KwpConn, KwpGate};
use crate::context::CliContext;
use anyhow::{anyhow, bail, Result};
use opentune_adaptation::kwp::{
    self, AdaptMode, BasicSettingsOutcome, KwpLogin, KwpWriteOptions, KwpWriteOutcome,
};
use opentune_adaptation::sim::{open_sim_channel, sim_bus, SimKwpModule};
use opentune_adaptation::{AdaptationDef, ChannelKind, RawDiag};
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::protocol::tp20channel::DEFAULT_TESTER_RX_ID;
use opentune_core::protocol::tp20sim::Tp20EcuConfig;
use opentune_core::{ConnectConfig, CoreDiag, DiagLayer, Protocol};
use std::sync::{Arc, Mutex};

/// Raw-CAN drivers that open a VW TP2.0 channel.
pub const TP20_DRIVERS: &[&str] = &["socketcan", "slcan", "gsusb"];
/// K-line drivers that speak KWP2000.
pub const KLINE_DRIVERS: &[&str] = &["kkl", "kline-bridge", "elm327"];

fn parse_num(flag: &str, s: &str, max: u64) -> Result<u64> {
    let t = s.trim();
    let v = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => t.parse().ok(),
    };
    match v {
        Some(v) if v <= max => Ok(v),
        _ => bail!("{flag} '{s}': expected a number 0..={max} (decimal or 0x hex)"),
    }
}

fn parse_session(s: &str) -> Result<Option<u8>> {
    if s.trim().eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    Ok(Some(parse_num("--session", s, 0xFF)? as u8))
}

/// `--wsc`: exactly 6 bytes as 12 hex digits (spaces allowed).
fn parse_wsc(s: &str) -> Result<[u8; kwp::WORKSHOP_CODE_LEN]> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if clean.len() != 2 * kwp::WORKSHOP_CODE_LEN || !clean.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("--wsc '{s}': expected the 6-byte workshop code as 12 hex digits");
    }
    let mut out = [0u8; kwp::WORKSHOP_CODE_LEN];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&clean[2 * i..2 * i + 2], 16)?;
    }
    Ok(out)
}

/// The driver connection for a real (not simulated) KWP flow.
fn connect_config(c: &KwpConn, driver: &str) -> Result<ConnectConfig> {
    let conn = ConnectArgs {
        driver: driver.to_string(),
        file: None,
        base: "0x0".into(),
        seedkey: None,
        port: c.port.clone(),
        baud: c.baud.clone(),
        bus: c.bus.clone(),
        tx_id: None,
        rx_id: None,
        opts: c.opts.clone(),
    };
    let mut cfg = conn.config()?;
    cfg.diag_layer = DiagLayer::Kwp2000;
    if TP20_DRIVERS.contains(&driver) {
        let addr = c.tp20_addr.as_deref().ok_or_else(|| {
            anyhow!("--tp20-addr is required with '{driver}' (the module's TP2.0 logical address)")
        })?;
        let addr = parse_num("--tp20-addr", addr, 0xFF)? as u8;
        cfg.protocol = Protocol::Vwtp20;
        cfg.tx_id = addr as u32;
        cfg.rx_id = DEFAULT_TESTER_RX_ID;
        cfg.options
            .entry("transport".into())
            .or_insert_with(|| "tp20".into());
        cfg.options
            .insert("tp20_addr".into(), format!("0x{addr:02X}"));
    } else if KLINE_DRIVERS.contains(&driver) {
        let addr = c.kline_addr.as_deref().ok_or_else(|| {
            anyhow!("--kline-addr is required with '{driver}' (the module's K-line address)")
        })?;
        cfg.protocol = Protocol::Iso14230Kline;
        cfg.tx_id = parse_num("--kline-addr", addr, 0xFF)? as u32;
    } else {
        bail!(
            "driver '{driver}' cannot carry VAG KWP2000: use a raw-CAN adapter for TP2.0 \
             ({}), a K-line adapter ({}), or --sim",
            TP20_DRIVERS.join(", "),
            KLINE_DRIVERS.join(", ")
        );
    }
    Ok(cfg)
}

/// Run `f` over the transport: the synthetic module behind the core TP2.0
/// simulator, or the driver.
fn with_diag<T>(
    ctx: &CliContext,
    c: &KwpConn,
    driver: &str,
    timeout_ms: u64,
    f: impl FnOnce(&mut dyn RawDiag) -> T,
) -> Result<T> {
    if c.sim {
        let unit = Arc::new(Mutex::new(SimKwpModule::default()));
        let ecu = sim_bus(unit.clone(), Tp20EcuConfig::default());
        let mut ch =
            open_sim_channel(ecu, unit, 0).map_err(|e| anyhow!("open simulated channel: {e}"))?;
        let r = f(&mut CoreDiag::new(&mut ch, timeout_ms));
        ch.close();
        return Ok(r);
    }
    let cfg = connect_config(c, driver)?;
    let mut drv = ctx
        .registry()
        .make(driver)
        .ok_or_else(|| anyhow!("unknown driver '{driver}'"))?;
    drv.open(&cfg).map_err(|e| anyhow!("open {driver}: {e}"))?;
    let r = match drv.diag() {
        Some(t) => Ok(f(&mut CoreDiag::new(t, timeout_ms))),
        None => Err(anyhow!("driver '{driver}' exposes no diagnostic transport")),
    };
    let _ = drv.close();
    r
}

fn print_json(v: serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

fn cells_text(cells: &[kwp::KwpCell]) -> String {
    cells
        .iter()
        .map(|c| match c.value {
            Some(v) => format!("[{}] {v:.3} {}", c.index, c.unit),
            None => format!(
                "[{}] raw f=0x{:02X} a={} b={}",
                c.index, c.formula, c.a, c.b
            ),
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// Dispatch one `adaptation kwp-*` action.
pub fn run(
    ctx: &CliContext,
    def: &AdaptationDef,
    driver: &str,
    timeout_ms: u64,
    action: AdaptAction,
) -> Result<()> {
    match action {
        AdaptAction::KwpRead { channel, kwp } => read(ctx, def, driver, timeout_ms, &channel, &kwp),
        AdaptAction::KwpBasic {
            channel,
            polls,
            interval_ms,
            gate,
            kwp,
        } => basic(
            ctx,
            def,
            driver,
            timeout_ms,
            &channel,
            polls,
            interval_ms,
            &gate,
            &kwp,
        ),
        AdaptAction::KwpTest {
            channel,
            value,
            gate,
            kwp,
        } => adapt(
            ctx,
            def,
            driver,
            timeout_ms,
            &channel,
            &value,
            None,
            AdaptMode::Test,
            &gate,
            &kwp,
        ),
        AdaptAction::KwpSave {
            channel,
            value,
            wsc,
            gate,
            kwp,
        } => adapt(
            ctx,
            def,
            driver,
            timeout_ms,
            &channel,
            &value,
            wsc.as_deref(),
            AdaptMode::Save,
            &gate,
            &kwp,
        ),
        AdaptAction::List | AdaptAction::Read { .. } => {
            bail!("internal: not a KWP adaptation action")
        }
    }
}

/// The seed/key handler `--seedkey` names, resolved before any I/O.
fn handler(ctx: &CliContext, c: &KwpConn) -> Result<Option<Box<dyn SeedKey>>> {
    c.seedkey
        .as_deref()
        .map(|id| ctx.require_seed_key(id))
        .transpose()
}

fn login<'a>(c: &KwpConn, sk: Option<&'a dyn SeedKey>) -> Result<KwpLogin<'a>> {
    Ok(KwpLogin {
        session: parse_session(&c.session)?,
        security_level: parse_num("--level", &c.level, 0xFE)? as u8,
        seed_key: sk,
    })
}

fn read(
    ctx: &CliContext,
    def: &AdaptationDef,
    driver: &str,
    timeout_ms: u64,
    channel: &str,
    c: &KwpConn,
) -> Result<()> {
    let kind = def
        .channel(channel)
        .map(|ch| ch.kind.clone())
        .ok_or_else(|| anyhow!("unknown channel `{channel}`"))?;
    let sk = handler(ctx, c)?;
    let login = login(c, sk.as_deref())?;
    match kind {
        ChannelKind::KwpGroup { .. } => {
            let r = with_diag(ctx, c, driver, timeout_ms, |d| {
                kwp::read_group(d, def, channel, &login)
            })?
            .map_err(|e| anyhow!("{e}"))?;
            if c.json {
                return print_json(serde_json::to_value(&r)?);
            }
            if c.sim {
                println!("(synthetic KWP module)");
            }
            println!("{}: group {} {}", r.channel, r.group, cells_text(&r.cells));
            if r.reestablished {
                println!("  (session was lost and re-established once)");
            }
        }
        ChannelKind::KwpAdaptation { .. } => {
            let r = with_diag(ctx, c, driver, timeout_ms, |d| {
                kwp::read_adaptation(d, def, channel, &login)
            })?
            .map_err(|e| anyhow!("{e}"))?;
            if c.json {
                return print_json(serde_json::to_value(&r)?);
            }
            if c.sim {
                println!("(synthetic KWP module)");
            }
            println!(
                "{}: channel {} raw={} (0x{:04X}) engineering={} {}",
                r.channel, r.adaptation_channel, r.raw, r.raw, r.engineering, r.unit
            );
            if let Some(m) = &r.measured {
                println!("  measured {}", cells_text(std::slice::from_ref(m)));
            }
        }
        _ => bail!("channel `{channel}` is not a kwp_group or kwp_adaptation channel"),
    }
    Ok(())
}

fn write_opts(gate: &KwpGate, wsc: Option<[u8; kwp::WORKSHOP_CODE_LEN]>) -> KwpWriteOptions {
    KwpWriteOptions {
        dry_run: gate.dry_run,
        confirmed: gate.ack,
        workshop_code: wsc,
    }
}

#[allow(clippy::too_many_arguments)]
fn basic(
    ctx: &CliContext,
    def: &AdaptationDef,
    driver: &str,
    timeout_ms: u64,
    channel: &str,
    polls: u32,
    interval_ms: u64,
    gate: &KwpGate,
    c: &KwpConn,
) -> Result<()> {
    let sk = handler(ctx, c)?;
    let login = login(c, sk.as_deref())?;
    let opts = write_opts(gate, None);
    let run = |d: &mut dyn RawDiag| {
        kwp::run_basic_settings(d, def, channel, polls, interval_ms, &login, &opts)
    };
    let r = if gate.dry_run {
        // Nothing is sent: no adapter is opened.
        run(&mut |_: &[u8]| -> std::result::Result<Vec<u8>, String> {
            Err("dry run: nothing is sent".into())
        })
    } else {
        with_diag(ctx, c, driver, timeout_ms, run)?
    }
    .map_err(|e| anyhow!("{e}"))?;
    if c.json {
        return print_json(serde_json::to_value(&r)?);
    }
    for l in &r.plan {
        println!("  {l}");
    }
    for s in &r.samples {
        println!(
            "poll {}: status 0x{:02X} {}",
            s.index,
            s.status,
            cells_text(&s.cells)
        );
    }
    match &r.outcome {
        BasicSettingsOutcome::Planned => println!("dry run: nothing was sent"),
        BasicSettingsOutcome::Completed => println!("basic settings: started, read, stopped"),
        BasicSettingsOutcome::StopFailed { message } => {
            bail!("basic settings ran but the stop was refused ({message}): the module may stay in basic settings until its session times out")
        }
        BasicSettingsOutcome::Aborted {
            step,
            message,
            session_lost,
        } => bail!(
            "basic settings aborted at {step}: {message}{}",
            if *session_lost {
                " (session lost; nothing more was sent)"
            } else {
                ""
            }
        ),
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn adapt(
    ctx: &CliContext,
    def: &AdaptationDef,
    driver: &str,
    timeout_ms: u64,
    channel: &str,
    value: &str,
    wsc: Option<&str>,
    mode: AdaptMode,
    gate: &KwpGate,
    c: &KwpConn,
) -> Result<()> {
    let raw = parse_num("--value", value, 0xFFFF)? as u16;
    let wsc = wsc.map(parse_wsc).transpose()?;
    let sk = handler(ctx, c)?;
    let login = login(c, sk.as_deref())?;
    let opts = write_opts(gate, wsc);
    let run =
        |d: &mut dyn RawDiag| kwp::write_adaptation(d, def, channel, raw, mode, &login, &opts);
    let r = if gate.dry_run {
        run(&mut |_: &[u8]| -> std::result::Result<Vec<u8>, String> {
            Err("dry run: nothing is sent".into())
        })
    } else {
        with_diag(ctx, c, driver, timeout_ms, run)?
    }
    .map_err(|e| anyhow!("{e}"))?;
    if c.json {
        return print_json(serde_json::to_value(&r)?);
    }
    for l in &r.plan {
        println!("  {l}");
    }
    if let Some(b) = &r.before {
        println!(
            "before: raw={} engineering={} {}",
            b.raw, b.engineering, b.unit
        );
    }
    if let Some(a) = &r.after {
        println!(
            "after:  raw={} engineering={} {}",
            a.raw, a.engineering, a.unit
        );
    }
    match &r.outcome {
        KwpWriteOutcome::Planned => println!("dry run: nothing was sent"),
        KwpWriteOutcome::Tested => println!("test value applied and read back (not stored)"),
        KwpWriteOutcome::Verified => println!("saved and verified"),
        KwpWriteOutcome::VerifyFailed { read_back } => {
            bail!("read-back {read_back} differs from {raw}: the module state is uncertain")
        }
        KwpWriteOutcome::Aborted {
            step,
            message,
            session_lost,
        } => bail!(
            "aborted at {step}: {message}{} - read the channel again before anything else",
            if *session_lost {
                " (session lost; nothing more was sent)"
            } else {
                ""
            }
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CliConfig;

    fn def() -> AdaptationDef {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../opentune-adaptation/tests/fixtures/kwp-sim.json");
        AdaptationDef::from_json(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    fn ctx() -> CliContext {
        CliContext::from_config(&CliConfig::default())
    }

    fn sim(seedkey: Option<&str>) -> KwpConn {
        KwpConn {
            sim: true,
            session: "0x89".into(),
            level: "0x03".into(),
            seedkey: seedkey.map(str::to_string),
            json: true,
            ..KwpConn::default()
        }
    }

    fn ack() -> KwpGate {
        KwpGate {
            ack: true,
            dry_run: false,
        }
    }

    #[test]
    fn connection_is_tp20_or_kline_or_refused() {
        let mut c = KwpConn {
            tp20_addr: Some("0x01".into()),
            ..KwpConn::default()
        };
        let cfg = connect_config(&c, "slcan").unwrap();
        assert_eq!(cfg.options["transport"], "tp20");
        assert_eq!(cfg.options["tp20_addr"], "0x01");
        assert_eq!(cfg.diag_layer, DiagLayer::Kwp2000);
        c.tp20_addr = None;
        assert!(connect_config(&c, "slcan").is_err(), "address required");
        c.kline_addr = Some("0x01".into());
        let cfg = connect_config(&c, "kkl").unwrap();
        assert_eq!((cfg.protocol, cfg.tx_id), (Protocol::Iso14230Kline, 1));
        let e = connect_config(&c, "elm327obd").unwrap_err().to_string();
        assert!(e.contains("cannot carry VAG KWP2000"), "{e}");
        assert!(connect_config(&c, "mock").is_err());
    }

    #[test]
    fn wsc_and_numbers_parse() {
        assert_eq!(
            parse_wsc("00 00 01 23 45 67").unwrap(),
            [0, 0, 1, 0x23, 0x45, 0x67]
        );
        assert!(parse_wsc("0123").is_err());
        assert_eq!(parse_session("none").unwrap(), None);
        assert_eq!(parse_num("--value", "0x0300", 0xFFFF).unwrap(), 0x300);
        assert!(parse_num("--value", "70000", 0xFFFF).is_err());
    }

    #[test]
    fn simulated_reads_and_writes() {
        let (ctx, def) = (ctx(), def());
        read(&ctx, &def, "mock", 500, "Group 1", &sim(None)).unwrap();
        read(&ctx, &def, "mock", 500, "Adaptation 1", &sim(None)).unwrap();
        let c = sim(Some("kwpsim-test"));
        adapt(
            &ctx,
            &def,
            "mock",
            500,
            "Adaptation 1",
            "0x0300",
            Some("000001234567"),
            AdaptMode::Save,
            &ack(),
            &c,
        )
        .unwrap();
        adapt(
            &ctx,
            &def,
            "mock",
            500,
            "Adaptation 1",
            "600",
            None,
            AdaptMode::Test,
            &ack(),
            &c,
        )
        .unwrap();
        basic(
            &ctx,
            &def,
            "mock",
            500,
            "Basic settings 60",
            2,
            0,
            &ack(),
            &c,
        )
        .unwrap();
    }

    #[test]
    fn writes_are_gated() {
        let (ctx, def) = (ctx(), def());
        // No --ack.
        let e = adapt(
            &ctx,
            &def,
            "mock",
            500,
            "Adaptation 1",
            "1",
            Some("000000000000"),
            AdaptMode::Save,
            &KwpGate::default(),
            &sim(Some("kwpsim-test")),
        )
        .unwrap_err();
        assert!(e.to_string().contains("confirmed"), "{e}");
        // No --seedkey.
        let e = basic(
            &ctx,
            &def,
            "mock",
            500,
            "Basic settings 60",
            1,
            0,
            &ack(),
            &sim(None),
        )
        .unwrap_err();
        assert!(e.to_string().contains("seed/key"), "{e}");
        // An unknown handler id fails before any I/O.
        let e = adapt(
            &ctx,
            &def,
            "mock",
            500,
            "Adaptation 1",
            "1",
            None,
            AdaptMode::Test,
            &ack(),
            &sim(Some("vag-login")),
        )
        .unwrap_err();
        assert!(e.to_string().contains("unknown seed-key id"), "{e}");
        // A dry run needs neither and opens nothing (a real driver here).
        adapt(
            &ctx,
            &def,
            "slcan",
            500,
            "Adaptation 1",
            "1",
            Some("000000000000"),
            AdaptMode::Save,
            &KwpGate {
                ack: false,
                dry_run: true,
            },
            &KwpConn {
                sim: false,
                ..sim(None)
            },
        )
        .unwrap();
    }
}
