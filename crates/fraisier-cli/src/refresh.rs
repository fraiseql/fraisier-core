//! `fraisier db refresh` and `fraisier db mark-refresh-target`: the staging
//! refresh, over [`fraisier_core::refresh`].

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use fraisier_config::{DeployConfig, RefreshSection};
use fraisier_core::refresh::{
    marker_value, run_refresh, RefreshDb, RefreshPlan, RefreshReport, RefreshService,
};
use fraisier_core::restore_rehearsal::DATABASE_URL_SECRET;
use fraisier_db::archive::Archive;
use fraisier_db::PgConn;
use fraisier_saga::state_store::{FilesystemStateStore, FraiseKey, StateStore, StateStoreError};
use serde_json::{json, Value};

use crate::commands::{self, CommandOutput};
use crate::factory;

/// The database server a refresh runs against: a [`RefreshDb`] that can also
/// size the target and set its marker.
#[async_trait]
pub(crate) trait RefreshServer: RefreshDb {
    /// The target database's size on disk, in bytes.
    async fn target_bytes(&self, target_dsn: &str) -> Result<u64, String>;

    /// Set the target database's refresh marker to `marker`.
    async fn set_marker(&self, target_dsn: &str, marker: &str) -> Result<(), String>;
}

/// How `db refresh` was invoked.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RefreshOptions {
    /// `--yes`: refresh. Without it, only the plan is shown.
    pub(crate) execute: bool,
    /// `--skip-if-locked`: exit 0 when a deploy holds the lock.
    pub(crate) skip_if_locked: bool,
}

/// A step that failed, and why.
type StepError = (&'static str, String);

const fn refuse(pretty: String, json: Value) -> CommandOutput {
    CommandOutput {
        exit_code: 1,
        pretty,
        json,
    }
}

/// The config, its `[refresh]` section, and the fraise/environment it names, or
/// the output refusing to go on.
fn refresh_config(
    config_path: &Path,
    verb: &str,
) -> Result<std::result::Result<(DeployConfig, RefreshSection, String, String), CommandOutput>> {
    let config = commands::load(config_path)?;
    if let Some(refusal) = commands::refuse_invalid_db_config(&config, verb) {
        return Ok(Err(refusal));
    }
    let Some(refresh) = config.refresh.clone() else {
        return Ok(Err(refuse(
            format!("refusing to {verb}: this config has no [refresh] section\n"),
            json!({ "ok": false, "error": "no [refresh] section" }),
        )));
    };
    let deploy = config.deploy.as_ref().context("[deploy] section")?;
    let fraise = deploy.name.clone().unwrap_or_default();
    let environment = deploy.environment.clone().unwrap_or_default();
    Ok(Ok((config, refresh, fraise, environment)))
}

/// `db refresh`: replace this environment's database with the newest archive of
/// `[refresh].source_environment`, then migrate it forward.
///
/// Before anything changes it lists the archive, checks its age and the free
/// space, takes the deploy lock, and checks the target's marker, the source
/// environment and the archive's extensions. Every run that gets past loading
/// the config with `--yes` writes a receipt to `<state_dir>/refreshes/`.
pub(crate) async fn db_refresh(
    config_path: &Path,
    state_dir: &Path,
    options: RefreshOptions,
) -> Result<CommandOutput> {
    db_refresh_with(
        config_path,
        state_dir,
        options,
        &PgRefreshDb,
        &fraisier_db::archive::ArchiveTools::default().pg_restore,
        SystemTime::now(),
    )
    .await
}

/// [`db_refresh`] with the server, `pg_restore` and the clock supplied by the
/// caller.
pub(crate) async fn db_refresh_with(
    config_path: &Path,
    state_dir: &Path,
    options: RefreshOptions,
    server: &dyn RefreshServer,
    pg_restore: &Path,
    now: SystemTime,
) -> Result<CommandOutput> {
    let (config, refresh, fraise, environment) = match refresh_config(config_path, "refresh")? {
        Ok(parts) => parts,
        Err(refusal) => return Ok(refusal),
    };
    if !options.execute {
        return Ok(plan(
            &config,
            &refresh,
            (&fraise, &environment),
            server,
            pg_restore,
            now,
        )
        .await);
    }

    let store = FilesystemStateStore::new(state_dir)
        .with_context(|| format!("opening state store at {}", state_dir.display()))?;
    let key = FraiseKey::new(&fraise, &environment);
    let guard = match store.acquire_lock(&key).await {
        Ok(guard) => guard,
        Err(StateStoreError::Locked { .. }) if options.skip_if_locked => {
            return Ok(CommandOutput {
                exit_code: 0,
                pretty: format!(
                    "skipped: a deploy of {key} holds the deploy lock; the refresh runs next time\n"
                ),
                json: json!({ "ok": true, "skipped": "deploy lock held" }),
            });
        }
        Err(StateStoreError::Locked { .. }) => {
            return Ok(refuse(
                format!(
                    "a deploy of {key} holds the deploy lock; refusing to refresh while it runs \
                     (try again once it finishes, or pass --skip-if-locked)\n"
                ),
                json!({ "ok": false, "error": "deploy lock held" }),
            ));
        }
        Err(error) => return Err(error).context("taking the deploy lock"),
    };

    let mut archive = None;
    let outcome = refresh_steps(
        &config,
        &refresh,
        (&fraise, &environment),
        server,
        pg_restore,
        now,
        &mut archive,
    )
    .await;
    store
        .release_lock(guard)
        .await
        .context("releasing the deploy lock")?;

    let (exit_code, mut pretty, receipt) = rendered(
        &outcome,
        archive.as_ref(),
        (&fraise, &environment),
        &refresh,
        now,
    );
    if let Err(error) = write_receipt(state_dir, &fraise, &environment, &receipt) {
        let _ = writeln!(
            pretty,
            "warning: the refresh receipt was not written: {error}"
        );
    }
    Ok(CommandOutput {
        exit_code,
        pretty,
        json: receipt,
    })
}

/// A refresh's exit code, rendering and receipt.
fn rendered(
    outcome: &std::result::Result<RefreshReport, StepError>,
    archive: Option<&Archive>,
    (fraise, environment): (&str, &str),
    refresh: &RefreshSection,
    now: SystemTime,
) -> (i32, String, Value) {
    let mut receipt = json!({
        "fraise": fraise,
        "environment": environment,
        "source_environment": refresh.source_environment,
        "started_at": fraisier_db::archive::UtcStamp::from_system_time(now).as_str(),
        "archive": archive.map(|a| a.path.display().to_string()),
        "archive_taken_at": archive.map(|a| a.stamp.as_str().to_owned()),
        "ok": outcome.is_ok(),
    });
    let (exit_code, pretty) = match outcome {
        Ok(report) => {
            receipt["applied"] = json!(report
                .applied
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>());
            receipt["tables"] = json!(report.tables);
            (
                0,
                format!(
                    "refreshed {fraise}/{environment} from {}: applied {} migration(s), found {} \
                     table(s)\n",
                    archive.map_or_else(String::new, |a| a.path.display().to_string()),
                    report.applied.len(),
                    report.tables
                ),
            )
        }
        Err((step, message)) => {
            let message = fraisier_saga::redact::credentials(message);
            receipt["failed_step"] = json!(step);
            receipt["error"] = json!(message);
            (1, format!("refresh failed at {step}: {message}\n"))
        }
    };
    (exit_code, pretty, receipt)
}

/// The newest archive of the source environment, after checking it reads back
/// and is fresh enough, with the extensions its table of contents creates.
async fn chosen_archive(
    refresh: &RefreshSection,
    fraise: &str,
    pg_restore: &Path,
    now: SystemTime,
    archive: &mut Option<Archive>,
) -> std::result::Result<Vec<String>, StepError> {
    let dir = refresh.source_dir.clone().unwrap_or_default();
    let source = refresh.source_environment.clone().unwrap_or_default();
    let newest = fraisier_db::archive::latest_archive(&dir, fraise, &source)
        .map_err(|error| ("listing", format!("cannot read {}: {error}", dir.display())))?
        .ok_or_else(|| {
            (
                "listing",
                format!("no archive of {fraise}/{source} in {}", dir.display()),
            )
        })?;
    let path = archive.insert(newest).path.clone();

    let mut list = Command::new(pg_restore);
    list.arg("--list").arg(&path);
    let listed = fraisier_db::run(list)
        .await
        .map_err(|error| ("listing", format!("cannot run pg_restore --list: {error}")))?;
    if !listed.succeeded() {
        return Err((
            "listing",
            format!(
                "{} is not a readable archive: {}",
                path.display(),
                listed.stderr.trim()
            ),
        ));
    }

    let taken_at = archive.as_ref().map_or(now, |a| a.stamp.to_system_time());
    let max_age = std::time::Duration::from_hours(u64::from(refresh.max_age_hours()));
    if let Ok(age) = now.duration_since(taken_at) {
        if age > max_age {
            return Err((
                "age",
                format!(
                    "the newest archive, {}, is {} hours old: older than max_age_hours = {}, so \
                     {source}'s backups have stopped arriving",
                    path.display(),
                    age.as_secs() / 3_600,
                    refresh.max_age_hours()
                ),
            ));
        }
    }
    Ok(fraisier_db::refresh::archive_extensions(&listed.stdout))
}

/// The target DSN and the migration adapter's context, from the config.
fn resolve(config: &DeployConfig) -> std::result::Result<factory::ResolvedMigration, StepError> {
    factory::build_migration_only(config).map_err(|error| ("config", format!("{error:#}")))
}

fn target_dsn(resolved: &factory::ResolvedMigration) -> std::result::Result<String, StepError> {
    resolved.ctx.secret(DATABASE_URL_SECRET).map_err(|error| {
        (
            "config",
            format!("the database DSN is unavailable: {error}"),
        )
    })
}

/// The refresh's steps up to and including [`run_refresh`]. `archive` is set
/// once the newest archive is known.
async fn refresh_steps(
    config: &DeployConfig,
    refresh: &RefreshSection,
    (fraise, environment): (&str, &str),
    server: &dyn RefreshServer,
    pg_restore: &Path,
    now: SystemTime,
    archive: &mut Option<Archive>,
) -> std::result::Result<RefreshReport, StepError> {
    let extensions = chosen_archive(refresh, fraise, pg_restore, now, archive).await?;
    let resolved = resolve(config)?;
    let target = target_dsn(&resolved)?;
    if let Some(data_dir) = refresh.data_dir.as_deref() {
        room(server, &target, data_dir).await?;
    }
    let service =
        factory::build_refresh_service(config).map_err(|error| ("config", format!("{error:#}")))?;
    let source = refresh.source_environment.clone().unwrap_or_default();
    let path = archive.as_ref().map(|a| a.path.clone()).unwrap_or_default();
    run_refresh(
        server,
        service.as_ref().map(|(adapter, host)| RefreshService {
            adapter: adapter.as_ref(),
            ctx: &resolved.ctx,
            host,
        }),
        resolved.migration.as_ref(),
        &resolved.ctx,
        RefreshPlan {
            fraise,
            environment,
            archive: &path,
            archive_environment: &source,
            min_tables: refresh.min_tables,
            extensions: &extensions,
        },
    )
    .await
    .map_err(|error| (error.step(), error.to_string()))
}

/// Refuse when the filesystem holding `data_dir` cannot take the target's
/// current size plus 2 GB. A measurement that fails refuses too.
async fn room(
    server: &dyn RefreshServer,
    target_dsn: &str,
    data_dir: &Path,
) -> std::result::Result<(), StepError> {
    const HEADROOM: u64 = 2_000_000_000;
    let target = server.target_bytes(target_dsn).await.map_err(|error| {
        (
            "free space",
            format!("could not size the target database: {error}"),
        )
    })?;
    let free = fraisier_db::retention::available_bytes(data_dir).map_err(|error| {
        (
            "free space",
            format!(
                "could not measure free space in {}: {error}",
                data_dir.display()
            ),
        )
    })?;
    let needed = target.saturating_add(HEADROOM);
    if free < needed {
        return Err((
            "free space",
            format!(
                "{} has {} GB free; the refreshed database needs about {} GB (its current \
                 size plus 2 GB)",
                data_dir.display(),
                free / 1_000_000_000,
                needed.div_ceil(1_000_000_000)
            ),
        ));
    }
    Ok(())
}

/// What a refresh would do, read-only: the archive it would restore and the
/// marker it would check.
async fn plan(
    config: &DeployConfig,
    refresh: &RefreshSection,
    (fraise, environment): (&str, &str),
    server: &dyn RefreshServer,
    pg_restore: &Path,
    now: SystemTime,
) -> CommandOutput {
    let mut archive = None;
    let chosen = chosen_archive(refresh, fraise, pg_restore, now, &mut archive).await;
    let target = resolve(config).and_then(|resolved| target_dsn(&resolved));
    let mut pretty = format!("refresh plan for {fraise}/{environment}:\n");
    let mut ok = true;
    match (&chosen, archive.as_ref()) {
        (Ok(_), Some(archive)) => {
            let _ = writeln!(pretty, "  archive: {}", archive.path.display());
        }
        (Err((step, message)), _) => {
            ok = false;
            let _ = writeln!(pretty, "  archive: refused at {step}: {message}");
        }
        (Ok(_), None) => {}
    }
    match &target {
        Ok(dsn) => {
            let shown =
                PgConn::parse(dsn).map_or_else(|_| "<unparseable>".to_owned(), |c| c.redacted());
            let _ = writeln!(pretty, "  target:  {shown}");
            let expected = marker_value(fraise, environment);
            let marker = match server.marker(dsn).await {
                Ok(Some(found)) if found == expected => format!("ok ({found})"),
                Ok(Some(found)) => {
                    ok = false;
                    format!("marked for {found}, not {expected}: the refresh would be refused")
                }
                Ok(None) => {
                    ok = false;
                    "missing: the refresh would be refused (fraisier db mark-refresh-target)"
                        .to_owned()
                }
                Err(error) => {
                    ok = false;
                    format!("could not be read: {error}")
                }
            };
            let _ = writeln!(pretty, "  marker:  {marker}");
        }
        Err((_, message)) => {
            ok = false;
            let _ = writeln!(pretty, "  target:  {message}");
        }
    }
    let _ = writeln!(
        pretty,
        "  then: stop the service, drop the database, create it, restore, migrate, count, \
         start the service\npass --yes to execute: this REPLACES the {environment} database"
    );
    CommandOutput {
        exit_code: 0,
        pretty: fraisier_saga::redact::credentials(&pretty),
        json: json!({
            "executed": false,
            "ready": ok,
            "archive": archive.as_ref().map(|a| a.path.display().to_string()),
        }),
    }
}

/// Write `receipt` to `<state_dir>/refreshes/<fraise>-<environment>.json`,
/// replacing the previous one only once the new one is complete.
fn write_receipt(
    state_dir: &Path,
    fraise: &str,
    environment: &str,
    receipt: &Value,
) -> std::io::Result<PathBuf> {
    let dir = state_dir.join("refreshes");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{fraise}-{environment}.json"));
    let partial = dir.join(format!(".{fraise}-{environment}.json.partial"));
    let body = serde_json::to_vec_pretty(receipt).map_err(std::io::Error::other)?;
    std::fs::write(&partial, body)?;
    std::fs::rename(&partial, &path)?;
    Ok(path)
}

/// `db mark-refresh-target`: mark this config's database as its refresh
/// target. A database already marked for another config is refused; clearing
/// that marker is a deliberate act, by hand.
pub(crate) async fn db_mark_refresh_target(config_path: &Path) -> Result<CommandOutput> {
    db_mark_refresh_target_with(config_path, &PgRefreshDb).await
}

/// [`db_mark_refresh_target`] with the server supplied by the caller.
pub(crate) async fn db_mark_refresh_target_with(
    config_path: &Path,
    server: &dyn RefreshServer,
) -> Result<CommandOutput> {
    let (config, _, fraise, environment) =
        match refresh_config(config_path, "mark a refresh target")? {
            Ok(parts) => parts,
            Err(refusal) => return Ok(refusal),
        };
    let conn = match commands::resolve_pg_conn(&config) {
        Ok(conn) => conn,
        Err(error) => return Ok(commands::db_config_error(&error)),
    };
    let dsn = conn.dsn();
    let expected = marker_value(&fraise, &environment);
    let failed = |message: String| {
        let message = fraisier_saga::redact::credentials(&message);
        refuse(
            format!("{message}\n"),
            json!({ "ok": false, "error": message }),
        )
    };
    match server.marker(&dsn).await {
        Err(error) => Ok(failed(format!(
            "could not read the refresh marker: {error}"
        ))),
        Ok(Some(found)) if found == expected => Ok(CommandOutput {
            exit_code: 0,
            pretty: format!(
                "{} is already the refresh target of {expected}\n",
                conn.database()
            ),
            json: json!({ "ok": true, "marker": expected, "changed": false }),
        }),
        Ok(Some(found)) => Ok(failed(format!(
            "{} is already marked as the refresh target of '{found}'; refusing to re-mark it \
             for '{expected}'. If that is intended, clear it first: ALTER DATABASE \"{}\" RESET \
             {}",
            conn.database(),
            conn.database(),
            fraisier_db::refresh::MARKER_SETTING
        ))),
        Ok(None) => match server.set_marker(&dsn, &expected).await {
            Ok(()) => Ok(CommandOutput {
                exit_code: 0,
                pretty: format!(
                    "marked {} as the refresh target of {expected}: `fraisier db refresh` may \
                     now replace it\n",
                    conn.database()
                ),
                json: json!({ "ok": true, "marker": expected, "changed": true }),
            }),
            Err(error) => Ok(failed(format!("could not set the refresh marker: {error}"))),
        },
    }
}

/// The refresh's database operations, through `psql` and `pg_restore` with the
/// DSN in `PG*` variables. Drop and create connect through the server's
/// `postgres` maintenance database, as the target's role.
pub(crate) struct PgRefreshDb;

fn parse(dsn: &str) -> std::result::Result<PgConn, String> {
    PgConn::parse(dsn).map_err(|error| format!("invalid database DSN: {error}"))
}

async fn psql(conn: &PgConn, sql: &str) -> std::result::Result<String, String> {
    crate::pg_rehearsal::scalar(conn, sql).await
}

async fn run_ok(command: Command, what: &str) -> std::result::Result<(), String> {
    let outcome = fraisier_db::run(command)
        .await
        .map_err(|error| error.to_string())?;
    if outcome.succeeded() {
        Ok(())
    } else {
        Err(format!("{what} failed: {}", outcome.stderr.trim()))
    }
}

#[async_trait]
impl RefreshDb for PgRefreshDb {
    async fn marker(&self, target_dsn: &str) -> std::result::Result<Option<String>, String> {
        let target = parse(target_dsn)?;
        let sql = fraisier_db::refresh::marker_sql(target.database()).map_err(|e| e.to_string())?;
        let stdout = psql(&target.with_database("postgres"), &sql).await?;
        Ok(fraisier_db::refresh::parse_marker(&stdout))
    }

    async fn extensions_problem(
        &self,
        target_dsn: &str,
        extensions: &[String],
    ) -> std::result::Result<Option<String>, String> {
        if extensions.is_empty() {
            return Ok(None);
        }
        let target = parse(target_dsn)?;
        let stdout = psql(
            &target.with_database("postgres"),
            &fraisier_db::refresh::extensions_sql(extensions),
        )
        .await?;
        fraisier_db::refresh::extensions_problem(&stdout)
    }

    async fn drop_target(&self, target_dsn: &str) -> std::result::Result<(), String> {
        let target = parse(target_dsn)?;
        let drop = fraisier_db::drop_database_command(
            &target.with_database("postgres"),
            target.database(),
        )
        .map_err(|e| e.to_string())?;
        run_ok(drop, "DROP DATABASE").await
    }

    async fn create_target(
        &self,
        target_dsn: &str,
        marker: &str,
    ) -> std::result::Result<(), String> {
        let target = parse(target_dsn)?;
        let maintenance = target.with_database("postgres");
        let create = fraisier_db::create_database_command(&maintenance, target.database())
            .map_err(|e| e.to_string())?;
        run_ok(create, "CREATE DATABASE").await?;
        let mark =
            fraisier_db::refresh::set_marker_command(&maintenance, target.database(), marker)
                .map_err(|e| e.to_string())?;
        run_ok(mark, "setting the refresh marker").await
    }

    async fn restore(&self, target_dsn: &str, archive: &Path) -> std::result::Result<(), String> {
        let target = parse(target_dsn)?;
        run_ok(
            fraisier_db::refresh::restore_command(&target, archive),
            "pg_restore",
        )
        .await
    }

    async fn table_count(&self, target_dsn: &str) -> std::result::Result<u64, String> {
        let target = parse(target_dsn)?;
        let count = psql(
            &target,
            "SELECT count(*) FROM pg_catalog.pg_tables \
             WHERE schemaname NOT IN ('pg_catalog', 'information_schema')",
        )
        .await?;
        count
            .parse()
            .map_err(|_| format!("the table count was not a number: {count:?}"))
    }
}

#[async_trait]
impl RefreshServer for PgRefreshDb {
    async fn target_bytes(&self, target_dsn: &str) -> std::result::Result<u64, String> {
        let target = parse(target_dsn)?;
        let size = psql(&target, "SELECT pg_database_size(current_database())").await?;
        size.parse()
            .map_err(|_| format!("the database size was not a number: {size:?}"))
    }

    async fn set_marker(&self, target_dsn: &str, marker: &str) -> std::result::Result<(), String> {
        let target = parse(target_dsn)?;
        let mark = fraisier_db::refresh::set_marker_command(
            &target.with_database("postgres"),
            target.database(),
            marker,
        )
        .map_err(|e| e.to_string())?;
        run_ok(mark, "setting the refresh marker").await
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use async_trait::async_trait;
    use fraisier_core::refresh::RefreshDb;
    use fraisier_saga::state_store::{FilesystemStateStore, FraiseKey, StateStore};

    use super::{db_mark_refresh_target_with, db_refresh_with, RefreshOptions, RefreshServer};
    use crate::commands::CommandOutput;
    use crate::test_env::block_on;

    const DSN_VAR: &str = "FRAISIER_REFRESH_TEST_DSN";
    const DSN: &str = "postgres://app:s3cret@127.0.0.1/demo_staging";

    #[derive(Default)]
    struct FakeServer {
        calls: Mutex<Vec<String>>,
        marker: Mutex<Option<String>>,
        tables: u64,
        target_bytes: u64,
    }

    impl FakeServer {
        fn marked() -> Self {
            Self {
                marker: Mutex::new(Some("demo/staging".to_owned())),
                tables: 7,
                ..Self::default()
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls").clone()
        }
        fn record(&self, call: impl Into<String>) {
            self.calls.lock().expect("calls").push(call.into());
        }
    }

    #[async_trait]
    impl RefreshDb for FakeServer {
        async fn marker(&self, _target_dsn: &str) -> Result<Option<String>, String> {
            self.record("marker");
            Ok(self.marker.lock().expect("marker").clone())
        }
        async fn extensions_problem(
            &self,
            _target_dsn: &str,
            extensions: &[String],
        ) -> Result<Option<String>, String> {
            self.record(format!("extensions:{}", extensions.join(",")));
            Ok(None)
        }
        async fn drop_target(&self, _target_dsn: &str) -> Result<(), String> {
            self.record("drop");
            Ok(())
        }
        async fn create_target(&self, _target_dsn: &str, marker: &str) -> Result<(), String> {
            self.record(format!("create:{marker}"));
            Ok(())
        }
        async fn restore(&self, _target_dsn: &str, archive: &Path) -> Result<(), String> {
            self.record(format!("restore:{}", archive.display()));
            Ok(())
        }
        async fn table_count(&self, _target_dsn: &str) -> Result<u64, String> {
            self.record("count");
            Ok(self.tables)
        }
    }

    #[async_trait]
    impl RefreshServer for FakeServer {
        async fn target_bytes(&self, _target_dsn: &str) -> Result<u64, String> {
            Ok(self.target_bytes)
        }
        async fn set_marker(&self, _target_dsn: &str, marker: &str) -> Result<(), String> {
            self.record(format!("set_marker:{marker}"));
            *self.marker.lock().expect("marker") = Some(marker.to_owned());
            Ok(())
        }
    }

    /// A fake `pg_restore`: `--list` of a file starting `PGDMP` prints the rest
    /// of it as the table of contents; anything else is not an archive.
    fn fake_pg_restore() -> &'static Path {
        use std::os::unix::fs::PermissionsExt;
        static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        PATH.get_or_init(|| {
            let dir =
                std::env::temp_dir().join(format!("fraisier-refresh-pg-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("mkdir");
            let path = dir.join("pg_restore");
            std::fs::write(
                &path,
                "#!/bin/sh\n[ \"$1\" = --probe ] && exit 0\n\
                 [ \"$(head -c 5 \"$2\")\" = PGDMP ] || { echo 'pg_restore: error: input file \
                 does not appear to be a valid archive' >&2; exit 1; }\n\
                 tail -n +2 \"$2\"\n",
            )
            .expect("write fake");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            for _ in 0..200 {
                match std::process::Command::new(&path).arg("--probe").output() {
                    Ok(_) => return path,
                    Err(error) if error.raw_os_error() == Some(26) => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => panic!("fake is not executable: {error}"),
                }
            }
            panic!("fake stayed ETXTBSY");
        })
    }

    /// 2026-10-26T12:00:00Z.
    fn now() -> std::time::SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_hours(498_060)
    }

    fn config(dir: &Path, extra: &str) -> PathBuf {
        let source = dir.join("source");
        std::fs::create_dir_all(&source).expect("mkdir");
        let path = dir.join("fraisier.toml");
        std::fs::write(
            &path,
            format!(
                "[deploy]\nname = \"demo\"\nenvironment = \"staging\"\n\n\
                 [migration]\nadapter = \"command\"\ndatabase_url_env = \"{DSN_VAR}\"\n\n\
                 [migration.settings.commands]\nup = \"true\"\ncurrent_revision = \"true\"\n\n\
                 [refresh]\nsource_dir = \"{}\"\nsource_environment = \"production\"\n{extra}",
                source.display()
            ),
        )
        .expect("write config");
        path
    }

    fn archive(dir: &Path, environment: &str, stamp: &str, body: &str) -> PathBuf {
        let path = dir.join(format!("source/demo-{environment}-{stamp}.pgdump"));
        std::fs::write(&path, body).expect("write archive");
        path
    }

    fn refresh(
        dir: &Path,
        config: &Path,
        server: &FakeServer,
        options: RefreshOptions,
    ) -> CommandOutput {
        let _guard = crate::test_env::env_guard();
        std::env::set_var(DSN_VAR, DSN);
        let out = block_on(db_refresh_with(
            config,
            &dir.join("state"),
            options,
            server,
            fake_pg_restore(),
            now(),
        ))
        .expect("run");
        std::env::remove_var(DSN_VAR);
        out
    }

    const YES: RefreshOptions = RefreshOptions {
        execute: true,
        skip_if_locked: false,
    };

    fn receipt(dir: &Path) -> serde_json::Value {
        let text = std::fs::read_to_string(dir.join("state/refreshes/demo-staging.json"))
            .expect("a receipt");
        serde_json::from_str(&text).expect("json")
    }

    #[test]
    fn a_refresh_restores_the_newest_source_archive_and_writes_a_receipt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config(dir.path(), "min_tables = 5\n");
        archive(dir.path(), "production", "20261025T020000Z", "PGDMP\n");
        let newest = archive(
            dir.path(),
            "production",
            "20261026T020000Z",
            "PGDMP\n2; 3079 16390 EXTENSION - pg_tviews\n",
        );
        // Another environment's newer archive is never chosen.
        archive(dir.path(), "staging", "20261026T030000Z", "PGDMP\n");
        let server = FakeServer::marked();
        let out = refresh(dir.path(), &config, &server, YES);
        assert_eq!(out.exit_code, 0, "{}", out.pretty);
        assert_eq!(
            server.calls(),
            vec![
                "marker".to_owned(),
                "extensions:pg_tviews".to_owned(),
                "drop".to_owned(),
                "create:demo/staging".to_owned(),
                format!("restore:{}", newest.display()),
                "count".to_owned(),
            ]
        );
        let receipt = receipt(dir.path());
        assert_eq!(receipt["ok"], serde_json::json!(true));
        assert_eq!(
            receipt["archive"],
            serde_json::json!(newest.display().to_string())
        );
        assert_eq!(receipt["tables"], serde_json::json!(7));
        assert!(!out.pretty.contains("s3cret"), "{}", out.pretty);
    }

    #[test]
    fn a_corrupt_archive_fails_at_listing_before_anything_is_asked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config(dir.path(), "");
        archive(dir.path(), "production", "20261026T020000Z", "BAD");
        let server = FakeServer::marked();
        let out = refresh(dir.path(), &config, &server, YES);
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert_eq!(server.calls(), Vec::<String>::new());
        assert!(
            out.pretty.contains("not a readable archive"),
            "{}",
            out.pretty
        );
        assert_eq!(
            receipt(dir.path())["failed_step"],
            serde_json::json!("listing")
        );
    }

    #[test]
    fn a_stale_archive_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config(dir.path(), "max_age_hours = 24\n");
        archive(dir.path(), "production", "20261024T020000Z", "PGDMP\n");
        let server = FakeServer::marked();
        let out = refresh(dir.path(), &config, &server, YES);
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert_eq!(server.calls(), Vec::<String>::new());
        assert!(out.pretty.contains("max_age_hours"), "{}", out.pretty);
    }

    #[test]
    fn no_source_archive_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config(dir.path(), "");
        archive(dir.path(), "staging", "20261026T020000Z", "PGDMP\n");
        let server = FakeServer::marked();
        let out = refresh(dir.path(), &config, &server, YES);
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(
            out.pretty.contains("no archive of demo/production"),
            "{}",
            out.pretty
        );
        assert_eq!(server.calls(), Vec::<String>::new());
    }

    #[test]
    fn too_little_free_space_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config(
            dir.path(),
            &format!("data_dir = \"{}\"\n", dir.path().display()),
        );
        archive(dir.path(), "production", "20261026T020000Z", "PGDMP\n");
        let server = FakeServer {
            target_bytes: u64::MAX / 2,
            ..FakeServer::marked()
        };
        let out = refresh(dir.path(), &config, &server, YES);
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(out.pretty.contains("free"), "{}", out.pretty);
        assert_eq!(server.calls(), Vec::<String>::new());
    }

    #[test]
    fn an_unmarked_target_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config(dir.path(), "");
        archive(dir.path(), "production", "20261026T020000Z", "PGDMP\n");
        let server = FakeServer::default();
        let out = refresh(dir.path(), &config, &server, YES);
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert_eq!(server.calls(), vec!["marker".to_owned()]);
        assert!(out.pretty.contains("mark-refresh-target"), "{}", out.pretty);
        assert_eq!(
            receipt(dir.path())["failed_step"],
            serde_json::json!("marker")
        );
    }

    #[test]
    fn a_target_marked_for_another_config_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config(dir.path(), "");
        archive(dir.path(), "production", "20261026T020000Z", "PGDMP\n");
        let server = FakeServer {
            marker: Mutex::new(Some("demo/qa".to_owned())),
            ..FakeServer::default()
        };
        let out = refresh(dir.path(), &config, &server, YES);
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert_eq!(server.calls(), vec!["marker".to_owned()]);
        assert!(out.pretty.contains("demo/qa"), "{}", out.pretty);
    }

    #[test]
    fn without_yes_only_the_plan_is_shown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config(dir.path(), "");
        let newest = archive(dir.path(), "production", "20261026T020000Z", "PGDMP\n");
        let server = FakeServer::marked();
        let out = refresh(dir.path(), &config, &server, RefreshOptions::default());
        assert_eq!(out.exit_code, 0, "{}", out.pretty);
        assert_eq!(server.calls(), vec!["marker".to_owned()]);
        assert!(
            out.pretty.contains(&newest.display().to_string()),
            "{}",
            out.pretty
        );
        assert!(out.pretty.contains("--yes"), "{}", out.pretty);
        assert!(!out.pretty.contains("s3cret"), "{}", out.pretty);
    }

    #[test]
    fn a_held_deploy_lock_refuses_and_skip_if_locked_skips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config(dir.path(), "");
        archive(dir.path(), "production", "20261026T020000Z", "PGDMP\n");
        let store = FilesystemStateStore::new(dir.path().join("state")).expect("store");
        let held = block_on(store.acquire_lock(&FraiseKey::new("demo", "staging"))).expect("lock");

        let server = FakeServer::marked();
        let out = refresh(dir.path(), &config, &server, YES);
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(out.pretty.contains("deploy lock"), "{}", out.pretty);
        assert_eq!(server.calls(), Vec::<String>::new());

        let skip = RefreshOptions {
            skip_if_locked: true,
            ..YES
        };
        let out = refresh(dir.path(), &config, &server, skip);
        assert_eq!(out.exit_code, 0, "{}", out.pretty);
        assert!(out.pretty.contains("skipped"), "{}", out.pretty);
        assert_eq!(server.calls(), Vec::<String>::new());
        block_on(store.release_lock(held)).expect("release");
    }

    #[test]
    fn skip_if_locked_does_not_mask_a_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config(dir.path(), "");
        archive(dir.path(), "production", "20261026T020000Z", "BAD");
        let server = FakeServer::marked();
        let skip = RefreshOptions {
            skip_if_locked: true,
            ..YES
        };
        let out = refresh(dir.path(), &config, &server, skip);
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
    }

    #[test]
    fn a_refresh_without_a_refresh_section_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fraisier.toml");
        std::fs::write(
            &path,
            format!(
                "[deploy]\nname = \"demo\"\nenvironment = \"staging\"\n\n\
                 [migration]\nadapter = \"command\"\ndatabase_url_env = \"{DSN_VAR}\"\n"
            ),
        )
        .expect("write");
        let server = FakeServer::marked();
        let out = refresh(dir.path(), &path, &server, YES);
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(out.pretty.contains("[refresh]"), "{}", out.pretty);
    }

    fn mark(config: &Path, server: &FakeServer) -> CommandOutput {
        let _guard = crate::test_env::env_guard();
        std::env::set_var(DSN_VAR, DSN);
        let out = block_on(db_mark_refresh_target_with(config, server)).expect("run");
        std::env::remove_var(DSN_VAR);
        out
    }

    #[test]
    fn marking_sets_the_marker_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config(dir.path(), "");
        let server = FakeServer::default();
        let out = mark(&config, &server);
        assert_eq!(out.exit_code, 0, "{}", out.pretty);
        assert_eq!(
            server.calls(),
            vec!["marker".to_owned(), "set_marker:demo/staging".to_owned()]
        );
        let out = mark(&config, &server);
        assert_eq!(out.exit_code, 0, "{}", out.pretty);
        assert!(out.pretty.contains("already"), "{}", out.pretty);
        assert_eq!(server.calls().len(), 3, "a second mark sets nothing");
    }

    #[test]
    fn marking_a_database_marked_for_another_config_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config(dir.path(), "");
        let server = FakeServer {
            marker: Mutex::new(Some("demo/qa".to_owned())),
            ..FakeServer::default()
        };
        let out = mark(&config, &server);
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert!(out.pretty.contains("demo/qa"), "{}", out.pretty);
        assert_eq!(server.calls(), vec!["marker".to_owned()]);
    }

    #[test]
    fn marking_needs_a_refresh_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fraisier.toml");
        std::fs::write(
            &path,
            format!(
                "[deploy]\nname = \"demo\"\nenvironment = \"production\"\n\n\
                 [migration]\nadapter = \"command\"\ndatabase_url_env = \"{DSN_VAR}\"\n"
            ),
        )
        .expect("write");
        let server = FakeServer::default();
        let out = mark(&path, &server);
        assert_eq!(out.exit_code, 1, "{}", out.pretty);
        assert_eq!(server.calls(), Vec::<String>::new());
    }

    #[test]
    fn the_marker_setting_is_the_same_in_core_and_db() {
        assert_eq!(
            fraisier_core::refresh::MARKER_SETTING,
            fraisier_db::refresh::MARKER_SETTING
        );
    }
}
