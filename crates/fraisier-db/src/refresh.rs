//! The refresh marker: the database-wide setting that names the one config
//! allowed to replace a database with another environment's archive.
//!
//! `fraisier db mark-refresh-target` sets it once; `fraisier db refresh` drops
//! only a database that carries it, naming that config.

use std::path::Path;

use async_trait::async_trait;

use crate::restore::{self, RestoreError, RestoreIdentity, RestoreServer, TviewRebuild};
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

/// The extensions `listing` (a `pg_restore --list` table of contents) creates.
#[must_use]
pub fn archive_extensions(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| toc_entry(line, &["EXTENSION", "-"]))
        .map(str::to_owned)
        .collect()
}

/// The object name of a TOC entry `<id>; <oid> <oid> <kind...> <name>` whose
/// kind tokens are `kind`; comment lines (`;`) never match.
fn toc_entry<'a>(line: &'a str, kind: &[&str]) -> Option<&'a str> {
    if line.starts_with(';') {
        return None;
    }
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let end = 3 + kind.len();
    (tokens.get(3..end) == Some(kind))
        .then(|| tokens.get(end).copied())
        .flatten()
}

/// `listing` without the entries that create, or comment on, `extensions`:
/// the restore's `-L` list once those extensions exist in the target. Every
/// other entry is kept, in order.
#[must_use]
fn without_extensions(listing: &str, extensions: &[String]) -> String {
    let created =
        |name: Option<&str>| name.is_some_and(|name| extensions.iter().any(|e| e == name));
    listing
        .lines()
        .filter(|line| {
            !created(toc_entry(line, &["EXTENSION", "-"]))
                && !created(toc_entry(line, &["COMMENT", "-", "EXTENSION"]))
        })
        .fold(String::new(), |mut kept, line| {
            kept.push_str(line);
            kept.push('\n');
            kept
        })
}

/// A restore split in two `pg_restore -L` lists: what the app role restores,
/// and the extensions' configuration data, which only the admin may write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreLists {
    /// Everything but what the admin already created (the extensions, their
    /// comments, the schemas they made) and the extensions' configuration data.
    pub app: String,
    /// `TABLE DATA` entries with no `TABLE` entry of their own: rows of a table
    /// an extension owns (`pg_extension_config_dump`).
    pub extension_data: String,
    /// The schemas the archive creates that already exist in the target,
    /// because an extension made them: the app role may need to reach them.
    pub made_schemas: Vec<String>,
}

/// Split `listing` for a restore into a database where the admin has already
/// created `extensions` and the schemas `existing_schemas` exist.
#[must_use]
pub fn split_listing(
    listing: &str,
    extensions: &[String],
    existing_schemas: &[String],
) -> RestoreLists {
    let tokens = |line: &str| -> Vec<String> {
        if line.starts_with(';') {
            return Vec::new();
        }
        line.split_whitespace().skip(3).map(str::to_owned).collect()
    };
    let tables: Vec<(String, String)> = listing
        .lines()
        .map(tokens)
        .filter(|t| t.len() >= 3 && t[0] == "TABLE" && t[1] != "DATA")
        .map(|t| (t[1].clone(), t[2].clone()))
        .collect();
    let existing = |name: &str| existing_schemas.iter().any(|schema| schema == name);
    let mut lists = RestoreLists {
        app: String::new(),
        extension_data: String::new(),
        made_schemas: Vec::new(),
    };
    for line in without_extensions(listing, extensions).lines() {
        let t = tokens(line);
        let words: Vec<&str> = t.iter().map(String::as_str).collect();
        let made_schema = match words.as_slice() {
            ["SCHEMA", "-", name, ..] if existing(name) => {
                lists.made_schemas.push((*name).to_owned());
                true
            }
            ["ACL" | "COMMENT", "-", "SCHEMA", name, ..] => existing(name),
            _ => false,
        };
        let extension_rows = match words.as_slice() {
            ["TABLE", "DATA", schema, name, ..] => {
                !tables.iter().any(|(s, n)| s == schema && n == name)
            }
            _ => false,
        };
        if extension_rows {
            lists.extension_data.push_str(line);
            lists.extension_data.push('\n');
        } else if !made_schema {
            lists.app.push_str(line);
            lists.app.push('\n');
        }
    }
    lists
}

/// The SQL listing every session connected to `db`, other than the one
/// asking, as `user (application, pid N)` lines.
///
/// # Errors
/// [`DbError::InvalidDatabaseName`] if `db` is not a safe identifier.
pub fn connections_sql(db: &str) -> Result<String, DbError> {
    validate_db_name(db)?;
    Ok(format!(
        "SELECT format('%s (%s, pid %s)', usename, \
         COALESCE(NULLIF(application_name, ''), 'no application name'), pid) \
         FROM pg_catalog.pg_stat_activity \
         WHERE datname = {} AND pid <> pg_backend_pid() ORDER BY pid",
        literal(db)
    ))
}

/// The database operations a refresh needs beyond a restore's and the
/// marker's. [`crate::restore::Postgres`] runs them for real.
#[async_trait]
pub trait RefreshServer: RestoreServer + MarkerServer {
    /// The sessions connected to `db`, as [`connections_sql`] prints them.
    ///
    /// # Errors
    /// The query failed.
    async fn connections(&self, maintenance: &PgConn, db: &str) -> Result<Vec<String>, String>;

    /// Why `conn`'s role could not create `extensions` in a new database (one
    /// the server lacks, or an untrusted one for a non-superuser), or `None`.
    ///
    /// # Errors
    /// The query failed.
    async fn extensions_problem(
        &self,
        conn: &PgConn,
        extensions: &[String],
    ) -> Result<Option<String>, String>;

    /// `conn`'s database's size in bytes (`pg_database_size`).
    ///
    /// # Errors
    /// The query failed.
    async fn database_bytes(&self, conn: &PgConn) -> Result<u64, String>;

    /// How many tables `conn`'s database holds outside the system schemas.
    ///
    /// # Errors
    /// The query failed.
    async fn table_count(&self, conn: &PgConn) -> Result<u64, String>;

    /// `CREATE EXTENSION` each of `extensions` in `conn`'s database.
    ///
    /// # Errors
    /// One was not created.
    async fn create_extensions(&self, conn: &PgConn, extensions: &[String]) -> Result<(), String>;

    /// The schemas `conn`'s database holds.
    ///
    /// # Errors
    /// The query failed.
    async fn schemas(&self, conn: &PgConn) -> Result<Vec<String>, String>;

    /// `GRANT` (or, with `revoke`, `REVOKE`) `privilege` on `schema` to `role`.
    ///
    /// # Errors
    /// The statement failed.
    async fn schema_privilege(
        &self,
        conn: &PgConn,
        privilege: SchemaPrivilege,
        schema: &str,
        role: &str,
        revoke: bool,
    ) -> Result<(), String>;

    /// `pg_restore` the entries `list` names from `archive` into `conn`'s
    /// database, with no owners or grants from the archive; as `role` when
    /// given, rows only when `data_only`. Returns what `pg_restore` warned.
    ///
    /// # Errors
    /// `pg_restore` exited non-zero, with its stderr: an error it "ignored" is
    /// still a failed refresh.
    async fn restore_list(
        &self,
        conn: &PgConn,
        archive: &Path,
        list: &str,
        role: Option<&str>,
        data_only: bool,
    ) -> Result<String, String>;
}

/// A schema privilege a refresh grants the app role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaPrivilege {
    /// Reach the objects in it: kept, since the app role owns objects there.
    Usage,
    /// Create objects in it: granted for the restore only, then revoked.
    Create,
}

impl SchemaPrivilege {
    const fn keyword(self) -> &'static str {
        match self {
            Self::Usage => "USAGE",
            Self::Create => "CREATE",
        }
    }
}

/// What a refresh restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refreshed {
    /// What `pg_restore` warned about while still succeeding.
    pub warnings: String,
    /// The extensions the restoring identity created before the restore.
    pub extensions: Vec<String>,
    /// The post-restore TVIEW rebuild, when the archive creates pg_tviews.
    pub tviews: Option<TviewRebuild>,
}

/// Replace the database `identity`'s app connection names with `archive`,
/// after checking it carries the refresh marker `marker`.
///
/// Everything that can refuse runs before the drop: the archive reads back,
/// the restoring identity can restore it (pg_tviews needs the admin), its
/// extensions can be created, the target carries `marker`, the restorer may
/// set the marker again, and no other session is connected. Then the target
/// is dropped, re-created for the app role, marked, given the archive's
/// extensions, and restored in two passes: the archive's objects as the app
/// role (so it owns them), then the extensions' own rows as the restorer.
/// A pg_tviews archive's TVIEWs are rebuilt and probed last.
///
/// # Errors
/// [`RestoreError::Refused`] before the drop, with nothing changed;
/// [`RestoreError::Failed`] after it.
pub async fn refresh_in_place(
    server: &dyn RefreshServer,
    identity: &RestoreIdentity,
    archive: &Path,
    marker: &str,
) -> Result<Refreshed, RestoreError> {
    let target = identity.app().database().to_owned();
    let restorer = identity.restorer(&target);
    let maintenance = identity.restorer("postgres");
    let refused = |reason: String| RestoreError::Refused(format!("{reason}. Nothing was changed"));

    let listing = restore::list(server, archive).await?;
    let creates_tviews = restore::precheck_listing(server, identity, &restorer, &listing).await?;
    let extensions = archive_extensions(&listing);
    if !extensions.is_empty() {
        match server.extensions_problem(&maintenance, &extensions).await {
            Ok(None) => {}
            Ok(Some(problem)) => return Err(refused(problem)),
            Err(error) => return Err(refused(format!("could not check the extensions: {error}"))),
        }
    }
    let carries_extension_data = !split_listing(&listing, &extensions, &[])
        .extension_data
        .is_empty();
    if carries_extension_data && !identity.uses_admin() {
        return Err(refused(
            "the archive carries rows of tables an extension owns, which only a superuser \
             may restore; set [database].admin_url_env to a superuser DSN"
                .to_owned(),
        ));
    }
    // The role the restored objects are owned by, when the admin restores.
    let owner = if identity.uses_admin() {
        Some(
            server
                .current_user(identity.app())
                .await
                .map_err(|error| refused(format!("could not read the app role: {error}")))?,
        )
    } else {
        None
    };
    match server.marker_privilege(&restorer).await {
        Ok(None) => {}
        Ok(Some(problem)) => return Err(refused(problem)),
        Err(error) => {
            return Err(refused(format!(
                "could not check the marker privilege: {error}"
            )))
        }
    }
    // The marker and the sessions are read last, right before the drop.
    marked_and_idle(server, &restorer, &maintenance, marker)
        .await
        .map_err(refused)?;

    let failed = RestoreError::Failed;
    server
        .drop(&maintenance, &target)
        .await
        .map_err(|error| failed(format!("DROP DATABASE {target} failed: {error}")))?;
    server
        .create(&maintenance, &target, owner.as_deref())
        .await
        .map_err(|error| failed(format!("CREATE DATABASE {target} failed: {error}")))?;
    server
        .set_marker(&restorer, marker)
        .await
        .map_err(|error| failed(format!("could not mark {target} again: {error}")))?;
    if !extensions.is_empty() {
        server
            .create_extensions(&restorer, &extensions)
            .await
            .map_err(|error| failed(format!("CREATE EXTENSION failed: {error}")))?;
    }
    let schemas = server
        .schemas(&restorer)
        .await
        .map_err(|error| failed(format!("could not list the schemas: {error}")))?;
    let lists = split_listing(&listing, &extensions, &schemas);
    let warnings = restore_lists(server, &restorer, archive, &lists, owner.as_deref())
        .await
        .map_err(failed)?;
    let tviews = if creates_tviews {
        let rebuilt = restore::rebuild(server, &restorer)
            .await
            .map_err(|error| failed(format!("the post-restore TVIEW rebuild failed: {error}")))?;
        if !rebuilt.still_empty.is_empty() {
            return Err(failed(format!(
                "after the post-restore TVIEW rebuild, these TVIEWs are still empty over a \
                 populated view: {}",
                rebuilt.still_empty.join(", ")
            )));
        }
        Some(rebuilt)
    } else {
        None
    };
    Ok(Refreshed {
        warnings,
        extensions,
        tviews,
    })
}

/// Why the database `restorer` names may not be dropped yet: it does not
/// carry `marker`, or another session is connected to it.
async fn marked_and_idle(
    server: &dyn RefreshServer,
    restorer: &PgConn,
    maintenance: &PgConn,
    marker: &str,
) -> Result<(), String> {
    let target = restorer.database();
    match server.marker(restorer).await {
        Ok(Some(found)) if found == marker => {}
        Ok(Some(found)) => {
            return Err(format!(
                "{target} is the refresh target of '{found}', not '{marker}'"
            ))
        }
        Ok(None) => {
            return Err(format!(
                "{target} carries no refresh marker; mark it once with `fraisier db \
                 mark-refresh-target` ({marker})"
            ))
        }
        Err(error) => return Err(format!("could not read the refresh marker: {error}")),
    }
    let sessions = server
        .connections(maintenance, target)
        .await
        .map_err(|error| format!("could not list the sessions on {target}: {error}"))?;
    if !sessions.is_empty() {
        return Err(format!(
            "other sessions are connected to {target}: {}. Stop them first (list their \
             units in [refresh].units)",
            sessions.join("; ")
        ));
    }
    Ok(())
}

/// The two restore passes into `conn`'s new database: the archive's objects as
/// `owner` (when the admin restores for the app role), then the extensions'
/// rows as the restorer. `owner` reaches the schemas an extension made, where
/// the archive puts its objects: `USAGE` stays, `CREATE` is revoked again
/// whatever the first pass did.
async fn restore_lists(
    server: &dyn RefreshServer,
    conn: &PgConn,
    archive: &Path,
    lists: &RestoreLists,
    owner: Option<&str>,
) -> Result<String, String> {
    let reached: Vec<&String> = owner.map_or_else(Vec::new, |_| {
        lists
            .made_schemas
            .iter()
            .filter(|schema| {
                lists.app.lines().any(|line| {
                    line.split_whitespace()
                        .skip(3)
                        .any(|word| word == schema.as_str())
                })
            })
            .collect()
    });
    if let Some(role) = owner {
        for schema in &reached {
            for privilege in [SchemaPrivilege::Usage, SchemaPrivilege::Create] {
                server
                    .schema_privilege(conn, privilege, schema, role, false)
                    .await
                    .map_err(|error| {
                        format!(
                            "GRANT {} ON SCHEMA {schema} failed: {error}",
                            privilege.keyword()
                        )
                    })?;
            }
        }
    }
    let objects = server
        .restore_list(conn, archive, &lists.app, owner, false)
        .await
        .map_err(|error| format!("pg_restore of the archive's objects failed: {error}"));
    let mut revoked = Ok(());
    if let Some(role) = owner {
        for schema in &reached {
            if let Err(error) = server
                .schema_privilege(conn, SchemaPrivilege::Create, schema, role, true)
                .await
            {
                revoked = Err(format!(
                    "REVOKE CREATE ON SCHEMA {schema} FROM {role} failed, so the role keeps \
                     it: {error}"
                ));
            }
        }
    }
    let mut warnings = objects?;
    revoked?;
    if !lists.extension_data.is_empty() {
        let rows = server
            .restore_list(conn, archive, &lists.extension_data, None, true)
            .await
            .map_err(|error| format!("pg_restore of the extensions' rows failed: {error}"))?;
        warnings.push_str(&rows);
    }
    Ok(warnings)
}

/// The SQL asking whether the connecting role could create `extensions` in a
/// new database: one JSON object, `superuser`, `missing` (not on the server)
/// and `untrusted`.
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

/// Why [`extensions_sql`]'s reply rules the refresh out, or `None`.
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
            "the archive creates extension(s) only a superuser can create, and the restoring \
             role is not one: {}. Set [database].admin_url_env to a superuser DSN",
            untrusted.join(", ")
        )));
    }
    Ok(None)
}

/// The `pg_restore` of the entries listed in `list` from `archive` into
/// `conn`'s database.
///
/// No owners or grants come from the archive; it restores as `role` when
/// given, rows only when `data_only`. The database name is the only
/// connection value on argv.
#[must_use]
pub fn restore_list_command(
    conn: &PgConn,
    archive: &Path,
    list: &Path,
    role: Option<&str>,
    data_only: bool,
) -> std::process::Command {
    let mut command = std::process::Command::new("pg_restore");
    command.envs(conn.pg_env());
    command
        .arg("-w")
        .arg("-d")
        .arg(crate::conninfo_dbname(conn.database()))
        .arg("--no-owner")
        .arg("--no-acl")
        .arg("--exit-on-error");
    if let Some(role) = role {
        command.arg(format!("--role={role}"));
    }
    if data_only {
        command.arg("--data-only");
    }
    command.arg("-L").arg(list).arg(archive);
    command
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

/// The SQL counting the tables outside the system schemas.
pub const TABLE_COUNT_SQL: &str = "SELECT count(*) FROM pg_catalog.pg_tables \
     WHERE schemaname NOT IN ('pg_catalog', 'information_schema')";

/// The one number `sql` prints.
async fn scalar(conn: &PgConn, sql: &str) -> Result<u64, String> {
    let out = tviews::query(Path::new("psql"), conn, sql).await?;
    out.trim()
        .parse()
        .map_err(|_| format!("expected a number from the server, got {:?}", out.trim()))
}

#[async_trait]
impl RefreshServer for crate::restore::Postgres {
    async fn connections(&self, maintenance: &PgConn, db: &str) -> Result<Vec<String>, String> {
        let sql = connections_sql(db).map_err(|error| error.to_string())?;
        let out = tviews::query(Path::new("psql"), maintenance, &sql).await?;
        Ok(out
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect())
    }

    async fn extensions_problem(
        &self,
        conn: &PgConn,
        extensions: &[String],
    ) -> Result<Option<String>, String> {
        let out = tviews::query(Path::new("psql"), conn, &extensions_sql(extensions)).await?;
        extensions_problem(&out)
    }

    async fn database_bytes(&self, conn: &PgConn) -> Result<u64, String> {
        scalar(conn, "SELECT pg_database_size(current_database())").await
    }

    async fn table_count(&self, conn: &PgConn) -> Result<u64, String> {
        scalar(conn, TABLE_COUNT_SQL).await
    }

    async fn create_extensions(&self, conn: &PgConn, extensions: &[String]) -> Result<(), String> {
        for name in extensions {
            let sql = format!("CREATE EXTENSION IF NOT EXISTS {}", tviews::ident(name));
            tviews::query(Path::new("psql"), conn, &sql).await?;
        }
        Ok(())
    }

    async fn schemas(&self, conn: &PgConn) -> Result<Vec<String>, String> {
        let out = tviews::query(
            Path::new("psql"),
            conn,
            "SELECT nspname FROM pg_catalog.pg_namespace ORDER BY 1",
        )
        .await?;
        Ok(out.lines().map(str::trim).map(str::to_owned).collect())
    }

    async fn schema_privilege(
        &self,
        conn: &PgConn,
        privilege: SchemaPrivilege,
        schema: &str,
        role: &str,
        revoke: bool,
    ) -> Result<(), String> {
        let (verb, direction) = if revoke {
            ("REVOKE", "FROM")
        } else {
            ("GRANT", "TO")
        };
        let sql = format!(
            "{verb} {} ON SCHEMA {} {direction} {}",
            privilege.keyword(),
            tviews::ident(schema),
            tviews::ident(role)
        );
        tviews::query(Path::new("psql"), conn, &sql).await.map(drop)
    }

    async fn restore_list(
        &self,
        conn: &PgConn,
        archive: &Path,
        list: &str,
        role: Option<&str>,
        data_only: bool,
    ) -> Result<String, String> {
        let path = list_path();
        write_private(&path, list).map_err(|error| {
            format!(
                "could not write the restore list {}: {error}",
                path.display()
            )
        })?;
        let outcome = crate::run(restore_list_command(conn, archive, &path, role, data_only)).await;
        let _ = std::fs::remove_file(&path);
        let outcome = outcome.map_err(|error| error.to_string())?;
        if outcome.succeeded() {
            Ok(outcome.stderr.trim().to_owned())
        } else {
            Err(outcome.stderr.trim().to_owned())
        }
    }
}

/// A fresh path for a restore list in the temp directory.
fn list_path() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    std::env::temp_dir().join(format!(
        "fraisier-refresh-{}-{nanos:x}.list",
        std::process::id()
    ))
}

/// Write `contents` to a new file only its owner can read.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(contents.as_bytes())
}

#[cfg(test)]
mod in_place_tests;

#[cfg(test)]
mod tests {
    use super::{
        archive_extensions, connections_sql, extensions_problem, extensions_sql,
        marker_privilege_problem, marker_privilege_sql, marker_sql, marker_value, parse_marker,
        restore_list_command, set_marker_sql, split_listing, without_extensions, MARKER_SETTING,
    };

    const LISTING: &str = ";\n; Archive created at 2026-10-30\n;\n\
                           2; 3079 16390 EXTENSION - pg_tviews \n\
                           3; 0 0 COMMENT - EXTENSION pg_tviews \n\
                           4; 3079 16391 EXTENSION - pgcrypto \n\
                           5; 0 0 COMMENT - EXTENSION pgcrypto \n\
                           220; 1259 16400 TABLE public tb_item app\n\
                           221; 0 16400 TABLE DATA public tb_item app\n";

    #[test]
    fn the_listing_names_the_extensions_the_archive_creates() {
        assert_eq!(archive_extensions(LISTING), vec!["pg_tviews", "pgcrypto"]);
        assert_eq!(
            archive_extensions("220; 1259 16400 TABLE public t app\n"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn the_restore_list_skips_only_the_extensions_already_created() {
        let list = without_extensions(LISTING, &["pg_tviews".to_owned()]);
        assert!(!list.contains("EXTENSION - pg_tviews"), "{list}");
        assert!(!list.contains("COMMENT - EXTENSION pg_tviews"), "{list}");
        assert!(list.contains("EXTENSION - pgcrypto"), "{list}");
        assert!(list.contains("COMMENT - EXTENSION pgcrypto"), "{list}");
        assert!(list.contains("TABLE public tb_item"), "{list}");
        assert!(list.contains("TABLE DATA public tb_item"), "{list}");
        let tables = list.find("TABLE public").expect("table");
        let data = list.find("TABLE DATA").expect("data");
        assert!(tables < data, "order kept: {list}");
    }

    #[test]
    fn a_table_named_like_an_extension_is_kept() {
        let listing = "220; 1259 16400 TABLE public pg_tviews app\n";
        assert_eq!(
            without_extensions(listing, &["pg_tviews".to_owned()]).trim(),
            listing.trim()
        );
    }

    /// The TOC of a pg_tviews archive, as measured on PG 18 + pg_tviews beta.32.
    const TVIEWS_LISTING: &str = ";\n; Archive created at 2026-10-30\n;\n\
        7; 2615 16543 SCHEMA - tviews postgres\n\
        4157; 0 0 ACL - SCHEMA tviews postgres\n\
        2; 3079 16544 EXTENSION - pg_tviews \n\
        4158; 0 0 COMMENT - EXTENSION pg_tviews \n\
        230; 1259 16693 TABLE public tb_item app_prod\n\
        232; 1259 16708 TABLE public tv_item app_prod\n\
        4159; 0 0 ACL public TABLE tv_item app_prod\n\
        231; 1259 16704 VIEW tviews public__tv_item app_prod\n\
        4149; 0 16693 TABLE DATA public tb_item app_prod\n\
        4150; 0 16708 TABLE DATA public tv_item app_prod\n\
        3951; 0 16602 TABLE DATA tviews pg_tview_helpers postgres\n\
        3950; 0 16566 TABLE DATA tviews pg_tview_meta postgres\n\
        3952; 0 16619 TABLE DATA tviews pg_tview_valid postgres\n\
        3994; 2620 16723 TRIGGER public tb_item trg_tview_row_item_on_public_tb_item app_prod\n";

    #[test]
    fn a_tviews_archive_splits_into_app_objects_and_extension_data() {
        let lists = split_listing(
            TVIEWS_LISTING,
            &["pg_tviews".to_owned()],
            &["public".to_owned(), "tviews".to_owned()],
        );
        for gone in [
            "SCHEMA - tviews",
            "ACL - SCHEMA tviews",
            "EXTENSION - pg_tviews",
            "COMMENT - EXTENSION pg_tviews",
            "pg_tview_meta",
        ] {
            assert!(
                !lists.app.contains(gone),
                "{gone} in the app list:\n{}",
                lists.app
            );
        }
        for kept in [
            "TABLE public tb_item",
            "VIEW tviews public__tv_item",
            "TABLE DATA public tv_item",
            "TRIGGER public tb_item",
            "ACL public TABLE tv_item",
        ] {
            assert!(lists.app.contains(kept), "{kept} missing:\n{}", lists.app);
        }
        assert_eq!(
            lists.extension_data.lines().collect::<Vec<_>>(),
            vec![
                "3951; 0 16602 TABLE DATA tviews pg_tview_helpers postgres",
                "3950; 0 16566 TABLE DATA tviews pg_tview_meta postgres",
                "3952; 0 16619 TABLE DATA tviews pg_tview_valid postgres",
            ]
        );
    }

    #[test]
    fn a_plain_archive_restores_whole_as_the_app() {
        let listing = "220; 1259 16400 TABLE public t app\n221; 0 16400 TABLE DATA public t app\n\
                       5; 2615 16390 SCHEMA - sales app\n";
        let lists = split_listing(listing, &[], &["public".to_owned()]);
        assert_eq!(lists.app, listing);
        assert_eq!(lists.extension_data, "");
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
    }

    #[test]
    fn a_list_restore_drops_owners_and_grants_and_stops_at_the_first_error() {
        let conn =
            crate::PgConn::parse("postgres://app:pw@db.internal/checkout_staging").expect("dsn");
        let argv = |role, data_only| -> Vec<String> {
            restore_list_command(
                &conn,
                std::path::Path::new("/b/a.pgdump"),
                std::path::Path::new("/tmp/l.list"),
                role,
                data_only,
            )
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
        };
        let objects = argv(Some("app"), false);
        for flag in ["--no-owner", "--no-acl", "--exit-on-error", "--role=app"] {
            assert!(objects.contains(&flag.to_owned()), "{flag}: {objects:?}");
        }
        assert!(!objects.contains(&"--data-only".to_owned()), "{objects:?}");
        assert!(!objects.contains(&"--clean".to_owned()), "{objects:?}");
        assert_eq!(
            &objects[objects.len() - 3..],
            ["-L", "/tmp/l.list", "/b/a.pgdump"]
        );
        let rows = argv(None, true);
        assert!(rows.contains(&"--data-only".to_owned()), "{rows:?}");
        assert!(!rows.iter().any(|a| a.starts_with("--role")), "{rows:?}");
        assert!(
            !objects.iter().any(|a| a.contains("pw")),
            "no password on argv"
        );
    }

    #[test]
    fn the_connection_query_excludes_its_own_session() {
        let sql = connections_sql("checkout_staging").expect("valid name");
        assert!(sql.contains("pg_stat_activity"), "{sql}");
        assert!(sql.contains("pg_backend_pid()"), "{sql}");
        assert!(sql.contains("'checkout_staging'"), "{sql}");
        assert!(connections_sql("x; drop").is_err());
    }

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
