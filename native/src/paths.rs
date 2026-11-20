//! Where the bundled public data folders (ecudefs, database, ctrldefs) are
//! looked for, in order:
//!
//! 1. `./<name>` and `../<name>` (running from a repository checkout)
//! 2. `<exe dir>/<name>` (AppImage, Windows zip)
//! 3. `<exe dir>/../Resources/<name>` (macOS .app bundle)
//! 4. `<exe dir>/../share/opentune/<name>` (a Unix `prefix` install)

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
