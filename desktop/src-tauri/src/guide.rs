//! In-app guide: the bundled Markdown pages (rendered by the frontend).

use crate::state::AppData;
use serde_json::{json, Value};
use std::path::PathBuf;

/// Pretty section title from a `NN-slug.md` filename: `01-hardware.md` -> "Hardware".
fn guide_title(stem: &str) -> String {
    let slug = stem.split_once('-').map(|x| x.1).unwrap_or(stem);
    let mut out = String::new();
    for (i, word) in slug.split(['-', '_']).enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let mut cs = word.chars();
        if let Some(f) = cs.next() {
            out.extend(f.to_uppercase());
            out.push_str(cs.as_str());
        }
    }
    out
}

#[tauri::command]
pub async fn guide_list(state: tauri::State<'_, AppData>) -> Result<Vec<Value>, String> {
    let dir = state.guide.as_ref().ok_or("guide resources not found")?;
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
        .collect();
    files.sort();
    Ok(files
        .iter()
        .filter_map(|p| {
            let stem = p.file_stem()?.to_str()?;
            Some(json!({ "id": stem, "title": guide_title(stem) }))
        })
        .collect())
}

#[tauri::command]
pub async fn guide_read(id: String, state: tauri::State<'_, AppData>) -> Result<String, String> {
    let dir = state.guide.as_ref().ok_or("guide resources not found")?;
    // Guard against path traversal: only a bare NN-slug stem is accepted.
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("invalid guide id".into());
    }
    let path = dir.join(format!("{id}.md"));
    std::fs::read_to_string(&path).map_err(|e| e.to_string())
}
