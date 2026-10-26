//! The real Postgres-backed [`RehearsalDb`] used by the restore-rehearsal
//! preflight (`[migration].preflight_mode = "restore_rehearsal"`).
//!
//! It composes `fraisier-db`'s generic-Postgres lifecycle commands: provision a
//! freshly-named throwaway database on the live server, seed it from a full backup
//! (`pg_restore` of an archive, or a fresh `pg_dump` of the live DB), and drop it
//! afterwards. A **full** restore carries the migration-tracking rows with it, so
//! the pending set the rehearsal resolves matches a real restore + migrate.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use fraisier_core::drill::{DrillDb, THROWAWAY_MARKER};
use fraisier_core::restore_rehearsal::{BackupSource, RehearsalDb, RehearsalError, ThrowawayDb};
use fraisier_db::{
    backup_command, create_database_command, drop_database_command, restore_command, run, PgConn,
};

/// A throwaway-DB lifecycle backed by `pg_dump`/`pg_restore`/`psql` over the live
/// server. Stateless — every operation is derived from the DSNs it is handed.
pub(crate) struct PgRehearsalDb;

fn provision_error(msg: impl Into<String>) -> RehearsalError {
    RehearsalError::Provision(msg.into())
}

#[async_trait]
impl RehearsalDb for PgRehearsalDb {
    async fn provision(
        &self,
        live_dsn: &str,
        backup: &BackupSource,
    ) -> Result<ThrowawayDb, RehearsalError> {
        let live = PgConn::parse(live_dsn)
            .map_err(|e| provision_error(format!("invalid database DSN: {e}")))?;
        let name = throwaway_name(live.database());
        let maintenance = live.with_database("postgres");

        let create = create_database_command(&maintenance, &name)
            .map_err(|e| provision_error(e.to_string()))?;
        let created = run(create)
            .await
            .map_err(|e| provision_error(e.to_string()))?;
        if !created.succeeded() {
            return Err(provision_error(format!(
                "CREATE DATABASE failed: {}",
                created.stderr.trim()
            )));
        }

        let throwaway = live.with_database(name.clone());
        if let Err(error) = seed(&live, &throwaway, backup).await {
            // Best-effort cleanup of the half-provisioned database.
            let _ = teardown_named(&throwaway, &name).await;
            return Err(error);
        }
        Ok(ThrowawayDb {
            dsn: throwaway.dsn(),
            name,
        })
    }

    async fn teardown(&self, db: &ThrowawayDb) -> Result<(), String> {
        let throwaway = PgConn::parse(&db.dsn).map_err(|e| e.to_string())?;
        teardown_named(&throwaway, &db.name).await
    }
}

/// Seed `throwaway` from `backup`: restore an existing archive, or take a fresh
/// dump of `live` and restore that.
async fn seed(
    live: &PgConn,
    throwaway: &PgConn,
    backup: &BackupSource,
) -> Result<(), RehearsalError> {
    match backup {
        BackupSource::Archive(path) => restore_into(throwaway, path).await,
        BackupSource::FreshDump => {
            let tmp = temp_archive_path();
            let dumped = run(backup_command(live, &tmp))
                .await
                .map_err(|e| provision_error(e.to_string()))?;
            if !dumped.succeeded() {
                let _ = std::fs::remove_file(&tmp);
                return Err(provision_error(format!(
                    "pg_dump of the live DB failed: {}",
                    dumped.stderr.trim()
                )));
            }
            let result = restore_into(throwaway, &tmp).await;
            let _ = std::fs::remove_file(&tmp);
            result
        }
    }
}

/// Restore `archive` into the throwaway database (no `--clean`: the DB is empty).
async fn restore_into(throwaway: &PgConn, archive: &Path) -> Result<(), RehearsalError> {
    let restored = run(restore_command(throwaway, archive, false))
        .await
        .map_err(|e| provision_error(e.to_string()))?;
    if !restored.succeeded() {
        return Err(provision_error(format!(
            "pg_restore into the throwaway DB failed: {}",
            restored.stderr.trim()
        )));
    }
    Ok(())
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

/// Drop the throwaway database `name`, connecting via a maintenance database.
///
/// A name without [`THROWAWAY_MARKER`] is refused before anything runs: `DROP
/// DATABASE … WITH (FORCE)` drops any name that is a valid identifier.
async fn teardown_named(throwaway: &PgConn, name: &str) -> Result<(), String> {
    if !name.contains(THROWAWAY_MARKER) {
        return Err(format!(
            "refusing to drop '{name}': it does not carry the throwaway marker \
             '{THROWAWAY_MARKER}'"
        ));
    }
    let maintenance = throwaway.with_database("postgres");
    let drop = drop_database_command(&maintenance, name).map_err(|e| e.to_string())?;
    let dropped = run(drop).await.map_err(|e| e.to_string())?;
    if dropped.succeeded() {
        Ok(())
    } else {
        Err(format!(
            "DROP DATABASE {name} failed: {}",
            dropped.stderr.trim()
        ))
    }
}

/// A unique, valid throwaway database name derived from `base` (≤ 63 chars,
/// `[A-Za-z0-9_]`, leading letter). Process id + a high-resolution timestamp keep
/// concurrent deploys from colliding.
fn throwaway_name(base: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let pid = std::process::id();
    let cleaned: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(24)
        .collect();
    let head = if cleaned.starts_with(|c: char| c.is_ascii_alphabetic()) {
        cleaned
    } else {
        format!("db{cleaned}")
    };
    format!("{head}_fraisier_rh_{pid}_{nanos:x}")
        .chars()
        .take(63)
        .collect()
}

/// A temp file path for a fresh-dump archive (cleaned up by the caller).
fn temp_archive_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let pid = std::process::id();
    std::env::temp_dir().join(format!("fraisier-rehearsal-{pid}-{nanos:x}.dump"))
}

#[async_trait]
impl DrillDb for PgRehearsalDb {
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
    use super::{throwaway_name, PgRehearsalDb};
    use fraisier_core::drill::DrillDb;
    use fraisier_core::restore_rehearsal::{BackupSource, RehearsalDb, ThrowawayDb};

    #[test]
    fn the_live_database_is_the_one_its_dsn_names() {
        assert_eq!(
            PgRehearsalDb.live_database("postgres://app:pw@db.internal:5432/shop?sslmode=require"),
            Ok("shop".to_owned())
        );
        assert!(PgRehearsalDb.live_database("not a dsn").is_err());
    }

    #[tokio::test]
    async fn teardown_refuses_a_name_without_the_throwaway_marker() {
        // No server is reachable here: refusing must not need one.
        let refused = PgRehearsalDb
            .teardown(&ThrowawayDb {
                dsn: "postgres://app:pw@127.0.0.1:1/shop".to_owned(),
                name: "shop".to_owned(),
            })
            .await
            .expect_err("an unmarked name is never dropped");
        assert!(refused.contains("_fraisier_rh_"), "{refused}");
    }

    #[test]
    fn throwaway_name_is_a_valid_bounded_identifier() {
        for base in [
            "app",
            "shop_db",
            "9starts_with_digit",
            "",
            "weird-name.with.dots",
        ] {
            let name = throwaway_name(base);
            assert!(name.len() <= 63, "name {name:?} exceeds 63 chars");
            assert!(
                name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_'),
                "name {name:?} must start with a letter or underscore"
            );
            assert!(
                name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "name {name:?} has an invalid character"
            );
        }
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
        let db = PgRehearsalDb;
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
