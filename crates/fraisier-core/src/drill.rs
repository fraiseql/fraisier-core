//! The restore drill: prove the newest backup restores and migrates.
//!
//! A drill restores an archive into a throwaway database, runs the pending
//! migrations on it (the upgrade drill), checks that it holds at least the tables
//! it should, and drops it. It touches nothing else: the live database is only
//! read, to derive the throwaway's connection.
//!
//! Dropping is the one destructive step, so it is guarded. The throwaway is
//! dropped only when its name carries [`THROWAWAY_MARKER`] and differs from the
//! live database's name. A database that fails the guard is left in place and the
//! drill fails, because the name check is the last thing between `DROP DATABASE
//! … WITH (FORCE)` and a database nobody meant to drop.

use std::path::Path;

use async_trait::async_trait;

use crate::adapter_axes::{AdapterCtx, AdapterError, MigrationAdapter, Revision};
use crate::restore_rehearsal::{BackupSource, RehearsalDb, ThrowawayDb, DATABASE_URL_SECRET};

/// The marker every throwaway database name carries.
pub const THROWAWAY_MARKER: &str = "_fraisier_rh_";

/// The database operations a drill needs beyond a rehearsal's.
#[async_trait]
pub trait DrillDb: RehearsalDb {
    /// The name of the database `live_dsn` connects to.
    ///
    /// # Errors
    /// `live_dsn` names no database.
    fn live_database(&self, live_dsn: &str) -> Result<String, String>;

    /// How many ordinary tables the throwaway database holds, outside the system
    /// schemas.
    ///
    /// # Errors
    /// The count could not be taken.
    async fn table_count(&self, db: &ThrowawayDb) -> Result<u64, String>;
}

/// What a successful drill found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrillReport {
    /// The throwaway database the archive was restored into.
    pub throwaway: String,
    /// The migrations the upgrade drill applied on top of the restored archive.
    pub applied: Vec<Revision>,
    /// The tables the migrated copy holds.
    pub tables: u64,
    /// Set when the drill passed but the throwaway could not be dropped, so it
    /// leaked and should be removed by hand.
    pub teardown_warning: Option<String>,
}

/// Why a drill failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DrillError {
    /// The live database DSN could not be resolved.
    #[error("the drill could not resolve the database DSN: {0}")]
    Secret(#[source] AdapterError),
    /// The live DSN names no database.
    #[error("the drill could not read the live database name: {0}")]
    LiveDatabase(String),
    /// Creating the throwaway or restoring the archive into it failed.
    #[error("the archive did not restore: {0}")]
    Restore(String),
    /// The pending migrations failed on the restored copy.
    #[error("the migrations failed on the restored copy: {0}")]
    Migrate(#[source] AdapterError),
    /// The tables could not be counted.
    #[error("could not count the restored copy's tables: {0}")]
    TableCount(String),
    /// The restored copy holds fewer tables than `min_tables`.
    #[error("the restored copy holds {found} tables, fewer than min_tables = {required}")]
    TooFewTables {
        /// Tables found.
        found: u64,
        /// Tables required.
        required: u64,
    },
    /// The throwaway's name failed the teardown guard, so it was not dropped.
    #[error("refusing to drop database '{name}': {reason}; drop it by hand once checked")]
    UnsafeTeardown {
        /// The database left in place.
        name: String,
        /// Why the guard refused it.
        reason: String,
    },
}

/// Whether `throwaway` may be dropped when the live database is `live`.
///
/// # Errors
/// The reason it may not: the name lacks [`THROWAWAY_MARKER`], or it is the live
/// database.
pub fn teardown_guard(throwaway: &str, live: &str) -> Result<(), String> {
    if !throwaway.contains(THROWAWAY_MARKER) {
        return Err(format!(
            "its name does not carry the throwaway marker '{THROWAWAY_MARKER}'"
        ));
    }
    if throwaway == live {
        return Err("it is the live database".to_owned());
    }
    Ok(())
}

/// Restore `archive` into a throwaway, migrate it, check it holds at least
/// `min_tables` tables, and drop it.
///
/// The throwaway is always dropped once it passes [`teardown_guard`], whatever the
/// migration or the table check found. A failure to drop it after a passing drill
/// is a [`DrillReport::teardown_warning`]; after a failing drill, the failure is
/// what is reported.
///
/// # Errors
/// A [`DrillError`] naming the first step that failed.
pub async fn run_restore_drill(
    db: &dyn DrillDb,
    migration: &dyn MigrationAdapter,
    ctx: &AdapterCtx,
    archive: &Path,
    min_tables: Option<u64>,
) -> Result<DrillReport, DrillError> {
    let live_dsn = ctx
        .secret(DATABASE_URL_SECRET)
        .map_err(DrillError::Secret)?;
    let live = db
        .live_database(&live_dsn)
        .map_err(DrillError::LiveDatabase)?;
    let throwaway = db
        .provision(&live_dsn, &BackupSource::Archive(archive.to_owned()))
        .await
        .map_err(|error| DrillError::Restore(error.to_string()))?;
    // Checked before anything runs against it: a copy that may not be dropped
    // must not be migrated either.
    if let Err(reason) = teardown_guard(&throwaway.name, &live) {
        return Err(DrillError::UnsafeTeardown {
            name: throwaway.name,
            reason,
        });
    }

    let copy = ctx
        .clone()
        .with_resolved_secret(DATABASE_URL_SECRET, throwaway.dsn.clone());
    let checked = async {
        let migrated = migration
            .up(&copy, None)
            .await
            .map_err(DrillError::Migrate)?;
        let tables = db
            .table_count(&throwaway)
            .await
            .map_err(DrillError::TableCount)?;
        match min_tables {
            Some(required) if tables < required => Err(DrillError::TooFewTables {
                found: tables,
                required,
            }),
            _ => Ok((migrated.applied, tables)),
        }
    }
    .await;
    let dropped = db.teardown(&throwaway).await;

    let (applied, tables) = checked?;
    Ok(DrillReport {
        throwaway: throwaway.name,
        applied,
        tables,
        teardown_warning: dropped.err(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::{run_restore_drill, teardown_guard, DrillDb, DrillError};
    use crate::adapter_axes::{
        AdapterCtx, AdapterDescription, AdapterError, AdapterErrorKind, MigrationAdapter,
        MigrationOutcome, Revision, VerifyReport,
    };
    use crate::restore_rehearsal::{
        BackupSource, RehearsalDb, RehearsalError, ThrowawayDb, DATABASE_URL_SECRET,
    };

    const LIVE_DSN: &str = "postgres://u:pw@db.internal/app";

    #[derive(Default)]
    struct Fake {
        calls: Arc<Mutex<Vec<String>>>,
        throwaway_name: Option<String>,
        provision_fails: bool,
        tables: u64,
        teardown_fails: bool,
    }

    impl Fake {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls").clone()
        }
        fn name(&self) -> String {
            self.throwaway_name
                .clone()
                .unwrap_or_else(|| "app_fraisier_rh_1_a".to_owned())
        }
    }

    #[async_trait]
    impl RehearsalDb for Fake {
        async fn provision(
            &self,
            live_dsn: &str,
            backup: &BackupSource,
        ) -> Result<ThrowawayDb, RehearsalError> {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("provision:{live_dsn}:{backup:?}"));
            if self.provision_fails {
                return Err(RehearsalError::Provision(
                    "pg_restore: bad archive".to_owned(),
                ));
            }
            Ok(ThrowawayDb {
                dsn: format!("postgres://u:pw@db.internal/{}", self.name()),
                name: self.name(),
            })
        }

        async fn teardown(&self, db: &ThrowawayDb) -> Result<(), String> {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("teardown:{}", db.name));
            if self.teardown_fails {
                Err("DROP DATABASE failed".to_owned())
            } else {
                Ok(())
            }
        }
    }

    #[async_trait]
    impl DrillDb for Fake {
        fn live_database(&self, live_dsn: &str) -> Result<String, String> {
            live_dsn
                .rsplit('/')
                .next()
                .map(str::to_owned)
                .ok_or_else(|| "no database".to_owned())
        }

        async fn table_count(&self, db: &ThrowawayDb) -> Result<u64, String> {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("count:{}", db.name));
            Ok(self.tables)
        }
    }

    struct Migration {
        migrated: Arc<Mutex<Option<String>>>,
        fail: bool,
    }

    impl Migration {
        fn new(fail: bool) -> Self {
            Self {
                migrated: Arc::new(Mutex::new(None)),
                fail,
            }
        }
    }

    #[async_trait]
    impl MigrationAdapter for Migration {
        async fn describe(&self) -> Result<AdapterDescription, AdapterError> {
            unreachable!("a drill never calls describe")
        }
        async fn current_revision(
            &self,
            _ctx: &AdapterCtx,
        ) -> Result<Option<Revision>, AdapterError> {
            unreachable!("a drill never calls current_revision")
        }
        async fn up(
            &self,
            ctx: &AdapterCtx,
            _target: Option<Revision>,
        ) -> Result<MigrationOutcome, AdapterError> {
            *self.migrated.lock().expect("migrated") =
                Some(ctx.secret(DATABASE_URL_SECRET).expect("dsn"));
            if self.fail {
                return Err(AdapterError::new(
                    AdapterErrorKind::Execution,
                    "relation \"widgets\" does not exist",
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
            unreachable!("a drill never rolls back")
        }
        async fn verify(&self, _ctx: &AdapterCtx) -> Result<VerifyReport, AdapterError> {
            unreachable!("a drill never calls verify")
        }
    }

    fn ctx() -> AdapterCtx {
        AdapterCtx::new("app", "production").with_resolved_secret(DATABASE_URL_SECRET, LIVE_DSN)
    }

    fn archive() -> PathBuf {
        PathBuf::from("/backups/app-production-20261026T030000Z.pgdump")
    }

    async fn drill(
        db: &Fake,
        migration: &Migration,
        min_tables: Option<u64>,
    ) -> Result<super::DrillReport, DrillError> {
        run_restore_drill(db, migration, &ctx(), Path::new(&archive()), min_tables).await
    }

    #[test]
    fn the_guard_admits_a_marked_name_that_is_not_live() {
        assert_eq!(teardown_guard("app_fraisier_rh_1_a", "app"), Ok(()));
    }

    #[test]
    fn the_guard_refuses_a_name_without_the_marker() {
        let reason = teardown_guard("app_copy", "app").expect_err("no marker");
        assert!(reason.contains("_fraisier_rh_"), "{reason}");
    }

    #[test]
    fn the_guard_refuses_the_live_database_even_with_the_marker() {
        let reason = teardown_guard("app_fraisier_rh_x", "app_fraisier_rh_x").expect_err("live");
        assert!(reason.contains("live"), "{reason}");
    }

    #[tokio::test]
    async fn a_drill_restores_the_archive_migrates_the_copy_and_drops_it() {
        let db = Fake {
            tables: 15,
            ..Fake::default()
        };
        let migration = Migration::new(false);
        let report = drill(&db, &migration, Some(10))
            .await
            .expect("drill passes");
        assert_eq!(report.throwaway, "app_fraisier_rh_1_a");
        assert_eq!(report.applied, [Revision::new("0003")]);
        assert_eq!(report.tables, 15);
        assert_eq!(report.teardown_warning, None);
        assert_eq!(
            migration.migrated.lock().expect("migrated").as_deref(),
            Some("postgres://u:pw@db.internal/app_fraisier_rh_1_a"),
            "the migrations ran on the copy, never on live"
        );
        assert_eq!(
            db.calls(),
            [
                format!(
                    "provision:{LIVE_DSN}:{:?}",
                    BackupSource::Archive(archive())
                ),
                "count:app_fraisier_rh_1_a".to_owned(),
                "teardown:app_fraisier_rh_1_a".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn the_copy_is_dropped_when_the_migrations_fail() {
        let db = Fake::default();
        let error = drill(&db, &Migration::new(true), None)
            .await
            .expect_err("migrations fail");
        assert!(matches!(error, DrillError::Migrate(_)), "{error:?}");
        assert!(db
            .calls()
            .contains(&"teardown:app_fraisier_rh_1_a".to_owned()));
    }

    #[tokio::test]
    async fn the_copy_is_dropped_when_it_holds_too_few_tables() {
        let db = Fake {
            tables: 3,
            ..Fake::default()
        };
        let error = drill(&db, &Migration::new(false), Some(10))
            .await
            .expect_err("too few tables");
        assert!(
            matches!(
                error,
                DrillError::TooFewTables {
                    found: 3,
                    required: 10
                }
            ),
            "{error:?}"
        );
        assert!(db
            .calls()
            .contains(&"teardown:app_fraisier_rh_1_a".to_owned()));
    }

    #[tokio::test]
    async fn without_min_tables_any_count_passes() {
        let db = Fake::default();
        let report = drill(&db, &Migration::new(false), None)
            .await
            .expect("passes");
        assert_eq!(report.tables, 0);
    }

    #[tokio::test]
    async fn an_archive_that_does_not_restore_fails_and_drops_nothing() {
        let db = Fake {
            provision_fails: true,
            ..Fake::default()
        };
        let migration = Migration::new(false);
        let error = drill(&db, &migration, None)
            .await
            .expect_err("restore fails");
        assert!(matches!(error, DrillError::Restore(_)), "{error:?}");
        assert!(migration.migrated.lock().expect("migrated").is_none());
        assert!(!db.calls().iter().any(|call| call.starts_with("teardown")));
    }

    #[tokio::test]
    async fn a_throwaway_without_the_marker_is_neither_migrated_nor_dropped() {
        let db = Fake {
            throwaway_name: Some("app_restored".to_owned()),
            ..Fake::default()
        };
        let migration = Migration::new(false);
        let error = drill(&db, &migration, None)
            .await
            .expect_err("guard refuses");
        assert!(
            matches!(&error, DrillError::UnsafeTeardown { name, .. } if name == "app_restored"),
            "{error:?}"
        );
        assert!(migration.migrated.lock().expect("migrated").is_none());
        assert!(!db.calls().iter().any(|call| call.starts_with("teardown")));
    }

    #[tokio::test]
    async fn a_throwaway_named_like_live_is_neither_migrated_nor_dropped() {
        let db = Fake {
            throwaway_name: Some("app".to_owned()),
            ..Fake::default()
        };
        let error = drill(&db, &Migration::new(false), None)
            .await
            .expect_err("guard refuses");
        assert!(
            matches!(error, DrillError::UnsafeTeardown { .. }),
            "{error:?}"
        );
        assert!(!db.calls().iter().any(|call| call.starts_with("teardown")));
    }

    #[tokio::test]
    async fn a_passing_drill_whose_copy_will_not_drop_warns() {
        let db = Fake {
            teardown_fails: true,
            ..Fake::default()
        };
        let report = drill(&db, &Migration::new(false), None)
            .await
            .expect("passes");
        assert!(report.teardown_warning.is_some());
    }

    #[tokio::test]
    async fn a_drill_without_a_dsn_fails_before_restoring() {
        let db = Fake::default();
        let error = run_restore_drill(
            &db,
            &Migration::new(false),
            &AdapterCtx::new("app", "production"),
            &archive(),
            None,
        )
        .await
        .expect_err("no DSN");
        assert!(matches!(error, DrillError::Secret(_)), "{error:?}");
        assert!(db.calls().is_empty(), "{:?}", db.calls());
    }
}
