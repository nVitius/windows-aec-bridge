use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::{EndpointArgs, RunArgs, bridge, devices};

#[derive(Debug, Parser)]
#[command(
    name = "aec-bridge-cli",
    version,
    about = "Command-line diagnostics and control for AEC Bridge"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List active WASAPI audio endpoints and their stable IDs.
    List,

    /// Resolve and validate the three endpoints without opening audio streams.
    Check(EndpointArgs),

    /// Run the live AEC bridge until Ctrl+C or the requested duration elapses.
    Run(RunArgs),
}

pub fn run() -> Result<()> {
    match Cli::parse().command {
        Command::List => devices::print_devices(),
        Command::Check(args) => devices::check_endpoints(&args),
        Command::Run(args) => bridge::run(args),
    }
}
