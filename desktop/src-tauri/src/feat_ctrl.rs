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

use opentune_ctrldef::decode::{summarize, RawFrame, SignalStats};
use opentune_ctrldef::{
    ControllerDef, Decoder, Issue, LinkContext, LiveSource, Severity, ValidationState,
};
use opentune_packs::{discover, load_pack, Kind, Pack};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The definition `opentune dsg` reads link to (`CTRLDEF_ID` in the CLI).
const DSG_CTRLDEF: &str = "vag-dq250-02e";
/// Decoded frames returned by default / at most (the summary covers all).
const DEFAULT_ROWS: usize = 2000;
const MAX_ROWS: usize = 20_000;

/// One definition and where it came from.
struct Found {
    def: ControllerDef,
    path: PathBuf,
    /// `builtin`, `pack` or `file`.
    source: &'static str,
    pack: Option<Value>,
    pack_ecudefs: Vec<opentune_core::ecudef::EcuDef>,
}

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

fn pack_info(p: &Pack) -> Value {
    let m = &p.manifest;
    json!({
        "id": m.id, "name": m.name, "version": m.version, "license": m.license,
        "dir": p.root.display().to_string(), "valid": p.report.is_clean(),
    })
}

/// Read every `*.json` in the built-in directory; a file that does not parse
/// goes to `failed` with the reason (never silently dropped).
fn read_builtin(dir: &Path, found: &mut Vec<Found>, failed: &mut Vec<Value>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            !opentune_ctrldef::is_scaffolding_file(name)
                && p.extension().and_then(|s| s.to_str()) == Some("json")
        })
        .collect();
    files.sort();
    for path in files {
        let parsed = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|t| ControllerDef::from_json(&t).map_err(|e| format!("parse error: {e}")));
        match parsed {
            Ok(def) => found.push(Found {
                def,
                path,
                source: "builtin",
                pack: None,
                pack_ecudefs: Vec::new(),
            }),
            Err(message) => failed.push(json!({
                "file": path.display().to_string(), "source": "builtin", "pack": null, "message": message,
            })),
        }
    }
}

/// The controller definitions of one pack (the pack loader already ran the
/// ctrldef validator on them) and its skipped controller files.
fn read_pack(p: Pack, found: &mut Vec<Found>, failed: &mut Vec<Value>) {
    let info = pack_info(&p);
    for n in p.report.notes.iter().filter(|n| n.kind == Kind::Controller) {
        failed.push(json!({
            "file": p.root.join(&n.file).display().to_string(), "source": "pack",
            "pack": info, "message": n.message,
        }));
    }
    let files: Vec<&str> = p
        .entries
        .iter()
        .filter(|e| e.kind == Kind::Controller)
        .map(|e| e.file.as_str())
        .collect();
    // entries and `controllers` are both in load order
    for (def, file) in p.controllers.iter().zip(files) {
        found.push(Found {
            def: def.clone(),
            path: p.root.join(file),
            source: "pack",
            pack: Some(info.clone()),
            pack_ecudefs: p.ecudefs.clone(),
        });
    }
}

/// Built-in definitions, then every pack under `packs_root`, then the `extra`
/// pack folders that are not already among them.
fn catalog(
    builtin: Option<&Path>,
    packs_root: Option<&Path>,
    extra: &[String],
) -> (Vec<Found>, Vec<Value>) {
    let (mut found, mut failed) = (Vec::new(), Vec::new());
    if let Some(d) = builtin {
        read_builtin(d, &mut found, &mut failed);
    }
    let mut dirs: Vec<PathBuf> = packs_root.map(discover).unwrap_or_default();
    for e in extra {
        let p = PathBuf::from(e);
        let canon = |x: &Path| std::fs::canonicalize(x).unwrap_or_else(|_| x.to_path_buf());
        if !dirs.iter().any(|d| canon(d) == canon(&p)) {
            dirs.push(p);
        }
    }
    for dir in dirs {
        match load_pack(&dir) {
            Ok(p) => read_pack(p, &mut found, &mut failed),
            Err(e) => failed.push(json!({
                "file": dir.display().to_string(), "source": "pack", "pack": null, "message": e.to_string(),
            })),
        }
    }
    (found, failed)
}

/// Link context: the bundled ecudefs, the database unit ids and the public
/// checksum ids. [`ctx_for`] adds a pack's own ecudefs and plugin checksums.
fn base_context(state: &crate::AppData) -> LinkContext {
    let ecudefs: Vec<_> = crate::state::load_defs(&state.ecudefs).as_ref().clone();
    let unit_ids = state
        .database
        .as_deref()
        .and_then(|d| opentune_db::Database::load_dir(d).ok())
        .map(|db| db.entries().iter().map(|e| e.unit.id.clone()).collect())
        .unwrap_or_default();
    let checksum_ids: Vec<String> = opentune_checksum::algorithms()
        .into_iter()
        .map(|a| a.id.to_string())
        .collect();
    LinkContext {
        ecudefs,
        unit_ids,
        checksum_ids,
    }
}

/// The link context for one definition: `base` plus the ecudefs of the pack it
/// came from and a checksum id that only a plugin resolver knows.
fn ctx_for<'a>(
    base: &'a LinkContext,
    checksums: &crate::Checksums,
    f: &Found,
) -> Cow<'a, LinkContext> {
    let plugin = f
        .def
        .checksum
        .as_ref()
        .filter(|c| !base.checksum_ids.contains(c) && checksums.get(c).is_some());
    if f.pack_ecudefs.is_empty() && plugin.is_none() {
        return Cow::Borrowed(base);
    }
    let mut c = base.clone();
    c.ecudefs.extend(f.pack_ecudefs.iter().cloned());
    c.checksum_ids.extend(plugin.cloned());
    Cow::Owned(c)
}

fn all_issues(f: &Found, ctx: &LinkContext) -> Vec<Issue> {
    let stem = f.path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    f.def
        .validate()
        .into_iter()
        .chain(f.def.validate_filename(stem))
        .chain(f.def.validate_links(ctx))
        .collect()
}

fn issue_json(i: &Issue) -> Value {
    json!({
        "severity": if i.severity == Severity::Error { "error" } else { "warn" },
        "code": i.code, "location": i.location, "message": i.message,
    })
}

fn related(d: &ControllerDef) -> Value {
    let low = format!("{} {}", d.id, d.family).to_ascii_lowercase();
    json!({
        "dsg": d.id == DSG_CTRLDEF,
        "haldex": low.contains("haldex"),
    })
}

fn kind_str(d: &ControllerDef) -> &'static str {
    d.kind.as_str()
}

fn matches_query(d: &ControllerDef, q: &str) -> bool {
    let q = q.trim().to_ascii_lowercase();
    if q.is_empty() {
        return true;
    }
    let hay = format!(
        "{} {} {} {} {}",
        d.id,
        d.name,
        d.maker,
        d.family,
        d.vehicles.join(" ")
    )
    .to_ascii_lowercase();
    hay.contains(&q) || d.matches(&q)
}

fn row(f: &Found, issues: &[Issue]) -> Value {
    let d = &f.def;
    let errors = issues
        .iter()
        .filter(|i| i.severity == Severity::Error)
        .count();
    json!({
        "id": d.id, "name": d.name, "kind": kind_str(d), "maker": d.maker, "family": d.family,
        "vehicles": d.vehicles, "validation": d.validation.as_str(),
        "source": f.source, "pack": f.pack, "path": f.path.display().to_string(),
        "buses": d.buses.iter().map(|b| &b.name).collect::<Vec<_>>(),
        "counts": {
            "diagnostics": d.diagnostics.len(),
            "frames": d.buses.iter().map(|b| b.frames.len()).sum::<usize>(),
            "live": d.live.len(), "tables": d.tables.len(), "gearsets": d.gearsets.len(),
        },
        "errors": errors, "warnings": issues.len() - errors,
        "related": related(d),
    })
}

fn list_impl(
    state: &crate::AppData,
    checksums: &crate::Checksums,
    q: Option<&str>,
    kind: Option<&str>,
    extra: &[String],
) -> Value {
    let builtin = builtin_dir(state);
    let (found, failed) = catalog(builtin.as_deref(), state.packs.as_deref(), extra);
    let total = found.len();
    let base = base_context(state);
    let defs: Vec<Value> = found
        .iter()
        .filter(|f| {
            kind.map(str::trim)
                .filter(|k| !k.is_empty())
                .is_none_or(|k| kind_str(&f.def) == k)
        })
        .filter(|f| q.is_none_or(|q| matches_query(&f.def, q)))
        .map(|f| row(f, &all_issues(f, &ctx_for(&base, checksums, f))))
        .collect();
    json!({
        "builtin_dir": builtin.map(|p| p.display().to_string()),
        "total": total,
        "defs": defs,
        "failed": failed,
    })
}

/// The pack a definition file sits in (`<pack>/controllers/<id>.json`).
fn pack_of(path: &Path) -> Option<Pack> {
    let dir = path.parent()?.parent()?;
    if !dir.join("pack.json").is_file() {
        return None;
    }
    load_pack(dir).ok()
}

fn open_found(state: &crate::AppData, path: &Path) -> Result<Found, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read '{}': {e}", path.display()))?;
    let def = ControllerDef::from_json(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let builtin = builtin_dir(state)
        .and_then(|b| std::fs::canonicalize(b).ok())
        .zip(std::fs::canonicalize(path).ok())
        .is_some_and(|(b, p)| p.parent() == Some(b.as_path()));
    let (source, pack, pack_ecudefs) = if builtin {
        ("builtin", None, Vec::new())
    } else if let Some(p) = pack_of(path) {
        ("pack", Some(pack_info(&p)), p.ecudefs.clone())
    } else {
        ("file", None, Vec::new())
    };
    Ok(Found {
        def,
        path: path.to_path_buf(),
        source,
        pack,
        pack_ecudefs,
    })
}

fn get_impl(
    state: &crate::AppData,
    checksums: &crate::Checksums,
    path: &Path,
) -> Result<Value, String> {
    let f = open_found(state, path)?;
    let base = base_context(state);
    let issues = all_issues(&f, &ctx_for(&base, checksums, &f));
    let mut out = row(&f, &issues);
    out["def"] = serde_json::to_value(&f.def).map_err(|e| e.to_string())?;
    out["issues"] = issues.iter().map(issue_json).collect();
    Ok(out)
}

fn stats_json(s: &SignalStats, name: &str, unit: &str) -> Value {
    let mut v = serde_json::to_value(s).unwrap_or(Value::Null);
    v["name"] = json!(name);
    v["unit"] = json!(unit);
    v
}

/// Decode `text` with `def`. `bus` empty/None picks the only set (or all sets
/// when they do not share ids). `limit` caps the decoded frames returned.
pub(crate) fn decode_text(
    def: &ControllerDef,
    text: &str,
    bus: Option<&str>,
    limit: usize,
) -> Result<Value, String> {
    if def.buses.is_empty() {
        return Err(format!(
            "{} defines no CAN frame sets to decode with",
            def.id
        ));
    }
    let bus = bus
        .map(str::trim)
        .filter(|b| !b.is_empty() && *b != "all")
        .map(str::to_string)
        .or_else(|| (def.buses.len() == 1).then(|| def.buses[0].name.clone()));
    let dec = Decoder::new(def, bus.as_deref())?;
    let parsed = opentune_haldex::canlog::parse_log(text);
    let label = bus.clone().unwrap_or_else(|| "all".into());
    let (summary, decoded) = summarize(
        &dec,
        &label,
        parsed.frames.iter().map(|f| RawFrame {
            ts: f.ts,
            id: f.id,
            extended: f.extended,
            data: &f.data,
        }),
    );
    let known = dec.ids();
    let mut undecoded: BTreeMap<u32, u64> = BTreeMap::new();
    for f in &parsed.frames {
        if known.binary_search(&f.id).is_err() {
            *undecoded.entry(f.id).or_default() += 1;
        }
    }
    let mut undecoded: Vec<(u32, u64)> = undecoded.into_iter().collect();
    undecoded.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let unit_of = |frame: &str, sig: &str| -> String {
        def.buses
            .iter()
            .flat_map(|b| &b.frames)
            .find(|f| f.name == frame)
            .and_then(|f| f.signals.iter().find(|s| s.name == sig))
            .map(|s| s.unit.clone())
            .unwrap_or_default()
    };
    let t0 = parsed.frames.iter().find_map(|f| f.ts);
    let rows: Vec<Value> = decoded
        .iter()
        .take(limit)
        .map(|(ts, f)| {
            json!({
                "t": ts.zip(t0).map(|(a, b)| a - b),
                "id": format!("0x{:03X}", f.id),
                "bus": f.bus, "frame": f.frame, "signals": f.signals,
            })
        })
        .collect();
    let live: Vec<Value> = def
        .live
        .iter()
        .filter_map(|l| {
            let LiveSource::CanSignal {
                bus: b,
                frame,
                signal,
            } = &l.source
            else {
                return None;
            };
            if bus.as_deref().is_some_and(|x| !b.eq_ignore_ascii_case(x)) {
                return None;
            }
            let s = summary.live.get(&l.name)?;
            let mut v = stats_json(s, &l.name, &l.unit);
            v["source"] = json!(format!("{b}/{frame}.{signal}"));
            Some(v)
        })
        .collect();
    let frames: Vec<Value> = summary
        .per_frame
        .iter()
        .map(|(name, f)| {
            json!({
                "name": name, "bus": f.bus, "id": format!("0x{:03X}", f.id), "count": f.count,
                "span": match (f.first_ts, f.last_ts) { (Some(a), Some(b)) if b > a => Some(b - a), _ => None },
                "signals": f.signals.iter().map(|(s, st)| stats_json(st, s, &unit_of(name, s))).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "controller": def.id,
        "validation": def.validation.as_str(),
        "unconfirmed": def.validation < ValidationState::RealImageTested,
        "bus": label,
        "sets": def.buses.iter().map(|b| &b.name).collect::<Vec<_>>(),
        "frames_in_log": summary.frames,
        "decoded": summary.decoded,
        "bad_lines": parsed.bad_lines,
        "live": live,
        "per_frame": frames,
        "undecoded_ids": undecoded.iter().take(12).map(|(id, n)| json!({ "id": format!("0x{id:03X}"), "count": n })).collect::<Vec<_>>(),
        "undecoded_total": undecoded.iter().map(|x| x.1).sum::<u64>(),
        "rows": rows,
        "rows_total": decoded.len(),
    }))
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

    #[test]
    fn decodes_a_synthetic_log_like_the_cli() {
        let def = ControllerDef::from_json(
            &std::fs::read_to_string(repo("ctrldefs/borgwarner-haldex-gen4.json")).unwrap(),
        )
        .unwrap();
        let bus = def.buses[0].name.clone();
        let frame = &def.buses[0].frames[0];
        let len = frame.dlc.unwrap_or(8) as usize;
        let payload: String = (0..len).map(|i| format!("{:02X}", 0x10 + i)).collect();
        // candump + Vector ASC of the same synthetic frame, plus an unknown id
        // and an unreadable line
        let text = format!(
            "(1.000000) can0 {id:03X}#{payload}\n(1.010000) can0 7FF#00\n\
             garbage\n   1.020000 1  {id:X}             Rx   d {len:x} {asc}\n",
            id = frame.id,
            asc = (0..len)
                .map(|i| format!("{:02X}", 0x10 + i))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let v = decode_text(&def, &text, Some(&bus), 10).unwrap();
        assert_eq!(v["frames_in_log"], 3);
        assert_eq!(v["decoded"], 2);
        assert_eq!(v["bad_lines"], 1);
        assert_eq!(v["undecoded_ids"][0]["id"], "0x7FF");
        let rows = v["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["frame"], frame.name);
        assert!((rows[1]["t"].as_f64().unwrap() - 0.02).abs() < 1e-9);
        assert_eq!(
            rows[0]["signals"], rows[1]["signals"],
            "candump and ASC agree"
        );
        let pf = v["per_frame"].as_array().unwrap();
        assert_eq!(pf[0]["count"], 2);
        // same values as the library decoder the CLI uses
        let dec = Decoder::new(&def, Some(&bus)).unwrap();
        let data: Vec<u8> = (0..len as u8).map(|i| 0x10 + i).collect();
        let want = dec.decode(frame.id, frame.extended, &data).unwrap();
        assert_eq!(
            rows[0]["signals"],
            serde_json::to_value(&want.signals).unwrap()
        );
        // limit caps the rows, not the summary
        let v1 = decode_text(&def, &text, Some(&bus), 1).unwrap();
        assert_eq!(v1["rows"].as_array().unwrap().len(), 1);
        assert_eq!(v1["rows_total"], 2);
        // an unknown set is refused with the set names
        let e = decode_text(&def, &text, Some("nope"), 10).unwrap_err();
        assert!(e.contains(&bus), "{e}");
    }

    #[test]
    fn a_definition_without_frame_sets_cannot_decode() {
        let mut def = ControllerDef::from_json(
            &std::fs::read_to_string(repo("ctrldefs/vag-dq381.json")).unwrap(),
        )
        .unwrap();
        def.buses.clear();
        assert!(decode_text(&def, "", None, 10)
            .unwrap_err()
            .contains("no CAN frame sets"));
    }
}
