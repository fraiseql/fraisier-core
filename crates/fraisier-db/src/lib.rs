//! # fraisier-db
//!
//! Generic-PostgreSQL database lifecycle operations for fraisier: **backup**
//! (`pg_dump`), **restore** (`pg_restore`), and **reset** (drop user schemas).
//! These operate at the Postgres level and work regardless of which migration
//! adapter a deploy uses — migrations themselves stay with the adapter (the
//! Confiture model); this crate owns only the dump/restore/wipe lifecycle.
//!
//! ## Secrets never reach argv (Decision 5)
//!
//! A connection DSN carries the password, so it must not appear on a command
//! line (`ps` would leak it). [`PgConn::parse`] decomposes a DSN into its parts
//! and [`PgConn::pg_env`] exposes them as the standard libpq `PG*` environment
//! variables. The command builders set that environment on the child and pass
//! **no** connection string on argv; only non-secret values (the database name a
//! `pg_restore` needs as its target) ever appear as arguments.

use std::ffi::OsString;
use std::fmt::Write as _;

use percent_encoding::percent_decode_str;
use url::Url;

/// An error from parsing a DSN or running a database operation.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// The DSN could not be parsed as a URL.
    #[error("could not parse the database DSN: {0}")]
    Parse(String),
    /// The DSN's scheme is not `postgres`/`postgresql`. The generic-Postgres
    /// lifecycle ops only work against PostgreSQL.
    #[error(
        "database lifecycle ops require a postgres:// DSN, but the configured one uses '{0}://'"
    )]
    UnsupportedScheme(String),
    /// The DSN has no database name, which these ops require.
    #[error("the database DSN does not name a database (expected postgres://…/<dbname>)")]
    MissingDatabase,
}

/// PostgreSQL connection parameters resolved from a DSN.
///
/// Construct one with [`PgConn::parse`]. The fields map onto libpq's `PG*`
/// environment variables via [`PgConn::pg_env`]; the password is never rendered
/// onto a command line (see [`PgConn::redacted`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgConn {
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    password: Option<String>,
    database: String,
    sslmode: Option<String>,
}

/// Percent-decode a DSN component (username / password / path) into a plain
/// string, lossily for any non-UTF-8 bytes.
fn decode(component: &str) -> String {
    percent_decode_str(component)
        .decode_utf8_lossy()
        .into_owned()
}

impl PgConn {
    /// Parse a PostgreSQL DSN (e.g. `postgresql://user:pass@host:5432/db?sslmode=require`).
    ///
    /// The username, password, and database name are percent-decoded so a
    /// password containing reserved characters (`@`, `/`, …) round-trips intact.
    ///
    /// # Errors
    /// - [`DbError::Parse`] if the string is not a valid URL.
    /// - [`DbError::UnsupportedScheme`] if the scheme is not `postgres`/`postgresql`.
    /// - [`DbError::MissingDatabase`] if no database name is present.
    pub fn parse(dsn: &str) -> Result<Self, DbError> {
        let url = Url::parse(dsn).map_err(|error| DbError::Parse(error.to_string()))?;
        let scheme = url.scheme();
        if scheme != "postgres" && scheme != "postgresql" {
            return Err(DbError::UnsupportedScheme(scheme.to_owned()));
        }

        let host = url.host_str().filter(|h| !h.is_empty()).map(decode);
        let port = url.port();
        let user = {
            let raw = url.username();
            (!raw.is_empty()).then(|| decode(raw))
        };
        let password = url.password().map(decode);
        let database = decode(url.path().trim_start_matches('/'));
        if database.is_empty() {
            return Err(DbError::MissingDatabase);
        }
        let sslmode = url
            .query_pairs()
            .find(|(key, _)| key == "sslmode")
            .map(|(_, value)| value.into_owned());

        Ok(Self {
            host,
            port,
            user,
            password,
            database,
            sslmode,
        })
    }

    /// The target database name.
    #[must_use]
    pub fn database(&self) -> &str {
        &self.database
    }

    /// The libpq `PG*` environment variables for this connection — every value
    /// that is set, including `PGPASSWORD`. Pass these to a child process's
    /// environment so no secret appears on its argv.
    #[must_use]
    pub fn pg_env(&self) -> Vec<(OsString, OsString)> {
        let mut env: Vec<(OsString, OsString)> = Vec::new();
        let mut put =
            |key: &str, value: String| env.push((OsString::from(key), OsString::from(value)));
        if let Some(host) = &self.host {
            put("PGHOST", host.clone());
        }
        if let Some(port) = self.port {
            put("PGPORT", port.to_string());
        }
        if let Some(user) = &self.user {
            put("PGUSER", user.clone());
        }
        if let Some(password) = &self.password {
            put("PGPASSWORD", password.clone());
        }
        put("PGDATABASE", self.database.clone());
        if let Some(sslmode) = &self.sslmode {
            put("PGSSLMODE", sslmode.clone());
        }
        env
    }

    /// A password-free rendering of the connection, safe to print in plans/logs.
    #[must_use]
    pub fn redacted(&self) -> String {
        let mut out = String::from("postgres://");
        if let Some(user) = &self.user {
            out.push_str(user);
            out.push('@');
        }
        if let Some(host) = &self.host {
            out.push_str(host);
        }
        if let Some(port) = self.port {
            let _ = write!(out, ":{port}");
        }
        out.push('/');
        out.push_str(&self.database);
        if let Some(sslmode) = &self.sslmode {
            let _ = write!(out, "?sslmode={sslmode}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{DbError, PgConn};
    use std::ffi::OsString;

    fn env_of(conn: &PgConn) -> std::collections::BTreeMap<String, String> {
        conn.pg_env()
            .into_iter()
            .map(|(k, v)| {
                (
                    k.into_string().expect("utf8 key"),
                    v.into_string().expect("utf8 val"),
                )
            })
            .collect()
    }

    #[test]
    fn parses_a_full_dsn_into_pg_env() {
        let conn =
            PgConn::parse("postgresql://alice:s3cret@db.example.com:6432/shop?sslmode=require")
                .expect("parses");
        assert_eq!(conn.database(), "shop");
        let env = env_of(&conn);
        assert_eq!(env["PGHOST"], "db.example.com");
        assert_eq!(env["PGPORT"], "6432");
        assert_eq!(env["PGUSER"], "alice");
        assert_eq!(env["PGPASSWORD"], "s3cret");
        assert_eq!(env["PGDATABASE"], "shop");
        assert_eq!(env["PGSSLMODE"], "require");
    }

    #[test]
    fn percent_encoded_password_round_trips() {
        // A password with reserved characters (`@`, `/`) must decode exactly —
        // this is the load-bearing secret-handling case.
        let conn = PgConn::parse("postgres://u:p%40ss%2Fword@h/db").expect("parses");
        let env = env_of(&conn);
        assert_eq!(env["PGPASSWORD"], "p@ss/word");
    }

    #[test]
    fn minimal_dsn_sets_only_host_and_database() {
        let conn = PgConn::parse("postgres://localhost/mydb").expect("parses");
        let env = env_of(&conn);
        assert_eq!(env["PGHOST"], "localhost");
        assert_eq!(env["PGDATABASE"], "mydb");
        assert!(!env.contains_key("PGPORT"), "no port → no PGPORT");
        assert!(!env.contains_key("PGUSER"), "no user → no PGUSER");
        assert!(
            !env.contains_key("PGPASSWORD"),
            "no password → no PGPASSWORD"
        );
    }

    #[test]
    fn rejects_non_postgres_schemes() {
        for dsn in ["mysql://h/db", "sqlite:///tmp/x.db"] {
            let err = PgConn::parse(dsn).expect_err("non-postgres scheme rejected");
            assert!(
                matches!(err, DbError::UnsupportedScheme(_)),
                "{dsn}: {err:?}"
            );
        }
    }

    #[test]
    fn requires_a_database_name() {
        let err = PgConn::parse("postgres://user@host:5432/").expect_err("no dbname");
        assert!(matches!(err, DbError::MissingDatabase), "{err:?}");
    }

    #[test]
    fn redacted_never_shows_the_password() {
        let conn = PgConn::parse("postgresql://alice:s3cret@db:5432/shop?sslmode=require")
            .expect("parses");
        let shown = conn.redacted();
        assert!(!shown.contains("s3cret"), "password leaked: {shown}");
        assert!(shown.contains("alice"), "user shown: {shown}");
        assert!(shown.contains("db:5432/shop"), "host/db shown: {shown}");
    }

    #[test]
    fn pg_env_values_are_os_strings() {
        // The env is handed to Command::env as OsString pairs.
        let conn = PgConn::parse("postgres://localhost/db").expect("parses");
        let pairs: Vec<(OsString, OsString)> = conn.pg_env();
        assert!(pairs.iter().any(|(k, _)| k == "PGDATABASE"));
    }
}
