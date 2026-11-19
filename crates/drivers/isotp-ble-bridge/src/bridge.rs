//! One session with the bridge firmware over any [`Link`]: frame encoding,
//! BLE splitting/reassembly, matching answers to requests, settings.
use crate::codec::{
    self, CodecError, Packet, Reassembler, Setting, StreamDecoder, FLAG_SETTINGS,
    FLAG_SETTINGS_GET, FLAG_SPLIT, MAX_ATT_VALUE,
};
use crate::link::{Framing, Link};
use opentune_core::protocol::diag::is_response_pending;
use opentune_core::{Error, Result};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

enum Decoder {
    Ble(Reassembler),
    Stream(StreamDecoder),
}

/// A bridge session over one [`Link`].
pub struct Bridge {
    link: Box<dyn Link>,
    framing: Framing,
    decoder: Decoder,
    inbox: VecDeque<Packet>,
    /// Frames received that no caller was waiting for (other links, late
    /// answers, persist replays).
    unmatched: u64,
    /// Codec errors seen on the receive path.
    codec_errors: u64,
    last_codec_error: Option<CodecError>,
}

fn codec_err(e: CodecError) -> Error {
    Error::Protocol(format!("isotp-ble bridge: {e}"))
}

impl Bridge {
    /// Wrap a link. A BLE link's ATT payload is clamped to what the firmware
    /// accepts in one value.
    pub fn new(link: Box<dyn Link>) -> Self {
        let framing = match link.framing() {
            Framing::Ble { att_payload } => Framing::Ble {
                att_payload: att_payload.min(MAX_ATT_VALUE),
            },
            Framing::Stream => Framing::Stream,
        };
        let decoder = match framing {
            Framing::Ble { .. } => Decoder::Ble(Reassembler::new()),
            Framing::Stream => Decoder::Stream(StreamDecoder::new()),
        };
        Self {
            link,
            framing,
            decoder,
            inbox: VecDeque::new(),
            unmatched: 0,
            codec_errors: 0,
            last_codec_error: None,
        }
    }

    /// The link's framing (after clamping).
    pub fn framing(&self) -> Framing {
        self.framing
    }

    /// Description of the underlying link.
    pub fn describe(&self) -> String {
        self.link.describe()
    }

    /// Frames dropped because nobody was waiting for them.
    pub fn unmatched(&self) -> u64 {
        self.unmatched
    }

    /// Receive-path codec errors so far, and the last one.
    pub fn codec_errors(&self) -> (u64, Option<&CodecError>) {
        (self.codec_errors, self.last_codec_error.as_ref())
    }

    /// Encode and send one frame (split over BLE as needed).
    ///
    /// A settings frame whose setting number the split flag would change
    /// (every setting but the GAP name) is refused when it does not fit one
    /// ATT value: the firmware would reassemble it under the wrong number.
    pub fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        match self.framing {
            Framing::Ble { att_payload } => {
                let flags = packet.header.flags;
                if flags & FLAG_SETTINGS != 0
                    && flags | FLAG_SPLIT != flags
                    && codec::HEADER_LEN + packet.payload.len() > att_payload
                {
                    return Err(Error::Unsupported(format!(
                        "isotp-ble bridge: a {}-byte settings frame needs an ATT MTU of at \
                         least {} (the firmware cannot reassemble split settings)",
                        codec::HEADER_LEN + packet.payload.len(),
                        codec::HEADER_LEN + packet.payload.len() + 3
                    )));
                }
                for chunk in codec::encode_for_ble(packet, att_payload).map_err(codec_err)? {
                    self.link.write(&chunk)?;
                }
                Ok(())
            }
            Framing::Stream => self.link.write(&packet.encode()),
        }
    }

    /// Pull bytes from the link once (waiting at most `timeout`) and decode
    /// whatever frames they complete into the inbox. Returns whether the link
    /// delivered anything.
    fn pump(&mut self, timeout: Duration) -> Result<bool> {
        let Some(bytes) = self.link.read(timeout)? else {
            return Ok(false);
        };
        let mut out = Vec::new();
        let r = match &mut self.decoder {
            Decoder::Ble(r) => r.push(&bytes, &mut out),
            Decoder::Stream(d) => d.push(&bytes, &mut out),
        };
        if let Err(e) = r {
            self.codec_errors += 1;
            self.last_codec_error = Some(e);
        }
        self.inbox.extend(out);
        Ok(true)
    }

    /// Discard everything already received (stale answers from an earlier,
    /// timed-out request), without waiting.
    pub fn drain(&mut self) -> Result<()> {
        while self.pump(Duration::ZERO)? {}
        self.unmatched += self.inbox.len() as u64;
        self.inbox.clear();
        Ok(())
    }

    /// Wait until `deadline` for the first frame `want` accepts; frames it
    /// rejects are dropped (counted in [`unmatched`](Self::unmatched)).
    pub fn wait_for(
        &mut self,
        deadline: Instant,
        mut want: impl FnMut(&Packet) -> bool,
    ) -> Result<Option<Packet>> {
        loop {
            while let Some(p) = self.inbox.pop_front() {
                if want(&p) {
                    return Ok(Some(p));
                }
                self.unmatched += 1;
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            self.pump(deadline - now)?;
        }
    }

    /// Send an ISO-TP payload on `tx_id` (answer expected on `rx_id`).
    pub fn send(&mut self, tx_id: u16, rx_id: u16, payload: &[u8]) -> Result<()> {
        if payload.is_empty() {
            return Err(Error::Config("isotp-ble bridge: empty payload".into()));
        }
        let p = Packet::data(tx_id, rx_id, payload).map_err(codec_err)?;
        self.send_packet(&p)
    }

    /// Wait for the next ISO-TP payload the ECU sent on `can_id`.
    pub fn recv_from(&mut self, can_id: u16, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let deadline = Instant::now() + timeout;
        let p = self.wait_for(deadline, |p| {
            p.header.flags & FLAG_SETTINGS == 0 && p.header.tx_id == can_id
        })?;
        Ok(p.map(|p| p.payload))
    }

    /// Set a device setting (the firmware sends no acknowledgement).
    pub fn set_setting(
        &mut self,
        setting: Setting,
        tx_id: u16,
        rx_id: u16,
        value: &[u8],
    ) -> Result<()> {
        let p = Packet::set_setting(setting, tx_id, rx_id, value).map_err(codec_err)?;
        self.send_packet(&p)
    }

    /// Read a device setting. For [`Setting::IsoTpStminUs`] give the link's
    /// tester `tx_id` / ECU `rx_id`; the others ignore them.
    ///
    /// Over the serial port the reply carries the setting in its flags. Over
    /// BLE the firmware zeroes the flags of everything it notifies, so a
    /// reply is recognised by its ids instead (0/0 for global settings, the
    /// link's ids for STmin) — call this only while no ECU traffic is in
    /// flight on that link.
    pub fn get_setting(
        &mut self,
        setting: Setting,
        tx_id: u16,
        rx_id: u16,
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        if !setting.readable() {
            return Err(Error::Unsupported(format!(
                "isotp-ble bridge: setting {setting:?} cannot be read"
            )));
        }
        let (tx, rx) = if setting.per_link() {
            (tx_id, rx_id)
        } else {
            (0, 0)
        };
        self.drain()?;
        self.send_packet(&Packet::get_setting(setting, tx, rx))?;
        let flags = FLAG_SETTINGS | setting as u8;
        // The reply's ids are the link's, seen from the ECU side.
        let (want_tx, want_rx) = (rx, tx);
        let deadline = Instant::now() + timeout;
        let reply = self.wait_for(deadline, |p| {
            let h = &p.header;
            let ids = h.tx_id == want_tx && h.rx_id == want_rx;
            // BLE: zeroed flags, plus the split bit on a long reply.
            ids && (h.flags & !FLAG_SETTINGS_GET == flags || h.flags & !FLAG_SPLIT == 0)
        })?;
        reply
            .map(|p| p.payload)
            .ok_or_else(|| Error::Timeout(timeout.as_millis().min(u64::MAX as u128) as u64))
    }

    /// One UDS/KWP exchange on a link: send `req`, then return the first
    /// answer on `rx_id` that belongs to it (positive response to the same
    /// service, or a negative response naming it), draining `responsePending`
    /// (NRC 0x78) frames. Each wait gets the full `timeout`. Earlier, stale
    /// answers are discarded before sending.
    pub fn request(
        &mut self,
        tx_id: u16,
        rx_id: u16,
        req: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        const MAX_PENDING: usize = 255;
        let sid = *req
            .first()
            .ok_or_else(|| Error::Config("isotp-ble bridge: empty request".into()))?;
        self.drain()?;
        self.send(tx_id, rx_id, req)?;
        let mut pending = 0usize;
        loop {
            let deadline = Instant::now() + timeout;
            let resp = self.wait_for(deadline, |p| {
                p.header.flags & FLAG_SETTINGS == 0
                    && p.header.tx_id == rx_id
                    && belongs_to(sid, &p.payload)
            })?;
            let Some(resp) = resp else {
                return Err(Error::Timeout(
                    timeout.as_millis().min(u64::MAX as u128) as u64
                ));
            };
            let r = resp.payload;
            if is_response_pending(req, &r) {
                pending += 1;
                if pending > MAX_PENDING {
                    return Err(Error::Protocol(
                        "isotp-ble bridge: too many responsePending frames".into(),
                    ));
                }
                continue;
            }
            return Ok(r);
        }
    }
}

/// Whether `resp` answers service `sid`: a positive response (`sid + 0x40`)
/// or a negative response (`7F sid ..`).
fn belongs_to(sid: u8, resp: &[u8]) -> bool {
    match resp {
        [0x7F, s, ..] => *s == sid,
        [first, ..] => *first == sid.wrapping_add(0x40),
        [] => false,
    }
}

#[cfg(test)]
mod tests {
    use super::belongs_to;

    #[test]
    fn answer_correlation() {
        assert!(belongs_to(0x22, &[0x62, 0xF1, 0x90]));
        assert!(belongs_to(0x22, &[0x7F, 0x22, 0x31]));
        assert!(!belongs_to(0x22, &[0x7F, 0x10, 0x31]));
        assert!(!belongs_to(0x22, &[0x50, 0x03]));
        assert!(!belongs_to(0x22, &[]));
    }
}
