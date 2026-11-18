//! [`Client`]: the INI-driven request/response layer.
use crate::framing::{self, status_name, STATUS_BUSY, STATUS_CRC_FAILURE, STATUS_UNDERRUN};
use crate::port::{recv_exact, recv_until_idle, Port};
use crate::{ProtoError, Result, WriteAuth};
use opentune_tsini::{normalize_signature, CommandTemplate, Ini, RenderCtx, Tune};
use std::time::Duration;

/// How requests and responses are framed on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// CRC32 msEnvelope (`messageEnvelopeFormat = msEnvelope_1.0`).
    Envelope,
    /// Raw command bytes, raw response bytes (legacy MegaSquirt / Speeduino
    /// compatibility mode).
    Plain,
}

impl Framing {
    /// What the definition asks for.
    pub fn from_ini(ini: &Ini) -> Self {
        if ini.comms.uses_envelope() {
            Self::Envelope
        } else {
            Self::Plain
        }
    }
}

/// Client settings. `Default` follows the INI for framing and timeout.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// Override the INI's framing.
    pub framing: Option<Framing>,
    /// Per-response timeout; default the INI's `blockReadTimeout` or 1 s.
    pub timeout: Option<Duration>,
    /// Extra attempts after a timeout, CRC error or busy/corrupt status.
    pub retries: u32,
    /// Sleep for `interWriteDelay` / `pageActivationDelay` (off for the simulator).
    pub honor_delays: bool,
    /// Value of the `$tsCanId` variable in commands.
    pub can_id: u8,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            framing: None,
            timeout: None,
            retries: 2,
            honor_delays: true,
            can_id: 0,
        }
    }
}

/// Counters for diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub requests: u32,
    pub retries: u32,
    pub bytes_out: usize,
    pub bytes_in: usize,
}

/// Result of comparing the ECU's signature with the INI's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureCheck {
    pub ecu: String,
    pub ini: Option<String>,
    pub matches: bool,
}

/// What a request expects back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// Exactly this many data bytes.
    Data(usize),
    /// An acknowledgement (envelope: a status; plain: nothing).
    Ack,
    /// A reply of unknown length.
    Text,
}

/// Speaks one ECU's protocol as its INI describes it. Reads are always safe;
/// the writing methods need a [`WriteAuth`].
pub struct Client<'a, P: Port> {
    port: P,
    ini: &'a Ini,
    framing: Framing,
    timeout: Duration,
    retries: u32,
    honor_delays: bool,
    can_id: u8,
    stats: Stats,
}

impl<'a, P: Port> Client<'a, P> {
    pub fn new(port: P, ini: &'a Ini, opts: ClientOptions) -> Self {
        let timeout = opts.timeout.unwrap_or_else(|| {
            Duration::from_millis(ini.comms.block_read_timeout_ms.unwrap_or(1000).max(50))
        });
        Self {
            port,
            ini,
            framing: opts.framing.unwrap_or_else(|| Framing::from_ini(ini)),
            timeout,
            retries: opts.retries,
            honor_delays: opts.honor_delays,
            can_id: opts.can_id,
            stats: Stats::default(),
        }
    }

    pub fn framing(&self) -> Framing {
        self.framing
    }

    pub fn ini(&self) -> &'a Ini {
        self.ini
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn port_mut(&mut self) -> &mut P {
        &mut self.port
    }

    pub fn into_port(self) -> P {
        self.port
    }

    fn sleep_ms(&self, ms: Option<u64>) {
        if self.honor_delays {
            if let Some(ms) = ms.filter(|m| *m > 0) {
                std::thread::sleep(Duration::from_millis(ms));
            }
        }
    }

    /// Render `t` for `page` (identifier bytes for `%i`).
    pub fn render(
        &self,
        t: &CommandTemplate,
        page: Option<usize>,
        offset: usize,
        count: usize,
        value: &[u8],
    ) -> Result<Vec<u8>> {
        let can = self.can_id;
        let vars = move |n: &str| (n == "tsCanId" || n == "canId").then_some(can);
        let page_id = match page.and_then(|p| self.ini.pages.get(p)) {
            Some(pd) => match &pd.identifier {
                Some(idt) => idt
                    .render(&RenderCtx {
                        endian: self.ini.endianness,
                        page_id: &[],
                        offset: 0,
                        count: 0,
                        value: &[],
                        vars: &vars,
                    })
                    .map_err(ProtoError::Definition)?,
                None => Vec::new(),
            },
            None => Vec::new(),
        };
        t.render(&RenderCtx {
            endian: self.ini.endianness,
            page_id: &page_id,
            offset,
            count,
            value,
            vars: &vars,
        })
        .map_err(ProtoError::Definition)
    }

    fn once(&mut self, what: &str, req: &[u8], expect: Expect) -> Result<Vec<u8>> {
        self.stats.requests += 1;
        match self.framing {
            Framing::Envelope => {
                let frame = framing::wrap(req)?;
                self.stats.bytes_out += frame.len();
                self.port.send(&frame)?;
                let payload = framing::read_frame(&mut self.port, self.timeout, what)?;
                self.stats.bytes_in += payload.len() + 6;
                let status = payload[0];
                if status >= 0x80 {
                    return Err(ProtoError::Ecu {
                        code: status,
                        name: status_name(status),
                        command: what.into(),
                    });
                }
                let data = payload[1..].to_vec();
                if let Expect::Data(n) = expect {
                    if data.len() != n {
                        return Err(ProtoError::Frame(format!(
                            "{what}: expected {n} data bytes, got {}",
                            data.len()
                        )));
                    }
                }
                Ok(data)
            }
            Framing::Plain => {
                self.stats.bytes_out += req.len();
                self.port.send(req)?;
                match expect {
                    Expect::Ack => Ok(Vec::new()),
                    Expect::Data(n) => {
                        let mut buf = vec![0u8; n];
                        let got = recv_exact(&mut self.port, &mut buf, self.timeout)?;
                        self.stats.bytes_in += got;
                        if got < n {
                            return Err(ProtoError::Timeout {
                                what: what.into(),
                                got,
                                expected: n,
                            });
                        }
                        Ok(buf)
                    }
                    Expect::Text => {
                        let v = recv_until_idle(
                            &mut self.port,
                            256,
                            self.timeout,
                            Duration::from_millis(50),
                        )?;
                        self.stats.bytes_in += v.len();
                        if v.is_empty() {
                            return Err(ProtoError::Timeout {
                                what: what.into(),
                                got: 0,
                                expected: 1,
                            });
                        }
                        Ok(v)
                    }
                }
            }
        }
    }

    /// One exchange with retries on transient errors. Every request this
    /// client sends is idempotent (reads, or a write of the same bytes to the
    /// same place), so re-sending it is safe.
    fn exchange(&mut self, what: &str, req: &[u8], expect: Expect) -> Result<Vec<u8>> {
        let mut attempt = 0;
        loop {
            match self.once(what, req, expect) {
                Ok(v) => return Ok(v),
                Err(e) if attempt < self.retries && retryable(&e) => {
                    attempt += 1;
                    self.stats.retries += 1;
                    if matches!(
                        e,
                        ProtoError::Ecu {
                            code: STATUS_BUSY,
                            ..
                        }
                    ) {
                        self.sleep_ms(Some(50));
                    }
                    // Resynchronise: drop the rest of a bad reply.
                    self.sleep_ms(Some(20));
                    self.port.discard_input()?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Send the INI's `queryCommand` and return the signature (trailing NULs
    /// and spaces trimmed).
    pub fn query_signature(&mut self) -> Result<String> {
        let t = self
            .ini
            .comms
            .query_command
            .clone()
            .ok_or_else(|| ProtoError::Definition("INI has no queryCommand".into()))?;
        let req = self.render(&t, None, 0, 0, &[])?;
        let raw = self.exchange("signature query", &req, Expect::Text)?;
        let s: String = raw.iter().map(|&b| b as char).collect();
        Ok(normalize_signature(&s).to_owned())
    }

    /// Compare the ECU's signature with the INI's. A mismatch is an error
    /// unless `force`; the check result is returned either way.
    pub fn check_signature(&mut self, force: bool) -> Result<SignatureCheck> {
        let ecu = self.query_signature()?;
        let ini = self.ini.signature.clone();
        let matches = ini
            .as_deref()
            .is_some_and(|s| normalize_signature(s) == ecu);
        if !matches && !force {
            return Err(ProtoError::SignatureMismatch {
                ecu,
                ini: ini.unwrap_or_default(),
            });
        }
        Ok(SignatureCheck { ecu, ini, matches })
    }

    fn blocking_factor(&self) -> usize {
        self.ini.comms.blocking_factor.unwrap_or(256).max(1)
    }

    fn page_def(&self, page: usize) -> Result<&'a opentune_tsini::PageDef> {
        self.ini
            .pages
            .get(page)
            .ok_or_else(|| ProtoError::Definition(format!("no page {} in the INI", page + 1)))
    }

    /// Read one page (0-based), in `blockingFactor` chunks when the read
    /// command takes an offset and count. `progress(done, total)`.
    pub fn read_page(
        &mut self,
        page: usize,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Result<Vec<u8>> {
        let pd = self.page_def(page)?;
        let t = pd.read.clone().ok_or_else(|| {
            ProtoError::Definition(format!("page {} has no pageReadCommand", page + 1))
        })?;
        let size = pd.size;
        let what = format!("read page {}", page + 1);
        if !(t.has_offset() && t.has_count()) {
            let req = self.render(&t, Some(page), 0, size, &[])?;
            let data = self.exchange(&what, &req, Expect::Data(size))?;
            progress(size, size);
            return Ok(data);
        }
        let chunk = self.blocking_factor();
        let mut out = Vec::with_capacity(size);
        while out.len() < size {
            let off = out.len();
            let n = chunk.min(size - off);
            let req = self.render(&t, Some(page), off, n, &[])?;
            let data = self.exchange(&format!("{what} @{off}+{n}"), &req, Expect::Data(n))?;
            out.extend_from_slice(&data);
            progress(out.len(), size);
        }
        Ok(out)
    }

    /// Read every page into a [`Tune`]. `progress(page, done, total)`.
    pub fn read_tune(&mut self, progress: &mut dyn FnMut(usize, usize, usize)) -> Result<Tune> {
        let mut pages = Vec::with_capacity(self.ini.pages.len());
        for p in 0..self.ini.pages.len() {
            pages.push(self.read_page(p, &mut |d, t| progress(p, d, t))?);
        }
        Ok(Tune::from_pages(self.ini, pages)?)
    }

    /// The ECU's CRC32 of a page via `crc32CheckCommand`; `None` when the
    /// INI has no such command for the page.
    pub fn page_crc32(&mut self, page: usize) -> Result<Option<u32>> {
        let pd = self.page_def(page)?;
        let Some(t) = pd.crc32.clone() else {
            return Ok(None);
        };
        let req = self.render(&t, Some(page), 0, pd.size, &[])?;
        let d = self.exchange(
            &format!("CRC32 of page {}", page + 1),
            &req,
            Expect::Data(4),
        )?;
        Ok(Some(u32::from_be_bytes([d[0], d[1], d[2], d[3]])))
    }

    /// Read the realtime output-channel block (`ochGetCommand`, `ochBlockSize`
    /// bytes), chunked when the command takes an offset and count.
    pub fn read_realtime(&mut self) -> Result<Vec<u8>> {
        let t = self
            .ini
            .comms
            .och_get_command
            .clone()
            .ok_or_else(|| ProtoError::Definition("INI has no ochGetCommand".into()))?;
        let size = self
            .ini
            .comms
            .och_block_size
            .ok_or_else(|| ProtoError::Definition("INI has no ochBlockSize".into()))?;
        if !(t.has_offset() && t.has_count()) {
            let req = self.render(&t, None, 0, size, &[])?;
            return self.exchange("realtime data", &req, Expect::Data(size));
        }
        let chunk = self.blocking_factor();
        let mut out = Vec::with_capacity(size);
        while out.len() < size {
            let off = out.len();
            let n = chunk.min(size - off);
            let req = self.render(&t, None, off, n, &[])?;
            out.extend(self.exchange("realtime data", &req, Expect::Data(n))?);
        }
        Ok(out)
    }

    /// Write `data` at `offset` of `page` into the ECU's working memory
    /// (not burned). Split to fit `blockingFactor`. Needs [`WriteAuth`].
    pub fn write_chunk(
        &mut self,
        _auth: &WriteAuth,
        page: usize,
        offset: usize,
        data: &[u8],
    ) -> Result<()> {
        let pd = self.page_def(page)?;
        if offset + data.len() > pd.size {
            return Err(ProtoError::Definition(format!(
                "write {}+{} past page {} size {}",
                offset,
                data.len(),
                page + 1,
                pd.size
            )));
        }
        let t = pd
            .chunk_write
            .clone()
            .or_else(|| pd.value_write.clone())
            .filter(CommandTemplate::has_value)
            .ok_or_else(|| {
                ProtoError::Definition(format!("page {} has no write command", page + 1))
            })?;
        // Largest value that keeps the whole request within blockingFactor.
        let max = if t.has_count() {
            let overhead = self.render(&t, Some(page), offset, 0, &[])?.len();
            self.blocking_factor().saturating_sub(overhead).max(1)
        } else {
            1 // value-write commands without a count carry one byte
        };
        let mut done = 0;
        while done < data.len() {
            let n = max.min(data.len() - done);
            let off = offset + done;
            let req = self.render(&t, Some(page), off, n, &data[done..done + n])?;
            self.exchange(
                &format!("write page {} @{off}+{n}", page + 1),
                &req,
                Expect::Ack,
            )?;
            if self.framing == Framing::Plain || self.ini.comms.ts_write_blocks != Some(true) {
                self.sleep_ms(self.ini.comms.inter_write_delay_ms);
            }
            done += n;
        }
        Ok(())
    }

    /// Burn a page from working memory to permanent storage. Returns false
    /// when the INI has no burn command for it (RAM-only page). Needs
    /// [`WriteAuth`].
    pub fn burn(&mut self, _auth: &WriteAuth, page: usize) -> Result<bool> {
        let pd = self.page_def(page)?;
        let Some(t) = pd.burn.clone() else {
            return Ok(false);
        };
        let req = self.render(&t, Some(page), 0, pd.size, &[])?;
        self.exchange(&format!("burn page {}", page + 1), &req, Expect::Ack)?;
        self.sleep_ms(self.ini.comms.page_activation_delay_ms);
        Ok(true)
    }
}

fn retryable(e: &ProtoError) -> bool {
    match e {
        ProtoError::Timeout { .. } | ProtoError::Crc { .. } | ProtoError::Frame(_) => true,
        ProtoError::Ecu { code, .. } => {
            matches!(
                *code,
                STATUS_UNDERRUN | STATUS_CRC_FAILURE | STATUS_BUSY | framing::STATUS_FRAMING_ERROR
            )
        }
        _ => false,
    }
}
