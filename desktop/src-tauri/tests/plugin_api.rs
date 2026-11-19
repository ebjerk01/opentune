//! The plugin-facing API, named path by path (see docs/PLUGINS.md).
//!
//! A plugin build embeds `opentune_desktop` from outside the crate, so moving
//! or renaming any of these items breaks it. This test fails to compile when a
//! path changes; the assertions check the few pure helpers' behaviour.

use opentune_core::protocol::seedkey::SeedKey;
use opentune_desktop::adapter::{self, AdapterHub, Addr, ConnKey, Session};
use opentune_desktop::{
    builder_with, check_extra_commands, command_names, extension_handler, open_registry_driver,
    public_commands, public_handler, run, startup, with_extra_commands, AppData, BuildChannel,
    ChecksumResolver, Checksums, DebugLog, DesktopConfig, SeedKeyResolver, SeedKeys,
};
use serde_json::{json, Value};

#[tauri::command]
fn plugin_api_probe() -> String {
    "ok".into()
}

fn no_checksum(_: &str) -> Option<Box<dyn opentune_checksum::ChecksumAlgo>> {
    None
}

fn no_seedkey(_: &str) -> Option<Box<dyn SeedKey>> {
    None
}

/// The shape of a plugin's adapter command body: run on the hub, inside one
/// `Session::with_diag` binding (type-checked only; never called here).
#[allow(dead_code)]
fn plugin_operation(hub: &AdapterHub, key: ConnKey, keys: SeedKeys) -> Result<Value, String> {
    hub.run("plugin op", move |s: &mut Session| {
        let drv = s.interface(&key)?;
        adapter::apply_seed_key(drv, &keys, "plugin-id")?;
        s.with_diag(&key, 1000, |d| {
            adapter::enter_extended_session(d)?;
            adapter::leave_session(d);
            Ok(json!({}))
        })
    })
}

#[test]
fn plugin_api_paths_and_shapes() {
    // DesktopConfig, by field name (a plugin builds it with a struct literal).
    let checksums: Vec<ChecksumResolver> = vec![no_checksum];
    let seedkeys: Vec<SeedKeyResolver> = vec![no_seedkey];
    let cfg = DesktopConfig {
        channel: BuildChannel::new(BuildChannel::BENCH),
        data_roots: vec![std::path::PathBuf::from("data")],
        checksums,
        seedkeys,
        seedkey_loader: None,
        updater: false,
        diagnostics: None,
    };
    assert!(cfg.channel.debug_enabled());
    assert_eq!(cfg.channel.as_str(), "bench");
    let _: fn(tauri::Builder<tauri::Wry>, DesktopConfig) -> tauri::Builder<tauri::Wry> =
        builder_with;
    let _: fn(tauri::Context<tauri::Wry>) = run;
    let _: fn(&adapter::ConnKey) -> Result<Box<dyn opentune_core::Interface>, String> =
        open_registry_driver;
    let _: fn(&str) = startup::mark;
    // The diagnostics log: an embedder's own records and the read-back API.
    use opentune_desktop::diaglog;
    diaglog::record(
        diaglog::Level::Warn,
        diaglog::Src::Rust,
        "plugin record",
        None,
    );
    let _: fn(&std::path::Path, usize, diaglog::Level) -> Vec<diaglog::Record> = diaglog::read_tail;
    let _: fn(&std::path::Path) -> Vec<diaglog::Record> = diaglog::read_since_session;
    let _: fn() -> Option<std::path::PathBuf> = diaglog::log_dir;
    assert_eq!(diaglog::mask("67 01 AA BB"), "67 01 <masked: 2 bytes>");
    // The diagnostics upload target an embedder may set (never in the public
    // app; the endpoint here is a reserved, unresolvable name).
    use opentune_desktop::{diagupload, AutoPolicy, DiagMode, DiagUpload, SecretString};
    let up = DiagUpload {
        endpoint: diagupload::Url::parse("https://diag.example.invalid/upload").unwrap(),
        token: Some(SecretString::new("synthetic-test-token")),
        label: Some("plugin build".into()),
        mode: DiagMode::Auto(AutoPolicy {
            crash_on_next_start: true,
            on_error: true,
            periodic_min: Some(60),
        }),
    };
    assert!(!format!("{up:?}").contains("synthetic-test-token"));
    let _with = DesktopConfig {
        diagnostics: Some(up),
        ..DesktopConfig::default()
    };
    assert!(DesktopConfig::default().diagnostics.is_none());

    // Managed state types a plugin command may take as tauri::State.
    let data = AppData {
        ecudefs: None,
        catalog: None,
        guide: None,
        database: None,
        packs: None,
    };
    assert!(data.ecudefs.is_none());
    DebugLog::default().push("plugin line");
    assert!(Checksums::default().get("edc16").is_some());
    assert!(SeedKeys::default().get("anything").is_none());
    // The built-in SA2 interpreter over a user-armed script (see docs/PLUGINS.md).
    let keys = SeedKeys::new(vec![no_seedkey]);
    assert!(keys.armed_sa2().is_none() && keys.plugin("sa2").is_none());
    keys.arm_sa2(Some(opentune_desktop::seedkeys::ArmedSa2 {
        script: vec![0x4C],
        label: "plugin test".into(),
    }));
    assert!(keys.is_builtin_sa2("sa2"));
    keys.arm_sa2(None);

    // Command routing.
    assert_eq!(command_names("a, m :: b"), vec!["a", "b"]);
    assert!(check_extra_commands(&["plugin_api_probe"]).is_ok());
    assert!(check_extra_commands(&["list_ecudefs"]).is_err());
    assert!(public_commands().contains(&"list_ecudefs"));
    let _core = public_handler();
    let _ext = extension_handler![plugin_api_probe];
    let _ext2 = with_extra_commands(
        vec!["plugin_api_probe"],
        tauri::generate_handler![plugin_api_probe],
    );

    // Adapter addressing helpers.
    let addr = adapter::write_addr(
        adapter::resolve(Some("7E0"), Some("7E8"), None).unwrap(),
        "plugin write",
    )
    .unwrap();
    assert_eq!(addr.rx_id, 0x7E8);
    assert_eq!(addr, Addr::ENGINE);
    assert_eq!(
        addr.json(false),
        json!({"tx": "7E0", "rx": "7E8", "assumed": false})
    );
    let key = ConnKey::new("mock", None, None, Some("hs"), addr);
    assert_eq!(key.addr, addr);
    assert!(adapter::write_addr(None, "plugin write").is_err());
}
