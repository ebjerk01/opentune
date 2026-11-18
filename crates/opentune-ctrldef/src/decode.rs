//! Decode CAN frames with a definition's broadcast sets, and summarise a log.
use crate::{CanBus, CanFrameDef, ControllerDef, LiveSource, Signal, SignalKind};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

/// A decoded signal value.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Value {
    /// Scaled number.
    Number(f64),
    /// Flag.
    Bool(bool),
    /// Named raw value (`None` when the raw value is not listed).
    Enum(Option<String>),
    /// One ASCII character (non-printable shown as `\xNN`).
    Ascii(String),
    /// Marked invalid / not available.
    Invalid,
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Number(v) => write!(f, "{}", fmt_num(*v)),
            Value::Bool(b) => write!(f, "{}", if *b { "1" } else { "0" }),
            Value::Enum(Some(n)) => write!(f, "{n}"),
            Value::Enum(None) => write!(f, "?"),
            Value::Ascii(s) => write!(f, "'{s}'"),
            Value::Invalid => write!(f, "n/a"),
        }
    }
}

/// Format a number without trailing noise (`12`, `12.5`, `0.004`).
pub fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let s = format!("{v:.4}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// One decoded signal.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DecodedSignal {
    /// Signal name.
    pub name: String,
    /// Raw value.
    pub raw: u64,
    /// Decoded value.
    pub value: Value,
    /// Unit.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub unit: String,
}

/// One decoded frame.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DecodedFrame {
    /// Bus set name.
    pub bus: String,
    /// Frame name.
    pub frame: String,
    /// CAN id.
    pub id: u32,
    /// Signals that fit in the payload, in definition order.
    pub signals: Vec<DecodedSignal>,
}

impl DecodedFrame {
    /// The signal with this name.
    pub fn get(&self, name: &str) -> Option<&DecodedSignal> {
        self.signals.iter().find(|s| s.name == name)
    }
    /// Number value of a signal (`None` if missing, invalid or not a number).
    pub fn num(&self, name: &str) -> Option<f64> {
        match self.get(name)?.value {
            Value::Number(v) => Some(v),
            _ => None,
        }
    }
}

/// Decode one signal from a payload; `None` if it does not fit.
pub fn decode_signal(sig: &Signal, data: &[u8]) -> Option<DecodedSignal> {
    let raw = sig.raw(data)?;
    let value = if sig.is_invalid(raw) {
        Value::Invalid
    } else {
        match sig.kind {
            SignalKind::Number => Value::Number(sig.physical(raw)),
            SignalKind::Bool => Value::Bool(raw != 0),
            SignalKind::Enum => Value::Enum(sig.value_name(raw).map(str::to_string)),
            SignalKind::Ascii => {
                let b = raw as u8;
                Value::Ascii(if (0x20..0x7F).contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("\\x{b:02X}")
                })
            }
        }
    };
    Some(DecodedSignal {
        name: sig.name.clone(),
        raw,
        value,
        unit: sig.unit.clone(),
    })
}

/// Decode a payload with one frame definition (`None` below `min_len`).
pub fn decode_with(bus: &CanBus, frame: &CanFrameDef, data: &[u8]) -> Option<DecodedFrame> {
    if data.len() < frame.min_len.unwrap_or(1) as usize {
        return None;
    }
    Some(DecodedFrame {
        bus: bus.name.clone(),
        frame: frame.name.clone(),
        id: frame.id,
        signals: frame
            .signals
            .iter()
            .filter_map(|s| decode_signal(s, data))
            .collect(),
    })
}

/// A frame lookup over one or all of a definition's bus sets.
pub struct Decoder<'a> {
    def: &'a ControllerDef,
    index: HashMap<(u32, bool), (&'a CanBus, &'a CanFrameDef)>,
}

impl<'a> Decoder<'a> {
    /// Index the frames of `bus` (or of every set when `None`). Errors on an
    /// unknown set name, and — for `None` — when two sets define the same id
    /// (the caller must then pick a set).
    pub fn new(def: &'a ControllerDef, bus: Option<&str>) -> Result<Self, String> {
        let sets: Vec<&CanBus> = match bus {
            Some(name) => vec![def.bus(name).ok_or_else(|| {
                format!(
                    "{} has no bus set '{name}' (sets: {})",
                    def.id,
                    set_names(def)
                )
            })?],
            None => def.buses.iter().collect(),
        };
        let mut index = HashMap::new();
        for b in sets {
            for f in &b.frames {
                if let Some((other, of)) = index.insert((f.id, f.extended), (b, f)) {
                    let same_layout = serde_json::to_value(&of.signals).ok()
                        == serde_json::to_value(&f.signals).ok();
                    if other.name != b.name && !same_layout {
                        return Err(format!(
                            "{}: id 0x{:X} is defined by sets '{}' and '{}' with different layouts; choose one with --bus ({})",
                            def.id,
                            f.id,
                            other.name,
                            b.name,
                            set_names(def)
                        ));
                    }
                }
            }
        }
        Ok(Self { def, index })
    }

    /// The definition.
    pub fn def(&self) -> &'a ControllerDef {
        self.def
    }

    /// Ids this decoder knows (sorted).
    pub fn ids(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.index.keys().map(|k| k.0).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Decode one frame; `None` for an unknown id or a too-short payload.
    pub fn decode(&self, id: u32, extended: bool, data: &[u8]) -> Option<DecodedFrame> {
        let (b, f) = self.index.get(&(id, extended))?;
        decode_with(b, f, data)
    }
}

fn set_names(def: &ControllerDef) -> String {
    def.buses
        .iter()
        .map(|b| b.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Running statistics of one signal over a log.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SignalStats {
    /// Samples seen.
    pub samples: u64,
    /// Samples marked invalid.
    pub invalid: u64,
    /// Smallest number seen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    /// Largest number seen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Last value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last: Option<Value>,
    /// Count per enum name / flag state / character.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub counts: BTreeMap<String, u64>,
}

impl SignalStats {
    fn push(&mut self, v: &Value) {
        self.samples += 1;
        match v {
            Value::Number(x) => {
                self.min = Some(self.min.map_or(*x, |m| m.min(*x)));
                self.max = Some(self.max.map_or(*x, |m| m.max(*x)));
            }
            Value::Invalid => self.invalid += 1,
            other => *self.counts.entry(other.to_string()).or_default() += 1,
        }
        self.last = Some(v.clone());
    }
}

/// Per-frame statistics.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct FrameStats {
    /// Bus set.
    pub bus: String,
    /// CAN id.
    pub id: u32,
    /// Frames seen.
    pub count: u64,
    /// First/last timestamp, when the log has them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_ts: Option<f64>,
    /// See `first_ts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ts: Option<f64>,
    /// Per signal.
    pub signals: BTreeMap<String, SignalStats>,
}

/// Summary of a capture against one definition.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Summary {
    /// Definition id.
    pub controller: String,
    /// Bus set used (`all` when none was chosen).
    pub bus: String,
    /// Frames in the log.
    pub frames: u64,
    /// Frames decoded.
    pub decoded: u64,
    /// Per frame name.
    pub per_frame: BTreeMap<String, FrameStats>,
    /// The definition's live channels that come from CAN signals.
    pub live: BTreeMap<String, SignalStats>,
}

/// One frame of a log, as the summariser needs it.
#[derive(Debug, Clone, Copy)]
pub struct RawFrame<'d> {
    /// Timestamp, seconds.
    pub ts: Option<f64>,
    /// CAN id.
    pub id: u32,
    /// 29-bit id.
    pub extended: bool,
    /// Payload.
    pub data: &'d [u8],
}

/// Decode every frame and aggregate. Returns the summary and the decoded
/// frames (with their timestamps) in log order.
pub fn summarize<'d>(
    dec: &Decoder<'_>,
    bus_label: &str,
    frames: impl IntoIterator<Item = RawFrame<'d>>,
) -> (Summary, Vec<(Option<f64>, DecodedFrame)>) {
    let def = dec.def();
    let mut s = Summary {
        controller: def.id.clone(),
        bus: bus_label.to_string(),
        ..Default::default()
    };
    // (bus, frame, signal) -> live channel name
    let mut live_map: HashMap<(String, String, String), Vec<String>> = HashMap::new();
    for ch in &def.live {
        if let LiveSource::CanSignal { bus, frame, signal } = &ch.source {
            live_map
                .entry((bus.to_ascii_lowercase(), frame.clone(), signal.clone()))
                .or_default()
                .push(ch.name.clone());
        }
    }
    let mut out = Vec::new();
    for f in frames {
        s.frames += 1;
        let Some(d) = dec.decode(f.id, f.extended, f.data) else {
            continue;
        };
        s.decoded += 1;
        let fs = s
            .per_frame
            .entry(d.frame.clone())
            .or_insert_with(|| FrameStats {
                bus: d.bus.clone(),
                id: d.id,
                ..Default::default()
            });
        fs.count += 1;
        if let Some(t) = f.ts {
            fs.first_ts.get_or_insert(t);
            fs.last_ts = Some(t);
        }
        for sig in &d.signals {
            fs.signals
                .entry(sig.name.clone())
                .or_default()
                .push(&sig.value);
            if let Some(names) = live_map.get(&(
                d.bus.to_ascii_lowercase(),
                d.frame.clone(),
                sig.name.clone(),
            )) {
                for n in names {
                    s.live.entry(n.clone()).or_default().push(&sig.value);
                }
            }
        }
        out.push((f.ts, d));
    }
    (s, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_sets(same_layout: bool) -> ControllerDef {
        let len_b = if same_layout { 8 } else { 4 };
        serde_json::from_value(serde_json::json!({
            "id": "acme-tcu", "name": "Acme", "kind": "tcu", "maker": "Acme", "family": "T",
            "validation": "implemented", "safety": ["s"],
            "sources": [{"title": "t", "license": "MIT", "used_for": "x"}],
            "buses": [
                {"name": "a", "bitrate": 500000, "frames": [{"id": "0x100", "name": "F",
                    "signals": [{"name": "v", "start": 0, "len": 8}]}]},
                {"name": "b", "bitrate": 500000, "frames": [{"id": "0x100", "name": "F",
                    "signals": [{"name": "v", "start": 0, "len": len_b}]}]}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn shared_ids_need_a_set_choice_unless_identical() {
        let d = two_sets(false);
        let err = Decoder::new(&d, None).err().expect("ambiguous");
        assert!(err.contains("--bus") && err.contains("a, b"), "{err}");
        let dec = Decoder::new(&d, Some("b")).unwrap();
        let f = dec.decode(0x100, false, &[0xAB]).unwrap();
        assert_eq!((f.bus.as_str(), f.get("v").unwrap().raw), ("b", 0xB));
        assert!(Decoder::new(&d, Some("c")).is_err());
        // Identical layouts in two sets are not ambiguous.
        assert!(Decoder::new(&two_sets(true), None).is_ok());
    }
}
