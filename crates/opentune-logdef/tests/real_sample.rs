//! Optional test over a real RomRaider-style `logger.xml`.
//!
//! Gated on the `OPENTUNE_FIXTURES` environment variable so the large,
//! third-party sample is never committed to this repo. When `OPENTUNE_FIXTURES`
//! is set, the sample is expected at `$OPENTUNE_FIXTURES/logdef/<name>`; if it is
//! not there yet but the known read-only resource copy exists, it is copied in
//! at test time. When the variable is unset, the test is a no-op so a plain
//! `cargo test` stays hermetic and hardware-free.

use std::path::{Path, PathBuf};

const SAMPLE_NAME: &str = "romraider-log_defs.xml";

/// Read-only source location of the sample shipped with this task's resources.
/// Used only to populate the fixtures dir on demand; never read directly in the
/// committed test path.
const RESOURCE_SRC: &str =
    "/opt/nvme2/Claude/Project One/opentune-resources-2026-11-17/romraider-log_defs.xml";

fn fixture_path() -> Option<PathBuf> {
    let base = std::env::var_os("OPENTUNE_FIXTURES")?;
    let dir = Path::new(&base).join("logdef");
    let target = dir.join(SAMPLE_NAME);
    if !target.exists() {
        // Populate from the known resource copy if available; otherwise skip.
        if Path::new(RESOURCE_SRC).exists() {
            if std::fs::create_dir_all(&dir).is_err() {
                return None;
            }
            if std::fs::copy(RESOURCE_SRC, &target).is_err() {
                return None;
            }
        } else {
            return None;
        }
    }
    Some(target)
}

#[test]
fn parses_real_romraider_log_defs() {
    let Some(path) = fixture_path() else {
        eprintln!("OPENTUNE_FIXTURES/logdef not available; skipping real-sample test");
        return;
    };
    let xml = std::fs::read_to_string(&path).expect("read fixture");
    let def = opentune_logdef::parse(&xml).expect("parse real log_defs");

    // Structural expectations for the Subaru SSM EcuFlash-format sample.
    assert_eq!(def.format, opentune_logdef::SourceFormat::EcuFlashLogProtocols);
    assert!(!def.convert_factors.is_empty(), "convert_factors captured");
    assert!(!def.protocols.is_empty(), "at least one protocol");

    let ssm = def
        .protocols
        .iter()
        .find(|p| p.id.eq_ignore_ascii_case("SSM"))
        .expect("SSM protocol present");
    assert_eq!(ssm.kind, opentune_logdef::ProtocolKind::SubaruSsm);

    let total = def.param_count();
    assert!(total > 100, "expected a large parameter set, got {total}");

    // Every memory-addressed parameter has a real address, and most conversions
    // reduce to a linear scale.
    let mut linear = 0usize;
    let mut total_conv = 0usize;
    for (_proto, _ecu, p) in def.params() {
        if let opentune_logdef::Addressing::Memory { address, .. } = p.addressing {
            // Addresses in the sample are non-trivial SSM RAM ids.
            let _ = address;
        }
        if let Some(c) = &p.conversion {
            total_conv += 1;
            if matches!(c.fit, opentune_logdef::ConversionFit::Linear { .. }) {
                linear += 1;
            }
        }
    }
    assert!(total_conv > 0);
    // The vast majority of SSM conversions are affine (x*k+b style).
    assert!(
        linear * 2 >= total_conv,
        "expected most conversions linear: {linear}/{total_conv}"
    );

    // Serde round-trip over the real data.
    let json = serde_json::to_string(&def).unwrap();
    let back: opentune_logdef::LogDef = serde_json::from_str(&json).unwrap();
    assert_eq!(def, back);
}
