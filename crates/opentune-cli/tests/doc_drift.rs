//! Doc-drift gates: fail the build when the code grows something the docs do
//! not mention. Each test reads the live source of truth (the built `opentune`
//! binary, the checksum library, the workspace manifest) and checks one doc.
//!
//! When one of these fails, document the new item — don't loosen the test.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn read_doc(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn run_cli(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_opentune"))
        .args(args)
        .output()
        .expect("run opentune");
    assert!(out.status.success(), "opentune {args:?} failed: {out:?}");
    String::from_utf8(out.stdout).expect("utf-8 output")
}

/// Item names (two-space-indented first column) from a clap help section or
/// an `opentune drivers` style listing, starting after the `header` line and
/// ending at the first blank line.
fn listed_names(text: &str, header: &str) -> Vec<String> {
    text.lines()
        .skip_while(|l| l.trim_end() != header)
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .filter(|l| l.starts_with("  ") && !l.starts_with("   "))
        .filter_map(|l| l.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

fn report(what: &str, doc: &str, missing: &[String]) {
    assert!(
        missing.is_empty(),
        "{what} missing from {doc}: {missing:?}\n\
         Document them there (see docs/DEVELOPMENT.md, 'Docs stay in step')."
    );
}

/// (a) Every top-level CLI subcommand is documented as `` `opentune <cmd>` ``
/// in README.md or docs/ARCHITECTURE.md (the "CLI commands" table).
#[test]
fn every_cli_subcommand_is_documented() {
    let help = run_cli(&["--help"]);
    let cmds: Vec<String> = listed_names(&help, "Commands:")
        .into_iter()
        .filter(|c| c != "help")
        .collect();
    assert!(
        cmds.len() > 10,
        "could not parse `opentune --help`:\n{help}"
    );

    let docs = read_doc("README.md") + &read_doc("docs/ARCHITECTURE.md");
    let missing: Vec<String> = cmds
        .into_iter()
        .filter(|c| {
            let tok = format!("`opentune {c}");
            !docs.match_indices(&tok).any(|(i, _)| {
                // Next char must end the command word (` ` or the closing backtick).
                matches!(docs[i + tok.len()..].chars().next(), Some(' ' | '`'))
            })
        })
        .collect();
    report(
        "CLI subcommands",
        "README.md / docs/ARCHITECTURE.md",
        &missing,
    );
}

/// (b) Every driver id in the CLI registry appears (backticked) in docs/DRIVERS.md.
#[test]
fn every_registered_driver_is_in_drivers_md() {
    let ids = listed_names(&run_cli(&["drivers"]), "Available drivers:");
    assert!(
        ids.contains(&"mock".to_owned()),
        "could not parse `opentune drivers`"
    );
    let doc = read_doc("docs/DRIVERS.md");
    let missing: Vec<String> = ids
        .into_iter()
        .filter(|id| !doc.contains(&format!("`{id}`")))
        .collect();
    report("driver ids", "docs/DRIVERS.md", &missing);
}

/// (c) Every checksum algorithm id is in the `checksum` valid-id list of
/// docs/CONTRIBUTING-ECUDEFS.md.
#[test]
fn every_checksum_algo_is_in_the_ecudefs_guide() {
    let doc = read_doc("docs/CONTRIBUTING-ECUDEFS.md");
    let start = doc
        .find("- **`checksum`** must be an id")
        .expect("CONTRIBUTING-ECUDEFS.md lost its `checksum` valid-id list");
    // The bullet runs until the next bullet or blank line.
    let rest = &doc[start + 2..];
    let end = [rest.find("\n- "), rest.find("\n\n")]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(rest.len());
    let list = &rest[..end];

    let missing: Vec<String> = opentune_checksum::algorithms()
        .into_iter()
        .map(|a| a.id.to_owned())
        .filter(|id| !list.contains(&format!("`{id}`")))
        .collect();
    report(
        "checksum algorithm ids",
        "docs/CONTRIBUTING-ECUDEFS.md (`checksum` rule)",
        &missing,
    );
    // Sanity: every listed id resolves (algorithms() is the canonical set).
    for a in opentune_checksum::algorithms() {
        assert!(
            opentune_checksum::get(a.id).is_some(),
            "{} does not resolve",
            a.id
        );
    }
}

/// (d) Every workspace member crate is named (backticked) in docs/ARCHITECTURE.md.
#[test]
fn every_workspace_crate_is_in_architecture_md() {
    let root = repo_root();
    let manifest = read_doc("Cargo.toml");
    let members_start = manifest.find("members").expect("workspace members");
    let block = &manifest[members_start..];
    let block = &block[..block.find(']').expect("end of members")];
    let members: Vec<&str> = block
        .split('"')
        .skip(1)
        .step_by(2)
        .filter(|s| !s.trim().is_empty())
        .collect();
    assert!(members.len() > 10, "could not parse workspace members");

    let doc = read_doc("docs/ARCHITECTURE.md");
    let mut missing = Vec::new();
    for m in members {
        let toml = std::fs::read_to_string(root.join(m).join("Cargo.toml"))
            .unwrap_or_else(|e| panic!("{m}/Cargo.toml: {e}"));
        let name = toml
            .lines()
            .map(str::trim)
            .find_map(|l| {
                let v = l.strip_prefix("name")?.trim_start().strip_prefix('=')?;
                Some(v.trim().trim_matches('"').to_owned())
            })
            .unwrap_or_else(|| panic!("{m}: no package name"));
        if !doc.contains(&format!("`{name}`")) {
            missing.push(format!("{name} ({m})"));
        }
    }
    report("workspace crates", "docs/ARCHITECTURE.md", &missing);
}
