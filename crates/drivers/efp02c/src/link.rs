//! The CAN link over an EFP02C: init sequence, frame send, polled receive.
use crate::proto::{self, IdentBlock, Poll, RxFrame, IDENT_CHUNKS, IDENT_LEN, RECORD_LEN};
use crate::transport::Efp02cTransport;
use opentune_core::protocol::isotp::CanLink;
use opentune_core::{Error, Result};
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// Received frames held on the host before the oldest are dropped.
const RX_QUEUE_MAX: usize = 4096;
/// Response (`0A`) records held for identification / termination reads.
const RESP_QUEUE_MAX: usize = 64;
/// Poll round trips spent on an identification read before giving up.
const IDENT_ATTEMPTS: usize = 8;

/// Delays between init steps. [`Timing::CAPTURED`] are the gaps of the
/// working capture; tests use [`Timing::NONE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// After `04 05` (reset).
    pub after_reset_ms: u64,
    /// After termination / config records.
    pub settle_ms: u64,
    /// After `04 02` (enable).
    pub after_enable_ms: u64,
    /// USB timeout for one poll reply.
    pub usb_read_ms: u64,
}

impl Timing {
    /// The gaps seen in the working capture (250 / 50 / 200 ms).
    pub const CAPTURED: Timing = Timing {
        after_reset_ms: 250,
        settle_ms: 50,
        after_enable_ms: 200,
        usb_read_ms: 300,
    };
    /// No delays (scripted transports).
    pub const NONE: Timing = Timing {
        after_reset_ms: 0,
        settle_ms: 0,
        after_enable_ms: 0,
        usb_read_ms: 300,
    };
}

impl Default for Timing {
    fn default() -> Self {
        Timing::CAPTURED
    }
}

/// Link options, from the driver's string options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkOptions {
    /// Switch the box's bus termination on (`termination`, default on; the
    /// owner's bench received nothing without it). `off` skips the command —
    /// the "off" value of the record is not confirmed, so it is never sent.
    pub termination: bool,
    /// Read the identification block during open (`identify=on`, default off).
    pub identify: bool,
    /// Which attached EFP02C to use (`usb_index`, default 0).
    pub usb_index: usize,
    pub timing: Timing,
}

impl Default for LinkOptions {
    fn default() -> Self {
        Self {
            termination: true,
            identify: false,
            usb_index: 0,
            timing: Timing::CAPTURED,
        }
    }
}

fn parse_bool(key: &str, v: &str) -> Result<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "on" | "1" | "true" | "yes" => Ok(true),
        "off" | "0" | "false" | "no" => Ok(false),
        other => Err(Error::Config(format!(
            "efp02c: bad {key} '{other}' (on/off)"
        ))),
    }
}

impl LinkOptions {
    /// Read `termination`, `identify` and `usb_index` from driver options.
    pub fn from_options(o: &BTreeMap<String, String>) -> Result<Self> {
        let mut l = LinkOptions::default();
        if let Some(v) = o.get("termination") {
            l.termination = parse_bool("termination", v)?;
        }
        if let Some(v) = o.get("identify") {
            l.identify = parse_bool("identify", v)?;
        }
        if let Some(v) = o.get("usb_index") {
            l.usb_index = v
                .trim()
                .parse()
                .map_err(|_| Error::Config(format!("efp02c: bad usb_index '{v}'")))?;
        }
        Ok(l)
    }
}

/// Counters for things the link tolerated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LinkStats {
    /// Frames dropped because the host queue was full.
    pub dropped: u64,
    /// Poll replies whose trailer did not echo the poll's sequence number.
    pub seq_mismatch: u64,
    /// IN records of a type this driver does not interpret (ignored).
    pub ignored_records: u64,
}

/// A [`CanLink`] over an EFP02C: 11-bit classic frames at 1 Mbit/s.
pub struct Efp02cLink {
    t: Box<dyn Efp02cTransport>,
    timing: Timing,
    seq: u8,
    enabled: bool,
    rx: VecDeque<RxFrame>,
    responses: VecDeque<[u8; RECORD_LEN]>,
    termination: Option<bool>,
    ident: Option<IdentBlock>,
    stats: LinkStats,
}

fn sleep_ms(ms: u64) {
    if ms > 0 {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

impl Efp02cLink {
    /// Run the captured init sequence and return a link ready for traffic:
    /// `04 05` reset → `04 09 01` termination on → `04 0A` + poll (termination
    /// read back) → [identification] → `04 01 …` config + `06 00` → `04 02`
    /// enable.
    pub fn open(t: Box<dyn Efp02cTransport>, opts: LinkOptions) -> Result<Self> {
        let mut l = Efp02cLink {
            t,
            timing: opts.timing,
            seq: 0,
            enabled: false,
            rx: VecDeque::new(),
            responses: VecDeque::new(),
            termination: None,
            ident: None,
            stats: LinkStats::default(),
        };
        l.t.write(&proto::reset())?;
        sleep_ms(l.timing.after_reset_ms);
        if opts.termination {
            l.t.write(&proto::termination_on())?;
            sleep_ms(l.timing.settle_ms);
        }
        l.responses.clear();
        l.poll_with(Some(proto::termination_read()), Poll::ONE)?;
        l.termination = l
            .responses
            .pop_front()
            .and_then(|r| proto::decode_termination(&r));
        if opts.identify {
            l.ident = Some(l.identify()?);
        }
        l.t.write(&proto::config_1mbit_accept_all())?;
        sleep_ms(l.timing.settle_ms);
        l.t.write(&proto::enable())?;
        sleep_ms(l.timing.after_enable_ms);
        l.enabled = true;
        Ok(l)
    }

    /// The termination state the box reported during open, if it answered.
    pub fn termination(&self) -> Option<bool> {
        self.termination
    }

    /// The identification block read during open (`identify=on`).
    pub fn ident(&self) -> Option<&IdentBlock> {
        self.ident.as_ref()
    }

    /// Counters for tolerated anomalies.
    pub fn stats(&self) -> LinkStats {
        self.stats
    }

    /// Transport description (USB bus / address).
    pub fn describe(&self) -> String {
        self.t.describe()
    }

    /// One poll round trip: write `prefix` (if any) and the `0B` record in one
    /// OUT transfer, read the reply and file its records. Returns the number of
    /// records received.
    fn poll_with(&mut self, prefix: Option<[u8; RECORD_LEN]>, poll: Poll) -> Result<usize> {
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        let mut out = Vec::with_capacity(2 * RECORD_LEN);
        if let Some(p) = prefix {
            out.extend_from_slice(&p);
        }
        out.extend_from_slice(&poll.record(seq));
        self.t.write(&out)?;
        let buf = self.t.read(self.timing.usb_read_ms)?;
        let reply = proto::parse_in(&buf);
        if let Some(tr) = reply.trailer {
            if tr.seq != seq {
                self.stats.seq_mismatch += 1;
            }
        }
        let n = reply.records.len();
        for r in reply.records {
            self.file_record(r);
        }
        Ok(n)
    }

    fn file_record(&mut self, r: [u8; RECORD_LEN]) {
        match r[0] {
            proto::rec::RX => {
                // Frames from before the channel was enabled are stale.
                if !self.enabled {
                    return;
                }
                if let Some(f) = proto::decode_rx(&r) {
                    if self.rx.len() >= RX_QUEUE_MAX {
                        self.rx.pop_front();
                        self.stats.dropped += 1;
                    }
                    self.rx.push_back(f);
                } else {
                    self.stats.ignored_records += 1;
                }
            }
            proto::rec::RESPONSE => {
                if self.responses.len() >= RESP_QUEUE_MAX {
                    self.responses.pop_front();
                }
                self.responses.push_back(r);
            }
            // e.g. `08` (a vendor licensing reply this driver never requests).
            _ => self.stats.ignored_records += 1,
        }
    }

    /// Read the 32-byte identification block: `09` + poll, then polls until
    /// all four `0A` chunks arrived (the captured box needed several), then
    /// `06 00`. Diagnostic information only.
    pub fn identify(&mut self) -> Result<IdentBlock> {
        self.responses.clear();
        let mut chunks: [Option<[u8; 8]>; IDENT_CHUNKS] = [None; IDENT_CHUNKS];
        for attempt in 0..IDENT_ATTEMPTS {
            // As captured: the request is repeated on every other poll.
            let prefix = (attempt % 2 == 0).then(proto::identify);
            self.poll_with(prefix, Poll::IDENTIFY)?;
            while let Some(r) = self.responses.pop_front() {
                if let Some((i, d)) = proto::decode_ident_chunk(&r) {
                    chunks[i] = Some(d);
                }
            }
            if chunks.iter().all(Option::is_some) {
                break;
            }
        }
        self.t.write(&proto::flush(0x00))?;
        sleep_ms(self.timing.after_enable_ms);
        let mut block = [0u8; IDENT_LEN];
        for (i, c) in chunks.iter().enumerate() {
            let c = c.ok_or_else(|| {
                Error::Protocol(format!(
                    "efp02c: identification chunk {i} of {IDENT_CHUNKS} never arrived"
                ))
            })?;
            block[i * 8..i * 8 + 8].copy_from_slice(&c);
        }
        let b = IdentBlock(block);
        if !b.checksum_ok() {
            return Err(Error::Protocol(
                "efp02c: identification block checksum mismatch".into(),
            ));
        }
        Ok(b)
    }

    /// The next received frame, polling the box until `timeout_ms` passes.
    /// Polls at least once even with a zero timeout.
    pub fn next_rx(&mut self, timeout_ms: u64) -> Result<Option<RxFrame>> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut polled = false;
        loop {
            if let Some(f) = self.rx.pop_front() {
                return Ok(Some(f));
            }
            if polled && Instant::now() >= deadline {
                return Ok(None);
            }
            // An empty reply has already waited on the box side.
            self.poll_with(None, Poll::CAN)?;
            polled = true;
        }
    }

    /// Reset the box (stops it ACKing bus traffic). Best effort.
    pub fn shutdown(&mut self) {
        if self.enabled {
            self.enabled = false;
            let _ = self.t.write(&proto::reset());
        }
    }
}

impl Drop for Efp02cLink {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl CanLink for Efp02cLink {
    fn send_frame(&mut self, id: u32, data: &[u8]) -> Result<()> {
        let r = proto::tx(id, data).map_err(|e| Error::Unsupported(format!("efp02c: {e}")))?;
        self.t.write(&r)
    }

    fn recv_frame(&mut self, timeout_ms: u64) -> Result<(u32, Vec<u8>)> {
        match self.next_rx(timeout_ms)? {
            Some(f) => Ok((f.id, f.data)),
            None => Err(Error::Timeout(timeout_ms)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse_and_refuse_garbage() {
        let mut o = BTreeMap::new();
        assert_eq!(
            LinkOptions::from_options(&o).unwrap(),
            LinkOptions::default()
        );
        o.insert("termination".into(), "off".into());
        o.insert("identify".into(), "on".into());
        o.insert("usb_index".into(), "1".into());
        let l = LinkOptions::from_options(&o).unwrap();
        assert!(!l.termination && l.identify);
        assert_eq!(l.usb_index, 1);
        o.insert("termination".into(), "maybe".into());
        assert!(matches!(
            LinkOptions::from_options(&o),
            Err(Error::Config(_))
        ));
    }
}
