//! OpenTune command-line interface, as a library.
//!
//! Wires the available interface drivers into a registry and exposes the
//! offline-capable operations (identify, read, diff, checksum) plus a
//! safety-gated flash. With the `mock` driver everything works on local files.
//! The `opentune` binary is a thin wrapper around [`run`].
use std::process::ExitCode;

mod candump;
mod cli;
mod commands;
mod context;
mod mapfind_cmd;
mod obd_cmd;
mod recipe_cmd;
mod registry;
mod util;

/// Run the public OpenTune CLI on the process arguments.
pub fn run() -> ExitCode {
    let cli = <cli::Cli as clap::Parser>::parse();
    let ctx = context::CliContext::new(registry::public_registry());
    match commands::dispatch(&ctx, cli.cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // Same report as `fn main() -> anyhow::Result<()>`.
            eprintln!("Error: {e:?}");
            ExitCode::FAILURE
        }
    }
}
