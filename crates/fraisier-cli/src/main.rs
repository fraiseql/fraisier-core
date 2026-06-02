//! The `fraisier` command-line binary.
//!
//! It wires the parts the lower crates keep apart: [`fraisier_config`] for the
//! `fraisier.toml`, [`fraisier_core`] for the deploy composition and adapter
//! axes, [`fraisier_saga`] for the state store, the in-process adapter crates,
//! and [`fraisier_ipc`] for external adapters. Per the crate-graph rule this is
//! the layer that depends on *both* `fraisier-core` and `fraisier-ipc` and
//! chooses between the in-process and IPC adapter paths (see [`factory`]).

// Reason: `pub(crate)` on items in this binary's private modules is intentional
// and clearer than bare `pub`; the nursery lint's suggestion (`pub`) would read as
// a public API that a binary crate does not actually expose.
#![allow(clippy::redundant_pub_crate)]

mod commands;
mod factory;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::commands::CommandOutput;

/// Atomic, rollback-safe deploys — single binary, adapter ecosystem.
#[derive(Debug, Parser)]
#[command(name = "fraisier", version, about, long_about = None)]
struct Cli {
    /// Emit machine-readable JSON instead of the human-readable rendering.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Parse, expand the SpecQL preset, and validate a `fraisier.toml`.
    ValidateConfig(ConfigArgs),

    /// Run a single-host deploy (or, with `--dry-run`, just resolve the plan).
    Deploy(DeployArgs),

    /// Show the recorded saga state and release ledger for a deploy.
    Status(StatusArgs),

    /// Inspect the migration adapters available as external processes.
    #[command(subcommand)]
    Adapter(AdapterCommand),
}

#[derive(Debug, Args)]
struct ConfigArgs {
    /// Path to the `fraisier.toml`.
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,
}

#[derive(Debug, Args)]
struct DeployArgs {
    /// Path to the `fraisier.toml`.
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,

    /// Directory for the filesystem state store.
    #[arg(long, default_value = ".fraisier/state")]
    state_dir: PathBuf,

    /// Override the single host to deploy to (required for multi-host configs).
    #[arg(long)]
    host: Option<String>,

    /// The application version, substituted for `{version}` in artifact URLs.
    #[arg(long)]
    app_version: Option<String>,

    /// Resolve and print the plan without executing anything.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct StatusArgs {
    /// Path to the `fraisier.toml`.
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,

    /// Directory for the filesystem state store.
    #[arg(long, default_value = ".fraisier/state")]
    state_dir: PathBuf,
}

#[derive(Debug, Subcommand)]
enum AdapterCommand {
    /// List `fraisier-adapter-*` binaries discoverable on `PATH`.
    List,
    /// Run the `describe` handshake against `fraisier-adapter-<name>`.
    Describe {
        /// The adapter name (without the `fraisier-adapter-` prefix).
        name: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(&cli).await {
        Ok(output) => {
            render(&output, cli.json);
            // Exit codes are small and non-negative; the clamp is just for safety.
            ExitCode::from(u8::try_from(output.exit_code).unwrap_or(1))
        }
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(cli: &Cli) -> Result<CommandOutput> {
    match &cli.command {
        Command::ValidateConfig(args) => commands::validate_config(&args.config),
        Command::Deploy(args) => {
            commands::deploy(
                &args.config,
                &args.state_dir,
                args.host.as_deref(),
                args.app_version.as_deref(),
                args.dry_run,
            )
            .await
        }
        Command::Status(args) => commands::status(&args.config, &args.state_dir).await,
        Command::Adapter(AdapterCommand::List) => Ok(commands::adapter_list()),
        Command::Adapter(AdapterCommand::Describe { name }) => {
            commands::adapter_describe(name).await
        }
    }
}

fn render(output: &CommandOutput, json: bool) {
    if json {
        match serde_json::to_string_pretty(&output.json) {
            Ok(rendered) => println!("{rendered}"),
            Err(error) => eprintln!("error: failed to render JSON: {error}"),
        }
    } else {
        print!("{}", output.pretty);
    }
}
