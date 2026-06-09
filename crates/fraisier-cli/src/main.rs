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
#[cfg(test)]
mod e2e;
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
    /// Write a starter `fraisier.toml` into the current project.
    Init(InitArgs),

    /// Parse, expand the SpecQL preset, and validate a `fraisier.toml`.
    ValidateConfig(ConfigArgs),

    /// Run a single-host deploy (or, with `--dry-run`, just resolve the plan).
    Deploy(DeployArgs),

    /// List every deploy recorded in the state store.
    List(ListArgs),

    /// Probe the configured health endpoint on every host.
    Health(HealthArgs),

    /// Roll the deploy back to a prior revision (migrates the database down).
    Rollback(RollbackArgs),

    /// Show the recorded saga state and release ledger for a deploy.
    Status(StatusArgs),

    /// Prepare each target host's deploy directories over SSH (or locally).
    Bootstrap(BootstrapArgs),

    /// Run the signed-webhook deploy trigger server (socket-activated or standalone).
    WebhookServer(WebhookServerArgs),

    /// Coordinated management of fraisier's own service (restart for now).
    #[command(subcommand)]
    SelfUpgrade(SelfUpgradeCommand),

    /// Manage scheduled fraisier runs via systemd timers.
    #[command(subcommand)]
    Scheduled(ScheduledCommand),

    /// Share the deploy ledger across operators over git refs (experimental).
    Sync(SyncArgs),

    /// Back up the database to a custom-format archive (`pg_dump -Fc`).
    Backup(BackupArgs),

    /// Database lifecycle operations (migrate / restore / reset).
    #[command(subcommand)]
    Db(DbCommand),

    /// List the adapters available to this binary, per axis (built-in + IPC).
    Providers,

    /// Probe one provider (IPC handshake, or presence for a built-in).
    ProviderTest(ProviderTestArgs),

    /// Show or bump the project version (`Cargo.toml` / `pyproject.toml`).
    #[command(subcommand)]
    Version(VersionCommand),

    /// Bump the version, commit, push, and (unless `--no-deploy`) deploy it.
    Ship(ShipCliArgs),

    /// Run the project's `[[checks]]` (lint/test/typecheck) with cross-check parallelism.
    Check(CheckCliArgs),

    /// Generate the systemd/socket/nginx/CI deploy files into a local tree.
    Scaffold(ScaffoldArgs),

    /// Install the generated system files (and optionally prune stale ones).
    ScaffoldInstall(ScaffoldInstallArgs),
}

#[derive(Debug, Args)]
struct ScaffoldArgs {
    /// Path to the `fraisier.toml`.
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,

    /// Directory to write the generated tree into.
    #[arg(long, default_value = "deploy")]
    out: PathBuf,

    /// List the files that would be written without writing them.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct ScaffoldInstallArgs {
    /// Path to the `fraisier.toml`.
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,

    /// Filesystem prefix the system paths are installed under (override for
    /// sandboxed installs; defaults to the real root).
    #[arg(long, default_value = "/")]
    root: PathBuf,

    /// Also remove stale fraisier-generated files from the install directories.
    #[arg(long)]
    prune: bool,

    /// Apply the changes (without this, only the plan is shown).
    #[arg(long)]
    yes: bool,

    /// Show the install + prune plans without applying them.
    #[arg(long)]
    dry_run: bool,
}

// Reason: these are independent ship flags (dry-run / no-deploy / no-check /
// no-push); folding them into enums would be more ceremony than signal.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Args)]
struct ShipCliArgs {
    /// Which component to bump.
    #[arg(value_enum)]
    level: BumpLevel,

    /// The project directory holding `Cargo.toml` / `pyproject.toml`.
    #[arg(long, default_value = ".")]
    path: PathBuf,

    /// Show the plan without writing, committing, pushing, or deploying.
    #[arg(long)]
    dry_run: bool,

    /// Skip the follow-on deploy.
    #[arg(long)]
    no_deploy: bool,

    /// Skip the pre-bump `[[checks]]` gate (checks run before the bump by default).
    #[arg(long)]
    no_check: bool,

    /// Do not push the release commit.
    #[arg(long)]
    no_push: bool,

    /// The git remote to push to.
    #[arg(long, default_value = "origin")]
    remote: String,

    /// The deploy config (used only when a deploy follows).
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,

    /// The state store directory (used only when a deploy follows).
    #[arg(long, default_value = ".fraisier/state")]
    state_dir: PathBuf,

    /// The single-host override (used only when a deploy follows).
    #[arg(long)]
    host: Option<String>,
}

#[derive(Debug, Args)]
struct CheckCliArgs {
    /// The `fraisier.toml` to read `[[checks]]` from.
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,

    /// The directory checks run in (the base for a check with no `workdir`).
    #[arg(long, default_value = ".")]
    path: PathBuf,

    /// Max checks to run concurrently. 0 = auto (number of logical CPUs).
    #[arg(short = 'j', long, default_value_t = 0)]
    jobs: usize,
}

#[derive(Debug, Subcommand)]
enum VersionCommand {
    /// Print the current project version.
    Show(ProjectArgs),
    /// Increment the version in place, preserving formatting.
    Bump {
        /// Which component to increment.
        #[arg(value_enum)]
        level: BumpLevel,
        #[command(flatten)]
        project: ProjectArgs,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum BumpLevel {
    Major,
    Minor,
    Patch,
}

impl From<BumpLevel> for fraisier_ship::Bump {
    fn from(level: BumpLevel) -> Self {
        match level {
            BumpLevel::Major => Self::Major,
            BumpLevel::Minor => Self::Minor,
            BumpLevel::Patch => Self::Patch,
        }
    }
}

#[derive(Debug, Args)]
struct ProjectArgs {
    /// The project directory holding `Cargo.toml` / `pyproject.toml`.
    #[arg(long, default_value = ".")]
    path: PathBuf,
}

#[derive(Debug, Args)]
struct ConfigArgs {
    /// Path to the `fraisier.toml`.
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,
}

#[derive(Debug, Args)]
struct InitArgs {
    /// Where to write the starter config.
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,

    /// Overwrite the file if it already exists.
    #[arg(long)]
    force: bool,
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

    /// Also query each host's live active artifact.
    #[arg(long)]
    per_host: bool,
}

#[derive(Debug, Args)]
struct ListArgs {
    /// Directory for the filesystem state store.
    #[arg(long, default_value = ".fraisier/state")]
    state_dir: PathBuf,

    /// Print only `fraise/environment` names, one per line.
    #[arg(long)]
    flat: bool,
}

#[derive(Debug, Args)]
struct HealthArgs {
    /// Path to the `fraisier.toml`.
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,

    /// Probe only this host (inventory name or address).
    #[arg(long)]
    host: Option<String>,
}

#[derive(Debug, Args)]
struct RollbackArgs {
    /// Path to the `fraisier.toml`.
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,

    /// Directory for the filesystem state store.
    #[arg(long, default_value = ".fraisier/state")]
    state_dir: PathBuf,

    /// The revision to roll the database down to.
    #[arg(long)]
    to: String,

    /// The application version of the rolled-back-to artifact to stage.
    #[arg(long)]
    app_version: Option<String>,

    /// Pin a single host (required for a multi-host config).
    #[arg(long)]
    host: Option<String>,

    /// Execute the rollback (without this, only the plan is shown).
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Subcommand)]
enum DbCommand {
    /// Apply pending migrations through the configured migration adapter.
    Migrate(DbMigrateArgs),

    /// Restore a `pg_dump` archive into the database (drops existing objects).
    Restore(DbRestoreArgs),

    /// Drop every user schema and re-apply migrations from scratch.
    Reset(DbResetArgs),
}

#[derive(Debug, Args)]
struct DbMigrateArgs {
    /// Path to the `fraisier.toml`.
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,

    /// Directory for the filesystem state store.
    #[arg(long, default_value = ".fraisier/state")]
    state_dir: PathBuf,
}

#[derive(Debug, Args)]
struct WebhookServerArgs {
    /// Path to the `fraisier.toml`.
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,

    /// Directory for the filesystem state store (used by triggered deploys).
    #[arg(long, default_value = ".fraisier/state")]
    state_dir: PathBuf,

    /// Override the standalone bind address (`host:port`). Ignored under systemd
    /// socket activation.
    #[arg(long)]
    listen: Option<String>,
}

#[derive(Debug, Args)]
struct BootstrapArgs {
    /// Path to the `fraisier.toml`.
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,

    /// Prepare only this host (inventory name or address).
    #[arg(long)]
    host: Option<String>,

    /// Show what would be created without creating anything.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct BackupArgs {
    /// Path to the `fraisier.toml`.
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,

    /// Where to write the archive (defaults to `<fraise>-<environment>.pgdump`).
    #[arg(long)]
    output: Option<PathBuf>,

    /// Overwrite the output file if it already exists.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct DbRestoreArgs {
    /// Path to the `fraisier.toml`.
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,

    /// The `pg_dump -Fc` archive to restore.
    #[arg(long)]
    input: PathBuf,

    /// Execute the restore (without this, only the plan is shown). Destructive.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct DbResetArgs {
    /// Path to the `fraisier.toml`.
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,

    /// Directory for the filesystem state store.
    #[arg(long, default_value = ".fraisier/state")]
    state_dir: PathBuf,

    /// Execute the reset (without this, only the plan is shown). Destructive.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct SyncArgs {
    /// Path to the `fraisier.toml`.
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,

    /// Directory for the filesystem state store.
    #[arg(long, default_value = ".fraisier/state")]
    state_dir: PathBuf,

    /// Fetch remote ledger state into the local store (instead of pushing).
    #[arg(long)]
    pull: bool,

    /// Delete remote sync refs that have no local deploy (orphan reclaim).
    #[arg(long)]
    reclaim_orphans: bool,
}

#[derive(Debug, Subcommand)]
enum ScheduledCommand {
    /// Generate + install the systemd timer + service for scheduled runs.
    Install(ScheduledInstallArgs),

    /// List the fraisier-installed systemd units (marker-bearing) under a root.
    List(ScheduledListArgs),

    /// Remove the scheduled timer + service this config installed.
    Uninstall(ScheduledUninstallArgs),
}

#[derive(Debug, Args)]
struct ScheduledListArgs {
    /// Filesystem prefix the units were installed under (defaults to the real root).
    #[arg(long, default_value = "/")]
    root: PathBuf,
}

#[derive(Debug, Args)]
struct ScheduledUninstallArgs {
    /// Path to the `fraisier.toml` (names the units to remove).
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,

    /// Filesystem prefix the units were installed under (defaults to the real root).
    #[arg(long, default_value = "/")]
    root: PathBuf,

    /// Apply the removal (without this, only the plan is shown).
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct ScheduledInstallArgs {
    /// Path to the `fraisier.toml`.
    #[arg(long, default_value = "fraisier.toml")]
    config: PathBuf,

    /// Filesystem prefix the unit files are installed under (override for
    /// sandboxed installs; defaults to the real root).
    #[arg(long, default_value = "/")]
    root: PathBuf,

    /// Apply the changes (without this, only the plan is shown).
    #[arg(long)]
    yes: bool,

    /// Show the install plan without applying it.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Subcommand)]
enum SelfUpgradeCommand {
    /// Coordinated restart of fraisier's own long-running unit (webhook server).
    Restart(SelfUpgradeRestartArgs),

    /// Fetch + verify + swap fraisier's own binary, with post-restart health-check
    /// and auto-revert to the kept-old binary on failure.
    Apply(SelfUpgradeApplyArgs),
}

#[derive(Debug, Args)]
struct SelfUpgradeRestartArgs {
    /// The systemd unit to restart (fraisier's own service).
    #[arg(long, default_value = "fraisier-webhook.service")]
    unit: String,

    /// Use the user systemd manager (`systemctl --user`).
    #[arg(long)]
    user: bool,
}

#[derive(Debug, Args)]
struct SelfUpgradeApplyArgs {
    /// The new binary: an `http(s)://` URL or a local filesystem path.
    source: String,

    /// Expected SHA-256 (hex) of the binary; a mismatch aborts before any swap.
    #[arg(long)]
    sha256: Option<String>,

    /// A URL whose body's first token is the expected SHA-256 (alternative to
    /// `--sha256`).
    #[arg(long)]
    checksum_url: Option<String>,

    /// An explicit version id for the staged binary (else a digest tag is used).
    #[arg(long)]
    version: Option<String>,

    /// The systemd unit to swap under (fraisier's own service).
    #[arg(long, default_value = "fraisier-webhook.service")]
    unit: String,

    /// Use the user systemd manager (`systemctl --user`).
    #[arg(long)]
    user: bool,

    /// The directory of staged binaries holding the `current` symlink the unit's
    /// `ExecStart` points at.
    #[arg(long, default_value = "/usr/local/lib/fraisier/bin")]
    bin_dir: PathBuf,

    /// The webhook's liveness URL, polled after the restart to confirm the new
    /// binary is serving before the swap is committed.
    #[arg(long, default_value = "http://127.0.0.1:8080/healthz")]
    healthz_url: String,

    /// How many binaries to retain after a healthy commit (incl. the active one).
    #[arg(long, default_value_t = 2)]
    keep: usize,

    /// Seconds to wait for the restarted unit to report healthy before judging it
    /// a failed start (and reverting).
    #[arg(long, default_value_t = 30)]
    health_timeout_secs: u64,

    /// A command run (via `sh -c`) on failure, with the failure payload on stdin
    /// and `FRAISIER_NOTIFY_*` env vars. Omit to log the OTel event only.
    #[arg(long)]
    notify: Option<String>,
}

#[derive(Debug, Args)]
struct ProviderTestArgs {
    /// The provider name: a built-in adapter, or an IPC adapter discovered on
    /// `PATH` (without the `fraisier-adapter-` prefix).
    name: String,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    // Install the OTLP export pipeline before any work so every saga span is
    // exported; the guard is held for the whole process and flushes on drop.
    #[cfg(feature = "otel")]
    let _otel_guard = init_otel();
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
        Command::Init(args) => commands::init(&args.config, args.force),
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
        Command::List(args) => commands::list(&args.state_dir, args.flat).await,
        Command::Health(args) => commands::health(&args.config, args.host.as_deref()).await,
        Command::Rollback(args) => {
            commands::rollback(
                &args.config,
                &args.state_dir,
                args.host.as_deref(),
                &args.to,
                args.app_version.as_deref(),
                args.yes,
            )
            .await
        }
        Command::Status(args) => {
            commands::status(&args.config, &args.state_dir, args.per_host).await
        }
        Command::Bootstrap(args) => {
            commands::bootstrap(&args.config, args.host.as_deref(), args.dry_run).await
        }
        Command::WebhookServer(args) => {
            commands::webhook_server(&args.config, &args.state_dir, args.listen.as_deref()).await
        }
        Command::Backup(args) => {
            commands::db_backup(&args.config, args.output.as_deref(), args.force).await
        }
        Command::Db(DbCommand::Migrate(args)) => {
            commands::db_migrate(&args.config, &args.state_dir).await
        }
        Command::Db(DbCommand::Restore(args)) => {
            commands::db_restore(&args.config, &args.input, args.yes).await
        }
        Command::Db(DbCommand::Reset(args)) => {
            commands::db_reset(&args.config, &args.state_dir, args.yes).await
        }
        Command::Providers => Ok(commands::providers()),
        Command::ProviderTest(args) => commands::provider_test(&args.name).await,
        Command::SelfUpgrade(SelfUpgradeCommand::Restart(args)) => {
            commands::self_upgrade_restart(&args.unit, args.user).await
        }
        Command::SelfUpgrade(SelfUpgradeCommand::Apply(args)) => self_upgrade_apply(args).await,
        Command::Scheduled(ScheduledCommand::Install(args)) => {
            commands::scheduled_install(&args.config, &args.root, args.yes, args.dry_run)
        }
        Command::Scheduled(ScheduledCommand::List(args)) => commands::scheduled_list(&args.root),
        Command::Scheduled(ScheduledCommand::Uninstall(args)) => {
            commands::scheduled_uninstall(&args.config, &args.root, args.yes)
        }
        Command::Sync(args) => {
            commands::sync(
                &args.config,
                &args.state_dir,
                args.pull,
                args.reclaim_orphans,
            )
            .await
        }
        Command::Version(VersionCommand::Show(args)) => commands::version_show(&args.path),
        Command::Version(VersionCommand::Bump { level, project }) => {
            commands::version_bump(&project.path, (*level).into())
        }
        Command::Ship(args) => {
            commands::ship(commands::ShipArgs {
                dir: &args.path,
                level: args.level.into(),
                dry_run: args.dry_run,
                no_deploy: args.no_deploy,
                no_check: args.no_check,
                push: !args.no_push,
                remote: args.remote.clone(),
                config: &args.config,
                state_dir: &args.state_dir,
                host: args.host.as_deref(),
            })
            .await
        }
        Command::Check(args) => commands::check(&args.config, &args.path, args.jobs).await,
        Command::Scaffold(args) => commands::scaffold(&args.config, &args.out, args.dry_run),
        Command::ScaffoldInstall(args) => {
            commands::scaffold_install(&args.config, &args.root, args.prune, args.yes, args.dry_run)
        }
    }
}

/// Translate the clap `self-upgrade apply` args into the command's borrow-based
/// arg struct and run it (kept out of `dispatch` to keep that match compact).
async fn self_upgrade_apply(args: &SelfUpgradeApplyArgs) -> Result<CommandOutput> {
    commands::self_upgrade_apply(commands::SelfUpgradeApplyArgs {
        source: &args.source,
        sha256: args.sha256.as_deref(),
        checksum_url: args.checksum_url.as_deref(),
        version: args.version.as_deref(),
        unit: &args.unit,
        user: args.user,
        bin_dir: &args.bin_dir,
        healthz_url: &args.healthz_url,
        keep: args.keep,
        health_timeout_secs: args.health_timeout_secs,
        notify: args.notify.as_deref(),
    })
    .await
}

/// Install the OTLP span exporter when configured, returning a guard that flushes
/// on drop. Returns `None` (a silent no-op) unless `OTEL_EXPORTER_OTLP_ENDPOINT` /
/// `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` (the standard OTel variables) or
/// `FRAISIER_OTEL` is set, so the `otel`-enabled binary stays quiet until pointed
/// at a collector.
///
/// We pass `None` and let the OTLP exporter resolve the endpoint from the standard
/// environment itself — that way a base `OTEL_EXPORTER_OTLP_ENDPOINT` gets the
/// `/v1/traces` signal path appended per the OTel spec (an explicit override would
/// be used verbatim and silently miss the path).
#[cfg(feature = "otel")]
fn init_otel() -> Option<fraisier_saga::otel::OtelGuard> {
    let configured = std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_some()
        || std::env::var_os("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").is_some()
        || std::env::var_os("FRAISIER_OTEL").is_some();
    if !configured {
        return None;
    }
    match fraisier_saga::otel::install(None) {
        Ok(guard) => Some(guard),
        Err(error) => {
            eprintln!("warning: OpenTelemetry export disabled: {error}");
            None
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
