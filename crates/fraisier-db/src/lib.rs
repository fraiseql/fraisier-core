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
use std::path::Path;
use std::process::Command;

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};
use url::Url;

/// The URL characters that must be percent-encoded inside a DSN's userinfo
/// (username / password) so a rendered DSN round-trips back through [`PgConn::parse`].
const USERINFO: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/')
    .add(b':')
    .add(b'@')
    .add(b'\\')
    .add(b'[')
    .add(b']')
    .add(b'%');

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
    /// A database name handed to [`create_database_command`] /
    /// [`drop_database_command`] is not a safe SQL identifier.
    #[error(
        "'{0}' is not a valid database name (expected 1–63 chars of [A-Za-z0-9_], \
         starting with a letter or underscore)"
    )]
    InvalidDatabaseName(String),
    /// A database tool (`pg_dump`/`pg_restore`/`psql`) could not be launched.
    #[error("could not run `{program}` (is it installed and on PATH?): {source}")]
    Spawn {
        /// The program that failed to spawn.
        program: String,
        /// The underlying spawn error.
        source: std::io::Error,
    },
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

    /// A clone of this connection pointing at a different database on the same
    /// server (same host/port/user/password/sslmode). Used to reach a maintenance
    /// database (`postgres`) and to address a throwaway rehearsal database.
    #[must_use]
    pub fn with_database(&self, database: impl Into<String>) -> Self {
        Self {
            database: database.into(),
            ..self.clone()
        }
    }

    /// The full DSN **including the password**, percent-encoded so it round-trips
    /// back through [`PgConn::parse`]. Carries a secret — keep it in-process; never
    /// log it or place it on argv. For display use [`Self::redacted`].
    #[must_use]
    pub fn dsn(&self) -> String {
        let mut out = String::from("postgres://");
        if let Some(user) = &self.user {
            out.push_str(&utf8_percent_encode(user, USERINFO).to_string());
            if let Some(password) = &self.password {
                out.push(':');
                out.push_str(&utf8_percent_encode(password, USERINFO).to_string());
            }
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

/// SQL that drops every user schema and recreates an empty `public`.
///
/// This is the generic "reset to empty database" step; after running it the
/// caller re-applies migrations through the migration adapter (this crate does
/// not migrate). System schemas (`pg_catalog`, `information_schema`,
/// `pg_toast*`, `pg_temp*`) are left untouched, and `%I` quoting in the `DO`
/// block makes the drop safe for any schema name.
pub const RESET_SQL: &str = "\
DO $$
DECLARE schema_name text;
BEGIN
  FOR schema_name IN
    SELECT nspname FROM pg_namespace
    WHERE nspname NOT IN ('pg_catalog', 'information_schema')
      AND nspname NOT LIKE 'pg\\_toast%'
      AND nspname NOT LIKE 'pg\\_temp%'
  LOOP
    EXECUTE format('DROP SCHEMA IF EXISTS %I CASCADE', schema_name);
  END LOOP;
END $$;
CREATE SCHEMA IF NOT EXISTS public;";

/// Build the `pg_dump` command for a custom-format (`-Fc`) backup to `out`.
///
/// The connection comes entirely from [`PgConn::pg_env`] (set on the child's
/// environment); the only argv entries are non-secret flags and the output path.
/// `-w` makes it non-interactive (never prompt for a password).
#[must_use]
pub fn backup_command(conn: &PgConn, out: &Path) -> Command {
    let mut command = Command::new("pg_dump");
    command.envs(conn.pg_env());
    command.arg("-Fc").arg("-w").arg("-f").arg(out);
    command
}

/// Build the `pg_restore` command restoring `archive` into the database.
///
/// With `clean`, existing objects are dropped first (`--clean --if-exists`). The
/// target database name (not a secret) is the only connection value on argv;
/// everything else comes from [`PgConn::pg_env`].
#[must_use]
pub fn restore_command(conn: &PgConn, archive: &Path, clean: bool) -> Command {
    let mut command = Command::new("pg_restore");
    command.envs(conn.pg_env());
    command.arg("-w").arg("-d").arg(conn.database());
    if clean {
        command.arg("--clean").arg("--if-exists");
    }
    command.arg(archive);
    command
}

/// Build a non-interactive `psql` command that runs `sql` with
/// `ON_ERROR_STOP=1` (so a failing statement makes psql exit non-zero) and
/// skips `~/.psqlrc` (`-X`) for reproducibility.
#[must_use]
pub fn psql_command(conn: &PgConn, sql: &str) -> Command {
    let mut command = Command::new("psql");
    command.envs(conn.pg_env());
    command
        .arg("-w")
        .arg("-X")
        .arg("-v")
        .arg("ON_ERROR_STOP=1")
        .arg("-c")
        .arg(sql);
    command
}

/// Whether `name` is a safe, unquoted-ish PostgreSQL database identifier: 1–63
/// characters of `[A-Za-z0-9_]` starting with a letter or underscore. Names that
/// pass can be double-quoted into DDL without any injection risk.
fn validate_db_name(name: &str) -> Result<(), DbError> {
    let mut chars = name.chars();
    let head_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if head_ok && rest_ok && name.len() <= 63 {
        Ok(())
    } else {
        Err(DbError::InvalidDatabaseName(name.to_owned()))
    }
}

/// Build a `psql` command that runs `CREATE DATABASE "<new_db>"`.
///
/// Runs against the `maintenance` connection (point it at a database *other* than
/// `new_db`, e.g. `postgres`). `new_db` is validated as a safe identifier first.
///
/// # Errors
/// [`DbError::InvalidDatabaseName`] if `new_db` is not a safe identifier.
pub fn create_database_command(maintenance: &PgConn, new_db: &str) -> Result<Command, DbError> {
    validate_db_name(new_db)?;
    Ok(psql_command(
        maintenance,
        &format!("CREATE DATABASE \"{new_db}\""),
    ))
}

/// Build a `psql` command that runs `DROP DATABASE IF EXISTS "<db>" WITH (FORCE)`.
///
/// Runs against the `maintenance` connection (point it at a database *other* than
/// `db`). `FORCE` (PostgreSQL 13+) disconnects any lingering sessions so a
/// throwaway rehearsal database can always be reaped. `db` is validated first.
///
/// # Errors
/// [`DbError::InvalidDatabaseName`] if `db` is not a safe identifier.
pub fn drop_database_command(maintenance: &PgConn, db: &str) -> Result<Command, DbError> {
    validate_db_name(db)?;
    Ok(psql_command(
        maintenance,
        &format!("DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE)"),
    ))
}

/// The captured result of a finished database tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The process exit code, or `None` if it was killed by a signal.
    pub code: Option<i32>,
    /// Captured standard output (lossy UTF-8).
    pub stdout: String,
    /// Captured standard error (lossy UTF-8).
    pub stderr: String,
}

impl Outcome {
    /// Whether the process exited with code 0.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.code == Some(0)
    }
}

/// Run a built command to completion, capturing its output.
///
/// # Errors
/// [`DbError::Spawn`] if the program cannot be launched (e.g. the tool is not
/// installed). A non-zero exit is **not** an error here — it is reported in the
/// returned [`Outcome`] so the caller decides what a failure means.
pub async fn run(command: Command) -> Result<Outcome, DbError> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = tokio::process::Command::from(command)
        .output()
        .await
        .map_err(|source| DbError::Spawn { program, source })?;
    Ok(Outcome {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::{create_database_command, drop_database_command, DbError, PgConn};
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
    fn rendered_dsn_round_trips_through_parse_including_reserved_chars() {
        // A reserved-char password must survive render → re-parse intact, so a
        // throwaway DSN handed to a migration adapter resolves to the same secret.
        let conn =
            PgConn::parse("postgres://u:p%40ss%2Fword@h:5432/db?sslmode=require").expect("parses");
        let reparsed = PgConn::parse(&conn.dsn()).expect("rendered DSN re-parses");
        assert_eq!(conn, reparsed);
        assert_eq!(env_of(&reparsed)["PGPASSWORD"], "p@ss/word");
    }

    #[test]
    fn with_database_swaps_only_the_database_name() {
        let conn = PgConn::parse("postgres://alice:pw@h:5432/app?sslmode=require").expect("parses");
        let maintenance = conn.with_database("postgres");
        assert_eq!(maintenance.database(), "postgres");
        let env = env_of(&maintenance);
        assert_eq!(env["PGHOST"], "h");
        assert_eq!(env["PGUSER"], "alice");
        assert_eq!(env["PGDATABASE"], "postgres");
    }

    #[test]
    fn create_and_drop_validate_the_database_name() {
        let conn = PgConn::parse("postgres://u:pw@h/postgres").expect("parses");
        // A safe name builds a psql command carrying the quoted DDL.
        let create = create_database_command(&conn, "app_fraisier_rehearsal_1").expect("valid");
        let args: Vec<String> = create
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.iter()
                .any(|a| a == "CREATE DATABASE \"app_fraisier_rehearsal_1\""),
            "{args:?}"
        );
        assert!(drop_database_command(&conn, "app_fraisier_rehearsal_1").is_ok());

        // Injection attempts and malformed names are rejected before any DDL.
        for bad in [
            "a\"; DROP DATABASE postgres;--",
            "has space",
            "1leading",
            "",
        ] {
            assert!(
                matches!(
                    create_database_command(&conn, bad),
                    Err(DbError::InvalidDatabaseName(_))
                ),
                "name {bad:?} must be rejected"
            );
        }
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

    use super::{backup_command, psql_command, restore_command, RESET_SQL};
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::process::Command;

    fn args_of(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn envs_of(command: &Command) -> BTreeMap<String, String> {
        command
            .get_envs()
            .filter_map(|(k, v)| {
                v.map(|v| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect()
    }

    /// The shared invariant: a built command must carry the connection in its
    /// environment, never on argv (Decision 5).
    fn assert_no_secret_on_argv(command: &Command, password: &str) {
        for arg in args_of(command) {
            assert!(!arg.contains(password), "password leaked onto argv: {arg}");
        }
        assert_eq!(
            envs_of(command).get("PGPASSWORD").map(String::as_str),
            Some(password),
            "the password must travel as PGPASSWORD in the environment",
        );
    }

    fn conn_with_secret() -> PgConn {
        PgConn::parse("postgresql://alice:s3cret@db.example.com:6432/shop").expect("parses")
    }

    #[test]
    fn backup_builds_pg_dump_with_custom_format() {
        let conn = conn_with_secret();
        let command = backup_command(&conn, Path::new("/backups/shop.pgdump"));
        assert_eq!(command.get_program().to_string_lossy(), "pg_dump");
        let args = args_of(&command);
        assert!(args.contains(&"-Fc".to_owned()), "custom format: {args:?}");
        assert!(args.contains(&"-f".to_owned()), "output flag: {args:?}");
        assert!(
            args.contains(&"/backups/shop.pgdump".to_owned()),
            "output path: {args:?}"
        );
        let env = envs_of(&command);
        assert_eq!(env["PGDATABASE"], "shop");
        assert_no_secret_on_argv(&command, "s3cret");
    }

    #[test]
    fn restore_targets_the_database_and_can_clean() {
        let conn = conn_with_secret();

        let plain = restore_command(&conn, Path::new("/backups/shop.pgdump"), false);
        assert_eq!(plain.get_program().to_string_lossy(), "pg_restore");
        let args = args_of(&plain);
        assert!(args.contains(&"-d".to_owned()), "target db flag: {args:?}");
        assert!(
            args.contains(&"shop".to_owned()),
            "target db name: {args:?}"
        );
        assert!(
            args.contains(&"/backups/shop.pgdump".to_owned()),
            "archive path: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "--clean"),
            "no clean without the flag: {args:?}"
        );
        assert_no_secret_on_argv(&plain, "s3cret");

        let clean = restore_command(&conn, Path::new("/backups/shop.pgdump"), true);
        let args = args_of(&clean);
        assert!(args.contains(&"--clean".to_owned()), "clean: {args:?}");
        assert!(
            args.contains(&"--if-exists".to_owned()),
            "if-exists: {args:?}"
        );
    }

    #[test]
    fn psql_runs_sql_with_on_error_stop() {
        let conn = conn_with_secret();
        let command = psql_command(&conn, "SELECT 1");
        assert_eq!(command.get_program().to_string_lossy(), "psql");
        let args = args_of(&command);
        assert!(args.contains(&"-c".to_owned()), "command flag: {args:?}");
        assert!(args.contains(&"SELECT 1".to_owned()), "sql arg: {args:?}");
        assert!(
            args.iter().any(|a| a.contains("ON_ERROR_STOP=1")),
            "on-error-stop set: {args:?}"
        );
        assert_no_secret_on_argv(&command, "s3cret");
    }

    #[test]
    fn reset_sql_drops_user_schemas_and_recreates_public() {
        assert!(RESET_SQL.contains("DROP SCHEMA"), "drops schemas");
        assert!(
            RESET_SQL.contains("CREATE SCHEMA IF NOT EXISTS public"),
            "recreates public"
        );
        assert!(RESET_SQL.contains("pg_catalog"), "spares system schemas");
    }

    #[tokio::test]
    async fn run_reports_a_spawn_failure_clearly() {
        // A program that does not exist surfaces DbError::Spawn, not a panic.
        let command = Command::new("fraisier-db-no-such-tool-xyz");
        let err = super::run(command).await.expect_err("spawn fails");
        assert!(matches!(err, super::DbError::Spawn { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn run_captures_output_and_exit_code() {
        // `true` exits 0 with no output; a portable, dependency-free smoke test.
        let outcome = super::run(Command::new("true")).await.expect("runs");
        assert!(outcome.succeeded(), "true exits 0: {outcome:?}");
    }
}
