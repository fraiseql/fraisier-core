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

/// Drop the throwaway database `name`, connecting via a maintenance database.
async fn teardown_named(throwaway: &PgConn, name: &str) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::{throwaway_name, PgRehearsalDb};
    use fraisier_core::restore_rehearsal::{BackupSource, RehearsalDb};

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
