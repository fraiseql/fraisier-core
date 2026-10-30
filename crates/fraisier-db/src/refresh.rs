//! The refresh marker: the database-wide setting that names the one config
//! allowed to replace a database with another environment's archive.
//!
//! `fraisier db mark-refresh-target` sets it once; `fraisier db refresh` drops
//! only a database that carries it, naming that config.

use std::path::Path;

use async_trait::async_trait;

use crate::{tviews, validate_db_name, DbError, PgConn};

/// The setting that marks a database as a refresh target.
pub const MARKER_SETTING: &str = "fraisier.refresh_target";

/// The first PostgreSQL version (`server_version_num`) on which a non-superuser
/// needs `GRANT SET ON PARAMETER` to set [`MARKER_SETTING`].
const PARAMETER_PRIVILEGES_SINCE: u32 = 150_000;

/// The marker value naming the config that may refresh a database.
#[must_use]
pub fn marker_value(fraise: &str, environment: &str) -> String {
    format!("{fraise}/{environment}")
}

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

/// The marker [`marker_sql`] printed, or `None` for an unmarked database.
#[must_use]
pub fn parse_marker(stdout: &str) -> Option<String> {
    let marker = stdout.trim();
    (!marker.is_empty()).then(|| marker.to_owned())
}

/// The SQL marking `db` as the refresh target `value`.
///
/// # Errors
/// [`DbError::InvalidDatabaseName`] if `db` is not a safe identifier.
pub fn set_marker_sql(db: &str, value: &str) -> Result<String, DbError> {
    validate_db_name(db)?;
    Ok(format!(
        "ALTER DATABASE \"{db}\" SET {MARKER_SETTING} = {}",
        literal(value)
    ))
}

/// The SQL asking whether the connecting role may set [`MARKER_SETTING`]: one
/// JSON object, `user` and `set`. Only valid on PostgreSQL 15 and later.
#[must_use]
pub fn marker_privilege_sql() -> String {
    format!(
        "SELECT json_build_object('user', current_user, 'set', \
         has_parameter_privilege({}, 'SET'))",
        literal(MARKER_SETTING)
    )
}

/// Why [`marker_privilege_sql`]'s reply rules out setting the marker, with the
/// GRANT that fixes it, or `None`.
///
/// # Errors
/// The reply is not the JSON object the query prints.
pub fn marker_privilege_problem(stdout: &str) -> Result<Option<String>, String> {
    let reply: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|error| format!("the marker privilege query did not print its JSON: {error}"))?;
    if reply["set"].as_bool() == Some(true) {
        return Ok(None);
    }
    let user = reply["user"]
        .as_str()
        .unwrap_or("<role>")
        .replace('"', "\"\"");
    Ok(Some(format!(
        "the role \"{user}\" may not set {MARKER_SETTING}, so it cannot mark a database \
         (or mark it again after a refresh re-creates it). A superuser runs, once: \
         GRANT SET ON PARAMETER {MARKER_SETTING} TO \"{user}\""
    )))
}

/// The marker operations, against the database a connection names.
/// [`crate::restore::Postgres`] runs them for real.
#[async_trait]
pub trait MarkerServer: Send + Sync {
    /// `conn`'s database's refresh marker, or `None` when unmarked.
    ///
    /// # Errors
    /// The query failed.
    async fn marker(&self, conn: &PgConn) -> Result<Option<String>, String>;

    /// Why `conn`'s role could not set the marker, or `None` when it can.
    ///
    /// # Errors
    /// The query failed.
    async fn marker_privilege(&self, conn: &PgConn) -> Result<Option<String>, String>;

    /// Mark `conn`'s database as the refresh target `value`.
    ///
    /// # Errors
    /// The marker was not set.
    async fn set_marker(&self, conn: &PgConn, value: &str) -> Result<(), String>;
}

#[async_trait]
impl MarkerServer for crate::restore::Postgres {
    async fn marker(&self, conn: &PgConn) -> Result<Option<String>, String> {
        let sql = marker_sql(conn.database()).map_err(|error| error.to_string())?;
        Ok(parse_marker(
            &tviews::query(Path::new("psql"), conn, &sql).await?,
        ))
    }

    async fn marker_privilege(&self, conn: &PgConn) -> Result<Option<String>, String> {
        let psql = Path::new("psql");
        let version = tviews::query(psql, conn, "SHOW server_version_num").await?;
        let version: u32 = version
            .trim()
            .parse()
            .map_err(|_| format!("unreadable server_version_num: {version:?}"))?;
        if version < PARAMETER_PRIVILEGES_SINCE {
            return Ok(None);
        }
        marker_privilege_problem(&tviews::query(psql, conn, &marker_privilege_sql()).await?)
    }

    async fn set_marker(&self, conn: &PgConn, value: &str) -> Result<(), String> {
        let sql = set_marker_sql(conn.database(), value).map_err(|error| error.to_string())?;
        tviews::query(Path::new("psql"), conn, &sql).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        marker_privilege_problem, marker_privilege_sql, marker_sql, marker_value, parse_marker,
        set_marker_sql, MARKER_SETTING,
    };

    #[test]
    fn the_marker_names_the_fraise_and_environment() {
        assert_eq!(marker_value("checkout", "staging"), "checkout/staging");
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
        let sql = set_marker_sql("checkout_staging", "it's/staging").expect("valid name");
        assert_eq!(
            sql,
            "ALTER DATABASE \"checkout_staging\" SET fraisier.refresh_target = 'it''s/staging'"
        );
        assert!(set_marker_sql("x\"y", "a/b").is_err());
    }

    #[test]
    fn the_marker_privilege_is_asked_of_the_setting() {
        let sql = marker_privilege_sql();
        assert!(
            sql.contains("has_parameter_privilege('fraisier.refresh_target', 'SET')"),
            "{sql}"
        );
    }

    #[test]
    fn a_role_that_cannot_set_the_marker_is_told_the_grant() {
        let problem = marker_privilege_problem(r#"{"user": "app", "set": false}"#)
            .expect("json")
            .expect("a problem");
        assert!(
            problem.contains("GRANT SET ON PARAMETER fraisier.refresh_target TO \"app\""),
            "{problem}"
        );
        assert_eq!(
            marker_privilege_problem(r#"{"user": "app", "set": true}"#).expect("json"),
            None
        );
        assert!(marker_privilege_problem("nope").is_err());
    }
}
