//! Where the bundled public data folders (ecudefs, database, ctrldefs,
//! catalog, packs, guide) are looked for, in order:
//!
//! 1. `./<name>` and `../<name>` (running from a repository checkout)
//! 2. `<exe dir>/<name>` (AppImage, Windows zip)
//! 3. `<exe dir>/../Resources/<name>` (macOS .app bundle)
//! 4. `<exe dir>/../share/opentune/<name>` (a Unix `prefix` install)

use crate::host::DataRoot;
use std::path::{Path, PathBuf};

pub fn candidates(name: &str) -> Vec<PathBuf> {
    let mut c: Vec<PathBuf> = vec![PathBuf::from(name), Path::new("..").join(name)];
    if let Some(d) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        c.extend(exe_relative(&d, name));
    }
    c
}

fn exe_relative(exe_dir: &Path, name: &str) -> [PathBuf; 3] {
    [
        exe_dir.join(name),
        exe_dir.join("..").join("Resources").join(name),
        exe_dir.join("..").join("share").join("opentune").join(name),
    ]
}

/// The first existing directory for `name`.
pub fn find(name: &str) -> Option<PathBuf> {
    candidates(name).into_iter().find(|p| p.is_dir())
}

/// Where the guide lives in a repository checkout (it is a Tauri resource,
/// `desktop/src-tauri/guide`), after the usual places.
fn find_guide() -> Option<PathBuf> {
    find("guide").or_else(|| {
        [".", ".."]
            .iter()
            .map(|r| Path::new(r).join("desktop").join("src-tauri").join("guide"))
            .find(|p| p.is_dir())
    })
}

/// Every bundled data folder, looked up once at start-up.
#[derive(Clone, Debug, Default)]
pub struct DataRoots(Vec<(DataRoot, PathBuf)>);

impl DataRoots {
    pub fn resolve() -> Self {
        Self(
            DataRoot::ALL
                .iter()
                .filter_map(|&r| {
                    let p = match r {
                        DataRoot::Guide => find_guide(),
                        _ => find(r.dir_name()),
                    };
                    p.map(|p| (r, p))
                })
                .collect(),
        )
    }

    pub fn get(&self, root: DataRoot) -> Option<&Path> {
        self.0
            .iter()
            .find(|(r, _)| *r == root)
            .map(|(_, p)| p.as_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_layouts_are_covered() {
        let r = exe_relative(Path::new("/A/OpenTune.app/Contents/MacOS"), "ecudefs");
        assert_eq!(r[0], Path::new("/A/OpenTune.app/Contents/MacOS/ecudefs"));
        assert_eq!(
            r[1],
            Path::new("/A/OpenTune.app/Contents/MacOS/../Resources/ecudefs")
        );
        assert_eq!(
            r[2],
            Path::new("/A/OpenTune.app/Contents/MacOS/../share/opentune/ecudefs")
        );
        let c = candidates("database");
        assert_eq!(c[0], Path::new("database"));
        assert_eq!(c[1], Path::new("../database"));
    }
}
