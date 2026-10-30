//! `fraisier db refresh`: replace this config's database with the newest
//! archive of another environment.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context as _, Result};
use fraisier_config::{DeployConfig, Sanitize};
use fraisier_db::archive::{Archive, UtcStamp};
use fraisier_db::refresh::{refresh_in_place, RefreshServer, Refreshed};
use fraisier_db::restore::{RestoreError, RestoreIdentity, TviewRebuild};
use fraisier_db::retention::Verdict;
use fraisier_db::sanitize::Sanitized;
use fraisier_saga::state_store::{
    FilesystemStateStore, FraiseKey, StateStore as _, StateStoreError,
};
use serde_json::{json, Value};

use super::units::{current_uid, systemd_service, units_to_stop, Systemctl, Units};
use super::{expected_marker, flag, refuse, refuse_as_target};
use crate::commands::{self, CommandOutput};

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
/// restore the archive into a new one owned by the app role, migrate it as the
/// app role, run the sanitize step, write the outbound flag, start the units
/// that were running, and prove each started with the flag set.
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
            etc: Path::new("/etc"),
            operator_uid: current_uid(),
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
    /// Where `<fraise>/<environment>.refreshed.env` goes: `/etc` on a host.
    pub(crate) etc: &'a Path,
    /// The uid running the refresh, trusted with the sanitize step and the
    /// flag's directory alongside root.
    pub(crate) operator_uid: u32,
    pub(crate) now: SystemTime,
}

/// The sanitize step a refresh runs, as the config declares it.
enum Step {
    Script(PathBuf),
    Skipped(String),
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
    sanitize: Step,
    flag: PathBuf,
    flag_var: String,
    flag_gid: u32,
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
        return Ok(show_plan(&plan));
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
    let started = Instant::now();
    let outcome = run_locked(&config, state_dir, machinery, &plan).await;
    store
        .release_lock(guard)
        .await
        .context("releasing the deploy lock")?;
    let mut out = render(&mut plan, outcome);
    out.json["started_at"] = json!(UtcStamp::from_system_time(machinery.now).as_str());
    out.json["duration_seconds"] = json!(started.elapsed().as_secs());
    if let Err(error) = write_receipt(state_dir, &plan.fraise, &plan.environment, &out.json) {
        let _ = writeln!(
            out.pretty,
            "warning: the refresh receipt was not written: {error}"
        );
    }
    Ok(out)
}

/// The plan a refresh without `--yes` prints.
fn show_plan(plan: &Plan) -> CommandOutput {
    let sanitize = match &plan.sanitize {
        Step::Script(path) => path.display().to_string(),
        Step::Skipped(reason) => format!("none ({reason})"),
    };
    CommandOutput {
        exit_code: 0,
        pretty: format!(
            "refresh plan: {} → {}\n  stops: {}\n  then drops the database, restores, \
             migrates\n  sanitize: {sanitize}\n  outbound flag: {}=1 in {}\n  then starts \
             the units that were running\npass --yes to execute — this REPLACES the database\n",
            plan.archive.path.display(),
            plan.target,
            plan.units.join(", "),
            plan.flag_var,
            plan.flag.display()
        ),
        json: json!({
            "executed": false,
            "archive": plan.archive.path.display().to_string(),
            "target": plan.target,
            "units": plan.units,
            "sanitize": sanitize,
            "outbound_flag": { "var": plan.flag_var, "file": plan.flag.display().to_string() },
            "warnings": plan.warnings,
        }),
    }
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
    let sanitize = match refresh.sanitize() {
        Some(Sanitize::Script(path)) => {
            if let Err(problem) = fraisier_db::sanitize::check(path, machinery.operator_uid) {
                return Ok(Err(refused(&problem)));
            }
            Step::Script(path.to_owned())
        }
        Some(Sanitize::Skipped { reason }) => Step::Skipped(reason.to_owned()),
        // validate() requires one; this is only reached if that changes.
        None => return Ok(Err(refused("[refresh].sanitize is not set"))),
    };
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
    let units = units_to_stop(config, &service);
    let flag = flag::flag_path(machinery.etc, &fraise, &environment);
    let flag_dir = flag.parent().unwrap_or(machinery.etc);
    if let Some(problem) = flag::dir_problem(flag_dir, machinery.operator_uid) {
        return Ok(Err(refused(&problem)));
    }
    if let Err(problem) = flag::loaded_by_every_unit(machinery.units, &units, &flag).await {
        return Ok(Err(refused(&problem)));
    }
    let flag_gid = match machinery.units.group(&service).await {
        Ok(gid) => gid,
        Err(error) => return Ok(Err(refused(&error))),
    };
    Ok(Ok(Plan {
        target: identity.app().redacted(),
        marker: expected_marker(config),
        flag_var: refresh.outbound_flag().to_owned(),
        fraise,
        environment,
        archive,
        identity,
        units,
        sanitize,
        flag,
        flag_gid,
        warnings,
    }))
}

/// The output of a refresh that ran under the lock: also its receipt.
fn render(plan: &mut Plan, outcome: std::result::Result<Done, Failure>) -> CommandOutput {
    let archive_meta = std::fs::metadata(&plan.archive.path).ok();
    let mut json = json!({
        "fraise": plan.fraise,
        "environment": plan.environment,
        "archive": {
            "path": plan.archive.path.display().to_string(),
            "taken_at": plan.archive.stamp.as_str(),
            "bytes": archive_meta.as_ref().map(std::fs::Metadata::len),
            "modified": archive_meta
                .and_then(|meta| meta.modified().ok())
                .map(|time| UtcStamp::from_system_time(time).as_str().to_owned()),
        },
        "target": plan.target,
        "units": plan.units,
        "outbound_flag": { "var": plan.flag_var, "file": plan.flag.display().to_string() },
    });
    json["sanitize"] = match &plan.sanitize {
        Step::Script(path) => json!({ "path": path.display().to_string() }),
        Step::Skipped(reason) => json!({ "path": "none", "reason": reason }),
    };
    let (exit_code, mut pretty) = match outcome {
        Ok(done) => {
            plan.warnings.extend(done.warnings);
            json["ok"] = json!(true);
            json["applied"] = json!(done.applied);
            json["tables"] = json!(done.tables);
            record_sanitize(&mut json, done.sanitized.as_ref(), "passed");
            json["outbound_flag"]["proof"] = json!(done
                .proof
                .iter()
                .map(|(unit, pid)| json!({ "unit": unit, "pid": pid }))
                .collect::<Vec<_>>());
            json["service"] = json!("started");
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
            let _ = match (&done.sanitized, &plan.sanitize) {
                (Some(ran), _) => writeln!(
                    pretty,
                    "sanitized with {} (sha256 {})",
                    ran.path.display(),
                    ran.sha256
                ),
                (None, Step::Skipped(reason)) => writeln!(pretty, "sanitize skipped: {reason}"),
                (None, Step::Script(_)) => Ok(()),
            };
            let _ = writeln!(
                pretty,
                "started {} with {}=1; clear it with `fraisier db refresh --clear-flag` once \
                 the integrations are safe to turn on",
                plan.units.join(", "),
                plan.flag_var
            );
            (0, pretty)
        }
        Err(failure) => {
            let message = fraisier_saga::redact::credentials(&failure.message);
            json["ok"] = json!(false);
            json["failed_step"] = json!(failure.step);
            json["error"] = json!(message);
            if failure.step == "sanitize" {
                record_sanitize(&mut json, None, "failed");
            }
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

/// Record the sanitize step's `outcome`, and what ran, in `json`.
fn record_sanitize(json: &mut Value, ran: Option<&Sanitized>, outcome: &str) {
    if json["sanitize"]["path"] == "none" {
        json["sanitize"]["outcome"] = json!("skipped");
        return;
    }
    json["sanitize"]["outcome"] = json!(outcome);
    if let Some(ran) = ran {
        json["sanitize"]["kind"] = json!(ran.kind);
        json["sanitize"]["sha256"] = json!(ran.sha256);
    }
}

/// What a refresh did.
struct Done {
    applied: Vec<String>,
    tables: u64,
    tviews: Option<TviewRebuild>,
    sanitized: Option<Sanitized>,
    proof: Vec<(String, u32)>,
    warnings: Vec<String>,
}

/// Why a refresh stopped, at which step, and whether the units were started
/// again (only when nothing was changed).
struct Failure {
    step: &'static str,
    message: String,
    restarted: bool,
}

impl Failure {
    const fn at(step: &'static str, message: String) -> Self {
        Self {
            step,
            message,
            restarted: false,
        }
    }
}

/// The refresh's steps under the deploy lock.
async fn run_locked(
    config: &DeployConfig,
    state_dir: &Path,
    machinery: &Machinery<'_>,
    plan: &Plan,
) -> std::result::Result<Done, Failure> {
    let units = machinery.units;
    let running = stop_all(units, &plan.units).await?;
    let Refreshed {
        warnings, tviews, ..
    } = match refresh_in_place(
        machinery.server,
        &plan.identity,
        &plan.archive.path,
        &plan.marker,
    )
    .await
    {
        Ok(refreshed) => refreshed,
        Err(RestoreError::Refused(message)) => {
            return Err(restart(units, &running, Failure::at("refresh", message)).await);
        }
        // Anything else happened after the drop: the units stay stopped.
        Err(error) => return Err(Failure::at("restore", error.to_string())),
    };
    let applied = migrate(config, state_dir)
        .await
        .map_err(|error| Failure::at("migrate", format!("{error:#}")))?;
    let tables = machinery
        .server
        .table_count(plan.identity.app())
        .await
        .map_err(|error| Failure::at("tables", format!("could not count the tables: {error}")))?;
    let min_tables = config
        .refresh
        .as_ref()
        .and_then(|refresh| refresh.min_tables)
        .unwrap_or_default();
    if tables < min_tables {
        return Err(Failure::at(
            "tables",
            format!(
                "the refreshed database holds {tables} table(s), fewer than min_tables = \
                 {min_tables}"
            ),
        ));
    }
    let sanitized = match &plan.sanitize {
        Step::Script(path) => Some(
            fraisier_db::sanitize::run(path, plan.identity.app(), machinery.operator_uid)
                .await
                .map_err(|error| Failure::at("sanitize", error))?,
        ),
        Step::Skipped(_) => None,
    };
    let proof = start_flagged(units, plan, &running).await?;
    Ok(Done {
        applied,
        tables,
        tviews,
        sanitized,
        proof,
        warnings: (!warnings.trim().is_empty())
            .then(|| format!("pg_restore warned: {}", warnings.trim()))
            .into_iter()
            .collect(),
    })
}

/// Stop `units` in order, returning those that were running. A unit that
/// fails to stop starts the stopped ones again.
async fn stop_all(
    units: &dyn Units,
    names: &[String],
) -> std::result::Result<Vec<String>, Failure> {
    let mut running = Vec::new();
    for unit in names {
        match units.is_active(unit).await {
            Ok(true) => running.push(unit.clone()),
            Ok(false) => {}
            Err(error) => return Err(restart(units, &running, Failure::at("stop", error)).await),
        }
        if let Err(error) = units.stop(unit).await {
            return Err(restart(units, &running, Failure::at("stop", error)).await);
        }
    }
    Ok(running)
}

/// Write the outbound flag, start `running` (last stopped first), and prove
/// each started with the flag set; a unit that did not is stopped again, with
/// all the others.
async fn start_flagged(
    units: &dyn Units,
    plan: &Plan,
    running: &[String],
) -> std::result::Result<Vec<(String, u32)>, Failure> {
    flag::write(&plan.flag, &plan.flag_var, plan.flag_gid)
        .map_err(|error| Failure::at("outbound flag", error))?;
    for unit in running.iter().rev() {
        if let Err(error) = units.start(unit).await {
            return Err(stop_again(units, &plan.units, Failure::at("start", error)).await);
        }
    }
    match flag::prove(units, running, &plan.flag_var).await {
        Ok(proof) => Ok(proof),
        Err(error) => {
            Err(stop_again(units, &plan.units, Failure::at("outbound flag", error)).await)
        }
    }
}

/// Stop every unit again after a start that cannot be trusted.
async fn stop_again(units: &dyn Units, names: &[String], mut failure: Failure) -> Failure {
    for unit in names {
        if let Err(error) = units.stop(unit).await {
            let _ = write!(
                failure.message,
                "; and stopping {unit} again failed: {error}"
            );
        }
    }
    failure
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

/// `<state_dir>/refreshes/<fraise>-<environment>.json`.
fn receipt_path(state_dir: &Path, fraise: &str, environment: &str) -> PathBuf {
    state_dir
        .join("refreshes")
        .join(format!("{fraise}-{environment}.json"))
}

/// Write `receipt`, replacing the previous one only once the new one is
/// complete.
fn write_receipt(
    state_dir: &Path,
    fraise: &str,
    environment: &str,
    receipt: &Value,
) -> std::io::Result<()> {
    let path = receipt_path(state_dir, fraise, environment);
    let dir = path.parent().unwrap_or(state_dir);
    std::fs::create_dir_all(dir)?;
    let partial = dir.join(format!(".{fraise}-{environment}.json.partial"));
    std::fs::write(
        &partial,
        serde_json::to_vec_pretty(receipt).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&partial, &path)
}

/// `db refresh --clear-flag`: remove the outbound flag file, recording who
/// cleared it and when. It never restarts a unit: the next start loads the
/// environment without the flag.
pub(crate) fn db_refresh_clear_flag(config_path: &Path, state_dir: &Path) -> Result<CommandOutput> {
    clear_flag_with(config_path, state_dir, Path::new("/etc"), SystemTime::now())
}

/// [`db_refresh_clear_flag`] with the flag's root and the clock supplied.
pub(crate) fn clear_flag_with(
    config_path: &Path,
    state_dir: &Path,
    etc: &Path,
    now: SystemTime,
) -> Result<CommandOutput> {
    let config = commands::load(config_path)?;
    if let Some(refusal) = refuse_as_target(&config, "clear the outbound flag") {
        return Ok(refusal);
    }
    let deploy = config.deploy.as_ref().context("[deploy] section")?;
    let fraise = deploy.name.clone().unwrap_or_default();
    let environment = deploy.environment.clone().unwrap_or_default();
    let flag = flag::flag_path(etc, &fraise, &environment);
    let removed = match flag::clear(&flag) {
        Ok(removed) => removed,
        Err(error) => return Ok(refuse(&error)),
    };
    let who = std::env::var("SUDO_USER")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".to_owned());
    let uid = current_uid();
    let when = UtcStamp::from_system_time(now).as_str().to_owned();
    let record = json!({ "by": who, "uid": uid, "at": when, "removed": removed });
    let line = if removed {
        format!(
            "{who} (uid {uid}) cleared the outbound flag {} at {when}; units started from now \
             on run without it (restart them to apply)",
            flag.display()
        )
    } else {
        format!(
            "there is no outbound flag at {}; nothing to clear",
            flag.display()
        )
    };
    eprintln!("{line}");
    let path = receipt_path(state_dir, &fraise, &environment);
    let mut receipt = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or_else(|| json!({ "fraise": fraise, "environment": environment }));
    receipt["flag_cleared"] = record.clone();
    let mut pretty = format!("{line}\n");
    if let Err(error) = write_receipt(state_dir, &fraise, &environment, &receipt) {
        let _ = writeln!(
            pretty,
            "warning: the refresh receipt was not updated: {error}"
        );
    }
    Ok(CommandOutput {
        exit_code: 0,
        pretty,
        json: json!({ "ok": true, "flag": flag.display().to_string(), "flag_cleared": record }),
    })
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
