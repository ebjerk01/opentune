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
        seedkeys: vec![/* fn(&str) -> Option<Box<dyn SeedKey>>, see "Seed/key algorithms" */],
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
| `DesktopConfig` | `channel`, extra `data_roots` searched for `ecudefs/` `catalog/` `guide/`, extra checksum `checksums` resolvers (tried before the public set by every checksum command), `seedkeys` resolvers (see below; empty in the public app), `updater` on/off. `Default` = the public app's settings. |
| `BuildChannel` | Managed state behind the `build_channel` command. `STABLE`, `ALPHA` (public `--features alpha`), `BENCH`, or any name. Every channel except `stable` enables the debug commands. |
| `public_handler()` / `public_commands()` | The core invoke handler and the names of every core command. |
| `extension_handler![..]` / `with_extra_commands(names, handler)` | An invoke handler that sends your command names to your `generate_handler!` and everything else to the core. A name that collides with a core command (or repeats) panics at startup. |
| `command_names(list)` | Splits a stringified `generate_handler!`-style list (`"a, m::b"`) into the command names the frontend invokes (`["a", "b"]`, the last path segment of each entry). `extension_handler!` uses it; call it yourself to build the `names` for `with_extra_commands`. |
| `check_extra_commands(&names)` | The collision check `with_extra_commands` runs, as a `Result` instead of a panic: `Err` names the first command that repeats or shadows a core command. Useful in a plugin test. |
| `adapter` module | `AdapterHub` (managed; `run`, `start_op`), `Session` (`with_diag`, `interface`), `ConnKey`, `Addr`, `resolve`, `read_addr`, `write_addr`, `enter_extended_session`, `leave_session`, `apply_seed_key`. `Addr::json(assumed)` gives the UI's `{ tx, rx, assumed }` target object (hex strings; `assumed` = the engine ECU was assumed for a read). |
| `open_registry_driver(&ConnKey)` | Opens a registry driver bound to a key (the hub's opener). |
| `AppData`, `DebugLog`, `Checksums`, `SeedKeys` | Managed state your commands can take as `tauri::State`. |

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

## Seed/key algorithms

OpenTune ships **no** seed/key (SecurityAccess) algorithm, and the core never
will. A plugin that has one supplies it through `DesktopConfig::seedkeys`: a
list of `SeedKeyResolver = fn(&str) -> Option<Box<dyn SeedKey>>`, asked in
order by id. The core manages them as `SeedKeys` and uses them in three places:

- **Service scripts** whose JSON names a `"seedkey": "<id>"` run their
  `security_access` steps with a key provider wrapping the resolved algorithm.
  Without the field, or with an id no resolver knows, the script stays
  plan-only and the plan and the refused run name the missing id.
- The **Utilities seed/key calculator**: an algorithm id that is not one of
  the generic transforms is resolved through `SeedKeys` (pass `level` for the
  access level, default `0x01`). The result carries `source: "plugin"` and a
  note saying a plugin computed it. Add the option to the `#skAlgo` selector
  from your UI overlay; the public list does not change.
- **Your own read/flash commands**: attach a handler to the hub-opened driver
  with `adapter::apply_seed_key` so the driver's unlock step uses it.

```rust
use opentune_core::protocol::seedkey::SeedKey;

struct MyAlgo; // your algorithm; its provenance is your responsibility
impl SeedKey for MyAlgo {
    fn id(&self) -> &str { "my-ecu-level1" }
    fn compute(&self, seed: &[u8], _level: u8) -> opentune_core::Result<Vec<u8>> {
        Ok(seed.to_vec() /* ... */)
    }
}

fn my_seedkeys(id: &str) -> Option<Box<dyn SeedKey>> {
    match id {
        "my-ecu-level1" => Some(Box::new(MyAlgo)),
        _ => None, // unknown ids fall through to the next resolver
    }
}

// DesktopConfig { seedkeys: vec![my_seedkeys], .. }

#[tauri::command]
async fn my_read(
    driver: String, port: Option<String>, tx_id: Option<String>, rx_id: Option<String>,
    ack: bool,
    hub: tauri::State<'_, opentune_desktop::adapter::AdapterHub>,
    keys: tauri::State<'_, opentune_desktop::SeedKeys>,
) -> Result<serde_json::Value, String> {
    use opentune_desktop::adapter::{self, ConnKey};
    if !ack { return Err("my read needs an explicit acknowledgement".into()); }
    let addr = adapter::write_addr(
        adapter::resolve(tx_id.as_deref(), rx_id.as_deref(), None)?,
        "my read",
    )?;
    let key = ConnKey::new(&driver, port.as_deref(), None, None, addr);
    let keys = keys.inner().clone();
    hub.run("my read", move |s| {
        let drv = s.interface(&key)?;
        // Ok(false) = this driver has no unlock step of its own.
        adapter::apply_seed_key(drv, &keys, "my-ecu-level1")?;
        /* drv.read_memory(..) */
        Ok(serde_json::json!({}))
    })
}
```

Expectations:

- The **plugin owns the algorithm**: where it came from, that it may be
  distributed, and that it is correct. Never contribute one to this repo.
- A wrong key counts against the module's attempt counter. Every command that
  can make a security-access attempt takes an explicit `ack`.
- Resolve on the adapter worker (inside `hub.run`), as above: a
  `Box<dyn SeedKey>` need not be `Send`, but `SeedKeys` is `Clone + Send`.
- The attached handler lives as long as the opened driver. The hub re-opens
  it after an error or a connection change, so attach it in every operation.
- Return `None` for ids you do not know; never return a guess.

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
- Globals an overlay may rely on: `window.OT` (`conn`, `connArgs`, `gate`,
  `probe`, `onCaps`), `window.OTNav.register`, `window.showPanel`,
  `window.onPanelFirstShow`, `toast(msg, bad)`, and the pure helpers in
  `window.OTUtil` (`esc`, `hex8`, `parseHexInput`, `renderMarkdown`, …). The
  public app scripts (`util.js`, `app-core.js` … `app-boot.js`) load before any
  overlay. `desktop/ui/tests/load.test.mjs` checks these globals.
