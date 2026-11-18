//! `opentune dsg`: read-only VAG DQ250 02E (6-speed wet DSG) diagnostics over
//! KWP2000 on a VW TP2.0 channel to the gearbox (address 0x02):
//! identification, stored trouble codes and raw measuring groups.
use anyhow::{anyhow, bail, Result};
use opentune_core::protocol::diag::DiagTransport;
use opentune_core::protocol::tp20sim::Tp20EcuConfig;
use opentune_core::{ConnectConfig, Registry};
use opentune_ctrldef::ControllerDef;
use opentune_dsg::sim::{open_sim_channel, sim_bus, SimDq250};
use opentune_dsg::{
    dsg_connect_config, tp20_refusal, Dq250Reader, Group, GroupLabels, IdentifyReport,
    MeaningState, StorageState, DEFAULT_ID_OPTIONS,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Controller definition the reads link to.
pub const CTRLDEF_ID: &str = "vag-dq250-02e";

#[derive(clap::Subcommand)]
pub enum DsgAction {
    /// Read the identification (KWP 0x1A): part number, software version,
    /// component text (0x9B), hardware number (0x91), system name (0x97).
    Identify {
        #[command(flatten)]
        conn: DsgConn,
        /// Identification option to read instead of the defaults, e.g.
        /// `--id-option 0x9B` (repeatable; 0x80..=0x9F).
        #[arg(long = "id-option", value_name = "OPT", value_parser = parse_u8)]
        id_options: Vec<u8>,
    },
    /// Read the stored trouble codes (KWP 0x18). Read-only: never clears.
    Dtcs {
        #[command(flatten)]
        conn: DsgConn,
    },
    /// Read measuring groups (KWP 0x21) as numbered, formula-decoded cells.
    /// Cell meanings come only from the controller definition's live[]
    /// entries; without one a cell is reported as unconfirmed.
    Groups {
        /// Group numbers (decimal or 0x hex).
        #[arg(required = true, value_name = "GROUP", value_parser = parse_u8)]
        groups: Vec<u8>,
        /// Read the groups this many times.
        #[arg(long, default_value_t = 1)]
        count: u32,
        /// Pause between repeated reads.
        #[arg(long, default_value_t = 500)]
        interval_ms: u64,
        #[command(flatten)]
        conn: DsgConn,
    },
}

#[derive(clap::Args, Clone)]
pub struct DsgConn {
    /// Driver id: a raw-CAN adapter that opens VW TP2.0 (socketcan, slcan,
    /// gsusb). The ELM327 drivers are refused.
    #[arg(long, required_unless_present = "sim")]
    driver: Option<String>,
    /// Run against the built-in scripted DQ250 responder (synthetic data).
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
    /// Extra driver option, e.g. `--opt tp20_keepalive_ms=500` (repeatable).
    /// `transport=tp20` and `tp20_addr=0x02` are implied.
    #[arg(long = "opt", value_name = "KEY=VALUE")]
    opts: Vec<String>,
    /// KWP2000 session started first (default 0x89; `none` skips it).
    #[arg(long, default_value = "0x89")]
    session: String,
    /// Per-request timeout.
    #[arg(long, default_value_t = 2000)]
    timeout_ms: u64,
    /// Directory holding the controller definition `vag-dq250-02e.json`
    /// (identify keys and measuring-group meanings).
    #[arg(long, default_value = "ctrldefs")]
    ctrldefs: PathBuf,
    /// JSON output.
    #[arg(long)]
    json: bool,
}

fn parse_u8(s: &str) -> std::result::Result<u8, String> {
    let t = s.trim();
    let r = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => u8::from_str_radix(h, 16),
        None => t.parse::<u8>(),
    };
    r.map_err(|_| format!("'{s}' is not a number 0..=255 (decimal or 0x hex)"))
}

fn parse_session(s: &str) -> Result<Option<u8>> {
    if s.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    parse_u8(s).map(Some).map_err(|e| anyhow!("--session: {e}"))
}

/// Dispatch `opentune dsg`.
pub fn run(reg: &Registry, action: DsgAction) -> Result<()> {
    match action {
        DsgAction::Identify { conn, id_options } => identify(reg, &conn, &id_options),
        DsgAction::Dtcs { conn } => dtcs(reg, &conn),
        DsgAction::Groups {
            groups,
            count,
            interval_ms,
            conn,
        } => read_groups(reg, &conn, &groups, count.max(1), interval_ms),
    }
}

/// The driver connection: the adapter flags plus the implied TP2.0 options.
fn connect_config(c: &DsgConn, driver: &str) -> Result<ConnectConfig> {
    let conn = crate::cli::ConnectArgs {
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
    dsg_connect_config(conn.config()?).map_err(|e| anyhow!(e))
}

/// Open the channel (simulated or real), run `f` on it, tear it down.
fn with_transport<T>(
    reg: &Registry,
    c: &DsgConn,
    f: impl FnOnce(&mut dyn DiagTransport) -> T,
) -> Result<T> {
    if c.sim {
        // The full stack: the core TP2.0 channel (setup, keep-alive, A8) to the
        // core TP2.0 module simulator around the scripted responder.
        let unit = Arc::new(Mutex::new(SimDq250 {
            dtcs: vec![(12345, 0x23)],
            ..SimDq250::default()
        }));
        let ecu = sim_bus(unit, Tp20EcuConfig::default());
        let mut ch =
            open_sim_channel(ecu, 1000).map_err(|e| anyhow!("open simulated channel: {e}"))?;
        let r = f(&mut ch);
        ch.close();
        return Ok(r);
    }
    let driver = c
        .driver
        .as_deref()
        .ok_or_else(|| anyhow!("--driver or --sim is required"))?;
    if let Some(why) = tp20_refusal(driver) {
        bail!("{why} Use `--sim` to see the read.");
    }
    let cfg = connect_config(c, driver)?;
    let mut drv = reg
        .make(driver)
        .ok_or_else(|| anyhow!("unknown driver '{driver}'"))?;
    if !drv.capabilities().vwtp20 {
        bail!("driver '{driver}' does not report VW TP2.0 support in this build");
    }
    drv.open(&cfg).map_err(|e| anyhow!("open {driver}: {e}"))?;
    let r = match drv.diag() {
        Some(t) => Ok(f(t)),
        None => Err(anyhow!("driver '{driver}' exposes no diagnostic transport")),
    };
    let _ = drv.close();
    r
}

/// The linked controller definition, if its file is present.
fn load_def(dir: &Path) -> Option<ControllerDef> {
    let p = dir.join(format!("{CTRLDEF_ID}.json"));
    let text = std::fs::read_to_string(p).ok()?;
    ControllerDef::from_json(&text).ok()
}

fn identify(reg: &Registry, c: &DsgConn, options: &[u8]) -> Result<()> {
    let session = parse_session(&c.session)?;
    let options = if options.is_empty() {
        DEFAULT_ID_OPTIONS.to_vec()
    } else {
        options.to_vec()
    };
    let (session_error, rep) = with_transport(reg, c, |t| {
        let mut r = Dq250Reader::start(t, session, c.timeout_ms);
        let rep = r.identify(&options);
        (r.session_error, rep)
    })?;
    let def = load_def(&c.ctrldefs);
    let matches = def.as_ref().and_then(|d| {
        let id = rep.identity.as_ref()?;
        (!id.part_number.is_empty()).then(|| d.identify.matches_part(&id.part_number))
    });
    if c.json {
        let v = serde_json::json!({
            "simulated": c.sim,
            "session_error": session_error,
            "definition": def.as_ref().map(|d| d.id.clone()),
            "matches_definition": matches,
            "identify": rep,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        header(c.sim, session, &session_error);
        print_identify(&rep);
        match matches {
            Some(true) => println!("definition   {CTRLDEF_ID} (part number matches)"),
            Some(false) => println!("definition   part number does not match {CTRLDEF_ID}"),
            None => {}
        }
    }
    if !rep.responded() {
        bail!("the gearbox did not answer any identification request");
    }
    Ok(())
}

fn header(sim: bool, session: Option<u8>, err: &Option<String>) {
    if sim {
        println!("(simulated DQ250 responder: synthetic data)");
    }
    if let (Some(s), Some(e)) = (session, err) {
        println!("session 0x{s:02X}: {e} (continuing)");
    }
}

fn print_identify(r: &IdentifyReport) {
    if let Some(id) = &r.identity {
        println!("part number  {}", id.part_number);
        println!("software     {}", id.sw_version);
        println!("component    {}", id.component);
        if let Some(h) = &id.hardware_number {
            println!("hardware     {h}");
        }
        if let Some(n) = &id.system_name {
            println!("system name  {n}");
        }
        println!("family       {:?}", id.family);
        if !id.extra_hex.is_empty() {
            println!("extra bytes  {} (not decoded)", id.extra_hex);
        }
    }
    for rec in &r.records {
        println!(
            "  1A {:02X}  {}  {}",
            rec.option,
            rec.name.unwrap_or("-"),
            rec.hex
        );
    }
    for e in &r.errors {
        println!("  1A {:02X}  not available: {}", e.option, e.error);
    }
}

fn storage_text(s: StorageState) -> &'static str {
    match s {
        StorageState::NotStored => "not stored",
        StorageState::StoredNotPresent => "stored, not present",
        StorageState::Pending => "pending",
        StorageState::Present => "present",
    }
}

fn dtcs(reg: &Registry, c: &DsgConn) -> Result<()> {
    let session = parse_session(&c.session)?;
    let (session_error, res) = with_transport(reg, c, |t| {
        let mut r = Dq250Reader::start(t, session, c.timeout_ms);
        let d = r.dtcs();
        (r.session_error, d)
    })?;
    if c.json {
        let v = serde_json::json!({
            "simulated": c.sim,
            "session_error": session_error,
            "dtcs": res.as_ref().ok(),
            "error": res.as_ref().err(),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        header(c.sim, session, &session_error);
        match &res {
            Ok(d) if d.is_empty() => println!("trouble codes: none stored"),
            Ok(d) => {
                println!("trouble codes: {}", d.len());
                for x in d {
                    let mut flags = vec![storage_text(x.status.storage)];
                    if x.status.warning_lamp {
                        flags.push("warning lamp");
                    }
                    if x.status.readiness {
                        flags.push("readiness flag");
                    }
                    println!(
                        "  {:05}  status 0x{:02X}  {}",
                        x.vag_number,
                        x.status_byte,
                        flags.join(", ")
                    );
                }
            }
            Err(_) => {}
        }
    }
    res.map(|_| ())
        .map_err(|e| anyhow!("read trouble codes: {e}"))
}

fn read_groups(
    reg: &Registry,
    c: &DsgConn,
    groups: &[u8],
    count: u32,
    interval_ms: u64,
) -> Result<()> {
    let session = parse_session(&c.session)?;
    let def = load_def(&c.ctrldefs);
    let labels = def
        .as_ref()
        .map(GroupLabels::from_ctrldef)
        .unwrap_or_default();
    let start = Instant::now();
    let (session_error, samples) = with_transport(reg, c, |t| {
        let mut r = Dq250Reader::start(t, session, c.timeout_ms);
        let mut samples = Vec::new();
        for i in 0..count {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(interval_ms));
            }
            let t_ms = start.elapsed().as_millis() as u64;
            let gs: Vec<Group> = groups.iter().map(|&g| r.group(g, &labels)).collect();
            samples.push((t_ms, gs));
        }
        (r.session_error, samples)
    })?;
    let answered = samples
        .iter()
        .flat_map(|(_, gs)| gs)
        .any(|g| g.error.is_none());
    if c.json {
        let s: Vec<_> = samples
            .iter()
            .map(|(t, gs)| serde_json::json!({"t_ms": t, "groups": gs}))
            .collect();
        let v = serde_json::json!({
            "simulated": c.sim,
            "session_error": session_error,
            "definition": def.as_ref().map(|d| d.id.clone()),
            "defined_cells": labels.len(),
            "samples": s,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        header(c.sim, session, &session_error);
        if labels.is_empty() {
            println!(
                "(no measuring-group meanings defined for the DQ250 yet: every cell is unconfirmed)"
            );
        }
        for (t, gs) in &samples {
            if count > 1 {
                println!("t = {t} ms");
            }
            for g in gs {
                print_group(g);
            }
        }
    }
    if !answered {
        bail!("the gearbox answered none of the requested measuring groups");
    }
    Ok(())
}

fn print_group(g: &Group) {
    if let Some(e) = &g.error {
        println!("group {:3}: {e}", g.group);
        return;
    }
    println!("group {:3}: {}", g.group, g.raw_hex);
    for c in &g.cells {
        let val = c
            .value
            .map(|x| format!("{x:.3} {}", c.unit))
            .unwrap_or_else(|| "raw".into());
        let what = match (&c.meaning, c.state) {
            (Some(m), MeaningState::FromDefinition) => format!("{} ({})", m.name, m.source),
            _ => "unconfirmed".into(),
        };
        println!(
            "    cell {}  f=0x{:02X} a={:3} b={:3}  {val:<16} {what}",
            c.cell, c.formula, c.a, c.b
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(driver: Option<&str>, sim: bool) -> DsgConn {
        DsgConn {
            driver: driver.map(str::to_string),
            sim,
            port: None,
            baud: None,
            bus: None,
            opts: vec!["tp20_keepalive_ms=500".into()],
            session: "0x89".into(),
            timeout_ms: 200,
            ctrldefs: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ctrldefs"),
            json: true,
        }
    }

    #[test]
    fn numbers_and_sessions_parse() {
        assert_eq!(parse_u8("0x9B"), Ok(0x9B));
        assert_eq!(parse_u8("125"), Ok(125));
        assert!(parse_u8("256").is_err());
        assert_eq!(parse_session("none").unwrap(), None);
        assert_eq!(
            parse_session("0x89").unwrap(),
            Some(opentune_dsg::KWP_SESSION)
        );
    }

    #[test]
    fn elm_is_refused_and_raw_can_is_not() {
        let reg = Registry::new();
        let e = dtcs(&reg, &conn(Some("elm327obd"), false)).unwrap_err();
        assert!(e.to_string().contains("TP2.0"), "{e}");
        let e = dtcs(&reg, &conn(Some("slcan"), false)).unwrap_err();
        assert!(e.to_string().contains("unknown driver"), "{e}");
    }

    #[test]
    fn config_implies_tp20_to_the_gearbox() {
        let c = connect_config(&conn(Some("slcan"), false), "slcan").unwrap();
        assert_eq!(c.options["transport"], "tp20");
        assert_eq!(c.options["tp20_addr"], "0x02");
        assert_eq!(c.options["tp20_keepalive_ms"], "500");
    }

    #[test]
    fn simulated_reads_succeed() {
        let reg = Registry::new();
        let c = conn(None, true);
        identify(&reg, &c, &[]).unwrap();
        dtcs(&reg, &c).unwrap();
        read_groups(&reg, &c, &[1, 2, 99], 2, 1).unwrap();
        // Only unimplemented groups: an error.
        assert!(read_groups(&reg, &c, &[99], 1, 1).is_err());
    }

    #[test]
    fn linked_definition_is_found() {
        let d = load_def(&conn(None, true).ctrldefs).unwrap();
        assert_eq!(d.id, CTRLDEF_ID);
        assert!(d.identify.matches_part("02E 300 000 X"));
        // The synthetic responder's part number is deliberately not a real one.
        assert!(!d.identify.matches_part("02E000000X"));
    }
}
