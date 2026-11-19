//! **Datalogging triggers**: watch the logged channels and act on a condition.
//!
//! A [`TriggerSet`] is plain data (JSON, `"format": "opentune-triggers"`), so
//! it can ship in a definition pack (`triggers/*.json`) next to the logger it
//! was written for. Each [`Trigger`] has
//!
//! * a [`Condition`]: comparisons of channel values against constants
//!   ([`Compare`], units-aware), combined with `all` / `any`;
//! * hold times on both edges: the condition must be **true** for `arm_ms`
//!   before the trigger fires, and **false** for `release_ms` before it
//!   releases (hysteresis without a second threshold);
//! * an [`Action`]: `record` (gate the log to windows around the event, with
//!   `pre_ms` of history from a bounded ring buffer and `post_ms` after the
//!   release), `split` (the same, one file per window) or `mark` (a marker
//!   row in the log, nothing gated).
//!
//! The [`TriggerEngine`] evaluates a set one logging round at a time. It is
//! pure and deterministic (timestamps come with the round, no clock, no I/O).
//! Missing data is **unknown**, never false: a channel without a value, a
//! unit that cannot be converted or a link-lost marker row
//! ([`LINK_PARAM_ID`](crate::LINK_PARAM_ID)) neither fires nor releases a
//! trigger, and breaks a running hold time. [`TriggerCsvWriter`] runs the
//! engine over the Live-log CSV a logging loop writes, as a [`std::io::Write`]
//! layer, so the logging loops themselves are unchanged.

use serde::{Deserialize, Serialize};
use std::fmt;

mod csv;
mod eval;
mod units;

pub use csv::{TriggerCsvWriter, TriggerOutput, WindowFactory, MARKER_PARAM_ID};
pub use eval::{Emit, MarkerKind, MarkerRow, TriggerEngine, TriggerReport, WindowSummary};
pub use units::{canonical_unit, convert_units};

/// The `format` tag of a trigger file.
pub const TRIGGER_FORMAT: &str = "opentune-triggers";
/// The trigger-file format version this crate reads.
pub const TRIGGER_VERSION: u32 = 1;
/// Default bound of the pre-trigger ring buffer, in rows.
pub const DEFAULT_MAX_PRE_ROWS: usize = 20_000;
/// Largest allowed pre-trigger bound, in rows.
pub const MAX_PRE_ROWS_LIMIT: usize = 1_000_000;
/// Longest allowed hold time (`arm_ms`, `release_ms`) or post window, ms.
pub const MAX_HOLD_MS: u64 = 3_600_000;
/// Longest allowed pre-trigger window, ms.
pub const MAX_PRE_MS: u64 = 600_000;
/// Deepest allowed `all` / `any` nesting.
pub const MAX_DEPTH: usize = 8;

fn default_format() -> String {
    TRIGGER_FORMAT.into()
}
fn default_version() -> u32 {
    TRIGGER_VERSION
}
fn default_max_pre_rows() -> usize {
    DEFAULT_MAX_PRE_ROWS
}

/// A trigger file: one or more [`Trigger`]s evaluated together.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerSet {
    /// Always [`TRIGGER_FORMAT`].
    #[serde(default = "default_format")]
    pub format: String,
    /// Always [`TRIGGER_VERSION`].
    #[serde(default = "default_version")]
    pub version: u32,
    /// Stable id (a pack lists the file under it; the file stem when empty).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Human-readable name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// What the set is for.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Bound of the pre-trigger ring buffer, in CSV rows (every channel of a
    /// round is a row). History older than this is dropped even when a
    /// trigger's `pre_ms` would reach further.
    #[serde(default = "default_max_pre_rows")]
    pub max_pre_rows: usize,
    /// The triggers.
    pub triggers: Vec<Trigger>,
}

/// One trigger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trigger {
    /// Unique name within the set; written into marker rows.
    pub name: String,
    /// What it is for.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// What happens when it fires.
    pub action: Action,
    /// The condition.
    pub when: Condition,
    /// The condition must be true this long (ms, continuously observed)
    /// before the trigger fires. 0 = on the first true round.
    #[serde(default)]
    pub arm_ms: u64,
    /// The condition must be false this long (ms, continuously observed)
    /// before the trigger releases. 0 = on the first false round.
    #[serde(default)]
    pub release_ms: u64,
    /// `record` / `split`: history kept before the fire, ms.
    #[serde(default)]
    pub pre_ms: u64,
    /// `record` / `split`: recording continues this long after the release,
    /// ms.
    #[serde(default)]
    pub post_ms: u64,
}

/// What a trigger does when it fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Gate the log: only windows around this trigger are recorded.
    Record,
    /// Write a marker row into the log when it fires; nothing is gated.
    Mark,
    /// Like `record`, and every window goes to its own file.
    Split,
}

impl Action {
    /// Whether the action gates the log (`record`, `split`).
    pub fn gates(self) -> bool {
        matches!(self, Action::Record | Action::Split)
    }
}

/// A condition over one round of channel values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Condition {
    /// True when every member is true (false when any is false, unknown
    /// otherwise).
    All {
        /// The members.
        all: Vec<Condition>,
    },
    /// True when any member is true (false when every member is false,
    /// unknown otherwise).
    Any {
        /// The members.
        any: Vec<Condition>,
    },
    /// One comparison.
    Compare(Compare),
}

/// `channel op value [units]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compare {
    /// The channel's `param_id`, or a list of alternative ids: the first one
    /// present in the round is used (one file then works for a logger
    /// definition's ids and `obd read`'s keys alike).
    pub channel: ChannelRef,
    /// The comparison.
    pub op: Op,
    /// The constant, in `units`.
    pub value: f64,
    /// Units of `value`. A channel logged in other units is converted when
    /// the conversion is known (°C/°F/K, km/h/mph/m/s, kPa/hPa/bar/psi/Pa);
    /// otherwise the comparison is unknown. Empty: compare the logged value
    /// as is. A channel logged without units is taken to be in these units.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub units: String,
}

/// One channel id or a list of alternatives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChannelRef {
    /// One id.
    One(String),
    /// Alternative ids, first present wins.
    Any(Vec<String>),
}

impl ChannelRef {
    /// The ids, in preference order.
    pub fn ids(&self) -> Vec<&str> {
        match self {
            ChannelRef::One(s) => vec![s.as_str()],
            ChannelRef::Any(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    /// `>`
    #[serde(rename = ">")]
    Gt,
    /// `>=`
    #[serde(rename = ">=")]
    Ge,
    /// `<`
    #[serde(rename = "<")]
    Lt,
    /// `<=`
    #[serde(rename = "<=")]
    Le,
    /// `==`
    #[serde(rename = "==")]
    Eq,
    /// `!=`
    #[serde(rename = "!=")]
    Ne,
}

impl Op {
    /// Apply the operator.
    pub fn apply(self, a: f64, b: f64) -> bool {
        match self {
            Op::Gt => a > b,
            Op::Ge => a >= b,
            Op::Lt => a < b,
            Op::Le => a <= b,
            Op::Eq => a == b,
            Op::Ne => a != b,
        }
    }
    /// The operator's symbol.
    pub fn symbol(self) -> &'static str {
        match self {
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Eq => "==",
            Op::Ne => "!=",
        }
    }
}

impl fmt::Display for Condition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let join = |f: &mut fmt::Formatter<'_>, v: &[Condition], sep: &str| {
            write!(f, "(")?;
            for (i, c) in v.iter().enumerate() {
                if i > 0 {
                    write!(f, " {sep} ")?;
                }
                write!(f, "{c}")?;
            }
            write!(f, ")")
        };
        match self {
            Condition::All { all } => join(f, all, "&"),
            Condition::Any { any } => join(f, any, "|"),
            Condition::Compare(c) => {
                write!(
                    f,
                    "{} {} {}",
                    c.channel.ids().first().copied().unwrap_or("?"),
                    c.op.symbol(),
                    c.value
                )?;
                if !c.units.is_empty() {
                    write!(f, " {}", c.units)?;
                }
                Ok(())
            }
        }
    }
}

impl Condition {
    /// Every channel reference in the condition.
    pub fn channels(&self) -> Vec<&ChannelRef> {
        let mut out = Vec::new();
        self.walk(&mut |c| out.push(&c.channel));
        out
    }

    fn walk<'a>(&'a self, f: &mut dyn FnMut(&'a Compare)) {
        match self {
            Condition::All { all: v } | Condition::Any { any: v } => {
                v.iter().for_each(|c| c.walk(f))
            }
            Condition::Compare(c) => f(c),
        }
    }

    fn validate(&self, at: &str, depth: usize, errs: &mut Vec<String>) {
        if depth > MAX_DEPTH {
            errs.push(format!("{at}: nested deeper than {MAX_DEPTH} levels"));
            return;
        }
        match self {
            Condition::All { all: v } | Condition::Any { any: v } => {
                if v.is_empty() {
                    errs.push(format!("{at}: an empty all/any list"));
                }
                for (i, c) in v.iter().enumerate() {
                    c.validate(&format!("{at}[{i}]"), depth + 1, errs);
                }
            }
            Condition::Compare(c) => {
                let ids = c.channel.ids();
                if ids.is_empty() {
                    errs.push(format!("{at}: no channel id"));
                }
                for id in ids {
                    if id.trim().is_empty() || id.contains([',', '"', '\n', '\r']) {
                        errs.push(format!(
                            "{at}: channel id {id:?} is empty or holds a comma, quote or line break"
                        ));
                    }
                }
                if !c.value.is_finite() {
                    errs.push(format!("{at}: value must be a finite number"));
                }
                if c.units.contains(['\n', '\r']) {
                    errs.push(format!("{at}: units hold a line break"));
                }
            }
        }
    }
}

impl TriggerSet {
    /// Parse a trigger file (does not validate; see [`validate`](Self::validate)).
    pub fn from_json(s: &str) -> Result<Self, String> {
        serde_json::from_str(s).map_err(|e| e.to_string())
    }

    /// Parse and validate a trigger file.
    pub fn load_json(s: &str) -> Result<Self, String> {
        let set = Self::from_json(s)?;
        set.check()?;
        Ok(set)
    }

    /// Serialise as pretty JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Every problem with the set (empty = valid).
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if self.format != TRIGGER_FORMAT {
            errs.push(format!(
                "format must be \"{TRIGGER_FORMAT}\", not {:?}",
                self.format
            ));
        }
        if self.version != TRIGGER_VERSION {
            errs.push(format!(
                "version {} is not supported (this build reads {TRIGGER_VERSION})",
                self.version
            ));
        }
        if !(1..=MAX_PRE_ROWS_LIMIT).contains(&self.max_pre_rows) {
            errs.push(format!(
                "max_pre_rows must be 1..={MAX_PRE_ROWS_LIMIT}, not {}",
                self.max_pre_rows
            ));
        }
        if self.triggers.is_empty() {
            errs.push("no triggers".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for (i, t) in self.triggers.iter().enumerate() {
            let at = format!("triggers[{i}] ({})", t.name);
            if t.name.trim().is_empty() || t.name.chars().any(char::is_control) {
                errs.push(format!(
                    "triggers[{i}]: name is empty or holds control characters"
                ));
            } else if !seen.insert(t.name.as_str()) {
                errs.push(format!("{at}: duplicate name"));
            }
            for (what, v, max) in [
                ("arm_ms", t.arm_ms, MAX_HOLD_MS),
                ("release_ms", t.release_ms, MAX_HOLD_MS),
                ("post_ms", t.post_ms, MAX_HOLD_MS),
                ("pre_ms", t.pre_ms, MAX_PRE_MS),
            ] {
                if v > max {
                    errs.push(format!("{at}: {what} {v} is above the {max} ms limit"));
                }
            }
            if t.action == Action::Mark && (t.pre_ms > 0 || t.post_ms > 0) {
                errs.push(format!(
                    "{at}: pre_ms/post_ms apply to record/split triggers, not mark"
                ));
            }
            t.when.validate(&format!("{at}.when"), 1, &mut errs);
        }
        errs
    }

    /// [`validate`](Self::validate) as a `Result` (problems joined by `; `).
    pub fn check(&self) -> Result<(), String> {
        let e = self.validate();
        if e.is_empty() {
            Ok(())
        } else {
            Err(e.join("; "))
        }
    }

    /// Whether any trigger gates the log (`record` / `split`).
    pub fn gates(&self) -> bool {
        self.triggers.iter().any(|t| t.action.gates())
    }

    /// Whether any trigger asks for one file per window (`split`).
    pub fn splits(&self) -> bool {
        self.triggers.iter().any(|t| t.action == Action::Split)
    }

    /// Channel references none of whose ids is in `logged` (the channel ids
    /// the log will carry), as `"a|b"` strings: these comparisons will stay
    /// unknown for the whole log.
    pub fn missing_channels(&self, logged: &[&str]) -> Vec<String> {
        let mut out = Vec::new();
        for t in &self.triggers {
            for c in t.when.channels() {
                let ids = c.ids();
                if !ids
                    .iter()
                    .any(|id| logged.iter().any(|l| l == id || l.eq_ignore_ascii_case(id)))
                {
                    let s = ids.join("|");
                    if !out.contains(&s) {
                        out.push(s);
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WOT: &str = r#"{
      "format": "opentune-triggers",
      "version": 1,
      "id": "wot",
      "triggers": [
        {
          "name": "wot-pull",
          "action": "record",
          "when": { "all": [
            { "channel": ["obd01_0c_rpm", "rpm"], "op": ">", "value": 3000, "units": "rpm" },
            { "channel": "throttle_pos", "op": ">=", "value": 90, "units": "%" }
          ] },
          "arm_ms": 200, "release_ms": 500, "pre_ms": 1000, "post_ms": 2000
        },
        {
          "name": "hot",
          "action": "mark",
          "when": { "any": [ { "channel": "coolant_temp", "op": ">", "value": 105, "units": "°C" } ] }
        }
      ]
    }"#;

    #[test]
    fn parses_validates_and_round_trips() {
        let set = TriggerSet::load_json(WOT).unwrap();
        assert_eq!(set.triggers.len(), 2);
        assert_eq!(set.max_pre_rows, DEFAULT_MAX_PRE_ROWS);
        assert!(set.gates() && !set.splits());
        assert_eq!(
            set.triggers[0].when.to_string(),
            "(obd01_0c_rpm > 3000 rpm & throttle_pos >= 90 %)"
        );
        let back = TriggerSet::from_json(&set.to_json()).unwrap();
        assert_eq!(back, set);
        assert_eq!(
            set.missing_channels(&["rpm", "THROTTLE_POS"]),
            vec!["coolant_temp".to_string()]
        );
    }

    #[test]
    fn rejects_bad_sets() {
        let bad = |s: &str| TriggerSet::load_json(s).unwrap_err();
        assert!(bad(r#"{"triggers":[]}"#).contains("no triggers"));
        assert!(bad(r#"{"format":"x","triggers":[{"name":"a","action":"mark","when":{"channel":"c","op":">","value":1}}]}"#)
            .contains("format"));
        let e = bad(r#"{"triggers":[
              {"name":"a","action":"mark","pre_ms":5,"when":{"all":[]}},
              {"name":"a","action":"record","arm_ms":99999999,"when":{"channel":"x,y","op":"<","value":1}}
            ]}"#);
        assert!(e.contains("empty all/any"), "{e}");
        assert!(e.contains("not mark"), "{e}");
        assert!(e.contains("duplicate name"), "{e}");
        assert!(e.contains("arm_ms"), "{e}");
        assert!(e.contains("comma"), "{e}");
        // unknown fields and operators are parse errors
        assert!(!bad(r#"{"triggers":[{"name":"a","action":"mark","when":{"channel":"c","op":"~","value":1}}]}"#).is_empty());
        assert!(bad(r#"{"triggers":[],"extra":1}"#).contains("extra"));
        assert!(bad(r#"{"max_pre_rows":0,"triggers":[{"name":"a","action":"mark","when":{"channel":"c","op":">","value":1}}]}"#)
            .contains("max_pre_rows"));
    }
}
