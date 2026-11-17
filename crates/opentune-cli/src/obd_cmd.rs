//! `opentune obd pids|supported|read`: generic OBD-II mode-01 live data over
//! [`opentune_core::obd`]; `opentune obd readiness|freeze|info|dtcs`: the
//! inspection data of [`opentune_core::obd::inspect`]. Driver `obdsim` uses the
//! built-in simulated vehicle (engine on 7E0, transmission on 7E1).
use anyhow::{anyhow, Result};
use clap::{Args, Subcommand};
use opentune_core::obd::inspect::{
    inspect, obd_ecu_label, DtcEntry, Inspection, MonitorState, Sections, OBD_ECU_IDS,
};
use opentune_core::obd::{
    self,
    sim::{SimEcu, SimVehicle},
    ObdSession, ObdValue,
};
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
    /// Readiness monitors (mode 01 PID 01 + 41): MIL, stored-DTC count and
    /// each monitor's complete / incomplete / not-supported state.
    Readiness(InspectArgs),
    /// Freeze frame (mode 02): the DTC that stored it and the values then.
    Freeze {
        #[command(flatten)]
        args: InspectArgs,
        /// Freeze frame number (SAE J1979 defines frame 0).
        #[arg(long, default_value_t = 0)]
        frame: u8,
    },
    /// Vehicle information (mode 09): VIN, calibration IDs, CVNs, ECU name.
    Info(InspectArgs),
    /// Stored (mode 03), pending (07) and permanent (0A) DTCs.
    Dtcs(InspectArgs),
}

#[derive(Args)]
pub struct InspectArgs {
    #[command(flatten)]
    conn: ObdConn,
    /// Ask every OBD ECU address (7E0..7E7) instead of the engine (7E0) only;
    /// silent addresses are skipped. Re-opens the adapter per address.
    #[arg(long)]
    all_ecus: bool,
    /// Emit JSON: an object with one `ecus` entry per module.
    #[arg(long)]
    json: bool,
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
    with_diag_at(reg, conn, OBD_ECU_IDS[0], f)
}

/// Open `conn`'s driver bound to the module `(tx, rx)` and run `f` on it.
fn with_diag_at<T>(
    reg: &Registry,
    conn: &ObdConn,
    (tx, rx): (u32, u32),
    f: impl FnOnce(&mut dyn RawDiag) -> Result<T>,
) -> Result<T> {
    let mut drv = reg
        .make(&conn.driver)
        .ok_or_else(|| anyhow!("unknown driver '{}'", conn.driver))?;
    let mut cfg = ConnectConfig {
        tx_id: tx,
        rx_id: rx,
        ..ConnectConfig::default()
    };
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

/// Inspect the engine, or every OBD address with `--all-ecus` (keeping only
/// those that answered). Returns `(tx, rx, inspection)` per module.
fn inspect_ecus(
    reg: &Registry,
    args: &InspectArgs,
    sections: Sections,
    frame: u8,
) -> Result<Vec<(u32, u32, Inspection)>> {
    let addrs: Vec<(u32, u32)> = if args.all_ecus {
        OBD_ECU_IDS.to_vec()
    } else {
        vec![OBD_ECU_IDS[0]]
    };
    let mut out = Vec::new();
    let mut sim = (args.conn.driver == "obdsim").then(|| SimVehicle::demo(0));
    for (tx, rx) in addrs {
        let insp = match sim.as_mut() {
            Some(v) => match v.ecus.get_mut(&tx) {
                Some(ecu) => inspect(ecu, sections, frame),
                None => {
                    let mut silent = |_: &[u8]| -> std::result::Result<Vec<u8>, String> {
                        Err("NO DATA".into())
                    };
                    inspect(&mut silent, sections, frame)
                }
            },
            None => {
                match with_diag_at(reg, &args.conn, (tx, rx), |d| {
                    Ok(inspect(d, sections, frame))
                }) {
                    Ok(i) => i,
                    Err(e) if !args.all_ecus => return Err(e),
                    Err(e) => {
                        let mut i = Inspection::default();
                        i.errors.insert("module".into(), e.to_string());
                        i
                    }
                }
            }
        };
        out.push((tx, rx, insp));
    }
    if args.all_ecus {
        out.retain(|(_, _, i)| i.responded);
    }
    Ok(out)
}

fn ecus_json(ecus: &[(u32, u32, Inspection)]) -> Result<serde_json::Value> {
    let mut list = Vec::new();
    for (tx, rx, i) in ecus {
        let mut v = serde_json::to_value(i)?;
        v["tx"] = format!("{tx:03X}").into();
        v["rx"] = format!("{rx:03X}").into();
        v["label"] = obd_ecu_label(*tx).into();
        list.push(v);
    }
    Ok(serde_json::json!({ "ecus": list }))
}

/// A value with at most 2 decimals, trailing zeros dropped.
fn num(v: f64) -> String {
    let s = format!("{v:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" {
        "0".into()
    } else {
        s.into()
    }
}

fn fmt_value(v: &ObdValue) -> String {
    match v {
        ObdValue::Scalar(x) => num(*x),
        ObdValue::Array(a) => a
            .iter()
            .map(|x| x.map(num).unwrap_or_else(|| "-".into()))
            .collect::<Vec<_>>()
            .join(" / "),
    }
}

fn state_text(s: MonitorState, cycle: bool) -> &'static str {
    match s {
        MonitorState::Complete => "complete",
        MonitorState::Incomplete => "INCOMPLETE",
        MonitorState::NotSupported if cycle => "disabled",
        MonitorState::NotSupported => "not supported",
    }
}

fn print_dtc_list(title: &str, list: &[DtcEntry], err: Option<&String>) {
    match err {
        Some(e) => println!("{title}: not available ({e})"),
        None if list.is_empty() => println!("{title}: none"),
        None => {
            println!("{title}: {}", list.len());
            for d in list {
                println!("  {}  {}", d.code, d.description);
            }
        }
    }
}

fn print_inspection(i: &Inspection) {
    if !i.responded {
        println!(
            "no response ({})",
            i.errors
                .get("module")
                .map(String::as_str)
                .unwrap_or("module silent")
        );
        return;
    }
    for key in ["readiness", "freeze"] {
        if let Some(e) = i.errors.get(key) {
            println!("{key}: {e}");
        }
    }
    if let Some(r) = &i.readiness {
        let s = &r.status;
        println!(
            "MIL          : {}",
            if s.mil {
                "ON (check-engine lamp lit)"
            } else {
                "off"
            }
        );
        println!("Stored DTCs  : {}", s.dtc_count);
        println!(
            "Ignition     : {}",
            serde_json::to_value(s.ignition)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default()
        );
        println!("{:<38} {:<15} This drive cycle", "Monitor", "Since clear");
        for (k, m) in s.monitors.iter().enumerate() {
            let cyc = r
                .this_cycle
                .as_ref()
                .and_then(|c| c.monitors.get(k))
                .map(|c| state_text(c.state, true))
                .unwrap_or("-");
            let name = if m.continuous {
                format!("{} (continuous)", m.name)
            } else {
                m.name.to_string()
            };
            println!("{name:<38} {:<15} {cyc}", state_text(m.state, false));
        }
        let inc: Vec<&str> = s.incomplete().map(|m| m.name).collect();
        if inc.is_empty() {
            println!("All supported monitors complete.");
        } else {
            println!("Not ready: {}", inc.join(", "));
        }
    }
    if let Some(d) = &i.dtcs {
        print_dtc_list("Stored (03)", &d.stored, d.errors.get("stored"));
        print_dtc_list("Pending (07)", &d.pending, d.errors.get("pending"));
        print_dtc_list("Permanent (0A)", &d.permanent, d.errors.get("permanent"));
    }
    if let Some(f) = &i.freeze_frame {
        if !f.stored {
            println!("Freeze frame {}: none stored", f.frame);
        } else {
            match &f.dtc {
                Some(d) => println!(
                    "Freeze frame {} stored by {}  {}",
                    f.frame, d.code, d.description
                ),
                None => println!("Freeze frame {} (trigger DTC not reported)", f.frame),
            }
            for v in &f.values {
                let val = match (&v.value, &v.error) {
                    (Some(x), _) => format!("{} {}", fmt_value(x), v.unit),
                    (None, Some(e)) => format!("- ({e})"),
                    (None, None) => format!("raw {}", v.raw),
                };
                println!("  {:02X} {:<38} {}", v.pid, v.name, val.trim_end());
            }
            for (k, e) in &f.errors {
                println!("  {k}: {e}");
            }
        }
    }
    if let Some(info) = &i.info {
        println!("VIN          : {}", info.vin.as_deref().unwrap_or("-"));
        println!("ECU name     : {}", info.ecu_name.as_deref().unwrap_or("-"));
        let n = info.calids.len().max(info.cvns.len());
        if n == 0 {
            println!("Calibration  : -");
        }
        for k in 0..n {
            println!(
                "CALID {}      : {:<18} CVN {}",
                k + 1,
                info.calids.get(k).map(String::as_str).unwrap_or("-"),
                info.cvns.get(k).map(String::as_str).unwrap_or("-")
            );
        }
        if !info.cvns.is_empty() {
            println!(
                "(A CVN that differs from the stock value for its CALID means the calibration was modified.)"
            );
        }
        for (k, e) in &info.errors {
            println!("{k}: {e}");
        }
    }
}

fn run_inspect(reg: &Registry, args: &InspectArgs, sections: Sections, frame: u8) -> Result<()> {
    let ecus = inspect_ecus(reg, args, sections, frame)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&ecus_json(&ecus)?)?);
        return Ok(());
    }
    if ecus.is_empty() {
        println!("No OBD module answered.");
    }
    for (n, (tx, rx, i)) in ecus.iter().enumerate() {
        if n > 0 {
            println!();
        }
        println!("== ECU {tx:03X}/{rx:03X} ({}) ==", obd_ecu_label(*tx));
        print_inspection(i);
    }
    Ok(())
}

fn only(f: impl FnOnce(&mut Sections)) -> Sections {
    let mut s = Sections {
        readiness: false,
        dtcs: false,
        freeze: false,
        info: false,
    };
    f(&mut s);
    s
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
        ObdAction::Readiness(args) => run_inspect(reg, &args, only(|s| s.readiness = true), 0)?,
        ObdAction::Freeze { args, frame } => {
            run_inspect(reg, &args, only(|s| s.freeze = true), frame)?
        }
        ObdAction::Info(args) => run_inspect(reg, &args, only(|s| s.info = true), 0)?,
        ObdAction::Dtcs(args) => run_inspect(reg, &args, only(|s| s.dtcs = true), 0)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(all: bool) -> InspectArgs {
        InspectArgs {
            conn: ObdConn {
                driver: "obdsim".into(),
                port: None,
                baud: None,
                bus: None,
                timeout_ms: 100,
            },
            all_ecus: all,
            json: true,
        }
    }

    #[test]
    fn obdsim_engine_only_and_all_ecus() {
        let reg = Registry::new();
        let e = inspect_ecus(&reg, &args(false), Sections::ALL, 0).unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].0, 0x7E0);
        assert!(e[0].2.readiness.as_ref().unwrap().status.mil);
        let all = inspect_ecus(&reg, &args(true), Sections::ALL, 0).unwrap();
        assert_eq!(
            all.iter().map(|(t, _, _)| *t).collect::<Vec<_>>(),
            [0x7E0, 0x7E1]
        );
        let j = ecus_json(&all).unwrap();
        assert_eq!(j["ecus"][0]["rx"], "7E8");
        assert_eq!(j["ecus"][1]["label"], "transmission");
        assert_eq!(j["ecus"][0]["info"]["cvns"][0], "1A2B3C4D");
        assert_eq!(j["ecus"][0]["dtcs"]["pending"][0]["code"], "P0299");
        // Text output must not panic for any section mix.
        for (_, _, i) in &all {
            print_inspection(i);
        }
    }

    #[test]
    fn value_formatting() {
        assert_eq!(num(1726.0), "1726");
        assert_eq!(num(50.196), "50.2");
        assert_eq!(num(-0.001), "0");
        assert_eq!(
            fmt_value(&ObdValue::Array(vec![Some(450.0), None])),
            "450 / -"
        );
    }
}
