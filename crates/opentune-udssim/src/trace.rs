//! Deterministic request/response trace recording.
//!
//! Every byte the client sends and every byte the simulated ECU answers is
//! appended to a [`Trace`] in order, including the NRC 0x78 `responsePending`
//! frames the fault layer interposes before a real answer. The ordered log is
//! what lets a test replay or assert the exact wire exchange a behaviour
//! produced.

/// Which side of the exchange a [`TraceEntry`] records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// A request the client sent to the ECU.
    ClientRequest,
    /// A (final) response the ECU sent back.
    ServerResponse,
    /// An interposed NRC 0x78 `responsePending` frame sent before the real
    /// answer (from the fault layer).
    ServerPending,
}

/// One recorded frame: its direction, its raw bytes, and an optional note.
#[derive(Debug, Clone)]
pub struct TraceEntry {
    /// The side that produced this frame.
    pub direction: Direction,
    /// The raw UDS bytes.
    pub data: Vec<u8>,
    /// A short human note (e.g. which fault produced the frame), if any.
    pub note: Option<String>,
}

/// An ordered log of the client/ECU exchange.
#[derive(Debug, Clone, Default)]
pub struct Trace {
    entries: Vec<TraceEntry>,
}

impl Trace {
    /// A new, empty trace.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a frame.
    pub fn push(&mut self, direction: Direction, data: &[u8], note: Option<String>) {
        self.entries.push(TraceEntry {
            direction,
            data: data.to_vec(),
            note,
        });
    }

    /// All recorded entries, in order.
    pub fn entries(&self) -> &[TraceEntry] {
        &self.entries
    }

    /// Just the client requests, in order.
    pub fn requests(&self) -> Vec<&[u8]> {
        self.entries
            .iter()
            .filter(|e| e.direction == Direction::ClientRequest)
            .map(|e| e.data.as_slice())
            .collect()
    }

    /// Just the final server responses, in order (excludes `responsePending`).
    pub fn responses(&self) -> Vec<&[u8]> {
        self.entries
            .iter()
            .filter(|e| e.direction == Direction::ServerResponse)
            .map(|e| e.data.as_slice())
            .collect()
    }

    /// How many `responsePending` frames were interposed in total.
    pub fn pending_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.direction == Direction::ServerPending)
            .count()
    }

    /// Total number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the trace is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// One line per entry, e.g. `-> 22 F1 90` / `<= 78 (pending)` / `<- 62 F1 90 …`,
    /// for diagnostics and golden-trace comparison.
    pub fn to_lines(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|e| {
                let arrow = match e.direction {
                    Direction::ClientRequest => "->",
                    Direction::ServerResponse => "<-",
                    Direction::ServerPending => "<=",
                };
                let hex = e
                    .data
                    .iter()
                    .map(|b| format!("{b:02X}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                match &e.note {
                    Some(n) => format!("{arrow} {hex}  ({n})"),
                    None => format!("{arrow} {hex}"),
                }
            })
            .collect()
    }
}
