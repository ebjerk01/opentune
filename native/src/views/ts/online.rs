//! The aftermarket-ECU panel's ONLINE side: the serial ECU (or `sim`, the
//! in-memory simulated ECU) through `opentune-tsproto`. READ-ONLY toward the
//! ECU: only `Client` methods that take no `WriteAuth` are called here
//! (signature query, page reads, realtime block). A connection is moved
//! into each background job and handed back with its result, so two
//! operations can never share the port.
//!
//! The serial port is the ECU's own, not the vehicle adapter's; connecting
//! is refused when it is the port the app's adapter link uses.

use opentune_tsini::{decode_channels, normalize_signature, ChannelKind, ChannelSample, Ini, Tune};
use opentune_tsproto::{Client, ClientOptions, Framing, Port, SerialPort, SimEcu, SimOptions};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A port a connection keeps between operations.
pub(crate) type BoxPort = Box<dyn Port + Send>;

/// Default serial speed (Speeduino, rusEFI and MS3 all use 115200).
pub(crate) const DEFAULT_BAUD: u32 = 115_200;

/// An open ECU connection, checked against `ini`.
pub(crate) struct Conn {
    port: Option<BoxPort>,
    pub label: String,
    pub sim: bool,
    framing: Option<Framing>,
    since: Instant,
    pub ini: Arc<Ini>,
}

/// What a connect found.
#[derive(Clone, Debug)]
pub(crate) struct ConnectInfo {
    pub label: String,
    pub envelope: bool,
    pub ecu_signature: String,
    pub ini_signature: Option<String>,
    pub matches: bool,
}

impl Conn {
    fn opts(&self) -> ClientOptions {
        ClientOptions {
            framing: self.framing,
            retries: 2,
            honor_delays: !self.sim,
            ..ClientOptions::default()
        }
    }

    /// Run `f` with a protocol client over the port, then keep the port
    /// (also after an error).
    fn with_client<R>(
        &mut self,
        f: impl FnOnce(&mut Client<'_, BoxPort>) -> Result<R, String>,
    ) -> Result<R, String> {
        let port = self.port.take().ok_or("the connection is closed")?;
        let ini = self.ini.clone();
        let mut client = Client::new(port, &ini, self.opts());
        let r = f(&mut client);
        self.port = Some(client.into_port());
        r
    }

    /// Seconds since the connection opened.
    pub(crate) fn uptime(&self) -> Duration {
        self.since.elapsed()
    }
}

/// The framing names the panel offers (`auto` = from the INI).
pub(crate) fn parse_framing(f: &str) -> Result<Option<Framing>, String> {
    match f.trim() {
        "" | "auto" => Ok(None),
        "envelope" => Ok(Some(Framing::Envelope)),
        "plain" => Ok(Some(Framing::Plain)),
        o => Err(format!("unknown framing '{o}' (auto, envelope or plain)")),
    }
}

/// Check the signature over a freshly opened `port` and keep it. A mismatch
/// (unless `force`) drops the port: nothing is kept open.
pub(crate) fn attach(
    ini: Arc<Ini>,
    port: BoxPort,
    label: String,
    sim: bool,
    framing: Option<Framing>,
    force: bool,
) -> Result<(Conn, ConnectInfo), String> {
    let mut conn = Conn {
        port: Some(port),
        label,
        sim,
        framing,
        since: Instant::now(),
        ini,
    };
    let (used, check) = conn.with_client(|c| {
        let used = c.framing();
        c.check_signature(force)
            .map(|s| (used, s))
            .map_err(|e| e.to_string())
    })?;
    let info = ConnectInfo {
        label: conn.label.clone(),
        envelope: used == Framing::Envelope,
        ecu_signature: check.ecu,
        ini_signature: check
            .ini
            .as_deref()
            .map(|s| normalize_signature(s).to_string()),
        matches: check.matches,
    };
    Ok((conn, info))
}

/// Open `port` (`sim` = the simulated ECU, holding `working`) and check the
/// ECU's signature against the INI's. Blocking: call it from a job.
pub(crate) fn connect(
    ini: Arc<Ini>,
    working: Option<Tune>,
    port: &str,
    baud: u32,
    framing: Option<Framing>,
    force: bool,
) -> Result<(Conn, ConnectInfo), String> {
    let port = port.trim();
    if port.is_empty() {
        return Err("choose a serial port (or 'sim')".into());
    }
    if port.eq_ignore_ascii_case("sim") {
        let mut ecu = SimEcu::new(&ini, SimOptions::default());
        if let Some(t) = &working {
            ecu.load_tune(t);
        }
        let label = "sim (simulated ECU, no hardware)".to_string();
        return attach(ini, Box::new(ecu), label, true, framing, force);
    }
    let baud = if baud == 0 { DEFAULT_BAUD } else { baud };
    let settle = Duration::from_millis(ini.comms.delay_after_port_open_ms.unwrap_or(0));
    let p = SerialPort::open(port, baud, settle).map_err(|e| format!("open {port}: {e}"))?;
    attach(
        ini,
        Box::new(p),
        format!("{port} @ {baud} baud"),
        false,
        framing,
        force,
    )
}

/// Read every page of the ECU's tune (page-read commands only).
pub(crate) fn read_tune(conn: &mut Conn) -> Result<Tune, String> {
    conn.with_client(|c| c.read_tune(&mut |_, _, _| {}).map_err(|e| e.to_string()))
}

/// Read the realtime block and decode `wanted` (every non-array channel
/// when empty), with `tune` resolving the constants expressions use.
pub(crate) fn poll(
    conn: &mut Conn,
    tune: Option<&Tune>,
    wanted: &[String],
) -> Result<Vec<ChannelSample>, String> {
    let ini = conn.ini.clone();
    let t_s = conn.uptime().as_secs_f64();
    let wanted: Vec<String> = if wanted.is_empty() {
        ini.output_channels
            .iter()
            .filter(|c| !matches!(c.kind, ChannelKind::Array { .. }))
            .map(|c| c.name.clone())
            .collect()
    } else {
        wanted.to_vec()
    };
    let block = conn.with_client(|c| c.read_realtime().map_err(|e| e.to_string()))?;
    let vars = |n: &str| {
        if n == "timeNow" {
            return Some(t_s);
        }
        tune.and_then(|t| t.var(&ini, n))
    };
    Ok(decode_channels(&ini, &block, &vars, Some(&wanted)))
}
