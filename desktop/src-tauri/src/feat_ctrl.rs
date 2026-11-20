//! Controller feature — drivetrain controller definitions (TCU / AWD).
//!
//! Backs the "Controllers" panel (`desktop/ui/src/feat_ctrl.js`) with three
//! read-only, offline commands over `opentune-ctrldef` (docs/CONTROLLER-DEFS.md):
//!
//! * [`ctrl_list`] — `{q?, kind?, extra?}` → every definition: the built-in set
//!   (the repository's `ctrldefs/`, bundled as a resource next to `ecudefs/`)
//!   plus the `controllers/` of every definition pack (the installed `packs/`
//!   and the pack folders in `extra`, i.e. the packs the pack picker loaded
//!   from disk). Each row carries its source, pack, validation word and
//!   error / warning counts; files that do not load are listed in `failed`.
//! * [`ctrl_get`] — `{path}` → one definition (as stored), where it came from,
//!   every validation issue (structure, honesty, file name and links against
//!   the bundled ecudefs / database units / checksum ids plus the pack's own
//!   ecudefs) and the related tools (`opentune dsg`, the Haldex panel).
//! * [`ctrl_decode_log`] — `{path, log, bus?, limit?}` → decode a candump /
//!   OpenTune CSV / SavvyCAN CSV / Vector ASC capture with the definition's
//!   frame sets: the same reader (`opentune_haldex::canlog::parse_log`) and the
//!   same decoder + summary (`opentune_ctrldef::decode`) as
//!   `opentune controller decode`. Nothing is transmitted.
//!
//! Definitions are small, so they are re-read per call.
//!
//! The catalog, link validation, list / detail JSON and the log decode live
//! in [`opentune_hub::controllers`] (shared with other front ends); this
//! module resolves the app's data directories and checksum resolvers and
//! wraps them as Tauri commands.

use opentune_ctrldef::{ControllerDef, LinkContext};
pub(crate) use opentune_hub::controllers::decode_text;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Decoded frames returned by default / at most (the summary covers all).
const DEFAULT_ROWS: usize = 2000;
const MAX_ROWS: usize = 20_000;

/// The built-in `ctrldefs/` directory: next to the other data directories
/// (bundled resources, `OPENTUNE_DATA`, an embedder's data root), else the
/// repository in a dev build.
pub(crate) fn builtin_dir(state: &crate::AppData) -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(e) = std::env::var("OPENTUNE_DATA") {
        cands.push(PathBuf::from(e).join("ctrldefs"));
    }
    for d in [&state.ecudefs, &state.packs, &state.database]
        .into_iter()
        .flatten()
    {
        if let Some(parent) = d.parent() {
            cands.push(parent.join("ctrldefs"));
        }
    }
    cands.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ctrldefs"));
    cands.into_iter().find(|p| p.is_dir())
}

/// Link context: the bundled ecudefs, the database unit ids and the public
/// checksum ids ([`opentune_hub::controllers::link_context`]).
fn base_context(state: &crate::AppData) -> LinkContext {
    let ecudefs: Vec<_> = crate::state::load_defs(&state.ecudefs).as_ref().clone();
    opentune_hub::controllers::link_context(ecudefs, state.database.as_deref())
}

fn list_impl(
    state: &crate::AppData,
    checksums: &crate::Checksums,
    q: Option<&str>,
    kind: Option<&str>,
    extra: &[String],
) -> Value {
    let builtin = builtin_dir(state);
    opentune_hub::controllers::list(
        builtin.as_deref(),
        state.packs.as_deref(),
        &base_context(state),
        &|id| checksums.get(id).is_some(),
        q,
        kind,
        extra,
    )
}

fn get_impl(
    state: &crate::AppData,
    checksums: &crate::Checksums,
    path: &Path,
) -> Result<Value, String> {
    opentune_hub::controllers::get(
        builtin_dir(state).as_deref(),
        &base_context(state),
        &|id| checksums.get(id).is_some(),
        path,
    )
}

/// Every controller definition: built-in `ctrldefs/` plus the `controllers/`
/// of the installed packs and of the pack folders in `extra`.
#[tauri::command]
pub async fn ctrl_list(
    q: Option<String>,
    kind: Option<String>,
    extra: Option<Vec<String>>,
    state: tauri::State<'_, crate::AppData>,
    checksums: tauri::State<'_, crate::Checksums>,
) -> Result<Value, String> {
    Ok(list_impl(
        &state,
        &checksums,
        q.as_deref(),
        kind.as_deref(),
        &extra.unwrap_or_default(),
    ))
}

/// One controller definition file with its validation issues and source.
#[tauri::command]
pub async fn ctrl_get(
    path: String,
    state: tauri::State<'_, crate::AppData>,
    checksums: tauri::State<'_, crate::Checksums>,
) -> Result<Value, String> {
    get_impl(&state, &checksums, Path::new(&path))
}

/// Decode a CAN log offline with a controller definition's frame sets.
#[tauri::command]
pub async fn ctrl_decode_log(
    path: String,
    log: String,
    bus: Option<String>,
    limit: Option<usize>,
) -> Result<Value, String> {
    let text = std::fs::read_to_string(&path).map_err(|e| format!("read '{path}': {e}"))?;
    let def = ControllerDef::from_json(&text).map_err(|e| format!("{path}: {e}"))?;
    let bytes = std::fs::read(&log).map_err(|e| format!("read '{log}': {e}"))?;
    let limit = limit.unwrap_or(DEFAULT_ROWS).min(MAX_ROWS);
    let mut v = tauri::async_runtime::spawn_blocking(move || {
        decode_text(
            &def,
            &String::from_utf8_lossy(&bytes),
            bus.as_deref(),
            limit,
        )
    })
    .await
    .map_err(|e| format!("decode task failed: {e}"))??;
    v["log"] = json!(log);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(rel)
    }

    fn app_data() -> crate::AppData {
        crate::AppData {
            ecudefs: Some(repo("ecudefs")),
            catalog: None,
            guide: None,
            database: Some(repo("database")),
            packs: Some(repo("packs")),
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("ot-ctrl-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A pack with one copied repo definition and one broken file.
    fn demo_pack() -> PathBuf {
        let d = temp_dir("pack");
        std::fs::write(
            d.join("pack.json"),
            r#"{"id":"ctrl-test","name":"Controller test pack","version":"0.1.0","license":"GPL-3.0-or-later"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(d.join("controllers")).unwrap();
        std::fs::copy(
            repo("ctrldefs/openhaldex-c6.json"),
            d.join("controllers/openhaldex-c6.json"),
        )
        .unwrap();
        std::fs::write(d.join("controllers/broken.json"), "{ not json").unwrap();
        d
    }

    #[test]
    fn builtin_dir_is_found_next_to_the_data_dirs() {
        let b = builtin_dir(&app_data()).expect("ctrldefs found");
        assert!(b.join("vag-dq250-02e.json").is_file());
    }

    #[test]
    fn lists_builtin_and_pack_definitions_with_validation_state() {
        let pack = demo_pack();
        let v = list_impl(
            &app_data(),
            &crate::Checksums::default(),
            None,
            None,
            &[pack.display().to_string()],
        );
        let defs = v["defs"].as_array().unwrap();
        let builtin: Vec<&Value> = defs.iter().filter(|d| d["source"] == "builtin").collect();
        assert!(builtin.len() >= 8, "{}", builtin.len());
        // the shipped set validates without errors (links included)
        for d in &builtin {
            assert_eq!(d["errors"], 0, "{} has errors", d["id"]);
        }
        let dsg = builtin.iter().find(|d| d["id"] == "vag-dq250-02e").unwrap();
        assert_eq!(dsg["related"]["dsg"], true);
        assert_eq!(dsg["kind"], "tcu");
        let hx = builtin
            .iter()
            .find(|d| d["id"] == "borgwarner-haldex-gen4")
            .unwrap();
        assert_eq!(hx["related"]["haldex"], true);
        assert!(hx["counts"]["frames"].as_u64().unwrap() > 0);
        // the pack copy is listed with its pack, and the broken file is reported
        let from_pack = defs
            .iter()
            .find(|d| d["source"] == "pack" && d["id"] == "openhaldex-c6")
            .expect("pack controller listed");
        assert_eq!(from_pack["pack"]["id"], "ctrl-test");
        assert_eq!(from_pack["pack"]["valid"], false);
        let failed = v["failed"].as_array().unwrap();
        assert!(
            failed
                .iter()
                .any(|f| f["file"].as_str().unwrap().ends_with("broken.json")
                    && f["message"].as_str().unwrap().contains("parse error")),
            "{failed:?}"
        );
        // listing the same pack twice does not duplicate it
        let again = list_impl(
            &app_data(),
            &crate::Checksums::default(),
            None,
            None,
            &[pack.display().to_string(), pack.display().to_string()],
        );
        assert_eq!(again["total"], v["total"]);
        std::fs::remove_dir_all(&pack).ok();
    }

    #[test]
    fn query_and_kind_filter() {
        let st = app_data();
        let c = crate::Checksums::default();
        let by_part = list_impl(&st, &c, Some("0AY907554C"), None, &[]);
        assert!(by_part["defs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["id"] == "borgwarner-haldex-gen4"));
        let awd = list_impl(&st, &c, None, Some("awd"), &[]);
        assert!(awd["defs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["kind"] == "awd"));
        let none = list_impl(&st, &c, Some("no-such-controller-xyz"), None, &[]);
        assert!(none["defs"].as_array().unwrap().is_empty());
    }

    #[test]
    fn get_reports_source_definition_and_issues() {
        let st = app_data();
        let c = crate::Checksums::default();
        let v = get_impl(&st, &c, &repo("ctrldefs/mercedes-egs5x-7226.json")).unwrap();
        assert_eq!(v["source"], "builtin");
        assert_eq!(v["def"]["id"], "mercedes-egs5x-7226");
        assert!(v["def"]["buses"][0]["frames"].as_array().unwrap().len() > 1);
        assert_eq!(v["errors"], 0, "{:?}", v["issues"]);
        // a loose copy under another name: source "file", file-name rule fires
        let d = temp_dir("loose");
        let p = d.join("renamed.json");
        std::fs::copy(repo("ctrldefs/openhaldex-c6.json"), &p).unwrap();
        let v = get_impl(&st, &c, &p).unwrap();
        assert_eq!(v["source"], "file");
        assert!(v["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["code"] == "id.filename" && i["severity"] == "error"));
        assert!(get_impl(&st, &c, &d.join("missing.json")).is_err());
        std::fs::remove_dir_all(&d).ok();
    }
}
