//! Saving a file the user picked in a save dialog (an exported CSV, a `.msq`
//! tune, a capture's output file), shared by the N7 panels.
//!
//! A NEW file is written directly. Replacing an EXISTING file asks first
//! ([`Host::confirm`]) and then goes through [`Host::write_file`], which
//! archives the current contents in the vault before the atomic replace, so a
//! save can never silently destroy data.

use crate::host::{ConfirmRequest, ConfirmResult, Host};
use std::path::{Path, PathBuf};

struct Pending {
    key: String,
    path: PathBuf,
    data: Vec<u8>,
}

/// One save in flight (at most one per panel).
#[derive(Default)]
pub(crate) struct Saver {
    pending: Option<Pending>,
}

impl Saver {
    /// Save `data` to `path`. `Some` when it finished at once (a new file);
    /// `None` while the replace confirmation is open (see [`Saver::tick`]).
    pub(crate) fn save(
        &mut self,
        key: &str,
        path: PathBuf,
        data: Vec<u8>,
    ) -> Option<Result<PathBuf, String>> {
        if path.exists() {
            self.pending = Some(Pending {
                key: key.into(),
                path,
                data,
            });
            return None;
        }
        Some(write_new(&path, &data).map(|_| path))
    }

    /// Is a replace confirmation open?
    pub(crate) fn busy(&self) -> bool {
        self.pending.is_some()
    }

    /// Drive the replace confirmation; `Some` once it is answered.
    pub(crate) fn tick(&mut self, host: &mut Host<'_>) -> Option<Result<PathBuf, String>> {
        let p = self.pending.as_ref()?;
        let req = ConfirmRequest {
            key: p.key.clone(),
            title: "Replace the file?".into(),
            body: format!(
                "{} exists. Its current contents are archived in the vault first.",
                p.path.display()
            ),
            typed: None,
            confirm_label: "Replace".into(),
        };
        match host.confirm(req) {
            ConfirmResult::Pending => None,
            ConfirmResult::Declined => {
                self.pending = None;
                Some(Err("not saved: cancelled".into()))
            }
            ConfirmResult::Accepted(ack) => {
                let p = self.pending.take()?;
                Some(
                    host.write_file(ack, &p.path, &p.data)
                        .map(|_| p.path)
                        .map_err(|e| format!("not saved: {e}")),
                )
            }
        }
    }
}

/// Create `path` (it must not exist yet) and write `data`.
fn write_new(path: &Path, data: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    f.write_all(data)
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::DiagTarget;
    use crate::views::coding::simkit::Fixture;

    #[test]
    fn new_files_are_written_and_replacements_are_confirmed_and_archived() {
        let mut fx = Fixture::new("n7-save");
        let p = fx.dir.join("out.csv");
        let mut s = Saver::default();
        assert_eq!(s.save("k", p.clone(), b"one".to_vec()), Some(Ok(p.clone())));
        assert_eq!(std::fs::read(&p).unwrap(), b"one");
        // Existing: nothing happens before the confirmation.
        assert_eq!(s.save("k", p.clone(), b"two".to_vec()), None);
        assert!(s.busy());
        assert!(fx.host(DiagTarget::ENGINE, |h| s.tick(h)).is_none());
        assert_eq!(std::fs::read(&p).unwrap(), b"one");
        fx.confirms.answer("k", true);
        let r = fx.host(DiagTarget::ENGINE, |h| s.tick(h)).unwrap();
        assert_eq!(r, Ok(p.clone()));
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        let vault = crate::views::coding::plumbing::vault_dir(&p);
        let archived = std::fs::read_dir(&vault).unwrap().count();
        assert!(archived >= 1, "the old contents are in the vault");
        // Declined: unchanged.
        s.save("k", p.clone(), b"three".to_vec());
        fx.host(DiagTarget::ENGINE, |h| s.tick(h));
        fx.confirms.answer("k", false);
        assert!(fx.host(DiagTarget::ENGINE, |h| s.tick(h)).unwrap().is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
    }
}
