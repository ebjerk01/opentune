//! Parse every shipped ECU definition in `ecudefs/` and sanity-check it.
//!
//! This guards the JSON files against drifting out of sync with the `EcuDef`
//! schema: a malformed or renamed field fails the build, not a car.
use opentune_core::ecudef::{self, FullRead};
use std::path::PathBuf;

/// Locate the repo-root `ecudefs/` directory from this crate.
fn ecudefs_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR = <repo>/crates/opentune-core
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("crate is two levels below the repo root")
        .join("ecudefs")
}

#[test]
fn all_ecudefs_parse() {
    let dir = ecudefs_dir();
    assert!(
        dir.is_dir(),
        "ecudefs directory not found at {}",
        dir.display()
    );

    let defs = ecudef::load_dir(&dir).expect("every ecudefs/*.json must parse");
    assert!(
        !defs.is_empty(),
        "expected at least one ECU definition in {}",
        dir.display()
    );

    for def in &defs {
        // Identity fields must be populated.
        assert!(!def.id.is_empty(), "ecu def with empty id");
        assert!(!def.name.is_empty(), "{}: empty name", def.id);
        assert!(!def.maker.is_empty(), "{}: empty maker", def.id);
        assert!(!def.family.is_empty(), "{}: empty family", def.id);

        // A bench/boot-only ECU should still declare a flash size.
        if def.full_read == FullRead::BenchBoot {
            assert!(
                def.flash_size > 0,
                "{}: bench_boot def must set flash_size",
                def.id
            );
        }

        // Every named region must fit inside the declared flash image.
        if def.flash_size > 0 {
            for r in &def.regions {
                let end = r.region.address as u64 + r.region.length as u64;
                let base = def.regions.first().map(|f| f.region.address).unwrap_or(0);
                let image_end = base as u64 + def.flash_size as u64;
                assert!(
                    end <= image_end,
                    "{}: region '{}' ends past the flash image",
                    def.id,
                    r.name
                );
            }
        }
    }
}
