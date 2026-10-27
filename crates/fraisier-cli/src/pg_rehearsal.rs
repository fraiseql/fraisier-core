//! The real Postgres-backed [`RehearsalDb`] and [`DrillDb`] behind the
//! restore-rehearsal preflight (`[migration].preflight_mode =
//! "restore_rehearsal"`) and `backup drill`.
//!
//! Both are thin callers of [`fraisier_db::restore`], which owns the identity,
//! the archive listing, the pg_tviews precheck and rebuild, and the teardown
//! guard. A **full** restore carries the migration-tracking rows with it, so the
//! pending set the rehearsal resolves matches a real restore + migrate.

use std::path::Path;

use async_trait::async_trait;
use fraisier_core::drill::{DrillDb, RestoredCopy, TviewRebuilt};
use fraisier_core::restore_rehearsal::{BackupSource, RehearsalDb, RehearsalError, ThrowawayDb};
use fraisier_db::restore::{self, Postgres, RestoreIdentity, Seed, Trigger};
use fraisier_db::{run, PgConn};

/// A throwaway-DB lifecycle over the live server. Every operation is derived
/// from the DSNs it is handed, plus the restore identity when
/// `[database].admin_url_env` names one and `trigger` may use it.
///
/// With an admin identity, the throwaway is created (owned by the app role),
/// restored into, and dropped as that identity: a pg_tviews archive restores
/// only as a superuser. Migrations still run as the app role, through the
/// returned DSN.
pub(crate) struct PgRehearsalDb {
    /// The env var holding the admin DSN, read when a throwaway is provisioned.
    pub(crate) admin_url_env: Option<String>,
    /// What started the run: a webhook-started one never resolves the admin.
    pub(crate) trigger: Trigger,
}

impl PgRehearsalDb {
    /// The identity a run reaching `app` restores as.
    fn identity(&self, app: PgConn) -> Result<RestoreIdentity, String> {
        RestoreIdentity::from_env(app, self.admin_url_env.as_deref(), self.trigger)
    }

    /// Restore `seed` into a throwaway off `live_dsn`, rebuilding its TVIEWs
    /// when `rebuild`.
    async fn fill(
        &self,
        live_dsn: &str,
        seed: Seed<'_>,
        rebuild: bool,
    ) -> Result<restore::Throwaway, String> {
        let live = PgConn::parse(live_dsn).map_err(|e| format!("invalid database DSN: {e}"))?;
        let identity = self.identity(live)?;
        restore::restore_throwaway(&Postgres, &identity, seed, rebuild)
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait]
impl RehearsalDb for PgRehearsalDb {
    async fn provision(
        &self,
        live_dsn: &str,
        backup: &BackupSource,
    ) -> Result<ThrowawayDb, RehearsalError> {
        let seed = match backup {
            BackupSource::Archive(path) => Seed::Archive(path),
            BackupSource::FreshDump => Seed::FreshDump,
        };
        // The rehearsal proves the migrations, not the TVIEW contents: it does
        // not rebuild, unlike a drill or `db restore`.
        let copy = self
            .fill(live_dsn, seed, false)
            .await
            .map_err(RehearsalError::Provision)?;
        Ok(ThrowawayDb {
            dsn: copy.app.dsn(),
            name: copy.name,
        })
    }

    async fn teardown(&self, db: &ThrowawayDb) -> Result<(), String> {
        let throwaway = PgConn::parse(&db.dsn).map_err(|e| e.to_string())?;
        let identity = self.identity(throwaway)?;
        restore::drop_throwaway(&Postgres, &identity, &db.name, None).await
    }
}

/// Run `sql` and return its single value.
pub(crate) async fn scalar(conn: &PgConn, sql: &str) -> Result<String, String> {
    let mut query = fraisier_db::psql_command(conn, sql);
    query.arg("-tA");
    let outcome = run(query).await.map_err(|e| e.to_string())?;
    if outcome.succeeded() {
        Ok(outcome.stdout.trim().to_owned())
    } else {
        Err(outcome.stderr.trim().to_owned())
    }
}

#[async_trait]
impl DrillDb for PgRehearsalDb {
    async fn restore_copy(&self, live_dsn: &str, archive: &Path) -> Result<RestoredCopy, String> {
        let copy = self.fill(live_dsn, Seed::Archive(archive), true).await?;
        Ok(RestoredCopy {
            db: ThrowawayDb {
                dsn: copy.app.dsn(),
                name: copy.name,
            },
            tviews_rebuilt: copy.tviews.map(|rebuild| {
                rebuild
                    .rebuilt
                    .into_iter()
                    .map(|r| TviewRebuilt {
                        entity: r.entity,
                        rows: r.rows,
                    })
                    .collect()
            }),
        })
    }

    fn live_database(&self, live_dsn: &str) -> Result<String, String> {
        PgConn::parse(live_dsn)
            .map(|live| live.database().to_owned())
            .map_err(|e| format!("invalid database DSN: {e}"))
    }

    async fn table_count(&self, db: &ThrowawayDb) -> Result<u64, String> {
        let throwaway = PgConn::parse(&db.dsn).map_err(|e| e.to_string())?;
        let count = scalar(
            &throwaway,
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
impl crate::commands::DrillServer for PgRehearsalDb {
    async fn live_bytes(&self, live_dsn: &str) -> Result<u64, String> {
        let live = PgConn::parse(live_dsn).map_err(|e| format!("invalid database DSN: {e}"))?;
        let size = scalar(&live, "SELECT pg_database_size(current_database())").await?;
        size.parse()
            .map_err(|_| format!("the database size was not a number: {size:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::PgRehearsalDb;
    use fraisier_core::drill::DrillDb;
    use fraisier_core::restore_rehearsal::{BackupSource, RehearsalDb, ThrowawayDb};

    const NO_ADMIN: PgRehearsalDb = PgRehearsalDb {
        admin_url_env: None,
        trigger: fraisier_db::restore::Trigger::Operator,
    };

    #[test]
    fn the_drill_and_the_primitive_agree_on_the_throwaway_marker() {
        // The drill checks the name before migrating; the primitive before
        // dropping. Both guards must recognise the same throwaways.
        assert_eq!(
            fraisier_core::drill::THROWAWAY_MARKER,
            fraisier_db::restore::THROWAWAY_MARKER
        );
    }

    #[test]
    fn the_live_database_is_the_one_its_dsn_names() {
        assert_eq!(
            NO_ADMIN.live_database("postgres://app:pw@db.internal:5432/shop?sslmode=require"),
            Ok("shop".to_owned())
        );
        assert!(NO_ADMIN.live_database("not a dsn").is_err());
    }

    #[tokio::test]
    async fn teardown_refuses_a_name_without_the_throwaway_marker() {
        // No server is reachable here: refusing must not need one.
        let refused = NO_ADMIN
            .teardown(&ThrowawayDb {
                dsn: "postgres://app:pw@127.0.0.1:1/shop".to_owned(),
                name: "shop".to_owned(),
            })
            .await
            .expect_err("an unmarked name is never dropped");
        assert!(refused.contains("_fraisier_rh_"), "{refused}");
    }

    /// End-to-end exercise of the real `pg_dump → CREATE DATABASE → pg_restore →
    /// DROP DATABASE` chain against a live Postgres. Ignored by default; set
    /// `FRAISIER_REHEARSAL_TEST_DSN` to a DSN whose role may create/drop databases
    /// and run with `cargo test -- --ignored`. (The migration-rehearsal logic over
    /// this lifecycle — including the Python-0.34 backup-behind-tracking scenario —
    /// is unit-tested in `fraisier_core::restore_rehearsal`; the full confiture
    /// chain runs on the fixture host per the finalize-phase validation checklist.)
    #[tokio::test]
    #[ignore = "requires a real Postgres reachable via FRAISIER_REHEARSAL_TEST_DSN"]
    async fn provisions_and_tears_down_a_throwaway_from_a_fresh_dump() {
        let Ok(dsn) = std::env::var("FRAISIER_REHEARSAL_TEST_DSN") else {
            eprintln!("skipped: set FRAISIER_REHEARSAL_TEST_DSN to a postgres DSN");
            return;
        };
        let db = NO_ADMIN;
        let throwaway = db
            .provision(&dsn, &BackupSource::FreshDump)
            .await
            .expect("provisioning a throwaway from a fresh dump succeeds");
        assert!(
            throwaway.name.contains("fraisier_rh"),
            "throwaway name should be recognizable: {}",
            throwaway.name
        );
        assert_ne!(
            throwaway.dsn, dsn,
            "the throwaway must be a distinct database"
        );
        db.teardown(&throwaway)
            .await
            .expect("the throwaway database is dropped");
    }
}
