//! Structural, honesty and link checks for controller definitions.
//!
//! [`ControllerDef::validate`] needs nothing but the definition;
//! [`ControllerDef::validate_links`] also checks the ids it points at (ecudefs,
//! database units, checksum algorithms) against a [`LinkContext`] the caller
//! fills from the repository (the CLI and the repository test do).
use crate::signal::bytes_needed;
use crate::{ControllerDef, DiagChannel, LiveSource, SignalKind, TableAxis, TableDef};
use opentune_core::ecudef::EcuDef;
use opentune_core::Protocol;
use std::collections::HashSet;

/// How serious an [`Issue`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Must be fixed; CI fails.
    Error,
    /// Worth a reviewer's look.
    Warn,
}

impl Severity {
    /// `ERROR` / `WARN`.
    pub fn tag(self) -> &'static str {
        match self {
            Severity::Error => "ERROR",
            Severity::Warn => "WARN",
        }
    }
}

/// One finding.
#[derive(Debug, Clone)]
pub struct Issue {
    /// Severity.
    pub severity: Severity,
    /// Stable rule code (e.g. `signal.layout`).
    pub code: &'static str,
    /// Where (e.g. `bus 'pq' frame 'Allrad_1' signal 'temp'`).
    pub location: String,
    /// Explanation.
    pub message: String,
}

impl std::fmt::Display for Issue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.location.is_empty() {
            write!(
                f,
                "{} [{}] {}",
                self.severity.tag(),
                self.code,
                self.message
            )
        } else {
            write!(
                f,
                "{} [{}] {}: {}",
                self.severity.tag(),
                self.code,
                self.location,
                self.message
            )
        }
    }
}

/// True if any issue is an error.
pub fn has_errors(issues: &[Issue]) -> bool {
    issues.iter().any(|i| i.severity == Severity::Error)
}

struct Out(Vec<Issue>);

impl Out {
    fn err(&mut self, code: &'static str, loc: impl Into<String>, msg: impl Into<String>) {
        self.0.push(Issue {
            severity: Severity::Error,
            code,
            location: loc.into(),
            message: msg.into(),
        });
    }
    fn warn(&mut self, code: &'static str, loc: impl Into<String>, msg: impl Into<String>) {
        self.0.push(Issue {
            severity: Severity::Warn,
            code,
            location: loc.into(),
            message: msg.into(),
        });
    }
}

fn valid_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('-').collect();
    parts.len() >= 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

fn fits(v: u64, len: u8) -> bool {
    len >= 64 || v < (1u64 << len)
}

/// What [`ControllerDef::validate_links`] checks references against.
#[derive(Debug, Clone, Default)]
pub struct LinkContext {
    /// Every ecudef in the repository / pack.
    pub ecudefs: Vec<EcuDef>,
    /// Every `database/units/` id.
    pub unit_ids: Vec<String>,
    /// Every checksum algorithm id (incl. aliases) `opentune-checksum` knows.
    pub checksum_ids: Vec<String>,
}

fn check_diag(o: &mut Out, i: usize, d: &DiagChannel) {
    let loc = format!("diagnostics[{i}] '{}'", d.name);
    if d.name.trim().is_empty() {
        o.err("diag.name", &loc, "channel needs a name");
    }
    if d.bitrate == 0 {
        o.err("diag.bitrate", &loc, "bitrate must be > 0");
    }
    let max_id = if d.extended_ids { 0x1FFF_FFFF } else { 0x7FF };
    for (what, v) in [("tx_id", d.tx_id), ("rx_id", d.rx_id)] {
        if let Some(v) = v {
            if v > max_id {
                o.err(
                    "diag.can_id",
                    &loc,
                    format!(
                        "{what} 0x{v:X} is out of range (extended_ids = {})",
                        d.extended_ids
                    ),
                );
            }
        }
    }
    match d.transport {
        Protocol::IsoTp if d.tx_id.is_none() || d.rx_id.is_none() => {
            o.err("diag.isotp_ids", &loc, "iso_tp needs tx_id and rx_id")
        }
        Protocol::Vwtp20 if d.tp20_address.is_none() => {
            o.err("diag.tp20_address", &loc, "vwtp20 needs tp20_address")
        }
        Protocol::Iso14230Kline | Protocol::Iso9141 if d.kline_address.is_none() => o.warn(
            "diag.kline_address",
            &loc,
            "K-line channel without kline_address",
        ),
        _ => {}
    }
    if let Some(req) = &d.identification {
        let ok =
            !req.is_empty() && req.len() % 2 == 0 && req.bytes().all(|b| b.is_ascii_hexdigit());
        if !ok {
            o.err(
                "diag.identification",
                &loc,
                format!("identification '{req}' must be an even-length hex string"),
            );
        }
    }
}

fn check_axis(o: &mut Out, loc: &str, axis: &TableAxis, want: u32, dim: &str) {
    let kinds = [
        !axis.values.is_empty(),
        !axis.labels.is_empty(),
        !axis.derived.is_empty(),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    if kinds != 1 {
        o.err(
            "table.axis_kind",
            loc,
            format!("{dim} axis must give exactly one of values, labels or derived"),
        );
    }
    if !axis.derived.is_empty() && axis.count.is_none() {
        o.err(
            "table.axis_count",
            loc,
            format!("{dim} axis is derived but has no count"),
        );
    }
    if axis.len() != want {
        o.err(
            "table.axis_len",
            loc,
            format!(
                "{dim} axis has {} breakpoints, table has {want}",
                axis.len()
            ),
        );
    }
    if axis.values.windows(2).any(|w| w[1] <= w[0]) {
        o.warn(
            "table.axis_monotonic",
            loc,
            format!("{dim} axis values are not strictly increasing"),
        );
    }
    if axis.values.iter().any(|v| !v.is_finite()) {
        o.err(
            "table.axis_value",
            loc,
            format!("{dim} axis has a non-finite value"),
        );
    }
}

fn check_table(o: &mut Out, t: &TableDef, ecudefs: &[String]) {
    let loc = format!("table '{}'", t.name);
    if t.rows == 0 || t.cols == 0 {
        o.err("table.dims", &loc, "rows and cols must be >= 1");
    }
    check_axis(o, &loc, &t.x_axis, t.cols, "x");
    match &t.y_axis {
        Some(y) => check_axis(o, &loc, y, t.rows, "y"),
        None if t.rows > 1 => o.err("table.y_axis", &loc, "a table with rows > 1 needs a y_axis"),
        None => {}
    }
    let structure_only = t.confidence == crate::Confidence::StructureOnly;
    if structure_only && (t.storage.is_some() || t.ecudef_map.is_some()) {
        o.err(
            "table.confidence",
            &loc,
            "structure_only tables cannot claim a storage location",
        );
    }
    if !structure_only && t.storage.is_none() && t.ecudef_map.is_none() {
        o.err(
            "table.confidence",
            &loc,
            "a table without storage or ecudef_map must be confidence structure_only",
        );
    }
    if let Some(s) = &t.storage {
        if !s.factor.is_finite() || s.factor == 0.0 {
            o.err(
                "table.storage",
                &loc,
                "storage factor must be finite and non-zero",
            );
        }
    }
    if let Some(m) = &t.ecudef_map {
        if !ecudefs.iter().any(|e| e == &m.ecudef) {
            o.err(
                "table.ecudef_map",
                &loc,
                format!("ecudef_map names '{}', which is not in ecudefs", m.ecudef),
            );
        }
    }
    if t.source.trim().is_empty() && t.ecudef_map.is_none() {
        o.warn("table.source", &loc, "table has no source");
    }
}

impl ControllerDef {
    /// Check the definition on its own. See `docs/CONTROLLER-DEFS.md` for the rules.
    pub fn validate(&self) -> Vec<Issue> {
        let mut o = Out(Vec::new());
        if !valid_id(&self.id) {
            o.err(
                "id.format",
                "",
                format!(
                    "id '{}' must be lowercase <maker>-<family>[-<variant>] (a-z, 0-9, '-')",
                    self.id
                ),
            );
        }
        for (what, v) in [
            ("name", &self.name),
            ("maker", &self.maker),
            ("family", &self.family),
        ] {
            if v.trim().is_empty() {
                o.err("identity.empty", "", format!("{what} is empty"));
            }
        }
        if self.safety.iter().all(|s| s.trim().is_empty()) {
            o.err("safety.missing", "", "at least one safety note is required");
        }
        if self.sources.is_empty() {
            o.err(
                "sources.missing",
                "",
                "at least one source (with licence) is required",
            );
        }
        for (i, s) in self.sources.iter().enumerate() {
            if s.title.trim().is_empty()
                || s.license.trim().is_empty()
                || s.used_for.trim().is_empty()
            {
                o.err(
                    "sources.incomplete",
                    format!("sources[{i}]"),
                    "title, license and used_for are required",
                );
            }
        }
        if self.identify.is_empty() {
            o.warn(
                "identify.empty",
                "",
                "no identify keys: the unit cannot be matched automatically",
            );
        }
        if let Some(c) = &self.checksum {
            if c.trim().is_empty() {
                o.err("checksum.empty", "", "checksum must be an id or null");
            }
        }
        let mut seen = HashSet::new();
        for e in &self.ecudefs {
            if !seen.insert(e) {
                o.err(
                    "ecudefs.duplicate",
                    "",
                    format!("ecudef '{e}' listed twice"),
                );
            }
        }
        for (i, d) in self.diagnostics.iter().enumerate() {
            check_diag(&mut o, i, d);
        }

        // --- CAN sets -----------------------------------------------------
        let mut bus_names = HashSet::new();
        for b in &self.buses {
            let bloc = format!("bus '{}'", b.name);
            if b.name.is_empty() || b.name != b.name.to_ascii_lowercase() || b.name.contains(' ') {
                o.err(
                    "bus.name",
                    &bloc,
                    "bus set names are lowercase, without spaces",
                );
            }
            if !bus_names.insert(b.name.to_ascii_lowercase()) {
                o.err("bus.duplicate", &bloc, "bus set defined twice");
            }
            if b.bitrate == 0 {
                o.err("bus.bitrate", &bloc, "bitrate must be > 0");
            }
            let mut ids = HashSet::new();
            let mut fnames = HashSet::new();
            for f in &b.frames {
                let floc = format!("bus '{}' frame '{}' (0x{:X})", b.name, f.name, f.id);
                let max = if f.extended { 0x1FFF_FFFF } else { 0x7FF };
                if f.id > max {
                    o.err("frame.id", &floc, "CAN id out of range");
                }
                if !ids.insert((f.id, f.extended)) {
                    o.err("frame.duplicate_id", &floc, "id defined twice in this set");
                }
                if !fnames.insert(f.name.clone()) {
                    o.err(
                        "frame.duplicate_name",
                        &floc,
                        "frame name used twice in this set",
                    );
                }
                let max_bytes = f.dlc.map(|d| d as usize).unwrap_or(8);
                if let (Some(min), Some(dlc)) = (f.min_len, f.dlc) {
                    if min > dlc {
                        o.err("frame.min_len", &floc, "min_len exceeds dlc");
                    }
                }
                if f.signals.is_empty() {
                    o.warn("frame.empty", &floc, "frame has no signals");
                }
                let mut snames = HashSet::new();
                for s in &f.signals {
                    let sloc = format!("{floc} signal '{}'", s.name);
                    if !snames.insert(s.name.clone()) {
                        o.err(
                            "signal.duplicate",
                            &sloc,
                            "signal name used twice in this frame",
                        );
                    }
                    match bytes_needed(s.start, s.len, s.byte_order) {
                        None => o.err(
                            "signal.layout",
                            &sloc,
                            format!("start {} len {} is not a valid layout", s.start, s.len),
                        ),
                        Some(n) if n > max_bytes => o.err(
                            "signal.layout",
                            &sloc,
                            format!(
                                "needs {n} bytes, frame has {max_bytes} ({:?} order, start {}, len {})",
                                s.byte_order, s.start, s.len
                            ),
                        ),
                        _ => {}
                    }
                    match s.kind {
                        SignalKind::Bool if s.len != 1 => {
                            o.err("signal.kind", &sloc, "bool signals are 1 bit")
                        }
                        SignalKind::Enum if s.values.is_empty() => {
                            o.err("signal.kind", &sloc, "enum signals need values")
                        }
                        SignalKind::Ascii if s.len != 8 => {
                            o.err("signal.kind", &sloc, "ascii signals are 8 bits")
                        }
                        _ => {}
                    }
                    if !s.factor.is_finite() || s.factor == 0.0 || !s.offset.is_finite() {
                        o.err(
                            "signal.scaling",
                            &sloc,
                            "factor must be finite and non-zero, offset finite",
                        );
                    }
                    if !s.curve.is_empty() {
                        if s.curve.len() < 2 {
                            o.err("signal.curve", &sloc, "a curve needs at least two points");
                        }
                        if s.curve.windows(2).any(|w| w[1][0] < w[0][0]) {
                            o.err("signal.curve", &sloc, "curve raw values must be ascending");
                        }
                        if s.factor != 1.0 || s.offset != 0.0 {
                            o.err("signal.curve", &sloc, "use either curve or factor/offset");
                        }
                    }
                    let mut raws = HashSet::new();
                    for v in &s.values {
                        if !fits(v.raw, s.len) {
                            o.err(
                                "signal.value",
                                &sloc,
                                format!("value {} does not fit in {} bits", v.raw, s.len),
                            );
                        }
                        if !raws.insert(v.raw) {
                            o.err(
                                "signal.value",
                                &sloc,
                                format!("value {} listed twice", v.raw),
                            );
                        }
                    }
                    for v in &s.invalid {
                        if !fits(*v, s.len) {
                            o.err(
                                "signal.invalid",
                                &sloc,
                                format!("invalid marker {v} does not fit in {} bits", s.len),
                            );
                        }
                    }
                    if let Some([lo, hi]) = s.raw_range {
                        if lo > hi || !fits(hi, s.len) {
                            o.err(
                                "signal.raw_range",
                                &sloc,
                                "raw_range must be [min, max] within the signal width",
                            );
                        }
                    }
                }
            }
        }

        // --- live channels ----------------------------------------------------
        let mut live_names = HashSet::new();
        let mut kwp_cells = HashSet::new();
        for ch in &self.live {
            let loc = format!("live '{}'", ch.name);
            // Names are unique per bus set (the same channel can come from
            // two alternative sets, e.g. PQ / MQB variants) and among non-CAN sources.
            let scope = match &ch.source {
                LiveSource::CanSignal { bus, .. } => bus.to_ascii_lowercase(),
                _ => String::new(),
            };
            if !live_names.insert((scope, ch.name.clone())) {
                o.err(
                    "live.duplicate",
                    &loc,
                    "live channel name used twice for the same source set",
                );
            }
            match &ch.source {
                LiveSource::CanSignal { bus, frame, signal } => {
                    let found = self
                        .bus(bus)
                        .and_then(|b| b.frames.iter().find(|f| &f.name == frame))
                        .and_then(|f| f.signals.iter().find(|s| &s.name == signal));
                    if found.is_none() {
                        o.err(
                            "live.ref",
                            &loc,
                            format!("no signal '{signal}' in frame '{frame}' of bus '{bus}'"),
                        );
                    }
                }
                LiveSource::UdsDid { len, factor, .. } => {
                    if !(1..=4).contains(len) {
                        o.err("live.uds_len", &loc, "UDS DID value length is 1..=4 bytes");
                    }
                    if !factor.is_finite() || *factor == 0.0 {
                        o.err("live.scaling", &loc, "factor must be finite and non-zero");
                    }
                }
                LiveSource::KwpMeasuringBlock { group, cell } => {
                    // One meaning per cell: a second entry would silently
                    // replace the first when the engine attaches meanings.
                    if !kwp_cells.insert((*group, *cell)) {
                        o.err(
                            "live.kwp_duplicate",
                            &loc,
                            format!("measuring block group {group} cell {cell} is named twice"),
                        );
                    }
                }
            }
        }

        // --- adaptations ----------------------------------------------------
        let mut ad_names = HashSet::new();
        for a in &self.adaptations {
            let loc = format!("adaptation '{}'", a.name);
            if !ad_names.insert(a.name.clone()) {
                o.err("adaptation.duplicate", &loc, "adaptation name used twice");
            }
            if let Some(t) = &a.table {
                if self.table(t).is_none() {
                    o.err("adaptation.table", &loc, format!("no table named '{t}'"));
                }
            }
        }

        // --- tables -----------------------------------------------------------
        let mut tnames = HashSet::new();
        for t in &self.tables {
            if !tnames.insert(t.name.clone()) {
                o.err(
                    "table.duplicate",
                    format!("table '{}'", t.name),
                    "table name used twice",
                );
            }
            check_table(&mut o, t, &self.ecudefs);
        }

        // --- gear sets ------------------------------------------------------
        for g in &self.gearsets {
            let loc = format!("gearset '{}'", g.name);
            if g.gears.is_empty() {
                o.err("gearset.empty", &loc, "gear set has no gears");
            }
            let mut names = HashSet::new();
            for gear in &g.gears {
                if !names.insert(gear.gear.clone()) {
                    o.err(
                        "gearset.duplicate",
                        &loc,
                        format!("gear '{}' listed twice", gear.gear),
                    );
                }
                if !gear.ratio.is_finite() || gear.ratio == 0.0 {
                    o.err(
                        "gearset.ratio",
                        &loc,
                        format!("gear '{}' ratio must be finite and non-zero", gear.gear),
                    );
                }
            }
            let fwd: Vec<f64> = g
                .gears
                .iter()
                .map(|x| x.ratio)
                .filter(|r| *r > 0.0)
                .collect();
            if fwd.windows(2).any(|w| w[1] >= w[0]) {
                o.warn(
                    "gearset.order",
                    &loc,
                    "forward ratios are not strictly decreasing",
                );
            }
        }

        // --- flash ------------------------------------------------------------
        if let Some(f) = &self.flash {
            if f.size > 0 {
                for r in &f.regions {
                    if r.region.address as u64 + r.region.length as u64 > f.size as u64 {
                        o.err(
                            "flash.region",
                            format!("flash region '{}'", r.name),
                            "region runs past the flash size",
                        );
                    }
                }
            }
        }

        if self.diagnostics.is_empty()
            && self.buses.is_empty()
            && self.ecudefs.is_empty()
            && self.flash.is_none()
        {
            o.warn(
                "scope.empty",
                "",
                "no diagnostics, buses, ecudefs or flash layout: nothing to act on",
            );
        }
        o.0
    }

    /// Check that the file stem equals the id.
    pub fn validate_filename(&self, stem: &str) -> Vec<Issue> {
        let mut o = Out(Vec::new());
        if stem != self.id {
            o.err(
                "id.filename",
                "",
                format!(
                    "file '{stem}.json' must be named after the id '{}'",
                    self.id
                ),
            );
        }
        o.0
    }

    /// Check references against the repository.
    pub fn validate_links(&self, ctx: &LinkContext) -> Vec<Issue> {
        let mut o = Out(Vec::new());
        for e in &self.ecudefs {
            match ctx.ecudefs.iter().find(|d| &d.id == e) {
                None => o.err("link.ecudef", "", format!("ecudef '{e}' does not exist")),
                Some(d) => {
                    if let (Some(mine), Some(theirs)) = (&self.checksum, &d.checksum) {
                        if mine != theirs {
                            o.warn(
                                "link.checksum_mismatch",
                                "",
                                format!("checksum '{mine}' differs from ecudef '{e}' ('{theirs}')"),
                            );
                        }
                    }
                }
            }
        }
        for u in &self.units {
            if !ctx.unit_ids.iter().any(|x| x == u) {
                o.err(
                    "link.unit",
                    "",
                    format!("database unit '{u}' does not exist"),
                );
            }
        }
        if let Some(c) = &self.checksum {
            if !ctx.checksum_ids.iter().any(|x| x == c) {
                o.err(
                    "link.checksum",
                    "",
                    format!("checksum '{c}' is not an opentune-checksum id"),
                );
            }
        }
        for t in &self.tables {
            if let Some(m) = &t.ecudef_map {
                if let Some(d) = ctx.ecudefs.iter().find(|d| d.id == m.ecudef) {
                    if !d.maps.iter().any(|x| x.name == m.map) {
                        o.err(
                            "link.ecudef_map",
                            format!("table '{}'", t.name),
                            format!("ecudef '{}' has no map '{}'", m.ecudef, m.map),
                        );
                    }
                }
            }
        }
        o.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ControllerDef;

    fn minimal() -> serde_json::Value {
        serde_json::json!({
            "id": "acme-tcu",
            "name": "Acme TCU",
            "kind": "tcu",
            "maker": "Acme",
            "family": "T1",
            "validation": "implemented",
            "identify": {"part_numbers": ["123"]},
            "diagnostics": [{"name": "uds", "transport": "iso_tp", "layer": "uds",
                "bitrate": 500000, "tx_id": "0x7E1", "rx_id": "0x7E9", "validation": "implemented"}],
            "safety": ["careful"],
            "sources": [{"title": "t", "license": "MIT", "used_for": "x"}]
        })
    }

    fn def(v: serde_json::Value) -> ControllerDef {
        serde_json::from_value(v).unwrap()
    }

    fn codes(d: &ControllerDef) -> Vec<&'static str> {
        d.validate().into_iter().map(|i| i.code).collect()
    }

    #[test]
    fn minimal_is_clean() {
        let d = def(minimal());
        assert!(d.validate().is_empty(), "{:?}", d.validate());
        assert_eq!(d.diagnostics[0].tx_id, Some(0x7E1));
        assert!(d.validate_filename("acme-tcu").is_empty());
        assert!(!d.validate_filename("other").is_empty());
    }

    #[test]
    fn honesty_rules() {
        let mut v = minimal();
        v["safety"] = serde_json::json!([]);
        v["sources"] = serde_json::json!([]);
        v["id"] = serde_json::json!("Acme_TCU");
        let c = codes(&def(v));
        assert!(c.contains(&"safety.missing"));
        assert!(c.contains(&"sources.missing"));
        assert!(c.contains(&"id.format"));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let mut v = minimal();
        v["tx_id"] = serde_json::json!(1);
        assert!(serde_json::from_value::<ControllerDef>(v).is_err());
    }

    #[test]
    fn signal_and_table_rules() {
        let mut v = minimal();
        v["buses"] = serde_json::json!([{
            "name": "main", "bitrate": 500000,
            "frames": [{"id": "0x100", "name": "F", "signals": [
                {"name": "a", "start": 60, "len": 8},
                {"name": "b", "start": 0, "len": 2, "kind": "bool"},
                {"name": "c", "start": 8, "len": 2, "kind": "enum", "values": [{"raw": 4, "name": "x"}]}
            ]}]
        }]);
        v["tables"] = serde_json::json!([{
            "name": "t", "role": "shift_up", "rows": 2, "cols": 3,
            "x_axis": {"name": "pedal", "values": [0, 50]},
            "confidence": "high", "source": "s"
        }]);
        v["live"] = serde_json::json!([{"name": "l", "source": {"kind": "can_signal", "bus": "main", "frame": "F", "signal": "zz"}}]);
        let c = codes(&def(v));
        for want in [
            "signal.layout",
            "signal.kind",
            "signal.value",
            "table.axis_len",
            "table.y_axis",
            "table.confidence",
            "live.ref",
        ] {
            assert!(c.contains(&want), "{want} not in {c:?}");
        }
    }

    #[test]
    fn a_measuring_block_cell_has_one_meaning() {
        // Regression: two live entries for the same (group, cell) validated,
        // and the DSG engine's label map then kept only the last one.
        let mut v = minimal();
        let kwp = |name: &str, group: u8, cell: u8| {
            serde_json::json!({"name": name,
                "source": {"kind": "kwp_measuring_block", "group": group, "cell": cell}})
        };
        v["live"] = serde_json::json!([kwp("a", 1, 0), kwp("b", 1, 1), kwp("c", 2, 0)]);
        assert!(!codes(&def(v.clone())).contains(&"live.kwp_duplicate"));
        v["live"] = serde_json::json!([kwp("a", 1, 0), kwp("b", 1, 0)]);
        assert_eq!(codes(&def(v)), vec!["live.kwp_duplicate"]);
    }

    #[test]
    fn link_rules() {
        let mut v = minimal();
        v["ecudefs"] = serde_json::json!(["nope"]);
        v["units"] = serde_json::json!(["nounit"]);
        v["checksum"] = serde_json::json!("zz");
        let d = def(v);
        let issues = d.validate_links(&LinkContext::default());
        let c: Vec<_> = issues.iter().map(|i| i.code).collect();
        assert!(
            c.contains(&"link.ecudef") && c.contains(&"link.unit") && c.contains(&"link.checksum")
        );
    }
}
