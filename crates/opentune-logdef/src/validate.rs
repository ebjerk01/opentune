//! Structural validation of a [`LogDef`]: the checks a logger definition must
//! pass before it is logged or shipped in a pack (`packs validate` runs them
//! on every `loggers/*.json`, and `import-romraider-logger` on what it writes).

use std::collections::{HashMap, HashSet};

use crate::expr::LogExpr;
use crate::model::{Addressing, ConversionFit, LogDef, LogParam, ProtocolGroup, ProtocolKind};

impl LogDef {
    /// Every structural problem, one line each; empty when the definition is
    /// sound. Checked per protocol:
    ///
    /// * parameter ids are unique within an ECU group;
    /// * the addressing fits the protocol (a PID only on OBD-II mode 01, a DID
    ///   only on UDS mode 22, a memory address only on a memory-read
    ///   protocol), a memory read is at least one byte and a `bit` lies inside
    ///   it;
    /// * linear conversions have finite factors; expression conversions parse
    ///   in the safe grammar, reference parameters of the same protocol (with
    ///   an existing conversion for `[id:units]`), do not use `x` without an
    ///   address, and calculated parameters do not depend on each other in a
    ///   cycle.
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        for proto in &self.protocols {
            validate_protocol(proto, &mut errs);
        }
        errs
    }
}

fn validate_protocol(proto: &ProtocolGroup, errs: &mut Vec<String>) {
    let all: Vec<&LogParam> = proto.ecus.iter().flat_map(|g| g.params.iter()).collect();
    let by_id: HashMap<&str, &LogParam> = all.iter().rev().map(|p| (p.id.as_str(), *p)).collect();
    for group in &proto.ecus {
        let mut seen = HashSet::new();
        for p in &group.params {
            if !seen.insert(p.id.as_str()) {
                errs.push(format!(
                    "{}: duplicate parameter id '{}'{}",
                    proto.id,
                    p.id,
                    group
                        .id
                        .as_deref()
                        .map(|g| format!(" in ECU group '{g}'"))
                        .unwrap_or_default()
                ));
            }
        }
    }
    let mut calc_edges: Vec<(&str, Vec<String>)> = Vec::new();
    for p in &all {
        let at = format!("{} / {}", proto.id, p.id);
        check_addressing(&proto.kind, p, &at, errs);
        for (n, c) in p.conversions.iter().enumerate() {
            let at = format!("{at} conversion {} ('{}')", n + 1, c.units);
            match &c.fit {
                ConversionFit::Linear { factor, offset } => {
                    if !factor.is_finite() || !offset.is_finite() {
                        errs.push(format!("{at}: non-finite linear scaling"));
                    }
                }
                ConversionFit::Expression { refs } => {
                    let e = match LogExpr::parse(&c.expr) {
                        Ok(e) => e,
                        Err(why) => {
                            errs.push(format!("{at}: expression '{}': {why}", c.expr));
                            continue;
                        }
                    };
                    if &e.ref_ids() != refs {
                        errs.push(format!(
                            "{at}: refs {refs:?} do not match the expression '{}'",
                            c.expr
                        ));
                    }
                    if e.uses_x() && p.addressing == Addressing::Unknown {
                        errs.push(format!("{at}: uses x but the parameter has no address"));
                    }
                    for r in e.refs() {
                        if r.id == p.id {
                            errs.push(format!("{at}: references itself"));
                            continue;
                        }
                        let Some(dep) = by_id.get(r.id.as_str()) else {
                            errs.push(format!("{at}: references unknown parameter '{}'", r.id));
                            continue;
                        };
                        if let Some(u) = &r.units {
                            if !dep
                                .conversions
                                .iter()
                                .any(|c| c.units.eq_ignore_ascii_case(u))
                            {
                                errs.push(format!("{at}: '{}' has no '{u}' conversion", r.id));
                            }
                        }
                    }
                    if n == 0 && !refs.is_empty() {
                        calc_edges.push((p.id.as_str(), refs.clone()));
                    }
                }
                ConversionFit::NonLinear { .. } => {}
            }
        }
    }
    // Cycles among calculated parameters (depth-first, white/grey/black).
    let edges: HashMap<&str, &Vec<String>> = calc_edges.iter().map(|(k, v)| (*k, v)).collect();
    let mut state: HashMap<&str, u8> = HashMap::new();
    fn visit<'a>(
        n: &'a str,
        edges: &HashMap<&'a str, &'a Vec<String>>,
        state: &mut HashMap<&'a str, u8>,
    ) -> bool {
        match state.get(n) {
            Some(1) => return true,
            Some(2) => return false,
            _ => {}
        }
        state.insert(n, 1);
        if let Some(next) = edges.get(n) {
            for m in next.iter() {
                if visit(m.as_str(), edges, state) {
                    return true;
                }
            }
        }
        state.insert(n, 2);
        false
    }
    for (id, _) in &calc_edges {
        if !state.contains_key(id) && visit(id, &edges, &mut state) {
            errs.push(format!(
                "{} / {id}: calculated parameters depend on each other in a cycle",
                proto.id
            ));
        }
    }
}

fn check_addressing(kind: &ProtocolKind, p: &LogParam, at: &str, errs: &mut Vec<String>) {
    match (&p.addressing, kind) {
        (Addressing::ObdPid { .. }, ProtocolKind::Obd2Mode01)
        | (Addressing::UdsDid { .. }, ProtocolKind::UdsMode22)
        | (Addressing::Unknown, _) => {}
        (Addressing::ObdPid { .. }, _) => errs.push(format!(
            "{at}: an OBD PID outside an OBD-II mode 01 protocol"
        )),
        (Addressing::UdsDid { .. }, _) => {
            errs.push(format!("{at}: a UDS DID outside a UDS mode 22 protocol"))
        }
        (Addressing::Memory { .. }, ProtocolKind::Obd2Mode01 | ProtocolKind::UdsMode22) => {
            errs.push(format!("{at}: a memory address on a PID/DID protocol"))
        }
        (Addressing::Memory { length, bit, .. }, _) => {
            if *length == 0 {
                errs.push(format!("{at}: zero-length memory read"));
            }
            if let Some(b) = bit {
                if u32::from(*b) >= u32::from(*length) * 8 {
                    errs.push(format!("{at}: bit {b} outside a {length}-byte read"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::parse;

    #[test]
    fn sound_definition_has_no_errors() {
        let def = parse(
            r#"<logger><protocols><protocol id="SSM">
            <parameters>
              <parameter id="P8" name="rpm" desc=""><address length="2">0x0E</address>
                <conversions><conversion units="rpm" expr="x/4" format="0"/></conversions></parameter>
              <parameter id="P12" name="maf" desc=""><address length="2">0x13</address>
                <conversions><conversion units="g/s" expr="x/100" format="0"/></conversions></parameter>
              <parameter id="P200" name="load" desc=""><depends><ref parameter="P8"/><ref parameter="P12"/></depends>
                <conversions><conversion units="g/rev" expr="(P12*60)/P8" format="0.00"/></conversions></parameter>
            </parameters></protocol></protocols></logger>"#,
        )
        .unwrap();
        assert_eq!(def.validate(), Vec::<String>::new());
    }

    #[test]
    fn reports_each_problem() {
        let def = parse(
            r#"<logger><protocols><protocol id="SSM">
            <parameters>
              <parameter id="A" name="" desc=""><address length="1" bit="9">0x10</address>
                <conversions><conversion units="" expr="x" format="0"/></conversions></parameter>
              <parameter id="A" name="" desc=""><address>0x11</address></parameter>
              <parameter id="B" name="" desc=""><depends><ref parameter="Q"/></depends>
                <conversions><conversion units="" expr="Q*2" format="0"/></conversions></parameter>
              <parameter id="C" name="" desc=""><depends><ref parameter="D"/></depends>
                <conversions><conversion units="" expr="[D:mph]+x" format="0"/></conversions></parameter>
              <parameter id="D" name="" desc=""><depends><ref parameter="C"/></depends>
                <conversions><conversion units="" expr="C+1" format="0"/></conversions></parameter>
            </parameters></protocol></protocols></logger>"#,
        )
        .unwrap();
        let errs = def.validate().join("\n");
        for want in [
            "bit 9 outside a 1-byte read",
            "duplicate parameter id 'A'",
            "references unknown parameter 'Q'",
            "uses x but the parameter has no address",
            "'D' has no 'mph' conversion",
            "cycle",
        ] {
            assert!(errs.contains(want), "missing '{want}' in:\n{errs}");
        }
    }
}
