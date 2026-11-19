//! Definition packs for the panels' pack picker (`desktop/ui/src/pack-picker.js`).
//!
//! The coding / adaptation / service / live-log / auto-scan engines each run on
//! one definition *file*. The pack picker lets the user choose that file from a
//! loaded [definition pack](opentune_packs) instead of browsing for it, and
//! shows which pack (id, version) it came from and whether the pack loaded
//! cleanly. Two read-only commands, both async (they read directories):
//!
//! * [`packs_list`] — every pack under the bundled `packs/` resource (resolved
//!   like `ecudefs/`, see [`crate::AppData`]); `_`-prefixed scaffolds skipped.
//! * [`packs_open`] — one pack from anywhere on disk ("Load pack from file"):
//!   its `pack.json`, its folder, or any definition file inside it.
//!
//! Each pack is reported as its manifest plus `dir`, `source`
//! (`installed` / `file`), `loaded` (definition count), `valid` (no file was
//! skipped), `notes` (each skipped file and why) and `defs` (`kind`, `file`,
//! absolute `path`, `id`, `name`) — the `path` is what the panels pass to their
//! engine commands. Packs are small, so they are re-read per call.

use opentune_packs::{discover, load_pack, Pack};
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};

/// Drop `.` and resolve `..` lexically (a dev build finds `packs/` as
/// `src-tauri/../../packs`), so the definition paths match what a file dialog
/// returns for the same file. No filesystem access, no `\\?\` prefixes.
fn tidy(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(out.components().next_back(), Some(Component::Normal(_))) || !out.pop()
                {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn pack_json(p: &Pack, source: &str) -> Value {
    let defs: Vec<Value> = p
        .entries
        .iter()
        .map(|e| {
            json!({
                "kind": e.kind.dir(),
                "file": e.file,
                "path": p.root.join(&e.file).display().to_string(),
                "id": e.id,
                "name": e.name,
            })
        })
        .collect();
    let notes: Vec<Value> = p
        .report
        .notes
        .iter()
        .map(|n| json!({ "file": n.file.replace('\\', "/"), "kind": n.kind.dir(), "message": n.message }))
        .collect();
    let m = &p.manifest;
    json!({
        "id": m.id,
        "name": m.name,
        "version": m.version,
        "maker": m.maker,
        "platform": m.platform,
        "vehicles": m.vehicles,
        "author": m.author,
        "license": m.license,
        "description": m.description,
        "dir": p.root.display().to_string(),
        "source": source,
        "loaded": p.report.loaded,
        "valid": p.report.is_clean(),
        "notes": notes,
        "defs": defs,
    })
}

fn list_impl(root: Option<&Path>) -> Value {
    let Some(root) = root else {
        return json!({ "root": null, "packs": [], "failed": [] });
    };
    let root = &tidy(root);
    let mut packs = Vec::new();
    let mut failed = Vec::new();
    for dir in discover(root) {
        match load_pack(&dir) {
            Ok(p) => packs.push(pack_json(&p, "installed")),
            Err(e) => {
                failed.push(json!({ "dir": dir.display().to_string(), "error": e.to_string() }))
            }
        }
    }
    json!({ "root": root.display().to_string(), "packs": packs, "failed": failed })
}

/// The pack directory for a picked path: a pack folder, its `pack.json`, or a
/// definition file inside it (`<pack>/<kind>/<file>.json`).
fn pack_dir(path: &Path) -> Result<PathBuf, String> {
    if path.is_dir() {
        return Ok(path.to_path_buf());
    }
    if !path.is_file() {
        return Err(format!("{}: no such file or folder", path.display()));
    }
    let mut dir = path.parent();
    for _ in 0..2 {
        match dir {
            Some(d) if d.join("pack.json").is_file() => return Ok(d.to_path_buf()),
            Some(d) => dir = d.parent(),
            None => break,
        }
    }
    Err(format!(
        "{} is not part of a definition pack — choose a pack's pack.json (see docs/DEFINITION-PACKS.md)",
        path.display()
    ))
}

fn open_impl(path: &Path) -> Result<Value, String> {
    let dir = tidy(&pack_dir(path)?);
    let pack = load_pack(&dir).map_err(|e| e.to_string())?;
    Ok(pack_json(&pack, "file"))
}

/// Every installed definition pack (the bundled `packs/` resource).
#[tauri::command]
pub async fn packs_list(state: tauri::State<'_, crate::AppData>) -> Result<Value, String> {
    Ok(list_impl(state.packs.as_deref()))
}

/// Load one pack from disk: its `pack.json`, its folder, or a definition file
/// inside it.
#[tauri::command]
pub async fn packs_open(path: String) -> Result<Value, String> {
    open_impl(Path::new(&path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_packs() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs")
    }

    #[test]
    fn lists_the_shipped_packs_with_definition_paths() {
        let v = list_impl(Some(&repo_packs()));
        let packs = v["packs"].as_array().unwrap();
        let ids: Vec<&str> = packs.iter().map(|p| p["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"vcar-demo"), "{ids:?}");
        assert!(ids.contains(&"generic-obd2"), "{ids:?}");
        assert!(
            !ids.iter().any(|i| i.contains("template")),
            "scaffold skipped: {ids:?}"
        );
        assert!(v["failed"].as_array().unwrap().is_empty());
        let demo = packs.iter().find(|p| p["id"] == "vcar-demo").unwrap();
        assert_eq!(demo["source"], "installed");
        assert_eq!(demo["valid"], true, "{:?}", demo["notes"]);
        let defs = demo["defs"].as_array().unwrap();
        assert_eq!(defs.len() as u64, demo["loaded"].as_u64().unwrap());
        let coding = defs
            .iter()
            .find(|d| d["kind"] == "coding" && d["file"] == "coding/engine.json")
            .expect("coding/engine.json listed");
        assert!(Path::new(coding["path"].as_str().unwrap()).is_file());
        assert!(!coding["id"].as_str().unwrap().is_empty());
        for kind in ["adaptation", "services", "modules"] {
            assert!(
                defs.iter().any(|d| d["kind"] == kind),
                "vcar-demo has {kind}"
            );
        }
    }

    #[test]
    fn tidy_resolves_dot_segments_lexically() {
        assert_eq!(tidy(Path::new("/a/b/../../packs")), PathBuf::from("/packs"));
        assert_eq!(tidy(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(tidy(Path::new("../x")), PathBuf::from("../x"));
        assert_eq!(tidy(Path::new("a/../../x")), PathBuf::from("../x"));
        // the listed paths carry no dot segments even from a dev build's root
        let v = list_impl(Some(&repo_packs()));
        assert!(!v["root"].as_str().unwrap().contains(".."));
    }

    #[test]
    fn controller_definitions_are_listed_with_their_kind() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("ot-packs-ctrl-{}-{n}", std::process::id()));
        std::fs::create_dir_all(d.join("controllers")).unwrap();
        std::fs::write(d.join("pack.json"), r#"{"id":"c","name":"C"}"#).unwrap();
        std::fs::copy(
            repo_packs().join("../ctrldefs/openhaldex-c6.json"),
            d.join("controllers/openhaldex-c6.json"),
        )
        .unwrap();
        let v = open_impl(&d.join("controllers/openhaldex-c6.json")).unwrap();
        assert_eq!(v["valid"], true, "{:?}", v["notes"]);
        let defs = v["defs"].as_array().unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0]["kind"], "controllers");
        assert_eq!(defs[0]["id"], "openhaldex-c6");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn no_packs_dir_is_an_empty_list() {
        let v = list_impl(None);
        assert!(v["root"].is_null());
        assert!(v["packs"].as_array().unwrap().is_empty());
    }

    #[test]
    fn opens_a_pack_from_its_manifest_folder_or_a_definition() {
        let root = repo_packs().join("vcar-demo");
        for p in [
            root.join("pack.json"),
            root.clone(),
            root.join("coding/engine.json"),
        ] {
            let v = open_impl(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            assert_eq!(v["id"], "vcar-demo");
            assert_eq!(v["source"], "file");
        }
        // a file outside any pack is refused with a pointer to the format
        let e = open_impl(&repo_packs().join("README.md")).unwrap_err();
        assert!(e.contains("not part of a definition pack"), "{e}");
        assert!(open_impl(&root.join("no-such.json")).is_err());
    }
}
