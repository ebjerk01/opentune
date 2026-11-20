//! Plugin-supplied seed/key algorithms (UDS / KWP SecurityAccess).
//!
//! OpenTune ships **no** OEM seed/key algorithm, and this module adds none. It
//! is an extension point: an embedding app (a plugin build) passes resolvers in
//! the desktop's `DesktopConfig::seedkeys` and the core asks
//! them, by id, wherever it can unlock a module:
//!
//! * a service script that names a `seedkey` id (the desktop's Service panel);
//! * the Utilities seed/key calculator, for an id that is not one of its
//!   generic transforms (the result is labelled as plugin-supplied);
//! * a plugin's own read/flash commands, which attach a resolved handler to a
//!   hub-opened driver with [`crate::adapter::apply_seed_key`].
//!
//! The plugin owns the algorithm and its provenance; when no resolver knows an
//! id, every caller keeps the honest "plan only / not available" behaviour.
//!
//! One generic handler is built in: the VAG **SA2 interpreter**
//! (`opentune-sa2`), which computes nothing until the *user* supplies a
//! script. After the plugin resolvers, `sa2` resolves to the script the user
//! armed in the SA2 panel ([`SeedKeys::arm_sa2`], held in memory for this run
//! only), `sa2:<hex>` to the script given inline in the id, and `sa2:@<file>`
//! to a script file (raw bytes, hex or SA2 assembler, at most
//! [`MAX_SCRIPT_FILE`](crate::sa2script::MAX_SCRIPT_FILE) bytes) — the CLI's
//! `--seedkey sa2:<hex>` / `sa2:@<file>` forms. No script ships with OpenTune.
//!
//! Last, `unlockecu:<EcuName>` resolves through the user's UnlockECU-format
//! definitions loader when the embedder configured one
//! (the desktop's `DesktopConfig::seedkey_loader`,
//! Bench tier: alpha / bench channels only; see `docs/SEEDKEY-LOADER.md`).

use crate::channel::BuildChannel;
use opentune_core::protocol::seedkey::SeedKey;
use opentune_seedkey_loader::{Loader, LoaderConfig};
use std::sync::{Arc, Mutex};

/// Looks up a seed/key algorithm by id (see the desktop's `DesktopConfig::seedkeys`).
/// Return `None` for ids the resolver does not know.
pub type SeedKeyResolver = fn(&str) -> Option<Box<dyn SeedKey>>;

/// Seed/key algorithm lookup (managed state): the embedder's resolvers, in
/// order, then the built-in SA2 interpreter over a user-supplied script. With
/// no resolver and no armed script (the public app at startup),
/// [`get`](SeedKeys::get) returns `None` for every id except an inline
/// `sa2:<hex>` or a readable `sa2:@<file>`.
///
/// Clones share the armed SA2 script, so a clone taken into an adapter
/// operation sees what the user armed.
#[derive(Clone, Default)]
pub struct SeedKeys {
    resolvers: Vec<SeedKeyResolver>,
    sa2: Arc<Mutex<Option<ArmedSa2>>>,
    loader: Option<Loader>,
}

/// The definitions loader `cfg` describes, if the channel allows it: `None`
/// without a config (the default), an error on the stable channel (Bench
/// tier) or when the file / provider cannot be used. On success the load
/// report (counts, refused and rejected entries; never values) goes to stderr.
pub fn loader_for(
    cfg: Option<&LoaderConfig>,
    channel: &BuildChannel,
) -> Result<Option<Loader>, String> {
    let Some(cfg) = cfg else {
        return Ok(None);
    };
    if !channel.debug_enabled() {
        return Err(format!(
            "seed/key definitions loader ignored on the '{}' channel: it is a Bench-tier feature \
             (alpha / bench builds) until vehicle-tested",
            channel.as_str()
        ));
    }
    let loader = Loader::new(cfg)?;
    for line in loader.report() {
        eprintln!("{line}");
    }
    Ok(Some(loader))
}

/// The SA2 script the user armed for live sessions (see [`SeedKeys::arm_sa2`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArmedSa2 {
    /// The script bytes.
    pub script: Vec<u8>,
    /// Where the user said it came from (shown in the UI; free text).
    pub label: String,
}

impl SeedKeys {
    /// A lookup over `resolvers`, consulted in order.
    pub fn new(resolvers: Vec<SeedKeyResolver>) -> Self {
        SeedKeys {
            resolvers,
            sa2: Arc::default(),
            loader: None,
        }
    }

    /// This lookup with the user's definitions loader as its last resolver
    /// (see [`loader_for`]).
    pub fn with_loader(mut self, loader: Option<Loader>) -> Self {
        self.loader = loader;
        self
    }

    /// The definitions loader, if one is installed.
    pub fn seedkey_loader(&self) -> Option<&Loader> {
        self.loader.as_ref()
    }

    /// The loader's handler for `unlockecu:<name>`: `None` when `id` is not
    /// of that form; an error when no loader is installed or it cannot serve
    /// the id.
    fn loaded(&self, id: &str) -> Option<Result<Box<dyn SeedKey>, String>> {
        if !Loader::handles(id) {
            return None;
        }
        Some(match &self.loader {
            None => Err(format!(
                "seed/key '{id}': the definitions loader is off — it needs your definitions file \
                 and a provider in an alpha / bench build (OpenTune ships no algorithm or data)"
            )),
            Some(l) => l
                .seed_key(id)
                .unwrap_or_else(|| Err(format!("'{id}' is not a loader id")))
                .map(|sk| Box::new(sk) as Box<dyn SeedKey>),
        })
    }

    /// Resolve `id` (trimmed): the first plugin resolver that knows it, else
    /// the built-in SA2 interpreter (`sa2` = the armed script, `sa2:<hex>` =
    /// the inline one, `sa2:@<file>` = a script file). A blank id never
    /// resolves.
    pub fn get(&self, id: &str) -> Option<Box<dyn SeedKey>> {
        let id = id.trim();
        if id.is_empty() {
            return None;
        }
        self.plugin(id)
            .or_else(|| self.builtin_sa2(id).and_then(Result::ok))
            .or_else(|| self.loaded(id).and_then(Result::ok))
    }

    /// Resolve `id` through the plugin resolvers only.
    pub fn plugin(&self, id: &str) -> Option<Box<dyn SeedKey>> {
        let id = id.trim();
        if id.is_empty() {
            return None;
        }
        self.resolvers.iter().find_map(|r| r(id))
    }

    /// The built-in SA2 handler for `sa2` (the armed script), `sa2:<hex>` or
    /// `sa2:@<file>`. `None` when `id` is none of these (or `sa2` with nothing
    /// armed); `Some(Err)` when it is one but the script cannot be used.
    fn builtin_sa2(&self, id: &str) -> Option<Result<Box<dyn SeedKey>, String>> {
        let script = if id == SA2_ID {
            Ok(self.armed_sa2()?.script)
        } else {
            let spec = id.strip_prefix("sa2:")?;
            match spec.strip_prefix('@') {
                Some(path) => sa2_script_file(path),
                None => opentune_sa2::parse_hex(spec).map_err(|e| e.to_string()),
            }
        };
        Some(script.map(|s| Box::new(opentune_sa2::Sa2SeedKey::new(s)) as Box<dyn SeedKey>))
    }

    /// Like [`get`](SeedKeys::get), with the standard "not available" error,
    /// or why an `sa2:<hex>` / `sa2:@<file>` id could not be used.
    pub fn require(&self, id: &str) -> Result<Box<dyn SeedKey>, String> {
        let id = id.trim();
        if let Some(h) = self.plugin(id) {
            return Ok(h);
        }
        match self.builtin_sa2(id) {
            Some(Ok(h)) => Ok(h),
            Some(Err(e)) => Err(format!("seed/key '{id}': {e}")),
            None => self.loaded(id).unwrap_or_else(|| Err(unavailable(id))),
        }
    }

    /// Whether no plugin resolver is registered (the public app).
    pub fn is_empty(&self) -> bool {
        self.resolvers.is_empty()
    }

    /// Arm `script` as the `sa2` handler for this run (replacing any earlier
    /// one), or disarm it with `None`. Nothing is persisted.
    pub fn arm_sa2(&self, armed: Option<ArmedSa2>) {
        *self.sa2.lock().unwrap_or_else(|p| p.into_inner()) = armed;
    }

    /// The armed SA2 script, if any.
    pub fn armed_sa2(&self) -> Option<ArmedSa2> {
        self.sa2.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Whether `id` resolves to the built-in SA2 interpreter (not a plugin).
    pub fn is_builtin_sa2(&self, id: &str) -> bool {
        let id = id.trim();
        self.plugin(id).is_none() && matches!(self.builtin_sa2(id), Some(Ok(_)))
    }
}

/// Read an `sa2:@<file>` script: size-capped, raw bytes or hex/assembler
/// text, and no longer than any SA2 script.
fn sa2_script_file(path: &str) -> Result<Vec<u8>, String> {
    use crate::sa2script::{check_len, read_capped, script_from_file_bytes, MAX_SCRIPT_FILE};
    if path.trim().is_empty() {
        return Err("no file after 'sa2:@'".into());
    }
    let raw = read_capped(path, MAX_SCRIPT_FILE, "SA2 script")?;
    let (script, _) = script_from_file_bytes(raw)?;
    check_len(&script)?;
    Ok(script)
}

/// The seed/key id the armed SA2 script answers to.
pub const SA2_ID: &str = "sa2";

/// The message for an id no resolver knows.
pub fn unavailable(id: &str) -> String {
    if id.trim() == SA2_ID {
        return "no SA2 script is armed — load your own script in the SA2 script panel and arm it for live sessions (OpenTune ships none)".into();
    }
    format!(
        "no seed/key algorithm '{}' is available in this build — OpenTune ships none; it must come from a plugin",
        id.trim()
    )
}

/// Run a resolved handler as a `KeyProvider`-shaped closure body.
pub fn compute(
    handler: &dyn SeedKey,
    seed: &[u8],
    level: u8,
) -> std::result::Result<Vec<u8>, String> {
    handler
        .compute(seed, level)
        .map_err(|e| format!("seed/key '{}': {e}", handler.id()))
}

#[cfg(any(test, feature = "testkit"))]
pub mod testkeys {
    //! Test-only resolvers. `vcar-test` is the virtual car's documented
    //! TEST-ONLY transform (`opentune_driver_vcar::test_security_key`), never a
    //! manufacturer algorithm.
    use super::*;

    pub struct VcarTest;

    impl SeedKey for VcarTest {
        fn id(&self) -> &str {
            "vcar-test"
        }
        fn compute(&self, seed: &[u8], _level: u8) -> opentune_core::Result<Vec<u8>> {
            let mask = opentune_driver_vcar::Profile::demo().security.xor_mask;
            Ok(opentune_driver_vcar::test_security_key(seed, mask))
        }
    }

    /// A fixed transform tagged with the resolver that produced it.
    pub struct Tagged(pub &'static str, pub u8);

    impl SeedKey for Tagged {
        fn id(&self) -> &str {
            self.0
        }
        fn compute(&self, seed: &[u8], level: u8) -> opentune_core::Result<Vec<u8>> {
            Ok(seed.iter().map(|b| b ^ self.1 ^ level).collect())
        }
    }

    pub fn vcar(id: &str) -> Option<Box<dyn SeedKey>> {
        (id == "vcar-test").then(|| Box::new(VcarTest) as Box<dyn SeedKey>)
    }

    pub fn first(id: &str) -> Option<Box<dyn SeedKey>> {
        (id == "shared" || id == "only-first").then(|| Box::new(Tagged("first", 0x10)) as _)
    }

    pub fn second(id: &str) -> Option<Box<dyn SeedKey>> {
        (id == "shared" || id == "only-second").then(|| Box::new(Tagged("second", 0x20)) as _)
    }
}

#[cfg(test)]
mod tests {
    use super::testkeys::{first, second};
    use super::*;

    #[test]
    fn resolvers_are_asked_in_order_and_none_is_honest() {
        let keys = SeedKeys::new(vec![first, second]);
        assert_eq!(keys.get("shared").unwrap().id(), "first", "first wins");
        assert_eq!(keys.get(" only-second ").unwrap().id(), "second");
        assert!(keys.get("unknown").is_none());
        assert!(keys.get("  ").is_none(), "blank ids never resolve");
        let e = keys.require("unknown").err().unwrap();
        assert!(e.contains("'unknown'") && e.contains("plugin"), "{e}");

        let public = SeedKeys::default();
        assert!(public.is_empty());
        assert!(public.get("shared").is_none(), "the public app has none");
        assert!(
            public.get(SA2_ID).is_none(),
            "no SA2 script until one is armed"
        );
    }

    // Synthetic SA2 script (written for the tests): eor 0xFFFFFFFF; finish.
    const NOT: &[u8] = &[0x87, 0xFF, 0xFF, 0xFF, 0xFF, 0x4C];

    #[test]
    fn armed_sa2_script_resolves_as_sa2_and_is_shared_by_clones() {
        let keys = SeedKeys::default();
        let e = keys.require("sa2").err().unwrap();
        assert!(e.contains("SA2 script panel"), "{e}");
        let op = keys.clone(); // e.g. taken into an adapter operation
        keys.arm_sa2(Some(ArmedSa2 {
            script: NOT.to_vec(),
            label: "test".into(),
        }));
        let h = op.get(" sa2 ").expect("clone sees the armed script");
        assert_eq!(h.id(), "sa2");
        assert_eq!(
            h.compute(&[0, 0, 0, 1], 0x11).unwrap(),
            [0xFF, 0xFF, 0xFF, 0xFE]
        );
        assert!(keys.is_builtin_sa2("sa2"));
        assert_eq!(op.armed_sa2().unwrap().label, "test");
        keys.arm_sa2(None);
        assert!(op.get("sa2").is_none(), "disarm is shared too");
        assert!(keys.armed_sa2().is_none());
    }

    #[test]
    fn inline_sa2_ids_and_plugin_precedence() {
        let keys = SeedKeys::default();
        let h = keys.get("sa2:87FFFFFFFF4C").unwrap();
        assert_eq!(
            h.compute(&[0, 0, 0, 1], 0x11).unwrap(),
            [0xFF, 0xFF, 0xFF, 0xFE]
        );
        assert!(keys.get("sa2:8").is_none(), "bad hex never resolves");
        assert!(keys.get("sa2x").is_none());
        // A plugin that answers "sa2" keeps precedence over the armed script.
        fn plugin_sa2(id: &str) -> Option<Box<dyn SeedKey>> {
            (id == "sa2").then(|| Box::new(super::testkeys::Tagged("plugin-sa2", 0)) as _)
        }
        let keys = SeedKeys::new(vec![plugin_sa2]);
        keys.arm_sa2(Some(ArmedSa2 {
            script: NOT.to_vec(),
            label: String::new(),
        }));
        assert_eq!(keys.get("sa2").unwrap().id(), "plugin-sa2");
        assert!(!keys.is_builtin_sa2("sa2"));
        assert!(keys.plugin("sa2:87FFFFFFFF4C").is_none());
    }

    #[test]
    fn sa2_script_files_resolve_with_a_size_cap() {
        let dir = std::env::temp_dir().join(format!("ot-seedkeys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let keys = SeedKeys::default();
        let want = [0xFF, 0xFF, 0xFF, 0xFE];
        // Raw bytes, hex text and assembler source all load.
        let cases: [(&str, &[u8]); 3] = [
            ("raw.bin", NOT),
            ("hex.txt", b"87 FF FF FF FF 4C\n"),
            ("asm.sa2", b"eor 0xFFFFFFFF\nfinish\n"),
        ];
        for (name, body) in cases {
            let p = dir.join(name);
            std::fs::write(&p, body).unwrap();
            let id = format!("sa2:@{}", p.display());
            let h = keys.require(&id).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(h.compute(&[0, 0, 0, 1], 0x11).unwrap(), want, "{name}");
            assert!(keys.is_builtin_sa2(&id));
        }
        // A missing file, an oversized file and a blank path are refused with
        // a reason, and never resolve through get().
        let missing = format!("sa2:@{}", dir.join("nope").display());
        assert!(keys.get(&missing).is_none());
        let e = keys.require(&missing).err().unwrap();
        assert!(e.contains("SA2 script") && e.contains("nope"), "{e}");
        let big = dir.join("big.bin");
        std::fs::write(
            &big,
            vec![0x4Cu8; crate::sa2script::MAX_SCRIPT_FILE as usize + 1],
        )
        .unwrap();
        let e = keys
            .require(&format!("sa2:@{}", big.display()))
            .err()
            .unwrap();
        assert!(e.contains("limit"), "{e}");
        let long = dir.join("long.bin");
        std::fs::write(&long, vec![0x87u8; crate::sa2script::MAX_SCRIPT + 1]).unwrap();
        let e = keys
            .require(&format!("sa2:@{}", long.display()))
            .err()
            .unwrap();
        assert!(e.contains("longer than any SA2 script"), "{e}");
        let e = keys.require("sa2:@ ").err().unwrap();
        assert!(e.contains("no file"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn definitions_loader_is_off_by_default_and_bench_tier() {
        use opentune_seedkey_loader::ProviderSpec;
        let dir = std::env::temp_dir().join(format!("ot-seedkeys-loader-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Synthetic definition: tape `eor 0xFFFFFFFF; finish` (key = NOT seed).
        let defs = dir.join("defs.json");
        std::fs::write(
            &defs,
            r#"[{"EcuName":"SIM","AccessLevel":17,"SeedLength":4,"KeyLength":4,"Provider":"VolkswagenSA2",
                "Parameters":[{"Key":"InstructionTape","Value":"87FFFFFFFF4C","DataType":"ByteArray"}]},
               {"EcuName":"ELV-SIM","AccessLevel":17,"SeedLength":4,"KeyLength":4,"Provider":"VolkswagenSA2"}]"#,
        )
        .unwrap();
        let cfg = LoaderConfig::new(defs, vec![ProviderSpec::BuiltinSa2]);
        let bench = BuildChannel::new(BuildChannel::BENCH);
        let stable = BuildChannel::new(BuildChannel::STABLE);

        // Off by default: no config, no loader; the id says why.
        assert!(loader_for(None, &bench).unwrap().is_none());
        let keys = SeedKeys::default();
        assert!(keys.get("unlockecu:SIM").is_none());
        let e = keys.require("unlockecu:SIM").err().unwrap();
        assert!(e.contains("loader is off"), "{e}");
        // Stable channel: refused (Bench tier).
        let e = loader_for(Some(&cfg), &stable).unwrap_err();
        assert!(e.contains("Bench-tier"), "{e}");
        // Bench channel: resolves, after plugins and SA2.
        let keys = SeedKeys::default().with_loader(loader_for(Some(&cfg), &bench).unwrap());
        let h = keys.require(" unlockecu:sim ").unwrap();
        assert_eq!(
            h.compute(&[0, 0, 0, 1], 0x11).unwrap(),
            [0xFF, 0xFF, 0xFF, 0xFE]
        );
        assert!(keys.seedkey_loader().is_some());
        assert!(
            keys.clone().get("unlockecu:SIM").is_some(),
            "clones keep it"
        );
        let e = keys.require("unlockecu:ELV-SIM").err().unwrap();
        assert!(e.contains("CONTRIBUTING.md"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compute_reports_the_handler() {
        let k = first("shared").unwrap();
        assert_eq!(
            compute(k.as_ref(), &[0x00, 0x01], 0x01).unwrap(),
            [0x11, 0x10]
        );
    }
}
