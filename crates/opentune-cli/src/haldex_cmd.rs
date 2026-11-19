//! `opentune haldex`: BorgWarner Haldex AWD tools — offline CAN-log decode,
//! a read-only Gen4 controller read, and OpenHaldex status / mode control.
use anyhow::{anyhow, bail, Context, Result};
use opentune_core::protocol::vwtp20::VwTp20;
use opentune_core::{ConnectConfig, CoreDiag, Interface, MonitorOpts, Registry};
use opentune_haldex::canlog::{parse_log, summarize, HaldexSummary, LogFrame, ParsedLog};
use opentune_haldex::gen4::{
    gen4_connect_config, read_gen4, tp20_refusal, Gen4ReadOptions, Gen4Report, DEFAULT_GROUPS,
    TP20_LOGICAL_ADDRESS, TP20_TESTER_RX_ID,
};
use opentune_haldex::openhaldex::{self, Mode, OpenHaldexUds};
use opentune_haldex::sim::{SimGen4, SimGen4Link};
use opentune_haldex::{Bus, HaldexFrame};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(clap::Subcommand)]
pub enum HaldexAction {
    /// Decode Haldex-related frames in a CAN log (candump, OpenTune CSV,
    /// SavvyCAN CSV or Vector ASC): lock request/actual, status flags, temperature, wheel
    /// speeds, OpenHaldex frames. Offline.
    Decode {
        /// The log file.
        log: PathBuf,
        /// Frame set: auto (default), pq (Gen1/2/4) or mqb (Gen5).
        #[arg(long, default_value = "auto")]
        bus: String,
        /// Also print every decoded frame.
        #[arg(long)]
        frames: bool,
        /// JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Read-only Haldex Gen4 controller read (KWP2000 over VW TP2.0, logical
    /// address 0x0A): identification, stored codes, measuring groups.
    Gen4(Gen4Args),
    /// Read an OpenHaldex controller's state (read-only).
    Status(StatusArgs),
    /// OpenHaldex modes.
    Mode {
        #[command(subcommand)]
        action: ModeAction,
    },
}

#[derive(clap::Args)]
pub struct Gen4Args {
    /// Driver id: a raw-CAN adapter that opens VW TP2.0 (socketcan, slcan,
    /// gsusb). The ELM327 drivers are refused; see docs/HALDEX.md.
    #[arg(long, required_unless_present = "sim")]
    driver: Option<String>,
    /// Run against the built-in simulated Gen4 unit (synthetic data).
    #[arg(long, conflicts_with = "driver")]
    sim: bool,
    /// Serial device (slcan) or CAN interface (socketcan, gsusb).
    #[arg(long, value_name = "PORT")]
    port: Option<String>,
    /// Serial baud of the adapter link.
    #[arg(long, value_name = "BAUD")]
    baud: Option<String>,
    /// Vehicle bus (driver option `bus`).
    #[arg(long, value_name = "BUS")]
    bus: Option<String>,
    /// Extra driver option, e.g. `--opt transport=tp20 --opt tp20_keepalive_ms=500`
    /// (repeatable). `transport=tp20` and `tp20_addr=0x0A` are implied.
    #[arg(long = "opt", value_name = "KEY=VALUE")]
    opts: Vec<String>,
    /// Measuring groups, comma separated (default 1,3,125,126).
    #[arg(long)]
    groups: Option<String>,
    /// Skip reading stored trouble codes.
    #[arg(long)]
    no_dtcs: bool,
    /// Per-request timeout.
    #[arg(long, default_value_t = 2000)]
    timeout_ms: u64,
    /// JSON output.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
pub struct StatusArgs {
    /// Driver id (see `drivers`).
    #[arg(long)]
    driver: String,
    /// `can`: listen (listen-only where the adapter allows) for the OpenHaldex
    /// broadcast 0x6B0 and the Haldex status frames; `uds`: read the
    /// OpenHaldex DIDs 0xFDA0/0xFDA1 on 0x70F/0x779.
    #[arg(long, default_value = "can")]
    via: String,
    /// Seconds to listen (`--via can`).
    #[arg(long, default_value_t = 3.0)]
    duration: f64,
    /// Serial device or CAN interface.
    #[arg(long)]
    port: Option<String>,
    /// Serial baud.
    #[arg(long)]
    baud: Option<String>,
    /// Explicit CAN bitrate in bit/s (`--via can`; default 500000).
    #[arg(long)]
    bitrate: Option<u32>,
    /// Per-request timeout (`--via uds`).
    #[arg(long, default_value_t = 1000)]
    timeout_ms: u64,
    /// JSON output.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Subcommand)]
pub enum ModeAction {
    /// List the modes with their nominal lock and wire values. Offline.
    List,
    /// **Write**: set the OpenHaldex mode over UDS (WDBI 0xFDA2 on 0x70F),
    /// then read it back. Refuses without `--confirm`.
    Set {
        /// stock, fwd, 5050, 6040, 7525 or expert.
        mode: String,
        /// Driver id (see `drivers`).
        #[arg(long)]
        driver: String,
        /// Serial device or CAN interface.
        #[arg(long)]
        port: Option<String>,
        /// Serial baud.
        #[arg(long)]
        baud: Option<String>,
        /// Per-request timeout.
        #[arg(long, default_value_t = 1000)]
        timeout_ms: u64,
        /// Required: confirm that this changes how the car's AWD behaves.
        #[arg(long)]
        confirm: bool,
    },
}

/// Dispatch `opentune haldex`.
pub fn run(reg: &Registry, action: HaldexAction) -> Result<()> {
    match action {
        HaldexAction::Decode {
            log,
            bus,
            frames,
            json,
        } => decode(&log, &bus, frames, json),
        HaldexAction::Gen4(a) => gen4(reg, &a),
        HaldexAction::Status(a) => status(reg, a),
        HaldexAction::Mode { action } => match action {
            ModeAction::List => {
                mode_list();
                Ok(())
            }
            ModeAction::Set {
                mode,
                driver,
                port,
                baud,
                timeout_ms,
                confirm,
            } => mode_set(reg, &mode, &driver, port, baud, timeout_ms, confirm),
        },
    }
}

fn decode(path: &std::path::Path, bus: &str, frames: bool, json: bool) -> Result<()> {
    let bus: Bus = bus.parse().map_err(|e: String| anyhow!(e))?;
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let log = parse_log(&text);
    let (summary, decoded) = summarize(&log, bus);
    if json {
        let v = serde_json::json!({
            "bad_lines": log.bad_lines,
            "summary": summary,
            "frames": if frames { serde_json::to_value(&decoded)? } else { serde_json::Value::Null },
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    if frames {
        for (f, d) in &decoded {
            let ts = f.ts.map(|t| format!("{t:.6}")).unwrap_or_default();
            println!("{ts:>18} {:03X}  {}", f.id, describe(d));
        }
        println!();
    }
    print_summary(&summary);
    if log.bad_lines > 0 {
        println!("  ({} unparsed line(s) skipped)", log.bad_lines);
    }
    Ok(())
}

fn describe(d: &HaldexFrame) -> String {
    match d {
        HaldexFrame::LockRequestPq { raw, lock_pct, .. } => {
            format!("Bremse_4 lock request {lock_pct:5.1} % (raw 0x{raw:02X})")
        }
        HaldexFrame::StatusPq {
            flags,
            actual_raw,
            engagement_pct,
            gen4_temp_c,
            ..
        } => format!(
            "Allrad_1 actual {engagement_pct:5.1} % (raw 0x{actual_raw:02X}){}{}",
            gen4_temp_c
                .map(|t| format!(" temp {t} °C"))
                .unwrap_or_default(),
            if flags.names().is_empty() {
                String::new()
            } else {
                format!(" [{}]", flags.names().join(","))
            }
        ),
        HaldexFrame::StatusMqb {
            status,
            engagement_pct,
            request_accepted,
            ..
        } => format!(
            "Allrad_03 actual {} status {status}{}",
            engagement_pct
                .map(|p| format!("{p:5.1} %"))
                .unwrap_or_else(|| "  n/a".into()),
            if *request_accepted {
                " request-accepted"
            } else {
                ""
            }
        ),
        HaldexFrame::WheelSpeeds(w) => format!(
            "wheels FL {:.1} FR {:.1} RL {:.1} RR {:.1} km/h",
            w.fl, w.fr, w.rl, w.rr
        ),
        HaldexFrame::EnginePq { rpm, pedal_pct } => {
            format!("Motor_1 {rpm:.0} rpm pedal {pedal_pct:.1} %")
        }
        HaldexFrame::OpenHaldexBroadcast(b) => format!(
            "OpenHaldex mode {} actual {} % speed {} km/h pedal {} %",
            b.mode
                .map(|m| m.to_string())
                .unwrap_or_else(|| format!("?{}", b.mode_index)),
            b.actual_pct,
            b.speed_kmh,
            b.pedal_pct
        ),
        HaldexFrame::OpenHaldexCommand { value, mode } => format!(
            "OpenHaldex command {}",
            mode.map(|m| m.to_string())
                .unwrap_or_else(|| format!("unknown 0x{value:02X}"))
        ),
    }
}

fn print_summary(s: &HaldexSummary) {
    println!(
        "{} frame(s), {} Haldex-related decoded",
        s.frames, s.decoded
    );
    for (k, n) in &s.per_kind {
        println!("  {k:<22} {n}");
    }
    let range = |name: &str, r: &Option<opentune_haldex::canlog::Range>, unit: &str| {
        if let Some(r) = r {
            println!(
                "  {name:<22} min {:.1} max {:.1} last {:.1} {unit} ({} samples)",
                r.min, r.max, r.last, r.samples
            );
        }
    };
    range("lock request", &s.lock_request_pct, "%");
    range("lock actual", &s.lock_actual_pct, "%");
    range("Gen4 temperature", &s.gen4_temp_c, "°C");
    if let Some(v) = s.max_wheel_speed_kmh {
        println!("  {:<22} {v:.1} km/h", "max wheel speed");
    }
    if let Some(v) = s.max_front_rear_delta_kmh {
        println!("  {:<22} {v:+.1} km/h", "max front-rear delta");
    }
    if !s.flags_seen.is_empty() {
        let f: Vec<&str> = s.flags_seen.iter().map(String::as_str).collect();
        println!("  {:<22} {}", "status flags seen", f.join(", "));
    }
    if s.fault_stored_seen {
        println!("  Haldex reports a stored fault (Allrad_1 byte 3 bit 7)");
    }
    if !s.openhaldex_modes.is_empty() {
        let m: Vec<&str> = s.openhaldex_modes.iter().map(String::as_str).collect();
        println!("  {:<22} {}", "OpenHaldex modes", m.join(", "));
    }
    if !s.openhaldex_commands.is_empty() {
        println!(
            "  {:<22} {}",
            "OpenHaldex commands",
            s.openhaldex_commands.join(", ")
        );
    }
}

fn parse_groups(s: Option<&str>) -> Result<Vec<u8>> {
    let Some(s) = s else {
        return Ok(DEFAULT_GROUPS.to_vec());
    };
    s.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| {
            t.parse::<u8>()
                .map_err(|_| anyhow!("invalid measuring group '{t}' (1..=255)"))
        })
        .collect()
}

/// The driver connection for a real Gen4 read: the adapter flags plus the
/// implied TP2.0 options (`transport=tp20`, `tp20_addr=0x0A`).
fn gen4_config(a: &Gen4Args, driver: &str) -> Result<ConnectConfig> {
    let conn = crate::cli::ConnectArgs {
        driver: driver.to_string(),
        file: None,
        base: "0x0".into(),
        seedkey: None,
        port: a.port.clone(),
        baud: a.baud.clone(),
        bus: a.bus.clone(),
        tx_id: None,
        rx_id: None,
        opts: a.opts.clone(),
    };
    gen4_connect_config(conn.config()?).map_err(|e| anyhow!(e))
}

fn gen4(reg: &Registry, a: &Gen4Args) -> Result<()> {
    let (sim, json) = (a.sim, a.json);
    let opts = Gen4ReadOptions {
        groups: parse_groups(a.groups.as_deref())?,
        read_dtcs: !a.no_dtcs,
        timeout_ms: a.timeout_ms,
    };
    let report = if sim {
        // The full stack: a real VwTp20 channel to the simulated controller.
        let link = SimGen4Link::new(SimGen4::default());
        let mut tp = VwTp20::new(Box::new(link), TP20_LOGICAL_ADDRESS, TP20_TESTER_RX_ID);
        let r = read_gen4(&mut tp, &opts);
        tp.disconnect();
        r
    } else {
        let driver = a
            .driver
            .as_deref()
            .ok_or_else(|| anyhow!("--driver or --sim is required"))?;
        if let Some(why) = tp20_refusal(driver) {
            bail!(
                "{why} Use `--sim` to see the read, or `opentune candump` + \
                 `opentune haldex decode` for the passive decode."
            );
        }
        let cfg = gen4_config(a, driver)?;
        let mut drv = reg
            .make(driver)
            .ok_or_else(|| anyhow!("unknown driver '{driver}'"))?;
        if !drv.capabilities().vwtp20 {
            bail!("driver '{driver}' does not report VW TP2.0 support in this build");
        }
        drv.open(&cfg).map_err(|e| anyhow!("open {driver}: {e}"))?;
        let transport = drv
            .diag()
            .ok_or_else(|| anyhow!("driver '{driver}' exposes no diagnostic transport"))?;
        let r = read_gen4(transport, &opts);
        let _ = drv.close();
        r
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_gen4(&report, sim);
    }
    if !report.responded() {
        bail!("the Haldex controller did not answer the identification request");
    }
    Ok(())
}

fn print_gen4(r: &Gen4Report, sim: bool) {
    if sim {
        println!("(simulated Gen4 unit — synthetic data)");
    }
    if let Some(e) = &r.session_error {
        println!("session 0x89: {e}");
    }
    match (&r.identity, &r.identity_error) {
        (Some(id), _) => {
            println!("part number  {}", id.part_number);
            println!("software     {}", id.sw_version);
            println!("component    {}", id.component);
            println!("generation   {:?}", id.generation);
        }
        (None, Some(e)) => println!("identification failed: {e}"),
        (None, None) => {}
    }
    match (&r.dtcs, &r.dtc_error) {
        (Some(d), _) if d.is_empty() => println!("trouble codes: none stored"),
        (Some(d), _) => {
            println!("trouble codes: {}", d.len());
            for c in d {
                println!("  {:05}  status 0x{:02X}", c.vag_number, c.status);
            }
        }
        (None, Some(e)) => println!("trouble codes: read failed: {e}"),
        (None, None) => {}
    }
    for g in &r.groups {
        if let Some(e) = &g.error {
            println!("group {:3}: {e}", g.group);
            continue;
        }
        println!("group {:3}: {}", g.group, g.raw_hex);
        for (i, v) in g.values.iter().enumerate() {
            let val = v
                .value
                .map(|x| format!("{x:.3} {}", v.unit))
                .unwrap_or_else(|| format!("raw f=0x{:02X} a={} b={}", v.formula, v.a, v.b));
            println!("    [{}] {:<26} {val}", i + 1, v.label.unwrap_or("-"));
        }
    }
}

fn connect_cfg(port: Option<String>, baud: Option<String>, tx: u32, rx: u32) -> ConnectConfig {
    let mut cfg = ConnectConfig {
        tx_id: tx,
        rx_id: rx,
        ..ConnectConfig::default()
    };
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    if let Some(p) = port.or_else(|| env("OPENTUNE_PORT")) {
        cfg.options.insert("port".into(), p);
    }
    if let Some(b) = baud.or_else(|| env("OPENTUNE_BAUD")) {
        cfg.options.insert("baud".into(), b);
    }
    cfg
}

fn open_uds(
    reg: &Registry,
    driver: &str,
    port: Option<String>,
    baud: Option<String>,
) -> Result<Box<dyn Interface>> {
    let mut drv = reg
        .make(driver)
        .ok_or_else(|| anyhow!("unknown driver '{driver}' (see `opentune drivers`)"))?;
    let cfg = connect_cfg(port, baud, openhaldex::UDS_REQ_ID, openhaldex::UDS_RESP_ID);
    drv.open(&cfg).map_err(|e| anyhow!("open {driver}: {e}"))?;
    Ok(drv)
}

fn status(reg: &Registry, a: StatusArgs) -> Result<()> {
    match a.via.as_str() {
        "uds" => {
            let mut drv = open_uds(reg, &a.driver, a.port, a.baud)?;
            let transport = drv
                .diag()
                .ok_or_else(|| anyhow!("driver '{}' exposes no diagnostic transport", a.driver))?;
            let mut diag = CoreDiag::new(transport, a.timeout_ms);
            let mut oh = OpenHaldexUds::new(&mut diag);
            let lock = oh.read_lock().map_err(|e| anyhow!("read 0xFDA0: {e}"))?;
            let slip = oh.read_slip().ok();
            if a.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({"lock": lock, "slip": slip}))?
                );
            } else {
                println!(
                    "mode {}  commanded {} %  engagement raw 0x{:02X}",
                    lock.mode
                        .map(|m| m.to_string())
                        .unwrap_or_else(|| format!("?{}", lock.mode_index)),
                    lock.commanded_pct,
                    lock.engagement_raw
                );
                if let Some(s) = slip {
                    let f = |v: Option<i8>| v.map(|x| x.to_string()).unwrap_or_else(|| "-".into());
                    println!(
                        "slip FL {} FR {} RL {} RR {}",
                        f(s.fl),
                        f(s.fr),
                        f(s.rl),
                        f(s.rr)
                    );
                }
            }
            let _ = drv.close();
            Ok(())
        }
        "can" => status_can(reg, a),
        other => bail!("unknown --via '{other}' (can, uds)"),
    }
}

fn status_can(reg: &Registry, a: StatusArgs) -> Result<()> {
    if !(a.duration.is_finite() && a.duration > 0.0) {
        bail!("--duration must be a positive number of seconds");
    }
    let mut drv = reg
        .make(&a.driver)
        .ok_or_else(|| anyhow!("unknown driver '{}' (see `opentune drivers`)", a.driver))?;
    let mon = drv.can_monitor().ok_or_else(|| {
        anyhow!(
            "driver '{}' cannot capture raw CAN traffic (see `opentune candump --help`)",
            a.driver
        )
    })?;
    let mut opts = MonitorOpts {
        bitrate: a.bitrate.unwrap_or(0),
        filter_ids: vec![
            openhaldex::BROADCAST_ID,
            opentune_haldex::can::ID_ALLRAD_1,
            opentune_haldex::can::ID_BREMSE_4,
            opentune_haldex::can::ID_ALLRAD_03,
        ],
        ..MonitorOpts::default()
    };
    for (k, v) in [("port", a.port), ("baud", a.baud)] {
        if let Some(v) = v.filter(|v| !v.trim().is_empty()) {
            opts.options.insert(k.into(), v);
        }
    }
    mon.start(&opts)
        .map_err(|e| anyhow!("start listening on {}: {e}", a.driver))?;
    eprintln!(
        "listening {:.1} s (listen-only: {})",
        a.duration,
        mon.listen_only().as_str()
    );
    let deadline = Instant::now() + Duration::from_secs_f64(a.duration);
    let mut log = ParsedLog::default();
    let mut err = None;
    while Instant::now() < deadline {
        match mon.next_frame(100) {
            Ok(Some(f)) if opts.accepts(f.id()) => log.frames.push(LogFrame {
                ts: Some(f.ts_us as f64 / 1e6),
                channel: None,
                id: f.id(),
                extended: f.extended,
                data: f.data().to_vec(),
            }),
            Ok(_) => {}
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    let _ = mon.stop();
    if let Some(e) = err {
        bail!("capture failed: {e}");
    }
    let (summary, _) = summarize(&log, Bus::Auto);
    if a.json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        print_summary(&summary);
        if summary.decoded == 0 {
            println!(
                "  no OpenHaldex broadcast or Haldex status frame seen: is the adapter on the \
                 drivetrain CAN (not the gateway-isolated OBD port), and is the OpenHaldex \
                 'Broadcast over CAN' setting on?"
            );
        }
    }
    Ok(())
}

fn mode_list() {
    println!("mode     lock     index (BLE/UDS)  0x6A0 byte");
    for m in Mode::ALL {
        let lock = m
            .nominal_lock_pct()
            .map(|p| format!("{p} %"))
            .unwrap_or_else(|| match m {
                Mode::Stock => "OEM".into(),
                _ => "map".into(),
            });
        println!(
            "{:<8} {:<8} {:<16} {}",
            m.as_str(),
            lock,
            m.index(),
            m.can_command()
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn mode_set(
    reg: &Registry,
    mode: &str,
    driver: &str,
    port: Option<String>,
    baud: Option<String>,
    timeout_ms: u64,
    confirm: bool,
) -> Result<()> {
    let mode: Mode = mode.parse().map_err(|e: String| anyhow!(e))?;
    let [h, l] = openhaldex::DID_MODE.to_be_bytes();
    if !confirm {
        bail!(
            "refusing to change the AWD mode without --confirm. This would send \
             2E {h:02X} {l:02X} {:02X} to 0x{:03X} (OpenHaldex mode {mode}).",
            mode.index(),
            openhaldex::UDS_REQ_ID
        );
    }
    let mut drv = open_uds(reg, driver, port, baud)?;
    let transport = drv
        .diag()
        .ok_or_else(|| anyhow!("driver '{driver}' exposes no diagnostic transport"))?;
    let mut diag = CoreDiag::new(transport, timeout_ms);
    let st = OpenHaldexUds::new(&mut diag)
        .set_mode(mode)
        .map_err(|e| anyhow!("set mode {mode}: {e}"));
    let _ = drv.close();
    let st = st?;
    println!(
        "OpenHaldex mode now {mode} (commanded {} %)",
        st.commanded_pct
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_parse() {
        assert_eq!(parse_groups(None).unwrap(), DEFAULT_GROUPS.to_vec());
        assert_eq!(parse_groups(Some("1, 3,125")).unwrap(), vec![1, 3, 125]);
        assert!(parse_groups(Some("1,300")).is_err());
    }

    fn args(driver: Option<&str>, sim: bool, extra: &[&str]) -> Gen4Args {
        Gen4Args {
            driver: driver.map(str::to_string),
            sim,
            port: None,
            baud: None,
            bus: None,
            opts: extra.iter().map(|s| s.to_string()).collect(),
            groups: Some("1,3".into()),
            no_dtcs: false,
            timeout_ms: 100,
            json: true,
        }
    }

    #[test]
    fn real_gen4_read_is_refused_without_a_tp20_driver() {
        let reg = Registry::new();
        let err = gen4(&reg, &args(Some("elm327obd"), false, &[])).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("TP2.0") && msg.contains("slcan"), "{msg}");
    }

    #[test]
    fn raw_can_drivers_are_not_refused() {
        // An empty registry: slcan passes the TP2.0 check and only fails at
        // "unknown driver", i.e. it is no longer refused up front.
        let reg = Registry::new();
        let err = gen4(&reg, &args(Some("slcan"), false, &[])).unwrap_err();
        assert!(err.to_string().contains("unknown driver"), "{err}");
    }

    #[test]
    fn gen4_config_implies_tp20_and_keeps_user_options() {
        let mut a = args(Some("slcan"), false, &["tp20_keepalive_ms=500"]);
        a.port = Some("/dev/ttyACM0".into());
        let c = gen4_config(&a, "slcan").unwrap();
        assert_eq!(c.protocol, opentune_core::Protocol::Vwtp20);
        assert_eq!(c.options["transport"], "tp20");
        assert_eq!(c.options["tp20_addr"], "0x0A");
        assert_eq!(c.options["tp20_keepalive_ms"], "500");
        assert_eq!(c.options["port"], "/dev/ttyACM0");
        assert_eq!(
            (c.tx_id, c.rx_id),
            (TP20_LOGICAL_ADDRESS as u32, TP20_TESTER_RX_ID)
        );
        let a = args(Some("slcan"), false, &["transport=isotp"]);
        assert!(gen4_config(&a, "slcan").is_err());
    }

    #[test]
    fn simulated_gen4_read_succeeds() {
        let reg = Registry::new();
        gen4(&reg, &args(None, true, &[])).unwrap();
    }

    #[test]
    fn mode_set_refuses_without_confirm() {
        let reg = Registry::new();
        let err = mode_set(&reg, "5050", "mock", None, None, 100, false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--confirm") && msg.contains("2E FD A2 02"),
            "{msg}"
        );
        assert!(mode_set(&reg, "awd", "mock", None, None, 100, true).is_err());
    }
}
