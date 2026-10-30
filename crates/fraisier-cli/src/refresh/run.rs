//! `fraisier db refresh`: replace this config's database with the newest
//! archive of another environment.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use fraisier_config::DeployConfig;
use fraisier_db::archive::Archive;
use fraisier_db::refresh::{refresh_in_place, RefreshServer, Refreshed};
use fraisier_db::restore::{RestoreError, RestoreIdentity};
use fraisier_db::retention::Verdict;
use fraisier_saga::state_store::{
    FilesystemStateStore, FraiseKey, StateStore as _, StateStoreError,
};
use serde_json::json;

use super::{expected_marker, refuse, refuse_as_target};
use crate::commands::{self, CommandOutput};

/// What a refresh leaves undone until sanitizing and the outbound flag exist:
/// the service stays stopped, so a copy of another environment's data never
/// runs with its integrations on.
const NOT_STARTED: &str = "sanitize and the outbound flag are not implemented yet, so the \
                           service was left stopped";

/// The systemd units a refresh stops and starts.
#[async_trait]
pub(crate) trait Units: Send + Sync {
    /// Whether `unit` is running.
    ///
    /// # Errors
    /// `systemctl` could not be run.
    async fn is_active(&self, unit: &str) -> Result<bool, String>;

    /// Stop `unit`.
    ///
    /// # Errors
    /// `systemctl stop` failed.
    async fn stop(&self, unit: &str) -> Result<(), String>;

    /// Start `unit`.
    ///
    /// # Errors
    /// `systemctl start` failed.
    async fn start(&self, unit: &str) -> Result<(), String>;
}

/// [`Units`] over `systemctl` (or `$FRAISIER_SYSTEMCTL_BIN`), on this host.
pub(crate) struct Systemctl {
    program: OsString,
    user: bool,
}

impl Systemctl {
    /// Drive the user manager when `user`, else the system manager.
    pub(crate) fn new(user: bool) -> Self {
        let program = std::env::var_os("FRAISIER_SYSTEMCTL_BIN")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| OsString::from("systemctl"));
        Self { program, user }
    }

    async fn run(&self, verb: &str, unit: &str) -> Result<std::process::Output, String> {
        let mut command = tokio::process::Command::new(&self.program);
        fraisier_core::child_env::scrub(command.as_std_mut());
        if self.user {
            command.arg("--user");
        }
        command
            .arg(verb)
            .arg(unit)
            .output()
            .await
            .map_err(|error| format!("could not run systemctl: {error}"))
    }

    async fn act(&self, verb: &str, unit: &str) -> Result<(), String> {
        let out = self.run(verb, unit).await?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "systemctl {verb} {unit}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
}

#[async_trait]
impl Units for Systemctl {
    async fn is_active(&self, unit: &str) -> Result<bool, String> {
        let out = self.run("is-active", unit).await?;
        Ok(String::from_utf8_lossy(&out.stdout).trim() == "active")
    }

    async fn stop(&self, unit: &str) -> Result<(), String> {
        self.act("stop", unit).await
    }

    async fn start(&self, unit: &str) -> Result<(), String> {
        self.act("start", unit).await
    }
}

/// The units a refresh stops, in order: the timers first, so nothing is
/// started again behind it, then the other services, then the `[service]`
/// unit. They start again in the reverse order.
fn units_to_stop(config: &DeployConfig, service: &str) -> Vec<String> {
    let extra = config
        .refresh
        .as_ref()
        .map(|refresh| refresh.units.as_slice())
        .unwrap_or_default();
    let is_timer = |unit: &&String| {
        unit.rsplit_once('.')
            .is_some_and(|(_, kind)| kind == "timer")
    };
    let timers = extra.iter().filter(is_timer);
    let services = extra.iter().filter(|unit| !is_timer(unit));
    timers
        .chain(services)
        .cloned()
        .chain(std::iter::once(service.to_owned()))
        .collect()
}

/// The `[service]` unit and whether it runs under the user manager, or why a
/// refresh cannot drive this config's service.
fn systemd_service(config: &DeployConfig) -> std::result::Result<(String, bool), &'static str> {
    if config.hosts.is_some() {
        return Err(
            "a refresh runs on the one host it is invoked on, and this config \
                    deploys to [hosts]",
        );
    }
    let service = config.service.as_ref().filter(|service| {
        service.adapter.as_deref() == Some("systemd")
            && service.unit.as_deref().is_some_and(|unit| !unit.is_empty())
    });
    service.map_or(
        Err(
            "a refresh stops and starts the service with systemd, and this config has no \
             [service] adapter = \"systemd\" with a unit",
        ),
        |service| {
            Ok((
                service.unit.clone().unwrap_or_default(),
                service.user == Some(true),
            ))
        },
    )
}

/// The newest archive of `fraise`/`source` in `dir`, refused when there is
/// none, it does not read back, or its age is out of bounds.
async fn pick_archive(
    dir: &Path,
    (fraise, source): (&str, &str),
    max_age_hours: u32,
    pg_restore: &Path,
    now: SystemTime,
) -> std::result::Result<(Archive, Vec<String>), String> {
    let archive = fraisier_db::archive::latest_archive(dir, fraise, source)
        .map_err(|error| format!("cannot read {}: {error}", dir.display()))?
        .ok_or_else(|| format!("no archive of {fraise}/{source} in {}", dir.display()))?;
    let path = archive.path.display().to_string();
    let mut warnings = Vec::new();
    match fraisier_db::archive::verify_archive(pg_restore, &archive.path).await {
        Verdict::Invalid(reason) => {
            return Err(format!("{path} is not a readable archive: {reason}"))
        }
        Verdict::Unverifiable(reason) => {
            warnings.push(format!(
                "{path} could not be verified before the refresh: {reason}"
            ));
        }
        Verdict::Valid => {}
    }
    let taken_at = archive.stamp.to_system_time();
    if let Ok(ahead) = taken_at.duration_since(now) {
        if ahead > commands::DRILL_CLOCK_SKEW {
            return Err(format!(
                "the newest archive, {path}, is stamped {} minutes in the future: check the \
                 clocks",
                ahead.as_secs() / 60
            ));
        }
    }
    if let Ok(age) = now.duration_since(taken_at) {
        if age > Duration::from_hours(u64::from(max_age_hours)) {
            return Err(format!(
                "the newest archive, {path}, is {} hours old: older than max_age_hours = \
                 {max_age_hours}",
                age.as_secs() / 3_600
            ));
        }
    }
    Ok((archive, warnings))
}

/// `db refresh`: replace this config's database with the newest archive of
/// `[refresh].source_environment`.
///
/// Without `execute` (the `--yes` flag) it prints the plan. With it, under the
/// deploy lock: stop every unit of the environment, drop the marked database,
/// restore the archive into a new one owned by the app role, and run the
/// migrations forward as the app role.
pub(crate) async fn db_refresh(
    config_path: &Path,
    state_dir: &Path,
    execute: bool,
) -> Result<CommandOutput> {
    let config = commands::load(config_path)?;
    let user = systemd_service(&config).is_ok_and(|(_, user)| user);
    db_refresh_with(
        config_path,
        state_dir,
        execute,
        &Machinery {
            server: &fraisier_db::restore::Postgres,
            units: &Systemctl::new(user),
            pg_restore: &fraisier_db::archive::ArchiveTools::default().pg_restore,
            now: SystemTime::now(),
        },
    )
    .await
}

/// What [`db_refresh`] drives, supplied by the caller.
pub(crate) struct Machinery<'a> {
    pub(crate) server: &'a dyn RefreshServer,
    pub(crate) units: &'a dyn Units,
    pub(crate) pg_restore: &'a Path,
    pub(crate) now: SystemTime,
}

/// A refresh that passed every check made before the deploy lock.
struct Plan {
    fraise: String,
    environment: String,
    archive: Archive,
    identity: RestoreIdentity,
    target: String,
    marker: String,
    units: Vec<String>,
    warnings: Vec<String>,
}

/// [`db_refresh`] with its machinery supplied by the caller.
pub(crate) async fn db_refresh_with(
    config_path: &Path,
    state_dir: &Path,
    execute: bool,
    machinery: &Machinery<'_>,
) -> Result<CommandOutput> {
    let config = commands::load(config_path)?;
    let mut plan = match prepare(&config, machinery).await? {
        Ok(plan) => plan,
        Err(refusal) => return Ok(refusal),
    };
    if !execute {
        return Ok(CommandOutput {
            exit_code: 0,
            pretty: format!(
                "refresh plan: {} → {}\n  stops: {}\n  then drops the database, \
                 restores, and migrates\npass --yes to execute — this REPLACES the database\n",
                plan.archive.path.display(),
                plan.target,
                plan.units.join(", ")
            ),
            json: json!({
                "executed": false,
                "archive": plan.archive.path.display().to_string(),
                "target": plan.target,
                "units": plan.units,
                "warnings": plan.warnings,
            }),
        });
    }

    let store = FilesystemStateStore::new(state_dir)
        .with_context(|| format!("opening state store at {}", state_dir.display()))?;
    let key = FraiseKey::new(&plan.fraise, &plan.environment);
    let guard = match store.acquire_lock(&key).await {
        Ok(guard) => guard,
        Err(StateStoreError::Locked { .. }) => {
            return Ok(refuse(&format!(
                "a deploy of {key} holds the deploy lock; refusing to refresh while it runs. \
                 Nothing was changed"
            )));
        }
        Err(error) => return Err(error).context("taking the deploy lock"),
    };
    let outcome = run_locked(&config, state_dir, machinery, &plan).await;
    store
        .release_lock(guard)
        .await
        .context("releasing the deploy lock")?;
    Ok(render(&mut plan, outcome))
}

/// The checks a refresh makes before taking the deploy lock, refusing (with
/// nothing changed) when one fails.
async fn prepare(
    config: &DeployConfig,
    machinery: &Machinery<'_>,
) -> Result<std::result::Result<Plan, CommandOutput>> {
    let refused = |problem: &str| {
        refuse(&format!(
            "refusing to refresh: {problem}. Nothing was changed"
        ))
    };
    if let Some(refusal) = refuse_as_target(config, "refresh") {
        return Ok(Err(refusal));
    }
    let service = match systemd_service(config) {
        Ok((service, _)) => service,
        Err(problem) => return Ok(Err(refused(problem))),
    };
    let refresh = config.refresh.clone().unwrap_or_default();
    let deploy = config.deploy.as_ref().context("[deploy] section")?;
    let fraise = deploy.name.clone().unwrap_or_default();
    let environment = deploy.environment.clone().unwrap_or_default();
    let source = refresh.source_environment.clone().unwrap_or_default();
    let source_dir: PathBuf = refresh.source_dir.clone().unwrap_or_default();
    let (archive, warnings) = match pick_archive(
        &source_dir,
        (&fraise, &source),
        refresh.max_age_hours(),
        machinery.pg_restore,
        machinery.now,
    )
    .await
    {
        Ok(picked) => picked,
        Err(problem) => return Ok(Err(refused(&problem))),
    };
    let identity = match commands::restore_identity(config) {
        Ok(identity) => identity,
        Err(error) => return Ok(Err(commands::db_config_error(&error))),
    };
    if let Some(data_dir) = refresh.data_dir.as_deref() {
        let room = match machinery.server.database_bytes(identity.app()).await {
            Ok(bytes) => fraisier_db::restore::room_for_copy(bytes, data_dir),
            Err(error) => Err(format!("could not size the database: {error}")),
        };
        if let Err(problem) = room {
            return Ok(Err(refused(&problem)));
        }
    }
    Ok(Ok(Plan {
        target: identity.app().redacted(),
        marker: expected_marker(config),
        units: units_to_stop(config, &service),
        fraise,
        environment,
        archive,
        identity,
        warnings,
    }))
}

/// The output of a refresh that ran under the lock.
fn render(plan: &mut Plan, outcome: std::result::Result<Done, Failure>) -> CommandOutput {
    let mut json = json!({
        "fraise": plan.fraise,
        "environment": plan.environment,
        "archive": plan.archive.path.display().to_string(),
        "archive_taken_at": plan.archive.stamp.as_str(),
        "target": plan.target,
        "units": plan.units,
    });
    let (exit_code, mut pretty) = match outcome {
        Ok(done) => {
            plan.warnings.extend(done.warnings);
            json["ok"] = json!(true);
            json["complete"] = json!(false);
            json["applied"] = json!(done.applied);
            json["tables"] = json!(done.tables);
            json["service"] = json!("stopped");
            json["note"] = json!(NOT_STARTED);
            let mut pretty = format!(
                "refreshed {} from {}: applied {} migration(s), {} table(s)\n",
                plan.target,
                plan.archive.path.display(),
                done.applied.len(),
                done.tables
            );
            if let Some(rebuild) = &done.tviews {
                let _ = writeln!(
                    pretty,
                    "post-restore TVIEW rebuild: {}",
                    commands::describe_rebuilt(&rebuild.rebuilt)
                );
            }
            let _ = writeln!(pretty, "{NOT_STARTED}: {}", plan.units.join(", "));
            (0, pretty)
        }
        Err(failure) => {
            let message = fraisier_saga::redact::credentials(&failure.message);
            json["ok"] = json!(false);
            json["failed_step"] = json!(failure.step);
            json["error"] = json!(message);
            json["service"] = json!(if failure.restarted {
                "restarted"
            } else {
                "stopped"
            });
            let after = if failure.restarted {
                "the units that were running were started again"
            } else {
                "the units were left stopped"
            };
            (
                1,
                format!("refresh failed at {}: {message}; {after}\n", failure.step),
            )
        }
    };
    json["warnings"] = json!(plan.warnings);
    for warning in &plan.warnings {
        let _ = writeln!(pretty, "warning: {warning}");
    }
    CommandOutput {
        exit_code,
        pretty,
        json,
    }
}

/// What a refresh did, once the database is migrated.
struct Done {
    applied: Vec<String>,
    tables: u64,
    tviews: Option<fraisier_db::restore::TviewRebuild>,
    warnings: Vec<String>,
}

/// Why a refresh stopped, at which step, and whether the units were started
/// again (only when nothing was changed).
struct Failure {
    step: &'static str,
    message: String,
    restarted: bool,
}

/// The refresh's steps under the deploy lock.
async fn run_locked(
    config: &DeployConfig,
    state_dir: &Path,
    machinery: &Machinery<'_>,
    plan: &Plan,
) -> std::result::Result<Done, Failure> {
    let (identity, units) = (&plan.identity, &plan.units);
    let units_api = machinery.units;
    let mut running = Vec::new();
    for unit in units {
        let failed = |message: String| Failure {
            step: "stop",
            message,
            restarted: false,
        };
        match units_api.is_active(unit).await {
            Ok(true) => running.push(unit.clone()),
            Ok(false) => {}
            Err(error) => {
                return Err(restart(units_api, &running, failed(error)).await);
            }
        }
        if let Err(error) = units_api.stop(unit).await {
            return Err(restart(units_api, &running, failed(error)).await);
        }
    }

    let Refreshed {
        warnings, tviews, ..
    } = match refresh_in_place(machinery.server, identity, &plan.archive.path, &plan.marker).await {
        Ok(refreshed) => refreshed,
        Err(RestoreError::Refused(message)) => {
            let refused = Failure {
                step: "refresh",
                message,
                restarted: false,
            };
            return Err(restart(units_api, &running, refused).await);
        }
        // Anything else happened after the drop: the units stay stopped.
        Err(error) => {
            return Err(Failure {
                step: "restore",
                message: error.to_string(),
                restarted: false,
            })
        }
    };
    let failed = |step, message| Failure {
        step,
        message,
        restarted: false,
    };
    let applied = migrate(config, state_dir)
        .await
        .map_err(|error| failed("migrate", format!("{error:#}")))?;
    let tables = machinery
        .server
        .table_count(identity.app())
        .await
        .map_err(|error| failed("tables", format!("could not count the tables: {error}")))?;
    let min_tables = config
        .refresh
        .as_ref()
        .and_then(|refresh| refresh.min_tables)
        .unwrap_or_default();
    if tables < min_tables {
        return Err(failed(
            "tables",
            format!("the refreshed database holds {tables} table(s), fewer than min_tables = {min_tables}"),
        ));
    }
    Ok(Done {
        applied,
        tables,
        tviews,
        warnings: (!warnings.trim().is_empty())
            .then(|| format!("pg_restore warned: {}", warnings.trim()))
            .into_iter()
            .collect(),
    })
}

/// Start `running` again, last stopped first, after a refusal that changed
/// nothing; `failure` records whether every one started.
async fn restart(units: &dyn Units, running: &[String], mut failure: Failure) -> Failure {
    let mut problems = Vec::new();
    for unit in running.iter().rev() {
        if let Err(error) = units.start(unit).await {
            problems.push(error);
        }
    }
    if problems.is_empty() {
        failure.restarted = true;
    } else {
        let _ = write!(
            failure.message,
            "; and starting the units again failed: {}",
            problems.join("; ")
        );
    }
    failure
}

/// Run the migrations forward on the refreshed database, as `db migrate` does:
/// through the migration adapter, with the app DSN only, and record the
/// revision in the ledger.
async fn migrate(config: &DeployConfig, state_dir: &Path) -> Result<Vec<String>> {
    let resolved = crate::factory::build_migration_only(config)?;
    let adapter = resolved.migration.as_ref();
    let outcome = adapter
        .up(&resolved.ctx, None)
        .await
        .context("applying migrations")?;
    let current = adapter.current_revision(&resolved.ctx).await.ok().flatten();
    commands::record_ledger_revision(state_dir, &resolved.fraise, &resolved.environment, current)
        .await?;
    Ok(outcome.applied.iter().map(ToString::to_string).collect())
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
