//! The staging refresh: replace an environment's database with the newest
//! archive of another, and migrate it forward.
//!
//! A refresh drops a database, so it is guarded twice before anything changes:
//!
//! 1. **The marker.** The target database must carry
//!    `fraisier.refresh_target = '<fraise>/<environment>'` (a database-level
//!    setting, [`MARKER_SETTING`]) naming the config that refreshes it. A
//!    database nobody marked is never a target, so the guard fails closed: an
//!    environment nobody thought to list is protected. `pg_dump` does not carry
//!    database-level settings, so the marker cannot travel inside an archive to
//!    the database it was taken from.
//! 2. **The source.** The archive's environment is never the target's.
//!
//! Two more checks run before anything changes. The role must be able to set
//! the marker, because the refresh re-applies it to the database it re-creates;
//! without that, it would drop a database it could not mark again. And the role
//! must be able to create every extension the archive creates: a restore that
//! cannot recreate one loads the tables and fails, after the database it
//! replaced is gone.
//!
//! Then the service is stopped, the database dropped, created again with the
//! marker re-applied, restored, migrated, counted, and the service restarted. A
//! failed drop restarts the service. A failure after the drop leaves it
//! stopped: the database it would read is partly restored or not migrated, and
//! serving from it is worse than serving nothing.

use std::path::Path;

use async_trait::async_trait;

use crate::adapter_axes::{
    AdapterCtx, AdapterError, HostId, MigrationAdapter, Revision, ServiceAdapter,
};
use crate::restore_rehearsal::DATABASE_URL_SECRET;

/// The database-level setting that marks a database as a refresh target.
pub const MARKER_SETTING: &str = "fraisier.refresh_target";

/// The marker value naming the config that may refresh a database.
#[must_use]
pub fn marker_value(fraise: &str, environment: &str) -> String {
    format!("{fraise}/{environment}")
}

/// Whether a database carrying `found` may be refreshed as `fraise/environment`.
///
/// # Errors
/// Why it may not: it carries no marker, or one naming another config.
pub fn marker_guard(found: Option<&str>, fraise: &str, environment: &str) -> Result<(), String> {
    let expected = marker_value(fraise, environment);
    match found {
        Some(marker) if marker == expected => Ok(()),
        Some(marker) => Err(format!(
            "the target database is marked as the refresh target of '{marker}', not of \
             '{expected}'"
        )),
        None => Err(format!(
            "the target database carries no {MARKER_SETTING} marker, so it is not a refresh \
             target. If it is this config's staging database, mark it once with `fraisier db \
             mark-refresh-target --config <this config>`"
        )),
    }
}

/// Whether an archive of `archive_environment` may be restored into
/// `target_environment`.
///
/// # Errors
/// They are the same environment.
pub fn source_guard(archive_environment: &str, target_environment: &str) -> Result<(), String> {
    if archive_environment == target_environment {
        return Err(format!(
            "the archive is of '{archive_environment}', the environment being refreshed; a \
             refresh restores another environment's archive"
        ));
    }
    Ok(())
}

/// The database operations a refresh needs, each against the target's DSN.
#[async_trait]
pub trait RefreshDb: Send + Sync {
    /// The [`MARKER_SETTING`] value the target database carries, if any.
    ///
    /// # Errors
    /// The setting could not be read.
    async fn marker(&self, target_dsn: &str) -> Result<Option<String>, String>;

    /// Why the connecting role could not create `extensions` (the ones the
    /// archive creates) in a fresh database: one is not available on the server,
    /// or one is untrusted and the role is not a superuser. `None` when it can.
    ///
    /// # Errors
    /// The server could not be asked.
    async fn extensions_problem(
        &self,
        target_dsn: &str,
        extensions: &[String],
    ) -> Result<Option<String>, String>;

    /// Why the connecting role could not set [`MARKER_SETTING`], which the
    /// refresh re-applies to the database it re-creates; `None` when it can.
    ///
    /// # Errors
    /// The server could not be asked.
    async fn remark_problem(&self, target_dsn: &str) -> Result<Option<String>, String>;

    /// Drop the target database, ending the sessions connected to it.
    ///
    /// # Errors
    /// The drop failed.
    async fn drop_target(&self, target_dsn: &str) -> Result<(), String>;

    /// Create the target database again, owned by the connecting role, and set
    /// [`MARKER_SETTING`] to `marker` on it.
    ///
    /// # Errors
    /// The create or the setting failed.
    async fn create_target(&self, target_dsn: &str, marker: &str) -> Result<(), String>;

    /// Restore `archive` into the (empty) target database.
    ///
    /// # Errors
    /// The restore failed.
    async fn restore(&self, target_dsn: &str, archive: &Path) -> Result<(), String>;

    /// How many ordinary tables the target holds, outside the system schemas.
    ///
    /// # Errors
    /// The count could not be taken.
    async fn table_count(&self, target_dsn: &str) -> Result<u64, String>;
}

/// What a refresh restores, and into what.
#[derive(Debug, Clone, Copy)]
pub struct RefreshPlan<'a> {
    /// The deployable being refreshed.
    pub fraise: &'a str,
    /// The environment being refreshed (the target).
    pub environment: &'a str,
    /// The archive to restore.
    pub archive: &'a Path,
    /// The environment the archive was taken from.
    pub archive_environment: &'a str,
    /// The refreshed, migrated database must hold at least this many tables.
    pub min_tables: Option<u64>,
    /// The extensions the archive creates, read from its table of contents.
    pub extensions: &'a [String],
}

/// The service a refresh stops and restarts, with the context it acts under.
#[derive(Clone, Copy)]
pub struct RefreshService<'a> {
    /// The service adapter.
    pub adapter: &'a dyn ServiceAdapter,
    /// The context carrying the service's settings.
    pub ctx: &'a AdapterCtx,
    /// The host the service runs on.
    pub host: &'a HostId,
}

/// What a successful refresh did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshReport {
    /// The migrations applied on top of the restored archive.
    pub applied: Vec<Revision>,
    /// The tables the refreshed database holds.
    pub tables: u64,
}

/// Why a refresh failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RefreshError {
    /// The target DSN could not be resolved.
    #[error("the refresh could not resolve the database DSN: {0}")]
    Secret(#[source] AdapterError),
    /// The marker could not be read. Nothing was changed.
    #[error("could not read the refresh marker: {0}. Nothing was changed")]
    Marker(String),
    /// The target is not this config's refresh target. Nothing was changed.
    #[error("{0}. Nothing was changed")]
    NotATarget(String),
    /// The archive is of the target's own environment. Nothing was changed.
    #[error("{0}. Nothing was changed")]
    SameEnvironment(String),
    /// The role could not re-apply the marker to the database it re-creates.
    /// Nothing was changed.
    #[error("{0}. Nothing was changed")]
    Remark(String),
    /// The archive creates an extension the role could not create. Nothing was
    /// changed.
    #[error("{0}. Nothing was changed")]
    Extensions(String),
    /// The service could not be stopped. Nothing was changed.
    #[error("the service could not be stopped: {0}. Nothing was changed")]
    Stop(#[source] AdapterError),
    /// The drop failed, so the database is as it was. The service was restarted,
    /// unless `restart` says otherwise.
    #[error("the database could not be dropped: {error}. It is unchanged{restart}")]
    Drop {
        /// Why the drop failed.
        error: String,
        /// Empty when the service came back; otherwise why it did not.
        restart: String,
    },
    /// Re-creating the dropped database failed. The service is left stopped.
    #[error(
        "the database was dropped but could not be created again: {0}. The service is left stopped"
    )]
    Create(String),
    /// The restore failed. The service is left stopped.
    #[error("the archive did not restore: {0}. The service is left stopped")]
    Restore(String),
    /// The migrations failed on the restored database. The service is left stopped.
    #[error("the migrations failed on the restored database: {0}. The service is left stopped")]
    Migrate(#[source] AdapterError),
    /// The tables could not be counted. The service is left stopped.
    #[error("could not count the refreshed database's tables: {0}. The service is left stopped")]
    TableCount(String),
    /// Too few tables came back. The service is left stopped.
    #[error(
        "the refreshed database holds {found} tables, fewer than min_tables = {required}. \
         The service is left stopped"
    )]
    TooFewTables {
        /// Tables found.
        found: u64,
        /// Tables required.
        required: u64,
    },
    /// The refresh succeeded but the service did not restart.
    #[error("the database was refreshed, but the service did not restart: {0}")]
    Restart(#[source] AdapterError),
}

impl RefreshError {
    /// The name of the step that failed, for receipts and alerts.
    #[must_use]
    pub const fn step(&self) -> &'static str {
        match self {
            Self::Secret(_) => "config",
            Self::Marker(_) | Self::NotATarget(_) | Self::Remark(_) => "marker",
            Self::SameEnvironment(_) => "source",
            Self::Extensions(_) => "extensions",
            Self::Stop(_) => "stop",
            Self::Drop { .. } => "drop",
            Self::Create(_) => "create",
            Self::Restore(_) => "restore",
            Self::Migrate(_) => "migrate",
            Self::TableCount(_) | Self::TooFewTables { .. } => "tables",
            Self::Restart(_) => "restart",
        }
    }
}

/// Refresh the database `ctx`'s DSN names from `plan.archive`.
///
/// # Errors
/// A [`RefreshError`] naming the first step that failed, and saying whether the
/// database changed and whether the service is left stopped.
pub async fn run_refresh(
    db: &dyn RefreshDb,
    service: Option<RefreshService<'_>>,
    migration: &dyn MigrationAdapter,
    ctx: &AdapterCtx,
    plan: RefreshPlan<'_>,
) -> Result<RefreshReport, RefreshError> {
    let target = ctx
        .secret(DATABASE_URL_SECRET)
        .map_err(RefreshError::Secret)?;
    let marker = db.marker(&target).await.map_err(RefreshError::Marker)?;
    marker_guard(marker.as_deref(), plan.fraise, plan.environment)
        .map_err(RefreshError::NotATarget)?;
    source_guard(plan.archive_environment, plan.environment)
        .map_err(RefreshError::SameEnvironment)?;
    if let Some(problem) = db.remark_problem(&target).await.map_err(|error| {
        RefreshError::Remark(format!("could not check the marker privilege: {error}"))
    })? {
        return Err(RefreshError::Remark(problem));
    }
    if let Some(problem) = db
        .extensions_problem(&target, plan.extensions)
        .await
        .map_err(|error| RefreshError::Extensions(format!("could not check extensions: {error}")))?
    {
        return Err(RefreshError::Extensions(problem));
    }

    if let Some(service) = service {
        service
            .adapter
            .stop(service.ctx, service.host)
            .await
            .map_err(RefreshError::Stop)?;
    }
    if let Err(error) = db.drop_target(&target).await {
        let restart = match service {
            Some(service) => match service.adapter.restart(service.ctx, service.host).await {
                Ok(()) => String::new(),
                Err(restart) => format!(", and the service did not restart: {restart}"),
            },
            None => String::new(),
        };
        return Err(RefreshError::Drop { error, restart });
    }
    db.create_target(&target, &marker_value(plan.fraise, plan.environment))
        .await
        .map_err(RefreshError::Create)?;
    db.restore(&target, plan.archive)
        .await
        .map_err(RefreshError::Restore)?;
    let migrated = migration
        .up(ctx, None)
        .await
        .map_err(RefreshError::Migrate)?;
    let tables = db
        .table_count(&target)
        .await
        .map_err(RefreshError::TableCount)?;
    if let Some(required) = plan.min_tables {
        if tables < required {
            return Err(RefreshError::TooFewTables {
                found: tables,
                required,
            });
        }
    }
    if let Some(service) = service {
        service
            .adapter
            .restart(service.ctx, service.host)
            .await
            .map_err(RefreshError::Restart)?;
    }
    Ok(RefreshReport {
        applied: migrated.applied,
        tables,
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::{
        marker_guard, marker_value, run_refresh, source_guard, RefreshDb, RefreshError,
        RefreshPlan, RefreshService,
    };
    use crate::adapter_axes::{
        AdapterCtx, AdapterDescription, AdapterError, AdapterErrorKind, HostId, MigrationAdapter,
        MigrationOutcome, Revision, ServiceAdapter, ServiceStatus, VerifyReport,
    };
    use crate::restore_rehearsal::DATABASE_URL_SECRET;

    const TARGET_DSN: &str = "postgres://app:pw@db.internal/checkout_staging";

    type Log = Arc<Mutex<Vec<String>>>;

    fn push(log: &Log, entry: impl Into<String>) {
        log.lock().expect("log").push(entry.into());
    }

    /// Which step a fake fails at.
    #[derive(Default, Clone, Copy, PartialEq, Eq)]
    enum FailAt {
        #[default]
        Nothing,
        Marker,
        Stop,
        Drop,
        Create,
        Restore,
        Restart,
    }

    struct Fake {
        log: Log,
        marker: Option<String>,
        fail: FailAt,
        tables: u64,
        extensions_problem: Option<String>,
        remark_problem: Option<String>,
    }

    impl Fake {
        fn marked(log: &Log) -> Self {
            Self {
                log: log.clone(),
                marker: Some(marker_value("checkout", "staging")),
                fail: FailAt::Nothing,
                tables: 12,
                extensions_problem: None,
                remark_problem: None,
            }
        }
    }

    fn outcome(fail: bool, step: &str) -> Result<(), String> {
        if fail {
            Err(format!("{step} failed"))
        } else {
            Ok(())
        }
    }

    #[async_trait]
    impl RefreshDb for Fake {
        async fn marker(&self, target_dsn: &str) -> Result<Option<String>, String> {
            push(&self.log, format!("marker:{target_dsn}"));
            outcome(self.fail == FailAt::Marker, "marker")?;
            Ok(self.marker.clone())
        }
        async fn extensions_problem(
            &self,
            _target_dsn: &str,
            extensions: &[String],
        ) -> Result<Option<String>, String> {
            push(&self.log, format!("extensions:{}", extensions.join(",")));
            Ok(self.extensions_problem.clone())
        }
        async fn remark_problem(&self, _target_dsn: &str) -> Result<Option<String>, String> {
            push(&self.log, "remark?");
            Ok(self.remark_problem.clone())
        }
        async fn drop_target(&self, _target_dsn: &str) -> Result<(), String> {
            push(&self.log, "drop");
            outcome(self.fail == FailAt::Drop, "drop")
        }
        async fn create_target(&self, _target_dsn: &str, marker: &str) -> Result<(), String> {
            push(&self.log, format!("create:{marker}"));
            outcome(self.fail == FailAt::Create, "create")
        }
        async fn restore(&self, _target_dsn: &str, archive: &Path) -> Result<(), String> {
            push(&self.log, format!("restore:{}", archive.display()));
            outcome(self.fail == FailAt::Restore, "restore")
        }
        async fn table_count(&self, _target_dsn: &str) -> Result<u64, String> {
            push(&self.log, "count");
            Ok(self.tables)
        }
    }

    struct Service {
        log: Log,
        fail: FailAt,
        can_stop: bool,
    }

    #[async_trait]
    impl ServiceAdapter for Service {
        async fn restart(&self, _ctx: &AdapterCtx, _host: &HostId) -> Result<(), AdapterError> {
            push(&self.log, "service:restart");
            if self.fail == FailAt::Restart {
                return Err(AdapterError::new(AdapterErrorKind::Execution, "no start"));
            }
            Ok(())
        }
        async fn stop(&self, ctx: &AdapterCtx, host: &HostId) -> Result<(), AdapterError> {
            if !self.can_stop {
                let _ = (ctx, host);
                return Err(AdapterError::method_not_supported("stop"));
            }
            push(&self.log, "service:stop");
            if self.fail == FailAt::Stop {
                return Err(AdapterError::new(AdapterErrorKind::Execution, "no stop"));
            }
            Ok(())
        }
        async fn status(
            &self,
            _ctx: &AdapterCtx,
            _host: &HostId,
        ) -> Result<ServiceStatus, AdapterError> {
            unreachable!("a refresh never asks for status")
        }
    }

    struct Migration {
        log: Log,
        fail: bool,
    }

    #[async_trait]
    impl MigrationAdapter for Migration {
        async fn describe(&self) -> Result<AdapterDescription, AdapterError> {
            unreachable!("a refresh never calls describe")
        }
        async fn current_revision(
            &self,
            _ctx: &AdapterCtx,
        ) -> Result<Option<Revision>, AdapterError> {
            unreachable!("a refresh never calls current_revision")
        }
        async fn up(
            &self,
            ctx: &AdapterCtx,
            _target: Option<Revision>,
        ) -> Result<MigrationOutcome, AdapterError> {
            push(
                &self.log,
                format!("migrate:{}", ctx.secret(DATABASE_URL_SECRET).expect("dsn")),
            );
            if self.fail {
                return Err(AdapterError::new(
                    AdapterErrorKind::Execution,
                    "bad migration",
                ));
            }
            Ok(MigrationOutcome {
                from: Some(Revision::new("0002")),
                to: Some(Revision::new("0003")),
                applied: vec![Revision::new("0003")],
                log: String::new(),
            })
        }
        async fn down_to(
            &self,
            _ctx: &AdapterCtx,
            _target: Revision,
        ) -> Result<MigrationOutcome, AdapterError> {
            unreachable!("a refresh never rolls back")
        }
        async fn verify(&self, _ctx: &AdapterCtx) -> Result<VerifyReport, AdapterError> {
            unreachable!("a refresh never calls verify")
        }
    }

    fn ctx() -> AdapterCtx {
        AdapterCtx::new("checkout", "staging").with_resolved_secret(DATABASE_URL_SECRET, TARGET_DSN)
    }

    fn archive() -> PathBuf {
        PathBuf::from("/var/backups/checkout-production-20261026T020000Z.pgdump")
    }

    fn plan(archive: &Path) -> RefreshPlan<'_> {
        RefreshPlan {
            fraise: "checkout",
            environment: "staging",
            archive,
            archive_environment: "production",
            min_tables: Some(10),
            extensions: &[],
        }
    }

    struct Run {
        result: Result<super::RefreshReport, RefreshError>,
        log: Vec<String>,
    }

    async fn refresh(fake: Fake, service: Option<Service>, migrate_fails: bool) -> Run {
        refresh_plan(fake, service, migrate_fails, |p| p).await
    }

    async fn refresh_plan(
        fake: Fake,
        service: Option<Service>,
        migrate_fails: bool,
        tweak: impl FnOnce(RefreshPlan<'_>) -> RefreshPlan<'_>,
    ) -> Run {
        let log = fake.log.clone();
        let migration = Migration {
            log: log.clone(),
            fail: migrate_fails,
        };
        let ctx = ctx();
        let host = HostId::new("localhost");
        let archive = archive();
        let result = run_refresh(
            &fake,
            service.as_ref().map(|adapter| RefreshService {
                adapter,
                ctx: &ctx,
                host: &host,
            }),
            &migration,
            &ctx,
            tweak(plan(&archive)),
        )
        .await;
        let log = log.lock().expect("log").clone();
        Run { result, log }
    }

    fn service(log: &Log) -> Service {
        Service {
            log: log.clone(),
            fail: FailAt::Nothing,
            can_stop: true,
        }
    }

    #[test]
    fn the_marker_names_the_fraise_and_environment() {
        assert_eq!(marker_value("checkout", "staging"), "checkout/staging");
    }

    #[test]
    fn an_unmarked_database_is_not_a_target() {
        let refused = marker_guard(None, "checkout", "staging").expect_err("unmarked");
        assert!(refused.contains("mark-refresh-target"), "{refused}");
    }

    #[test]
    fn a_database_marked_for_another_config_is_not_a_target() {
        let refused =
            marker_guard(Some("checkout/qa"), "checkout", "staging").expect_err("other marker");
        assert!(refused.contains("checkout/qa"), "{refused}");
        assert!(marker_guard(Some("checkout/staging"), "checkout", "staging").is_ok());
    }

    #[test]
    fn an_archive_of_the_targets_own_environment_is_refused() {
        assert!(source_guard("production", "production").is_err());
        assert!(source_guard("production", "staging").is_ok());
    }

    #[tokio::test]
    async fn a_refresh_runs_its_steps_in_order() {
        let log = Log::default();
        let run = refresh(Fake::marked(&log), Some(service(&log)), false).await;
        let report = run.result.expect("refresh passes");
        assert_eq!(report.applied, vec![Revision::new("0003")]);
        assert_eq!(report.tables, 12);
        assert_eq!(
            run.log,
            vec![
                format!("marker:{TARGET_DSN}"),
                "remark?".to_owned(),
                "extensions:".to_owned(),
                "service:stop".to_owned(),
                "drop".to_owned(),
                "create:checkout/staging".to_owned(),
                format!("restore:{}", archive().display()),
                format!("migrate:{TARGET_DSN}"),
                "count".to_owned(),
                "service:restart".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn an_unmarked_target_changes_nothing() {
        let log = Log::default();
        let mut fake = Fake::marked(&log);
        fake.marker = None;
        let run = refresh(fake, Some(service(&log)), false).await;
        assert!(matches!(run.result, Err(RefreshError::NotATarget(_))));
        assert_eq!(run.log, vec![format!("marker:{TARGET_DSN}")]);
    }

    #[tokio::test]
    async fn a_target_marked_for_another_config_changes_nothing() {
        let log = Log::default();
        let mut fake = Fake::marked(&log);
        fake.marker = Some("checkout/qa".to_owned());
        let run = refresh(fake, Some(service(&log)), false).await;
        assert!(matches!(run.result, Err(RefreshError::NotATarget(_))));
        assert_eq!(run.log.len(), 1);
    }

    #[tokio::test]
    async fn an_unreadable_marker_changes_nothing() {
        let log = Log::default();
        let mut fake = Fake::marked(&log);
        fake.fail = FailAt::Marker;
        let run = refresh(fake, Some(service(&log)), false).await;
        assert!(matches!(run.result, Err(RefreshError::Marker(_))));
        assert_eq!(run.log.len(), 1);
    }

    #[tokio::test]
    async fn an_archive_of_the_targets_environment_changes_nothing() {
        let log = Log::default();
        let run = refresh_plan(Fake::marked(&log), Some(service(&log)), false, |p| {
            RefreshPlan {
                archive_environment: "staging",
                ..p
            }
        })
        .await;
        assert!(matches!(run.result, Err(RefreshError::SameEnvironment(_))));
        assert!(!run.log.iter().any(|s| s == "drop" || s == "service:stop"));
    }

    #[tokio::test]
    async fn a_role_that_cannot_re_mark_the_database_changes_nothing() {
        let log = Log::default();
        let mut fake = Fake::marked(&log);
        fake.remark_problem = Some("GRANT SET ON PARAMETER".to_owned());
        let run = refresh(fake, Some(service(&log)), false).await;
        assert!(matches!(run.result, Err(RefreshError::Remark(_))));
        assert!(
            !run.log.iter().any(|s| s == "drop" || s == "service:stop"),
            "{:?}",
            run.log
        );
    }

    #[tokio::test]
    async fn an_extension_the_role_cannot_create_changes_nothing() {
        let log = Log::default();
        let mut fake = Fake::marked(&log);
        fake.extensions_problem = Some("pg_tviews is untrusted".to_owned());
        let run = refresh(fake, Some(service(&log)), false).await;
        assert!(matches!(run.result, Err(RefreshError::Extensions(_))));
        assert!(
            !run.log.iter().any(|s| s == "drop" || s == "service:stop"),
            "{:?}",
            run.log
        );
    }

    #[tokio::test]
    async fn a_service_that_cannot_stop_changes_nothing() {
        let log = Log::default();
        let mut svc = service(&log);
        svc.can_stop = false;
        let run = refresh(Fake::marked(&log), Some(svc), false).await;
        let Err(RefreshError::Stop(error)) = run.result else {
            panic!("expected a stop refusal");
        };
        assert_eq!(error.kind, AdapterErrorKind::MethodNotSupported);
        assert!(!run.log.iter().any(|s| s == "drop"), "{:?}", run.log);
    }

    #[tokio::test]
    async fn without_a_service_the_database_is_still_refreshed() {
        let log = Log::default();
        let run = refresh(Fake::marked(&log), None, false).await;
        assert!(run.result.is_ok());
        assert!(!run.log.iter().any(|s| s.starts_with("service:")));
    }

    #[tokio::test]
    async fn a_failed_drop_restarts_the_service() {
        let log = Log::default();
        let mut fake = Fake::marked(&log);
        fake.fail = FailAt::Drop;
        let run = refresh(fake, Some(service(&log)), false).await;
        assert!(matches!(run.result, Err(RefreshError::Drop { .. })));
        assert_eq!(run.log.last().map(String::as_str), Some("service:restart"));
    }

    #[tokio::test]
    async fn a_failure_after_the_drop_leaves_the_service_stopped() {
        for fail in [FailAt::Create, FailAt::Restore] {
            let log = Log::default();
            let mut fake = Fake::marked(&log);
            fake.fail = fail;
            let run = refresh(fake, Some(service(&log)), false).await;
            assert!(run.result.is_err());
            assert!(
                !run.log.iter().any(|s| s == "service:restart"),
                "{:?}",
                run.log
            );
            assert!(run
                .result
                .err()
                .is_some_and(|e| e.to_string().contains("left stopped")));
        }
        let log = Log::default();
        let run = refresh(Fake::marked(&log), Some(service(&log)), true).await;
        assert!(matches!(run.result, Err(RefreshError::Migrate(_))));
        assert!(!run.log.iter().any(|s| s == "service:restart"));
    }

    #[tokio::test]
    async fn too_few_tables_fails_and_leaves_the_service_stopped() {
        let log = Log::default();
        let mut fake = Fake::marked(&log);
        fake.tables = 3;
        let run = refresh(fake, Some(service(&log)), false).await;
        assert!(matches!(
            run.result,
            Err(RefreshError::TooFewTables {
                found: 3,
                required: 10
            })
        ));
        assert!(!run.log.iter().any(|s| s == "service:restart"));
    }

    #[tokio::test]
    async fn a_service_that_does_not_restart_fails_the_refresh() {
        let log = Log::default();
        let mut svc = service(&log);
        svc.fail = FailAt::Restart;
        let run = refresh(Fake::marked(&log), Some(svc), false).await;
        assert!(matches!(run.result, Err(RefreshError::Restart(_))));
    }
}
