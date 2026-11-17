//! Tuning project files (`.otproj`).

use crate::common::write_error;
use opentune_core::ecudef::MapDef;
use serde::Serialize;
use serde_json::{json, Value};

// A project is a small JSON side-file that records *what you changed* against a
// base flash image — never a copy of the image itself. It bundles:
//   * the base image path plus a content fingerprint (size + a fast FNV-1a hash)
//     so a later open can detect that the referenced image has changed,
//   * the selected ECU definition + checksum algorithm,
//   * one edit record per map you touched (the edited physical-value grid, the
//     originally-decoded baseline grid, and free-text notes).
//
// On open the backend re-reads the referenced image, re-fingerprints it, and
// reports whether it still matches. The *values* are restored verbatim onto a
// freshly-decoded map (see the frontend): re-apply is absolute-value restore,
// not a delta replay, so a fingerprint mismatch is surfaced as a hard warning.

const PROJECT_FORMAT: &str = "opentune-project";
const PROJECT_VERSION: u32 = 1;

/// 64-bit FNV-1a over the whole file. This is a fast *change detector*, not a
/// cryptographic hash — it only has to catch "this is a different image".
fn fnv1a64(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Read a file and return `(size, hex-fingerprint)` for mismatch detection.
fn file_fingerprint(path: &str) -> Result<(u64, String), String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    Ok((data.len() as u64, format!("{:016x}", fnv1a64(&data))))
}

/// Base-image reference: path + the fingerprint captured when the project was saved.
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
struct BaseImageRef {
    #[serde(default)]
    path: String,
    #[serde(default)]
    size: u64,
    /// Hex FNV-1a of the image at save time; empty when unknown.
    #[serde(default)]
    hash: String,
}

/// One edited map inside a project. Either definition-backed (`def_id` +
/// `map_name`) or ad-hoc (`mapdef` carries the full MapDef). `cells` is the
/// edited physical-value grid; `baseline` is the originally-decoded grid, kept so
/// the UI can show a change count without re-reading the image.
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
struct MapEdit {
    #[serde(default)]
    kind: String, // "def" | "adhoc"
    #[serde(default)]
    def_id: Option<String>,
    #[serde(default)]
    map_name: String,
    #[serde(default)]
    mapdef: Option<MapDef>,
    #[serde(default)]
    cells: Vec<Vec<f64>>,
    #[serde(default)]
    baseline: Vec<Vec<f64>>,
    #[serde(default)]
    notes: String,
}

/// A whole tuning session as saved to disk. All fields default so an older or
/// hand-written project still loads.
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
pub(crate) struct Project {
    #[serde(default)]
    format: String,
    #[serde(default)]
    version: u32,
    #[serde(default)]
    name: String,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    ecudef_id: Option<String>,
    #[serde(default)]
    checksum_algo: Option<String>,
    #[serde(default)]
    base_image: BaseImageRef,
    #[serde(default)]
    edits: Vec<MapEdit>,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    saved: Option<String>,
}

/// Save a tuning project to `path`. The backend authoritatively (re)computes the
/// base-image fingerprint from the current file so the saved hash always matches
/// what a later [`project_open`] will recompute, and stamps the format/version.
/// Returns a small summary for the UI.
#[tauri::command]
pub fn project_save(path: String, mut project: Project) -> Result<Value, String> {
    project.format = PROJECT_FORMAT.to_string();
    if project.version == 0 {
        project.version = PROJECT_VERSION;
    }
    // Refresh the base-image fingerprint from the referenced file when it exists;
    // if it cannot be read we keep whatever the caller supplied (so a project can
    // still be saved when the image is temporarily unavailable).
    let mut image_ok = false;
    if !project.base_image.path.trim().is_empty() {
        if let Ok((size, hash)) = file_fingerprint(&project.base_image.path) {
            project.base_image.size = size;
            project.base_image.hash = hash;
            image_ok = true;
        }
    }

    let json = serde_json::to_string_pretty(&project).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| write_error(&path, &e))?;

    Ok(json!({
        "path": path,
        "name": project.name,
        "edits": project.edits.len(),
        "image_fingerprinted": image_ok,
        "base_image": {
            "path": project.base_image.path,
            "size": project.base_image.size,
            "hash": project.base_image.hash,
        },
    }))
}

/// Open a tuning project from `path`. Parses it, then re-fingerprints the
/// referenced base image and reports `image_status`:
///   * `"match"`    — the image is present and its fingerprint matches,
///   * `"mismatch"` — present but changed since the project was saved,
///   * `"missing"`  — the referenced path cannot be read,
///   * `"no_image"` — the project records no base image,
/// so the frontend can warn before the stored edits are restored onto it.
#[tauri::command]
pub fn project_open(path: String) -> Result<Value, String> {
    let txt = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let project: Project = serde_json::from_str(&txt)
        .map_err(|e| format!("not a valid OpenTune project ({path}): {e}"))?;

    let img_path = project.base_image.path.clone();
    let stored_hash = project.base_image.hash.clone();
    let stored_size = project.base_image.size;

    let (image_status, current_size, current_hash) = if img_path.trim().is_empty() {
        ("no_image", None, None)
    } else {
        match file_fingerprint(&img_path) {
            Ok((size, hash)) => {
                let matches = if stored_hash.is_empty() {
                    // No stored hash to compare against — fall back to a size check.
                    stored_size == 0 || stored_size == size
                } else {
                    hash == stored_hash
                };
                (
                    if matches { "match" } else { "mismatch" },
                    Some(size),
                    Some(hash),
                )
            }
            Err(_) => ("missing", None, None),
        }
    };

    let project_val = serde_json::to_value(&project).map_err(|e| e.to_string())?;
    Ok(json!({
        "path": path,
        "project": project_val,
        "image_status": image_status,
        "image_path": img_path,
        "stored_size": stored_size,
        "stored_hash": stored_hash,
        "current_size": current_size,
        "current_hash": current_hash,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::testutil::TmpDir;

    fn sample_project(img_path: &str) -> Project {
        Project {
            name: "demo tune".into(),
            notes: "stage 1".into(),
            ecudef_id: Some("edc16-demo".into()),
            checksum_algo: Some("edc16".into()),
            base_image: BaseImageRef {
                path: img_path.to_string(),
                ..Default::default()
            },
            edits: vec![MapEdit {
                kind: "def".into(),
                def_id: Some("edc16-demo".into()),
                map_name: "injection limiter".into(),
                cells: vec![vec![10.0, 12.5], vec![14.0, 16.5]],
                baseline: vec![vec![10.0, 10.0], vec![10.0, 10.0]],
                notes: "+25% top end".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn fnv1a_detects_change() {
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325); // FNV-1a 64 offset basis
        assert_ne!(fnv1a64(b"abc"), fnv1a64(b"abd"));
        assert_eq!(fnv1a64(b"opentune"), fnv1a64(b"opentune"));
    }

    #[test]
    fn project_round_trips_and_image_matches() {
        let dir = TmpDir::new("rt");
        let img = dir.join("stock.bin");
        std::fs::write(&img, vec![0xAAu8; 2048]).unwrap();
        let proj_path = dir.join("tune.otproj");

        let save = project_save(
            proj_path.to_string_lossy().into_owned(),
            sample_project(&img.to_string_lossy()),
        )
        .expect("save ok");
        assert_eq!(save["edits"], 1);
        assert_eq!(save["image_fingerprinted"], true);
        // The save authoritatively fills size + a non-empty hash.
        assert_eq!(save["base_image"]["size"], 2048);
        assert!(save["base_image"]["hash"].as_str().unwrap().len() == 16);

        let open = project_open(proj_path.to_string_lossy().into_owned()).expect("open ok");
        assert_eq!(open["image_status"], "match");
        let p = &open["project"];
        assert_eq!(p["format"], PROJECT_FORMAT);
        assert_eq!(p["version"], PROJECT_VERSION);
        assert_eq!(p["name"], "demo tune");
        assert_eq!(p["ecudef_id"], "edc16-demo");
        assert_eq!(p["checksum_algo"], "edc16");
        // Edits survive verbatim.
        assert_eq!(p["edits"][0]["map_name"], "injection limiter");
        assert_eq!(p["edits"][0]["cells"][1][1], 16.5);
        assert_eq!(p["edits"][0]["baseline"][0][0], 10.0);
        assert_eq!(p["edits"][0]["notes"], "+25% top end");
    }

    #[test]
    fn project_open_flags_image_mismatch() {
        let dir = TmpDir::new("mismatch");
        let img = dir.join("stock.bin");
        std::fs::write(&img, vec![0x00u8; 1024]).unwrap();
        let proj_path = dir.join("tune.otproj");
        project_save(
            proj_path.to_string_lossy().into_owned(),
            sample_project(&img.to_string_lossy()),
        )
        .unwrap();

        // Change the referenced image after saving the project.
        std::fs::write(&img, vec![0xFFu8; 1024]).unwrap();
        let open = project_open(proj_path.to_string_lossy().into_owned()).unwrap();
        assert_eq!(open["image_status"], "mismatch");
        assert_ne!(open["current_hash"], open["stored_hash"]);

        // A size change is likewise a mismatch.
        std::fs::write(&img, vec![0xFFu8; 2048]).unwrap();
        let open2 = project_open(proj_path.to_string_lossy().into_owned()).unwrap();
        assert_eq!(open2["image_status"], "mismatch");
        assert_eq!(open2["current_size"], 2048);
    }

    #[test]
    fn project_open_reports_missing_and_no_image() {
        let dir = TmpDir::new("missing");
        let proj_path = dir.join("tune.otproj");

        // Referenced image that does not exist.
        project_save(
            proj_path.to_string_lossy().into_owned(),
            sample_project(&dir.join("gone.bin").to_string_lossy()),
        )
        .unwrap();
        let open = project_open(proj_path.to_string_lossy().into_owned()).unwrap();
        assert_eq!(open["image_status"], "missing");

        // A project with no base image at all.
        let mut p = sample_project("");
        p.base_image = BaseImageRef::default();
        let proj2 = dir.join("noimg.otproj");
        project_save(proj2.to_string_lossy().into_owned(), p).unwrap();
        let open2 = project_open(proj2.to_string_lossy().into_owned()).unwrap();
        assert_eq!(open2["image_status"], "no_image");
    }

    #[test]
    fn project_open_rejects_garbage() {
        let dir = TmpDir::new("garbage");
        let bad = dir.join("bad.otproj");
        std::fs::write(&bad, b"this is not json").unwrap();
        assert!(project_open(bad.to_string_lossy().into_owned()).is_err());
    }
}
