//! The SQL and commands a staging refresh runs: reading and setting the refresh
//! marker, and restoring another environment's archive.

use std::path::Path;
use std::process::Command;

use crate::{psql_command, validate_db_name, DbError, PgConn};

/// The setting that marks a database as a refresh target.
pub const MARKER_SETTING: &str = "fraisier.refresh_target";

/// `'value'`, with embedded quotes doubled.
fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// The SQL printing `db`'s refresh marker, read from `pg_db_role_setting` (the
/// database-wide entry, not a per-role one); it prints nothing when unmarked.
///
/// # Errors
/// [`DbError::InvalidDatabaseName`] if `db` is not a safe identifier.
pub fn marker_sql(db: &str) -> Result<String, DbError> {
    validate_db_name(db)?;
    let prefix = format!("{MARKER_SETTING}=");
    Ok(format!(
        "SELECT substr(c, {}) FROM pg_catalog.pg_db_role_setting s \
         JOIN pg_catalog.pg_database d ON d.oid = s.setdatabase, unnest(s.setconfig) c \
         WHERE s.setrole = 0 AND d.datname = {} AND starts_with(c, {})",
        prefix.len() + 1,
        literal(db),
        literal(&prefix)
    ))
}

/// The marker `marker_sql` printed, or `None` for an unmarked database.
#[must_use]
pub fn parse_marker(stdout: &str) -> Option<String> {
    let marker = stdout.trim();
    (!marker.is_empty()).then(|| marker.to_owned())
}

/// A `psql` command that marks `db` as the refresh target `value`, run against
/// `maintenance` (any database on the same server).
///
/// # Errors
/// [`DbError::InvalidDatabaseName`] if `db` is not a safe identifier.
pub fn set_marker_command(maintenance: &PgConn, db: &str, value: &str) -> Result<Command, DbError> {
    validate_db_name(db)?;
    Ok(psql_command(
        maintenance,
        &format!(
            "ALTER DATABASE \"{db}\" SET {MARKER_SETTING} = {}",
            literal(value)
        ),
    ))
}

/// A `pg_restore` of another environment's `archive` into the empty database
/// `conn` names.
///
/// `--no-owner --no-acl`: the archive's owners and grants are the source
/// environment's roles, which need not exist here. Every restored object is
/// owned by the restoring role instead.
#[must_use]
pub fn restore_command(conn: &PgConn, archive: &Path) -> Command {
    let mut command = Command::new("pg_restore");
    command.envs(conn.pg_env());
    command
        .arg("-w")
        .arg("--no-owner")
        .arg("--no-acl")
        .arg("-d")
        .arg(conn.database())
        .arg(archive);
    command
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{marker_sql, parse_marker, restore_command, set_marker_command, MARKER_SETTING};
    use crate::PgConn;

    fn args(command: &std::process::Command) -> Vec<String> {
        command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn maintenance() -> PgConn {
        PgConn::parse("postgres://app:pw@db.internal/postgres").expect("dsn")
    }

    #[test]
    fn the_marker_is_read_from_the_database_wide_setting() {
        let sql = marker_sql("checkout_staging").expect("valid name");
        assert!(sql.contains("pg_db_role_setting"), "{sql}");
        assert!(sql.contains("setrole = 0"), "{sql}");
        assert!(sql.contains("'checkout_staging'"), "{sql}");
        assert!(sql.contains(MARKER_SETTING), "{sql}");
        assert!(marker_sql("bad name; drop").is_err());
    }

    #[test]
    fn an_empty_reply_is_an_unmarked_database() {
        assert_eq!(parse_marker("\n"), None);
        assert_eq!(
            parse_marker("checkout/staging\n").as_deref(),
            Some("checkout/staging")
        );
    }

    #[test]
    fn the_marker_is_set_on_the_database_with_its_value_quoted() {
        let command = set_marker_command(&maintenance(), "checkout_staging", "it's/staging")
            .expect("valid name");
        let sql = args(&command).last().cloned().expect("sql");
        assert_eq!(
            sql,
            "ALTER DATABASE \"checkout_staging\" SET fraisier.refresh_target = 'it''s/staging'"
        );
        assert!(set_marker_command(&maintenance(), "x\"y", "a/b").is_err());
    }

    #[test]
    fn a_refresh_restore_drops_the_source_environments_owners_and_grants() {
        let conn = PgConn::parse("postgres://app:pw@db.internal/checkout_staging").expect("dsn");
        let argv = args(&restore_command(&conn, Path::new("/b/a.pgdump")));
        assert!(argv.contains(&"--no-owner".to_owned()), "{argv:?}");
        assert!(argv.contains(&"--no-acl".to_owned()), "{argv:?}");
        assert!(!argv.contains(&"--clean".to_owned()), "{argv:?}");
        assert_eq!(argv.last().map(String::as_str), Some("/b/a.pgdump"));
        assert!(
            !argv.iter().any(|a| a.contains("pw")),
            "no password on argv"
        );
    }
}
