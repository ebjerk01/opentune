//! Drivetrain controller definitions (TCU / AWD) for a front end's
//! "Controllers" view, over `opentune-ctrldef` (docs/CONTROLLER-DEFS.md):
//!
//! * [`catalog`] — every definition: a built-in directory (the repository's
//!   `ctrldefs/`) plus the `controllers/` of every definition pack under a
//!   packs root and of extra pack folders. Files that do not load are
//!   reported, never silently dropped.
//! * [`link_context`] / [`ctx_for`] — the cross-link context (ecudefs,
//!   database unit ids, checksum ids, a pack's own ecudefs, plugin
//!   checksums); [`all_issues`] runs structure, file-name and link checks.
//! * [`list`] / [`get`] — the list rows and one definition with its issues,
//!   as the JSON the desktop's `ctrl_list` / `ctrl_get` return.
//! * [`decode_text`] — decode a candump / OpenTune CSV / SavvyCAN CSV /
//!   Vector ASC capture with a definition's frame sets: the same reader
//!   (`opentune_haldex::canlog::parse_log`) and the same decoder + summary
//!   (`opentune_ctrldef::decode`) as `opentune controller decode`. Nothing is
//!   transmitted.
//!
//! Where the built-in directory is and which checksum ids a plugin resolver
//! knows are the front end's business; they are passed in.
//!
//! It lives here rather than in `opentune-ctrldef` because `opentune-packs`
//! depends on `opentune-ctrldef` (the pack catalog would be a cycle) and the
//! log decode reads captures with `opentune-haldex`.

use opentune_core::ecudef::EcuDef;
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

/// One definition and where it came from.
pub struct Found {
    pub def: ControllerDef,
    pub path: PathBuf,
    /// `builtin`, `pack` or `file`.
    pub source: &'static str,
    pub pack: Option<Value>,
    pub pack_ecudefs: Vec<opentune_core::ecudef::EcuDef>,
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
pub fn catalog(
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

/// Link context: the bundled ecudefs (`ecudefs`, the front end's loaded set),
/// the unit ids of the database at `database` and the public checksum ids.
/// [`ctx_for`] adds a pack's own ecudefs and plugin checksums.
pub fn link_context(ecudefs: Vec<EcuDef>, database: Option<&Path>) -> LinkContext {
    let unit_ids = database
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
/// came from and a checksum id that only a plugin resolver knows
/// (`plugin_checksum(id)` is true when the front end's resolvers know `id`).
pub fn ctx_for<'a>(
    base: &'a LinkContext,
    plugin_checksum: &dyn Fn(&str) -> bool,
    f: &Found,
) -> Cow<'a, LinkContext> {
    let plugin = f
        .def
        .checksum
        .as_ref()
        .filter(|c| !base.checksum_ids.contains(c) && plugin_checksum(c));
    if f.pack_ecudefs.is_empty() && plugin.is_none() {
        return Cow::Borrowed(base);
    }
    let mut c = base.clone();
    c.ecudefs.extend(f.pack_ecudefs.iter().cloned());
    c.checksum_ids.extend(plugin.cloned());
    Cow::Owned(c)
}

/// Every validation issue of `f`: structure, honesty, file name and links.
pub fn all_issues(f: &Found, ctx: &LinkContext) -> Vec<Issue> {
    let stem = f.path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    f.def
        .validate()
        .into_iter()
        .chain(f.def.validate_filename(stem))
        .chain(f.def.validate_links(ctx))
        .collect()
}

pub fn issue_json(i: &Issue) -> Value {
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

/// One list row: identity, source, pack, counts, error / warning counts and
/// the related tools.
pub fn row(f: &Found, issues: &[Issue]) -> Value {
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

/// Every definition of the [`catalog`] (`builtin` dir, packs under
/// `packs_root`, `extra` pack folders) filtered by `kind` and the query `q`,
/// each row validated against `base` (see [`ctx_for`]).
pub fn list(
    builtin: Option<&Path>,
    packs_root: Option<&Path>,
    base: &LinkContext,
    plugin_checksum: &dyn Fn(&str) -> bool,
    q: Option<&str>,
    kind: Option<&str>,
    extra: &[String],
) -> Value {
    let (found, failed) = catalog(builtin, packs_root, extra);
    let total = found.len();
    let defs: Vec<Value> = found
        .iter()
        .filter(|f| {
            kind.map(str::trim)
                .filter(|k| !k.is_empty())
                .map_or(true, |k| kind_str(&f.def) == k)
        })
        // `map_or(true, ..)` here and above is `Option::is_none_or` (Rust
        // 1.82), which the desktop used; the workspace MSRV is 1.75.
        .filter(|f| q.map_or(true, |q| matches_query(&f.def, q)))
        .map(|f| row(f, &all_issues(f, &ctx_for(base, plugin_checksum, f))))
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

/// Load one definition file and tell where it came from: the `builtin`
/// directory, a pack (`<pack>/controllers/<id>.json`) or a loose file.
pub fn open_found(builtin: Option<&Path>, path: &Path) -> Result<Found, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read '{}': {e}", path.display()))?;
    let def = ControllerDef::from_json(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let builtin = builtin
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

/// One definition file: its list [`row`] plus the definition as stored
/// (`def`) and every validation issue (`issues`).
pub fn get(
    builtin: Option<&Path>,
    base: &LinkContext,
    plugin_checksum: &dyn Fn(&str) -> bool,
    path: &Path,
) -> Result<Value, String> {
    let f = open_found(builtin, path)?;
    let issues = all_issues(&f, &ctx_for(base, plugin_checksum, &f));
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
pub fn decode_text(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(rel)
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
