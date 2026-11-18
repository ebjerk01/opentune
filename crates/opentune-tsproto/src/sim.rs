//! [`SimEcu`]: an in-memory ECU that answers the commands its INI defines.
//!
//! It is generic: requests are matched against the INI's own command
//! templates (query, page read/write/burn/CRC, realtime), so the same
//! simulator plays a Speeduino-, rusEFI- or MegaSquirt-style ECU depending on
//! the definition. It keeps working memory (`ram`) and burned memory
//! (`flash`) per page, logs every request ([`SimEvent`]) and can inject
//! transfer faults ([`Fault`]) to exercise the client's error handling.
use crate::client::Framing;
use crate::crc::crc32;
use crate::framing::{
    self, STATUS_BURN_OK, STATUS_CRC_FAILURE, STATUS_OK, STATUS_OUT_OF_RANGE, STATUS_UNDERRUN,
    STATUS_UNRECOGNIZED,
};
use crate::port::Port;
use opentune_tsini::{CommandTemplate, Ini, RenderCtx, Tune};
use std::collections::VecDeque;
use std::io;
use std::time::Duration;

/// How the simulator behaves.
#[derive(Debug, Clone)]
pub struct SimOptions {
    /// Framing; default from the INI.
    pub framing: Option<Framing>,
    /// Signature to report; default the INI's.
    pub signature: Option<String>,
    /// Append a NUL to the signature (rusEFI does).
    pub signature_nul: bool,
    /// Status for a successful burn (Speeduino and rusEFI send 0x04).
    pub burn_status: u8,
    /// `$tsCanId` this ECU answers to.
    pub can_id: u8,
    /// Fixed realtime block; default a counter pattern.
    pub realtime: Option<Vec<u8>>,
}

impl Default for SimOptions {
    fn default() -> Self {
        Self {
            framing: None,
            signature: None,
            signature_nul: false,
            burn_status: STATUS_BURN_OK,
            can_id: 0,
            realtime: None,
        }
    }
}

/// A fault applied to the next request or response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// Flip a bit in the next response's CRC (envelope) or data (plain).
    CorruptResponse,
    /// Send only the first `n` bytes of the next response.
    TruncateResponse(usize),
    /// Send nothing for the next request.
    DropResponse,
    /// Answer the next request with this error status (envelope only).
    Status(u8),
    /// Acknowledge the next write but do not apply it.
    IgnoreWrite,
    /// Apply the next write with its first byte's lowest bit flipped.
    CorruptWrite,
}

/// What the simulator saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimEvent {
    Query,
    Read {
        page: usize,
        offset: usize,
        len: usize,
    },
    Write {
        page: usize,
        offset: usize,
        len: usize,
    },
    Burn {
        page: usize,
    },
    Crc {
        page: usize,
    },
    Realtime {
        offset: usize,
        len: usize,
    },
    BadRequestCrc,
    Unrecognized(Vec<u8>),
    OutOfRange,
}

/// The simulated ECU. Implements [`Port`].
pub struct SimEcu {
    ini: Ini,
    opts: SimOptions,
    framing: Framing,
    ram: Vec<Vec<u8>>,
    flash: Vec<Vec<u8>>,
    inbuf: Vec<u8>,
    out: VecDeque<u8>,
    faults: VecDeque<Fault>,
    events: Vec<SimEvent>,
    tick: u32,
}

enum Reply {
    /// Status + data (envelope) / data (plain).
    Data(u8, Vec<u8>),
    /// Nothing on the wire in plain mode; a status in envelope mode.
    Ack(u8),
}

impl SimEcu {
    pub fn new(ini: &Ini, opts: SimOptions) -> Self {
        let pages: Vec<Vec<u8>> = ini.pages.iter().map(|p| vec![0u8; p.size]).collect();
        Self {
            framing: opts.framing.unwrap_or_else(|| Framing::from_ini(ini)),
            ini: ini.clone(),
            opts,
            ram: pages.clone(),
            flash: pages,
            inbuf: Vec::new(),
            out: VecDeque::new(),
            faults: VecDeque::new(),
            events: Vec::new(),
            tick: 0,
        }
    }

    /// Load a tune into working and burned memory (as if previously burned).
    pub fn load_tune(&mut self, t: &Tune) {
        for (i, p) in t.pages.iter().enumerate() {
            if i < self.ram.len() && p.len() == self.ram[i].len() {
                self.ram[i].clone_from(p);
                self.flash[i].clone_from(p);
            }
        }
    }

    /// Working memory (what reads return).
    pub fn ram(&self) -> &[Vec<u8>] {
        &self.ram
    }

    /// Burned memory.
    pub fn flash(&self) -> &[Vec<u8>] {
        &self.flash
    }

    /// Power cycle: working memory reloads from burned memory.
    pub fn power_cycle(&mut self) {
        self.ram.clone_from(&self.flash);
        self.inbuf.clear();
        self.out.clear();
    }

    pub fn inject(&mut self, f: Fault) {
        self.faults.push_back(f);
    }

    pub fn events(&self) -> &[SimEvent] {
        &self.events
    }

    pub fn set_realtime(&mut self, block: Vec<u8>) {
        self.opts.realtime = Some(block);
    }

    fn take_fault(&mut self, pred: impl Fn(&Fault) -> bool) -> Option<Fault> {
        let i = self.faults.iter().position(pred)?;
        self.faults.remove(i)
    }

    fn vars(&self) -> impl Fn(&str) -> Option<u8> {
        let can = self.opts.can_id;
        move |n: &str| (n == "tsCanId" || n == "canId").then_some(can)
    }

    fn page_id(&self, page: usize) -> Vec<u8> {
        let vars = self.vars();
        self.ini.pages[page]
            .identifier
            .as_ref()
            .and_then(|t| {
                t.render(&RenderCtx {
                    endian: self.ini.endianness,
                    page_id: &[],
                    offset: 0,
                    count: 0,
                    value: &[],
                    vars: &vars,
                })
                .ok()
            })
            .unwrap_or_default()
    }

    fn matches(
        &self,
        t: &Option<CommandTemplate>,
        req: &[u8],
        page_id: &[u8],
    ) -> Option<opentune_tsini::Matched> {
        let t = t.as_ref()?;
        let vars = self.vars();
        t.match_request(req, self.ini.endianness, page_id, &vars)
    }

    fn realtime(&mut self) -> Vec<u8> {
        let size = self.ini.comms.och_block_size.unwrap_or(0);
        if let Some(b) = &self.opts.realtime {
            let mut v = b.clone();
            v.resize(size, 0);
            return v;
        }
        self.tick = self.tick.wrapping_add(1);
        let t = self.tick;
        (0..size)
            .map(|i| (i as u32).wrapping_mul(7).wrapping_add(t) as u8)
            .collect()
    }

    fn handle(&mut self, req: &[u8]) -> Reply {
        let c = &self.ini.comms;
        let query = c.query_command.clone();
        if self.matches(&query, req, &[]).is_some() {
            self.events.push(SimEvent::Query);
            let mut sig = self
                .opts
                .signature
                .clone()
                .or_else(|| self.ini.signature.clone())
                .unwrap_or_default()
                .into_bytes();
            if self.opts.signature_nul {
                sig.push(0);
            }
            return Reply::Data(STATUS_OK, sig);
        }
        let och = c.och_get_command.clone();
        if let Some(m) = self.matches(&och, req, &[]) {
            let block = self.realtime();
            let off = m.offset.unwrap_or(0);
            let len = m.count.unwrap_or(block.len());
            self.events.push(SimEvent::Realtime { offset: off, len });
            return match block.get(off..off + len) {
                Some(d) => Reply::Data(STATUS_OK, d.to_vec()),
                None => {
                    self.events.push(SimEvent::OutOfRange);
                    Reply::Ack(STATUS_OUT_OF_RANGE)
                }
            };
        }
        for page in 0..self.ini.pages.len() {
            let pd = self.ini.pages[page].clone();
            let id = self.page_id(page);
            let size = pd.size;
            let range = |m: &opentune_tsini::Matched, dflt: usize| {
                let off = m.offset.unwrap_or(0);
                let len = m.count.unwrap_or(dflt);
                (off + len <= size).then_some((off, len))
            };
            if let Some(m) = self.matches(&pd.read, req, &id) {
                let Some((off, len)) = range(&m, size) else {
                    self.events.push(SimEvent::OutOfRange);
                    return Reply::Ack(STATUS_OUT_OF_RANGE);
                };
                self.events.push(SimEvent::Read {
                    page,
                    offset: off,
                    len,
                });
                return Reply::Data(STATUS_OK, self.ram[page][off..off + len].to_vec());
            }
            let write = self
                .matches(&pd.chunk_write, req, &id)
                .or_else(|| self.matches(&pd.value_write, req, &id));
            if let Some(m) = write {
                let len = m.value.len();
                if m.count.is_some_and(|c| c != len) {
                    return Reply::Ack(STATUS_UNDERRUN);
                }
                let Some((off, _)) = range(&m, len) else {
                    self.events.push(SimEvent::OutOfRange);
                    return Reply::Ack(STATUS_OUT_OF_RANGE);
                };
                self.events.push(SimEvent::Write {
                    page,
                    offset: off,
                    len,
                });
                match self.take_fault(|f| matches!(f, Fault::IgnoreWrite | Fault::CorruptWrite)) {
                    Some(Fault::IgnoreWrite) => {}
                    Some(_) => {
                        self.ram[page][off..off + len].copy_from_slice(&m.value);
                        if len > 0 {
                            self.ram[page][off] ^= 1;
                        }
                    }
                    None => self.ram[page][off..off + len].copy_from_slice(&m.value),
                }
                return Reply::Ack(STATUS_OK);
            }
            if self.matches(&pd.burn, req, &id).is_some() {
                self.events.push(SimEvent::Burn { page });
                self.flash[page].clone_from(&self.ram[page]);
                return Reply::Ack(self.opts.burn_status);
            }
            if let Some(m) = self.matches(&pd.crc32, req, &id) {
                let Some((off, len)) = range(&m, size) else {
                    return Reply::Ack(STATUS_OUT_OF_RANGE);
                };
                self.events.push(SimEvent::Crc { page });
                let crc = crc32(&self.ram[page][off..off + len]);
                return Reply::Data(STATUS_OK, crc.to_be_bytes().to_vec());
            }
        }
        self.events.push(SimEvent::Unrecognized(req.to_vec()));
        Reply::Ack(STATUS_UNRECOGNIZED)
    }

    fn respond(&mut self, reply: Reply) {
        if self.take_fault(|f| *f == Fault::DropResponse).is_some() {
            return;
        }
        let mut bytes = match (self.framing, reply) {
            (Framing::Envelope, Reply::Data(s, d)) => {
                let mut p = vec![s];
                p.extend(d);
                framing::wrap(&p).expect("non-empty")
            }
            (Framing::Envelope, Reply::Ack(s)) => framing::wrap(&[s]).expect("non-empty"),
            // Plain mode has no status: errors and acks are silent.
            (Framing::Plain, Reply::Data(s, d)) if s < 0x80 => d,
            (Framing::Plain, _) => Vec::new(),
        };
        if bytes.is_empty() {
            return;
        }
        if self.take_fault(|f| *f == Fault::CorruptResponse).is_some() {
            let last = bytes.len() - 1;
            bytes[last] ^= 0x01;
        }
        if let Some(Fault::TruncateResponse(n)) =
            self.take_fault(|f| matches!(f, Fault::TruncateResponse(_)))
        {
            bytes.truncate(n);
        }
        self.out.extend(bytes);
    }

    fn process(&mut self, req: Vec<u8>) {
        if let Some(Fault::Status(s)) = self.take_fault(|f| matches!(f, Fault::Status(_))) {
            self.respond(Reply::Ack(s));
            return;
        }
        let r = self.handle(&req);
        self.respond(r);
    }
}

impl Port for SimEcu {
    fn send(&mut self, data: &[u8]) -> io::Result<()> {
        match self.framing {
            // Plain mode has no length: each send is one request.
            Framing::Plain => self.process(data.to_vec()),
            Framing::Envelope => {
                self.inbuf.extend_from_slice(data);
                while self.inbuf.len() >= 2 {
                    let len = u16::from_be_bytes([self.inbuf[0], self.inbuf[1]]) as usize;
                    if self.inbuf.len() < len + 6 {
                        break;
                    }
                    let frame: Vec<u8> = self.inbuf.drain(..len + 6).collect();
                    match framing::unwrap(&frame) {
                        Ok(payload) => self.process(payload),
                        Err(_) => {
                            self.events.push(SimEvent::BadRequestCrc);
                            self.respond(Reply::Ack(STATUS_CRC_FAILURE));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn recv(&mut self, buf: &mut [u8], _timeout: Duration) -> io::Result<usize> {
        let n = buf.len().min(self.out.len());
        for b in buf.iter_mut().take(n) {
            *b = self.out.pop_front().expect("counted");
        }
        Ok(n)
    }

    fn discard_input(&mut self) -> io::Result<()> {
        self.out.clear();
        Ok(())
    }
}
