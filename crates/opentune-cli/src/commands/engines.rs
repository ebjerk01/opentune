//! The data-driven diagnostic engines over a driver's `DiagTransport`:
//! coding, adaptation, auto-scan, service functions, logging, and the
//! offline UDS simulator.
use crate::cli::{AdaptAction, CodingAction, CodingConn, ServiceAction};
use crate::context::CliContext;
use anyhow::{anyhow, Context, Result};
use opentune_core::{ConnectConfig, Interface, Registry};

/// Open the `conn`'s driver and borrow its diagnostic transport, erroring
/// clearly when the driver cannot expose one.
/// Apply serial-port overrides from the environment to a connect config, so a
/// live serial driver (e.g. `elm327obd`) can be pointed at whatever device it
/// enumerates as without a per-command flag:
///   `OPENTUNE_PORT=/dev/ttyUSB1 OPENTUNE_BAUD=115200 opentune autoscan …`
/// Unset variables leave the driver's own defaults (elm327obd: /dev/ttyUSB0
/// @38400) untouched.
fn apply_serial_env(cfg: &mut ConnectConfig) {
    if let Ok(port) = std::env::var("OPENTUNE_PORT") {
        if !port.trim().is_empty() {
            cfg.options.insert("port".into(), port);
        }
    }
    if let Ok(baud) = std::env::var("OPENTUNE_BAUD") {
        if !baud.trim().is_empty() {
            cfg.options.insert("baud".into(), baud);
        }
    }
}

fn open_coding_driver(reg: &Registry, conn: &CodingConn) -> Result<Box<dyn Interface>> {
    let mut drv = reg
        .make(&conn.driver)
        .ok_or_else(|| anyhow!("unknown driver '{}'", conn.driver))?;
    let mut cfg = ConnectConfig::default();
    if let Some(store) = &conn.store {
        cfg.options
            .insert("coding_store".into(), store.to_string_lossy().into_owned());
    }
    apply_serial_env(&mut cfg);
    drv.open(&cfg)
        .map_err(|e| anyhow!("open {}: {e}", conn.driver))?;
    Ok(drv)
}

/// Load and semantically validate a coding definition from `conn.def`.
fn load_coding_def(conn: &CodingConn) -> Result<opentune_coding::CodingDef> {
    let text = std::fs::read_to_string(&conn.def)
        .with_context(|| format!("read coding def {}", conn.def.display()))?;
    let def = opentune_coding::CodingDef::from_json(&text)
        .map_err(|e| anyhow!("parse coding def {}: {e}", conn.def.display()))?;
    def.validate()
        .map_err(|e| anyhow!("invalid coding def {}: {e}", conn.def.display()))?;
    Ok(def)
}

/// Open a driver that exposes a diagnostic transport, optionally backing the
/// `mock` driver's offline UDS with a `--store` JSON. Shared by the adaptation,
/// auto-scan and service-function subcommands.
fn open_diag_driver(
    reg: &Registry,
    driver: &str,
    store: Option<&std::path::Path>,
) -> Result<Box<dyn Interface>> {
    let mut drv = reg
        .make(driver)
        .ok_or_else(|| anyhow!("unknown driver '{driver}'"))?;
    let mut cfg = ConnectConfig::default();
    if let Some(store) = store {
        cfg.options
            .insert("coding_store".into(), store.to_string_lossy().into_owned());
    }
    apply_serial_env(&mut cfg);
    drv.open(&cfg).map_err(|e| anyhow!("open {driver}: {e}"))?;
    Ok(drv)
}

/// Parse a whitespace-tolerant hex-byte string (`"22 F1 90"` or `"22F190"`).
fn parse_hex_vec(s: &str) -> Result<Vec<u8>> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !clean.len().is_multiple_of(2) {
        return Err(anyhow!("hex string '{s}' has an odd number of digits"));
    }
    (0..clean.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&clean[i..i + 2], 16)
                .map_err(|_| anyhow!("invalid hex byte in '{s}'"))
        })
        .collect()
}

/// Dispatch the `simulate` subcommand: run the in-memory UDS server offline.
pub fn run_simulate(
    dids: &[String],
    require_security_write: bool,
    requests: &[String],
) -> Result<()> {
    use opentune_udssim::server::{ServerConfig, UdsServer};

    let config = ServerConfig {
        require_security_for_write: require_security_write,
        ..Default::default()
    };
    let mut server = UdsServer::with_config(config);

    for d in dids {
        let (id, hex) = d
            .split_once('=')
            .ok_or_else(|| anyhow!("--did expects ID=HEX, got '{d}'"))?;
        let did = u16::from_str_radix(id.trim().trim_start_matches("0x"), 16)
            .map_err(|_| anyhow!("invalid DID '{id}' (expected hex, e.g. F190)"))?;
        server.set_did(did, parse_hex_vec(hex)?);
    }

    for r in requests {
        let req = parse_hex_vec(r)?;
        let resp = server.handle(&req);
        println!("-> {:<20} <- {}", hex_bytes(&req), hex_bytes(&resp));
    }
    Ok(())
}

/// Format bytes as uppercase, space-separated hex.
fn hex_bytes(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Build the `log` command's fast-logging policy from its flags, validating
/// that every dynamic DID it may use lies in 0xF200..=0xF3FF.
pub fn fast_log_config(
    fast: bool,
    dynamic_did: &str,
    max_record: usize,
    session: Option<&str>,
) -> Result<opentune_logdef::FastLogConfig> {
    let did = crate::util::parse_num(dynamic_did)?;
    let first_did = u16::try_from(did)
        .map_err(|_| anyhow!("--dynamic-did {dynamic_did} is not a 16-bit DID"))?;
    let session = session
        .map(|s| {
            crate::util::parse_num(s).and_then(|v| {
                u8::try_from(v).map_err(|_| anyhow!("--fast-session {s} is not one byte"))
            })
        })
        .transpose()?;
    let cfg = opentune_logdef::FastLogConfig {
        enabled: fast,
        first_did,
        max_record_len: max_record,
        session,
        ..Default::default()
    };
    cfg.validate().map_err(|e| anyhow!("--dynamic-did: {e}"))?;
    Ok(cfg)
}

/// `log --gps`: the GPS source and how to pair it with the log.
pub struct LogGps {
    /// Serial port or recorded NMEA file.
    pub source: String,
    /// Baud rate for a UART receiver.
    pub baud: u32,
    /// Explicit vehicle-speed channel id for `speed_error_kmh`.
    pub speed_param: Option<String>,
}

/// Dispatch the `log` subcommand: parse a logger.xml and log live data to CSV,
/// per-channel polling or (with `fast.enabled`) over UDS 0x2C dynamic DIDs.
/// The achieved rate and the mode (with the fallback reason) go to stderr.
/// With `gps`, each round also gets the GPS channels.
#[allow(clippy::too_many_arguments)]
pub fn run_log(
    reg: &Registry,
    def_path: &std::path::Path,
    driver: &str,
    store: Option<&std::path::Path>,
    timeout_ms: u64,
    rounds: usize,
    interval_ms: u64,
    out: Option<&std::path::Path>,
    params: &[String],
    fast: opentune_logdef::FastLogConfig,
    gps: Option<LogGps>,
) -> Result<()> {
    use opentune_core::CoreDiag;
    use opentune_logdef::{LiveSession, LogMode};

    let raw = std::fs::read(def_path)
        .with_context(|| format!("read logger def {}", def_path.display()))?;
    let src = String::from_utf8_lossy(&raw);
    let def = opentune_logdef::parse(&src)
        .map_err(|e| anyhow!("parse logger def {}: {e:?}", def_path.display()))?;

    let ids: Vec<&str> = params.iter().map(String::as_str).collect();

    let mut drv = open_diag_driver(reg, driver, store)?;
    let transport = drv
        .diag()
        .ok_or_else(|| anyhow!("driver '{driver}' does not expose a diagnostic transport"))?;
    let mut diag = CoreDiag::new(transport, timeout_ms);

    let mut session = LiveSession::new(&def, &ids, fast).map_err(|e| anyhow!("{e}"))?;
    let mut gps = match gps {
        Some(g) => {
            let mut feed =
                opentune_gps::open_source(&g.source, g.baud).map_err(|e| anyhow!("{e}"))?;
            feed.wait_ready(std::time::Duration::from_millis(2500));
            let logger =
                opentune_logdef::GpsLogger::new(feed, session.params(), g.speed_param.as_deref())
                    .map_err(|e| anyhow!("--gps-speed-param: {e}"))?;
            let speed = match logger.speed_channel() {
                Some(i) => format!(
                    "speed_error_kmh vs '{}'",
                    session.params().get(i).map_or("?", |p| p.id.as_str())
                ),
                None => "no vehicle-speed channel, no speed_error_kmh".into(),
            };
            eprintln!("gps: {} ({speed})", logger.describe());
            Some(logger)
        }
        None => None,
    };
    let started = session.start(&mut diag).clone();
    match started.mode {
        LogMode::Fast2c => {
            let dids: Vec<String> = started
                .dynamic_dids
                .iter()
                .map(|d| {
                    format!(
                        "0x{:04X} ({} B, {} ch)",
                        d.did,
                        d.record_len,
                        d.channels.len()
                    )
                })
                .collect();
            eprintln!(
                "mode: fast-2c via {}; {} channel(s) polled individually",
                dids.join(", "),
                started.polled.len()
            );
            for n in &started.notes {
                eprintln!("  note: {n}");
            }
        }
        LogMode::Polling => eprintln!(
            "mode: polling ({})",
            started.reason.as_deref().unwrap_or("no reason given")
        ),
    }

    let t0 = std::time::Instant::now();
    let result = match out {
        Some(path) => {
            let mut f = std::fs::File::create(path)
                .map_err(|e| anyhow!("create {}: {e}", path.display()))?;
            session.run_csv_gps(&mut diag, rounds, interval_ms, gps.as_mut(), &mut f)
        }
        None => {
            let mut stdout = std::io::stdout().lock();
            session.run_csv_gps(&mut diag, rounds, interval_ms, gps.as_mut(), &mut stdout)
        }
    };
    let elapsed = t0.elapsed().as_secs_f64();
    // Always clear the dynamic DIDs, even if writing the CSV failed.
    let st = session.stop(&mut diag).clone();
    result?;

    if st.mode == LogMode::Polling && started.mode == LogMode::Fast2c {
        eprintln!(
            "mode: fell back to polling mid-log ({})",
            st.reason.as_deref().unwrap_or("no reason given")
        );
    }
    let channels = session.params().len();
    let rate = if elapsed > 0.0 {
        rounds as f64 / elapsed
    } else {
        0.0
    };
    eprintln!(
        "{rounds} sample(s) of {channels} channel(s) in {elapsed:.3} s: {rate:.1} samples/s \
         ({:.1} channel values/s), {:.2} requests/sample, {} setup request(s), {} re-define(s)",
        rate * channels as f64,
        st.requests_per_tick(),
        st.setup_requests,
        st.redefines
    );
    if let Some(g) = &gps {
        let (good, ticks) = g.good_ticks();
        eprintln!(
            "gps: {good} of {ticks} sample(s) with a good fix; {}",
            g.stats()
        );
    }
    if let Some(path) = out {
        println!("Logged {rounds} round(s) to {}", path.display());
    }
    Ok(())
}

/// Dispatch the `adaptation` subcommand.
pub fn run_adaptation(
    reg: &Registry,
    def_path: &std::path::Path,
    driver: &str,
    store: Option<&std::path::Path>,
    timeout_ms: u64,
    action: AdaptAction,
) -> Result<()> {
    use opentune_adaptation::AdaptationDef;
    use opentune_core::CoreDiag;

    let text = std::fs::read_to_string(def_path)
        .with_context(|| format!("read adaptation def {}", def_path.display()))?;
    let def = AdaptationDef::from_json(&text)
        .map_err(|e| anyhow!("parse adaptation def {}: {e}", def_path.display()))?;
    def.validate()
        .map_err(|e| anyhow!("invalid adaptation def {}: {e}", def_path.display()))?;

    match action {
        AdaptAction::List => {
            println!("Adaptation channels in {} ({}):", def.name, def.id);
            for ch in &def.channels {
                println!("  {}", ch.name);
            }
        }
        AdaptAction::Read { channel } => {
            let mut drv = open_diag_driver(reg, driver, store)?;
            let transport = drv.diag().ok_or_else(|| {
                anyhow!("driver '{driver}' does not expose a diagnostic transport")
            })?;
            let mut diag = CoreDiag::new(transport, timeout_ms);
            let r = opentune_adaptation::read_value(&mut diag, &def, &channel)
                .map_err(|e| anyhow!("{e}"))?;
            println!(
                "{}: raw={} engineering={} {}",
                r.channel, r.raw, r.engineering, r.unit
            );
        }
    }
    Ok(())
}

/// Dispatch the `autoscan` subcommand (read-only whole-vehicle scan).
pub fn run_autoscan(
    reg: &Registry,
    modules_path: &std::path::Path,
    driver: &str,
    store: Option<&std::path::Path>,
    timeout_ms: u64,
) -> Result<()> {
    use opentune_autoscan::VehicleModules;
    use opentune_core::CoreDiag;

    let text = std::fs::read_to_string(modules_path)
        .with_context(|| format!("read module map {}", modules_path.display()))?;
    let modules = VehicleModules::from_json(&text)
        .map_err(|e| anyhow!("parse module map {}: {e}", modules_path.display()))?;

    // Bind the adapter to each module's own request/response ids before
    // probing it (as the desktop does): one binding would send every module's
    // requests to the one ECU it is bound to.
    let mut results = Vec::with_capacity(modules.modules.len());
    for m in &modules.modules {
        let mut drv = reg
            .make(driver)
            .ok_or_else(|| anyhow!("unknown driver '{driver}'"))?;
        let mut cfg = ConnectConfig {
            tx_id: m.req_id,
            rx_id: m.resp_id,
            ..ConnectConfig::default()
        };
        if let Some(store) = store {
            cfg.options
                .insert("coding_store".into(), store.to_string_lossy().into_owned());
        }
        apply_serial_env(&mut cfg);
        drv.open(&cfg).map_err(|e| anyhow!("open {driver}: {e}"))?;
        let transport = drv
            .diag()
            .ok_or_else(|| anyhow!("driver '{driver}' does not expose a diagnostic transport"))?;
        let mut diag = CoreDiag::new(transport, timeout_ms);
        results.push(opentune_autoscan::scan_module(&mut diag, m));
        let _ = drv.close();
    }
    let scan = opentune_autoscan::VehicleScan { modules: results };

    let present = scan.present().count();
    println!(
        "Scanned {} modules: {} present, {} DTC(s) total",
        scan.modules.len(),
        present,
        scan.dtc_count()
    );
    for m in &scan.modules {
        let mark = if m.present { "+" } else { "-" };
        print!("  [{mark}] {:<24}", m.name);
        if let Some(id) = &m.identity {
            if let Some(vin) = &id.vin {
                print!(" VIN={vin}");
            }
        }
        if !m.dtcs.is_empty() {
            print!(" {} DTC(s)", m.dtcs.len());
        }
        if let Some(err) = &m.error {
            print!(" ({err})");
        }
        println!();
    }
    Ok(())
}

/// A [`SeedKey`](opentune_core::protocol::seedkey::SeedKey) handler used as a
/// service script's [`KeyProvider`](opentune_servicefn::KeyProvider).
struct SeedKeyProvider(Box<dyn opentune_core::protocol::seedkey::SeedKey>);

impl opentune_servicefn::KeyProvider for SeedKeyProvider {
    fn key(&self, seed: &[u8], level: u8) -> std::result::Result<Vec<u8>, String> {
        self.0.compute(seed, level).map_err(|e| e.to_string())
    }
}

/// Dispatch the `service` subcommand.
pub fn run_service(
    ctx: &CliContext,
    script_path: &std::path::Path,
    driver: &str,
    store: Option<&std::path::Path>,
    timeout_ms: u64,
    seedkey: Option<&str>,
    action: ServiceAction,
) -> Result<()> {
    use opentune_core::CoreDiag;
    use opentune_servicefn::{run_script, RunOptions, ServiceScript};

    let text = std::fs::read_to_string(script_path)
        .with_context(|| format!("read service script {}", script_path.display()))?;
    let script = ServiceScript::from_json(&text)
        .map_err(|e| anyhow!("parse service script {}: {e}", script_path.display()))?;

    let dry_run = matches!(action, ServiceAction::Plan);
    if let ServiceAction::Run { ack } = &action {
        if !ack {
            return Err(anyhow!(
                "`service run` sends real requests; re-run with --ack (or use `service plan`)"
            ));
        }
    }

    let key = seedkey
        .map(|id| ctx.require_seed_key(id).map(SeedKeyProvider))
        .transpose()?;

    let mut drv = open_diag_driver(ctx.registry(), driver, store)?;
    let transport = drv
        .diag()
        .ok_or_else(|| anyhow!("driver '{driver}' does not expose a diagnostic transport"))?;
    let mut diag = CoreDiag::new(transport, timeout_ms);

    let opts = RunOptions {
        dry_run,
        skip_waits: false,
    };
    // OpenTune ships no OEM seed/key provider: without `--seedkey` (resolved by
    // an embedding build) a secured script errors cleanly.
    let key = key
        .as_ref()
        .map(|k| k as &dyn opentune_servicefn::KeyProvider);
    let report = run_script(&mut diag, &script, key, &opts).map_err(|e| anyhow!("{e}"))?;

    println!(
        "Service '{}': {:?}{}",
        report.script_id,
        report.outcome,
        if report.security_unlocked {
            " (security unlocked)"
        } else {
            ""
        }
    );
    for s in &report.steps {
        println!(
            "  #{:<2} {:<20} {:?}  {}",
            s.index, s.op, s.status, s.detail
        );
    }
    Ok(())
}

/// Resolve a user-supplied value for `name`: a number (decimal or `0x` hex), or
/// an enum option label declared for that parameter.
fn resolve_coding_value(def: &opentune_coding::CodingDef, name: &str, value: &str) -> Result<u64> {
    let v = value.trim();
    let num = if let Some(hex) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        v.parse::<u64>().ok()
    };
    if let Some(n) = num {
        return Ok(n);
    }
    // Not numeric: try to match an enum option label.
    let param = def
        .param(name)
        .ok_or_else(|| anyhow!("unknown parameter '{name}'"))?;
    param
        .options
        .iter()
        .find(|o| o.label.eq_ignore_ascii_case(v))
        .map(|o| o.value)
        .ok_or_else(|| {
            anyhow!("value '{value}' is neither a number nor a known option of '{name}'")
        })
}

/// Print one coding's blocks as `DID  hex`.
fn print_coding_blocks(coding: &opentune_coding::Coding) {
    for b in &coding.blocks {
        let hex: String = b.bytes.iter().map(|x| format!("{x:02X}")).collect();
        println!("  0x{:04X}  {}", b.did, hex);
    }
}

/// Print a write report's plan (byte + parameter changes) and its outcome.
fn print_coding_report(rep: &opentune_coding::WriteReport) {
    if rep.security_required {
        println!("note: this module requires SecurityAccess (0x27) to be unlocked first");
    }
    if rep.param_changes.is_empty() {
        println!("No parameter changes.");
    } else {
        println!("Parameter changes:");
        for pc in &rep.param_changes {
            println!(
                "  {:<20} {} ({}) -> {} ({})",
                pc.name, pc.old.0, pc.old.1, pc.new.0, pc.new.1
            );
        }
    }
    if !rep.byte_changes.is_empty() {
        println!("Byte changes:");
        for bc in &rep.byte_changes {
            println!(
                "  0x{:04X}[{}] 0x{:02X} -> 0x{:02X}",
                bc.did, bc.byte, bc.old, bc.new
            );
        }
    }
    match &rep.outcome {
        opentune_coding::WriteOutcome::Planned => {
            println!("Outcome: planned (dry run, nothing written)")
        }
        opentune_coding::WriteOutcome::NoChange => println!("Outcome: no change (nothing written)"),
        opentune_coding::WriteOutcome::Verified => println!("Outcome: written and verified"),
        opentune_coding::WriteOutcome::Unverified => println!("Outcome: written (verify skipped)"),
        opentune_coding::WriteOutcome::VerifyFailed { did } => {
            println!("Outcome: VERIFY FAILED for block 0x{did:04X}")
        }
        opentune_coding::WriteOutcome::WriteError {
            uncertain_did,
            message,
        } => println!(
            "Outcome: WRITE ERROR at block 0x{uncertain_did:04X} ({message}); backup retained"
        ),
    }
}

/// Dispatch the `coding` subcommand: load the def, obtain a transport from the
/// selected driver, wrap it in [`opentune_coding::CoreDiag`] and run the op.
pub fn run_coding(reg: &Registry, action: CodingAction) -> Result<()> {
    use opentune_coding::{read_coding, set, write_coding, CoreDiag, WriteOptions};

    let conn = match &action {
        CodingAction::Read { conn }
        | CodingAction::Decode { conn }
        | CodingAction::Set { conn, .. }
        | CodingAction::Write { conn, .. } => conn,
    };
    let def = load_coding_def(conn)?;
    let timeout = conn.timeout_ms;
    let mut drv = open_coding_driver(reg, conn)?;
    let transport = drv.diag().ok_or_else(|| {
        anyhow!(
            "driver '{}' does not expose a diagnostic transport for coding (try --driver mock --store <file>)",
            conn.driver
        )
    })?;
    let mut diag = CoreDiag::new(transport, timeout);

    match action {
        CodingAction::Read { .. } => {
            let coding = read_coding(&mut diag, &def).map_err(|e| anyhow!("{e}"))?;
            println!("Coding for {} ({}):", def.name, def.id);
            print_coding_blocks(&coding);
        }
        CodingAction::Decode { .. } => {
            let coding = read_coding(&mut diag, &def).map_err(|e| anyhow!("{e}"))?;
            let params = opentune_coding::decode(&def, &coding).map_err(|e| anyhow!("{e}"))?;
            println!("Decoded coding for {} ({}):", def.name, def.id);
            for p in params {
                println!("  {:<20} {:>6}  {}", p.name, p.value, p.label);
            }
        }
        CodingAction::Set { name, value, .. } => {
            let val = resolve_coding_value(&def, &name, &value)?;
            // Preview only: read fresh as the baseline, apply the edit, dry-run.
            let baseline = read_coding(&mut diag, &def).map_err(|e| anyhow!("{e}"))?;
            let desired = set(&def, &baseline, &name, val).map_err(|e| anyhow!("{e}"))?;
            let opts = WriteOptions {
                dry_run: true,
                ..Default::default()
            };
            let rep = write_coding(&mut diag, &def, &baseline, &desired, &opts)
                .map_err(|e| anyhow!("{e}"))?;
            println!("Planned change: {name} = {value}");
            print_coding_report(&rep);
        }
        CodingAction::Write {
            name,
            value,
            dry_run,
            ..
        } => {
            let val = resolve_coding_value(&def, &name, &value)?;
            let baseline = read_coding(&mut diag, &def).map_err(|e| anyhow!("{e}"))?;
            let desired = set(&def, &baseline, &name, val).map_err(|e| anyhow!("{e}"))?;
            let opts = WriteOptions {
                dry_run,
                ..Default::default()
            };
            let rep = write_coding(&mut diag, &def, &baseline, &desired, &opts)
                .map_err(|e| anyhow!("{e}"))?;
            print_coding_report(&rep);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::fast_log_config;

    #[test]
    fn fast_log_flags_validate_the_dynamic_range() {
        let cfg = fast_log_config(true, "0xF200", 32, Some("0x03")).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.first_did, 0xF200);
        assert_eq!(cfg.max_record_len, 32);
        assert_eq!(cfg.session, Some(0x03));
        assert!(!fast_log_config(false, "0xF3FC", 8, None).unwrap().enabled);
        // Outside 0xF200..=0xF3FF, not 16-bit, a zero record, a 2-byte session.
        assert!(fast_log_config(true, "0xF190", 32, None).is_err());
        assert!(fast_log_config(true, "0x1F200", 32, None).is_err());
        assert!(fast_log_config(true, "0xF200", 0, None).is_err());
        assert!(fast_log_config(true, "0xF200", 32, Some("0x103")).is_err());
    }
}
