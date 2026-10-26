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

/// The SQL asking whether the connecting role may set [`MARKER_SETTING`]
/// (PostgreSQL 15+): one JSON object, `user` and `set`.
#[must_use]
pub fn marker_privilege_sql() -> String {
    format!(
        "SELECT json_build_object('user', current_user, 'set', \
         has_parameter_privilege({}, 'SET'))",
        literal(MARKER_SETTING)
    )
}

/// Why [`marker_privilege_sql`]'s reply rules the refresh out, with the GRANT
/// that fixes it, or `None`.
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
        "the role \"{user}\" may not set {MARKER_SETTING}, so it could not mark a \
         database again. A superuser runs, once: \
         GRANT SET ON PARAMETER {MARKER_SETTING} TO \"{user}\""
    )))
}

/// The extensions `listing` (a `pg_restore --list` table of contents) creates.
#[must_use]
pub fn archive_extensions(listing: &str) -> Vec<String> {
    // `<id>; <oid> <oid> EXTENSION - <name>`; comments start with `;`.
    listing
        .lines()
        .filter(|line| !line.starts_with(';'))
        .filter_map(|line| {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            (tokens.get(3..5) == Some(&["EXTENSION", "-"][..]))
                .then(|| tokens.get(5).map(|name| (*name).to_owned()))
                .flatten()
        })
        .collect()
}

/// The SQL asking whether the connecting role could create `extensions` in a
/// fresh database. It prints one JSON object: `superuser`, `missing` (not
/// available on the server) and `untrusted`.
#[must_use]
pub fn extensions_sql(extensions: &[String]) -> String {
    let names = extensions
        .iter()
        .map(|name| literal(name))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "WITH wanted(name) AS (SELECT unnest(ARRAY[{names}]::text[])) \
         SELECT json_build_object(\
         'superuser', (SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname = current_user), \
         'missing', COALESCE((SELECT json_agg(w.name) FROM wanted w WHERE NOT EXISTS \
           (SELECT 1 FROM pg_catalog.pg_available_extensions a WHERE a.name = w.name)), '[]'), \
         'untrusted', COALESCE((SELECT json_agg(w.name) FROM wanted w WHERE NOT EXISTS \
           (SELECT 1 FROM pg_catalog.pg_available_extension_versions v \
            WHERE v.name = w.name AND v.trusted)), '[]'))"
    )
}

/// Why [`extensions_sql`]'s reply rules the restore out, or `None`.
///
/// # Errors
/// The reply is not the JSON object the query prints.
pub fn extensions_problem(stdout: &str) -> Result<Option<String>, String> {
    let reply: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|error| format!("the extension query did not print its JSON: {error}"))?;
    let names = |key: &str| -> Vec<String> {
        reply[key]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|n| n.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let missing = names("missing");
    if !missing.is_empty() {
        return Ok(Some(format!(
            "the archive creates extension(s) this server does not have: {}",
            missing.join(", ")
        )));
    }
    let untrusted = names("untrusted");
    if !untrusted.is_empty() && reply["superuser"].as_bool() != Some(true) {
        return Ok(Some(format!(
            "the archive creates extension(s) only a superuser can create, and this role is \
             not one: {}. The restore would load the tables and fail without them",
            untrusted.join(", ")
        )));
    }
    Ok(None)
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

    use super::{
        archive_extensions, extensions_problem, extensions_sql, marker_privilege_problem,
        marker_privilege_sql, marker_sql, parse_marker, restore_command, set_marker_command,
        MARKER_SETTING,
    };
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

    #[test]
    fn the_listing_names_the_extensions_the_archive_creates() {
        let listing = ";\n; Archive created at 2026-10-26\n;\n\
                       2; 3079 16390 EXTENSION - pg_tviews \n\
                       3; 0 0 COMMENT - EXTENSION pg_tviews \n\
                       4; 3079 16391 EXTENSION - pgcrypto \n\
                       220; 1259 16400 TABLE public tb_item app\n";
        assert_eq!(archive_extensions(listing), vec!["pg_tviews", "pgcrypto"]);
        assert_eq!(
            archive_extensions("220; 1259 16400 TABLE public t app\n"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_extension_query_quotes_each_name() {
        let sql = extensions_sql(&["pg_tviews".to_owned(), "o'dd".to_owned()]);
        assert!(sql.contains("'pg_tviews'"), "{sql}");
        assert!(sql.contains("'o''dd'"), "{sql}");
        assert!(sql.contains("pg_available_extension_versions"), "{sql}");
    }

    #[test]
    fn an_untrusted_extension_needs_a_superuser() {
        let reply = r#"{"superuser": false, "missing": [], "untrusted": ["pg_tviews"]}"#;
        let problem = extensions_problem(reply).expect("json").expect("a problem");
        assert!(
            problem.contains("pg_tviews") && problem.contains("superuser"),
            "{problem}"
        );
        let reply = r#"{"superuser": true, "missing": [], "untrusted": ["pg_tviews"]}"#;
        assert_eq!(extensions_problem(reply).expect("json"), None);
    }

    #[test]
    fn an_extension_the_server_lacks_is_a_problem_for_anyone() {
        let reply = r#"{"superuser": true, "missing": ["postgis"], "untrusted": []}"#;
        let problem = extensions_problem(reply).expect("json").expect("a problem");
        assert!(problem.contains("postgis"), "{problem}");
        assert!(extensions_problem("not json").is_err());
        assert_eq!(
            extensions_problem(r#"{"superuser": false, "missing": [], "untrusted": []}"#)
                .expect("json"),
            None
        );
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
