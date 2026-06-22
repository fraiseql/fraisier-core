//! Restore-rehearsal migration preflight (a DR-grade preflight).
//!
//! The forward-compatibility lint ([`crate::single_host`] preflight, gated by
//! [`PreflightMode::Live`](crate::single_host::PreflightMode::Live)) checks the
//! pending migrations against the **live** database. This module adds the
//! complementary [`RestoreRehearsal`](crate::single_host::PreflightMode::RestoreRehearsal)
//! mode: it provisions a *throwaway* copy of the database from a backup, runs the
//! pending migrations there, and tears it down — proving that a real
//! `db restore` + `migrate up` succeeds *before* the live deploy touches anything.
//!
//! # Self-consistency by construction (the Python 0.34 lesson)
//!
//! Python fraisier's restore preflight restored the backup **schema-only** and
//! then resolved the *pending* set from the app's **live** config database. When
//! the backup was several migrations behind that live state, migrations the live
//! DB already considered applied — but absent from the older backup — were never
//! re-run, so a later dependent migration was evaluated against a base that lacked
//! its predecessor and failed spuriously. The fix there was to restore the
//! tracking rows too and resolve pending from the restored DB.
//!
//! This implementation sidesteps that bug entirely: the throwaway is seeded from a
//! **full** restore (schema *and* the migration-tracking rows travel with it), and
//! the pending set is resolved from the throwaway's **own** tracking table — never
//! from the live DB. The pending source and the apply base are the same database,
//! so the rehearsal applies exactly the chain a real restore + `migrate up` would.
//!
//! # Composition
//!
//! The orchestration ([`run_restore_rehearsal`]) is generic over a
//! [`RehearsalDb`] (provision + teardown of the throwaway) and any
//! [`MigrationAdapter`], so it is unit-testable with fakes. The real
//! Postgres-backed [`RehearsalDb`] lives in the CLI wiring layer (over
//! `fraisier-db`'s `pg_dump`/`pg_restore`).

use std::path::PathBuf;

use async_trait::async_trait;

use crate::adapter_axes::{AdapterCtx, AdapterError, MigrationAdapter, Revision};

/// The logical secret name the migration DSN is exposed under (Decision 5); the
/// migration adapter resolves it with `ctx.secret("DATABASE_URL")`.
pub const DATABASE_URL_SECRET: &str = "DATABASE_URL";

/// Where the rehearsal's seed data comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupSource {
    /// Take a fresh `pg_dump` of the live database at rehearsal time. Proves the
    /// pending chain applies against the *current* live schema + data.
    FreshDump,
    /// Restore an existing backup archive. Proves the pending chain applies on the
    /// backup's (possibly older) base — the genuine disaster-recovery rehearsal.
    Archive(PathBuf),
}

/// A provisioned throwaway database the rehearsal migrates against. Handed back to
/// [`RehearsalDb::teardown`] for cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThrowawayDb {
    /// A DSN that resolves to the throwaway database (carries the password, like
    /// any DSN — kept in-process, never logged).
    pub dsn: String,
    /// The throwaway database name, for diagnostics and teardown.
    pub name: String,
}

/// The outcome of a successful rehearsal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RehearsalReport {
    /// The migrations applied during the rehearsal — the pending set proved safe.
    pub applied: Vec<Revision>,
    /// Set when the migration rehearsed green but tearing the throwaway DB down
    /// afterwards failed: the rehearsal still *passes* (the deploy is safe), but
    /// the named database leaked and should be reaped.
    pub teardown_warning: Option<String>,
}

/// A failure during the restore-rehearsal preflight.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RehearsalError {
    /// The live database DSN could not be resolved (so there is nothing to clone).
    #[error("restore-rehearsal could not resolve the database DSN: {0}")]
    Secret(#[source] AdapterError),
    /// Provisioning or seeding the throwaway database failed.
    #[error("restore-rehearsal could not provision the throwaway database: {0}")]
    Provision(String),
    /// The rehearsed `migrate up` failed against the throwaway database — the same
    /// failure a real restore + migrate would hit, caught before touching live.
    #[error("restore-rehearsal migration failed on the throwaway database: {0}")]
    Migrate(#[source] AdapterError),
}

/// Provision and tear down the throwaway database the rehearsal runs against.
///
/// Implementations restore a **full** backup (schema + migration-tracking rows) so
/// the pending set resolved against the throwaway matches a real restore.
#[async_trait]
pub trait RehearsalDb: Send + Sync {
    /// Provision a throwaway database seeded from `backup`, deriving it from
    /// `live_dsn` (same server/credentials, a fresh database name). Returns a DSN
    /// that resolves to it.
    ///
    /// # Errors
    /// Implementation-defined failure provisioning or seeding the database.
    async fn provision(
        &self,
        live_dsn: &str,
        backup: &BackupSource,
    ) -> Result<ThrowawayDb, RehearsalError>;

    /// Tear down a throwaway database provisioned by [`Self::provision`].
    ///
    /// # Errors
    /// Implementation-defined failure dropping the database; surfaced as a
    /// non-fatal warning when the rehearsal itself succeeded.
    async fn teardown(&self, db: &ThrowawayDb) -> Result<(), String>;
}

/// Run the restore-rehearsal preflight: provision a throwaway DB from `backup`,
/// rehearse the pending migrations on it via `migration`, and tear it down.
///
/// The migration runs against a clone of `ctx` whose `DATABASE_URL` secret is
/// redirected (in-process) to the throwaway DB, so the pending set is resolved
/// from the throwaway's own tracking table — never the live DB.
///
/// The throwaway is always torn down: a teardown failure after a *successful*
/// rehearsal is reported as [`RehearsalReport::teardown_warning`] rather than
/// failing the deploy; a rehearsal failure takes precedence over any teardown
/// error.
///
/// # Errors
/// [`RehearsalError`] if the DSN cannot be resolved, the throwaway cannot be
/// provisioned, or the rehearsed migration fails.
pub async fn run_restore_rehearsal(
    db: &dyn RehearsalDb,
    migration: &dyn MigrationAdapter,
    ctx: &AdapterCtx,
    backup: &BackupSource,
) -> Result<RehearsalReport, RehearsalError> {
    let live_dsn = ctx
        .secret(DATABASE_URL_SECRET)
        .map_err(RehearsalError::Secret)?;
    let throwaway = db.provision(&live_dsn, backup).await?;

    // Redirect the migration adapter at the throwaway DB (in-process only — the
    // value never serializes and never logs). Pending resolves from *its* tracking.
    let rehearsal_ctx = ctx
        .clone()
        .with_resolved_secret(DATABASE_URL_SECRET, throwaway.dsn.clone());
    let migrate_result = migration.up(&rehearsal_ctx, None).await;

    // Always tear down, whatever the migration did.
    let teardown_result = db.teardown(&throwaway).await;

    let outcome = migrate_result.map_err(RehearsalError::Migrate)?;
    Ok(RehearsalReport {
        applied: outcome.applied,
        teardown_warning: teardown_result.err(),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        run_restore_rehearsal, BackupSource, RehearsalDb, RehearsalError, ThrowawayDb,
        DATABASE_URL_SECRET,
    };
    use crate::adapter_axes::{
        AdapterCtx, AdapterDescription, AdapterError, AdapterErrorKind, MigrationAdapter,
        MigrationOutcome, Revision, VerifyReport,
    };
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    const LIVE_DSN: &str = "postgres://u:pw@db.internal/app";
    const THROWAWAY_DSN: &str = "postgres://u:pw@db.internal/app_fraisier_rehearsal";

    /// A fake throwaway-DB lifecycle that records provision/teardown calls.
    struct FakeDb {
        calls: Arc<Mutex<Vec<String>>>,
        teardown_fails: bool,
    }

    #[async_trait]
    impl RehearsalDb for FakeDb {
        async fn provision(
            &self,
            live_dsn: &str,
            backup: &BackupSource,
        ) -> Result<ThrowawayDb, RehearsalError> {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("provision:{live_dsn}:{backup:?}"));
            Ok(ThrowawayDb {
                dsn: THROWAWAY_DSN.to_owned(),
                name: "app_fraisier_rehearsal".to_owned(),
            })
        }

        async fn teardown(&self, db: &ThrowawayDb) -> Result<(), String> {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("teardown:{}", db.name));
            if self.teardown_fails {
                return Err("drop database failed".to_owned());
            }
            Ok(())
        }
    }

    /// A fake migration adapter that records the DSN its `up` was asked to migrate
    /// (by resolving the secret from the context it is handed).
    struct RecordingMigration {
        migrated_dsn: Arc<Mutex<Option<String>>>,
        fail: bool,
    }

    #[async_trait]
    impl MigrationAdapter for RecordingMigration {
        async fn describe(&self) -> Result<AdapterDescription, AdapterError> {
            unreachable!("the rehearsal never calls describe")
        }
        async fn current_revision(
            &self,
            _ctx: &AdapterCtx,
        ) -> Result<Option<Revision>, AdapterError> {
            unreachable!("the rehearsal never calls current_revision")
        }
        async fn up(
            &self,
            ctx: &AdapterCtx,
            _target: Option<Revision>,
        ) -> Result<MigrationOutcome, AdapterError> {
            let dsn = ctx.secret(DATABASE_URL_SECRET).expect("ctx resolves a DSN");
            *self.migrated_dsn.lock().expect("dsn") = Some(dsn);
            if self.fail {
                return Err(AdapterError::new(
                    AdapterErrorKind::Execution,
                    "relation \"widgets\" does not exist",
                ));
            }
            Ok(MigrationOutcome {
                from: None,
                to: Some(Revision::new("0003")),
                applied: vec![Revision::new("0002"), Revision::new("0003")],
                log: String::new(),
            })
        }
        async fn down_to(
            &self,
            _ctx: &AdapterCtx,
            _target: Revision,
        ) -> Result<MigrationOutcome, AdapterError> {
            unreachable!("the rehearsal never rolls back")
        }
        async fn verify(&self, _ctx: &AdapterCtx) -> Result<VerifyReport, AdapterError> {
            unreachable!("the rehearsal never calls verify")
        }
    }

    /// A live context whose `DATABASE_URL` resolves (in-process) to the live DSN.
    fn live_ctx() -> AdapterCtx {
        AdapterCtx::new("app", "production").with_resolved_secret(DATABASE_URL_SECRET, LIVE_DSN)
    }

    #[tokio::test]
    async fn rehearses_against_the_throwaway_db_not_the_live_dsn() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let migrated_dsn = Arc::new(Mutex::new(None));
        let db = FakeDb {
            calls: Arc::clone(&calls),
            teardown_fails: false,
        };
        let migration = RecordingMigration {
            migrated_dsn: Arc::clone(&migrated_dsn),
            fail: false,
        };

        let report = run_restore_rehearsal(&db, &migration, &live_ctx(), &BackupSource::FreshDump)
            .await
            .expect("rehearsal succeeds");

        // The migration ran against the THROWAWAY db, never the live DSN — so the
        // pending set was resolved from the restored DB's own tracking table.
        assert_eq!(
            migrated_dsn.lock().expect("dsn").as_deref(),
            Some(THROWAWAY_DSN),
            "the rehearsal must migrate the throwaway DB, not the live one"
        );
        assert_eq!(report.applied.len(), 2);
        assert!(report.teardown_warning.is_none());

        // Provision (seeded from the live DSN) happened, and teardown always runs.
        let calls = calls.lock().expect("calls").clone();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(
            calls[0].starts_with(&format!("provision:{LIVE_DSN}")),
            "{calls:?}"
        );
        assert_eq!(calls[1], "teardown:app_fraisier_rehearsal");
    }

    #[tokio::test]
    async fn tears_down_even_when_the_rehearsed_migration_fails() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let db = FakeDb {
            calls: Arc::clone(&calls),
            teardown_fails: false,
        };
        let migration = RecordingMigration {
            migrated_dsn: Arc::new(Mutex::new(None)),
            fail: true,
        };

        let err = run_restore_rehearsal(&db, &migration, &live_ctx(), &BackupSource::FreshDump)
            .await
            .expect_err("a broken pending chain fails the rehearsal");
        assert!(matches!(err, RehearsalError::Migrate(_)), "got {err:?}");

        // The throwaway DB is still torn down after a failed rehearsal.
        let calls = calls.lock().expect("calls").clone();
        assert!(
            calls.iter().any(|c| c == "teardown:app_fraisier_rehearsal"),
            "teardown must run even on failure: {calls:?}"
        );
    }

    #[tokio::test]
    async fn a_clean_rehearsal_with_a_failed_teardown_warns_but_passes() {
        let db = FakeDb {
            calls: Arc::new(Mutex::new(Vec::new())),
            teardown_fails: true,
        };
        let migration = RecordingMigration {
            migrated_dsn: Arc::new(Mutex::new(None)),
            fail: false,
        };

        let report = run_restore_rehearsal(&db, &migration, &live_ctx(), &BackupSource::FreshDump)
            .await
            .expect("a clean rehearsal passes even if teardown fails");
        assert!(
            report.teardown_warning.is_some(),
            "a leaked throwaway DB must be surfaced as a warning"
        );
    }
}
