//! Database feature — the offline control-unit reference (ECU / TCU / AWD).
//!
//! Backs the "ECU database" panel (`desktop/ui/src/feat_database.js`) with two
//! read-only commands over the bundled `database/` resource (resolved like
//! `ecudefs/` and `catalog/`, see [`crate::AppData`]):
//!
//! * [`db_list`] — `{kind?, q?}` → compact rows (`id, kind, maker, family,
//!   name, cpu, flash_kib, bus, read_methods, checksum, ecudef, vehicle_count,
//!   has_pinout, pinout_confidence`). `q` matches name / family / maker / part
//!   numbers / vehicles, case-insensitively.
//! * [`db_get`] — `{id}` → the unit JSON exactly as stored.
//!
//! The directory is small (tens of files), so it is re-read per call: edits to
//! the JSON show up without restarting the app.

use opentune_db::{Database, UnitSummary};
use serde_json::Value;
use std::path::{Path, PathBuf};

fn load(dir: &Option<PathBuf>) -> Result<Database, String> {
    let dir = dir
        .as_deref()
        .ok_or("database resources not found (set OPENTUNE_DATA or reinstall)")?;
    load_from(dir)
}

fn load_from(dir: &Path) -> Result<Database, String> {
    Database::load_dir(dir).map_err(|e| e.to_string())
}

fn list_impl(db: &Database, kind: Option<&str>, q: Option<&str>) -> Vec<UnitSummary> {
    db.list(kind, q)
}

fn get_impl(db: &Database, id: &str) -> Result<Value, String> {
    db.get(id)
        .cloned()
        .ok_or_else(|| format!("unknown unit '{id}'"))
}

/// List control units, optionally filtered by kind (`ecu`/`tcu`/`awd`) and a
/// free-text query. Async, like [`db_get`]: the directory is read per call and
/// must not hold the UI thread.
#[tauri::command]
pub async fn db_list(
    kind: Option<String>,
    q: Option<String>,
    state: tauri::State<'_, crate::AppData>,
) -> Result<Vec<UnitSummary>, String> {
    let db = load(&state.database)?;
    Ok(list_impl(&db, kind.as_deref(), q.as_deref()))
}

/// One control unit, exactly as stored in `database/units/<id>.json`.
#[tauri::command]
pub async fn db_get(id: String, state: tauri::State<'_, crate::AppData>) -> Result<Value, String> {
    let db = load(&state.database)?;
    get_impl(&db, &id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_db() -> Database {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../database");
        load_from(&dir).expect("bundled database loads")
    }

    #[test]
    fn list_rows_carry_the_ui_contract_fields() {
        let db = repo_db();
        let rows = list_impl(&db, None, None);
        assert!(!rows.is_empty());
        let v = serde_json::to_value(&rows[0]).unwrap();
        for k in [
            "id",
            "kind",
            "maker",
            "family",
            "name",
            "cpu",
            "flash_kib",
            "bus",
            "read_methods",
            "checksum",
            "ecudef",
            "vehicle_count",
            "has_pinout",
            "pinout_confidence",
        ] {
            assert!(v.get(k).is_some(), "db_list row lacks '{k}'");
        }
    }

    #[test]
    fn kind_and_query_filter() {
        let db = repo_db();
        let tcus = list_impl(&db, Some("tcu"), None);
        assert!(!tcus.is_empty());
        assert!(tcus.iter().all(|r| r.kind.as_str() == "tcu"));
        let hits = list_impl(&db, None, Some("edc16u34"));
        assert!(hits.iter().any(|r| r.id == "bosch-edc16u34-vag"));
        assert!(list_impl(&db, None, Some("no-such-unit-xyz")).is_empty());
    }

    #[test]
    fn get_returns_stored_json_or_error() {
        let db = repo_db();
        let u = get_impl(&db, "bosch-edc17cp14-vag").unwrap();
        assert_eq!(u["id"], "bosch-edc17cp14-vag");
        assert!(u["connectors"].is_array());
        assert!(get_impl(&db, "nope").is_err());
    }
}
