//! The definition packs shipped in the repository's top-level `packs/` folder
//! must all load cleanly: no pack that fails to open, no file that fails to
//! parse or validate, and no logger conversion whose stored linear fit
//! disagrees with its expression.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use opentune_autoscan::Protocol;
use opentune_logdef::{Addressing, ConversionFit, ProtocolKind};
use opentune_packs::{load_pack, Pack, PackLibrary};
use opentune_xdf::math::{linear_from_equation, LinearFit};

fn packs_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs")
}

fn assert_clean(p: &Pack) {
    assert!(
        p.report.is_clean(),
        "pack {} has load errors: {:?}",
        p.manifest.id,
        p.report.notes
    );
    assert!(p.report.loaded > 0, "pack {} is empty", p.manifest.id);
    assert!(
        !p.manifest.license.is_empty(),
        "pack {} has no license",
        p.manifest.id
    );
}

#[test]
fn every_shipped_pack_loads_without_errors() {
    let lib = PackLibrary::load_all(&packs_root());
    assert!(
        lib.failed.is_empty(),
        "packs failed to open: {:?}",
        lib.failed
    );
    assert!(lib.packs.len() >= 3, "expected the starter packs");
    let mut ids = HashSet::new();
    for p in &lib.packs {
        assert_clean(p);
        assert!(ids.insert(p.manifest.id.clone()), "duplicate pack id");
        // the pack id is its directory name
        assert_eq!(
            p.root.file_name().and_then(|s| s.to_str()),
            Some(p.manifest.id.as_str())
        );
    }
    // the scaffold is not discovered, but must itself be a valid pack
    assert!(lib.packs.iter().all(|p| p.manifest.id != "my-pack"));
    assert_clean(&load_pack(&packs_root().join("_template")).unwrap());

    // service-script ids are unique across packs
    let mut sids = HashSet::new();
    for s in lib.services() {
        assert!(sids.insert(s.id.clone()), "duplicate service id {}", s.id);
    }
}

#[test]
fn logger_conversions_match_their_expressions() {
    let mut checked = 0;
    let lib = PackLibrary::load_all(&packs_root());
    for p in &lib.packs {
        for def in &p.loggers {
            for (proto, _ecu, param) in def.params() {
                // addressing agrees with the declared protocol
                match (&proto.kind, &param.addressing) {
                    (ProtocolKind::Obd2Mode01, Addressing::ObdPid { .. })
                    | (ProtocolKind::UdsMode22, Addressing::UdsDid { .. }) => {}
                    other => panic!("{}: mismatched protocol/addressing {other:?}", param.id),
                }
                let Some(c) = &param.conversion else { continue };
                let ConversionFit::Linear { factor, offset } = c.fit else {
                    panic!("{}: shipped conversions must be linear", param.id);
                };
                match linear_from_equation(&c.expr) {
                    LinearFit::Linear {
                        factor: f,
                        offset: o,
                    } => {
                        assert!(
                            (f - factor).abs() < 1e-9 && (o - offset).abs() < 1e-9,
                            "{}: '{}' fits to {f}/{o}, file says {factor}/{offset}",
                            param.id,
                            c.expr
                        );
                    }
                    other => panic!("{}: '{}' is {other:?}", param.id, c.expr),
                }
                checked += 1;
            }
        }
    }
    assert!(checked > 20, "expected the OBD mode-01 conversions");
}

#[test]
fn obd_logger_polls_through_the_live_engine() {
    let pack = load_pack(&packs_root().join("generic-obd2")).unwrap();
    let def = &pack.loggers[0];
    // 41 0C 1A F8 -> 0x1AF8 / 4 = 1726 rpm; 41 05 7B -> 123 - 40 = 83 degC
    let mut diag = |req: &[u8]| -> Result<Vec<u8>, String> {
        match req {
            [0x01, 0x0C] => Ok(vec![0x41, 0x0C, 0x1A, 0xF8]),
            [0x01, 0x05] => Ok(vec![0x41, 0x05, 0x7B]),
            _ => Err("no response".into()),
        }
    };
    let s = opentune_logdef::poll_once(&mut diag, def, &["obd01_0c_rpm", "obd01_05_coolant_temp"]);
    assert_eq!(s.len(), 2);
    let by_id = |id: &str| s.iter().find(|x| x.param_id == id).unwrap().value;
    assert_eq!(by_id("obd01_0c_rpm"), Some(1726.0));
    assert_eq!(by_id("obd01_05_coolant_temp"), Some(83.0));
}

/// Every shipped trigger set refers to channels its own pack logs: each
/// comparison has an id among the pack's logger parameters.
#[test]
fn shipped_triggers_refer_to_logged_channels() {
    let lib = PackLibrary::load_all(&packs_root());
    let mut n = 0;
    for p in &lib.packs {
        let ids: Vec<&str> = p
            .loggers
            .iter()
            .flat_map(|d| d.params())
            .map(|(_, _, x)| x.id.as_str())
            .collect();
        for t in &p.triggers {
            n += 1;
            assert!(
                t.missing_channels(&ids).is_empty(),
                "{}/{}: channels not in the pack's loggers: {:?}",
                p.manifest.id,
                t.id,
                t.missing_channels(&ids)
            );
        }
    }
    assert!(n >= 1, "the generic-obd2 pack ships a trigger set");
}

/// The generic-obd2 WOT trigger fires on a simulated pull polled through the
/// pack's own logger, and the over-temperature mark fires on a hot engine.
#[test]
fn obd_wot_trigger_fires_on_a_pull() {
    use opentune_logdef::trigger::{Emit, MarkerKind};
    use opentune_logdef::TriggerEngine;
    let pack = load_pack(&packs_root().join("generic-obd2")).unwrap();
    let def = &pack.loggers[0];
    let set = pack.triggers.iter().find(|t| t.id == "wot-pull").unwrap();
    let mut e: TriggerEngine<usize> = TriggerEngine::new(set.clone());
    let mut kinds = Vec::new();
    for round in 0..40usize {
        // 100 ms rounds: WOT at 4000 rpm from 1.0 s to 2.5 s, coolant
        // 110 degC from 2.5 s (mark after its 1 s hold, inside the window).
        let pull = (10..25).contains(&round);
        let rpm = if pull { 4000u16 } else { 1500 };
        let tps = if pull { 250u8 } else { 30 };
        let ect = if round >= 25 { 150u8 } else { 120 };
        let mut diag = |req: &[u8]| -> Result<Vec<u8>, String> {
            match req {
                [0x01, 0x0C] => Ok([vec![0x41, 0x0C], (rpm * 4).to_be_bytes().to_vec()].concat()),
                [0x01, 0x11] => Ok(vec![0x41, 0x11, tps]),
                [0x01, 0x05] => Ok(vec![0x41, 0x05, ect]),
                _ => Err("no response".into()),
            }
        };
        let s = opentune_logdef::poll_once(
            &mut diag,
            def,
            &["obd01_0c_rpm", "obd01_11_throttle", "obd01_05_coolant_temp"],
        );
        for em in e.push(round as u64 * 100, &s, vec![round]) {
            if let Emit::Marker(m) = em {
                kinds.push((m.kind, m.trigger));
            }
        }
    }
    e.finish();
    let r = e.report();
    assert_eq!(
        r.fires,
        vec![("wot-pull".to_string(), 1), ("coolant-hot".to_string(), 1)],
        "{r:?}"
    );
    assert_eq!(r.windows.len(), 1);
    // Fires at 1.3 s (300 ms hold), 3 s of history back to round 0 (the log
    // start); released at 3.5 s (1 s below), recorded to the end of the log.
    let w = &r.windows[0];
    assert_eq!(w.start_ts, 0);
    assert!(w.cut_by_end, "{w:?}");
    assert_eq!(kinds[0], (MarkerKind::Start, "wot-pull".to_string()));
    assert!(kinds.contains(&(MarkerKind::Mark, "coolant-hot".to_string())));
}

#[test]
fn module_lists_use_valid_can_ids() {
    let lib = PackLibrary::load_all(&packs_root());
    for p in &lib.packs {
        for list in &p.modules {
            let mut seen = HashSet::new();
            for m in &list.modules {
                assert!(
                    m.req_id <= 0x7FF && m.resp_id <= 0x7FF,
                    "{}: 11-bit ids expected",
                    m.name
                );
                assert_ne!(m.req_id, m.resp_id, "{}", m.name);
                // Real-vehicle packs use the documented low-byte convention;
                // the simulator pack keeps its own module numbers.
                if !is_simulator_pack(p) {
                    assert_eq!(
                        m.addr,
                        (m.req_id & 0xFF) as u8,
                        "{}: addr convention",
                        m.name
                    );
                }
                assert!(seen.insert(m.req_id), "{}: duplicate request id", m.name);
                if p.manifest.id == "vag-modules" {
                    assert_eq!(m.protocol, Protocol::Uds);
                }
            }
        }
    }
}

/// Packs that only target the `vcar` simulator (never a real vehicle).
fn is_simulator_pack(p: &Pack) -> bool {
    p.manifest.id.starts_with("vcar")
}

#[test]
fn obd_clear_is_the_only_shipped_write() {
    // Writes in simulator packs are exercised against the virtual car only;
    // for packs aimed at real vehicles, OBD mode 04 is the only write shipped.
    let lib = PackLibrary::load_all(&packs_root());
    let writers: Vec<&str> = lib
        .packs
        .iter()
        .filter(|p| !is_simulator_pack(p))
        .flat_map(|p| p.services.iter())
        .filter(|s| s.steps.iter().any(|st| st.is_write()))
        .map(|s| s.id.as_str())
        .collect();
    assert_eq!(writers, vec!["obd2-clear-dtcs"]);
    let clear = lib.service_by_id("obd2-clear-dtcs").unwrap();
    assert!(clear.name.contains("WRITE"));
}

/// The contributor gate (`opentune packs validate packs`, run in CI): every
/// shipped pack has a complete manifest with provenance, loads cleanly, and
/// passes the privacy scan.
#[test]
fn every_shipped_pack_passes_contribution_validation() {
    let results = opentune_packs::validate_all(&packs_root(), &[]);
    assert!(results.len() >= 4, "found only {} packs", results.len());
    for v in &results {
        let errs: Vec<_> = v
            .issues
            .iter()
            .filter(|i| i.severity == opentune_core::Severity::Error)
            .collect();
        assert!(errs.is_empty(), "pack {} fails validation: {errs:#?}", v.id);
    }
}
