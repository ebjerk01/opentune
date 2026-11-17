# Desktop plugins

The OpenTune desktop app (`desktop/`) is a **plugin host**. Its backend is a
Rust library, `opentune_desktop` (`desktop/src-tauri/src/lib.rs`); the public
app's binary (`src/main.rs`) only calls `opentune_desktop::run`. A *plugin build*
is a separate Tauri app that embeds the same library, adds its own commands, and
ships the public UI plus a few extra panel scripts. Every public panel and
command works unchanged inside a plugin build.

A plugin lives in its own repository and depends on this one by path or git;
nothing plugin-specific is ever added to this repo. The public app has no
knowledge of any particular plugin.

The command-line app is a host in the same way: see [CLI host API](#cli-host-api).

## Backend: embed the library and add commands

```toml
# plugin's desktop/src-tauri/Cargo.toml
[dependencies]
tauri = { version = "2", features = [] }
tauri-plugin-dialog = "2"   # direct dep so the plugin's capabilities can grant dialog:*
opentune-desktop = { path = "../../../opentune/desktop/src-tauri", default-features = false }
opentune-core = { path = "../../../opentune/crates/opentune-core" }

[features]
default = ["custom-protocol"]
custom-protocol = ["tauri/custom-protocol"]
```

```rust
// plugin's src/main.rs
use opentune_desktop::{BuildChannel, DesktopConfig};

fn main() {
    let config = DesktopConfig {
        channel: BuildChannel::new(BuildChannel::BENCH),
        data_roots: vec![/* e.g. the plugin's own data dir in dev */],
        checksums: vec![/* extra fn(&str) -> Option<Box<dyn ChecksumAlgo>> */],
        updater: false, // true only with a plugins.updater section in tauri.conf.json
    };
    opentune_desktop::builder_with(tauri::Builder::default(), config)
        .manage(MyState::default())
        .invoke_handler(opentune_desktop::extension_handler![
            my_tool_plan,
            my_mod::my_tool_read,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the plugin build");
}
```

What the library provides:

| Item | Purpose |
|---|---|
| `builder_with(builder, DesktopConfig)` | Registers the dialog (and optionally updater) plugin and a setup plugin that manages all core state. Leaves `Builder::setup` and the invoke handler to you. Do not register dialog/updater yourself. |
| `DesktopConfig` | `channel`, extra `data_roots` searched for `ecudefs/` `catalog/` `guide/`, extra checksum `checksums` resolvers (tried before the public set by every checksum command), `updater` on/off. `Default` = the public app's settings. |
| `BuildChannel` | Managed state behind the `build_channel` command. `STABLE`, `ALPHA` (public `--features alpha`), `BENCH`, or any name. Every channel except `stable` enables the debug commands. |
| `public_handler()` / `public_commands()` | The core invoke handler and the names of every core command. |
| `extension_handler![..]` / `with_extra_commands(names, handler)` | An invoke handler that sends your command names to your `generate_handler!` and everything else to the core. A name that collides with a core command (or repeats) panics at startup; `check_extra_commands` reports the same check as a `Result`. |
| `adapter` module | `AdapterHub` (managed), `ConnKey`, `Addr`, `resolve`, `read_addr`, `write_addr`, `enter_extended_session`, `leave_session`. |
| `open_registry_driver(&ConnKey)` | Opens a registry driver bound to a key (the hub's opener). |
| `AppData`, `DebugLog`, `Checksums` | Managed state your commands can take as `tauri::State`. |

## Adapter access and writes

Plugin commands that touch a vehicle adapter **must** go through the shared
`AdapterHub`, never open a driver themselves. The hub holds the app-wide
"one adapter operation at a time" lock (two openers on one serial adapter
corrupt each other) and reuses the open connection:

```rust
#[tauri::command]
async fn my_tool_write(
    driver: String, port: Option<String>, baud: Option<String>, bus: Option<String>,
    tx_id: Option<String>, rx_id: Option<String>,
    timeout_ms: u64, ack: bool,
    hub: tauri::State<'_, opentune_desktop::adapter::AdapterHub>,
) -> Result<serde_json::Value, String> {
    use opentune_desktop::adapter::{self, ConnKey};
    // 1. Explicit target: a mutating operation refuses an unknown module.
    let addr = adapter::write_addr(
        adapter::resolve(tx_id.as_deref(), rx_id.as_deref(), None)?,
        "my tool write",
    )?;
    // 2. Explicit acknowledgement from the UI for anything that can change the
    //    module or count against it (writes, security access attempts, routines).
    if !ack {
        return Err("my tool write needs an explicit acknowledgement".into());
    }
    let key = ConnKey::new(&driver, port.as_deref(), baud.as_deref(), bus.as_deref(), addr);
    // 3. Run on the hub: fails fast with "adapter busy" if another panel is using it.
    hub.run("my tool write", move |s| {
        s.with_diag(&key, timeout_ms, |d| {
            adapter::enter_extended_session(d)?;
            let r = opentune_core::RawDiag::request(d, &[/* ... */]);
            adapter::leave_session(d);
            r.map(|resp| serde_json::json!({ "resp": format!("{resp:02X?}") }))
        })
    })
}
```

Rules for plugin commands:

- Read-only operations may use `read_addr` (falls back to the engine ECU and
  says so); mutating ones use `write_addr` and refuse an unknown target.
- Anything that writes, starts a routine, or makes a security-access attempt
  (a wrong key can count against the module's attempt counter) takes an
  explicit `ack`/`confirm` argument and refuses without it. Destructive local
  writes also require a backup path first.
- Return "adapter busy" errors as-is; the UI retries after a short wait.
- Long destructive operations should not overlap a software update: the core
  update flow and flash/write-back share one long-operation guard.

## Frontend: overlay scripts

The plugin build's UI is the public `desktop/ui` **plus** its own scripts.
Point the plugin's `tauri.conf.json` `build.frontendDist` at an assembled
directory (gitignored in the plugin repo), produced by a small step run from
`beforeDevCommand`/`beforeBuildCommand`:

1. copy the public `desktop/ui/` into the dist directory;
2. copy the plugin's overlay scripts into `dist/src/`;
3. overwrite `dist/src/extensions.js` with
   `window.OT_EXTENSIONS = ["my_tool.js", ...];` followed by the public
   loader body (or set `OT_EXTENSIONS` before including it).

`index.html` loads `src/extensions.js` last; it appends
`<script src="./src/<name>">` for each entry, in order, after every built-in
panel. The public build ships an empty list.

An overlay panel script:

```js
(function () {
  const sec = document.createElement("section");
  sec.className = "panel";
  sec.id = "panel-mytool";
  sec.innerHTML = "<h2>My tool</h2>...";
  document.getElementById("main").appendChild(sec);
  window.OTNav.register({ id: "mytool", label: "My tool", icon: "*", group: "bench", tier: "bench" });
  // window.showPanel("mytool") switches to it.
})();
```

- `group` is one of `calibration`, `vehicle`, `reference`, `help`, `bench`.
- `tier: "public"` panels sit in their own group in every build.
- `tier: "bench"` panels always go to the **Bench** group at the bottom of the
  sidebar and are shown only when the build channel is `alpha` or `bench`. Use
  it for untested tools and for anything that is not public. A tool graduates
  by changing its tier once it has passed a real-car test.
- To extend an existing public panel, append a card into its section
  (`#panel-<id>`) when present instead of forking the public script.
- Use `OT.connArgs()` for adapter arguments (driver/port/baud/bus/txId/rxId) so
  plugin commands target the same module as the public panels.

## CLI host API

The CLI is a library too: `opentune_cli` (`crates/opentune-cli/src/lib.rs`),
whose `opentune` binary only calls `opentune_cli::run()`. A private front end
is a separate binary that calls `run_with(CliConfig)`: every public subcommand
works unchanged, and the front end adds its own subcommands, registry drivers,
checksum algorithms and seed-key handlers.

```toml
# front end's Cargo.toml
[dependencies]
opentune-cli = { path = "../opentune/crates/opentune-cli" }
opentune-core = { path = "../opentune/crates/opentune-core" }
opentune-checksum = { path = "../opentune/crates/opentune-checksum" }
clap = "4"
anyhow = "1"
```

```rust
// front end's src/main.rs
use clap::{Args, FromArgMatches};
use opentune_checksum::ChecksumAlgo;
use opentune_cli::{CliConfig, CliContext, ConnectArgs, ExtraCommand};
use opentune_core::protocol::seedkey::SeedKey;

/// Extra checksum ids, tried before the public set by `checksum verify|fix|attest`
/// and `recipe create|apply`.
fn my_checksums(id: &str) -> Option<Box<dyn ChecksumAlgo>> {
    match id {
        "my-algo" => Some(Box::new(my_algos::MyAlgo)),
        _ => None,
    }
}

/// Seed-key handlers for `--seedkey <id>` (info/read/flash/dump/identify/service).
fn my_seedkeys(id: &str) -> Option<Box<dyn SeedKey>> {
    match id {
        "my-unit" => Some(Box::new(my_keys::MyUnit)),
        _ => None,
    }
}

/// `my-tool bench-read --driver kkl --seedkey my-unit ...`
fn bench_read(m: &clap::ArgMatches, ctx: &CliContext) -> anyhow::Result<()> {
    let conn = ConnectArgs::from_arg_matches(m)?; // --driver/--file/--base/--seedkey
    let mut drv = ctx.open_connect(&conn)?; // seed key attached before open
    // ... drv.read_memory(...), ctx.checksum("my-algo"), ...
    drv.close()?;
    Ok(())
}

fn main() -> std::process::ExitCode {
    opentune_cli::run_with(CliConfig {
        name: "my-tool",
        about: Some("OpenTune plus my bench tools".into()),
        seedkeys: vec![my_seedkeys],
        checksums: vec![my_checksums],
        drivers: vec![("my-adapter", "My adapter", my_adapter::make)], // fn() -> Box<dyn Interface>
        extra: vec![ExtraCommand {
            command: ConnectArgs::augment_args(
                clap::Command::new("bench-read").about("Read through my unlock"),
            ),
            run: bench_read,
        }],
    })
}
```

| Item | Purpose |
|---|---|
| `run()` / `run_with(CliConfig)` | Run the public CLI / the configured one on the process arguments; returns the `ExitCode`. |
| `run_from(cfg, args)` / `try_run_from(cfg, args)` | Same on explicit arguments, never exiting the process; `try_run_from` returns help/usage as a `clap::Error`, a silent exit as `ExitStatus`, other failures as the command's error (for tests). |
| `CliConfig` | `name` (help and `--version`, default `opentune`), `about`, `seedkeys`, `checksums`, `drivers` (`(id, description, fn() -> Box<dyn Interface>)`, listed after the public drivers), `extra` subcommands. `Default` = the public CLI. |
| `SeedKeyResolver` / `ChecksumResolver` | `fn(&str) -> Option<Box<dyn SeedKey>>` / `fn(&str) -> Option<Box<dyn ChecksumAlgo>>`, the desktop's shapes. Tried in order; checksum ids fall back to the public `opentune_checksum::get`. |
| `ExtraCommand { command, run }` | A `clap::Command` merged into the top-level tree and its `fn(&ArgMatches, &CliContext) -> anyhow::Result<()>`. A name or alias that collides with a core subcommand, `help` or another extra (or an extra driver id that repeats a public one) is refused at startup; `check_config` reports the same check. |
| `CliContext` | `registry()`, `checksum(id)` / `require_checksum(id)`, `seed_key(id)` / `require_seed_key(id)`, `open_driver(driver, &ConnectConfig, seedkey)`, `open_connect(&ConnectArgs)`, `attach_seed_key(drv, driver, id)`. |
| `ConnectArgs` | The public `--driver --file --base --seedkey` group, to flatten into extra commands. |
| `command(cfg)` | The merged clap tree (e.g. to render help in tests). |
| `ExitStatus(n)` | Return `Err(ExitStatus(n).into())` to end with exit code `n` and no error message. |

`--seedkey <id>` on `info`, `read`, `flash`, `dump` and `identify` resolves the
id through the context and hands it to the driver with `Interface::set_seed_key`
before the driver opens. An unknown id is an error before any driver is made;
a driver without an unlock step (`set_seed_key` returns `false`, e.g. `mock`)
is an error rather than a silently dropped key. On `service`, `--seedkey`
becomes the script runner's `KeyProvider` for its SecurityAccess steps.

Safety expectations for a CLI front end:

- **The embedder owns every algorithm it registers.** OpenTune ships no OEM
  seed/key algorithm and registers no seed-key resolver, so `--seedkey` fails
  in the public build; checksum algorithms the public set lacks come only from
  the embedder. Keep them in the embedder's own repository.
- Extra commands that write, start a routine or attempt security access follow
  the same rules as desktop plugin commands: explicit target, explicit
  acknowledgement flag, backup before destructive writes, and the core flash
  orchestrator (`opentune_flash`) for writes rather than raw driver calls.
- A checksum resolver must refuse rather than guess, like the public engines:
  return `None` for an id it does not fully support.
- Core subcommands cannot be replaced or shadowed; add a new name instead.
