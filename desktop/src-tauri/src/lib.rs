// OpenTune desktop — Tauri 2 backend (PUBLIC, calibration-only edition).
//
// This is the open-source OpenTune app. It exposes the public OpenTune core to
// the UI and runs the whole calibration workflow OFFLINE on a flash-image file
// through the mock/file driver — no hardware required:
//   open a dump → identify it → verify/fix checksums → diff for maps →
//   decode a defined map into a heat grid → run a safety-gated (mock) flash →
//   browse a backup vault.
//
// It is deliberately calibration-only: there is NO immobiliser / EEPROM /
// standalone / bench tooling here (out of scope for this app). ECU
// definitions, the vehicle catalog and the in-app guide are resolved from
// bundled resources (or a dev path) at startup.
//
// The crate is a library plus a thin binary (`main.rs` → [`run`]). The library
// half lets another Tauri app embed the complete public desktop backend and add
// its own commands next to it — see "Embedding" below:
// [`builder_with`] manages all public state, [`public_handler`] dispatches every
// public command, [`with_extra_commands`] routes an embedder's own command names
// to its own handler, and [`adapter`] gives those commands the shared hub.
//
// Layout: this file holds the module list, the plugin-facing API
// ([`DesktopConfig`], [`builder_with`], the command list and handlers) and the
// re-exports that keep every public path stable. Each panel's commands live in
// their own module (`image`, `checksum`, `maps`, …; live diagnostics in
// `feat_*`).

use std::path::PathBuf;
use tauri::Manager;

// Live-diagnostics feature panels (each self-contained; they reuse the shared
// adapter hub `adapter::AdapterHub` and the public engine crates). The guided
// Auto-debug wizard is UI-only (it orchestrates these commands from the browser).
pub mod adapter;
mod feat_adaptation;
mod feat_candump;
mod feat_coding;
mod feat_dash;
mod feat_db;
mod feat_gps;
mod feat_haldex;
mod feat_logging;
mod feat_logview;
mod feat_obd;
mod feat_obdinfo;
mod feat_scan;
mod feat_service;
mod feat_ts;
// Plugin-supplied seed/key algorithms (OpenTune ships none).
pub mod seedkeys;
// Startup trace (`OPENTUNE_TRACE_STARTUP=1`), WebKitGTK env defaults and the
// parsed-data cache. Public so an embedder can add its own trace marks.
pub mod startup;
#[cfg(test)]
mod vcar_e2e;

// Calibration panels and app plumbing.
mod checksum; // Checksum panel + algorithm resolvers
mod common; // hex / number parsing, file-write errors
mod compare; // findmaps diff + map-by-map compare
mod debug; // Debug panel (alpha builds)
mod defs; // XDF/A2L import + region promotion
mod drivers; // driver registry + adapter probe
mod flash; // safety-gated file flash + backup vault
mod guide; // in-app guide
mod image; // Image / Identify / Analyze bin
mod maps; // map lists, decode, editor write-back
mod packs; // definition packs for the panels' pack picker
mod project; // .otproj tuning projects
mod state; // managed state: data dirs, channel, guards, debug log
mod update; // self-update
mod utilities; // VIN / part / DTC / seed-key calculator

// The public plugin API (see docs/PLUGINS.md): these paths are stable.
pub use checksum::{ChecksumResolver, Checksums};
pub use drivers::open_registry_driver;
pub use seedkeys::{SeedKeyResolver, SeedKeys};
pub use state::{AppData, BuildChannel, DebugLog};

use state::{resolve_data, OpGuard};

// ---------------------------------------------------------------------------
// Embedding / plugin host API
// ---------------------------------------------------------------------------
//
// The public binary is `builder_with(Builder::default(), DesktopConfig::default())
// .invoke_handler(public_handler())`. Another Tauri app (a plugin build) embeds
// the same backend and adds its own commands:
//
//     let b = opentune_desktop::builder_with(tauri::Builder::default(), config)
//         .manage(MyState::default());
//     b.invoke_handler(opentune_desktop::extension_handler![my_cmd, my_mod::other])
//         .run(tauri::generate_context!())
//
// The embedder's command names are dispatched to its own handler; every other
// name goes to the core handler. A name that collides with a core command is a
// startup panic (it would silently shadow a public command otherwise). See
// docs/PLUGINS.md.

/// How an app embeds the public desktop backend.
#[derive(Clone)]
pub struct DesktopConfig {
    /// Channel reported to the UI and used to gate debug commands.
    pub channel: BuildChannel,
    /// Extra directories searched for `ecudefs/`, `catalog/`, `guide/`,
    /// `database/` and `packs/`.
    pub data_roots: Vec<PathBuf>,
    /// Extra checksum algorithm resolvers, consulted before the public set.
    pub checksums: Vec<ChecksumResolver>,
    /// Seed/key algorithm resolvers (managed as [`SeedKeys`]), consulted in
    /// order by id. The public app has none: OpenTune ships no seed/key
    /// algorithm, so without a plugin every secured path stays plan-only.
    pub seedkeys: Vec<SeedKeyResolver>,
    /// Register the self-updater plugin. Needs a `plugins.updater` section
    /// (endpoint + pubkey) in the embedding app's `tauri.conf.json`; when off,
    /// the update commands report "updater unavailable".
    pub updater: bool,
}

impl Default for DesktopConfig {
    fn default() -> Self {
        DesktopConfig {
            channel: BuildChannel::compiled(),
            data_roots: Vec::new(),
            checksums: Vec::new(),
            seedkeys: Vec::new(),
            updater: true,
        }
    }
}

/// Add the public desktop backend to `builder`: the dialog (and optionally the
/// updater) plugin, plus a small setup plugin that manages every piece of core
/// state — [`AppData`], [`BuildChannel`], [`Checksums`], [`SeedKeys`], [`DebugLog`], the
/// long-operation guard and the shared [`adapter::AdapterHub`].
///
/// It does not call `Builder::setup` (that slot stays free for the embedder)
/// and does not set the invoke handler: pass [`public_handler`] for the core
/// app, or [`with_extra_commands`] / [`extension_handler!`] to add commands.
/// The embedder must not register `tauri-plugin-dialog`/`-updater` itself.
pub fn builder_with(
    builder: tauri::Builder<tauri::Wry>,
    config: DesktopConfig,
) -> tauri::Builder<tauri::Wry> {
    // Before anything touches GTK/WebKit: start the startup clock and apply the
    // WebKitGTK environment defaults (see startup.rs and
    // docs/TROUBLESHOOTING-STARTUP.md).
    startup::begin();
    startup::apply_platform_env();
    let DesktopConfig {
        channel,
        data_roots,
        checksums,
        seedkeys,
        updater,
    } = config;
    let core = tauri::plugin::Builder::<tauri::Wry>::new("opentune-core")
        .setup(move |app, _api| {
            startup::mark("setup plugin: start");
            let data = startup::timed("setup: data roots resolved", || {
                resolve_data(app, &data_roots)
            });
            // Nothing is parsed here: the definitions and the catalog warm up
            // on a background thread and every command reads the cache.
            startup::warm(data.ecudefs.clone(), data.catalog.clone());
            app.manage(data);
            app.manage(channel);
            app.manage(Checksums(checksums));
            app.manage(SeedKeys::new(seedkeys));
            app.manage(OpGuard::default());
            app.manage(DebugLog::default());
            app.manage(adapter::AdapterHub::with_registry());
            app.manage(feat_candump::CandumpState::default());
            app.manage(feat_logging::LiveLogState::default());
            app.manage(feat_haldex::HaldexState::default());
            app.manage(std::sync::Arc::new(feat_ts::TsState::default()));
            startup::mark("setup plugin: done (state managed)");
            Ok(())
        })
        .on_window_ready(|w| startup::mark(&format!("window '{}' created", w.label())))
        .on_webview_ready(|w| startup::mark(&format!("webview '{}' created", w.label())))
        .on_page_load(|_w, p| startup::mark(&format!("page load {:?}: {}", p.event(), p.url())))
        .on_event(|_app, ev| match ev {
            tauri::RunEvent::Ready => startup::mark("event loop ready"),
            tauri::RunEvent::WindowEvent { event, .. } if startup::enabled() => {
                startup::window_event(&format!("{event:?}"))
            }
            _ => {}
        })
        .build();
    let builder = builder.plugin(tauri_plugin_dialog::init());
    let builder = if updater {
        builder.plugin(tauri_plugin_updater::Builder::new().build())
    } else {
        builder
    };
    builder.plugin(core)
}

/// The single list of core IPC commands. Feature modules add their commands
/// HERE (one entry per command, `module::command` for commands defined in a
/// feature module); [`public_handler`] and [`public_commands`] both derive
/// from it, so the handler and the name list can never drift apart.
macro_rules! core_command_list {
    ($emit:ident) => {
        $emit! {
            state::build_channel,
            drivers::probe_adapter,
            debug::debug_raw_request,
            debug::debug_simulate,
            debug::debug_log_dump,
            debug::debug_log_clear,
            feat_coding::coding_read,
            feat_coding::coding_set_preview,
            feat_coding::coding_write,
            feat_scan::autoscan_run,
            feat_scan::autoscan_clear,
            feat_adaptation::adaptation_list,
            feat_adaptation::adaptation_read,
            feat_adaptation::adaptation_actuator,
            feat_adaptation::adaptation_routine,
            feat_service::service_plan,
            feat_service::service_run,
            feat_logging::log_params,
            feat_logging::log_poll_once,
            feat_logging::log_start,
            feat_logging::log_tick,
            feat_logging::log_stop,
            feat_logview::log_open,
            feat_logview::log_series,
            feat_logview::log_sample_at,
            feat_logview::log_def_maps,
            feat_logview::log_map_hits,
            feat_obd::obd_live,
            feat_dash::obd_pid_table,
            feat_dash::obd_supported,
            feat_dash::obd_read,
            feat_dash::dash_record_write,
            feat_gps::gps_ports,
            feat_gps::gps_open,
            feat_gps::gps_fix,
            feat_gps::gps_close,
            feat_obdinfo::obd_inspect,
            feat_db::db_list,
            feat_db::db_get,
            feat_candump::candump_start,
            feat_candump::candump_status,
            feat_candump::candump_stop,
            feat_haldex::haldex_modes,
            feat_haldex::haldex_status,
            feat_haldex::haldex_set_mode,
            feat_haldex::haldex_gen4_read,
            feat_haldex::haldex_decode_log,
            feat_haldex::haldex_export_csv,
            feat_ts::ts_load_ini,
            feat_ts::ts_load_msq,
            feat_ts::ts_save_msq,
            feat_ts::ts_diff_msq,
            feat_ts::ts_values,
            feat_ts::ts_get,
            feat_ts::ts_set,
            feat_ts::ts_table,
            feat_ts::ts_curve,
            feat_ts::ts_status,
            feat_ts::ts_connect,
            feat_ts::ts_disconnect,
            feat_ts::ts_read,
            feat_ts::ts_plan_write,
            feat_ts::ts_write,
            feat_ts::ts_poll,
            drivers::list_drivers,
            state::data_paths,
            packs::packs_list,
            packs::packs_open,
            image::image_info,
            image::hex_preview,
            image::identify,
            image::match_identity,
            checksum::checksum_algos,
            checksum::checksum_verify,
            checksum::checksum_fix,
            compare::findmaps,
            maps::list_ecudefs,
            maps::list_maps,
            maps::decode_map,
            maps::decode_mapdef,
            maps::map_write,
            maps::mapdef_write,
            compare::compare_maps,
            image::analyze_image,
            defs::import_definitions,
            defs::promote_region,
            utilities::vin_decode,
            utilities::part_decode,
            utilities::dtc_lookup,
            utilities::seedkey_algos,
            utilities::seedkey_compute,
            project::project_save,
            project::project_open,
            flash::flash,
            flash::vault_list,
            guide::guide_list,
            guide::guide_read,
            update::update_status,
            update::update_check,
            update::update_install,
            update::update_relaunch,
            startup::startup_mark,
        }
    };
}

macro_rules! emit_handler {
    ($($cmd:tt)*) => {
        tauri::generate_handler![$($cmd)*]
    };
}

macro_rules! emit_names {
    ($($cmd:tt)*) => {
        stringify!($($cmd)*)
    };
}

/// The invoke handler for every core command (see [`public_commands`]).
pub fn public_handler() -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static {
    // (`typed` pins the closure's argument type for `generate_handler!`.)
    fn typed<F>(f: F) -> F
    where
        F: Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static,
    {
        f
    }
    let handler = typed(core_command_list!(emit_handler));
    // With OPENTUNE_TRACE_STARTUP=1 the first IPC calls are timed (a sync
    // command runs inside this dispatch, on the UI thread).
    move |invoke| {
        if !startup::enabled() {
            return handler(invoke);
        }
        let cmd = invoke.message.command().to_string();
        startup::ipc(&cmd, || handler(invoke))
    }
}

/// The names of every core command, as the frontend invokes them.
pub fn public_commands() -> &'static [&'static str] {
    static NAMES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| command_names(core_command_list!(emit_names)))
}

/// Command names from a stringified `generate_handler!`-style list
/// (`"a, m :: b,"` → `["a", "b"]`): the last path segment of each entry.
pub fn command_names(list: &'static str) -> Vec<&'static str> {
    list.split(',')
        .filter_map(|entry| entry.rsplit("::").next().map(str::trim))
        .filter(|name| !name.is_empty())
        .collect()
}

/// Check an embedder's command names: no duplicates and no collision with a
/// core command (which it would otherwise silently shadow).
pub fn check_extra_commands(names: &[&str]) -> Result<(), String> {
    let core = public_commands();
    for (i, n) in names.iter().enumerate() {
        if core.contains(n) {
            return Err(format!(
                "extension command '{n}' collides with a core command"
            ));
        }
        if names[..i].contains(n) {
            return Err(format!("extension command '{n}' is listed twice"));
        }
    }
    Ok(())
}

/// An invoke handler that routes the embedder's `names` to `extra` and every
/// other command to the core [`public_handler`].
///
/// # Panics
/// At construction, when [`check_extra_commands`] rejects `names`.
pub fn with_extra_commands<F>(
    names: Vec<&'static str>,
    extra: F,
) -> impl Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static
where
    F: Fn(tauri::ipc::Invoke<tauri::Wry>) -> bool + Send + Sync + 'static,
{
    if let Err(e) = check_extra_commands(&names) {
        panic!("{e}");
    }
    let core = public_handler();
    move |invoke| {
        if names.contains(&invoke.message.command()) {
            extra(invoke)
        } else {
            core(invoke)
        }
    }
}

/// Build an invoke handler from an embedder's command list (same syntax as
/// `tauri::generate_handler!`), dispatched next to the core commands via
/// [`with_extra_commands`].
#[macro_export]
macro_rules! extension_handler {
    ($($cmd:tt)*) => {
        $crate::with_extra_commands(
            $crate::command_names(::core::stringify!($($cmd)*)),
            ::tauri::generate_handler![$($cmd)*],
        )
    };
}

/// Run the public OpenTune desktop app. `context` comes from
/// `tauri::generate_context!()` in the binary, so an embedder never compiles
/// the public app's assets into its own binary.
pub fn run(context: tauri::Context<tauri::Wry>) {
    startup::begin();
    builder_with(tauri::Builder::default(), DesktopConfig::default())
        .invoke_handler(public_handler())
        .run(context)
        .expect("error while running OpenTune");
}

// ---------------------------------------------------------------------------
// Tests — the embedding / plugin host API.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod embed_tests {
    use super::*;
    use crate::debug::alpha_only;

    #[test]
    fn command_names_take_last_path_segment() {
        assert_eq!(
            command_names("a, m :: b, x :: y :: c,"),
            vec!["a", "b", "c"]
        );
        assert!(command_names("").is_empty());
    }

    #[test]
    fn public_command_list_is_complete_and_unique() {
        let names = public_commands();
        for n in [
            "build_channel",
            "probe_adapter",
            "coding_read",
            "obd_live",
            "checksum_fix",
            "update_relaunch",
        ] {
            assert!(names.contains(&n), "missing core command {n}");
        }
        assert!(names.iter().all(|n| !n.contains(':') && !n.contains(' ')));
        let mut sorted = names.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate core command");
    }

    #[test]
    fn extra_commands_cannot_shadow_core_or_repeat() {
        assert!(check_extra_commands(&["my_tool_run", "my_tool_plan"]).is_ok());
        let e = check_extra_commands(&["my_tool_run", "flash"]).unwrap_err();
        assert!(e.contains("collides"), "{e}");
        let e = check_extra_commands(&["my_tool_run", "my_tool_run"]).unwrap_err();
        assert!(e.contains("twice"), "{e}");
    }

    #[test]
    fn build_channel_gates_debug_commands() {
        assert!(alpha_only(&BuildChannel::new(BuildChannel::STABLE)).is_err());
        assert!(alpha_only(&BuildChannel::new(BuildChannel::ALPHA)).is_ok());
        assert!(alpha_only(&BuildChannel::new(BuildChannel::BENCH)).is_ok());
        let compiled = BuildChannel::compiled();
        assert_eq!(compiled.debug_enabled(), cfg!(feature = "alpha"));
        assert_eq!(DesktopConfig::default().channel, compiled);
    }

    #[test]
    fn checksum_resolvers_take_precedence() {
        fn none(_: &str) -> Option<Box<dyn opentune_checksum::ChecksumAlgo>> {
            None
        }
        fn alias(id: &str) -> Option<Box<dyn opentune_checksum::ChecksumAlgo>> {
            (id == "my-alias")
                .then(|| opentune_checksum::get("edc16"))
                .flatten()
        }
        let c = Checksums(vec![none, alias]);
        assert!(c.get("my-alias").is_some());
        assert!(c.get("edc16").is_some(), "public set still resolves");
        assert!(c.require("nope").is_err());
        assert!(Checksums::default().get("my-alias").is_none());
    }
}
