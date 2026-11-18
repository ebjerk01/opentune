//! An in-process stand-in for the bridge firmware, for tests and offline
//! front-end work. It follows the firmware's observable behaviour as
//! documented in `docs/ISOTP-BLE-BRIDGE.md`:
//!
//! * BLE framing: split-packet reassembly of host writes; answers packed
//!   several to a notification when they fit ("multisend"), split when they
//!   do not; **flags zeroed** on everything notified, as the firmware does.
//! * Stream framing: answers emitted as raw bytes in small, uneven pieces.
//! * Only the compiled-in [`LINKS`](crate::codec::LINKS) are served; a request
//!   for any other id pair, or longer than the link buffer, is dropped
//!   silently.
//! * Settings are stored; reads are answered (flags kept on the stream).
//!
//! The ECU side is a [`MockEcu`]: given a request payload on a link, it
//! returns the payloads the ECU answers with (none, one, or several — e.g.
//! `7F xx 78` pending frames before the final answer).
use crate::codec::{
    self, find_link, Header, Packet, Reassembler, Setting, StreamDecoder, FLAG_PERSIST_ADD,
    FLAG_PERSIST_CLEAR, FLAG_PERSIST_ENABLE, FLAG_SETTINGS, FLAG_SETTINGS_GET,
};
use crate::link::{Framing, Link};
use opentune_core::{Error, Result};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The simulated ECU(s) behind the bridge.
pub trait MockEcu: Send {
    /// Answer `req`, sent by the tester on `tx_id` (the link's request id).
    /// Each returned payload goes back as one ISO-TP message on the link's
    /// response id.
    fn handle(&mut self, tx_id: u16, req: &[u8]) -> Vec<Vec<u8>>;
}

impl<F> MockEcu for F
where
    F: FnMut(u16, &[u8]) -> Vec<Vec<u8>> + Send,
{
    fn handle(&mut self, tx_id: u16, req: &[u8]) -> Vec<Vec<u8>> {
        self(tx_id, req)
    }
}

/// What the mock recorded, shared with the test through [`MockHandle`].
#[derive(Debug, Default, Clone)]
pub struct MockState {
    /// Every value/byte run the host wrote, in order.
    pub writes: Vec<Vec<u8>>,
    /// Every value/byte run the device sent, in order.
    pub notifications: Vec<Vec<u8>>,
    /// Every ISO-TP request delivered to the ECU: (tx id, payload).
    pub requests: Vec<(u16, Vec<u8>)>,
    /// Requests dropped like the firmware would (unknown link, too long).
    pub dropped: Vec<(u16, u16, usize)>,
    /// STmin override per (tx id, rx id), µs.
    pub stmin_us: BTreeMap<(u16, u16), u16>,
    /// Last value of each global setting.
    pub settings: BTreeMap<u8, Vec<u8>>,
    /// Messages queued for persist mode: (tx id, rx id, payload).
    pub persist: Vec<(u16, u16, Vec<u8>)>,
    /// Persist mode on.
    pub persist_enabled: bool,
    /// Receive-path codec errors (bad writes from the host).
    pub codec_errors: usize,
}

/// A handle onto a [`MockBridge`]'s recorded state.
#[derive(Clone)]
pub struct MockHandle(Arc<Mutex<MockState>>);

impl MockHandle {
    /// A snapshot of the recorded state.
    pub fn state(&self) -> MockState {
        self.0.lock().expect("mock state").clone()
    }
}

enum Inbound {
    Ble(Reassembler),
    Stream(StreamDecoder),
}

/// The simulated bridge firmware, as a [`Link`].
pub struct MockBridge {
    framing: Framing,
    inbound: Inbound,
    outbound: VecDeque<Vec<u8>>,
    ecu: Box<dyn MockEcu>,
    state: Arc<Mutex<MockState>>,
    stream_piece: usize,
}

impl MockBridge {
    /// A mock BLE bridge with the given ATT payload (MTU − 3).
    pub fn ble(att_payload: usize, ecu: impl MockEcu + 'static) -> (Self, MockHandle) {
        Self::new(
            Framing::Ble { att_payload },
            Inbound::Ble(Reassembler::new()),
            ecu,
        )
    }

    /// A mock bridge on the USB serial port.
    pub fn serial(ecu: impl MockEcu + 'static) -> (Self, MockHandle) {
        Self::new(Framing::Stream, Inbound::Stream(StreamDecoder::new()), ecu)
    }

    fn new(framing: Framing, inbound: Inbound, ecu: impl MockEcu + 'static) -> (Self, MockHandle) {
        let state = Arc::new(Mutex::new(MockState::default()));
        (
            Self {
                framing,
                inbound,
                outbound: VecDeque::new(),
                ecu: Box::new(ecu),
                state: state.clone(),
                stream_piece: 7,
            },
            MockHandle(state),
        )
    }

    /// Process one complete frame from the host, as `parse_packet` does.
    fn on_packet(&mut self, p: Packet, replies: &mut Vec<Packet>) {
        let h = p.header;
        let mut st = self.state.lock().expect("mock state");
        if h.flags & FLAG_SETTINGS != 0 {
            let Some(setting) = h.setting() else {
                return;
            };
            if h.flags & FLAG_SETTINGS_GET != 0 {
                if h.len != 0 {
                    return;
                }
                let value = match setting {
                    Setting::IsoTpStminUs => {
                        if find_link(h.tx_id, h.rx_id).is_none() {
                            return;
                        }
                        let v = st.stmin_us.get(&(h.tx_id, h.rx_id)).copied().unwrap_or(0);
                        v.to_le_bytes().to_vec()
                    }
                    Setting::GapName => st
                        .settings
                        .get(&(setting as u8))
                        .cloned()
                        .unwrap_or_else(|| codec::DEFAULT_DEVICE_NAME.as_bytes().to_vec()),
                    Setting::Password => return,
                    Setting::LedColor => st
                        .settings
                        .get(&(setting as u8))
                        .cloned()
                        .unwrap_or_else(|| vec![0; 4]),
                    _ => st
                        .settings
                        .get(&(setting as u8))
                        .cloned()
                        .unwrap_or_else(|| vec![0; 2]),
                };
                // Reply ids: the link's, from the ECU side (0/0 when global).
                replies.push(Packet {
                    header: Header {
                        flags: FLAG_SETTINGS | setting as u8,
                        rx_id: h.tx_id,
                        tx_id: h.rx_id,
                        len: value.len() as u16,
                    },
                    payload: value,
                });
            } else {
                let ok = match setting.value_len() {
                    Some(n) => p.payload.len() == n,
                    None => p.payload.len() <= setting.max_len(),
                };
                if !ok {
                    return;
                }
                if setting == Setting::IsoTpStminUs {
                    if find_link(h.tx_id, h.rx_id).is_some() {
                        let v = u16::from_le_bytes([p.payload[0], p.payload[1]]);
                        st.stmin_us.insert((h.tx_id, h.rx_id), v);
                    }
                } else {
                    st.settings.insert(setting as u8, p.payload.clone());
                }
            }
            return;
        }
        // Persist handling, in the firmware's order.
        if st.persist_enabled {
            if h.flags & FLAG_PERSIST_CLEAR != 0 {
                st.persist.clear();
            }
            if h.flags & FLAG_PERSIST_ENABLE == 0 {
                st.persist_enabled = false;
            } else {
                return;
            }
        } else {
            if h.flags & FLAG_PERSIST_CLEAR != 0 {
                st.persist.clear();
            }
            if h.flags & FLAG_PERSIST_ADD != 0 {
                st.persist.push((h.tx_id, h.rx_id, p.payload.clone()));
            }
            if h.flags & FLAG_PERSIST_ENABLE != 0 {
                st.persist_enabled = true;
                return;
            }
        }
        if p.payload.is_empty() || st.persist_enabled {
            return;
        }
        let Some(link) = find_link(h.tx_id, h.rx_id) else {
            st.dropped.push((h.tx_id, h.rx_id, p.payload.len()));
            return;
        };
        if p.payload.len() > link.max_payload {
            st.dropped.push((h.tx_id, h.rx_id, p.payload.len()));
            return;
        }
        st.requests.push((h.tx_id, p.payload.clone()));
        drop(st);
        for answer in self.ecu.handle(h.tx_id, &p.payload) {
            if answer.is_empty() || answer.len() > link.max_payload {
                continue;
            }
            replies.push(Packet {
                header: Header {
                    flags: 0,
                    rx_id: link.tx_id,
                    tx_id: link.rx_id,
                    len: answer.len() as u16,
                },
                payload: answer,
            });
        }
    }

    fn emit(&mut self, mut replies: Vec<Packet>) {
        if replies.is_empty() {
            return;
        }
        let values = match self.framing {
            Framing::Ble { att_payload } => {
                // The firmware never copies flags into a notified header.
                for r in &mut replies {
                    r.header.flags = 0;
                }
                codec::pack_for_ble(&replies, att_payload).expect("att payload")
            }
            Framing::Stream => {
                let bytes: Vec<u8> = replies.iter().flat_map(|r| r.encode()).collect();
                let piece = self.stream_piece;
                // Uneven pieces exercise the host's stream decoder.
                self.stream_piece = if piece >= 61 { 7 } else { piece * 3 };
                bytes.chunks(piece).map(<[u8]>::to_vec).collect()
            }
        };
        let mut st = self.state.lock().expect("mock state");
        st.notifications.extend(values.iter().cloned());
        self.outbound.extend(values);
    }
}

impl Link for MockBridge {
    fn framing(&self) -> Framing {
        self.framing
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if let Framing::Ble { att_payload } = self.framing {
            if bytes.len() > att_payload {
                return Err(Error::Protocol(format!(
                    "mock bridge: write of {} bytes exceeds the ATT payload {att_payload}",
                    bytes.len()
                )));
            }
        }
        self.state
            .lock()
            .expect("mock state")
            .writes
            .push(bytes.to_vec());
        let mut packets = Vec::new();
        let r = match &mut self.inbound {
            Inbound::Ble(r) => r.push(bytes, &mut packets),
            Inbound::Stream(d) => d.push(bytes, &mut packets),
        };
        if r.is_err() {
            self.state.lock().expect("mock state").codec_errors += 1;
        }
        let mut replies = Vec::new();
        for p in packets {
            self.on_packet(p, &mut replies);
        }
        self.emit(replies);
        Ok(())
    }

    fn read(&mut self, _timeout: Duration) -> Result<Option<Vec<u8>>> {
        // Answers are produced synchronously on write, so an empty queue
        // means nothing more will come: report the timeout at once.
        Ok(self.outbound.pop_front())
    }

    fn describe(&self) -> String {
        match self.framing {
            Framing::Ble { att_payload } => format!("mock BLE bridge (ATT payload {att_payload})"),
            Framing::Stream => "mock serial bridge".into(),
        }
    }
}
