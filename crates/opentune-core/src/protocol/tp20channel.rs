//! Transport selection for raw-CAN drivers: ISO-TP (the default) or a VW TP2.0
//! channel, chosen from [`ConnectConfig`].
//!
//! Raw-CAN drivers (socketcan, slcan, gsusb) own a [`CanLink`]. On `open` they
//! hand it to [`CanDiag::open`], which builds an [`IsoTp`] exactly as before
//! unless the config selects TP2.0, in which case it opens a [`Tp20Channel`]:
//! channel setup on `0x200`, the `A0`/`A1` parameter exchange, then a
//! background keep-alive (`A3` after an idle interval) until `close`, which
//! sends the disconnect (`A8`).
//!
//! ## Options ([`ConnectConfig::options`])
//!
//! | Key | Meaning | Default |
//! |---|---|---|
//! | `transport` | `isotp` or `tp20` (aliases `iso-tp`, `vwtp20`, `tp2.0`) | `isotp`, or `tp20` when `protocol` is `vwtp20` |
//! | `tp20_addr` | logical address of the module (`0x0A` = Haldex, `0x01` = engine, `0x02` = gearbox) | `tx_id` when `protocol` is `vwtp20`; otherwise required |
//! | `tp20_rx_id` | CAN id the module is asked to transmit on (11-bit, not `0x200..=0x2FF`) | `rx_id` when `protocol` is `vwtp20`, else `0x300` |
//! | `tp20_keepalive_ms` | send `A3` after this long idle; `0` = never | `1000` |
//! | `tp20_timeout_ms` | wait per setup / parameter / keep-alive step | `1000` |
//! | `tp20_block_size` | block size requested in our `A0` (frames the module sends before it waits for our ACK), 1..=15 | `15` |
//! | `tp20_ack_retries` | times a block whose ACK does not arrive is re-sent (after an `A4` break), 0..=5 | `2` |
//!
//! ## Breaks and re-opening
//!
//! When an exchange fails mid-way (no ACK after the retries, a lost frame the
//! module never resent, a reply timeout) or the module closes the channel
//! (`A8`), that request fails and the channel is torn down; the **next**
//! request transparently sets up a new channel ([`Tp20Channel::reopens`]
//! counts them). The module's KWP session does not survive that, so callers
//! restart it. A failed keep-alive (two unanswered `A3`s in a row) instead
//! marks the channel lost: requests fail until [`Tp20Channel::reopen`].
//!
//! A `tp20_*` option without TP2.0 selected is refused, so a missing
//! `transport=tp20` never silently falls back to ISO-TP.
use crate::protocol::diag::DiagTransport;
use crate::protocol::isotp::{CanLink, IsoTp};
use crate::protocol::vwtp20::{Tp20Stats, VwTp20, BROADCAST_ID};
use crate::{ConnectConfig, Error, Protocol, Result};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// CAN id the module is asked to transmit on when nothing else is given.
pub const DEFAULT_TESTER_RX_ID: u32 = 0x300;
/// Idle time after which a channel test (`A3`) is sent.
pub const DEFAULT_KEEPALIVE_MS: u64 = 1000;
/// Wait per channel-management step.
pub const DEFAULT_TIMEOUT_MS: u64 = 1000;
/// Block size requested in the `A0`.
pub const DEFAULT_BLOCK_SIZE: u8 = 0x0F;
/// Retransmissions of an unacknowledged block.
pub const DEFAULT_ACK_RETRIES: u8 = 2;
/// Keep-alive attempts (each bounded by the step timeout) before the
/// channel is declared lost.
const KEEPALIVE_ATTEMPTS: u32 = 2;

/// Which transport a raw-CAN driver builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanTransportKind {
    IsoTp,
    Tp20,
}

/// VW TP2.0 channel parameters, parsed from a [`ConnectConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tp20Options {
    /// Logical address of the target module.
    pub addr: u8,
    /// CAN id the module is asked to transmit on.
    pub rx_id: u32,
    /// Idle time before a keep-alive `A3`; 0 disables the keep-alive.
    pub keepalive_ms: u64,
    /// Wait per channel-management step (setup, `A0`/`A1`, `A3`/`A1`).
    pub timeout_ms: u64,
    /// Block size requested in our `A0`.
    pub block_size: u8,
    /// Retransmissions of a block whose ACK does not arrive.
    pub ack_retries: u8,
}

impl Tp20Options {
    /// Defaults for a module at `addr`.
    pub fn new(addr: u8) -> Self {
        Self {
            addr,
            rx_id: DEFAULT_TESTER_RX_ID,
            keepalive_ms: DEFAULT_KEEPALIVE_MS,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            block_size: DEFAULT_BLOCK_SIZE,
            ack_retries: DEFAULT_ACK_RETRIES,
        }
    }

    /// The transport `cfg` selects: `Ok(None)` = ISO-TP (the default),
    /// `Ok(Some(_))` = TP2.0 with these parameters. Bad or contradictory
    /// options are an [`Error::Config`], raised before any hardware is touched.
    pub fn from_config(cfg: &ConnectConfig) -> Result<Option<Self>> {
        let kind = transport_kind(cfg)?;
        if kind == CanTransportKind::IsoTp {
            if let Some(k) = cfg.options.keys().find(|k| k.starts_with("tp20_")) {
                return Err(Error::Config(format!(
                    "option '{k}' needs transport=tp20 (the connection is ISO-TP)"
                )));
            }
            return Ok(None);
        }
        let declared = cfg.protocol == Protocol::Vwtp20;
        let addr = match cfg.options.get("tp20_addr") {
            Some(s) => parse_num("tp20_addr", s, 0xFF)? as u8,
            None if declared && cfg.tx_id <= 0xFF => cfg.tx_id as u8,
            None => {
                return Err(Error::Config(
                    "transport=tp20 needs tp20_addr=<logical address of the module>, e.g. \
                     tp20_addr=0x0A for the Haldex, 0x01 for the engine, 0x02 for the gearbox"
                        .into(),
                ))
            }
        };
        let rx_id = match cfg.options.get("tp20_rx_id") {
            Some(s) => parse_num("tp20_rx_id", s, 0x7FF)? as u32,
            None if declared => cfg.rx_id,
            None => DEFAULT_TESTER_RX_ID,
        };
        if rx_id == 0 || rx_id > 0x7FF || (BROADCAST_ID..=BROADCAST_ID + 0xFF).contains(&rx_id) {
            return Err(Error::Config(format!(
                "TP2.0 receive id 0x{rx_id:X} is not usable: it must be an 11-bit id outside \
                 the channel-setup range 0x200..=0x2FF (default 0x300)"
            )));
        }
        let mut o = Self::new(addr);
        o.rx_id = rx_id;
        if let Some(s) = cfg.options.get("tp20_keepalive_ms") {
            o.keepalive_ms = parse_num("tp20_keepalive_ms", s, 60_000)?;
        }
        if let Some(s) = cfg.options.get("tp20_timeout_ms") {
            o.timeout_ms = parse_num("tp20_timeout_ms", s, 60_000)?;
            if o.timeout_ms == 0 {
                return Err(Error::Config("tp20_timeout_ms must be > 0".into()));
            }
        }
        if let Some(s) = cfg.options.get("tp20_block_size") {
            o.block_size = parse_num("tp20_block_size", s, 15)? as u8;
            if o.block_size == 0 {
                return Err(Error::Config("tp20_block_size must be 1..=15".into()));
            }
        }
        if let Some(s) = cfg.options.get("tp20_ack_retries") {
            o.ack_retries = parse_num("tp20_ack_retries", s, 5)? as u8;
        }
        Ok(Some(o))
    }

    /// One-line description, e.g. `VW TP2.0 to 0x0A (module transmits on 0x300)`.
    pub fn describe(&self) -> String {
        format!(
            "VW TP2.0 to 0x{:02X} (module transmits on 0x{:03X})",
            self.addr, self.rx_id
        )
    }
}

/// The transport `cfg` selects (see the module docs).
pub fn transport_kind(cfg: &ConnectConfig) -> Result<CanTransportKind> {
    match cfg
        .options
        .get("transport")
        .map(|s| s.trim().to_ascii_lowercase())
    {
        None => Ok(if cfg.protocol == Protocol::Vwtp20 {
            CanTransportKind::Tp20
        } else {
            CanTransportKind::IsoTp
        }),
        Some(s) => match s.as_str() {
            "" | "isotp" | "iso-tp" | "iso15765" => Ok(CanTransportKind::IsoTp),
            "tp20" | "vwtp20" | "tp2.0" | "vwtp" => Ok(CanTransportKind::Tp20),
            other => Err(Error::Config(format!(
                "unknown transport '{other}' (expected isotp or tp20)"
            ))),
        },
    }
}

/// Decimal or `0x`-hex, at most `max`.
fn parse_num(key: &str, s: &str, max: u64) -> Result<u64> {
    let t = s.trim();
    let v = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => t.parse().ok(),
    };
    match v {
        Some(v) if v <= max => Ok(v),
        _ => Err(Error::Config(format!(
            "{key}='{s}': expected a number 0..={max} (decimal or 0x-hex)"
        ))),
    }
}

type Link = dyn CanLink + Send;

struct Inner {
    tp: VwTp20<Link>,
    last_activity: Instant,
    /// Why the channel is gone (keep-alive failed, module disconnected).
    lost: Option<String>,
    keep_alives: u64,
}

struct Shared {
    inner: Mutex<Inner>,
    stop: Mutex<bool>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// An open VW TP2.0 channel with a background keep-alive.
///
/// Requests are serialised with the keep-alive through one lock, so an `A3`
/// never lands in the middle of a telegram. If keep-alives go unanswered
/// the channel is marked lost and requests fail with the reason until
/// [`reopen`](Self::reopen) (the module's KWP session is gone with it). A
/// break during a request (see the module docs) instead re-opens on the next
/// request. Dropping the channel stops the keep-alive and sends `A8`.
pub struct Tp20Channel {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    opts: Tp20Options,
    tester_tx_id: u32,
}

impl Tp20Channel {
    /// Set up the channel over `link` (setup + parameter exchange, each step
    /// bounded by `opts.timeout_ms`) and start the keep-alive. Fails with an
    /// error naming the step and CAN ids when the module does not answer.
    pub fn open(link: Box<Link>, opts: &Tp20Options) -> Result<Self> {
        let mut tp: VwTp20<Link> = VwTp20::new(link, opts.addr, opts.rx_id);
        tp.block_size = opts.block_size;
        tp.max_block_retries = opts.ack_retries;
        tp.open(opts.timeout_ms)?;
        let tester_tx_id = tp.tx_id;
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                tp,
                last_activity: Instant::now(),
                lost: None,
                keep_alives: 0,
            }),
            stop: Mutex::new(false),
            wake: Condvar::new(),
        });
        let mut ch = Self {
            shared,
            worker: None,
            opts: opts.clone(),
            tester_tx_id,
        };
        ch.start_keep_alive()?;
        Ok(ch)
    }

    fn start_keep_alive(&mut self) -> Result<()> {
        if self.opts.keepalive_ms == 0 {
            return Ok(());
        }
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        let s = self.shared.clone();
        let (interval, timeout) = (
            Duration::from_millis(self.opts.keepalive_ms),
            self.opts.timeout_ms,
        );
        self.worker = Some(
            std::thread::Builder::new()
                .name("tp20-keepalive".into())
                .spawn(move || keep_alive_loop(&s, interval, timeout))
                .map_err(Error::Io)?,
        );
        Ok(())
    }

    /// Set up a fresh channel after it was lost or closed (a no-op on an
    /// open, healthy channel) and restart the keep-alive. The module's KWP
    /// session starts over.
    pub fn reopen(&mut self) -> Result<()> {
        {
            let mut g = self.shared.lock();
            if g.lost.is_none() && g.tp.is_open() {
                return Ok(());
            }
            g.tp.disconnect();
            g.tp.open(self.opts.timeout_ms)?;
            g.lost = None;
            g.last_activity = Instant::now();
            self.tester_tx_id = g.tp.tx_id;
        }
        self.start_keep_alive()
    }

    /// Channel set-ups after the first (automatic or via [`reopen`](Self::reopen)).
    pub fn reopens(&self) -> u32 {
        self.shared.lock().tp.stats.reopens
    }

    /// The transport's recovery counters.
    pub fn stats(&self) -> Tp20Stats {
        self.shared.lock().tp.stats.clone()
    }

    /// The parameters this channel was opened with.
    pub fn options(&self) -> &Tp20Options {
        &self.opts
    }

    /// The CAN id the module assigned the tester to transmit on.
    pub fn tester_tx_id(&self) -> u32 {
        self.tester_tx_id
    }

    /// Keep-alive (`A3`) round trips completed so far.
    pub fn keep_alives(&self) -> u64 {
        self.shared.lock().keep_alives
    }

    /// Whether the channel is still usable.
    pub fn is_open(&self) -> bool {
        let g = self.shared.lock();
        g.lost.is_none() && g.tp.is_open()
    }

    /// One-line description including the assigned ids.
    pub fn describe(&self) -> String {
        format!(
            "VW TP2.0 to 0x{:02X} (tester 0x{:03X} -> module, module 0x{:03X} -> tester)",
            self.opts.addr, self.tester_tx_id, self.opts.rx_id
        )
    }

    /// Stop the keep-alive and send the disconnect (`A8`). Idempotent.
    pub fn close(&mut self) {
        {
            let mut stop = self.shared.stop.lock().unwrap_or_else(|e| e.into_inner());
            *stop = true;
        }
        self.shared.wake.notify_all();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        self.shared.lock().tp.disconnect();
    }
}

fn keep_alive_loop(shared: &Shared, interval: Duration, timeout_ms: u64) {
    loop {
        // Sleep until the channel has been idle for `interval` (or close).
        let wait = {
            let g = shared.lock();
            interval.saturating_sub(g.last_activity.elapsed())
        };
        {
            let stop = shared.stop.lock().unwrap_or_else(|e| e.into_inner());
            let (stop, _) = shared
                .wake
                .wait_timeout_while(stop, wait.max(Duration::from_millis(1)), |s| !*s)
                .unwrap_or_else(|e| e.into_inner());
            if *stop {
                return;
            }
        }
        let mut g = shared.lock();
        if g.lost.is_some() {
            return;
        }
        if !g.tp.is_open() {
            // Broken during a request: the next request re-opens it.
            g.last_activity = Instant::now();
            continue;
        }
        if g.last_activity.elapsed() < interval {
            continue; // a request ran meanwhile
        }
        let mut result = Ok(());
        for _ in 0..KEEPALIVE_ATTEMPTS {
            result = g.tp.keep_alive(timeout_ms);
            // A closed channel is final; a lost A3 or A1 is worth a retry.
            if result.is_ok() || !g.tp.is_open() {
                break;
            }
        }
        match result {
            Ok(()) => {
                g.keep_alives += 1;
                g.last_activity = Instant::now();
            }
            Err(e) => {
                g.lost = Some(format!("keep-alive failed: {e}"));
                g.tp.disconnect();
                return;
            }
        }
    }
}

impl Drop for Tp20Channel {
    fn drop(&mut self) {
        self.close();
    }
}

impl DiagTransport for Tp20Channel {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        let mut g = self.shared.lock();
        if let Some(why) = &g.lost {
            return Err(Error::Protocol(format!(
                "TP2.0 channel to 0x{:02X} is closed ({why}); re-open it (reopen) or the \
                 adapter",
                self.opts.addr
            )));
        }
        // A channel broken by an earlier request is set up again here
        // (`VwTp20::request` opens lazily), each step bounded by the timeout.
        let r = g.tp.request(req, timeout_ms);
        g.last_activity = Instant::now();
        let tx = g.tp.tx_id;
        drop(g);
        self.tester_tx_id = tx;
        r
    }
}

/// The diagnostic transport a raw-CAN driver built on `open`.
pub enum CanDiag {
    /// ISO 15765-2 (the default).
    IsoTp(IsoTp),
    /// A VW TP2.0 channel.
    Tp20(Tp20Channel),
}

impl CanDiag {
    /// Build the transport `cfg` selects over `link`: an [`IsoTp`] on
    /// `cfg.tx_id`/`cfg.rx_id` by default, or an opened [`Tp20Channel`].
    pub fn open(link: Box<Link>, cfg: &ConnectConfig) -> Result<Self> {
        match Tp20Options::from_config(cfg)? {
            None => Ok(CanDiag::IsoTp(IsoTp::new(link, cfg.tx_id, cfg.rx_id))),
            Some(o) => Ok(CanDiag::Tp20(Tp20Channel::open(link, &o)?)),
        }
    }

    pub fn kind(&self) -> CanTransportKind {
        match self {
            CanDiag::IsoTp(_) => CanTransportKind::IsoTp,
            CanDiag::Tp20(_) => CanTransportKind::Tp20,
        }
    }

    /// One-line description of the transport.
    pub fn describe(&self) -> String {
        match self {
            CanDiag::IsoTp(t) => format!("ISO-TP 0x{:03X}/0x{:03X}", t.tx_id, t.rx_id),
            CanDiag::Tp20(c) => c.describe(),
        }
    }

    /// The ISO-TP transport, or a clear refusal when this is a TP2.0
    /// channel (which carries KWP2000, not the UDS block flows). `driver`
    /// names the caller in the message.
    pub fn isotp(&mut self, driver: &str) -> Result<&mut IsoTp> {
        match self {
            CanDiag::IsoTp(t) => Ok(t),
            CanDiag::Tp20(c) => Err(Error::Unsupported(format!(
                "{driver}: this connection is a {} carrying KWP2000; identification and \
                 block read/write here are UDS over ISO-TP. Use the KWP2000 engines through \
                 the diagnostic transport (e.g. `opentune haldex gen4`), or re-open with \
                 transport=isotp",
                c.describe()
            ))),
        }
    }

    /// Tear down: a TP2.0 channel stops its keep-alive and sends `A8`.
    pub fn close(&mut self) {
        if let CanDiag::Tp20(c) = self {
            c.close();
        }
    }
}

impl DiagTransport for CanDiag {
    fn request(&mut self, req: &[u8], timeout_ms: u64) -> Result<Vec<u8>> {
        match self {
            CanDiag::IsoTp(t) => t.request(req, timeout_ms),
            CanDiag::Tp20(c) => c.request(req, timeout_ms),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(opts: &[(&str, &str)]) -> ConnectConfig {
        let mut c = ConnectConfig::default();
        for (k, v) in opts {
            c.options.insert((*k).into(), (*v).into());
        }
        c
    }

    #[test]
    fn default_stays_isotp() {
        assert_eq!(Tp20Options::from_config(&cfg(&[])).unwrap(), None);
        assert_eq!(
            Tp20Options::from_config(&cfg(&[("transport", "isotp")])).unwrap(),
            None
        );
    }

    #[test]
    fn tp20_options_parse() {
        let o = Tp20Options::from_config(&cfg(&[
            ("transport", "TP20"),
            ("tp20_addr", "0x0A"),
            ("tp20_keepalive_ms", "500"),
            ("tp20_block_size", "3"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(o.addr, 0x0A);
        assert_eq!(o.rx_id, DEFAULT_TESTER_RX_ID);
        assert_eq!(o.keepalive_ms, 500);
        assert_eq!(o.block_size, 3);
        assert_eq!(o.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert!(o.describe().contains("0x0A"));
    }

    #[test]
    fn protocol_vwtp20_uses_tx_rx_ids() {
        let c = ConnectConfig {
            protocol: Protocol::Vwtp20,
            tx_id: 0x0A,
            rx_id: 0x310,
            ..ConnectConfig::default()
        };
        let o = Tp20Options::from_config(&c).unwrap().unwrap();
        assert_eq!((o.addr, o.rx_id), (0x0A, 0x310));
        // An explicit transport=isotp wins over the protocol.
        let mut c2 = c.clone();
        c2.options.insert("transport".into(), "isotp".into());
        assert_eq!(Tp20Options::from_config(&c2).unwrap(), None);
    }

    #[test]
    fn bad_options_are_config_errors() {
        let bad: &[&[(&str, &str)]] = &[
            &[("transport", "kline")],
            &[("transport", "tp20")], // no address
            &[("transport", "tp20"), ("tp20_addr", "0x100")],
            &[
                ("transport", "tp20"),
                ("tp20_addr", "1"),
                ("tp20_rx_id", "0x20A"),
            ],
            &[
                ("transport", "tp20"),
                ("tp20_addr", "1"),
                ("tp20_rx_id", "0x800"),
            ],
            &[
                ("transport", "tp20"),
                ("tp20_addr", "1"),
                ("tp20_block_size", "0"),
            ],
            &[
                ("transport", "tp20"),
                ("tp20_addr", "1"),
                ("tp20_timeout_ms", "0"),
            ],
            &[("tp20_addr", "0x0A")], // tp20 option on an ISO-TP connection
        ];
        for opts in bad {
            assert!(
                matches!(Tp20Options::from_config(&cfg(opts)), Err(Error::Config(_))),
                "{opts:?}"
            );
        }
    }
}
