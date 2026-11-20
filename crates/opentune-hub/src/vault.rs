//! Backup vault listing: the `.bin` archives (and their `.meta` sidecars) in a
//! vault directory, as the desktop's Vault panel shows them.

use serde_json::{json, Value};
use std::path::Path;

/// List the vault directory `dir`: one entry per `.bin` archive (name, path,
/// size, whether a `.meta` sidecar exists and its text), sorted by name. A
/// missing directory is an empty vault.
pub fn list(dir: &str) -> Result<Vec<Value>, String> {
    let p = Path::new(dir);
    if !p.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(p).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().and_then(|s| s.to_str()) != Some("bin") {
            continue;
        }
        let bin_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let meta_path = path.with_extension("meta");
        let meta_txt = std::fs::read_to_string(&meta_path).unwrap_or_default();
        out.push(json!({
            "name": path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
            "path": path.display().to_string(),
            "size": bin_len,
            "has_meta": meta_path.exists(),
            "meta": meta_txt,
        }));
    }
    out.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });
    Ok(out)
}
