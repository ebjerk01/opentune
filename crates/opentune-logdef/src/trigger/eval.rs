//! The trigger evaluator: one round at a time, pure and deterministic.

use super::units::{canonical_unit, convert_units};
use super::{Action, Compare, Condition, TriggerSet};
use crate::live::Sample;
use crate::resilient::LINK_PARAM_ID;
use std::collections::{BTreeSet, HashMap, VecDeque};

/// Three-valued truth: missing data is [`Tri::Unknown`], never false.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tri {
    True,
    False,
    Unknown,
}

/// What a marker row marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerKind {
    /// A recording window starts (first row of the window).
    Start,
    /// A recording window ends (last row of the window).
    End,
    /// A `mark` trigger fired.
    Mark,
}

impl MarkerKind {
    /// The marker's `text` column: `start`, `end` or `mark`.
    pub fn as_str(self) -> &'static str {
        match self {
            MarkerKind::Start => "start",
            MarkerKind::End => "end",
            MarkerKind::Mark => "mark",
        }
    }
}

/// A marker row to insert into the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerRow {
    /// Timestamp (Unix ms) of the round it belongs to.
    pub ts: u64,
    /// What it marks.
    pub kind: MarkerKind,
    /// The trigger(s) concerned (`a+b` when several opened a window).
    pub trigger: String,
    /// Human-readable detail.
    pub note: String,
}

/// One output item of the engine, in log order.
#[derive(Debug, Clone, PartialEq)]
pub enum Emit<R> {
    /// Recording window `window` (1-based) opens; with `split` it goes to a
    /// file of its own.
    Open {
        /// 1-based window number.
        window: u32,
    },
    /// A marker row.
    Marker(MarkerRow),
    /// A logged row, passed through.
    Row(R),
    /// Window `window` closed.
    Close {
        /// 1-based window number.
        window: u32,
    },
}

/// A recording window, as reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowSummary {
    /// 1-based window number.
    pub index: u32,
    /// Timestamp of its first row (Unix ms).
    pub start_ts: u64,
    /// Timestamp of its last row (Unix ms).
    pub end_ts: u64,
    /// Triggers that fired inside it, in first-fire order.
    pub triggers: Vec<String>,
    /// Rows recorded (marker rows not counted).
    pub rows: u64,
    /// The log ended while the window was still open.
    pub cut_by_end: bool,
}

impl WindowSummary {
    /// `end_ts - start_ts`, ms.
    pub fn duration_ms(&self) -> u64 {
        self.end_ts.saturating_sub(self.start_ts)
    }
}

/// What the triggers did over a log.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TriggerReport {
    /// `(trigger name, times fired)`, in set order.
    pub fires: Vec<(String, u32)>,
    /// Recording windows (closed ones, plus the open one at [`finish`]).
    ///
    /// [`finish`]: TriggerEngine::finish
    pub windows: Vec<WindowSummary>,
    /// Marker rows from `mark` triggers.
    pub marks: u32,
    /// Rounds evaluated.
    pub rounds: u64,
    /// Rounds that were link-event markers (every condition unknown).
    pub link_rounds: u64,
    /// Rows dropped because no window covered them (gated logs only).
    pub dropped_rows: u64,
    /// Problems seen while evaluating (e.g. a unit that cannot be
    /// converted), each once.
    pub warnings: Vec<String>,
}

impl TriggerReport {
    /// Total fires across triggers.
    pub fn total_fires(&self) -> u32 {
        self.fires.iter().map(|(_, n)| n).sum()
    }
}

#[derive(Debug, Clone, Default)]
struct State {
    /// When the condition became (continuously) true while idle.
    true_since: Option<u64>,
    /// When the condition became (continuously) false while active.
    false_since: Option<u64>,
    active: bool,
    /// Release time + `post_ms` of the last release.
    post_until: Option<u64>,
    fires: u32,
}

enum Item<R> {
    Row(R),
    Marker(MarkerRow),
}

struct OpenWindow {
    index: u32,
    start_ts: u64,
    last_ts: u64,
    triggers: Vec<String>,
    rows: u64,
}

/// Evaluates a [`TriggerSet`] over a log, one round at a time.
///
/// `R` is the caller's row type (a CSV line, a [`Sample`], …): rows are
/// passed through, buffered for the pre-trigger window, or dropped, never
/// inspected. Feed each round with [`push`](Self::push) and end with
/// [`finish`](Self::finish); apply the returned [`Emit`]s in order.
///
/// Guarantees (checked by the fuzz test): windows never overlap and never
/// run backwards, every opened window is closed exactly once, and the
/// pre-trigger buffer never holds more than `max_pre_rows` rows between
/// calls.
pub struct TriggerEngine<R> {
    set: TriggerSet,
    st: Vec<State>,
    gated: bool,
    max_pre_ms: u64,
    pre: VecDeque<(u64, Item<R>)>,
    pre_rows: usize,
    open: Option<OpenWindow>,
    next_window: u32,
    windows: Vec<WindowSummary>,
    last_ts: Option<u64>,
    marks: u32,
    rounds: u64,
    link_rounds: u64,
    dropped_rows: u64,
    warnings: BTreeSet<String>,
}

impl<R> TriggerEngine<R> {
    /// An engine over `set` (validate it first: [`TriggerSet::check`]).
    pub fn new(set: TriggerSet) -> Self {
        let gated = set.gates();
        let max_pre_ms = set
            .triggers
            .iter()
            .filter(|t| t.action.gates())
            .map(|t| t.pre_ms)
            .max()
            .unwrap_or(0);
        Self {
            st: vec![State::default(); set.triggers.len()],
            set,
            gated,
            max_pre_ms,
            pre: VecDeque::new(),
            pre_rows: 0,
            open: None,
            next_window: 0,
            windows: Vec::new(),
            last_ts: None,
            marks: 0,
            rounds: 0,
            link_rounds: 0,
            dropped_rows: 0,
            warnings: BTreeSet::new(),
        }
    }

    /// The set being evaluated.
    pub fn set(&self) -> &TriggerSet {
        &self.set
    }

    /// Whether the log is gated (some trigger records or splits).
    pub fn gated(&self) -> bool {
        self.gated
    }

    /// Rows currently held in the pre-trigger buffer.
    pub fn buffered_rows(&self) -> usize {
        self.pre_rows
    }

    /// Whether a recording window is open.
    pub fn window_open(&self) -> bool {
        self.open.is_some()
    }

    /// Feed one round: its timestamp (Unix ms), its samples (what the
    /// conditions look at) and its rows (what is written). A round whose
    /// samples include a link marker ([`LINK_PARAM_ID`]) makes every
    /// condition unknown. A timestamp earlier than the previous one is
    /// treated as equal to it.
    pub fn push(&mut self, ts: u64, samples: &[Sample], rows: Vec<R>) -> Vec<Emit<R>> {
        let ts = self.last_ts.map_or(ts, |l| ts.max(l));
        self.last_ts = Some(ts);
        self.rounds += 1;
        let link = samples.iter().any(|s| s.param_id == LINK_PARAM_ID);
        if link {
            self.link_rounds += 1;
        }

        // Evaluate every trigger on this round.
        let index = (!link).then(|| index_samples(samples));
        let mut fired = Vec::new();
        for i in 0..self.set.triggers.len() {
            let v = match &index {
                Some(idx) => eval(&self.set.triggers[i].when, idx, &mut self.warnings),
                None => Tri::Unknown,
            };
            if self.step(i, v, ts) {
                fired.push(i);
            }
        }

        let mut items: Vec<Item<R>> = rows.into_iter().map(Item::Row).collect();
        for &i in &fired {
            let t = &self.set.triggers[i];
            if t.action == Action::Mark {
                self.marks += 1;
                items.push(Item::Marker(MarkerRow {
                    ts,
                    kind: MarkerKind::Mark,
                    trigger: t.name.clone(),
                    note: format!("{} fired: {}", t.name, t.when),
                }));
            }
        }

        let mut out = Vec::new();
        if !self.gated {
            out.extend(items.into_iter().map(into_emit));
            return out;
        }

        let gating: Vec<usize> = fired
            .iter()
            .copied()
            .filter(|&i| self.set.triggers[i].action.gates())
            .collect();

        if self.open.is_some() && !self.covers(ts) {
            self.close(&mut out, false);
        }
        if let Some(w) = self.open.as_mut() {
            for &i in &gating {
                let n = &self.set.triggers[i].name;
                if !w.triggers.contains(n) {
                    w.triggers.push(n.clone());
                }
            }
            for it in items {
                if matches!(it, Item::Row(_)) {
                    w.rows += 1;
                }
                out.push(into_emit(it));
            }
            w.last_ts = ts;
        } else {
            for it in items {
                if matches!(it, Item::Row(_)) {
                    self.pre_rows += 1;
                }
                self.pre.push_back((ts, it));
            }
            if gating.is_empty() {
                self.trim(ts);
                return out;
            }
            let pre_ms = gating
                .iter()
                .map(|&i| self.set.triggers[i].pre_ms)
                .max()
                .unwrap_or(0);
            self.drop_before(ts.saturating_sub(pre_ms));
            self.next_window += 1;
            let start_ts = self.pre.front().map_or(ts, |(t, _)| *t);
            let names: Vec<String> = gating
                .iter()
                .map(|&i| self.set.triggers[i].name.clone())
                .collect();
            let conds: Vec<String> = gating
                .iter()
                .map(|&i| self.set.triggers[i].when.to_string())
                .collect();
            out.push(Emit::Open {
                window: self.next_window,
            });
            out.push(Emit::Marker(MarkerRow {
                ts: start_ts,
                kind: MarkerKind::Start,
                trigger: names.join("+"),
                note: format!(
                    "window {} start: {} fired at +{} ms: {}",
                    self.next_window,
                    names.join("+"),
                    ts - start_ts,
                    conds.join(" ; ")
                ),
            }));
            let mut rows = 0;
            for (_, it) in self.pre.drain(..) {
                if matches!(it, Item::Row(_)) {
                    rows += 1;
                }
                out.push(into_emit(it));
            }
            self.pre_rows = 0;
            self.open = Some(OpenWindow {
                index: self.next_window,
                start_ts,
                last_ts: ts,
                triggers: names,
                rows,
            });
        }
        // Nothing can extend the window past this round: close it now.
        if self.open.is_some() && self.ends_at(ts) {
            self.close(&mut out, false);
        }
        out
    }

    /// End of the log: close an open window (its end marker says the log
    /// ended) and drop the pre-trigger buffer.
    pub fn finish(&mut self) -> Vec<Emit<R>> {
        let mut out = Vec::new();
        if self.open.is_some() {
            self.close(&mut out, true);
        }
        self.dropped_rows += self.pre_rows as u64;
        self.pre.clear();
        self.pre_rows = 0;
        out
    }

    /// The report so far (windows closed so far; call after
    /// [`finish`](Self::finish) for the whole log).
    pub fn report(&self) -> TriggerReport {
        TriggerReport {
            fires: self
                .set
                .triggers
                .iter()
                .zip(&self.st)
                .map(|(t, s)| (t.name.clone(), s.fires))
                .collect(),
            windows: self.windows.clone(),
            marks: self.marks,
            rounds: self.rounds,
            link_rounds: self.link_rounds,
            dropped_rows: self.dropped_rows,
            warnings: self.warnings.iter().cloned().collect(),
        }
    }

    // ---- internals ----

    /// Advance trigger `i`'s hold-time state machine; `true` when it fires.
    fn step(&mut self, i: usize, v: Tri, ts: u64) -> bool {
        let t = &self.set.triggers[i];
        let s = &mut self.st[i];
        if !s.active {
            if v == Tri::True {
                let since = *s.true_since.get_or_insert(ts);
                if ts - since >= t.arm_ms {
                    s.active = true;
                    s.true_since = None;
                    s.false_since = None;
                    s.post_until = None;
                    s.fires += 1;
                    return true;
                }
            } else {
                // False or unknown: the arming streak is broken.
                s.true_since = None;
            }
        } else if v == Tri::False {
            let since = *s.false_since.get_or_insert(ts);
            if ts - since >= t.release_ms {
                s.active = false;
                s.false_since = None;
                s.post_until = Some(ts.saturating_add(t.post_ms));
            }
        } else {
            // True or unknown: the release streak is broken.
            s.false_since = None;
        }
        false
    }

    fn gating_states(&self) -> impl Iterator<Item = &State> {
        self.set
            .triggers
            .iter()
            .zip(&self.st)
            .filter(|(t, _)| t.action.gates())
            .map(|(_, s)| s)
    }

    /// Whether a round at `ts` belongs to the open window.
    fn covers(&self, ts: u64) -> bool {
        self.gating_states()
            .any(|s| s.active || s.post_until.is_some_and(|p| p >= ts))
    }

    /// Whether no round after `ts` can belong to the open window.
    fn ends_at(&self, ts: u64) -> bool {
        !self
            .gating_states()
            .any(|s| s.active || s.post_until.is_some_and(|p| p > ts))
    }

    fn close(&mut self, out: &mut Vec<Emit<R>>, by_end: bool) {
        let Some(w) = self.open.take() else {
            return;
        };
        let dur = w.last_ts - w.start_ts;
        out.push(Emit::Marker(MarkerRow {
            ts: w.last_ts,
            kind: MarkerKind::End,
            trigger: w.triggers.join("+"),
            note: format!(
                "window {} end: {:.3} s, {} row(s){}",
                w.index,
                dur as f64 / 1000.0,
                w.rows,
                if by_end { ", log ended" } else { "" }
            ),
        }));
        out.push(Emit::Close { window: w.index });
        self.windows.push(WindowSummary {
            index: w.index,
            start_ts: w.start_ts,
            end_ts: w.last_ts,
            triggers: w.triggers,
            rows: w.rows,
            cut_by_end: by_end,
        });
        for s in &mut self.st {
            if !s.active {
                s.post_until = None;
            }
        }
    }

    fn drop_front(&mut self) {
        if let Some((_, it)) = self.pre.pop_front() {
            if matches!(it, Item::Row(_)) {
                self.pre_rows -= 1;
                self.dropped_rows += 1;
            }
        }
    }

    fn drop_before(&mut self, from: u64) {
        while self.pre.front().is_some_and(|(t, _)| *t < from) {
            self.drop_front();
        }
    }

    /// Keep at most `max_pre_ms` of history and `max_pre_rows` rows.
    fn trim(&mut self, ts: u64) {
        self.drop_before(ts.saturating_sub(self.max_pre_ms));
        while self.pre_rows > self.set.max_pre_rows {
            self.drop_front();
        }
        // A mark row follows the rows of its round: at the front of the
        // buffer those rows are gone, so it goes too.
        while matches!(self.pre.front(), Some((_, Item::Marker(_)))) {
            self.pre.pop_front();
        }
    }
}

fn into_emit<R>(it: Item<R>) -> Emit<R> {
    match it {
        Item::Row(r) => Emit::Row(r),
        Item::Marker(m) => Emit::Marker(m),
    }
}

/// The round's samples by `param_id` (exact) and lower-cased id.
struct RoundIndex<'a> {
    exact: HashMap<&'a str, &'a Sample>,
    folded: HashMap<String, &'a Sample>,
}

fn index_samples(samples: &[Sample]) -> RoundIndex<'_> {
    let mut exact = HashMap::with_capacity(samples.len());
    let mut folded = HashMap::with_capacity(samples.len());
    for s in samples {
        exact.entry(s.param_id.as_str()).or_insert(s);
        folded.entry(s.param_id.to_ascii_lowercase()).or_insert(s);
    }
    RoundIndex { exact, folded }
}

fn eval(c: &Condition, idx: &RoundIndex<'_>, warn: &mut BTreeSet<String>) -> Tri {
    match c {
        Condition::All { all } => {
            let mut unknown = false;
            for m in all {
                match eval(m, idx, warn) {
                    Tri::False => return Tri::False,
                    Tri::Unknown => unknown = true,
                    Tri::True => {}
                }
            }
            if unknown || all.is_empty() {
                Tri::Unknown
            } else {
                Tri::True
            }
        }
        Condition::Any { any } => {
            let mut unknown = false;
            for m in any {
                match eval(m, idx, warn) {
                    Tri::True => return Tri::True,
                    Tri::Unknown => unknown = true,
                    Tri::False => {}
                }
            }
            if unknown || any.is_empty() {
                Tri::Unknown
            } else {
                Tri::False
            }
        }
        Condition::Compare(c) => compare(c, idx, warn),
    }
}

fn compare(c: &Compare, idx: &RoundIndex<'_>, warn: &mut BTreeSet<String>) -> Tri {
    let Some(s) = c.channel.ids().into_iter().find_map(|id| {
        idx.exact
            .get(id)
            .or_else(|| idx.folded.get(&id.to_ascii_lowercase()))
            .copied()
    }) else {
        return Tri::Unknown;
    };
    let Some(v) = s.value.filter(|v| v.is_finite()) else {
        return Tri::Unknown;
    };
    let v = if c.units.is_empty() || canonical_unit(&s.units).is_empty() {
        v
    } else {
        match convert_units(v, &s.units, &c.units) {
            Some(v) => v,
            None => {
                warn.insert(format!(
                    "channel '{}' is logged in '{}', which cannot be compared with '{}': \
                     the comparison stays unknown",
                    s.param_id, s.units, c.units
                ));
                return Tri::Unknown;
            }
        }
    };
    if c.op.apply(v, c.value) {
        Tri::True
    } else {
        Tri::False
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trigger::{ChannelRef, Compare, Op, Trigger};

    fn s(id: &str, v: Option<f64>, units: &str) -> Sample {
        Sample {
            param_id: id.into(),
            name: id.into(),
            raw: None,
            value: v,
            units: units.into(),
            note: String::new(),
            text: None,
        }
    }

    fn link() -> Sample {
        let mut l = s(LINK_PARAM_ID, None, "");
        l.text = Some("lost".into());
        l
    }

    fn gt(ch: &str, v: f64, units: &str) -> Condition {
        Condition::Compare(Compare {
            channel: ChannelRef::One(ch.into()),
            op: Op::Gt,
            value: v,
            units: units.into(),
        })
    }

    fn trig(name: &str, action: Action, when: Condition, hold: (u64, u64, u64, u64)) -> Trigger {
        Trigger {
            name: name.into(),
            description: String::new(),
            action,
            when,
            arm_ms: hold.0,
            release_ms: hold.1,
            pre_ms: hold.2,
            post_ms: hold.3,
        }
    }

    fn set(t: Vec<Trigger>) -> TriggerSet {
        TriggerSet {
            format: super::super::TRIGGER_FORMAT.into(),
            version: 1,
            id: String::new(),
            name: String::new(),
            description: String::new(),
            max_pre_rows: 1000,
            triggers: t,
        }
    }

    /// Run rounds of a single `rpm` value (`None` = missing, `Some(NAN)` = link
    /// lost) at 100 ms steps; rows are the round index. Returns the emits as
    /// compact strings.
    fn run(e: &mut TriggerEngine<u64>, rpm: &[Option<f64>]) -> Vec<String> {
        let mut out = Vec::new();
        for (i, v) in rpm.iter().enumerate() {
            let ts = 1000 + i as u64 * 100;
            let samples = match v {
                Some(x) if x.is_nan() => vec![link()],
                _ => vec![s("rpm", *v, "rpm")],
            };
            out.extend(e.push(ts, &samples, vec![i as u64]).into_iter().map(fmt));
        }
        out.extend(e.finish().into_iter().map(fmt));
        out
    }

    fn fmt(e: Emit<u64>) -> String {
        match e {
            Emit::Open { window } => format!("open{window}"),
            Emit::Close { window } => format!("close{window}"),
            Emit::Row(r) => r.to_string(),
            Emit::Marker(m) => format!("{}@{}", m.kind.as_str(), m.ts),
        }
    }

    const HI: Option<f64> = Some(4000.0);
    const LO: Option<f64> = Some(1000.0);
    const LINK: Option<f64> = Some(f64::NAN);

    #[test]
    fn arm_hold_needs_continuous_truth() {
        // arm 200 ms: true at rounds 1,2 (100 ms) then false, then 3 trues.
        let mut e = TriggerEngine::new(set(vec![trig(
            "t",
            Action::Record,
            gt("rpm", 3000.0, "rpm"),
            (200, 0, 0, 0),
        )]));
        let out = run(&mut e, &[LO, HI, HI, LO, HI, HI, HI, LO, LO]);
        // fires at round 6 (1400+200=1600), releases at round 7 (1700).
        assert_eq!(out, ["open1", "start@1600", "6", "7", "end@1700", "close1"]);
        let r = e.report();
        assert_eq!(r.fires, vec![("t".to_string(), 1)]);
        assert_eq!(r.windows.len(), 1);
        assert_eq!(r.windows[0].duration_ms(), 100);
        assert_eq!(r.dropped_rows, 7);
    }

    #[test]
    fn release_hold_is_hysteresis() {
        // release 200 ms: one false round in the middle does not release.
        let mut e = TriggerEngine::new(set(vec![trig(
            "t",
            Action::Record,
            gt("rpm", 3000.0, "rpm"),
            (0, 200, 0, 0),
        )]));
        let out = run(&mut e, &[HI, LO, HI, LO, LO, LO, LO]);
        assert_eq!(
            out,
            [
                "open1",
                "start@1000",
                "0",
                "1",
                "2",
                "3",
                "4",
                "5",
                "end@1500",
                "close1"
            ]
        );
        assert_eq!(e.report().total_fires(), 1);
    }

    #[test]
    fn pre_and_post_windows() {
        let mut e = TriggerEngine::new(set(vec![trig(
            "t",
            Action::Record,
            gt("rpm", 3000.0, "rpm"),
            (0, 0, 200, 200),
        )]));
        let out = run(&mut e, &[LO, LO, LO, LO, HI, HI, LO, LO, LO, LO, LO]);
        // fire at 1400: pre from 1200 (rounds 2,3); release at 1600 (round
        // 6), post until 1800 (rounds 7, 8).
        assert_eq!(
            out,
            [
                "open1",
                "start@1200",
                "2",
                "3",
                "4",
                "5",
                "6",
                "7",
                "8",
                "end@1800",
                "close1"
            ]
        );
        let w = &e.report().windows[0];
        assert_eq!((w.start_ts, w.end_ts, w.rows), (1200, 1800, 7));
    }

    #[test]
    fn refire_in_post_window_extends_and_next_window_does_not_overlap() {
        let mut e = TriggerEngine::new(set(vec![trig(
            "t",
            Action::Record,
            gt("rpm", 3000.0, "rpm"),
            (0, 0, 300, 100),
        )]));
        let out = run(&mut e, &[HI, LO, HI, LO, LO, LO, HI, LO]);
        // window 1: 0..=3 (refire at 2 inside post), post ends 1400 -> closes
        // after round 4; window 2 fires at round 6, pre reaches back to
        // round 5 only (4 is already recorded).
        assert_eq!(
            out,
            [
                "open1",
                "start@1000",
                "0",
                "1",
                "2",
                "3",
                "4",
                "end@1400",
                "close1",
                "open2",
                "start@1500",
                "5",
                "6",
                "7",
                "end@1700",
                "close2"
            ]
        );
        assert_eq!(e.report().total_fires(), 3);
    }

    #[test]
    fn unknown_values_and_link_rows_never_fire_or_release() {
        let mut e = TriggerEngine::new(set(vec![trig(
            "t",
            Action::Record,
            gt("rpm", 3000.0, "rpm"),
            (100, 100, 0, 0),
        )]));
        // HI, missing, HI: the arming streak restarts after the gap, so it
        // fires at round 3 (2 + 100 ms), not round 2.
        let out = run(&mut e, &[HI, None, HI, HI, LINK, LINK, LO, None, LO, LO]);
        assert_eq!(
            out,
            [
                "open1",
                "start@1300",
                "3",
                "4",
                "5",
                "6",
                "7",
                "8",
                "9",
                "end@1900",
                "close1"
            ]
        );
        // The link rows did not release the window; the missing value broke
        // the release streak (6 false, 7 unknown, 8+9 false -> release at 9).
        let r = e.report();
        assert_eq!(r.link_rounds, 2);
        assert_eq!(r.total_fires(), 1);
        // Only unknowns and link rows: nothing fires at all.
        let mut e = TriggerEngine::new(set(vec![trig(
            "t",
            Action::Record,
            gt("rpm", 3000.0, "rpm"),
            (0, 0, 0, 0),
        )]));
        assert!(run(&mut e, &[None, LINK, None, LINK]).is_empty());
        assert_eq!(e.report().dropped_rows, 4);
    }

    #[test]
    fn unit_conversion_and_mismatch() {
        let mut e: TriggerEngine<()> = TriggerEngine::new(set(vec![
            trig("hot", Action::Mark, gt("ect", 220.0, "°F"), (0, 0, 0, 0)),
            trig("bad", Action::Mark, gt("ect", 1.0, "bar"), (0, 0, 0, 0)),
        ]));
        let out = e.push(0, &[s("ect", Some(106.0), "°C")], vec![]);
        assert!(
            matches!(&out[..], [Emit::Marker(m)] if m.trigger == "hot"),
            "{out:?}"
        );
        let r = e.report();
        assert_eq!(r.marks, 1);
        assert_eq!(r.warnings.len(), 1);
        assert!(r.warnings[0].contains("'bar'"), "{:?}", r.warnings);
    }

    #[test]
    fn mark_triggers_pass_everything_and_fire_once_per_episode() {
        let mut e = TriggerEngine::new(set(vec![trig(
            "m",
            Action::Mark,
            gt("rpm", 3000.0, "rpm"),
            (0, 0, 0, 0),
        )]));
        let out = run(&mut e, &[LO, HI, HI, LO, HI]);
        assert_eq!(out, ["0", "1", "mark@1100", "2", "3", "4", "mark@1400"]);
    }

    #[test]
    fn several_triggers_share_one_gate() {
        let any = |a, b| Condition::Any {
            any: vec![gt("rpm", a, ""), gt("rpm", b, "")],
        };
        let mut e = TriggerEngine::new(set(vec![
            trig("a", Action::Record, gt("rpm", 3000.0, ""), (0, 0, 0, 0)),
            trig("b", Action::Split, any(3500.0, 9999.0), (0, 0, 0, 100)),
            trig("m", Action::Mark, gt("rpm", 3900.0, "rpm"), (0, 0, 0, 0)),
        ]));
        let out = run(&mut e, &[LO, HI, LO, LO, LO]);
        assert_eq!(
            out,
            // b's post window (release at 1200 + 100 ms) keeps round 3.
            [
                "open1",
                "start@1100",
                "1",
                "mark@1100",
                "2",
                "3",
                "end@1300",
                "close1"
            ]
        );
        let r = e.report();
        assert_eq!(r.windows[0].triggers, ["a", "b"]);
        assert_eq!(r.total_fires(), 3);
    }

    #[test]
    fn pre_buffer_is_bounded_by_rows() {
        let mut t = set(vec![trig(
            "t",
            Action::Record,
            gt("rpm", 3000.0, ""),
            (0, 0, 600_000, 0),
        )]);
        t.max_pre_rows = 5;
        let mut e = TriggerEngine::new(t);
        for i in 0..50u64 {
            e.push(i, &[s("rpm", Some(0.0), "")], vec![i, i]);
            assert!(e.buffered_rows() <= 5);
        }
        let out = e.push(50, &[s("rpm", Some(5000.0), "")], vec![50]);
        let rows: Vec<u64> = out
            .iter()
            .filter_map(|e| match e {
                Emit::Row(r) => Some(*r),
                _ => None,
            })
            .collect();
        // The 5 newest history rows survive, then the firing round.
        assert_eq!(rows, [47, 48, 48, 49, 49, 50]);
    }

    #[test]
    fn time_going_backwards_is_clamped() {
        let mut e = TriggerEngine::new(set(vec![trig(
            "t",
            Action::Record,
            gt("rpm", 3000.0, ""),
            (0, 0, 0, 0),
        )]));
        e.push(5000, &[s("rpm", Some(5000.0), "")], vec![0u64]);
        e.push(10, &[s("rpm", Some(0.0), "")], vec![1]);
        e.finish();
        let w = &e.report().windows[0];
        assert_eq!((w.start_ts, w.end_ts), (5000, 5000));
    }
}
