//! `opentune ts …`: TunerStudio-compatible aftermarket ECUs (Speeduino,
//! rusEFI, MegaSquirt) through their INI definitions — inspect INIs and
//! `.msq` tunes offline, read/write/log over a serial port
//! (`opentune-tsini`, `opentune-tsproto`; see docs/AFTERMARKET-ECUS.md).
//!
//! `--port sim` runs against the built-in simulated ECU (no hardware).
use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use opentune_tsini::{
    decode_channels, load_ini, normalize_signature, ChannelKind, Constant, Ini, IniOptions, Kind,
    Msq, MsqLoadOptions, MsqMeta, Tune, Value, WarnKind,
};
use opentune_tsproto::{
    plan_write, write_tune, Client, ClientOptions, Framing, Port, SerialPort, SimEcu, SimOptions,
    WriteAuth, WriteOptions, WriteStage,
};
use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// `#if` conditions to apply to an INI.
#[derive(Args, Debug, Clone, Default)]
pub struct CondFlags {
    /// Set an INI condition (`#if NAME`), e.g. `--set LAMBDA`. Repeatable.
    #[arg(long = "set", value_name = "NAME")]
    pub set: Vec<String>,
    /// Unset a condition the INI (or the tune's settings) would set. Repeatable.
    #[arg(long = "unset", value_name = "NAME")]
    pub unset: Vec<String>,
}

/// The ECU definition to use.
#[derive(Args, Debug, Clone)]
pub struct IniFlags {
    /// TunerStudio INI definition matching the ECU firmware.
    #[arg(long)]
    pub ini: PathBuf,
    #[command(flatten)]
    pub cond: CondFlags,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramingArg {
    /// As the INI says (`messageEnvelopeFormat`).
    Auto,
    /// CRC32 msEnvelope framing.
    Envelope,
    /// Plain bytes (legacy / compatibility mode).
    Plain,
}

/// How to reach the ECU.
#[derive(Args, Debug, Clone)]
pub struct TsConn {
    /// Serial port (`/dev/ttyACM0`, `COM3`), or `sim` for the built-in
    /// simulated ECU (no hardware).
    #[arg(long)]
    pub port: String,
    /// Baud rate (Speeduino and rusEFI USB: 115200).
    #[arg(long, default_value_t = 115_200)]
    pub baud: u32,
    #[command(flatten)]
    pub ini: IniFlags,
    /// Framing override.
    #[arg(long, value_enum, default_value_t = FramingArg::Auto)]
    pub framing: FramingArg,
    /// Value of `$tsCanId` in commands.
    #[arg(long, default_value_t = 0)]
    pub can_id: u8,
    /// Continue although the ECU's (or tune's) signature differs from the INI's.
    #[arg(long)]
    pub force_signature: bool,
    /// Response timeout in ms (default: the INI's blockReadTimeout).
    #[arg(long)]
    pub timeout_ms: Option<u64>,
    /// With `--port sim`: preload this .msq into the simulated ECU.
    #[arg(long)]
    pub sim_tune: Option<PathBuf>,
}

/// `opentune ts …` subcommands.
#[derive(Subcommand, Debug)]
pub enum TsAction {
    /// Inspect a TunerStudio INI definition (offline).
    Ini {
        #[command(subcommand)]
        action: IniAction,
    },
    /// Show and compare .msq tunes through an INI (offline).
    Msq {
        #[command(subcommand)]
        action: MsqAction,
    },
    /// Read every page from the ECU (read-only) and optionally save a .msq.
    Read {
        #[command(flatten)]
        conn: TsConn,
        /// Save the tune here (.msq).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Write a .msq to the ECU. Without --confirm this is a dry run that
    /// lists what would change. With --confirm: the current tune is read and
    /// saved as a backup first, only changed bytes are written, every page is
    /// read back and verified, and only then burned.
    Write {
        #[command(flatten)]
        conn: TsConn,
        /// The tune to write (.msq).
        tune: PathBuf,
        /// Actually write. These ECUs run engines: wrong values can damage one.
        #[arg(long)]
        confirm: bool,
        /// Write and verify but do not burn (lost at power-off).
        #[arg(long)]
        no_burn: bool,
        /// Where to save the pre-write backup (default next to the tune).
        #[arg(long)]
        backup: Option<PathBuf>,
        /// Write even if some tune entries could not be applied.
        #[arg(long)]
        allow_partial: bool,
    },
    /// Log realtime output channels to a CSV in the Live-log format
    /// (`ts,param_id,name,raw,value,units,note`) the log viewer opens.
    Log {
        #[command(flatten)]
        conn: TsConn,
        /// Channels to log (comma-separated; default: every channel).
        #[arg(long, value_delimiter = ',')]
        channels: Vec<String>,
        /// Output CSV.
        #[arg(long)]
        out: PathBuf,
        /// Number of samples.
        #[arg(long, default_value_t = 100)]
        rounds: u32,
        /// Pause between samples, milliseconds.
        #[arg(long, default_value_t = 100)]
        interval_ms: u64,
    },
}

#[derive(Subcommand, Debug)]
pub enum IniAction {
    /// Signature, pages, commands, framing and counts.
    Info {
        /// The INI file.
        file: PathBuf,
        #[command(flatten)]
        cond: CondFlags,
        /// List every warning.
        #[arg(long)]
        warnings: bool,
        /// Print the whole parsed definition as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List the table and curve editors with their axes.
    Tables {
        /// The INI file.
        file: PathBuf,
        #[command(flatten)]
        cond: CondFlags,
    },
}

#[derive(Subcommand, Debug)]
pub enum MsqAction {
    /// Print a tune's values through the INI.
    Show {
        /// The tune (.msq).
        msq: PathBuf,
        #[command(flatten)]
        ini: IniFlags,
        /// Print only these constants, arrays in full. Repeatable.
        #[arg(long = "name")]
        names: Vec<String>,
        /// Load although the tune's signature differs from the INI's.
        #[arg(long)]
        force_signature: bool,
    },
    /// Compare two tunes constant by constant.
    Diff {
        a: PathBuf,
        b: PathBuf,
        #[command(flatten)]
        ini: IniFlags,
        /// Load although a signature differs from the INI's.
        #[arg(long)]
        force_signature: bool,
    },
}

/// Run one `ts` subcommand.
pub fn run(action: TsAction) -> Result<()> {
    match action {
        TsAction::Ini { action } => match action {
            IniAction::Info {
                file,
                cond,
                warnings,
                json,
            } => ini_info(&file, &cond, warnings, json),
            IniAction::Tables { file, cond } => ini_tables(&file, &cond),
        },
        TsAction::Msq { action } => match action {
            MsqAction::Show {
                msq,
                ini,
                names,
                force_signature,
            } => msq_show(&msq, &ini, &names, force_signature),
            MsqAction::Diff {
                a,
                b,
                ini,
                force_signature,
            } => msq_diff(&a, &b, &ini, force_signature),
        },
        TsAction::Read { conn, out } => read(&conn, out.as_deref()),
        TsAction::Write {
            conn,
            tune,
            confirm,
            no_burn,
            backup,
            allow_partial,
        } => write(&conn, &tune, confirm, no_burn, backup, allow_partial),
        TsAction::Log {
            conn,
            channels,
            out,
            rounds,
            interval_ms,
        } => log(&conn, &channels, &out, rounds, interval_ms),
    }
}

// ------------------------------------------------------------------ helpers

fn options(cond: &CondFlags, extra: BTreeSet<String>) -> IniOptions {
    let mut o = IniOptions {
        set: extra,
        unset: cond.unset.iter().cloned().collect(),
    };
    o.set.extend(cond.set.iter().cloned());
    for u in &o.unset.clone() {
        o.set.remove(u);
    }
    o
}

fn load(path: &Path, opts: &IniOptions) -> Result<Ini> {
    let ini = load_ini(path, opts).with_context(|| format!("load INI {}", path.display()))?;
    let invalid = ini.warnings_of(WarnKind::Invalid).count();
    if invalid > 0 {
        eprintln!(
            "note: {invalid} INI line(s) could not be parsed (see `opentune ts ini info --warnings`)"
        );
    }
    Ok(ini)
}

fn load_msq(path: &Path) -> Result<Msq> {
    Msq::load(path).with_context(|| format!("read tune {}", path.display()))
}

/// The INI with the tune's saved settings (plus the user's flags) applied.
fn ini_for_msq(flags: &IniFlags, msq: &Msq) -> Result<Ini> {
    load(&flags.ini, &options(&flags.cond, msq.settings_conditions()))
}

fn fmt_num(v: f64, digits: Option<usize>) -> String {
    match digits {
        Some(d) => format!("{v:.d$}", d = d.min(9)),
        None => format!("{v}"),
    }
}

fn fmt_value(c: &Constant, v: &Value) -> String {
    let units = if c.units.is_empty() {
        String::new()
    } else {
        format!(" {}", c.units)
    };
    match v {
        Value::Scalar(x) => format!("{}{units}", fmt_num(*x, c.digits_lit())),
        Value::Bits(i) => format!("{i} \"{}\"", c.bit_label(*i as usize)),
        Value::Text(t) => format!("{t:?}"),
        Value::Array(a) => {
            let (cols, rows) = match c.kind {
                Kind::Array { cols, rows } => (cols, rows),
                _ => (a.len(), 1),
            };
            let lo = a.iter().copied().fold(f64::INFINITY, f64::min);
            let hi = a.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            format!(
                "[{cols}x{rows}] {} .. {}{units}",
                fmt_num(lo, c.digits_lit()),
                fmt_num(hi, c.digits_lit())
            )
        }
    }
}

fn print_grid(c: &Constant, v: &Value) {
    if let (Kind::Array { cols, .. }, Value::Array(a)) = (&c.kind, v) {
        for row in a.chunks((*cols).max(1)) {
            let cells: Vec<String> = row.iter().map(|x| fmt_num(*x, c.digits_lit())).collect();
            println!("    {}", cells.join(" "));
        }
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// `YYYY-MM-DD HH:MM:SS UTC` without a date library.
fn utc_now() -> String {
    let secs = (now_ms() / 1000) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (proleptic Gregorian).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn meta(comment: &str) -> MsqMeta {
    MsqMeta {
        author: format!("OpenTune {}", env!("CARGO_PKG_VERSION")),
        comment: comment.into(),
        write_date: utc_now(),
        firmware_info: String::new(),
    }
}

fn save_msq(path: &Path, ini: &Ini, tune: &Tune, comment: &str) -> Result<usize> {
    let (m, skipped) = Msq::from_tune(ini, tune, &meta(comment));
    for (n, why) in &skipped {
        eprintln!("note: '{n}' not saved: {why}");
    }
    std::fs::write(path, m.to_xml()).with_context(|| format!("write {}", path.display()))?;
    Ok(m.entries().count())
}

// ------------------------------------------------------------------ ini

fn cmd_text(t: &Option<opentune_tsini::CommandTemplate>) -> String {
    t.as_ref()
        .map_or_else(|| "-".to_owned(), |t| format!("{:?}", t.raw))
}

fn ini_info(file: &Path, cond: &CondFlags, warnings: bool, json: bool) -> Result<()> {
    let ini = load_ini(file, &options(cond, BTreeSet::new()))
        .with_context(|| format!("load INI {}", file.display()))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&ini)?);
        return Ok(());
    }
    let c = &ini.comms;
    println!("INI          {}", file.display());
    println!(
        "signature    {:?}{}",
        ini.signature.as_deref().unwrap_or("-"),
        ini.ini_spec_version
            .as_ref()
            .map(|v| format!("  (iniSpecVersion {v})"))
            .unwrap_or_default()
    );
    println!("endianness   {:?}", ini.endianness);
    println!(
        "framing      {}",
        if c.uses_envelope() {
            format!(
                "{} (CRC32 envelope)",
                c.message_envelope.as_deref().unwrap_or("")
            )
        } else {
            "plain (no envelope)".to_owned()
        }
    );
    let sizes: Vec<String> = ini
        .pages
        .iter()
        .map(|p| format!("{}:{}", p.index + 1, p.size))
        .collect();
    println!(
        "pages        {} ({} bytes)  {}",
        ini.pages.len(),
        ini.total_page_bytes(),
        sizes.join(" ")
    );
    let opt = |v: Option<u64>| v.map_or("-".to_owned(), |x| format!("{x} ms"));
    println!(
        "comms        blockingFactor {}  ochBlockSize {}  blockReadTimeout {}  pageActivationDelay {}  interWriteDelay {}  delayAfterPortOpen {}",
        c.blocking_factor.map_or("-".into(), |v| v.to_string()),
        c.och_block_size.map_or("-".into(), |v| v.to_string()),
        opt(c.block_read_timeout_ms),
        opt(c.page_activation_delay_ms),
        opt(c.inter_write_delay_ms),
        opt(c.delay_after_port_open_ms),
    );
    if let Some(p) = ini.pages.first() {
        println!(
            "commands     query {}  read {}  write {}  burn {}  crc32 {}  realtime {}",
            cmd_text(&c.query_command),
            cmd_text(&p.read),
            cmd_text(
                p.chunk_write
                    .as_ref()
                    .map_or(&p.value_write, |_| &p.chunk_write)
            ),
            cmd_text(&p.burn),
            cmd_text(&p.crc32),
            cmd_text(&c.och_get_command),
        );
    }
    let exprs = ini
        .output_channels
        .iter()
        .filter(|c| matches!(c.kind, ChannelKind::Expr { .. }))
        .count();
    println!(
        "contents     {} constants, {} PC variables, {} output channels ({} expressions), {} tables, {} curves, {} menus",
        ini.constants.len(),
        ini.pc_variables.len(),
        ini.output_channels.len(),
        exprs,
        ini.tables.len(),
        ini.curves.len(),
        ini.menus.len()
    );
    if !ini.setting_groups.is_empty() {
        let groups: Vec<String> = ini
            .setting_groups
            .iter()
            .map(|g| {
                if g.options.is_empty() {
                    g.name.clone()
                } else {
                    let o: Vec<&str> = g.options.iter().map(|(n, _)| n.as_str()).collect();
                    format!("{}={}", g.name, o.join("|"))
                }
            })
            .collect();
        println!("settings     {}", groups.join("  "));
    }
    let active: Vec<&str> = ini.conditions.iter().map(String::as_str).collect();
    println!(
        "conditions   {}",
        if active.is_empty() {
            "(none set)".to_owned()
        } else {
            active.join(" ")
        }
    );
    let n = |k| ini.warnings_of(k).count();
    println!(
        "warnings     {} invalid, {} reference, {} ignored{}",
        n(WarnKind::Invalid),
        n(WarnKind::Reference),
        n(WarnKind::Ignored),
        if warnings || ini.warnings.is_empty() {
            ""
        } else {
            "  (--warnings lists them)"
        }
    );
    if warnings {
        for w in &ini.warnings {
            println!(
                "  line {:>5} [{}] {:?}: {}",
                w.line, w.section, w.kind, w.message
            );
        }
    }
    Ok(())
}

fn axis_text(ini: &Ini, a: &Option<opentune_tsini::AxisRef>) -> String {
    match a {
        None => "-".into(),
        Some(a) => {
            let n = ini
                .constant(&a.constant)
                .map_or("?".into(), |c| c.elements().to_string());
            match &a.channel {
                Some(ch) => format!("{}[{n}] ({ch})", a.constant),
                None => format!("{}[{n}]", a.constant),
            }
        }
    }
}

fn ini_tables(file: &Path, cond: &CondFlags) -> Result<()> {
    let ini = load(file, &options(cond, BTreeSet::new()))?;
    println!("Tables ({}):", ini.tables.len());
    for t in &ini.tables {
        let z = t.z_bins.as_deref().unwrap_or("-");
        let shape = t
            .z_bins
            .as_deref()
            .and_then(|n| ini.constant(n))
            .map(|c| match c.kind {
                Kind::Array { cols, rows } => format!("[{cols}x{rows}] {:?} {}", c.ty, c.units),
                _ => format!("{:?}", c.ty),
            })
            .unwrap_or_else(|| "(undefined)".into());
        println!(
            "  {:<22} {:<34} page {:<3} z {z} {shape}\n  {:<22} x {}  y {}",
            t.id,
            format!("{:?}", t.title),
            t.page.map_or("-".into(), |p| (p + 1).to_string()),
            "",
            axis_text(&ini, &t.x_bins),
            axis_text(&ini, &t.y_bins),
        );
    }
    println!("Curves ({}):", ini.curves.len());
    for c in &ini.curves {
        let ys: Vec<String> = c
            .y_bins
            .iter()
            .map(|y| axis_text(&ini, &Some(y.clone())))
            .collect();
        println!(
            "  {:<22} {:<34} x {}  y {}",
            c.id,
            format!("{:?}", c.title),
            axis_text(&ini, &c.x_bins),
            ys.join(", ")
        );
    }
    Ok(())
}

// ------------------------------------------------------------------ msq

fn report_load(rep: &opentune_tsini::MsqReport) {
    for n in &rep.notes {
        eprintln!("note: {n}");
    }
    if !rep.unknown.is_empty() {
        eprintln!(
            "note: {} name(s) not in the INI: {}",
            rep.unknown.len(),
            rep.unknown.join(", ")
        );
    }
    for (n, why) in &rep.skipped {
        eprintln!("warning: '{n}' not applied: {why}");
    }
    for r in &rep.out_of_range {
        eprintln!("note: outside the INI limits: {r}");
    }
}

fn msq_tune(ini: &Ini, msq: &Msq, force: bool) -> Result<(Tune, opentune_tsini::MsqReport)> {
    Ok(msq.to_tune(
        ini,
        Tune::blank(ini),
        MsqLoadOptions {
            force_signature: force,
        },
    )?)
}

fn msq_show(path: &Path, flags: &IniFlags, names: &[String], force: bool) -> Result<()> {
    let msq = load_msq(path)?;
    let ini = ini_for_msq(flags, &msq)?;
    let (tune, rep) = msq_tune(&ini, &msq, force)?;
    println!("tune         {}", path.display());
    println!(
        "signature    {:?}  (INI {:?})",
        msq.signature().unwrap_or("-"),
        ini.signature.as_deref().unwrap_or("-")
    );
    println!(
        "applied      {} of {} entries",
        rep.applied,
        msq.entries().count()
    );
    report_load(&rep);
    let in_msq: BTreeSet<&str> = msq.entries().map(|e| e.name.as_str()).collect();
    if !names.is_empty() {
        for n in names {
            let c = ini
                .constant(n)
                .ok_or_else(|| anyhow!("'{n}' is not defined in the INI"))?;
            let v = tune.get(&ini, n)?;
            println!("{n} = {}", fmt_value(c, &v));
            print_grid(c, &v);
        }
        return Ok(());
    }
    for (pi, _) in ini.pages.iter().enumerate() {
        println!("page {}:", pi + 1);
        for c in ini
            .page_constants(pi)
            .filter(|c| in_msq.contains(c.name.as_str()))
        {
            match tune.get(&ini, &c.name) {
                Ok(v) => println!("  {} = {}", c.name, fmt_value(c, &v)),
                Err(e) => println!("  {} = ? ({e})", c.name),
            }
        }
    }
    let pcs: Vec<&Constant> = ini
        .pc_variables
        .iter()
        .filter(|c| tune.pc.contains_key(&c.name))
        .collect();
    if !pcs.is_empty() {
        println!("PC variables:");
        for c in pcs {
            println!("  {} = {}", c.name, fmt_value(c, &tune.pc[&c.name]));
        }
    }
    Ok(())
}

/// Describe how two values of one constant differ, or `None` if equal.
fn describe_change(c: &Constant, a: &Value, b: &Value) -> Option<String> {
    if a == b {
        return None;
    }
    match (a, b) {
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            let changed: Vec<f64> = x
                .iter()
                .zip(y)
                .filter(|(p, q)| p != q)
                .map(|(p, q)| (q - p).abs())
                .collect();
            let max = changed.iter().copied().fold(0.0, f64::max);
            Some(format!(
                "{} of {} cells changed (max |change| {}{})",
                changed.len(),
                x.len(),
                fmt_num(max, c.digits_lit()),
                if c.units.is_empty() {
                    String::new()
                } else {
                    format!(" {}", c.units)
                }
            ))
        }
        _ => Some(format!("{} -> {}", fmt_value(c, a), fmt_value(c, b))),
    }
}

/// Lines describing every constant whose value differs between two tunes.
fn diff_lines(ini: &Ini, a: &Tune, b: &Tune) -> Vec<String> {
    let mut out = Vec::new();
    for c in ini.constants.iter().chain(&ini.pc_variables) {
        if let (Ok(x), Ok(y)) = (a.get(ini, &c.name), b.get(ini, &c.name)) {
            if let Some(d) = describe_change(c, &x, &y) {
                out.push(format!("{}: {d}", c.name));
            }
        }
    }
    out
}

fn msq_diff(a: &Path, b: &Path, flags: &IniFlags, force: bool) -> Result<()> {
    let (ma, mb) = (load_msq(a)?, load_msq(b)?);
    let ini = ini_for_msq(flags, &ma)?;
    let (ta, ra) = msq_tune(&ini, &ma, force)?;
    let (tb, rb) = msq_tune(&ini, &mb, force)?;
    report_load(&ra);
    report_load(&rb);
    let na: BTreeSet<&str> = ma.entries().map(|e| e.name.as_str()).collect();
    let nb: BTreeSet<&str> = mb.entries().map(|e| e.name.as_str()).collect();
    for n in na.difference(&nb) {
        println!("only in {}: {n}", a.display());
    }
    for n in nb.difference(&na) {
        println!("only in {}: {n}", b.display());
    }
    let lines = diff_lines(&ini, &ta, &tb);
    for l in &lines {
        println!("{l}");
    }
    println!("{} constant(s) differ", lines.len());
    Ok(())
}

// ------------------------------------------------------------------ online

fn open_client<'i>(conn: &TsConn, ini: &'i Ini) -> Result<Client<'i, Box<dyn Port>>> {
    let sim = conn.port.eq_ignore_ascii_case("sim");
    let port: Box<dyn Port> = if sim {
        let mut s = SimEcu::new(ini, SimOptions::default());
        if let Some(p) = &conn.sim_tune {
            let m = load_msq(p)?;
            let (t, _) = msq_tune(ini, &m, true)?;
            s.load_tune(&t);
        }
        println!("port         sim (simulated ECU, no hardware)");
        Box::new(s)
    } else {
        if conn.sim_tune.is_some() {
            bail!("--sim-tune only applies to --port sim");
        }
        let settle = Duration::from_millis(ini.comms.delay_after_port_open_ms.unwrap_or(0));
        let p = SerialPort::open(&conn.port, conn.baud, settle)
            .with_context(|| format!("open {}", conn.port))?;
        println!("port         {} @ {} baud", conn.port, conn.baud);
        Box::new(p)
    };
    let framing = match conn.framing {
        FramingArg::Auto => None,
        FramingArg::Envelope => Some(Framing::Envelope),
        FramingArg::Plain => Some(Framing::Plain),
    };
    let client = Client::new(
        port,
        ini,
        ClientOptions {
            framing,
            timeout: conn.timeout_ms.map(Duration::from_millis),
            retries: 2,
            honor_delays: !sim,
            can_id: conn.can_id,
        },
    );
    println!("framing      {:?}", client.framing());
    Ok(client)
}

fn connect<'i>(conn: &TsConn, ini: &'i Ini) -> Result<Client<'i, Box<dyn Port>>> {
    let mut c = open_client(conn, ini)?;
    let sig = c.check_signature(conn.force_signature)?;
    println!(
        "signature    {:?}{}",
        sig.ecu,
        if sig.matches {
            " (matches the INI)".to_owned()
        } else {
            format!(
                " (INI says {:?}; continuing because of --force-signature)",
                sig.ini.as_deref().map(normalize_signature).unwrap_or("-")
            )
        }
    );
    Ok(c)
}

fn read_all(c: &mut Client<'_, Box<dyn Port>>, ini: &Ini) -> Result<Tune> {
    let start = Instant::now();
    let t = c.read_tune(&mut |_, _, _| {})?;
    println!(
        "read         {} page(s), {} bytes in {:.1} s",
        ini.pages.len(),
        ini.total_page_bytes(),
        start.elapsed().as_secs_f64()
    );
    Ok(t)
}

fn read(conn: &TsConn, out: Option<&Path>) -> Result<()> {
    let ini = load(&conn.ini.ini, &options(&conn.ini.cond, BTreeSet::new()))?;
    let mut c = connect(conn, &ini)?;
    let tune = read_all(&mut c, &ini)?;
    match out {
        Some(p) => {
            let n = save_msq(p, &ini, &tune, "read by OpenTune")?;
            println!("saved        {} ({n} entries)", p.display());
        }
        None => println!("(nothing saved; pass --out tune.msq to keep it)"),
    }
    Ok(())
}

fn default_backup(tune: &Path) -> PathBuf {
    let stem = tune
        .file_stem()
        .map_or("tune".into(), |s| s.to_string_lossy().into_owned());
    let name = format!("{stem}.before-write-{}.msq", now_ms() / 1000);
    tune.with_file_name(name)
}

fn write(
    conn: &TsConn,
    tune_path: &Path,
    confirm: bool,
    no_burn: bool,
    backup: Option<PathBuf>,
    allow_partial: bool,
) -> Result<()> {
    let msq = load_msq(tune_path)?;
    let ini = ini_for_msq(&conn.ini, &msq)?;
    // Refuse a tune for other firmware before touching the port.
    msq.check_signature(&ini, conn.force_signature)?;
    let mut c = connect(conn, &ini)?;
    let current = read_all(&mut c, &ini)?;
    let (target, rep) = msq.to_tune(
        &ini,
        current.clone(),
        MsqLoadOptions {
            force_signature: conn.force_signature,
        },
    )?;
    report_load(&rep);
    if !rep.skipped.is_empty() && !allow_partial {
        bail!(
            "{} tune entr(ies) could not be applied (listed above); nothing written. \
             Fix the tune or INI, or pass --allow-partial to keep the ECU's values for them",
            rep.skipped.len()
        );
    }
    let plan = plan_write(&current, &target);
    if plan.is_empty() {
        println!("the ECU already holds this tune; nothing to write");
        return Ok(());
    }
    let lines = diff_lines(&ini, &current, &target);
    println!(
        "changes      {} byte(s) in {} page(s), {} constant(s):",
        plan.changed_bytes(),
        plan.pages.len(),
        lines.len()
    );
    for l in &lines {
        println!("  {l}");
    }
    if !confirm {
        println!("dry run: nothing written. Re-run with --confirm to write (the current tune is saved first).");
        return Ok(());
    }
    let backup = backup.unwrap_or_else(|| default_backup(tune_path));
    save_msq(&backup, &ini, &current, "backup before write")?;
    // The raw pages too: lossless even for constants a .msq cannot express
    // (e.g. an unevaluable scale expression).
    let raw = backup.with_extension("pages.bin");
    std::fs::write(&raw, current.pages.concat())
        .with_context(|| format!("write {}", raw.display()))?;
    println!(
        "backup       {} (+ raw pages {})",
        backup.display(),
        raw.display()
    );
    let auth = WriteAuth::user_confirmed();
    let mut stage = None;
    let rep = write_tune(
        &mut c,
        &auth,
        &current,
        &target,
        WriteOptions {
            burn: !no_burn,
            check_crc: true,
        },
        &mut |s, _, _, _| {
            if stage != Some(s) {
                stage = Some(s);
                let what = match s {
                    WriteStage::Write => "writing",
                    WriteStage::Verify => "verifying (read-back)",
                    WriteStage::Burn => "burning",
                    WriteStage::CrcCheck => "checking page CRC32",
                };
                println!("             {what}…");
            }
        },
    )
    .context("write aborted; the backup above holds the ECU's previous tune")?;
    let pages = |v: &[usize]| {
        v.iter()
            .map(|p| (p + 1).to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    println!(
        "written      {} byte(s), verified page(s) {}",
        rep.bytes_written,
        pages(&rep.pages)
    );
    if no_burn {
        println!("not burned   (--no-burn): the change is lost when the ECU powers off");
    } else {
        println!("burned       page(s) {}", pages(&rep.burned));
        if !rep.not_burnable.is_empty() {
            println!(
                "RAM only     page(s) {} have no burn command in the INI",
                pages(&rep.not_burnable)
            );
        }
    }
    if !rep.crc_checked.is_empty() {
        println!("CRC32 ok     page(s) {}", pages(&rep.crc_checked));
    }
    Ok(())
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

fn log(
    conn: &TsConn,
    channels: &[String],
    out: &Path,
    rounds: u32,
    interval_ms: u64,
) -> Result<()> {
    let ini = load(&conn.ini.ini, &options(&conn.ini.cond, BTreeSet::new()))?;
    for ch in channels {
        if ini.output_channel(ch).is_none() {
            bail!("'{ch}' is not an output channel in the INI");
        }
    }
    let wanted: Vec<String> = if channels.is_empty() {
        ini.output_channels
            .iter()
            .filter(|c| !matches!(c.kind, ChannelKind::Array { .. }))
            .map(|c| c.name.clone())
            .collect()
    } else {
        channels.to_vec()
    };
    let mut c = connect(conn, &ini)?;
    let file = std::fs::File::create(out).with_context(|| format!("create {}", out.display()))?;
    let mut w = std::io::BufWriter::new(file);
    writeln!(w, "ts,param_id,name,raw,value,units,note")?;
    let start = Instant::now();
    let mut ok = 0;
    let mut last_ts = 0u128;
    for round in 0..rounds {
        if round > 0 && interval_ms > 0 {
            std::thread::sleep(Duration::from_millis(interval_ms));
        }
        // Rows sharing a `ts` form one sample in the Live-log format, so keep
        // timestamps strictly increasing even for sub-millisecond polls.
        let ts = now_ms().max(last_ts + 1);
        last_ts = ts;
        let block = match c.read_realtime() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("sample {round}: {e}");
                continue;
            }
        };
        let t_s = start.elapsed().as_secs_f64();
        let vars = |n: &str| (n == "timeNow").then_some(t_s);
        for s in decode_channels(&ini, &block, &vars, Some(&wanted)) {
            let num = |v: Option<f64>| v.map_or(String::new(), |x| format!("{x}"));
            writeln!(
                w,
                "{ts},{},{},{},{},{},{}",
                csv_field(&s.name),
                csv_field(&s.name),
                num(s.raw),
                num(s.value),
                csv_field(&s.units),
                csv_field(&s.note)
            )?;
        }
        ok += 1;
    }
    w.flush()?;
    println!(
        "logged       {ok}/{rounds} sample(s) of {} channel(s) to {}",
        wanted.len(),
        out.display()
    );
    if ok == 0 && rounds > 0 {
        bail!("no realtime sample could be read");
    }
    Ok(())
}
