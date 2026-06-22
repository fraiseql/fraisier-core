//! The command handlers. Each returns a [`CommandOutput`] (an exit code plus
//! pretty and JSON renderings) so the same logic serves both output modes and is
//! testable without spawning the binary.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

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
// Reason: these mirror the independent ship CLI flags (dry-run / no-deploy /
// no-check / push); a flat arg bundle, not a state machine.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct ShipArgs<'a> {
    /// The project directory holding the version file.
    pub dir: &'a Path,
    /// Which component to bump (ignored when `no_bump` is set).
    pub level: fraisier_ship::Bump,
    /// Re-ship the current version without bumping (mutually exclusive with a
    /// bump level — the CLI rejects the combination at parse time).
    pub no_bump: bool,
    /// Compute the plan without writing/committing/pushing.
    pub dry_run: bool,
    /// Skip the follow-on deploy.
    pub no_deploy: bool,
    /// Skip the pre-bump `[[checks]]` gate.
    pub no_check: bool,
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
    // Pre-bump check gate (default on; `--no-check` skips; a dry run reports the
    // checks would run but does not execute them). Runs before any bump/commit,
    // so a gate failure leaves the version file and git history untouched.
    let checks_json = if args.dry_run {
        json!({ "would_run": count_checks(args.config) })
    } else if args.no_check {
        json!({ "ran": false, "reason": "skipped (--no-check)" })
    } else {
        match ship_check_gate(args.dir, args.config).await? {
            ShipGateOutcome::Abort(output) => return Ok(output),
            ShipGateOutcome::Proceed(value) => value,
        }
    };

    let opts = fraisier_ship::ShipOptions {
        dry_run: args.dry_run,
        no_deploy: args.no_deploy,
        push: args.push,
        no_bump: args.no_bump,
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
                "race_detected": report.race_detected,
                "checks": checks_json,
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
            "race_detected": report.race_detected,
            "checks": checks_json,
        }),
    })
}

/// The decision the pre-bump check gate hands back to [`ship`].
enum ShipGateOutcome {
    /// Checks failed (or were invalid): do not ship; return this output as-is.
    Abort(CommandOutput),
    /// Checks passed (or none are configured): proceed, embedding this JSON.
    Proceed(Value),
}

/// Run the configured `[[checks]]` before a ship, from `config_path`, in
/// `base_dir`. A missing config file means "no checks" (a pure version bump
/// without a `fraisier.toml` still ships). A present-but-invalid checks section,
/// or any failing check, aborts the ship.
async fn ship_check_gate(base_dir: &Path, config_path: &Path) -> Result<ShipGateOutcome> {
    if !config_path.exists() {
        return Ok(ShipGateOutcome::Proceed(
            json!({ "ran": false, "reason": "no config" }),
        ));
    }
    let config = load(config_path)?;
    let report = config.validate_checks_only();
    if !report.ok() {
        return Ok(ShipGateOutcome::Abort(CommandOutput {
            exit_code: 1,
            pretty: format!(
                "{}refusing to ship: invalid checks\n",
                render_issues(&report)
            ),
            json: json!({
                "shipped": false,
                "checks": { "ran": true, "ok": false, "issues": serde_json::to_value(&report.issues)? },
            }),
        }));
    }
    if config.checks.is_empty() {
        return Ok(ShipGateOutcome::Proceed(
            json!({ "ran": false, "reason": "no checks configured" }),
        ));
    }
    let checks = build_checks(&config, config_path);
    let run = fraisier_check::run(&checks, resolve_jobs(0), base_dir).await;
    if run.ok() {
        let mut value = check_report_json(&run);
        value["ran"] = json!(true);
        Ok(ShipGateOutcome::Proceed(value))
    } else {
        Ok(ShipGateOutcome::Abort(CommandOutput {
            exit_code: 1,
            pretty: format!(
                "{}refusing to ship: checks failed\n",
                render_check_report(&run)
            ),
            json: json!({ "shipped": false, "checks": check_report_json(&run) }),
        }))
    }
}

/// How many `[[checks]]` a config declares (0 if the file is absent or
/// unparseable — used only to preview a dry-run plan).
fn count_checks(config_path: &Path) -> usize {
    if !config_path.exists() {
        return 0;
    }
    load(config_path).map_or(0, |config| config.checks.len())
}

/// `check`: run the project's `[[checks]]` with cross-check parallelism. Exits 0
/// iff every check passes. Captured output is shown for failing checks (and
/// always under `--json`); it is never streamed.
pub(crate) async fn check(config_path: &Path, base: &Path, jobs: usize) -> Result<CommandOutput> {
    let config = load(config_path)?;
    let report = config.validate_checks_only();
    if !report.ok() {
        return Ok(CommandOutput {
            exit_code: 1,
            pretty: format!("{}refusing to run invalid checks\n", render_issues(&report)),
            json: json!({ "ok": false, "issues": serde_json::to_value(&report.issues)? }),
        });
    }
    if config.checks.is_empty() {
        return Ok(CommandOutput {
            exit_code: 0,
            pretty: "no [[checks]] configured\n".to_owned(),
            json: json!({ "ok": true, "checks": [], "total_ms": 0 }),
        });
    }
    let checks = build_checks(&config, config_path);
    let run = fraisier_check::run(&checks, resolve_jobs(jobs), base).await;
    Ok(CommandOutput {
        exit_code: i32::from(!run.ok()),
        pretty: render_check_report(&run),
        json: check_report_json(&run),
    })
}

/// Build runnable checks from a parsed config, resolving each `workdir` against
/// the config file's directory (absolute workdirs pass through unchanged).
fn build_checks(config: &DeployConfig, config_path: &Path) -> Vec<fraisier_check::Check> {
    let config_dir = config_path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    config
        .checks
        .iter()
        .map(|check| {
            let workdir = check.workdir.as_ref().map(|dir| {
                if dir.is_absolute() {
                    dir.clone()
                } else {
                    config_dir.join(dir)
                }
            });
            fraisier_check::Check {
                name: check.name.clone().unwrap_or_default(),
                command: check.command.clone().unwrap_or_default(),
                workdir,
            }
        })
        .collect()
}

/// Resolve the requested job count: `0` means auto (logical CPUs, at least 1).
fn resolve_jobs(jobs: usize) -> usize {
    if jobs == 0 {
        std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
    } else {
        jobs
    }
}

/// Milliseconds of a duration, saturating rather than overflowing `u64`.
fn duration_ms(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The human-readable check report: one line per check (captured output shown
/// for failures), then a pass/total summary.
fn render_check_report(run: &fraisier_check::CheckRunReport) -> String {
    use fraisier_check::CheckStatus;
    let mut out = String::new();
    for outcome in &run.outcomes {
        let tag = match outcome.status {
            CheckStatus::Passed => "ok",
            CheckStatus::Failed => "FAIL",
            CheckStatus::SpawnError => "ERROR",
        };
        let ms = duration_ms(outcome.duration);
        let _ = writeln!(out, "  [{tag}] {} ({ms} ms)", outcome.name);
        if !outcome.passed() {
            for line in outcome.stdout.lines().chain(outcome.stderr.lines()) {
                let _ = writeln!(out, "      {line}");
            }
        }
    }
    let total = run.outcomes.len();
    let passed = total - run.failed_count();
    let _ = writeln!(out, "{passed}/{total} checks passed");
    out
}

/// The machine-readable check report (the frozen `check` JSON shape).
fn check_report_json(run: &fraisier_check::CheckRunReport) -> Value {
    use fraisier_check::CheckStatus;
    let checks: Vec<Value> = run
        .outcomes
        .iter()
        .map(|outcome| {
            let status = match outcome.status {
                CheckStatus::Passed => "passed",
                CheckStatus::Failed => "failed",
                CheckStatus::SpawnError => "spawn_error",
            };
            json!({
                "name": outcome.name,
                "status": status,
                "code": outcome.code,
                "duration_ms": duration_ms(outcome.duration),
                "stdout": outcome.stdout,
                "stderr": outcome.stderr,
            })
        })
        .collect();
    json!({
        "ok": run.ok(),
        "total_ms": duration_ms(run.total_duration),
        "checks": checks,
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

/// `scheduled install`: generate the systemd timer + service that run fraisier on
/// a calendar schedule and install them under `root`. Unlike `scaffold-install`
/// it does **not** prune: scheduled units share the systemd directory (and the
/// fraisier marker) with the scaffold units, so a scoped-by-presence prune would
/// wrongly remove the other set. Writes only on `apply`; otherwise shows the plan.
pub(crate) fn scheduled_install(
    config_path: &Path,
    root: &Path,
    apply: bool,
    dry_run: bool,
) -> Result<CommandOutput> {
    let config = load(config_path)?;

    // The unattended-deploy gate: refuse to install a schedule whose [schedule]
    // section is invalid — in particular `command = "deploy"` without
    // `allow_unattended_deploy = true` + a notify sink. The validator carries the
    // policy; here we refuse (with the located reasons) rather than install.
    let report = config.validate();
    let schedule_errors: Vec<&fraisier_config::ValidationIssue> = report
        .issues
        .iter()
        .filter(|issue| issue.severity == Severity::Error && issue.path.starts_with("schedule"))
        .collect();
    if !schedule_errors.is_empty() {
        let mut pretty = String::from("refusing to install: invalid [schedule] config\n");
        for issue in &schedule_errors {
            let _ = writeln!(pretty, "  [error] {}: {}", issue.path, issue.message);
        }
        return Ok(CommandOutput {
            exit_code: 1,
            pretty,
            json: json!({
                "applied": false,
                "refused": true,
                "errors": schedule_errors
                    .iter()
                    .map(|i| json!({ "path": i.path, "message": i.message }))
                    .collect::<Vec<_>>(),
            }),
        });
    }

    let files = fraisier_scaffold::generate_scheduled(&config)?;
    let targets = fraisier_scaffold::install_targets(&files, root);

    if dry_run || !apply {
        let mut pretty = format!("scheduled install plan (root {}):\n", root.display());
        for path in &targets {
            let _ = writeln!(pretty, "  + {}", path.display());
        }
        if !dry_run {
            pretty.push_str("pass --yes to apply\n");
        }
        pretty.push_str("then: systemctl daemon-reload && systemctl enable --now <timer>\n");
        return Ok(CommandOutput {
            exit_code: 0,
            pretty,
            json: json!({
                "applied": false,
                "install": targets.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            }),
        });
    }

    let installed = fraisier_scaffold::install(&files, root)?;
    let mut pretty = format!(
        "installed {} scheduled unit(s) (root {})\n",
        installed.len(),
        root.display()
    );
    pretty.push_str("next: systemctl daemon-reload && systemctl enable --now <timer>\n");
    Ok(CommandOutput {
        exit_code: 0,
        pretty,
        json: json!({
            "applied": true,
            "installed": installed.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        }),
    })
}

/// `scheduled list`: enumerate the fraisier-installed (marker-bearing) systemd
/// units under `root` — no silent accretion of host state.
pub(crate) fn scheduled_list(root: &Path) -> Result<CommandOutput> {
    let units = fraisier_scaffold::list_installed(root)?;
    let mut pretty = format!("fraisier-installed units (root {}):\n", root.display());
    if units.is_empty() {
        pretty.push_str("  (none)\n");
    }
    for unit in &units {
        let _ = writeln!(pretty, "  {}", unit.display());
    }
    Ok(CommandOutput {
        exit_code: 0,
        pretty,
        json: json!({
            "units": units.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        }),
    })
}

/// `scheduled uninstall`: remove exactly the timer + service this config
/// installed (marker-checked), leaving the systemd directory clean. Writes only
/// on `apply`; otherwise shows the plan.
pub(crate) fn scheduled_uninstall(
    config_path: &Path,
    root: &Path,
    apply: bool,
) -> Result<CommandOutput> {
    let config = load(config_path)?;
    let files = fraisier_scaffold::generate_scheduled(&config)?;
    let targets = fraisier_scaffold::install_targets(&files, root);

    if !apply {
        let mut pretty = format!("scheduled uninstall plan (root {}):\n", root.display());
        for path in &targets {
            let _ = writeln!(pretty, "  - {}", path.display());
        }
        pretty.push_str("pass --yes to remove\n");
        return Ok(CommandOutput {
            exit_code: 0,
            pretty,
            json: json!({
                "removed": false,
                "plan": targets.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            }),
        });
    }

    let removed = fraisier_scaffold::uninstall(&files, root)?;
    let pretty = format!(
        "removed {} scheduled unit(s) (root {})\nthen: systemctl daemon-reload\n",
        removed.len(),
        root.display()
    );
    Ok(CommandOutput {
        exit_code: 0,
        pretty,
        json: json!({
            "removed": true,
            "files": removed.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
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

/// The in-process adapters compiled into this binary, per axis. Kept in step
/// with the factory's `build_*` match arms (the source of truth for what a
/// config can actually select).
const BUILTIN_PROVIDERS: &[(&str, &[&str])] = &[
    (
        "artifact",
        &["release", "pull", "release-ipc", "local", "git"],
    ),
    ("migration", &["confiture", "command"]),
    ("service", &["systemd", "rc", "docker-compose"]),
    ("health", &["http"]),
    ("lb", &["nginx"]),
];

/// Whether `name` is a built-in (compiled-in) provider on any axis.
fn builtin_axis(name: &str) -> Option<&'static str> {
    BUILTIN_PROVIDERS
        .iter()
        .find(|(_, names)| names.contains(&name))
        .map(|(axis, _)| *axis)
}

/// `providers`: list every adapter available to this binary, per axis — the
/// compiled-in ones and the `fraisier-adapter-*` IPC adapters discovered on
/// `PATH` (migration axis). A diagnostic for "which adapter will my config find?".
pub(crate) fn providers() -> CommandOutput {
    let discovered = discover_adapters(std::env::var_os("PATH").as_deref());
    let mut pretty = String::new();
    let mut axes = Vec::new();
    for (axis, names) in BUILTIN_PROVIDERS {
        let _ = writeln!(pretty, "{axis}:");
        let mut entries = Vec::new();
        for name in *names {
            let _ = writeln!(pretty, "  {name} (built-in)");
            entries.push(json!({ "name": name, "source": "built-in" }));
        }
        // The migration axis is the only one the IPC protocol extends.
        if *axis == "migration" {
            for name in &discovered {
                let _ = writeln!(pretty, "  {name} (IPC: fraisier-adapter-{name})");
                entries.push(json!({ "name": name, "source": "ipc" }));
            }
        }
        axes.push(json!({ "axis": axis, "providers": entries }));
    }
    CommandOutput {
        exit_code: 0,
        pretty,
        json: json!({ "axes": axes }),
    }
}

/// `provider-test <name>`: probe one provider. For a PATH-discovered IPC adapter
/// this runs the real `describe` handshake (the genuine pre-deploy check — a bad
/// protocol version or a crash-on-handshake is caught here). For a compiled-in
/// adapter it just confirms presence: there is nothing to handshake, the code is
/// in this binary.
pub(crate) async fn provider_test(name: &str) -> Result<CommandOutput> {
    use fraisier_core::adapter_axes::MigrationAdapter as _;

    let discovered = discover_adapters(std::env::var_os("PATH").as_deref());
    if discovered.iter().any(|d| d == name) {
        let program = format!("fraisier-adapter-{name}");
        let adapter = fraisier_ipc::IpcMigrationAdapter::new(&program, name);
        return Ok(match adapter.describe().await {
            Ok(description) => CommandOutput {
                exit_code: 0,
                pretty: format!(
                    "{} v{} (IPC, protocol v{})\n  capabilities: {}\n",
                    description.name,
                    description.version,
                    description.protocol_version,
                    description.capabilities.join(", "),
                ),
                json: json!({ "ok": true, "source": "ipc", "describe": serde_json::to_value(&description)? }),
            },
            Err(error) => CommandOutput {
                exit_code: 1,
                pretty: format!("IPC adapter '{name}' ({program}) failed its handshake: {error}\n"),
                json: json!({ "ok": false, "source": "ipc", "error": error.to_string() }),
            },
        });
    }

    if let Some(axis) = builtin_axis(name) {
        return Ok(CommandOutput {
            exit_code: 0,
            pretty: format!(
                "{name}: built-in {axis} adapter (compiled in; nothing to handshake)\n"
            ),
            json: json!({ "ok": true, "source": "built-in", "axis": axis }),
        });
    }

    Ok(CommandOutput {
        exit_code: 1,
        pretty: format!(
            "unknown provider '{name}' (not a built-in adapter, and no fraisier-adapter-{name} on PATH)\n"
        ),
        json: json!({ "ok": false, "error": "unknown provider" }),
    })
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

/// `rollback`: revert the deploy to a prior `to` revision. Shows the plan
/// (`source → target`) unless `execute` is set, then runs a deploy of the
/// rolled-back-to artifact with the migration taken `down_to(target)`.
pub(crate) async fn rollback(
    config_path: &Path,
    state_dir: &Path,
    host: Option<&str>,
    to: &str,
    app_version: Option<&str>,
    execute: bool,
) -> Result<CommandOutput> {
    use fraisier_core::adapter_axes::Revision;

    let config = load(config_path)?;
    let report = config.validate();
    if !report.ok() {
        let mut pretty = render_issues(&report);
        pretty.push_str("refusing to roll back with an invalid config\n");
        return Ok(CommandOutput {
            exit_code: 1,
            pretty,
            json: json!({ "ok": false, "issues": serde_json::to_value(&report.issues)? }),
        });
    }

    let deploy = config.deploy.as_ref().context("missing [deploy] section")?;
    let fraise = deploy.name.clone().context("[deploy].name is required")?;
    let environment = deploy
        .environment
        .clone()
        .context("[deploy].environment is required")?;

    let store = FilesystemStateStore::new(state_dir)
        .with_context(|| format!("opening state store at {}", state_dir.display()))?;
    let key = FraiseKey::new(&fraise, &environment);
    let source = store
        .current_snapshot(&key)
        .await?
        .and_then(|v| serde_json::from_value::<DeployRecord>(v).ok())
        .and_then(|r| r.revision)
        .map_or_else(|| "<unknown>".to_owned(), |r| r.to_string());

    if !execute {
        return Ok(CommandOutput {
            exit_code: 0,
            pretty: format!(
                "rollback plan for {fraise}/{environment}: {source} → {to}\n\
                 pass --yes to execute (migrates the database down to {to})\n"
            ),
            json: json!({
                "fraise": fraise, "environment": environment,
                "source": source, "target": to, "executed": false,
            }),
        });
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
    .rollback_to(Revision::new(to))
    .build()?;

    let outcome = plan.run(store).await?;
    let (exit_code, label, detail) = outcome_result(&outcome);
    Ok(CommandOutput {
        exit_code,
        pretty: format!("rollback of {fraise}/{environment} to {to} {label}{detail}\n"),
        json: json!({
            "fraise": fraise, "environment": environment,
            "source": source, "target": to, "executed": true,
            "outcome": label, "detail": detail.trim(),
        }),
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
/// With `per_host`, also query each host's *live* active artifact.
pub(crate) async fn status(
    config_path: &Path,
    state_dir: &Path,
    per_host: bool,
) -> Result<CommandOutput> {
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

    let per_host_json = if per_host {
        let hosts = per_host_active(&config).await?;
        pretty.push_str("  per-host active artifact:\n");
        for entry in &hosts {
            let host = entry["host"].as_str().unwrap_or("?");
            let active = entry["active"].as_str().unwrap_or("<none>");
            let _ = writeln!(pretty, "    {host}: {active}");
        }
        Value::Array(hosts)
    } else {
        Value::Null
    };

    Ok(CommandOutput {
        exit_code: 0,
        pretty,
        json: json!({
            "fraise": fraise,
            "environment": environment,
            "state": serde_json::to_value(&state)?,
            "ledger": ledger.unwrap_or(Value::Null),
            "per_host": per_host_json,
        }),
    })
}

/// Query each host's live active artifact via the artifact adapter's `current`.
async fn per_host_active(config: &DeployConfig) -> Result<Vec<Value>> {
    let plan = factory::build_artifact_probe(config)?;
    let mut entries = Vec::new();
    for (host, address) in &plan.hosts {
        let mut ctx = plan.ctx.clone();
        ctx.host = Some(host.clone());
        if let Some(addr) = address {
            ctx.settings
                .insert("address".to_owned(), Value::String(addr.clone()));
        }
        let (active, error) = match plan.artifact.current(&ctx, host).await {
            Ok(current) => (current.map(|c| c.id), None),
            Err(error) => (None, Some(error.to_string())),
        };
        entries.push(json!({
            "host": host.as_str(),
            "address": address,
            "active": active,
            "error": error,
        }));
    }
    Ok(entries)
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
    skip_preflight: bool,
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

    // `[deploy].strategy = "blue-green"` selects the HTTP-tier traffic-swap flow.
    if config.deploy.as_ref().and_then(|d| d.strategy.as_deref()) == Some("blue-green") {
        return run_blue_green(&config, app_version, state_dir, dry_run).await;
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
    // `--skip-preflight` is the per-run escape hatch: it forces every preflight off
    // regardless of `[migration].preflight_mode`.
    let mut builder = SingleHostDeploy::builder(
        resolved.fraise.clone(),
        resolved.environment.clone(),
        resolved.host.clone(),
    )
    .context(resolved.ctx)
    .forward_compatible_lint(resolved.forward_compatible_lint && !skip_preflight)
    .artifact(resolved.artifact)
    .migration(resolved.migration)
    .service(resolved.service)
    .health(resolved.health);
    if !skip_preflight {
        if let Some((db, backup)) = resolved.restore_rehearsal {
            builder = builder.restore_rehearsal(db, backup);
        }
    }
    let plan = builder.build()?;

    let outcome = plan.run(store).await?;
    notify_deploy_failure(&config, &resolved.fraise, &resolved.environment, &outcome).await;
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
    notify_deploy_failure(config, &resolved.fraise, &resolved.environment, &outcome).await;
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

/// Run (or, with `dry_run`, summarize) an HTTP-tier blue-green deploy. On a
/// non-committed outcome the `[schedule].notify` failure sink fires (unattended
/// path), exactly as the rolling/single deploys do.
async fn run_blue_green(
    config: &DeployConfig,
    app_version: Option<&str>,
    state_dir: &Path,
    dry_run: bool,
) -> Result<CommandOutput> {
    let resolved = factory::build_blue_green(config, app_version)?;
    if dry_run {
        let pretty = format!(
            "blue-green deploy plan for {}/{} (dry run — nothing executed)\n",
            resolved.fraise, resolved.environment
        );
        return Ok(CommandOutput {
            exit_code: 0,
            pretty,
            json: json!({
                "strategy": "blue-green",
                "fraise": resolved.fraise,
                "environment": resolved.environment,
                "dry_run": true,
            }),
        });
    }

    let store = FilesystemStateStore::new(state_dir)
        .with_context(|| format!("opening state store at {}", state_dir.display()))?;
    let outcome = resolved.deploy.run(store).await?;
    notify_deploy_failure(config, &resolved.fraise, &resolved.environment, &outcome).await;
    let (exit_code, label, detail) = outcome_result(&outcome);
    Ok(CommandOutput {
        exit_code,
        pretty: format!(
            "blue-green deploy of {}/{} {label}{detail}\n",
            resolved.fraise, resolved.environment
        ),
        json: json!({ "strategy": "blue-green", "outcome": label, "detail": detail.trim() }),
    })
}

/// Refuse a database operation whose `[deploy]`/`[migration]` config is invalid,
/// returning the rendered issues. `Ok(None)` means the config is acceptable.
fn refuse_invalid_db_config(config: &DeployConfig, verb: &str) -> Option<CommandOutput> {
    let report = config.validate_db_ops();
    if report.ok() {
        return None;
    }
    let mut pretty = render_issues(&report);
    let _ = writeln!(pretty, "refusing to {verb} with an invalid config");
    Some(CommandOutput {
        exit_code: 1,
        pretty,
        json: json!({ "ok": false, "issues": serde_json::to_value(&report.issues).unwrap_or(Value::Null) }),
    })
}

/// `db migrate`: apply pending migrations through the configured migration
/// adapter — the deploy's migrate phase on its own (no artifact/service/health).
///
/// It runs the same forward-compatibility `preflight` gate the deploy uses
/// (when the adapter advertises it and `forward_compatible_lint` is on), applies
/// all pending migrations (`up(None)`), and records the resulting revision in the
/// state ledger so `status`/`list` stay accurate.
pub(crate) async fn db_migrate(config_path: &Path, state_dir: &Path) -> Result<CommandOutput> {
    use fraisier_core::adapter_axes::Severity;

    let config = load(config_path)?;
    if let Some(refusal) = refuse_invalid_db_config(&config, "migrate") {
        return Ok(refusal);
    }

    let resolved = factory::build_migration_only(&config)?;
    let adapter = resolved.migration.as_ref();

    // Forward-compat preflight, gated exactly as the deploy flow gates it: skip
    // entirely on opt-out, describe to learn capabilities, only call preflight if
    // advertised, and block on Error-severity findings.
    if resolved.forward_compatible_lint {
        let described = adapter
            .describe()
            .await
            .context("describing the migration adapter")?;
        if described.capabilities.iter().any(|c| c == "preflight") {
            let report = adapter
                .preflight(&resolved.ctx)
                .await
                .context("running the forward-compatibility preflight")?;
            let blocking: Vec<_> = report
                .issues
                .iter()
                .filter(|issue| issue.severity == Severity::Error)
                .collect();
            if !blocking.is_empty() {
                let mut pretty = String::new();
                for issue in &blocking {
                    let _ = writeln!(pretty, "  [error] {}: {}", issue.code, issue.message);
                }
                let _ = writeln!(
                    pretty,
                    "refusing to migrate: {} blocking forward-compatibility issue(s)",
                    blocking.len()
                );
                return Ok(CommandOutput {
                    exit_code: 1,
                    pretty,
                    json: json!({ "ok": false, "preflight_blocking": blocking.len() }),
                });
            }
        }
    }

    let outcome = adapter
        .up(&resolved.ctx, None)
        .await
        .context("applying migrations")?;
    let applied: Vec<String> = outcome.applied.iter().map(ToString::to_string).collect();
    let to = outcome.to.as_ref().map(ToString::to_string);

    // Record the new current revision in the ledger, preserving the active
    // artifact a prior deploy recorded.
    let current = adapter.current_revision(&resolved.ctx).await.ok().flatten();
    record_ledger_revision(state_dir, &resolved.fraise, &resolved.environment, current).await?;

    let mut pretty = format!(
        "migrated {}/{}: {} migration(s) applied",
        resolved.fraise,
        resolved.environment,
        applied.len()
    );
    if let Some(to) = &to {
        let _ = write!(pretty, ", now at {to}");
    }
    pretty.push('\n');
    Ok(CommandOutput {
        exit_code: 0,
        pretty,
        json: json!({
            "ok": true,
            "fraise": resolved.fraise,
            "environment": resolved.environment,
            "applied": applied,
            "count": applied.len(),
            "to": to,
        }),
    })
}

/// Record `revision` as the current revision in the state ledger for
/// `fraise`/`environment`, preserving the active artifact a prior deploy recorded.
/// Used by the migration-state changing db ops (`db migrate`, `db reset`).
async fn record_ledger_revision(
    state_dir: &Path,
    fraise: &str,
    environment: &str,
    revision: Option<fraisier_core::adapter_axes::Revision>,
) -> Result<()> {
    let store = FilesystemStateStore::new(state_dir)
        .with_context(|| format!("opening state store at {}", state_dir.display()))?;
    let key = FraiseKey::new(fraise, environment);
    let mut record = store
        .current_snapshot(&key)
        .await?
        .and_then(|v| serde_json::from_value::<DeployRecord>(v).ok())
        .unwrap_or(DeployRecord {
            active: None,
            revision: None,
        });
    record.revision = revision;
    store
        .record_snapshot(&key, &serde_json::to_value(&record)?)
        .await?;
    Ok(())
}

/// Resolve the Postgres connection for a generic database op from the configured
/// DSN env var (`[migration].database_url_env`). The DSN value is read here and
/// decomposed into `PG*` env on the child by the command builders — it never
/// reaches argv.
///
/// # Errors
/// If `database_url_env` is unset in the config, the named env var is not set in
/// the environment, or the DSN is not a parseable `postgres://` URL.
fn resolve_pg_conn(config: &DeployConfig) -> Result<fraisier_db::PgConn> {
    let source = config
        .migration
        .as_ref()
        .and_then(|m| m.database_url_env.clone())
        .context(
            "db backup/restore/reset need [migration].database_url_env \
             (the name of the env var holding the database DSN)",
        )?;
    let dsn = std::env::var(&source)
        .with_context(|| format!("the configured database_url_env '{source}' is not set"))?;
    fraisier_db::PgConn::parse(&dsn).context("parsing the database DSN")
}

/// A clean exit-1 [`CommandOutput`] for an expected db-op configuration error
/// (missing/unset DSN, non-postgres DSN), so `--json` still renders.
fn db_config_error(error: &anyhow::Error) -> CommandOutput {
    let message = format!("{error:#}");
    CommandOutput {
        exit_code: 1,
        pretty: format!("{message}\n"),
        json: json!({ "ok": false, "error": message }),
    }
}

/// `backup`: dump the database to a custom-format archive with `pg_dump -Fc`.
///
/// Non-destructive, but refuses to clobber an existing archive unless `force` is
/// set. The output path defaults to `<fraise>-<environment>.pgdump`.
pub(crate) async fn db_backup(
    config_path: &Path,
    output: Option<&Path>,
    force: bool,
) -> Result<CommandOutput> {
    let config = load(config_path)?;
    if let Some(refusal) = refuse_invalid_db_config(&config, "back up") {
        return Ok(refusal);
    }
    let conn = match resolve_pg_conn(&config) {
        Ok(conn) => conn,
        Err(error) => return Ok(db_config_error(&error)),
    };

    let deploy = config.deploy.as_ref().context("[deploy] section")?;
    let fraise = deploy.name.clone().unwrap_or_default();
    let environment = deploy.environment.clone().unwrap_or_default();
    let default_path = PathBuf::from(format!("{fraise}-{environment}.pgdump"));
    let out_path: &Path = output.unwrap_or(&default_path);

    if out_path.exists() && !force {
        return Ok(CommandOutput {
            exit_code: 1,
            pretty: format!(
                "{} already exists; pass --force to overwrite it\n",
                out_path.display()
            ),
            json: json!({ "ok": false, "wrote": false, "output": out_path.display().to_string() }),
        });
    }

    let outcome = fraisier_db::run(fraisier_db::backup_command(&conn, out_path))
        .await
        .context("running pg_dump")?;
    if outcome.succeeded() {
        Ok(CommandOutput {
            exit_code: 0,
            pretty: format!("backed up {} → {}\n", conn.redacted(), out_path.display()),
            json: json!({ "ok": true, "output": out_path.display().to_string() }),
        })
    } else {
        Ok(CommandOutput {
            exit_code: 1,
            pretty: format!("pg_dump failed:\n{}\n", outcome.stderr.trim()),
            json: json!({ "ok": false, "stderr": outcome.stderr.trim() }),
        })
    }
}

/// `db restore`: restore a `pg_dump -Fc` archive into the database with
/// `pg_restore`, dropping existing objects first (`--clean --if-exists`).
///
/// Destructive: it overwrites the target database, so it only runs with
/// `execute` (the `--yes` flag); otherwise it prints the plan.
pub(crate) async fn db_restore(
    config_path: &Path,
    input: &Path,
    execute: bool,
) -> Result<CommandOutput> {
    let config = load(config_path)?;
    if let Some(refusal) = refuse_invalid_db_config(&config, "restore") {
        return Ok(refusal);
    }
    let conn = match resolve_pg_conn(&config) {
        Ok(conn) => conn,
        Err(error) => return Ok(db_config_error(&error)),
    };

    if !execute {
        return Ok(CommandOutput {
            exit_code: 0,
            pretty: format!(
                "restore plan: {} → {} (drops existing objects, then restores)\n\
                 pass --yes to execute — this OVERWRITES the database\n",
                input.display(),
                conn.redacted(),
            ),
            json: json!({
                "executed": false,
                "input": input.display().to_string(),
                "target": conn.redacted(),
            }),
        });
    }

    if !input.exists() {
        return Ok(CommandOutput {
            exit_code: 1,
            pretty: format!("backup file {} not found\n", input.display()),
            json: json!({ "ok": false, "input": input.display().to_string() }),
        });
    }

    let outcome = fraisier_db::run(fraisier_db::restore_command(&conn, input, true))
        .await
        .context("running pg_restore")?;
    let (exit_code, ok) = (i32::from(!outcome.succeeded()), outcome.succeeded());
    let mut pretty = if ok {
        format!("restored {} → {}\n", input.display(), conn.redacted())
    } else {
        format!("pg_restore failed:\n{}\n", outcome.stderr.trim())
    };
    if ok && !outcome.stderr.trim().is_empty() {
        // pg_restore can emit non-fatal warnings on stderr while still exiting 0.
        let _ = writeln!(pretty, "(warnings)\n{}", outcome.stderr.trim());
    }
    Ok(CommandOutput {
        exit_code,
        pretty,
        json: json!({ "ok": ok, "executed": true, "input": input.display().to_string() }),
    })
}

/// `db reset`: drop every user schema and re-apply migrations from scratch.
///
/// Destructive: it wipes the database, so it only runs with `execute` (the
/// `--yes` flag); otherwise it prints the plan. The wipe is generic Postgres
/// (`psql` running [`fraisier_db::RESET_SQL`]); the re-apply goes through the
/// configured migration adapter.
pub(crate) async fn db_reset(
    config_path: &Path,
    state_dir: &Path,
    execute: bool,
) -> Result<CommandOutput> {
    let config = load(config_path)?;
    if let Some(refusal) = refuse_invalid_db_config(&config, "reset") {
        return Ok(refusal);
    }
    let conn = match resolve_pg_conn(&config) {
        Ok(conn) => conn,
        Err(error) => return Ok(db_config_error(&error)),
    };

    let deploy = config.deploy.as_ref().context("[deploy] section")?;
    let fraise = deploy.name.clone().unwrap_or_default();
    let environment = deploy.environment.clone().unwrap_or_default();

    if !execute {
        return Ok(CommandOutput {
            exit_code: 0,
            pretty: format!(
                "reset plan for {fraise}/{environment}: DROP ALL user schemas in {}, \
                 then re-apply migrations\n\
                 pass --yes to execute — this DESTROYS all data in the database\n",
                conn.redacted(),
            ),
            json: json!({
                "executed": false,
                "fraise": fraise,
                "environment": environment,
                "target": conn.redacted(),
            }),
        });
    }

    // 1. Drop every user schema (generic Postgres).
    let dropped = fraisier_db::run(fraisier_db::psql_command(&conn, fraisier_db::RESET_SQL))
        .await
        .context("running psql to drop schemas")?;
    if !dropped.succeeded() {
        return Ok(CommandOutput {
            exit_code: 1,
            pretty: format!("schema drop failed:\n{}\n", dropped.stderr.trim()),
            json: json!({ "ok": false, "stage": "drop", "stderr": dropped.stderr.trim() }),
        });
    }

    // 2. Re-apply migrations through the configured adapter.
    let resolved = factory::build_migration_only(&config)?;
    let adapter = resolved.migration.as_ref();
    let outcome = adapter
        .up(&resolved.ctx, None)
        .await
        .context("re-applying migrations after reset")?;
    let applied = outcome.applied.len();

    // 3. Record the resulting revision in the ledger.
    let current = adapter.current_revision(&resolved.ctx).await.ok().flatten();
    record_ledger_revision(state_dir, &fraise, &environment, current).await?;

    Ok(CommandOutput {
        exit_code: 0,
        pretty: format!(
            "reset {fraise}/{environment}: dropped all schemas, re-applied {applied} migration(s)\n"
        ),
        json: json!({
            "ok": true,
            "executed": true,
            "fraise": fraise,
            "environment": environment,
            "applied": applied,
        }),
    })
}

/// `bootstrap`: prepare each target host's deploy directories over the transport
/// (`Local` single-host, `Ssh` per host). With `dry_run`, only the plan is shown.
pub(crate) async fn bootstrap(
    config_path: &Path,
    host_filter: Option<&str>,
    dry_run: bool,
) -> Result<CommandOutput> {
    let config = load(config_path)?;
    let plan = factory::build_bootstrap(&config);

    if plan.dirs.is_empty() {
        return Ok(CommandOutput {
            exit_code: 0,
            pretty:
                "nothing to prepare: set [artifact].staging_dir and/or [artifact].active_path\n"
                    .to_owned(),
            json: json!({ "ok": true, "prepared": 0, "dirs": [] }),
        });
    }

    let mut results = Vec::new();
    let mut all_ok = true;
    let mut prepared = 0usize;
    for (host, address) in &plan.hosts {
        if let Some(filter) = host_filter {
            if host.as_str() != filter && address.as_deref() != Some(filter) {
                continue;
            }
        }
        prepared += 1;
        if dry_run {
            results.push(json!({ "host": host.as_str(), "address": address }));
            continue;
        }
        let mut ctx = plan.ctx.clone();
        ctx.host = Some(host.clone());
        if let Some(addr) = address {
            ctx.settings
                .insert("address".to_owned(), Value::String(addr.clone()));
        }
        match fraisier_bootstrap::ensure_dirs(&plan.transport, &ctx, &plan.dirs).await {
            Ok(()) => results.push(json!({ "host": host.as_str(), "ok": true })),
            Err(error) => {
                all_ok = false;
                results.push(
                    json!({ "host": host.as_str(), "ok": false, "error": error.to_string() }),
                );
            }
        }
    }

    if prepared == 0 {
        let pretty = host_filter.map_or_else(
            || "no hosts to bootstrap\n".to_owned(),
            |f| format!("no host matching '{f}'\n"),
        );
        return Ok(CommandOutput {
            exit_code: 1,
            pretty,
            json: json!({ "ok": false, "prepared": 0 }),
        });
    }

    let mut pretty = String::new();
    if dry_run {
        let hosts: Vec<&str> = results.iter().filter_map(|r| r["host"].as_str()).collect();
        let _ = writeln!(
            pretty,
            "bootstrap plan ({} host(s): {}): mkdir -p",
            prepared,
            hosts.join(", ")
        );
        for dir in &plan.dirs {
            let _ = writeln!(pretty, "  {dir}");
        }
        pretty.push_str("(dry run — nothing was created)\n");
    } else {
        for result in &results {
            let mark = if result["ok"] == json!(true) {
                "ok"
            } else {
                "FAILED"
            };
            let host = result["host"].as_str().unwrap_or("?");
            let detail = result["error"]
                .as_str()
                .map_or(String::new(), |e| format!(" — {e}"));
            let _ = writeln!(pretty, "  [{mark}] {host}{detail}");
        }
    }

    Ok(CommandOutput {
        exit_code: i32::from(!all_ok),
        pretty,
        json: json!({
            "ok": all_ok,
            "prepared": prepared,
            "dirs": plan.dirs,
            "hosts": results,
            "dry_run": dry_run,
        }),
    })
}

/// A webhook handler that runs a deploy when a verified request arrives. An
/// optional `{"version": "..."}` in the (signed) body becomes the deploy's app
/// version.
struct DeployHandler {
    config: PathBuf,
    state_dir: PathBuf,
}

#[async_trait::async_trait]
impl fraisier_webhook::WebhookHandler for DeployHandler {
    async fn handle(&self, body: &[u8]) -> Result<String, String> {
        let version = serde_json::from_slice::<Value>(body).ok().and_then(|v| {
            v.get("version")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        });
        let out = deploy(
            &self.config,
            &self.state_dir,
            None,
            version.as_deref(),
            false,
            false,
        )
        .await
        .map_err(|error| format!("{error:#}"))?;
        let outcome = out
            .json
            .get("outcome")
            .and_then(Value::as_str)
            .unwrap_or("done")
            .to_owned();
        if out.exit_code == 0 {
            Ok(format!("deploy {outcome}"))
        } else {
            Err(format!("deploy {outcome}"))
        }
    }
}

/// `webhook-server`: run the signed-POST deploy trigger server. Blocks, serving
/// requests over systemd socket activation (when present) or a standalone bind,
/// until the listener fails. Refuses to start on an invalid config.
pub(crate) async fn webhook_server(
    config_path: &Path,
    state_dir: &Path,
    listen_override: Option<&str>,
) -> Result<CommandOutput> {
    let config = load(config_path)?;
    let report = config.validate();
    if !report.ok() {
        let mut pretty = render_issues(&report);
        pretty.push_str("refusing to start the webhook server with an invalid config\n");
        return Ok(CommandOutput {
            exit_code: 1,
            pretty,
            json: json!({ "ok": false, "issues": serde_json::to_value(&report.issues)? }),
        });
    }

    let webhook = config
        .webhook
        .as_ref()
        .context("missing [webhook] section")?;
    let secret_env = webhook
        .secret_env
        .as_deref()
        .context("[webhook].secret_env is required")?;
    let secret = std::env::var(secret_env).with_context(|| {
        format!("the configured [webhook].secret_env '{secret_env}' is not set in the environment")
    })?;
    let listen = listen_override
        .map(ToOwned::to_owned)
        .or_else(|| webhook.listen.clone())
        .unwrap_or_else(|| "127.0.0.1:9000".to_owned());

    let server_config = fraisier_webhook::ServerConfig {
        secret: secret.into_bytes(),
        tolerance_secs: webhook.tolerance_secs.unwrap_or(300),
        max_body_bytes: webhook.max_body_bytes.unwrap_or(1024 * 1024),
        read_timeout: std::time::Duration::from_secs(webhook.read_timeout_secs.unwrap_or(30)),
    };
    let (listener, source) = fraisier_webhook::acquire(&listen)
        .await
        .context("acquiring the webhook listener")?;
    let handler = DeployHandler {
        config: config_path.to_owned(),
        state_dir: state_dir.to_owned(),
    };
    eprintln!(
        "fraisier webhook-server listening on {source} (deploys {})",
        config_path.display()
    );
    fraisier_webhook::serve(listener, &server_config, &handler)
        .await
        .context("serving webhook requests")?;

    // `serve` only returns on a listener error (handled above by `?`).
    Ok(CommandOutput {
        exit_code: 0,
        pretty: "webhook server stopped\n".to_owned(),
        json: json!({ "ok": true }),
    })
}

/// `sync` (experimental): share the deploy ledger across operators over git
/// refs. By default it **pushes** each local fraise/env's state to the remote
/// (a non-fast-forward — another operator's concurrent change — is reported as a
/// conflict, never force-pushed). `--pull` fetches remote state into the local
/// store (accepting the remote); `--reclaim-orphans` deletes remote refs with no
/// local counterpart.
pub(crate) async fn sync(
    config_path: &Path,
    state_dir: &Path,
    pull: bool,
    reclaim_orphans: bool,
) -> Result<CommandOutput> {
    eprintln!(
        "warning: `fraisier sync` is experimental; the on-ref state format may change before GA"
    );
    let config = load(config_path)?;
    let section = config.sync.as_ref().context("missing [sync] section")?;
    let remote = section
        .remote
        .as_deref()
        .context("[sync].remote is required")?;
    let sync_dir = section
        .sync_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(".fraisier/sync.git"));

    let store = FilesystemStateStore::new(state_dir)
        .with_context(|| format!("opening state store at {}", state_dir.display()))?;

    if pull {
        return sync_pull(&store, &sync_dir, remote).await;
    }
    if reclaim_orphans {
        return sync_reclaim(&store, remote).await;
    }
    sync_push(&store, &sync_dir, remote).await
}

/// The ref key for a fraise/env: `<fraise>/<env>` (a valid two-component ref).
fn sync_key(key: &FraiseKey) -> String {
    format!("{}/{}", key.fraise(), key.environment())
}

/// Push every local deploy's state to the remote ledger.
async fn sync_push(
    store: &FilesystemStateStore,
    sync_dir: &Path,
    remote: &str,
) -> Result<CommandOutput> {
    use fraisier_sync::PushOutcome;

    let keys = store.keys().await?;
    let mut results = Vec::new();
    let mut pretty = String::new();
    let mut conflicts = 0;
    for key in &keys {
        let payload = serde_json::to_string(&json!({
            "state": store.current_state(key).await?,
            "ledger": store.current_snapshot(key).await?,
        }))?;
        let key_str = sync_key(key);
        let outcome = fraisier_sync::push_state(sync_dir, remote, &key_str, &payload)
            .with_context(|| format!("pushing {key_str}"))?;
        match outcome {
            PushOutcome::Pushed => {
                let _ = writeln!(pretty, "  [pushed] {key_str}");
                results.push(json!({ "key": key_str, "outcome": "pushed" }));
            }
            PushOutcome::UpToDate => {
                let _ = writeln!(pretty, "  [up-to-date] {key_str}");
                results.push(json!({ "key": key_str, "outcome": "up-to-date" }));
            }
            PushOutcome::Conflict {
                local_head,
                remote_head,
            } => {
                conflicts += 1;
                // Show the divergence loudly: local (rejected) vs the remote tip.
                let _ = writeln!(pretty, "  [CONFLICT] {key_str} — the remote diverged");
                let _ = writeln!(pretty, "      local  {local_head}");
                let _ = writeln!(pretty, "      remote {remote_head}");
                results.push(json!({
                    "key": key_str,
                    "outcome": "conflict",
                    "local_head": local_head,
                    "remote_head": remote_head,
                }));
            }
        }
    }

    if keys.is_empty() {
        pretty.push_str("no local deploys to sync\n");
    }
    if conflicts > 0 {
        let _ = writeln!(
            pretty,
            "{conflicts} conflict(s): run `fraisier sync --pull` to fetch the remote, review the \
             divergence, then retry"
        );
    }
    Ok(CommandOutput {
        exit_code: i32::from(conflicts > 0),
        pretty,
        json: json!({ "ok": conflicts == 0, "pushed": results }),
    })
}

/// Pull remote ledger state into the local store (accepting the remote).
async fn sync_pull(
    store: &FilesystemStateStore,
    sync_dir: &Path,
    remote: &str,
) -> Result<CommandOutput> {
    use fraisier_saga::state_store::DeploymentState;

    let keys = fraisier_sync::remote_keys(remote).context("listing remote sync refs")?;
    let mut pulled = Vec::new();
    for key_str in &keys {
        let Some((fraise, environment)) = key_str.split_once('/') else {
            continue; // malformed ref key
        };
        let Some(payload) = fraisier_sync::pull_state(sync_dir, remote, key_str)
            .with_context(|| format!("pulling {key_str}"))?
        else {
            continue;
        };
        let value: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
        let key = FraiseKey::new(fraise, environment);
        if let Some(state) = value.get("state") {
            if let Ok(state) = serde_json::from_value::<DeploymentState>(state.clone()) {
                store.record_state(&key, &state).await?;
            }
        }
        if let Some(ledger) = value.get("ledger") {
            if !ledger.is_null() {
                store.record_snapshot(&key, ledger).await?;
            }
        }
        pulled.push(key_str.clone());
    }

    let mut pretty = String::new();
    for key in &pulled {
        let _ = writeln!(pretty, "  [pulled] {key}");
    }
    if pulled.is_empty() {
        pretty.push_str("no remote state to pull\n");
    }
    Ok(CommandOutput {
        exit_code: 0,
        pretty,
        json: json!({ "ok": true, "pulled": pulled }),
    })
}

/// Delete remote sync refs that have no local deploy (orphan reclaim).
async fn sync_reclaim(store: &FilesystemStateStore, remote: &str) -> Result<CommandOutput> {
    let local: BTreeSet<String> = store.keys().await?.iter().map(sync_key).collect();
    let remote_keys = fraisier_sync::remote_keys(remote).context("listing remote sync refs")?;
    let mut reclaimed = Vec::new();
    for key in &remote_keys {
        if !local.contains(key) {
            fraisier_sync::delete_remote(remote, key)
                .with_context(|| format!("deleting orphan {key}"))?;
            reclaimed.push(key.clone());
        }
    }

    let mut pretty = String::new();
    for key in &reclaimed {
        let _ = writeln!(pretty, "  [reclaimed] {key}");
    }
    if reclaimed.is_empty() {
        pretty.push_str("no orphan refs to reclaim\n");
    }
    Ok(CommandOutput {
        exit_code: 0,
        pretty,
        json: json!({ "ok": true, "reclaimed": reclaimed }),
    })
}

/// Build the `systemctl [--user] restart <unit>` command (kept separate so its
/// construction is unit-testable without a systemd manager present).
fn systemctl_restart_command(unit: &str, user: bool) -> std::process::Command {
    let mut command = std::process::Command::new("systemctl");
    if user {
        command.arg("--user");
    }
    command.arg("restart").arg(unit);
    command
}

/// `self-upgrade restart`: a coordinated restart of fraisier's own long-running
/// unit (the webhook server) via systemd, so an externally-updated binary takes
/// effect. The restart is graceful: on `SIGTERM` the server drains its in-flight
/// request before exiting (see `fraisier_webhook::serve`), then systemd starts
/// the new binary. Binary fetch/verify/swap is a separate, still-to-come
/// `self-upgrade apply` (it needs keep-old + post-restart health-check + revert).
pub(crate) async fn self_upgrade_restart(unit: &str, user: bool) -> Result<CommandOutput> {
    let output = tokio::process::Command::from(systemctl_restart_command(unit, user))
        .output()
        .await
        .with_context(|| format!("running systemctl restart {unit}"))?;
    if output.status.success() {
        Ok(CommandOutput {
            exit_code: 0,
            pretty: format!("restarted {unit} (coordinated)\n"),
            json: json!({ "ok": true, "unit": unit }),
        })
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Ok(CommandOutput {
            exit_code: 1,
            pretty: format!("systemctl restart {unit} failed:\n{}\n", stderr.trim()),
            json: json!({ "ok": false, "unit": unit, "stderr": stderr.trim() }),
        })
    }
}

/// Arguments for [`self_upgrade_apply`].
pub(crate) struct SelfUpgradeApplyArgs<'a> {
    pub source: &'a str,
    pub sha256: Option<&'a str>,
    pub checksum_url: Option<&'a str>,
    pub version: Option<&'a str>,
    pub unit: &'a str,
    pub user: bool,
    pub bin_dir: &'a Path,
    pub healthz_url: &'a str,
    pub keep: usize,
    pub health_timeout_secs: u64,
    pub notify: Option<&'a str>,
}

/// Classify a `<source>` argument into a self-upgrade [`Source`]: an `http(s)://`
/// URL downloads, anything else is a local path.
///
/// [`Source`]: fraisier_self_upgrade::Source
fn classify_source(
    source: &str,
    sha256: Option<&str>,
    checksum_url: Option<&str>,
) -> fraisier_self_upgrade::Source {
    if source.starts_with("http://") || source.starts_with("https://") {
        fraisier_self_upgrade::Source::Url {
            url: source.to_owned(),
            sha256: sha256.map(str::to_owned),
            checksum_url: checksum_url.map(str::to_owned),
        }
    } else {
        fraisier_self_upgrade::Source::Path {
            path: PathBuf::from(source),
            sha256: sha256.map(str::to_owned),
        }
    }
}

/// Map an apply outcome to `(exit code, machine label)`. `Committed` → 0;
/// `Reverted`/`AbortedBeforeSwap` → 1; `ManualIntervention` → 2 (the terminal
/// state, mirroring the saga's `PartialRollback`).
const fn apply_exit(outcome: &fraisier_self_upgrade::ApplyOutcome) -> (i32, &'static str) {
    use fraisier_self_upgrade::ApplyOutcome;
    match outcome {
        ApplyOutcome::Committed { .. } => (0, "committed"),
        ApplyOutcome::Reverted { .. } => (1, "reverted"),
        ApplyOutcome::ManualIntervention { .. } => (2, "manual_intervention"),
        ApplyOutcome::AbortedBeforeSwap { .. } => (1, "aborted"),
    }
}

/// `self-upgrade apply`: fetch + verify + swap fraisier's own binary, restart the
/// unit, health-check it, and auto-revert to the kept-old binary on failure. The
/// post-swap restart **is** the coordinated `self-upgrade restart` (graceful
/// SIGTERM drain) — apply composes that coordination at the tail of its flow, and
/// drives everything out-of-process (systemctl + HTTP), never exec-ing the binary
/// it just swapped.
pub(crate) async fn self_upgrade_apply(args: SelfUpgradeApplyArgs<'_>) -> Result<CommandOutput> {
    use fraisier_self_upgrade::{
        apply, ApplyOutcome, ExecHookNotifier, HttpHealth, Layout, Notifier, Plan,
        SystemctlSupervisor, TracingNotifier,
    };
    use std::time::Duration;

    let plan = Plan {
        source: classify_source(args.source, args.sha256, args.checksum_url),
        layout: Layout::new(args.bin_dir),
        version: args.version.map(str::to_owned),
        keep: args.keep,
        systemd_available: fraisier_self_upgrade::systemd_available(args.user),
        health_timeout: Duration::from_secs(args.health_timeout_secs),
        poll_interval: Duration::from_millis(500),
    };
    let supervisor = SystemctlSupervisor::new(args.unit, args.user);
    let health = HttpHealth::new(
        args.healthz_url,
        Duration::from_secs(args.health_timeout_secs),
    );
    let notifier: Box<dyn Notifier> = match args.notify {
        Some(command) => {
            Box::new(ExecHookNotifier::new(command).with_context("FRAISIER_NOTIFY_UNIT", args.unit))
        }
        None => Box::new(TracingNotifier),
    };

    let outcome = apply(&plan, &supervisor, &health, notifier.as_ref()).await;
    let (exit_code, _label) = apply_exit(&outcome);
    let (pretty, json) = match &outcome {
        ApplyOutcome::Committed { id, pruned } => (
            format!(
                "self-upgrade committed: now running {id} ({pruned} stale binary(ies) pruned)\n"
            ),
            json!({ "result": "committed", "id": id, "pruned": pruned }),
        ),
        ApplyOutcome::Reverted {
            failed,
            restored,
            reason,
        } => (
            format!("self-upgrade REVERTED to {restored}: {reason}\n"),
            json!({ "result": "reverted", "failed": failed, "restored": restored, "reason": reason }),
        ),
        ApplyOutcome::ManualIntervention { reason } => (
            format!("self-upgrade MANUAL INTERVENTION REQUIRED: {reason}\n"),
            json!({ "result": "manual_intervention", "reason": reason }),
        ),
        ApplyOutcome::AbortedBeforeSwap { reason } => (
            format!("self-upgrade aborted (no swap performed): {reason}\n"),
            json!({ "result": "aborted", "reason": reason }),
        ),
    };
    Ok(CommandOutput {
        exit_code,
        pretty,
        json,
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

/// Fire the configured `[schedule].notify` failure sink when a deploy did not
/// commit. This is the unattended-failure path: a scheduled (operator-unwatched)
/// deploy of a sick build rolls back **and** emits a notification, reusing the
/// self-upgrade notify primitive. A committed deploy, or a config with no notify
/// sink, fires nothing.
async fn notify_deploy_failure(
    config: &DeployConfig,
    fraise: &str,
    environment: &str,
    outcome: &SagaOutcome,
) {
    use fraisier_self_upgrade::Notifier as _;
    if matches!(outcome, SagaOutcome::Committed) {
        return;
    }
    let Some(command) = config.schedule.as_ref().and_then(|s| s.notify.as_deref()) else {
        return;
    };
    let (_, label, detail) = outcome_result(outcome);
    let payload = fraisier_self_upgrade::FailurePayload {
        event: "scheduled-deploy-failed".to_owned(),
        failed: Some(format!("{fraise}/{environment}")),
        restored: None,
        reason: format!("{label}{detail}").trim().to_owned(),
    };
    fraisier_self_upgrade::ExecHookNotifier::new(command)
        .with_context("FRAISIER_NOTIFY_FRAISE", fraise)
        .with_context("FRAISIER_NOTIFY_ENVIRONMENT", environment)
        .notify(&payload)
        .await;
}

#[cfg(test)]
mod tests {
    use super::{
        apply_exit, bootstrap, check, classify_source, db_backup, db_migrate, db_reset, db_restore,
        deploy, discover_adapters, health, init, list, notify_deploy_failure, provider_test,
        providers, rollback, scaffold, scaffold_install, scheduled_install, scheduled_list,
        scheduled_uninstall, ship, status, sync, validate_config, version_bump, version_show,
        webhook_server, ShipArgs,
    };
    use fraisier_config::DeployConfig;
    use fraisier_core::single_host::DeployRecord;
    use fraisier_saga::events::SagaState;
    use fraisier_saga::saga::SagaOutcome;
    use fraisier_saga::state_store::{
        DeploymentState, FilesystemStateStore, FraiseKey, StateStore,
    };
    use std::path::Path;

    /// Serializes the env-mutating db-op tests so `set_var`/`var` don't race the
    /// process environment across threads.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Drive an async command to completion on a fresh current-thread runtime.
    /// Kept synchronous so the [`ENV_LOCK`] guard is never held across an `.await`.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    /// A db-ops config naming `dsn_env` as the DSN source (no-op command adapter).
    fn db_ops_config(dsn_env: &str) -> String {
        format!(
            "[deploy]\nname = \"demo\"\nenvironment = \"test\"\n\n\
             [migration]\nadapter = \"command\"\ndatabase_url_env = \"{dsn_env}\"\n\n\
             [migration.settings.commands]\nup = \"true\"\ncurrent_revision = \"true\"\n"
        )
    }

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
    fn notify_deploy_failure_carries_the_rollback_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("payload.txt");
        // A [schedule].notify sink that records the reason the webhook carries.
        // A TOML literal string sidesteps escaping the shell's double quotes.
        let notify = format!(
            "notify = 'printf \"%s\" \"$FRAISIER_NOTIFY_REASON\" > {}'",
            out.display()
        );
        let toml = format!("{VALID}\n[schedule]\n{notify}\n");
        let config = DeployConfig::from_toml_str(&toml).expect("parses");

        // The named perf detail rides in via HealthStatus.detail → SagaError.message
        // → RolledBack.reason → FailurePayload.reason (the recommended zero-API-change
        // path, Decision 3); with [schedule].notify set it reaches the webhook sink.
        // The reason here is the shape CommandHealth produces from a --json scan.
        let outcome = SagaOutcome::RolledBack {
            failed_step: "health".to_owned(),
            reason: "health check reported the host unhealthy: \
                     perf regression: order/UPDATE p50 +42% (12ms→17ms)"
                .to_owned(),
        };
        block_on(notify_deploy_failure(
            &config, "checkout", "staging", &outcome,
        ));

        let recorded = std::fs::read_to_string(&out).expect("notify wrote the payload");
        assert!(
            recorded.contains("perf regression: order/UPDATE p50 +42%"),
            "the webhook reason names the regression: {recorded}",
        );
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
            no_bump: false,
            dry_run: true,
            no_deploy: true,
            no_check: false,
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

    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    /// A git repo with a committed `Cargo.toml` at `version` and a committed
    /// `fraisier.toml` holding `config_body`, so the tree is clean for `ship`.
    fn ship_repo(dir: &Path, version: &str, config_body: &str) {
        run_git(dir, &["init", "-b", "main", "-q"]);
        run_git(dir, &["config", "user.email", "t@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
        run_git(dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"app\"\nversion = \"{version}\"\n"),
        )
        .expect("write Cargo.toml");
        std::fs::write(dir.join("fraisier.toml"), config_body).expect("write fraisier.toml");
        run_git(dir, &["add", "-A"]);
        run_git(dir, &["commit", "-q", "-m", "init"]);
    }

    fn head_subject(dir: &Path) -> String {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(["log", "-1", "--format=%s"])
            .output()
            .expect("git log");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// Build `ShipArgs` for a gate test against `dir` (no deploy, no push).
    fn ship_gate_args<'a>(dir: &'a Path, config: &'a Path, no_check: bool) -> ShipArgs<'a> {
        ShipArgs {
            dir,
            level: fraisier_ship::Bump::Patch,
            no_bump: false,
            dry_run: false,
            no_deploy: true,
            no_check,
            push: false,
            remote: "origin".to_owned(),
            config,
            state_dir: Path::new(".fraisier/state"),
            host: None,
        }
    }

    const FAILING_CHECK: &str = "[[checks]]\nname = \"test\"\ncommand = \"false\"\n";
    const PASSING_CHECK: &str = "[[checks]]\nname = \"test\"\ncommand = \"true\"\n";

    #[tokio::test]
    async fn ship_aborts_before_bump_when_a_check_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        ship_repo(dir.path(), "0.1.5", FAILING_CHECK);
        let config = dir.path().join("fraisier.toml");
        let out = ship(ship_gate_args(dir.path(), &config, false))
            .await
            .expect("ship runs");
        assert_eq!(out.exit_code, 1, "pretty: {}", out.pretty);
        assert_eq!(out.json["shipped"], serde_json::json!(false));
        // The gate ran before the bump: version file and history are untouched.
        assert!(std::fs::read_to_string(dir.path().join("Cargo.toml"))
            .unwrap()
            .contains("0.1.5"));
        assert_eq!(head_subject(dir.path()), "init");
    }

    #[tokio::test]
    async fn ship_no_check_bypasses_a_failing_gate() {
        let dir = tempfile::tempdir().expect("tempdir");
        ship_repo(dir.path(), "0.1.5", FAILING_CHECK);
        let config = dir.path().join("fraisier.toml");
        let out = ship(ship_gate_args(dir.path(), &config, true))
            .await
            .expect("ship runs");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert_eq!(out.json["new_version"], serde_json::json!("0.1.6"));
        assert_eq!(out.json["checks"]["ran"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn ship_with_no_checks_configured_bumps_normally() {
        let dir = tempfile::tempdir().expect("tempdir");
        ship_repo(dir.path(), "0.1.5", "# no checks here\n");
        let config = dir.path().join("fraisier.toml");
        let out = ship(ship_gate_args(dir.path(), &config, false))
            .await
            .expect("ship runs");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert_eq!(out.json["new_version"], serde_json::json!("0.1.6"));
        assert_eq!(out.json["checks"]["ran"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn ship_runs_passing_checks_then_bumps() {
        let dir = tempfile::tempdir().expect("tempdir");
        ship_repo(dir.path(), "0.1.5", PASSING_CHECK);
        let config = dir.path().join("fraisier.toml");
        let out = ship(ship_gate_args(dir.path(), &config, false))
            .await
            .expect("ship runs");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert_eq!(out.json["new_version"], serde_json::json!("0.1.6"));
        assert_eq!(out.json["checks"]["ran"], serde_json::json!(true));
        assert_eq!(out.json["checks"]["ok"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn ship_dry_run_does_not_execute_checks() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A failing check that *would* abort a real ship is never executed.
        ship_repo(dir.path(), "0.1.5", FAILING_CHECK);
        let config = dir.path().join("fraisier.toml");
        let mut args = ship_gate_args(dir.path(), &config, false);
        args.dry_run = true;
        let out = ship(args).await.expect("ship dry run");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert_eq!(out.json["checks"]["would_run"], serde_json::json!(1));
        assert!(std::fs::read_to_string(dir.path().join("Cargo.toml"))
            .unwrap()
            .contains("0.1.5"));
    }

    #[tokio::test]
    async fn check_exits_zero_when_all_pass() {
        // A checks-only config (no [deploy]) must run without a full deploy config.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", PASSING_CHECK);
        let out = check(&config, dir.path(), 1).await.expect("check");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert_eq!(out.json["ok"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn check_exits_nonzero_when_a_check_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", FAILING_CHECK);
        let out = check(&config, dir.path(), 1).await.expect("check");
        assert_eq!(out.exit_code, 1, "pretty: {}", out.pretty);
        assert_eq!(out.json["ok"], serde_json::json!(false));
        assert_eq!(out.json["checks"][0]["status"], serde_json::json!("failed"));
    }

    #[tokio::test]
    async fn check_refuses_an_invalid_checks_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A check with a name but no command is invalid.
        let config = write(dir.path(), "fraisier.toml", "[[checks]]\nname = \"a\"\n");
        let out = check(&config, dir.path(), 1).await.expect("check");
        assert_eq!(out.exit_code, 1, "pretty: {}", out.pretty);
        assert_eq!(out.json["ok"], serde_json::json!(false));
        assert!(out.json["issues"].is_array());
    }

    #[tokio::test]
    async fn check_with_no_checks_is_a_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", "# nothing\n");
        let out = check(&config, dir.path(), 1).await.expect("check");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert_eq!(out.json["checks"], serde_json::json!([]));
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
    fn providers_lists_built_in_adapters_per_axis() {
        // Whatever is on PATH, every axis with its built-ins must be listed.
        let out = providers();
        assert_eq!(out.exit_code, 0);
        let axes = out.json["axes"].as_array().expect("axes");
        assert_eq!(axes.len(), 5, "all five axes listed");
        assert!(
            out.pretty.contains("confiture (built-in)"),
            "{}",
            out.pretty
        );
        assert!(out.pretty.contains("nginx (built-in)"), "{}", out.pretty);
    }

    #[tokio::test]
    async fn provider_test_confirms_a_built_in_and_rejects_an_unknown() {
        // A compiled-in adapter is reported present (no handshake).
        let ok = provider_test("confiture").await.expect("built-in");
        assert_eq!(ok.exit_code, 0, "pretty: {}", ok.pretty);
        assert_eq!(ok.json["source"], serde_json::json!("built-in"));

        // An unknown name (no built-in, no fraisier-adapter-* on PATH) fails.
        let unknown = provider_test("definitely-not-a-real-provider-xyz")
            .await
            .expect("unknown");
        assert_eq!(unknown.exit_code, 1);
        assert_eq!(unknown.json["ok"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn deploy_dry_run_resolves_the_plan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", VALID);
        let state = dir.path().join("state");
        let out = deploy(
            &config,
            &state,
            Some("127.0.0.1"),
            Some("1.2.3"),
            true,
            false,
        )
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
    async fn deploy_routes_a_blue_green_strategy_to_the_swap_flow() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = format!(
            "{}\n[lb]\nadapter = \"nginx\"\nupstream = \"checkout_upstream\"\n\
             include_dir = \"{}\"\n\n[blue_green]\ngreen_unit = \"checkout-green.service\"\n\
             green_health_url = \"http://127.0.0.1:8081/healthz\"\n\
             green_servers = [\"127.0.0.1:8081\"]\nblue_servers = [\"127.0.0.1:8080\"]\n",
            VALID.replace(
                "environment = \"staging\"",
                "environment = \"staging\"\nstrategy = \"blue-green\""
            ),
            dir.path().join("nginx").display(),
        );
        let config = write(dir.path(), "fraisier.toml", &cfg);
        let state = dir.path().join("state");
        // Dry run: the strategy is recognized and routed; nothing is executed.
        let out = deploy(&config, &state, None, Some("1.2.3"), true, false)
            .await
            .expect("dry run");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert_eq!(out.json["strategy"], serde_json::json!("blue-green"));
        assert_eq!(out.json["dry_run"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn deploy_refuses_an_invalid_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bad = VALID.replace("database_url_env = \"CHECKOUT_DATABASE_URL\"\n", "");
        let config = write(dir.path(), "fraisier.toml", &bad);
        let state = dir.path().join("state");
        let out = deploy(&config, &state, None, None, false, false)
            .await
            .expect("run");
        assert_eq!(out.exit_code, 1);
        assert_eq!(out.json["ok"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn rollback_plan_shows_source_and_target_without_executing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", VALID);
        let state_dir = dir.path().join("state");
        // Seed a current revision so the plan can show the source.
        let store = FilesystemStateStore::new(&state_dir).expect("store");
        let ledger = DeployRecord {
            active: None,
            revision: Some(fraisier_core::adapter_axes::Revision::new("rev-9")),
        };
        store
            .record_snapshot(
                &FraiseKey::new("checkout", "staging"),
                &serde_json::to_value(&ledger).expect("encode"),
            )
            .await
            .expect("seed");

        // No --yes: plan only, nothing executed (so no adapters/DSN are needed).
        let out = rollback(&config, &state_dir, None, "rev-3", None, false)
            .await
            .expect("rollback plan");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert_eq!(out.json["executed"], serde_json::json!(false));
        assert_eq!(out.json["source"], serde_json::json!("rev-9"));
        assert_eq!(out.json["target"], serde_json::json!("rev-3"));
        assert!(
            out.pretty.contains("rev-9 → rev-3"),
            "pretty: {}",
            out.pretty
        );
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

        let out = status(&config, &state_dir, false).await.expect("status");
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.json["fraise"], serde_json::json!("checkout"));
        assert!(out.pretty.contains("Committed"), "pretty: {}", out.pretty);
    }

    #[tokio::test]
    async fn status_per_host_reports_the_live_active_artifact() {
        // A local artifact with an `active` symlink the current() probe reads back.
        let dir = tempfile::tempdir().expect("tempdir");
        let active = dir.path().join("current");
        let release = dir.path().join("releases/7.7.7");
        std::fs::create_dir_all(&release).expect("release");
        std::os::unix::fs::symlink(&release, &active).expect("symlink");

        let cfg = format!(
            "[deploy]\nname = \"app\"\nenvironment = \"prod\"\n\n\
             [artifact]\nsource = \"local\"\npath = \"{}\"\nactive_path = \"{}\"\n\n\
             [migration]\nadapter = \"command\"\n\n\
             [service]\nadapter = \"systemd\"\nunit = \"app.service\"\n\n\
             [health]\nadapter = \"http\"\nurl = \"http://127.0.0.1:8080/health\"\n",
            release.display(),
            active.display(),
        );
        let config = write(dir.path(), "fraisier.toml", &cfg);
        let state_dir = dir.path().join("state");

        let out = status(&config, &state_dir, true).await.expect("status");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        let per_host = out.json["per_host"].as_array().expect("per_host");
        assert_eq!(per_host.len(), 1);
        assert_eq!(per_host[0]["active"], serde_json::json!("7.7.7"));
        assert!(out.pretty.contains("7.7.7"), "pretty: {}", out.pretty);
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
        let out = deploy(&config, &state, None, Some("1.2.3"), true, false)
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
        let out = deploy(
            &config,
            &state,
            Some("web1.internal"),
            Some("1.2.3"),
            true,
            false,
        )
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
        let out = deploy(&config, &state, None, Some("1.2.3"), true, false)
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

    /// A db-ops config: only [deploy] + [migration] (no artifact/service/health),
    /// with a no-op `command` adapter so the migrate path runs hermetically.
    const DB_OPS: &str = r#"
[deploy]
name = "demo"
environment = "test"

[migration]
adapter = "command"

[migration.settings.commands]
up = "true"
current_revision = "true"
"#;

    #[tokio::test]
    async fn db_migrate_applies_through_the_adapter_and_records_the_ledger() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", DB_OPS);
        let state_dir = dir.path().join("state");

        let out = db_migrate(&config, &state_dir).await.expect("db migrate");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert_eq!(out.json["ok"], serde_json::json!(true));
        assert_eq!(out.json["count"], serde_json::json!(0));

        // The ledger now has an entry for this fraise/env (so status/list see it).
        let store = FilesystemStateStore::new(&state_dir).expect("store");
        let snap = store
            .current_snapshot(&FraiseKey::new("demo", "test"))
            .await
            .expect("snapshot");
        assert!(snap.is_some(), "db migrate records a ledger entry");
    }

    #[tokio::test]
    async fn db_migrate_refuses_a_config_without_a_migration_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        // db ops validate only [deploy] + [migration]; drop [migration].
        let config = write(
            dir.path(),
            "fraisier.toml",
            "[deploy]\nname = \"demo\"\nenvironment = \"test\"\n",
        );
        let out = db_migrate(&config, &dir.path().join("state"))
            .await
            .expect("run");
        assert_eq!(out.exit_code, 1);
        assert_eq!(out.json["ok"], serde_json::json!(false));
    }

    #[test]
    fn db_restore_plan_shows_the_target_without_executing() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let var = "FRAISIER_DBOPS_RESTORE_PLAN";
        std::env::set_var(var, "postgresql://u:s3cret@dbhost:5432/shop");
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", &db_ops_config(var));
        let out = block_on(db_restore(
            &config,
            Path::new("/backups/shop.pgdump"),
            false,
        ))
        .expect("plan");
        std::env::remove_var(var);

        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert_eq!(out.json["executed"], serde_json::json!(false));
        assert!(
            out.pretty.contains("dbhost:5432/shop"),
            "redacted target shown: {}",
            out.pretty
        );
        assert!(
            !out.pretty.contains("s3cret"),
            "the password must not appear in the plan: {}",
            out.pretty
        );
    }

    #[test]
    fn db_reset_plan_is_destructive_and_does_not_execute() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let var = "FRAISIER_DBOPS_RESET_PLAN";
        std::env::set_var(var, "postgres://app@dbhost/shop");
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", &db_ops_config(var));
        let out = block_on(db_reset(&config, &dir.path().join("state"), false)).expect("plan");
        std::env::remove_var(var);

        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert_eq!(out.json["executed"], serde_json::json!(false));
        assert!(
            out.pretty.contains("DESTROYS"),
            "the plan must spell out the destruction: {}",
            out.pretty
        );
    }

    #[test]
    fn db_backup_refuses_to_clobber_without_force() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let var = "FRAISIER_DBOPS_BACKUP_GUARD";
        std::env::set_var(var, "postgres://app@dbhost/shop");
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", &db_ops_config(var));
        let existing = dir.path().join("out.pgdump");
        std::fs::write(&existing, b"old archive").expect("seed");
        // The clobber guard fires before pg_dump is ever invoked.
        let out = block_on(db_backup(&config, Some(&existing), false)).expect("guard");
        std::env::remove_var(var);

        assert_eq!(out.exit_code, 1);
        assert_eq!(out.json["wrote"], serde_json::json!(false));
        assert_eq!(std::fs::read(&existing).expect("read"), b"old archive");
    }

    #[test]
    fn db_ops_error_when_the_dsn_env_is_unset() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let var = "FRAISIER_DBOPS_DELIBERATELY_UNSET";
        std::env::remove_var(var);
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", &db_ops_config(var));
        // Even the plan path resolves the connection first, so an unset var fails.
        let out = block_on(db_restore(&config, Path::new("/x.pgdump"), false)).expect("run");

        assert_eq!(out.exit_code, 1);
        assert_eq!(out.json["ok"], serde_json::json!(false));
        assert!(
            out.pretty.contains(var),
            "names the missing var: {}",
            out.pretty
        );
    }

    #[test]
    fn db_ops_reject_a_non_postgres_dsn() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let var = "FRAISIER_DBOPS_SQLITE";
        std::env::set_var(var, "sqlite:///tmp/x.db");
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", &db_ops_config(var));
        let out = block_on(db_backup(
            &config,
            Some(&dir.path().join("o.pgdump")),
            false,
        ))
        .expect("run");
        std::env::remove_var(var);

        assert_eq!(out.exit_code, 1);
        assert!(
            out.pretty.contains("postgres://"),
            "explains the postgres-only requirement: {}",
            out.pretty
        );
    }

    #[tokio::test]
    async fn db_ops_require_database_url_env() {
        // DB_OPS (the db_migrate fixture) has no database_url_env, so the generic
        // Postgres ops cannot find a DSN and refuse with a clear message.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", DB_OPS);
        let out = db_backup(&config, Some(&dir.path().join("o.pgdump")), false)
            .await
            .expect("run");
        assert_eq!(out.exit_code, 1);
        assert!(
            out.pretty.contains("database_url_env"),
            "points at the missing config: {}",
            out.pretty
        );
    }

    /// A single-host (Local transport) config whose artifact paths live under
    /// `dir`, so bootstrap's `mkdir -p` runs locally and is observable.
    fn bootstrap_config(dir: &Path) -> String {
        format!(
            "[deploy]\nname = \"app\"\nenvironment = \"test\"\n\n\
             [artifact]\nsource = \"local\"\npath = \"/x\"\n\
             staging_dir = \"{}/releases\"\nactive_path = \"{}/srv/current\"\n",
            dir.display(),
            dir.display(),
        )
    }

    #[tokio::test]
    async fn bootstrap_creates_deploy_directories_locally() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", &bootstrap_config(dir.path()));

        let out = bootstrap(&config, None, false).await.expect("bootstrap");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert!(dir.path().join("releases").is_dir(), "staging dir created");
        assert!(dir.path().join("srv").is_dir(), "active parent created");
    }

    #[tokio::test]
    async fn bootstrap_dry_run_creates_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = write(dir.path(), "fraisier.toml", &bootstrap_config(dir.path()));

        let out = bootstrap(&config, None, true).await.expect("dry run");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert!(out.pretty.contains("dry run"), "pretty: {}", out.pretty);
        assert!(
            !dir.path().join("releases").exists(),
            "dry run created nothing"
        );
    }

    #[tokio::test]
    async fn bootstrap_reports_nothing_to_prepare_without_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        // [artifact] with no staging_dir / active_path → nothing host-specific.
        let config = write(
            dir.path(),
            "fraisier.toml",
            "[deploy]\nname = \"app\"\nenvironment = \"test\"\n\n\
             [artifact]\nsource = \"local\"\npath = \"/x\"\n",
        );
        let out = bootstrap(&config, None, false).await.expect("run");
        assert_eq!(out.exit_code, 0);
        assert!(
            out.pretty.contains("nothing to prepare"),
            "pretty: {}",
            out.pretty
        );
    }

    #[test]
    fn systemctl_restart_command_is_built_correctly() {
        use super::systemctl_restart_command;
        let system = systemctl_restart_command("fraisier-webhook.service", false);
        assert_eq!(system.get_program().to_string_lossy(), "systemctl");
        let args: Vec<String> = system
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["restart", "fraisier-webhook.service"]);

        let user = systemctl_restart_command("u.service", true);
        let args: Vec<String> = user
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["--user", "restart", "u.service"]);
    }

    #[test]
    fn classify_source_splits_urls_from_local_paths() {
        use fraisier_self_upgrade::Source;
        assert!(matches!(
            classify_source("https://x/fraisier", Some("abc"), None),
            Source::Url { url, sha256: Some(s), .. } if url == "https://x/fraisier" && s == "abc"
        ));
        assert!(matches!(
            classify_source("http://x/fraisier", None, Some("https://x/sum")),
            Source::Url { checksum_url: Some(c), .. } if c == "https://x/sum"
        ));
        assert!(matches!(
            classify_source("/opt/fraisier/new", Some("def"), None),
            Source::Path { path, sha256: Some(s) } if path == std::path::Path::new("/opt/fraisier/new") && s == "def"
        ));
    }

    #[test]
    fn apply_exit_maps_each_outcome_to_a_code() {
        use fraisier_self_upgrade::ApplyOutcome;
        assert_eq!(
            apply_exit(&ApplyOutcome::Committed {
                id: "2".into(),
                pruned: 0
            }),
            (0, "committed")
        );
        assert_eq!(
            apply_exit(&ApplyOutcome::Reverted {
                failed: "2".into(),
                restored: "1".into(),
                reason: String::new()
            }),
            (1, "reverted")
        );
        assert_eq!(
            apply_exit(&ApplyOutcome::ManualIntervention {
                reason: String::new()
            }),
            (2, "manual_intervention")
        );
        assert_eq!(
            apply_exit(&ApplyOutcome::AbortedBeforeSwap {
                reason: String::new()
            }),
            (1, "aborted")
        );
    }

    #[tokio::test]
    async fn sync_pushes_local_state_and_pull_round_trips_via_a_remote() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A bare repo standing in for the shared remote.
        let remote = dir.path().join("remote.git");
        let ok = std::process::Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&remote)
            .status()
            .expect("git init")
            .success();
        assert!(ok, "bare remote created");

        let cfg = format!(
            "{VALID}\n[sync]\nremote = \"{}\"\nsync_dir = \"{}\"\n",
            remote.display(),
            dir.path().join("sync.git").display(),
        );
        let config = write(dir.path(), "fraisier.toml", &cfg);

        // Seed a deploy state, then push it to the remote ledger.
        let state_dir = dir.path().join("state");
        let store = FilesystemStateStore::new(&state_dir).expect("store");
        store
            .record_state(
                &FraiseKey::new("checkout", "staging"),
                &DeploymentState::new(SagaState::Committed, Some("rev-7".to_owned())),
            )
            .await
            .expect("seed");
        let out = sync(&config, &state_dir, false, false).await.expect("push");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        assert!(
            out.pretty.contains("checkout/staging"),
            "pushed the key: {}",
            out.pretty
        );

        // Pull into a fresh store: the state comes back through the remote.
        let fresh = dir.path().join("state-fresh");
        let out = sync(&config, &fresh, true, false).await.expect("pull");
        assert_eq!(out.exit_code, 0, "pretty: {}", out.pretty);
        let restored = FilesystemStateStore::new(&fresh)
            .expect("fresh store")
            .current_state(&FraiseKey::new("checkout", "staging"))
            .await
            .expect("read");
        assert!(
            restored.is_some(),
            "pull restored the state: {}",
            out.pretty
        );
    }

    #[tokio::test]
    async fn a_divergent_sync_push_shows_the_divergence_then_pull_retry_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let remote = dir.path().join("remote.git");
        assert!(std::process::Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&remote)
            .status()
            .expect("git init")
            .success());
        let key = FraiseKey::new("checkout", "staging");

        let config_for = |name: &str| {
            let cfg = format!(
                "{VALID}\n[sync]\nremote = \"{}\"\nsync_dir = \"{}\"\n",
                remote.display(),
                dir.path().join(format!("{name}/sync.git")).display(),
            );
            write(dir.path(), &format!("{name}.toml"), &cfg)
        };

        // Operator A establishes the remote ref.
        let (config_a, state_a) = (config_for("a"), dir.path().join("a/state"));
        FilesystemStateStore::new(&state_a)
            .expect("store a")
            .record_state(
                &key,
                &DeploymentState::new(SagaState::Committed, Some("A".to_owned())),
            )
            .await
            .expect("seed a");
        let out = sync(&config_a, &state_a, false, false)
            .await
            .expect("a push");
        assert_eq!(out.exit_code, 0, "A pushed: {}", out.pretty);

        // Operator B, with an independent sync base + divergent state, loses the race.
        let (config_b, state_b) = (config_for("b"), dir.path().join("b/state"));
        FilesystemStateStore::new(&state_b)
            .expect("store b")
            .record_state(
                &key,
                &DeploymentState::new(SagaState::Committed, Some("B".to_owned())),
            )
            .await
            .expect("seed b");
        let out = sync(&config_b, &state_b, false, false)
            .await
            .expect("b push");
        assert_eq!(
            out.exit_code, 1,
            "the loser gets a non-zero exit: {}",
            out.pretty
        );
        assert!(
            out.pretty.contains("CONFLICT"),
            "loud conflict: {}",
            out.pretty
        );
        assert!(
            out.pretty.contains("local "),
            "shows local head: {}",
            out.pretty
        );
        assert!(
            out.pretty.contains("remote "),
            "shows remote head: {}",
            out.pretty
        );

        // A's state was not clobbered.
        let on_remote = fraisier_sync::remote_keys(&remote.display().to_string()).expect("keys");
        assert_eq!(on_remote, vec!["checkout/staging".to_owned()]);

        // B reconciles: pull accepts the remote, then the retry succeeds (exit 0).
        let out = sync(&config_b, &state_b, true, false)
            .await
            .expect("b pull");
        assert_eq!(out.exit_code, 0, "pull: {}", out.pretty);
        let out = sync(&config_b, &state_b, false, false)
            .await
            .expect("b retry");
        assert_eq!(
            out.exit_code, 0,
            "retry succeeds after pull: {}",
            out.pretty
        );
    }

    #[test]
    fn scheduled_install_plans_then_installs_a_timer_and_service() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg =
            format!("{VALID}\n[schedule]\ncalendar = \"daily 03:00\"\ncommand = \"backup\"\n");
        let config = write(dir.path(), "fraisier.toml", &cfg);
        let root = dir.path().join("root");

        // Plan only (no --yes): nothing written.
        let plan = scheduled_install(&config, &root, false, false).expect("plan");
        assert_eq!(plan.json["applied"], serde_json::json!(false));
        assert!(!root.exists(), "planning wrote nothing");

        // Apply installs the timer + service under the root.
        let done = scheduled_install(&config, &root, true, false).expect("install");
        assert_eq!(done.json["applied"], serde_json::json!(true));
        assert!(root
            .join("etc/systemd/system/fraisier-checkout-staging-scheduled.timer")
            .exists());
        assert!(root
            .join("etc/systemd/system/fraisier-checkout-staging-scheduled.service")
            .exists());
    }

    #[test]
    fn scheduled_install_refuses_an_unattended_deploy_without_the_optin() {
        let dir = tempfile::tempdir().expect("tempdir");
        // command = "deploy" without allow_unattended_deploy + notify: refused.
        let cfg =
            format!("{VALID}\n[schedule]\ncalendar = \"daily 03:00\"\ncommand = \"deploy\"\n");
        let config = write(dir.path(), "fraisier.toml", &cfg);
        let root = dir.path().join("root");

        let out = scheduled_install(&config, &root, true, false).expect("runs");
        assert_eq!(out.exit_code, 1, "refused: {}", out.pretty);
        assert_eq!(out.json["refused"], serde_json::json!(true));
        assert!(!root.exists(), "nothing installed on refusal");
    }

    #[test]
    fn scheduled_install_with_optin_deploy_installs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = format!(
            "{VALID}\n[schedule]\ncalendar = \"daily 03:00\"\ncommand = \"deploy\"\n\
             allow_unattended_deploy = true\nnotify = \"systemd-cat -t fraisier\"\n"
        );
        let config = write(dir.path(), "fraisier.toml", &cfg);
        let root = dir.path().join("root");
        let out = scheduled_install(&config, &root, true, false).expect("install");
        assert_eq!(
            out.json["applied"],
            serde_json::json!(true),
            "{}",
            out.pretty
        );
    }

    #[test]
    fn scheduled_install_list_uninstall_round_trips_clean() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg =
            format!("{VALID}\n[schedule]\ncalendar = \"daily 03:00\"\ncommand = \"backup\"\n");
        let config = write(dir.path(), "fraisier.toml", &cfg);
        let root = dir.path().join("root");

        scheduled_install(&config, &root, true, false).expect("install");
        let listed = scheduled_list(&root).expect("list");
        let units = listed.json["units"].as_array().expect("units array");
        assert_eq!(units.len(), 2, "timer + service listed: {}", listed.pretty);

        // Plan only: nothing removed yet.
        let plan = scheduled_uninstall(&config, &root, false).expect("plan");
        assert_eq!(plan.json["removed"], serde_json::json!(false));

        let done = scheduled_uninstall(&config, &root, true).expect("uninstall");
        assert_eq!(done.json["removed"], serde_json::json!(true));
        let after = scheduled_list(&root).expect("list");
        assert!(
            after.json["units"].as_array().expect("array").is_empty(),
            "systemd dir clean after uninstall: {}",
            after.pretty
        );
    }

    #[tokio::test]
    async fn notify_fires_on_a_rolled_back_deploy_and_is_silent_on_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("captured");
        // A notify hook (no single quotes, so it embeds in a TOML literal string).
        let command = format!(
            "echo \"$FRAISIER_NOTIFY_EVENT $FRAISIER_NOTIFY_FRAISE\" > {}",
            out.display()
        );
        let toml = format!("[schedule]\nnotify = '{command}'\n");
        let config = DeployConfig::from_toml_str(&toml).expect("parses");

        // A rolled-back (sick) deploy fires the hook with a failure payload.
        notify_deploy_failure(
            &config,
            "checkout",
            "production",
            &SagaOutcome::RolledBack {
                failed_step: "health".to_owned(),
                reason: "500".to_owned(),
            },
        )
        .await;
        let captured = std::fs::read_to_string(&out).expect("hook ran");
        assert!(
            captured.contains("scheduled-deploy-failed checkout"),
            "payload via env: {captured}"
        );

        // A committed deploy notifies nothing (remove the file, re-run, stays gone).
        std::fs::remove_file(&out).expect("rm");
        notify_deploy_failure(&config, "checkout", "production", &SagaOutcome::Committed).await;
        assert!(!out.exists(), "no notify on a committed deploy");
    }

    #[tokio::test]
    async fn notify_is_a_noop_without_a_configured_sink() {
        // No [schedule].notify: a rolled-back deploy must not panic or error.
        let config = DeployConfig::from_toml_str("[deploy]\nname = \"x\"\n").expect("parses");
        notify_deploy_failure(
            &config,
            "x",
            "prod",
            &SagaOutcome::PartialRollback {
                reason: "boom".to_owned(),
            },
        )
        .await;
    }

    #[tokio::test]
    async fn webhook_server_refuses_an_invalid_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Valid deploy axes, but [webhook] is missing secret_env → validation
        // fails and the server refuses to start (no socket is bound).
        let cfg = format!("{VALID}\n[webhook]\nlisten = \"127.0.0.1:0\"\n");
        let config = write(dir.path(), "fraisier.toml", &cfg);
        let out = webhook_server(&config, &dir.path().join("state"), None)
            .await
            .expect("run");
        assert_eq!(out.exit_code, 1);
        assert!(
            out.pretty.contains("secret_env"),
            "names the missing field: {}",
            out.pretty
        );
    }
}
