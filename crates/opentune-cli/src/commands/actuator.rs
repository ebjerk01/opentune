//! `opentune adaptation actuator list|run`: timed actuator / output tests
//! (UDS InputOutputControlByIdentifier 0x2F) on the actuator channels an
//! `AdaptationDef` declares — the command-line twin of the desktop Actuator
//! tests panel, with the same rules: declared channels and options only, the
//! value range and time limit of the definition, a dry-run plan unless `--ack`
//! is given, a known module address, SecurityAccess through `--seedkey` when
//! the definition requires it, and returnControlToECU at the end, on Ctrl-C
//! and on any error. There is no free-form DID here (that stays behind the
//! desktop's alpha Debug gate).
use crate::cli::{ActuatorAction, ActuatorConn, ConnectArgs};
use crate::context::CliContext;
use anyhow::{anyhow, bail, Result};
use opentune_adaptation::actuator::{
    self, plan_test, run_test, ActuatorInfo, Exchange, PlanOptions, RealClock, TestClock,
    TestLogin, TestOutcome, TestPlan, TestReport, TestRequest, TestValue,
};
use opentune_adaptation::{AdaptationDef, IoControl};
use opentune_core::CoreDiag;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn hexs(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{}", (v * 1000.0).round() / 1000.0)
    }
}

fn info_line(a: &ActuatorInfo) -> String {
    let opts = a
        .options
        .iter()
        .map(|o| o.label())
        .collect::<Vec<_>>()
        .join(", ");
    let value = match &a.value {
        Some(v) => format!("value {}..{} {}", num(v.min), num(v.max), v.unit),
        None => format!("state {}", hexs(&a.default_state)),
    };
    format!(
        "  {:<26} DID 0x{:04X}  {value}  up to {} s\n      options: {opts}{}",
        a.name,
        a.did,
        a.max_duration_ms as f64 / 1000.0,
        if a.note.is_empty() {
            String::new()
        } else {
            format!("\n      {}", a.note)
        }
    )
}

/// The definition's own `req_id` / `resp_id` (number, or hex string).
fn def_ids(text: &str) -> Result<Option<(u32, u32)>> {
    let v: serde_json::Value = serde_json::from_str(text)?;
    let get = |k: &str| -> Result<Option<u32>> {
        Ok(match v.get(k) {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::Number(n)) => Some(
                n.as_u64()
                    .filter(|n| *n <= 0x1FFF_FFFF)
                    .ok_or_else(|| anyhow!("definition {k} is not a CAN id"))?
                    as u32,
            ),
            Some(serde_json::Value::String(s)) => {
                let h = s.trim().trim_start_matches("0x").trim_start_matches("0X");
                Some(
                    u32::from_str_radix(h, 16)
                        .map_err(|_| anyhow!("definition {k} '{s}' is not hex"))?,
                )
            }
            Some(_) => bail!("definition {k} must be a number or a hex string"),
        })
    };
    match (get("req_id")?, get("resp_id")?) {
        (Some(t), Some(r)) => Ok(Some((t, r))),
        (None, None) => Ok(None),
        _ => bail!("the definition has only one of req_id / resp_id"),
    }
}

fn print_plan(p: &TestPlan) {
    let what = match (p.value, p.raw) {
        (Some(v), Some(r)) => format!(" {} {} (raw {r})", num(v), p.unit),
        _ => String::new(),
    };
    println!(
        "Actuator test: {} (DID 0x{:04X}) {}{what}{}",
        p.channel,
        p.did,
        p.option.label(),
        if p.duration_ms > 0 {
            format!(" for {} ms", p.duration_ms)
        } else {
            String::new()
        }
    );
    for s in &p.steps {
        println!(
            "  {:<9} {:<22} {}",
            format!("{:?}", s.phase).to_lowercase(),
            hexs(&s.request),
            s.note
        );
    }
}

/// Prints each exchange as it happens.
struct Live {
    inner: RealClock,
    quiet: bool,
}

impl TestClock for Live {
    fn now_ms(&mut self) -> u64 {
        self.inner.now_ms()
    }
    fn wait(&mut self, ms: u64) {
        self.inner.wait(ms)
    }
    fn stop_requested(&mut self) -> bool {
        self.inner.stop_requested()
    }
    fn on_exchange(&mut self, ex: &Exchange) {
        if self.quiet {
            return;
        }
        let resp = match (&ex.response, &ex.error) {
            (_, Some(e)) => format!("!! {e}"),
            (Some(r), None) => hexs(r),
            (None, None) => String::new(),
        };
        eprintln!(
            "  {:>7.2} s  {:<9} -> {:<22} <- {resp}",
            ex.t_ms as f64 / 1000.0,
            format!("{:?}", ex.phase).to_lowercase(),
            hexs(&ex.request)
        );
    }
}

fn print_report(r: &TestReport) {
    let outcome = match &r.outcome {
        TestOutcome::Completed => "completed".to_string(),
        TestOutcome::Stopped => "stopped early".to_string(),
        TestOutcome::Aborted { phase, message } => {
            format!("aborted at {phase:?} — nothing was driven: {message}")
        }
        TestOutcome::ApplyFailed { message } => format!("control request failed: {message}"),
        TestOutcome::HoldFailed { message } => format!("link failed during the test: {message}"),
        TestOutcome::ControlReturnFailed { message } => {
            format!("CONTROL NOT RETURNED: {message}")
        }
    };
    println!("Outcome: {outcome}");
    println!(
        "  applied: {}  held: {} ms  control returned: {}  default session: {}",
        if r.applied { "yes" } else { "no" },
        r.held_ms,
        if r.control_returned { "yes" } else { "no" },
        if r.default_session { "yes" } else { "no" }
    );
    if let Some(s) = r.status {
        println!("  status: {} {}", num(s), r.status_unit);
    }
}

/// Dispatch `adaptation actuator ...`.
pub fn run(
    ctx: &CliContext,
    def: &AdaptationDef,
    def_text: &str,
    driver: &str,
    store: Option<&std::path::Path>,
    timeout_ms: u64,
    action: ActuatorAction,
) -> Result<()> {
    match action {
        ActuatorAction::List { json } => {
            let list = actuator::actuators(def).map_err(|e| anyhow!("{e}"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(());
            }
            println!("Actuator channels in {} ({}):", def.name, def.id);
            if list.is_empty() {
                println!("  (none declared — nothing can be tested)");
            }
            for a in &list {
                println!("{}", info_line(a));
            }
            if def.requires_security {
                println!(
                    "SecurityAccess level 0x{:02X} is required first (--seedkey).",
                    actuator::security_level(def)
                );
            }
            Ok(())
        }
        ActuatorAction::Run {
            channel,
            op,
            value,
            raw,
            duration_ms,
            ack,
            seedkey,
            conn,
            json,
        } => {
            let option = IoControl::parse(&op).map_err(|e| anyhow!("--op: {e}"))?;
            let value = match (value, raw) {
                (Some(v), _) => Some(TestValue::Engineering(v)),
                (None, Some(r)) => {
                    let t = r.trim();
                    let n = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                        Some(h) => u64::from_str_radix(h, 16),
                        None => t.parse(),
                    }
                    .map_err(|_| anyhow!("--raw '{r}': expected a number (decimal or 0x hex)"))?;
                    Some(TestValue::Raw(n))
                }
                (None, None) => None,
            };
            let plan = plan_test(
                def,
                &TestRequest {
                    channel,
                    option,
                    value,
                    state: None,
                    duration_ms,
                },
                PlanOptions::default(),
            )
            .map_err(|e| anyhow!("{e}"))?;
            if !ack {
                if json {
                    let mut v = serde_json::to_value(&plan)?;
                    v["dry_run"] = serde_json::json!(true);
                    println!("{}", serde_json::to_string_pretty(&v)?);
                } else {
                    print_plan(&plan);
                    println!("Dry run: nothing was sent. Add --ack to drive the output.");
                }
                return Ok(());
            }
            execute(
                ctx,
                def_text,
                driver,
                store,
                timeout_ms,
                &plan,
                seedkey.as_deref(),
                &conn,
                json,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn execute(
    ctx: &CliContext,
    def_text: &str,
    driver: &str,
    store: Option<&std::path::Path>,
    timeout_ms: u64,
    plan: &TestPlan,
    seedkey: Option<&str>,
    conn: &ActuatorConn,
    json: bool,
) -> Result<()> {
    // The module address: flags, else the definition's; never the engine by default.
    let (tx, rx) = match (conn.tx_id.as_deref(), conn.rx_id.as_deref()) {
        (Some(t), Some(r)) => (t.to_string(), r.to_string()),
        (None, None) => match def_ids(def_text)? {
            Some((t, r)) => (format!("{t:X}"), format!("{r:X}")),
            None => bail!(
                "actuator test refused: the module's CAN address is unknown — give --tx-id/--rx-id \
                 or add req_id/resp_id to the definition (a test never defaults to the engine ECU)"
            ),
        },
        _ => bail!("give both --tx-id and --rx-id (or neither, to use the definition's)"),
    };
    let handler = if plan.requires_security {
        let id = seedkey.ok_or_else(|| {
            anyhow!(
                "this module needs SecurityAccess (level 0x{:02X}) before IO control: give \
                 --seedkey ID (resolved by the embedding build, or sa2:<hex> / sa2:@<file>); \
                 OpenTune ships no seed/key algorithm",
                plan.security_level
            )
        })?;
        Some(ctx.require_seed_key(id)?)
    } else {
        None
    };
    let mut cfg = ConnectArgs {
        driver: driver.to_string(),
        file: None,
        base: "0x0".into(),
        seedkey: None,
        port: conn.port.clone(),
        baud: conn.baud.clone(),
        bus: conn.bus.clone(),
        tx_id: Some(tx),
        rx_id: Some(rx),
        opts: conn.opts.clone(),
    }
    .config()?;
    if let Some(s) = store {
        cfg.options
            .insert("coding_store".into(), s.to_string_lossy().into_owned());
    }
    let mut drv = ctx
        .registry()
        .make(driver)
        .ok_or_else(|| anyhow!("unknown driver '{driver}'"))?;
    drv.open(&cfg).map_err(|e| anyhow!("open {driver}: {e}"))?;

    // Ctrl-C ends the hold early; control is still returned.
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        let _ = ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst));
    }
    if !json {
        print_plan(plan);
        eprintln!("Running (Ctrl-C stops early and returns control to the ECU):");
    }
    let mut clock = Live {
        inner: RealClock::new(stop),
        quiet: json,
    };
    let login = TestLogin {
        confirmed: true,
        seed_key: handler.as_deref(),
    };
    let rep = match drv.diag() {
        Some(t) => {
            let mut diag = CoreDiag::new(t, timeout_ms);
            run_test(&mut diag, plan, &login, &mut clock).map_err(|e| anyhow!("{e}"))
        }
        None => Err(anyhow!("driver '{driver}' exposes no diagnostic transport")),
    };
    let _ = drv.close();
    let rep = rep?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rep)?);
    } else {
        print_report(&rep);
    }
    if !rep.safe {
        bail!(
            "control was NOT returned to the ECU — the output may still be forced; switch the \
             ignition off and on to recover the module before driving"
        );
    }
    if !matches!(rep.outcome, TestOutcome::Completed | TestOutcome::Stopped) {
        bail!("the actuator test did not complete");
    }
    Ok(())
}
