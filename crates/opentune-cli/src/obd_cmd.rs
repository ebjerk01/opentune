//! `opentune obd pids|supported|read`: generic OBD-II mode-01 live data over
//! [`opentune_core::obd`]. Driver `obdsim` uses the built-in simulated ECU.
use anyhow::{anyhow, Result};
use clap::{Args, Subcommand};
use opentune_core::obd::{self, sim::SimEcu, ObdSession};
use opentune_core::{ConnectConfig, RawDiag, Registry};

#[derive(Subcommand)]
pub enum ObdAction {
    /// Print the mode-01 channel table (offline).
    Pids {
        /// Emit JSON instead of a text table.
        #[arg(long)]
        json: bool,
    },
    /// Discover the ECU's supported PIDs and read the VIN.
    Supported(ObdConn),
    /// Poll PIDs and print decoded values.
    Read {
        #[command(flatten)]
        conn: ObdConn,
        /// Number of polls.
        #[arg(long, default_value_t = 1)]
        rounds: usize,
        /// Sleep between polls, milliseconds.
        #[arg(long, default_value_t = 0)]
        interval_ms: u64,
        /// Emit one JSON readout per poll.
        #[arg(long)]
        json: bool,
        /// Hex PIDs (e.g. `0C 05 78`). Default: every supported table PID.
        pids: Vec<String>,
    },
}

#[derive(Args)]
pub struct ObdConn {
    /// Driver id (see `drivers`), or `obdsim` for the simulated ECU.
    #[arg(long, default_value = "elm327obd")]
    driver: String,
    /// Serial port (also `OPENTUNE_PORT`).
    #[arg(long)]
    port: Option<String>,
    /// Serial baud (also `OPENTUNE_BAUD`).
    #[arg(long)]
    baud: Option<String>,
    /// CAN bus for switched ELM327s: `hs` or `ms`.
    #[arg(long)]
    bus: Option<String>,
    /// Per-request timeout, milliseconds.
    #[arg(long, default_value_t = 1000)]
    timeout_ms: u64,
}

fn with_diag<T>(
    reg: &Registry,
    conn: &ObdConn,
    f: impl FnOnce(&mut dyn RawDiag) -> Result<T>,
) -> Result<T> {
    if conn.driver == "obdsim" {
        return f(&mut SimEcu::demo(0));
    }
    let mut drv = reg
        .make(&conn.driver)
        .ok_or_else(|| anyhow!("unknown driver '{}'", conn.driver))?;
    let mut cfg = ConnectConfig::default();
    let env = |k: &str| std::env::var(k).ok();
    for (opt, val) in [
        ("port", conn.port.clone().or_else(|| env("OPENTUNE_PORT"))),
        ("baud", conn.baud.clone().or_else(|| env("OPENTUNE_BAUD"))),
        ("bus", conn.bus.clone()),
    ] {
        if let Some(v) = val.filter(|v| !v.trim().is_empty()) {
            cfg.options.insert(opt.into(), v);
        }
    }
    drv.open(&cfg)
        .map_err(|e| anyhow!("open {}: {e}", conn.driver))?;
    let t = drv
        .diag()
        .ok_or_else(|| anyhow!("driver '{}' has no diagnostic transport", conn.driver))?;
    f(&mut opentune_core::CoreDiag::new(t, conn.timeout_ms))
}

fn parse_pid(s: &str) -> Result<u8> {
    let t = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    u8::from_str_radix(t, 16).map_err(|_| anyhow!("bad PID '{s}' (expected hex)"))
}

pub fn run(reg: &Registry, action: ObdAction) -> Result<()> {
    match action {
        ObdAction::Pids { json } => {
            if json {
                let rows: Vec<_> = obd::channels()
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "pid": c.pid, "key": c.key, "name": c.name, "unit": c.unit,
                            "min": c.min, "max": c.max, "warn": c.warn, "crit": c.crit,
                            "group": c.group.as_str(), "derived": c.is_derived(),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                for c in obd::channels() {
                    let tag = if c.is_derived() { "*" } else { " " };
                    println!(
                        "{:02X}{tag} {:<24} {:<10} {:<9} {}",
                        c.pid,
                        c.key,
                        c.group.as_str(),
                        c.unit,
                        c.name
                    );
                }
                println!("(* = derived)");
            }
        }
        ObdAction::Supported(conn) => with_diag(reg, &conn, |d| {
            let set = ObdSession::new().discover(d).map_err(|e| anyhow!("{e}"))?;
            println!(
                "VIN       : {}",
                obd::read_vin(d).unwrap_or_else(|| "?".into())
            );
            let list: Vec<String> = set.iter().map(|p| format!("{p:02X}")).collect();
            println!("Supported : {}", list.join(" "));
            let known: Vec<&str> = set
                .iter()
                .flat_map(|&p| obd::channels_for_pid(p).map(|c| c.key))
                .collect();
            println!("Channels  : {}", known.join(", "));
            Ok(())
        })?,
        ObdAction::Read {
            conn,
            rounds,
            interval_ms,
            json,
            pids,
        } => {
            let mut pids: Vec<u8> = pids.iter().map(|s| parse_pid(s)).collect::<Result<_>>()?;
            with_diag(reg, &conn, |d| {
                let mut session = ObdSession::new();
                if pids.is_empty() {
                    let set = session.discover(d).map_err(|e| anyhow!("{e}"))?;
                    pids = set
                        .into_iter()
                        .filter(|&p| obd::channels_for_pid(p).next().is_some())
                        .collect();
                }
                for round in 0..rounds.max(1) {
                    if round > 0 && interval_ms > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(interval_ms));
                    }
                    let ro = session.read(d, &pids);
                    if json {
                        println!("{}", serde_json::to_string(&ro)?);
                        continue;
                    }
                    for (key, v) in &ro.values {
                        let unit = obd::channel(key).map(|c| c.unit).unwrap_or("");
                        match v {
                            Some(v) => println!("{key:<24} {} {unit}", serde_json::to_string(v)?),
                            None => println!(
                                "{key:<24} -  ({})",
                                ro.errors.get(key).map(String::as_str).unwrap_or("no value")
                            ),
                        }
                    }
                    for (key, e) in ro
                        .errors
                        .iter()
                        .filter(|(k, _)| !ro.values.contains_key(*k))
                    {
                        println!("{key:<24} -  ({e})");
                    }
                    if rounds > 1 {
                        println!();
                    }
                }
                Ok(())
            })?
        }
    }
    Ok(())
}
