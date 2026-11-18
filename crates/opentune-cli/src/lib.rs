//! OpenTune command-line interface, as a library.
//!
//! Wires the available interface drivers into a registry and exposes the
//! offline-capable operations (identify, read, diff, checksum) plus a
//! safety-gated flash. With the `mock` driver everything works on local files.
//! The `opentune` binary is a thin wrapper around [`run`].
//!
//! # Host API
//!
//! A separate front end can reuse the whole public CLI and add its own
//! subcommands, drivers, checksum algorithms and seed-key handlers without
//! forking, by calling [`run_with`] with a [`CliConfig`] (see `docs/PLUGINS.md`,
//! "CLI host API"). OpenTune itself ships no seed/key algorithm; the embedder
//! owns any it registers.
//!
//! ```no_run
//! use opentune_cli::{CliConfig, CliContext, ExtraCommand};
//!
//! fn hello(m: &clap::ArgMatches, ctx: &CliContext) -> anyhow::Result<()> {
//!     let who = m.get_one::<String>("who").map_or("world", String::as_str);
//!     println!("hello {who}; {} drivers", ctx.registry().list().len());
//!     Ok(())
//! }
//!
//! fn main() -> std::process::ExitCode {
//!     opentune_cli::run_with(CliConfig {
//!         name: "my-tool",
//!         about: Some("OpenTune plus my own commands".into()),
//!         extra: vec![ExtraCommand {
//!             command: clap::Command::new("hello")
//!                 .about("Say hello")
//!                 .arg(clap::Arg::new("who")),
//!             run: hello,
//!         }],
//!         ..CliConfig::default()
//!     })
//! }
//! ```
use clap::{CommandFactory, FromArgMatches};
use std::collections::HashSet;
use std::ffi::OsString;
use std::process::ExitCode;

mod bsl_cmd;
mod candump;
mod cli;
mod commands;
mod context;
mod ctrldef_cmd;
mod doip_cmd;
mod dsg_cmd;
mod gps_cmd;
mod haldex_cmd;
mod mapfind_cmd;
mod obd_cmd;
mod recipe_cmd;
mod registry;
mod seedkey_cmd;
mod ts_cmd;
mod util;

pub use cli::ConnectArgs;
pub use context::CliContext;

/// Looks up an extra seed-key handler by id (the `--seedkey` value).
pub type SeedKeyResolver = fn(&str) -> Option<Box<dyn opentune_core::protocol::seedkey::SeedKey>>;

/// Looks up an extra checksum algorithm by id (an `--algo` / `--checksum`
/// value), tried before the public `opentune_checksum` set.
pub type ChecksumResolver = fn(&str) -> Option<Box<dyn opentune_checksum::ChecksumAlgo>>;

/// Constructs an extra registry driver.
pub type DriverFactory = fn() -> Box<dyn opentune_core::Interface>;

/// Runs an extra subcommand with its own matches and the shared context.
pub type ExtraRun = fn(&clap::ArgMatches, &CliContext) -> anyhow::Result<()>;

/// An extra top-level subcommand merged into the public command tree.
#[derive(Clone)]
pub struct ExtraCommand {
    /// The subcommand's clap definition (builder API, or `MyArgs::command()`
    /// from a derive). Its name and aliases must not collide with a core
    /// subcommand.
    pub command: clap::Command,
    /// Called with the subcommand's own matches when it is selected.
    pub run: ExtraRun,
}

/// How [`run_with`] builds the CLI. `Default` is the public `opentune` CLI.
#[derive(Clone)]
pub struct CliConfig {
    /// Program name shown in help and `--version` (default `opentune`).
    pub name: &'static str,
    /// Replaces the top-level about text when set.
    pub about: Option<String>,
    /// Seed-key resolvers for `--seedkey`, tried in order.
    pub seedkeys: Vec<SeedKeyResolver>,
    /// Checksum resolvers, tried in order before the public set.
    pub checksums: Vec<ChecksumResolver>,
    /// Extra registry drivers `(id, description, factory)`, listed after the
    /// public ones. An id may not repeat a public driver id.
    pub drivers: Vec<(&'static str, &'static str, DriverFactory)>,
    /// Extra top-level subcommands.
    pub extra: Vec<ExtraCommand>,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            name: "opentune",
            about: None,
            seedkeys: Vec::new(),
            checksums: Vec::new(),
            drivers: Vec::new(),
            extra: Vec::new(),
        }
    }
}

/// Ends the run with this exit code and no error message (the command has
/// already printed its report). Return it as `Err(ExitStatus(n).into())`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus(pub u8);

impl std::fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit status {}", self.0)
    }
}

impl std::error::Error for ExitStatus {}

/// Check a config for name collisions: an extra subcommand (name or alias)
/// that repeats a core subcommand, `help`, or another extra; an extra driver id
/// that repeats a public driver id or another extra. [`run_with`] refuses such
/// a config at startup with this error.
pub fn check_config(cfg: &CliConfig) -> anyhow::Result<()> {
    let core = cli::Cli::command();
    let mut taken: HashSet<String> = core
        .get_subcommands()
        .flat_map(|c| std::iter::once(c.get_name()).chain(c.get_all_aliases()))
        .map(str::to_owned)
        .collect();
    taken.insert("help".to_owned());
    for e in &cfg.extra {
        let names = std::iter::once(e.command.get_name()).chain(e.command.get_all_aliases());
        for n in names {
            if !taken.insert(n.to_owned()) {
                anyhow::bail!(
                    "extra command '{n}' collides with an existing {} command \
                     (core commands cannot be replaced; rename the extra command)",
                    cfg.name
                );
            }
        }
    }
    let mut ids: HashSet<&str> = registry::public_registry().ids().collect();
    for (id, _, _) in &cfg.drivers {
        if !ids.insert(*id) {
            anyhow::bail!("extra driver '{id}' collides with an existing driver id");
        }
    }
    Ok(())
}

/// The full clap command tree for `cfg`: the core subcommands plus the extras.
/// Fails like [`check_config`].
pub fn command(cfg: &CliConfig) -> anyhow::Result<clap::Command> {
    check_config(cfg)?;
    let mut cmd = cli::Cli::command().name(cfg.name).bin_name(cfg.name);
    if let Some(about) = &cfg.about {
        cmd = cmd.about(about.clone());
    }
    for e in &cfg.extra {
        cmd = cmd.subcommand(e.command.clone());
    }
    Ok(cmd)
}

/// Run the public OpenTune CLI on the process arguments.
pub fn run() -> ExitCode {
    run_with(CliConfig::default())
}

/// Run the CLI described by `cfg` on the process arguments.
pub fn run_with(cfg: CliConfig) -> ExitCode {
    run_from(cfg, std::env::args_os())
}

/// Run the CLI described by `cfg` on `args` (`args[0]` is the program name),
/// printing errors and help like the binary does, and return the exit code.
/// Never exits the process.
pub fn run_from<I, T>(cfg: CliConfig, args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    match try_run_from(cfg, args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => report(&e),
    }
}

/// [`run_from`] without the reporting: help/usage come back as a
/// `clap::Error`, a silent exit as [`ExitStatus`], anything else as the
/// command's error.
pub fn try_run_from<I, T>(cfg: CliConfig, args: I) -> anyhow::Result<()>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let mut cmd = command(&cfg)?;
    let matches = cmd.try_get_matches_from_mut(args)?;
    let ctx = CliContext::from_config(&cfg);
    if let Some((name, sub)) = matches.subcommand() {
        if let Some(extra) = cfg.extra.iter().find(|e| e.command.get_name() == name) {
            return (extra.run)(sub, &ctx);
        }
    }
    let cli = cli::Cli::from_arg_matches(&matches).map_err(|e| e.format(&mut cmd))?;
    commands::dispatch(&ctx, cli.cmd)
}

/// Print an error the way the binary always has and pick its exit code.
fn report(e: &anyhow::Error) -> ExitCode {
    if let Some(ce) = e.downcast_ref::<clap::Error>() {
        // Help/version to stdout (exit 0), usage errors to stderr (exit 2).
        let _ = ce.print();
        return ExitCode::from(u8::try_from(ce.exit_code()).unwrap_or(2));
    }
    if let Some(ExitStatus(code)) = e.downcast_ref::<ExitStatus>() {
        return ExitCode::from(*code);
    }
    // Same report as `fn main() -> anyhow::Result<()>`.
    eprintln!("Error: {e:?}");
    ExitCode::FAILURE
}
