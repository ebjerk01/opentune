//! [`Loader`]: a user's definitions file plus the provider(s) the user chose,
//! answering seed/key ids `unlockecu:<EcuName or alias>` through OpenTune's
//! [`SeedKey`] hook.
use crate::defs::{DefinitionSet, ParamValue};
use crate::provider::{ExternalProvider, DEFAULT_TIMEOUT};
use crate::scope;
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::{Error, Result};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Seed/key ids this loader answers start with this prefix.
pub const ID_PREFIX: &str = "unlockecu:";
/// The provider spec that enables the built-in SA2 interpreter.
pub const BUILTIN_SA2: &str = "builtin:sa2";
/// The `Provider` name the built-in SA2 interpreter serves.
pub const SA2_PROVIDER: &str = "VolkswagenSA2";
/// The `ByteArray` parameter holding the user's SA2 script.
pub const SA2_TAPE_PARAM: &str = "InstructionTape";
/// Longest SA2 script accepted from a definition (bytes).
pub const MAX_SA2_TAPE: usize = 4096;

/// One provider the user configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSpec {
    /// `builtin:sa2`: definitions whose `Provider` is [`SA2_PROVIDER`] run on
    /// the built-in SA2 VM over their own [`SA2_TAPE_PARAM`].
    BuiltinSa2,
    /// An external provider program (absolute path).
    Program(PathBuf),
}

impl ProviderSpec {
    /// Parse `builtin:sa2` or an absolute program path.
    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        let s = s.trim();
        if s.eq_ignore_ascii_case(BUILTIN_SA2) {
            return Ok(ProviderSpec::BuiltinSa2);
        }
        if s.starts_with("builtin:") {
            return Err(format!(
                "unknown built-in provider '{s}': the only one is '{BUILTIN_SA2}' (OpenTune ships no algorithm)"
            ));
        }
        let p = PathBuf::from(s);
        if s.is_empty() || !p.is_absolute() {
            return Err(format!(
                "provider '{s}' must be '{BUILTIN_SA2}' or the absolute path of your provider program"
            ));
        }
        Ok(ProviderSpec::Program(p))
    }
}

impl fmt::Display for ProviderSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderSpec::BuiltinSa2 => f.write_str(BUILTIN_SA2),
            ProviderSpec::Program(p) => write!(f, "{}", p.display()),
        }
    }
}

/// What the user configured: a definitions file and at least one provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderConfig {
    /// The user's definitions file.
    pub definitions: PathBuf,
    /// The providers: `builtin:sa2` and/or one external program.
    pub providers: Vec<ProviderSpec>,
    /// How long an external program may take per key.
    pub timeout: Duration,
}

impl LoaderConfig {
    /// A config with the default provider timeout.
    pub fn new(definitions: PathBuf, providers: Vec<ProviderSpec>) -> Self {
        Self {
            definitions,
            providers,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

struct Inner {
    source: PathBuf,
    set: DefinitionSet,
    sa2: bool,
    program: Option<ExternalProvider>,
}

/// A loaded definitions file and its providers. Cheap to clone.
#[derive(Clone)]
pub struct Loader(Arc<Inner>);

impl fmt::Debug for Loader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Loader")
            .field("source", &self.0.source)
            .field("definitions", &self.0.set.definitions.len())
            .field("refused", &self.0.set.refused.len())
            .field("rejected", &self.0.set.rejected.len())
            .field("sa2", &self.0.sa2)
            .field("program", &self.0.program.as_ref().map(|p| p.program()))
            .finish()
    }
}

impl Loader {
    /// Load `cfg.definitions` and set up `cfg.providers`. Fails unless both a
    /// definitions file and at least one provider are given (the loader is
    /// off by default), on more than one external program, a bad program
    /// path, or a file that cannot be read as definitions.
    pub fn new(cfg: &LoaderConfig) -> std::result::Result<Self, String> {
        if cfg.providers.is_empty() {
            return Err(format!(
                "seed/key definitions file {} given without a provider: the loader stays off \
                 until you configure both a definitions file and a provider ('{BUILTIN_SA2}' or \
                 the absolute path of your provider program)",
                cfg.definitions.display()
            ));
        }
        let mut sa2 = false;
        let mut program = None;
        for p in &cfg.providers {
            match p {
                ProviderSpec::BuiltinSa2 => sa2 = true,
                ProviderSpec::Program(path) => {
                    if program.is_some() {
                        return Err("configure at most one external provider program".into());
                    }
                    program = Some(ExternalProvider::new(path.clone(), cfg.timeout)?);
                }
            }
        }
        let set = DefinitionSet::load(&cfg.definitions)?;
        Ok(Loader(Arc::new(Inner {
            source: cfg.definitions.clone(),
            set,
            sa2,
            program,
        })))
    }

    /// The loaded definitions (with what was refused and rejected).
    pub fn definitions(&self) -> &DefinitionSet {
        &self.0.set
    }

    /// A human summary: one line of counts, then one line per refused and
    /// rejected entry. Never contains parameter values.
    pub fn report(&self) -> Vec<String> {
        let s = &self.0.set;
        let mut lines = vec![format!(
            "seed/key definitions {}: {} loaded, {} refused (immobiliser / anti-theft scope), {} rejected; providers: {}",
            self.0.source.display(),
            s.definitions.len(),
            s.refused.len(),
            s.rejected.len(),
            self.providers()
        )];
        lines.extend(s.refused.iter().map(ToString::to_string));
        lines.extend(s.rejected.iter().map(ToString::to_string));
        lines
    }

    fn providers(&self) -> String {
        let mut v = Vec::new();
        if self.0.sa2 {
            v.push(BUILTIN_SA2.to_string());
        }
        if let Some(p) = &self.0.program {
            v.push(p.program().display().to_string());
        }
        v.join(", ")
    }

    /// Whether `id` is one of this loader's ids (`unlockecu:...`).
    pub fn handles(id: &str) -> bool {
        id.trim().starts_with(ID_PREFIX)
    }

    /// The handler for `unlockecu:<name>`: `None` when `id` is not of that
    /// form, `Some(Err)` when the name is refused or no definition answers
    /// to it.
    pub fn seed_key(&self, id: &str) -> Option<std::result::Result<LoadedSeedKey, String>> {
        let id = id.trim();
        let name = id.strip_prefix(ID_PREFIX)?.trim();
        if name.is_empty() {
            return Some(Err(format!(
                "seed-key id '{id}': no ECU name after '{ID_PREFIX}'"
            )));
        }
        if let Some(term) = scope::refused_term(name) {
            return Some(Err(format!(
                "seed-key id '{id}': '{name}' matches '{term}' — {}",
                scope::SCOPE_NOTE
            )));
        }
        if self.0.set.lookup(name).is_empty() {
            let refused = self
                .0
                .set
                .refused
                .iter()
                .any(|r| r.ecu_name.eq_ignore_ascii_case(name));
            return Some(Err(if refused {
                format!(
                    "seed-key id '{id}': that definition was {}",
                    scope::SCOPE_NOTE
                )
            } else {
                format!(
                    "seed-key id '{id}': no definition in {} has EcuName or alias '{name}'",
                    self.0.source.display()
                )
            }));
        }
        Some(Ok(LoadedSeedKey {
            id: id.to_string(),
            name: name.to_string(),
            inner: Arc::clone(&self.0),
        }))
    }
}

/// The [`SeedKey`] for one ECU name: picks the definition for the requested
/// access level and runs its provider.
pub struct LoadedSeedKey {
    id: String,
    name: String,
    inner: Arc<Inner>,
}

impl fmt::Debug for LoadedSeedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadedSeedKey")
            .field("id", &self.id)
            .finish()
    }
}

impl SeedKey for LoadedSeedKey {
    fn id(&self) -> &str {
        &self.id
    }

    fn compute(&self, seed: &[u8], level: u8) -> Result<Vec<u8>> {
        let defs = self.inner.set.lookup(&self.name);
        let Some(def) = defs.iter().find(|d| d.access_level == level) else {
            let have: Vec<String> = defs
                .iter()
                .map(|d| format!("0x{:02X}", d.access_level))
                .collect();
            return Err(Error::Config(format!(
                "'{}' has no definition for AccessLevel 0x{level:02X} (defined: {})",
                self.name,
                have.join(", ")
            )));
        };
        if seed.len() != def.seed_length {
            return Err(Error::Protocol(format!(
                "seed is {} bytes; definition #{} ('{}') expects SeedLength {}",
                seed.len(),
                def.index,
                def.ecu_name,
                def.seed_length
            )));
        }
        let key = if self.inner.sa2 && def.provider.eq_ignore_ascii_case(SA2_PROVIDER) {
            sa2(def, seed, level)?
        } else if let Some(p) = &self.inner.program {
            p.compute(def, seed, level).map_err(Error::Protocol)?
        } else {
            return Err(Error::Config(format!(
                "provider '{}' is not available: '{BUILTIN_SA2}' runs only '{SA2_PROVIDER}'; \
                 configure your own provider program for it (OpenTune ships no algorithm)",
                def.provider
            )));
        };
        if key.len() != def.key_length {
            return Err(Error::Protocol(format!(
                "provider returned a {}-byte key; definition #{} says KeyLength {}",
                key.len(),
                def.index,
                def.key_length
            )));
        }
        Ok(key)
    }
}

/// The built-in SA2 route: the definition's own InstructionTape on the VM.
fn sa2(def: &crate::defs::Definition, seed: &[u8], level: u8) -> Result<Vec<u8>> {
    if def.seed_length != 4 || def.key_length != 4 {
        return Err(Error::Config(format!(
            "definition #{}: {SA2_PROVIDER} needs SeedLength 4 and KeyLength 4",
            def.index
        )));
    }
    let Some(ParamValue::ByteArray(tape)) = def.param(SA2_TAPE_PARAM) else {
        return Err(Error::Config(format!(
            "definition #{}: {SA2_PROVIDER} needs a ByteArray parameter '{SA2_TAPE_PARAM}' \
             (your own SA2 script; OpenTune ships none)",
            def.index
        )));
    };
    if tape.is_empty() || tape.len() > MAX_SA2_TAPE {
        return Err(Error::Config(format!(
            "definition #{}: '{SA2_TAPE_PARAM}' must be 1..={MAX_SA2_TAPE} bytes",
            def.index
        )));
    }
    opentune_sa2::Sa2SeedKey::new(tape.clone()).compute(seed, level)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tmp(tag: &str) -> Tmp {
        let d = std::env::temp_dir().join(format!("ot-skl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Tmp(d)
    }

    // Synthetic definitions: a SA2 tape written for the test (eor 0xA5A5A5A5;
    // finish), an entry for an external provider, and a refused one.
    const DEFS: &str = r#"[
      {"EcuName":"SIM-SA2","Aliases":["SIMALT"],"AccessLevel":17,"SeedLength":4,"KeyLength":4,
       "Provider":"VolkswagenSA2","Origin":"synthetic",
       "Parameters":[{"Key":"InstructionTape","Value":"87A5A5A5A54C","DataType":"ByteArray"}]},
      {"EcuName":"SIM-SA2-NOTAPE","AccessLevel":17,"SeedLength":4,"KeyLength":4,"Provider":"VolkswagenSA2"},
      {"EcuName":"SIM-EXT","AccessLevel":1,"SeedLength":2,"KeyLength":2,"Provider":"DemoXor"},
      {"EcuName":"EZS-SIM","AccessLevel":1,"SeedLength":4,"KeyLength":4,"Provider":"DemoXor"}
    ]"#;

    fn sa2_loader(t: &Tmp) -> Loader {
        let f = t.0.join("defs.json");
        std::fs::write(&f, DEFS).unwrap();
        Loader::new(&LoaderConfig::new(f, vec![ProviderSpec::BuiltinSa2])).unwrap()
    }

    #[test]
    fn off_without_a_provider() {
        let t = tmp("off");
        let f = t.0.join("defs.json");
        std::fs::write(&f, DEFS).unwrap();
        let e = Loader::new(&LoaderConfig::new(f, vec![])).unwrap_err();
        assert!(
            e.contains("without a provider") && e.contains("stays off"),
            "{e}"
        );
    }

    #[test]
    fn provider_specs() {
        assert_eq!(
            ProviderSpec::parse(" builtin:SA2 ").unwrap(),
            ProviderSpec::BuiltinSa2
        );
        assert!(ProviderSpec::parse("builtin:other")
            .unwrap_err()
            .contains("only one"));
        assert!(ProviderSpec::parse("my-provider")
            .unwrap_err()
            .contains("absolute"));
        assert!(ProviderSpec::parse("").is_err());
        let abs = std::env::temp_dir().join("p");
        assert_eq!(
            ProviderSpec::parse(abs.to_str().unwrap()).unwrap(),
            ProviderSpec::Program(abs)
        );
        let t = tmp("two");
        let f = t.0.join("defs.json");
        std::fs::write(&f, DEFS).unwrap();
        let exe = std::env::current_exe().unwrap();
        let e = Loader::new(&LoaderConfig::new(
            f,
            vec![
                ProviderSpec::Program(exe.clone()),
                ProviderSpec::Program(exe),
            ],
        ))
        .unwrap_err();
        assert!(e.contains("at most one"), "{e}");
    }

    #[test]
    fn builtin_sa2_runs_the_definitions_own_tape() {
        let t = tmp("sa2");
        let l = sa2_loader(&t);
        let report = l.report().join("\n");
        assert!(report.contains("3 loaded, 1 refused"), "{report}");
        assert!(
            report.contains("EZS-SIM") && report.contains("CONTRIBUTING.md"),
            "{report}"
        );
        assert!(!report.contains("87A5"), "report never shows parameters");
        let sk = l.seed_key("unlockecu:simalt").unwrap().unwrap();
        assert_eq!(sk.id(), "unlockecu:simalt");
        assert_eq!(
            sk.compute(&[0x11, 0x22, 0x33, 0x44], 0x11).unwrap(),
            [0xB4, 0x87, 0x96, 0xE1]
        );
        // Wrong level, wrong seed length.
        let e = sk.compute(&[0; 4], 0x01).unwrap_err().to_string();
        assert!(e.contains("AccessLevel 0x01") && e.contains("0x11"), "{e}");
        assert!(sk
            .compute(&[1, 2], 0x11)
            .unwrap_err()
            .to_string()
            .contains("SeedLength 4"));
        // No tape, and a provider no configured route serves.
        let e = l.seed_key("unlockecu:SIM-SA2-NOTAPE").unwrap().unwrap();
        assert!(e
            .compute(&[1; 4], 0x11)
            .unwrap_err()
            .to_string()
            .contains(SA2_TAPE_PARAM));
        let e = l.seed_key("unlockecu:SIM-EXT").unwrap().unwrap();
        let e = e.compute(&[1, 2], 0x01).unwrap_err().to_string();
        assert!(e.contains("'DemoXor' is not available"), "{e}");
    }

    #[test]
    fn ids_and_refusals() {
        let t = tmp("ids");
        let l = sa2_loader(&t);
        assert!(l.seed_key("sa2:87").is_none());
        assert!(l.seed_key("udssim-test").is_none());
        assert!(Loader::handles(" unlockecu:x") && !Loader::handles("sa2"));
        let e = l.seed_key("unlockecu: ").unwrap().unwrap_err();
        assert!(e.contains("no ECU name"), "{e}");
        let e = l.seed_key("unlockecu:nope").unwrap().unwrap_err();
        assert!(e.contains("no definition"), "{e}");
        // A refused file entry and a refused requested name both say why.
        let e = l.seed_key("unlockecu:EZS-SIM").unwrap().unwrap_err();
        assert!(e.contains("CONTRIBUTING.md"), "{e}");
        let e = l.seed_key("unlockecu:Immobilizer").unwrap().unwrap_err();
        assert!(
            e.contains("immobili") && e.contains("CONTRIBUTING.md"),
            "{e}"
        );
    }

    #[test]
    fn bad_files_fail_to_load() {
        let t = tmp("bad");
        let f = t.0.join("bad.json");
        std::fs::write(&f, "{ not json").unwrap();
        let e = Loader::new(&LoaderConfig::new(
            f.clone(),
            vec![ProviderSpec::BuiltinSa2],
        ))
        .unwrap_err();
        assert!(e.contains("not valid JSON"), "{e}");
        let e = Loader::new(&LoaderConfig::new(
            t.0.join("missing.json"),
            vec![ProviderSpec::BuiltinSa2],
        ))
        .unwrap_err();
        assert!(e.contains("missing.json"), "{e}");
    }
}
