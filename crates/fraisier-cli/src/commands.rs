//! The command handlers. Each returns a [`CommandOutput`] (an exit code plus
//! pretty and JSON renderings) so the same logic serves both output modes and is
//! testable without spawning the binary.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context as _, Result};
use fraisier_config::{DeployConfig, Severity, ValidationReport};
use fraisier_core::multi_host::MultiHostDeploy;
use fraisier_core::single_host::{DeployRecord, SingleHostDeploy};
use fraisier_saga::saga::SagaOutcome;
use fraisier_saga::state_store::{FilesystemStateStore, FraiseKey, StateStore};
use serde_json::{json, Value};

use crate::factory;

/// The result of a command: an exit code and both renderings of its output.
pub(crate) struct CommandOutput {
    /// The process exit code.
    pub exit_code: i32,
    /// The human-readable rendering.
    pub pretty: String,
    /// The machine-readable rendering (printed under `--json`).
    pub json: Value,
}

fn load(config_path: &Path) -> Result<DeployConfig> {
    let toml = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading config {}", config_path.display()))?;
    DeployConfig::from_toml_str(&toml)
        .with_context(|| format!("parsing config {}", config_path.display()))
}

fn render_issues(report: &ValidationReport) -> String {
    let mut out = report
        .issues
        .iter()
        .map(|issue| {
            let tag = match issue.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
                Severity::Info => "info",
            };
            format!("  [{tag}] {}: {}", issue.path, issue.message)
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// `init`: write the starter [`fraisier.toml`](fraisier_scaffold::starter_config)
/// into the project. Refuses to clobber an existing file unless `force` is set —
/// the safe default for a command a user might run a second time by reflex.
pub(crate) fn init(config_path: &Path, force: bool) -> Result<CommandOutput> {
    if config_path.exists() && !force {
        return Ok(CommandOutput {
            exit_code: 1,
            pretty: format!(
                "{} already exists; pass --force to overwrite it\n",
                config_path.display()
            ),
            json: json!({ "ok": false, "path": config_path.display().to_string(), "wrote": false }),
        });
    }
    std::fs::write(config_path, fraisier_scaffold::starter_config())
        .with_context(|| format!("writing {}", config_path.display()))?;
    Ok(CommandOutput {
        exit_code: 0,
        pretty: format!(
            "wrote starter config to {}\nnext: edit it, then run `fraisier validate-config`\n",
            config_path.display()
        ),
        json: json!({ "ok": true, "path": config_path.display().to_string(), "wrote": true }),
    })
}

/// `version show`: report the project version and the file it came from.
pub(crate) fn version_show(dir: &Path) -> Result<CommandOutput> {
    let info = fraisier_ship::show(dir)?;
    Ok(CommandOutput {
        exit_code: 0,
        pretty: format!("{} ({})\n", info.version, info.path.display()),
        json: json!({
            "version": info.version,
            "path": info.path.display().to_string(),
        }),
    })
}

/// `version bump <level>`: increment the version in place and report old → new.
pub(crate) fn version_bump(dir: &Path, level: fraisier_ship::Bump) -> Result<CommandOutput> {
    let info = fraisier_ship::locate(dir)?;
    let (old, new) = fraisier_ship::bump(dir, level)?;
    Ok(CommandOutput {
        exit_code: 0,
        pretty: format!("bumped {old} → {new} in {}\n", info.path.display()),
        json: json!({
            "old": old,
            "new": new,
            "path": info.path.display().to_string(),
        }),
    })
}

/// Everything `ship` needs: the version bump plus the follow-on deploy inputs.
pub(crate) struct ShipArgs<'a> {
    /// The project directory holding the version file.
    pub dir: &'a Path,
    /// Which component to bump.
    pub level: fraisier_ship::Bump,
    /// Compute the plan without writing/committing/pushing.
    pub dry_run: bool,
    /// Skip the follow-on deploy.
    pub no_deploy: bool,
    /// Push the release commit.
    pub push: bool,
    /// The git remote to push to.
    pub remote: String,
    /// The deploy config (only used when a deploy follows).
    pub config: &'a Path,
    /// The state store directory (only used when a deploy follows).
    pub state_dir: &'a Path,
    /// The single-host override (only used when a deploy follows).
    pub host: Option<&'a str>,
}

/// `ship <level>`: bump → commit → push, then (unless `--no-deploy`) deploy the
/// freshly-bumped version.
pub(crate) async fn ship(args: ShipArgs<'_>) -> Result<CommandOutput> {
    let opts = fraisier_ship::ShipOptions {
        dry_run: args.dry_run,
        no_deploy: args.no_deploy,
        push: args.push,
        remote: args.remote,
        message_template: None,
    };
    let report = fraisier_ship::ship(args.dir, args.level, &opts)?;

    let mut pretty = String::new();
    if report.dry_run {
        let _ = writeln!(
            pretty,
            "ship plan: {} → {} (commit {:?}{})",
            report.old_version,
            report.new_version,
            report.message,
            if report.deploy_requested {
                ", then deploy"
            } else {
                ""
            },
        );
        let _ = writeln!(pretty, "(dry run — nothing was written)");
    } else {
        let _ = writeln!(
            pretty,
            "shipped {} → {} (committed{})",
            report.old_version,
            report.new_version,
            if report.pushed { ", pushed" } else { "" },
        );
    }

    // No deploy requested (or a dry run): report the ship result alone.
    if !report.deploy_requested || report.dry_run {
        return Ok(CommandOutput {
            exit_code: 0,
            pretty,
            json: json!({
                "old_version": report.old_version,
                "new_version": report.new_version,
                "committed": report.committed,
                "pushed": report.pushed,
                "deployed": false,
                "dry_run": report.dry_run,
            }),
        });
    }

    // Deploy the version just shipped; its exit code becomes the command's.
    let deploy_out = deploy(
        args.config,
        args.state_dir,
        args.host,
        Some(&report.new_version),
        false,
    )
    .await?;
    pretty.push_str(&deploy_out.pretty);
    Ok(CommandOutput {
        exit_code: deploy_out.exit_code,
        pretty,
        json: json!({
            "old_version": report.old_version,
            "new_version": report.new_version,
            "committed": report.committed,
            "pushed": report.pushed,
            "deployed": true,
            "deploy": deploy_out.json,
        }),
    })
}

/// `scaffold`: render the deploy infrastructure files and write the tree into
/// `out_dir` (or, with `dry_run`, just list what would be written).
pub(crate) fn scaffold(config_path: &Path, out_dir: &Path, dry_run: bool) -> Result<CommandOutput> {
    let config = load(config_path)?;
    let files = fraisier_scaffold::generate(&config)?;

    if dry_run {
        let mut pretty = format!("scaffold plan (into {}):\n", out_dir.display());
        for file in &files {
            let _ = writeln!(pretty, "  {}", out_dir.join(&file.rel_path).display());
        }
        pretty.push_str("(dry run — nothing was written)\n");
        return Ok(CommandOutput {
            exit_code: 0,
            pretty,
            json: json!({
                "dry_run": true,
                "files": files.iter().map(|f| f.rel_path.display().to_string()).collect::<Vec<_>>(),
            }),
        });
    }

    let written = fraisier_scaffold::write_tree(&files, out_dir)?;
    let mut pretty = format!(
        "wrote {} files into {}:\n",
        written.len(),
        out_dir.display()
    );
    for path in &written {
        let _ = writeln!(pretty, "  {}", path.display());
    }
    Ok(CommandOutput {
        exit_code: 0,
        pretty,
        json: json!({
            "wrote": written.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        }),
    })
}

/// `scaffold-install`: install the system files (under `root`) and optionally
/// prune stale fraisier-generated files. Writes only when `apply` is set;
/// otherwise (and under `dry_run`) it prints the install + prune plans.
pub(crate) fn scaffold_install(
    config_path: &Path,
    root: &Path,
    prune: bool,
    apply: bool,
    dry_run: bool,
) -> Result<CommandOutput> {
    let config = load(config_path)?;
    let files = fraisier_scaffold::generate(&config)?;
    let targets = fraisier_scaffold::install_targets(&files, root);
    let stale = if prune {
        fraisier_scaffold::prune_plan(&files, root)?
    } else {
        Vec::new()
    };

    let render_plan = |verb: &str| {
        let mut pretty = format!("install plan ({verb}, root {}):\n", root.display());
        for path in &targets {
            let _ = writeln!(pretty, "  + {}", path.display());
        }
        if prune {
            pretty.push_str("prune plan:\n");
            if stale.is_empty() {
                pretty.push_str("  (nothing stale)\n");
            }
            for path in &stale {
                let _ = writeln!(pretty, "  - {}", path.display());
            }
        }
        pretty
    };

    // Apply only on an explicit, non-dry-run go-ahead; otherwise show the plan.
    if dry_run || !apply {
        let mut pretty = render_plan("dry run");
        if !dry_run {
            pretty.push_str("pass --yes to apply\n");
        }
        return Ok(CommandOutput {
            exit_code: 0,
            pretty,
            json: json!({
                "applied": false,
                "install": targets.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "prune": stale.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            }),
        });
    }

    let installed = fraisier_scaffold::install(&files, root)?;
    if prune {
        fraisier_scaffold::prune(&stale)?;
    }
    let mut pretty = format!(
        "installed {} files (root {})\n",
        installed.len(),
        root.display()
    );
    if prune {
        let _ = writeln!(pretty, "pruned {} stale files", stale.len());
    }
    Ok(CommandOutput {
        exit_code: 0,
        pretty,
        json: json!({
            "applied": true,
            "installed": installed.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "pruned": stale.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        }),
    })
}

/// `validate-config`: parse, expand, and validate, reporting every located issue.
pub(crate) fn validate_config(config_path: &Path) -> Result<CommandOutput> {
    let toml = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading config {}", config_path.display()))?;
    match DeployConfig::from_toml_str(&toml) {
        Err(error) => Ok(CommandOutput {
            exit_code: 1,
            pretty: format!("config parse error:\n{error}\n"),
            json: json!({ "ok": false, "parse_error": error.to_string() }),
        }),
        Ok(config) => {
            let report = config.validate();
            let ok = report.ok();
            let mut pretty = render_issues(&report);
            pretty.push_str(match (ok, report.issues.is_empty()) {
                (true, true) => "config is valid\n",
                (true, false) => "config is valid (warnings only)\n",
                (false, _) => "config is INVALID\n",
            });
            Ok(CommandOutput {
                exit_code: i32::from(!ok),
                pretty,
                json: json!({ "ok": ok, "issues": serde_json::to_value(&report.issues)? }),
            })
        }
    }
}

/// Discover `fraisier-adapter-*` executables across the directories in `path_var`.
pub(crate) fn discover_adapters(path_var: Option<&OsStr>) -> Vec<String> {
    let mut names = BTreeSet::new();
    let Some(path_var) = path_var else {
        return Vec::new();
    };
    for dir in std::env::split_paths(path_var) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name
                .to_str()
                .and_then(|n| n.strip_prefix("fraisier-adapter-"))
            else {
                continue;
            };
            if !name.is_empty() && is_executable(&entry) {
                names.insert(name.to_owned());
            }
        }
    }
    names.into_iter().collect()
}

fn is_executable(entry: &std::fs::DirEntry) -> bool {
    let Ok(metadata) = entry.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// `adapter list`: the migration adapters discoverable on `PATH`.
pub(crate) fn adapter_list() -> CommandOutput {
    let names = discover_adapters(std::env::var_os("PATH").as_deref());
    let pretty = if names.is_empty() {
        "no fraisier-adapter-* binaries found on PATH\n".to_owned()
    } else {
        let mut listing = names.join("\n");
        listing.push('\n');
        listing
    };
    CommandOutput {
        exit_code: 0,
        pretty,
        json: json!({ "adapters": names }),
    }
}

/// `adapter describe <name>`: spawn the adapter and run the `describe` handshake.
pub(crate) async fn adapter_describe(name: &str) -> Result<CommandOutput> {
    use fraisier_core::adapter_axes::MigrationAdapter as _;

    let program = format!("fraisier-adapter-{name}");
    let adapter = fraisier_ipc::IpcMigrationAdapter::new(&program, name);
    match adapter.describe().await {
        Ok(description) => Ok(CommandOutput {
            exit_code: 0,
            pretty: format!(
                "{} v{} (protocol v{})\n  capabilities: {}\n",
                description.name,
                description.version,
                description.protocol_version,
                description.capabilities.join(", "),
            ),
            json: serde_json::to_value(&description)?,
        }),
        Err(error) => Ok(CommandOutput {
            exit_code: 1,
            pretty: format!("could not describe adapter '{name}' ({program}): {error}\n"),
            json: json!({ "adapter": name, "error": error.to_string() }),
        }),
    }
}

/// `list`: enumerate every deploy recorded in the state store, with its current
/// state (and, unless `flat`, the active artifact + revision from the ledger).
pub(crate) async fn list(state_dir: &Path, flat: bool) -> Result<CommandOutput> {
    let store = FilesystemStateStore::new(state_dir)
        .with_context(|| format!("opening state store at {}", state_dir.display()))?;
    let mut keys = store.keys().await?;
    keys.sort_by_key(ToString::to_string);

    let mut entries = Vec::new();
    let mut pretty = String::new();
    for key in &keys {
        let state = store.current_state(key).await?;
        let ledger = store.current_snapshot(key).await?;
        let state_label = state
            .as_ref()
            .map_or_else(|| "no-state".to_owned(), |s| format!("{:?}", s.state));
        let revision = ledger
            .as_ref()
            .and_then(|l| serde_json::from_value::<DeployRecord>(l.clone()).ok())
            .and_then(|r| r.revision.map(|rev| rev.to_string()));

        if flat {
            let _ = writeln!(pretty, "{key}");
        } else {
            let rev = revision.as_deref().unwrap_or("-");
            let _ = writeln!(pretty, "{key}  {state_label}  (revision {rev})");
        }
        entries.push(json!({
            "fraise": key.fraise(),
            "environment": key.environment(),
            "state": state_label,
            "revision": revision,
        }));
    }
    if entries.is_empty() {
        pretty = format!("no recorded deploys in {}\n", state_dir.display());
    }

    Ok(CommandOutput {
        exit_code: 0,
        pretty,
        json: json!({ "deploys": entries }),
    })
}

/// `health`: probe the configured health endpoint on every host (or just
/// `host_filter`), reporting each result. Exit 0 iff every probed host is healthy.
pub(crate) async fn health(config_path: &Path, host_filter: Option<&str>) -> Result<CommandOutput> {
    let config = load(config_path)?;
    let plan = factory::build_health_probe(&config)?;

    let mut results = Vec::new();
    let mut all_healthy = true;
    let mut probed = 0usize;
    for (host, address) in &plan.hosts {
        if let Some(filter) = host_filter {
            if host.as_str() != filter && address.as_deref() != Some(filter) {
                continue;
            }
        }
        probed += 1;
        let mut ctx = plan.ctx.clone();
        ctx.host = Some(host.clone());
        if let Some(addr) = address {
            ctx.settings
                .insert("address".to_owned(), Value::String(addr.clone()));
        }
        let (healthy, detail) = match plan.health.check(&ctx, host).await {
            Ok(status) => (status.healthy, status.detail),
            // A probe that can't be performed (unreachable, DNS, …) is reported as
            // unhealthy with the error as detail, not a hard command failure.
            Err(error) => (false, Some(error.to_string())),
        };
        all_healthy &= healthy;
        results.push(json!({
            "host": host.as_str(),
            "address": address,
            "healthy": healthy,
            "detail": detail,
        }));
    }

    let mut pretty = String::new();
    for result in &results {
        let mark = if result["healthy"] == json!(true) {
            "ok"
        } else {
            "UNHEALTHY"
        };
        let host = result["host"].as_str().unwrap_or("?");
        let detail = result["detail"]
            .as_str()
            .map_or(String::new(), |d| format!(" — {d}"));
        let _ = writeln!(pretty, "  [{mark}] {host}{detail}");
    }
    if probed == 0 {
        pretty = host_filter.map_or_else(
            || "no hosts to probe\n".to_owned(),
            |f| format!("no host matching '{f}'\n"),
        );
    }

    Ok(CommandOutput {
        exit_code: i32::from(!(all_healthy && probed > 0)),
        pretty,
        json: json!({ "healthy": all_healthy && probed > 0, "hosts": results }),
    })
}

/// `status`: the recorded saga state and release ledger for the config's deploy.
pub(crate) async fn status(config_path: &Path, state_dir: &Path) -> Result<CommandOutput> {
    let config = load(config_path)?;
    let deploy = config.deploy.as_ref().context("missing [deploy] section")?;
    let fraise = deploy.name.clone().context("[deploy].name is required")?;
    let environment = deploy
        .environment
        .clone()
        .context("[deploy].environment is required")?;

    let store = FilesystemStateStore::new(state_dir)
        .with_context(|| format!("opening state store at {}", state_dir.display()))?;
    let key = FraiseKey::new(&fraise, &environment);
    let state = store.current_state(&key).await?;
    let ledger = store.current_snapshot(&key).await?;

    let mut pretty = state.as_ref().map_or_else(
        || format!("{fraise}/{environment}: no recorded state\n"),
        |state| {
            format!(
                "{fraise}/{environment}: {:?} (recorded {})\n",
                state.state, state.recorded_at
            )
        },
    );
    if let Some(ledger) = &ledger {
        if let Ok(record) = serde_json::from_value::<DeployRecord>(ledger.clone()) {
            let active = record
                .active
                .as_ref()
                .map_or_else(|| "<none>".to_owned(), |a| a.artifact.id.clone());
            let revision = record
                .revision
                .as_ref()
                .map_or_else(|| "<none>".to_owned(), ToString::to_string);
            pretty = format!("{pretty}  active artifact: {active}\n  revision: {revision}\n");
        }
    }

    Ok(CommandOutput {
        exit_code: 0,
        pretty,
        json: json!({
            "fraise": fraise,
            "environment": environment,
            "state": serde_json::to_value(&state)?,
            "ledger": ledger.unwrap_or(Value::Null),
        }),
    })
}

fn render_summary(summary: &factory::PlanSummary) -> String {
    let mut lines = vec![
        format!(
            "deploy plan for {}/{} → host {}",
            summary.fraise, summary.environment, summary.host
        ),
        format!("  artifact:  {}", summary.artifact),
        format!("  migration: {}", summary.migration),
        format!("  service:   {}", summary.service),
        format!("  health:    {}", summary.health),
    ];
    if let Some(source) = &summary.database_url_env {
        lines.push(format!("  database_url_env: {source}"));
    }
    lines.push(format!("  settings:  {}", summary.settings_keys.join(", ")));
    lines.push("(dry run — nothing was executed)".to_owned());
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Render only a report's non-blocking issues (warnings/info), in the same shape
/// as [`render_issues`]. Empty when there are none.
fn render_warnings(report: &ValidationReport) -> String {
    let mut out = report
        .warnings()
        .map(|issue| {
            let tag = match issue.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
                Severity::Info => "info",
            };
            format!("  [{tag}] {}: {}", issue.path, issue.message)
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Render a multi-host `--dry-run` plan, with any validation warnings above it.
fn render_multi_host_summary(
    summary: &factory::MultiHostSummary,
    report: &ValidationReport,
) -> String {
    let mut out = render_warnings(report);
    let lines = vec![
        format!(
            "multi-host deploy plan for {}/{}:",
            summary.fraise, summary.environment
        ),
        format!("  strategy:  {}", summary.strategy),
        format!("  transport: {}", summary.transport),
        format!("  hosts:     {}", summary.hosts.join(", ")),
        format!("  artifact:  {}", summary.artifact),
        format!("  migration: {}", summary.migration),
        format!("  service:   {}", summary.service),
        format!("  health:    {}", summary.health),
        format!("  lb:        {}", summary.lb),
        "(dry run — nothing was executed)".to_owned(),
    ];
    out.push_str(&lines.join("\n"));
    out.push('\n');
    out
}

/// `deploy`: validate, then either summarize the plan (`--dry-run`) or run the
/// single-host deploy against the state store.
pub(crate) async fn deploy(
    config_path: &Path,
    state_dir: &Path,
    host: Option<&str>,
    app_version: Option<&str>,
    dry_run: bool,
) -> Result<CommandOutput> {
    let config = load(config_path)?;
    let report = config.validate();
    if !report.ok() {
        let mut pretty = render_issues(&report);
        pretty.push_str("refusing to deploy an invalid config\n");
        return Ok(CommandOutput {
            exit_code: 1,
            pretty,
            json: json!({ "ok": false, "issues": serde_json::to_value(&report.issues)? }),
        });
    }

    // A config with [hosts] runs the multi-host rollout — unless an explicit
    // --host override pins it to a single host (the existing single-host path).
    let multi_host = config.hosts.is_some() && host.is_none();

    if dry_run {
        if multi_host {
            let summary = factory::summarize_multi_host(&config, app_version)?;
            return Ok(CommandOutput {
                exit_code: 0,
                pretty: render_multi_host_summary(&summary, &report),
                json: serde_json::to_value(&summary)?,
            });
        }
        let summary = factory::summarize(&config, host, app_version)?;
        return Ok(CommandOutput {
            exit_code: 0,
            pretty: render_summary(&summary),
            json: serde_json::to_value(&summary)?,
        });
    }

    let store = FilesystemStateStore::new(state_dir)
        .with_context(|| format!("opening state store at {}", state_dir.display()))?;

    if multi_host {
        return run_multi_host(&config, store, app_version, &report).await;
    }

    let resolved = factory::build(&config, host, app_version)?;
    let plan = SingleHostDeploy::builder(
        resolved.fraise.clone(),
        resolved.environment.clone(),
        resolved.host.clone(),
    )
    .context(resolved.ctx)
    .forward_compatible_lint(resolved.forward_compatible_lint)
    .artifact(resolved.artifact)
    .migration(resolved.migration)
    .service(resolved.service)
    .health(resolved.health)
    .build()?;

    let outcome = plan.run(store).await?;
    let (exit_code, label, detail) = outcome_result(&outcome);
    Ok(CommandOutput {
        exit_code,
        pretty: format!(
            "deploy of {}/{} {label}{detail}\n",
            resolved.fraise, resolved.environment
        ),
        json: json!({ "outcome": label, "detail": detail.trim() }),
    })
}

/// Build and run the multi-host rollout, mapping its outcome onto a
/// [`CommandOutput`]. Any validation warnings (e.g. the artifact-locality caveat)
/// are surfaced above the result so the operator sees them on a live deploy.
async fn run_multi_host(
    config: &DeployConfig,
    store: FilesystemStateStore,
    app_version: Option<&str>,
    report: &ValidationReport,
) -> Result<CommandOutput> {
    let resolved = factory::build_multi_host(config, app_version)?;
    let deploy = MultiHostDeploy::builder(
        resolved.fraise.clone(),
        resolved.environment.clone(),
        resolved.plan,
    )
    .context(resolved.ctx)
    .forward_compatible_lint(resolved.forward_compatible_lint)
    .artifact(resolved.artifact)
    .migration(resolved.migration)
    .service(resolved.service)
    .health(resolved.health)
    .lb(resolved.lb)
    .build()?;

    let outcome = deploy.run(store).await?;
    let (exit_code, label, detail) = outcome_result(&outcome);
    let mut pretty = render_warnings(report);
    let _ = writeln!(
        pretty,
        "multi-host deploy of {}/{} {label}{detail}",
        resolved.fraise, resolved.environment
    );
    Ok(CommandOutput {
        exit_code,
        pretty,
        json: json!({ "outcome": label, "detail": detail.trim(), "multi_host": true }),
    })
}

/// Map a saga outcome onto `(exit code, label, detail)` for both deploy paths.
fn outcome_result(outcome: &SagaOutcome) -> (i32, &'static str, String) {
    match outcome {
        SagaOutcome::Committed => (0, "committed", String::new()),
        SagaOutcome::RolledBack {
            failed_step,
            reason,
        } => (
            1,
            "rolled_back",
            format!(" (step '{failed_step}': {reason})"),
        ),
        SagaOutcome::PartialRollback { reason } => (2, "partial_rollback", format!(" ({reason})")),
        _ => (1, "unknown", String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        adapter_list, deploy, discover_adapters, health, init, list, scaffold, scaffold_install,
        ship, status, validate_config, version_bump, version_show, ShipArgs,
    };
    use fraisier_core::single_host::DeployRecord;
    use fraisier_saga::events::SagaState;
    use fraisier_saga::state_store::{
        DeploymentState, FilesystemStateStore, FraiseKey, StateStore,
    };
    use std::path::Path;

    const VALID: &str = r#"
[deploy]
name = "checkout"
environment = "staging"

[artifact]
source = "release"
release_url = "https://example.com/checkout-{version}.tar.gz"
checksum_url = "https://example.com/checkout-{version}.tar.gz.sha256"

[migration]
adapter = "confiture"
database_url_env = "CHECKOUT_DATABASE_URL"

[service]
adapter = "systemd"
unit = "checkout.service"

[health]
adapter = "http"
url = "http://127.0.0.1:8080/health"
"#;

    fn write(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write file");
        path
    }

    #[test]
    fn validate_config_accepts_a_valid_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), "fraisier.toml", VALID);
        let out = validate_config(&path).expect("run");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert_eq!(out.json["ok"], serde_json::json!(true));
    }

    #[test]
    fn validate_config_rejects_and_locates_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        // confiture without database_url_env.
        let bad = VALID.replace("database_url_env = \"CHECKOUT_DATABASE_URL\"\n", "");
        let path = write(dir.path(), "fraisier.toml", &bad);
        let out = validate_config(&path).expect("run");
        assert_eq!(out.exit_code, 1);
        let issues = out.json["issues"].as_array().expect("issues array");
        assert!(issues
            .iter()
            .any(|i| i["path"] == "migration.database_url_env"));
    }

    #[test]
    fn init_writes_starter_then_guards_against_overwrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fraisier.toml");

        // Absent → writes the starter and reports success.
        let out = init(&path, false).expect("init writes");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert!(path.exists(), "the file was created");
        let written = std::fs::read_to_string(&path).expect("read back");
        assert!(written.contains("[deploy]"), "starter content written");

        // Present, no force → refuses, leaves the file untouched.
        std::fs::write(&path, "EDITED").expect("simulate user edit");
        let out = init(&path, false).expect("init guards");
        assert_eq!(out.exit_code, 1);
        assert_eq!(out.json["wrote"], serde_json::json!(false));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "EDITED",
            "the guard must not clobber the existing file"
        );

        // Present, force → overwrites.
        let out = init(&path, true).expect("init --force");
        assert_eq!(out.exit_code, 0);
        assert!(std::fs::read_to_string(&path)
            .expect("read back")
            .contains("[deploy]"));
    }

    #[test]
    fn version_show_and_bump_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.5\"\n",
        )
        .expect("write");

        let out = version_show(dir.path()).expect("show");
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.json["version"], serde_json::json!("0.1.5"));

        let out = version_bump(dir.path(), fraisier_ship::Bump::Patch).expect("bump");
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.json["old"], serde_json::json!("0.1.5"));
        assert_eq!(out.json["new"], serde_json::json!("0.1.6"));

        // The bump persisted.
        let out = version_show(dir.path()).expect("show after bump");
        assert_eq!(out.json["version"], serde_json::json!("0.1.6"));
    }

    #[tokio::test]
    async fn ship_dry_run_reports_the_plan_without_deploying() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.5\"\n",
        )
        .expect("write");
        let out = ship(ShipArgs {
            dir: dir.path(),
            level: fraisier_ship::Bump::Patch,
            dry_run: true,
            no_deploy: true,
            push: false,
            remote: "origin".to_owned(),
            config: Path::new("fraisier.toml"),
            state_dir: Path::new(".fraisier/state"),
            host: None,
        })
        .await
        .expect("ship dry run");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert_eq!(out.json["new_version"], serde_json::json!("0.1.6"));
        assert_eq!(out.json["deployed"], serde_json::json!(false));
        // Untouched on disk.
        assert!(std::fs::read_to_string(dir.path().join("Cargo.toml"))
            .unwrap()
            .contains("0.1.5"));
    }

    #[test]
    fn scaffold_writes_a_tree_and_dry_run_lists_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", VALID);
        let out = dir.path().join("deploy");

        // Dry run lists files, writes nothing.
        let plan = scaffold(&config, &out, true).expect("dry run");
        assert_eq!(plan.exit_code, 0);
        assert_eq!(plan.json["files"].as_array().expect("files").len(), 5);
        assert!(!out.exists(), "dry run wrote nothing");

        // Real run writes the tree.
        let done = scaffold(&config, &out, false).expect("scaffold");
        assert_eq!(done.exit_code, 0);
        assert!(out.join("systemd/checkout.service").exists());
    }

    #[test]
    fn scaffold_install_plans_then_applies_under_a_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", VALID);
        let root = dir.path().join("root");

        // Without --yes it only plans (apply = false).
        let plan = scaffold_install(&config, &root, false, false, false).expect("plan");
        assert_eq!(plan.json["applied"], serde_json::json!(false));
        assert!(!root.exists(), "planning wrote nothing");

        // With apply it installs under the root.
        let done = scaffold_install(&config, &root, false, true, false).expect("install");
        assert_eq!(done.json["applied"], serde_json::json!(true));
        assert!(root.join("etc/systemd/system/checkout.service").exists());
    }

    #[test]
    fn discover_adapters_finds_prefixed_executables() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("fraisier-adapter-demo");
        std::fs::write(&bin, "#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        // A non-adapter file is ignored.
        std::fs::write(dir.path().join("other-tool"), "x").expect("write");

        let found = discover_adapters(Some(dir.path().as_os_str()));
        assert_eq!(found, vec!["demo".to_owned()]);
    }

    #[test]
    fn adapter_list_runs_without_error() {
        // Whatever is on PATH, the command must produce valid output.
        let out = adapter_list();
        assert_eq!(out.exit_code, 0);
        assert!(out.json.get("adapters").is_some());
    }

    #[tokio::test]
    async fn deploy_dry_run_resolves_the_plan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", VALID);
        let state = dir.path().join("state");
        let out = deploy(&config, &state, Some("127.0.0.1"), Some("1.2.3"), true)
            .await
            .expect("dry run");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert_eq!(
            out.json["migration"],
            serde_json::json!("confiture (in-process)")
        );
        assert_eq!(out.json["host"], serde_json::json!("127.0.0.1"));
    }

    #[tokio::test]
    async fn deploy_refuses_an_invalid_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bad = VALID.replace("database_url_env = \"CHECKOUT_DATABASE_URL\"\n", "");
        let config = write(dir.path(), "fraisier.toml", &bad);
        let state = dir.path().join("state");
        let out = deploy(&config, &state, None, None, false)
            .await
            .expect("run");
        assert_eq!(out.exit_code, 1);
        assert_eq!(out.json["ok"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn list_enumerates_recorded_deploys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_dir = dir.path().join("state");
        let store = FilesystemStateStore::new(&state_dir).expect("store");
        for (fraise, env) in [("checkout", "staging"), ("billing", "production")] {
            store
                .record_state(
                    &FraiseKey::new(fraise, env),
                    &DeploymentState::new(SagaState::Committed, Some("r1".to_owned())),
                )
                .await
                .expect("record");
        }

        let out = list(&state_dir, false).await.expect("list");
        assert_eq!(out.exit_code, 0);
        let deploys = out.json["deploys"].as_array().expect("deploys");
        assert_eq!(deploys.len(), 2, "both recorded deploys: {}", out.pretty);
        assert!(out.pretty.contains("billing/production"));
        assert!(out.pretty.contains("checkout/staging"));
    }

    #[tokio::test]
    async fn health_reports_an_unreachable_host_as_unhealthy() {
        // Port 1 refuses immediately, so the probe fails fast (no hung DNS).
        let cfg = VALID.replace(
            "url = \"http://127.0.0.1:8080/health\"",
            "url = \"http://127.0.0.1:1/health\"",
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", &cfg);
        let out = health(&config, None).await.expect("health");
        assert_eq!(out.exit_code, 1, "unreachable → non-zero: {}", out.pretty);
        assert_eq!(out.json["healthy"], serde_json::json!(false));
        assert_eq!(out.json["hosts"].as_array().expect("hosts").len(), 1);
    }

    #[tokio::test]
    async fn health_host_filter_selecting_nothing_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", VALID);
        let out = health(&config, Some("no-such-host")).await.expect("health");
        assert!(out.pretty.contains("no host matching"), "{}", out.pretty);
    }

    #[tokio::test]
    async fn list_reports_an_empty_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = list(&dir.path().join("state"), false).await.expect("list");
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.json["deploys"].as_array().expect("deploys").len(), 0);
    }

    #[tokio::test]
    async fn status_reports_recorded_state_and_ledger() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", VALID);
        let state_dir = dir.path().join("state");
        let store = FilesystemStateStore::new(&state_dir).expect("store");
        let key = FraiseKey::new("checkout", "staging");
        store
            .record_state(
                &key,
                &DeploymentState::new(SagaState::Committed, Some("rev-7".to_owned())),
            )
            .await
            .expect("record state");
        let ledger = DeployRecord {
            active: None,
            revision: Some(fraisier_core::adapter_axes::Revision::new("rev-7")),
        };
        store
            .record_snapshot(&key, &serde_json::to_value(&ledger).expect("encode"))
            .await
            .expect("record ledger");

        let out = status(&config, &state_dir).await.expect("status");
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.json["fraise"], serde_json::json!("checkout"));
        assert!(out.pretty.contains("Committed"), "pretty: {}", out.pretty);
    }

    const MULTI_HOST: &str = r#"
[deploy]
name = "checkout"
environment = "production"

[hosts]
strategy = "rolling"
rolling_batch_size = 1
inventory = [
  { name = "web-1", address = "web1.internal" },
  { name = "web-2", address = "web2.internal" },
]

[ssh]
user = "deploy"
options = ["StrictHostKeyChecking=no"]

[artifact]
source = "release"
release_url = "https://example.com/checkout-{version}.tar.gz"
checksum_url = "https://example.com/checkout-{version}.tar.gz.sha256"

[migration]
adapter = "confiture"
database_url_env = "CHECKOUT_DATABASE_URL"

[service]
adapter = "systemd"
unit = "checkout.service"

[health]
adapter = "http"
url = "http://127.0.0.1:8080/health"

[lb]
adapter = "nginx"
config_path = "/etc/nginx/sites-available/checkout"
upstream = "checkout_upstream"
"#;

    #[tokio::test]
    async fn deploy_dry_run_renders_the_multi_host_plan_and_warnings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", MULTI_HOST);
        let state = dir.path().join("state");
        // No --host override: [hosts] selects the multi-host path.
        let out = deploy(&config, &state, None, Some("1.2.3"), true)
            .await
            .expect("multi-host dry run");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert_eq!(out.json["strategy"], serde_json::json!("rolling(1)"));
        assert_eq!(out.json["lb"], serde_json::json!("nginx"));
        assert_eq!(out.json["hosts"].as_array().expect("hosts").len(), 2);
        assert!(
            out.json["transport"]
                .as_str()
                .unwrap_or_default()
                .contains("ssh"),
            "transport: {}",
            out.json["transport"]
        );
        // The artifact-locality caveat is surfaced to the operator.
        assert!(
            out.pretty.contains("stages on the host running fraisier"),
            "warning shown: {}",
            out.pretty
        );
    }

    #[tokio::test]
    async fn deploy_with_host_override_takes_the_single_host_path() {
        // --host pins even a [hosts] config to a single host (the summary has a
        // `host` field; the multi-host summary does not).
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", MULTI_HOST);
        let state = dir.path().join("state");
        let out = deploy(&config, &state, Some("web1.internal"), Some("1.2.3"), true)
            .await
            .expect("single-host dry run");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert_eq!(out.json["host"], serde_json::json!("web1.internal"));
    }

    #[tokio::test]
    async fn deploy_dry_run_host_pull_artifact_has_no_locality_warning() {
        // source = "pull" stages on each host, so the artifact-locality warning
        // must NOT fire (only release/local trigger it).
        let dir = tempfile::tempdir().expect("tempdir");
        let pull = MULTI_HOST.replace("source = \"release\"", "source = \"pull\"");
        let config = write(dir.path(), "fraisier.toml", &pull);
        let state = dir.path().join("state");
        let out = deploy(&config, &state, None, Some("1.2.3"), true)
            .await
            .expect("multi-host pull dry run");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert_eq!(out.json["artifact"], serde_json::json!("pull"));
        assert!(
            !out.pretty.contains("stages on the host running fraisier"),
            "host-pull must not warn about artifact locality: {}",
            out.pretty
        );
    }
}
