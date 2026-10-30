//! The one restore primitive: every restore-class operation goes through here.
//!
//! A restore-class operation is one that runs `pg_restore`, or creates or drops
//! a database to restore into: `db restore` (in place), the restore rehearsal
//! and the restore drill (into a throwaway). Each needs the same things, in the
//! same order, and they are kept here so they cannot drift apart:
//!
//! 1. the **identity** ([`RestoreIdentity`]): the app role, or the
//!    `[database].admin_url_env` superuser, which a webhook-started operation
//!    never resolves;
//! 2. the **archive listing**, read before anything is created or dropped, so an
//!    archive that does not read back changes nothing;
//! 3. the **pg_tviews precheck**: an archive that creates pg_tviews restores only
//!    as a superuser, so any other restorer is refused before the first
//!    destructive step;
//! 4. the restore itself, then the **post-restore TVIEW rebuild** and the
//!    empty-TVIEW probe, when the archive creates pg_tviews;
//! 5. for a throwaway, the **teardown guard** ([`teardown_guard`]): only a name
//!    carrying [`THROWAWAY_MARKER`] that is not the live database is dropped.
//!
//! The free-space check before a copy is [`room_for_copy`]. The orchestration is
//! generic over a [`RestoreServer`], so its order and refusals are unit-tested
//! with a fake; [`Postgres`] is the real one, over `psql`/`pg_dump`/`pg_restore`
//! with every secret in the child's environment, never on argv.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use crate::tviews::{self, Identity, Rebuilt};
use crate::PgConn;

/// The marker every throwaway database name carries.
pub const THROWAWAY_MARKER: &str = "_fraisier_rh_";

/// The room a restored copy needs beyond the live database's size.
pub const COPY_HEADROOM_BYTES: u64 = 2_000_000_000;

/// What started a restore-class operation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Trigger {
    /// An operator: the CLI, or a timer the operator installed.
    #[default]
    Operator,
    /// A signed webhook POST. It never resolves the admin identity: a
    /// network-reachable trigger must have no path to a database superuser.
    Webhook,
}

/// The `[database].admin_url_env` identity: the env var's name, and the
/// connection its DSN parses to.
#[derive(Debug, Clone)]
pub struct Admin {
    /// The env var holding the DSN, for messages.
    pub env: String,
    /// The admin connection.
    pub conn: PgConn,
}

/// Who a restore-class operation runs as.
#[derive(Debug, Clone)]
pub struct RestoreIdentity {
    app: PgConn,
    admin: Option<Admin>,
    /// The admin identity is configured but was not resolved, because the
    /// operation came from the webhook.
    withheld: Option<String>,
}

impl RestoreIdentity {
    /// The identity for `trigger`: `admin` when one is given and the operation
    /// is not webhook-started, the app role otherwise.
    #[must_use]
    pub fn new(app: PgConn, admin: Option<Admin>, trigger: Trigger) -> Self {
        match (admin, trigger) {
            (Some(admin), Trigger::Webhook) => Self {
                app,
                admin: None,
                withheld: Some(admin.env),
            },
            (admin, _) => Self {
                app,
                admin,
                withheld: None,
            },
        }
    }

    /// The identity for `trigger`, reading the admin DSN from `admin_env` when
    /// one is configured. A webhook-started operation never reads it.
    ///
    /// # Errors
    /// The configured env var is unset, or its DSN does not parse. A configured
    /// identity is never silently replaced by the app role.
    pub fn from_env(
        app: PgConn,
        admin_env: Option<&str>,
        trigger: Trigger,
    ) -> Result<Self, String> {
        let Some(env) = admin_env else {
            return Ok(Self::new(app, None, trigger));
        };
        if trigger == Trigger::Webhook {
            return Ok(Self {
                app,
                admin: None,
                withheld: Some(env.to_owned()),
            });
        }
        let dsn = std::env::var(env)
            .map_err(|_| format!("the configured admin_url_env '{env}' is not set"))?;
        let conn =
            PgConn::parse(&dsn).map_err(|error| format!("invalid admin DSN in {env}: {error}"))?;
        Ok(Self::new(
            app,
            Some(Admin {
                env: env.to_owned(),
                conn,
            }),
            trigger,
        ))
    }

    /// The app role's connection, as the operation was handed it.
    #[must_use]
    pub const fn app(&self) -> &PgConn {
        &self.app
    }

    /// Whether the admin identity restores.
    #[must_use]
    pub const fn uses_admin(&self) -> bool {
        self.admin.is_some()
    }

    /// The connection that restores into, creates and drops `database`.
    #[must_use]
    pub fn restorer(&self, database: &str) -> PgConn {
        self.admin
            .as_ref()
            .map_or(&self.app, |admin| &admin.conn)
            .with_database(database)
    }

    /// The env var the pg_tviews refusal points at.
    fn admin_env(&self) -> Option<&str> {
        self.admin
            .as_ref()
            .map(|admin| admin.env.as_str())
            .or(self.withheld.as_deref())
    }
}

/// The database operations a restore needs. [`Postgres`] runs them for real.
#[async_trait]
pub trait RestoreServer: Send + Sync {
    /// `pg_restore --list` of `archive`.
    ///
    /// # Errors
    /// The archive does not read back.
    async fn list(&self, archive: &Path) -> Result<String, String>;

    /// Who `conn` connects as.
    ///
    /// # Errors
    /// The identity query failed.
    async fn identity(&self, conn: &PgConn) -> Result<Identity, String>;

    /// `current_user` on `conn`.
    ///
    /// # Errors
    /// The query failed.
    async fn current_user(&self, conn: &PgConn) -> Result<String, String>;

    /// `pg_dump -Fc` of `conn`'s database into `out`.
    ///
    /// # Errors
    /// The dump failed.
    async fn dump(&self, conn: &PgConn, out: &Path) -> Result<(), String>;

    /// `CREATE DATABASE name`, owned by `owner` when given, over `maintenance`.
    ///
    /// # Errors
    /// The database was not created.
    async fn create(
        &self,
        maintenance: &PgConn,
        name: &str,
        owner: Option<&str>,
    ) -> Result<(), String>;

    /// `pg_restore` of `archive` into `conn`'s database, dropping existing
    /// objects first when `clean`. Returns what `pg_restore` warned about.
    ///
    /// # Errors
    /// `pg_restore` failed, with its stderr.
    async fn restore(&self, conn: &PgConn, archive: &Path, clean: bool) -> Result<String, String>;

    /// `DROP DATABASE name WITH (FORCE)` over `maintenance`.
    ///
    /// # Errors
    /// The database was not dropped.
    async fn drop(&self, maintenance: &PgConn, name: &str) -> Result<(), String>;

    /// `pg_tviews_rebuild_all(only_empty)` on `conn`, after refusing on a
    /// standby or without the privilege.
    ///
    /// # Errors
    /// The rebuild was refused or failed.
    async fn rebuild(&self, conn: &PgConn, only_empty: bool) -> Result<Vec<Rebuilt>, String>;

    /// The TVIEWs still empty over a populated view, as `schema.name`.
    ///
    /// # Errors
    /// The probe failed.
    async fn still_empty(&self, conn: &PgConn) -> Result<Vec<String>, String>;
}

/// What the post-restore TVIEW rebuild did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TviewRebuild {
    /// What the rebuild filled.
    pub rebuilt: Vec<Rebuilt>,
    /// What is still empty over a populated view afterwards.
    pub still_empty: Vec<String>,
}

/// What an in-place restore did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restored {
    /// What `pg_restore` warned about while still succeeding.
    pub warnings: String,
    /// The post-restore rebuild, when the archive creates pg_tviews: what it
    /// did, or why it failed after the restore succeeded.
    pub tviews: Option<Result<TviewRebuild, String>>,
}

/// A throwaway database a restore filled.
#[derive(Debug, Clone)]
pub struct Throwaway {
    /// Its name, which carries [`THROWAWAY_MARKER`].
    pub name: String,
    /// The app role's connection to it: what migrations run as.
    pub app: PgConn,
    /// The post-restore rebuild, when one ran.
    pub tviews: Option<TviewRebuild>,
}

/// Where a throwaway's data comes from.
#[derive(Debug, Clone, Copy)]
pub enum Seed<'a> {
    /// An existing archive.
    Archive(&'a Path),
    /// A fresh `pg_dump` of the live database, removed afterwards.
    FreshDump,
}

/// Why a restore failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RestoreError {
    /// Refused before anything was created or dropped.
    #[error("{0}")]
    Refused(String),
    /// A step after the prechecks failed. A throwaway was dropped again.
    #[error("{0}")]
    Failed(String),
}

/// Restore `archive` over the database `identity` names, dropping its existing
/// objects first (`db restore`). Rebuilds the TVIEWs a pg_tviews archive leaves
/// empty.
///
/// # Errors
/// [`RestoreError::Refused`] when the archive does not read back or the
/// restorer could not finish it; [`RestoreError::Failed`] when `pg_restore`
/// failed. A failed rebuild after a good restore is in [`Restored::tviews`].
pub async fn restore_in_place(
    server: &dyn RestoreServer,
    identity: &RestoreIdentity,
    archive: &Path,
) -> Result<Restored, RestoreError> {
    let target = identity.app().database().to_owned();
    let restorer = identity.restorer(&target);
    let creates_tviews = precheck(server, identity, &restorer, archive).await?;
    let warnings = server
        .restore(&restorer, archive, true)
        .await
        .map_err(RestoreError::Failed)?;
    let tviews = if creates_tviews {
        // An UNLOGGED TVIEW comes back empty from a restore; refill those only.
        Some(rebuild(server, &restorer).await)
    } else {
        None
    };
    Ok(Restored { warnings, tviews })
}

/// Read `archive`'s listing and refuse a restore of it `restorer` could not
/// finish. Returns whether it creates pg_tviews.
async fn precheck(
    server: &dyn RestoreServer,
    identity: &RestoreIdentity,
    restorer: &PgConn,
    archive: &Path,
) -> Result<bool, RestoreError> {
    let listing = list(server, archive).await?;
    precheck_listing(server, identity, restorer, &listing).await
}

/// `archive`'s `pg_restore --list`, or the refusal saying it does not read back.
pub(crate) async fn list(
    server: &dyn RestoreServer,
    archive: &Path,
) -> Result<String, RestoreError> {
    server.list(archive).await.map_err(|error| {
        RestoreError::Refused(format!(
            "pg_restore --list cannot read the archive: {error}"
        ))
    })
}

/// [`precheck`] from a listing already read.
pub(crate) async fn precheck_listing(
    server: &dyn RestoreServer,
    identity: &RestoreIdentity,
    restorer: &PgConn,
    listing: &str,
) -> Result<bool, RestoreError> {
    if !tviews::archive_creates_pg_tviews(listing) {
        return Ok(false);
    }
    let who = server
        .identity(restorer)
        .await
        .map_err(RestoreError::Refused)?;
    if !who.superuser {
        if let Some(env) = identity.withheld.as_deref() {
            return Err(RestoreError::Refused(format!(
                "this archive creates the pg_tviews extension, which only a superuser can \
                 restore, and a run started by the webhook never uses the admin identity \
                 ({env}, [database].admin_url_env). Nothing was changed. Run this deploy \
                 from the CLI or its timer instead"
            )));
        }
    }
    tviews::restore_precheck(listing, &who, None, identity.admin_env())
        .map_err(RestoreError::Refused)?;
    Ok(true)
}

/// Refill the empty TVIEWs on `conn`, then probe for any still empty.
pub(crate) async fn rebuild(
    server: &dyn RestoreServer,
    conn: &PgConn,
) -> Result<TviewRebuild, String> {
    let rebuilt = server.rebuild(conn, true).await?;
    let still_empty = server
        .still_empty(conn)
        .await
        .map_err(|error| format!("could not check the TVIEWs after the rebuild: {error}"))?;
    Ok(TviewRebuild {
        rebuilt,
        still_empty,
    })
}

/// Restore `seed` into a new throwaway database on the live server.
///
/// When `rebuild` and the archive creates pg_tviews, its TVIEWs are rebuilt and
/// probed. A throwaway that fails after it was created is dropped again.
///
/// # Errors
/// [`RestoreError::Refused`] before anything was created;
/// [`RestoreError::Failed`] after.
pub async fn restore_throwaway(
    server: &dyn RestoreServer,
    identity: &RestoreIdentity,
    seed: Seed<'_>,
    rebuild: bool,
) -> Result<Throwaway, RestoreError> {
    let live = identity.app().clone();
    match seed {
        Seed::Archive(archive) => fill_throwaway(server, identity, archive, rebuild).await,
        Seed::FreshDump => {
            let tmp = temp_archive_path();
            if let Err(error) = server.dump(&live, &tmp).await {
                let _ = std::fs::remove_file(&tmp);
                return Err(RestoreError::Refused(format!(
                    "pg_dump of the live DB failed: {error}"
                )));
            }
            let filled = fill_throwaway(server, identity, &tmp, rebuild).await;
            let _ = std::fs::remove_file(&tmp);
            filled
        }
    }
}

/// [`restore_throwaway`] from an archive on disk.
async fn fill_throwaway(
    server: &dyn RestoreServer,
    identity: &RestoreIdentity,
    archive: &Path,
    rebuild_tviews: bool,
) -> Result<Throwaway, RestoreError> {
    let live = identity.app();
    let creates_tviews = precheck(
        server,
        identity,
        &identity.restorer(live.database()),
        archive,
    )
    .await?;
    let name = throwaway_name(live.database());
    let owner = if identity.uses_admin() {
        // The app role owns what the admin creates for it.
        Some(server.current_user(live).await.map_err(|error| {
            RestoreError::Refused(format!("could not read the app role: {error}"))
        })?)
    } else {
        None
    };
    server
        .create(&identity.restorer("postgres"), &name, owner.as_deref())
        .await
        .map_err(|error| RestoreError::Refused(format!("CREATE DATABASE failed: {error}")))?;

    let restorer = identity.restorer(&name);
    let filled = async {
        server
            .restore(&restorer, archive, false)
            .await
            .map_err(|error| format!("pg_restore into the throwaway DB failed: {error}"))?;
        if !(rebuild_tviews && creates_tviews) {
            return Ok(None);
        }
        let rebuilt = rebuild(server, &restorer)
            .await
            .map_err(|error| format!("the post-restore TVIEW rebuild failed: {error}"))?;
        if rebuilt.still_empty.is_empty() {
            Ok(Some(rebuilt))
        } else {
            Err(format!(
                "after the post-restore TVIEW rebuild, these TVIEWs are still empty over a \
                 populated view: {}",
                rebuilt.still_empty.join(", ")
            ))
        }
    }
    .await;
    match filled {
        Ok(tviews) => Ok(Throwaway {
            app: live.with_database(name.clone()),
            name,
            tviews,
        }),
        Err(error) => {
            // Best-effort cleanup of the half-provisioned database.
            let _ = drop_throwaway(server, identity, &name, live.database()).await;
            Err(RestoreError::Failed(error))
        }
    }
}

/// Drop the throwaway `name`, copied from the live database `live`, after
/// [`teardown_guard`] admits it.
///
/// # Errors
/// The guard refused it, or the drop failed.
pub async fn drop_throwaway(
    server: &dyn RestoreServer,
    identity: &RestoreIdentity,
    name: &str,
    live: &str,
) -> Result<(), String> {
    teardown_guard(name, live).map_err(|reason| format!("refusing to drop '{name}': {reason}"))?;
    server
        .drop(&identity.restorer("postgres"), name)
        .await
        .map_err(|error| format!("DROP DATABASE {name} failed: {error}"))
}

/// Whether `throwaway` may be dropped when the live database is `live`.
///
/// # Errors
/// Why it may not: its name lacks [`THROWAWAY_MARKER`], or it is the live
/// database.
pub fn teardown_guard(throwaway: &str, live: &str) -> Result<(), String> {
    if !throwaway.contains(THROWAWAY_MARKER) {
        return Err(format!(
            "its name does not carry the throwaway marker '{THROWAWAY_MARKER}'"
        ));
    }
    if live == throwaway {
        return Err("it is the live database".to_owned());
    }
    Ok(())
}

/// Refuse a copy of a `live_bytes` database into `dir`, which has `free_bytes`
/// free, unless it fits with [`COPY_HEADROOM_BYTES`] to spare: a copy that fills
/// the disk can stop the live server beside it.
///
/// # Errors
/// A sentence saying how much room there is and how much the copy needs.
pub fn check_room(live_bytes: u64, free_bytes: u64, dir: &Path) -> Result<(), String> {
    let needed = live_bytes.saturating_add(COPY_HEADROOM_BYTES);
    if free_bytes < needed {
        return Err(format!(
            "{} has {} GB free; a restored copy needs about {} GB (the live database's \
             size plus 2 GB)",
            dir.display(),
            free_bytes / 1_000_000_000,
            needed.div_ceil(1_000_000_000)
        ));
    }
    Ok(())
}

/// [`check_room`] for a copy into the filesystem holding `dir`, measuring its
/// free space. This is the one free-space rule every copy goes through, a
/// drill's restore and a rehearsal's dump alike.
///
/// # Errors
/// Why there is not room, or why the free space could not be measured: an
/// unmeasurable volume is not a roomy one.
pub fn room_for_copy(live_bytes: u64, dir: &Path) -> Result<(), String> {
    let free = crate::retention::available_bytes(dir)
        .map_err(|error| format!("could not measure free space in {}: {error}", dir.display()))?;
    check_room(live_bytes, free, dir)
}

/// The real [`RestoreServer`], over `psql`, `pg_dump` and `pg_restore` on
/// `PATH`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Postgres;

#[async_trait]
impl RestoreServer for Postgres {
    async fn list(&self, archive: &Path) -> Result<String, String> {
        let listed = crate::run(tviews::list_command(archive))
            .await
            .map_err(|error| error.to_string())?;
        if listed.succeeded() {
            Ok(listed.stdout)
        } else {
            Err(listed.stderr.trim().to_owned())
        }
    }

    async fn identity(&self, conn: &PgConn) -> Result<Identity, String> {
        tviews::parse_identity(&tviews::query(Path::new("psql"), conn, tviews::IDENTITY_SQL).await?)
    }

    async fn current_user(&self, conn: &PgConn) -> Result<String, String> {
        tviews::query(Path::new("psql"), conn, "SELECT current_user")
            .await
            .map(|user| user.trim().to_owned())
    }

    async fn dump(&self, conn: &PgConn, out: &Path) -> Result<(), String> {
        let dumped = crate::run(crate::backup_command(conn, out))
            .await
            .map_err(|error| error.to_string())?;
        if dumped.succeeded() {
            Ok(())
        } else {
            Err(dumped.stderr.trim().to_owned())
        }
    }

    async fn create(
        &self,
        maintenance: &PgConn,
        name: &str,
        owner: Option<&str>,
    ) -> Result<(), String> {
        let create = owner
            .map_or_else(
                || crate::create_database_command(maintenance, name),
                |owner| crate::create_database_owned_command(maintenance, name, owner),
            )
            .map_err(|error| error.to_string())?;
        let created = crate::run(create)
            .await
            .map_err(|error| error.to_string())?;
        if created.succeeded() {
            Ok(())
        } else {
            Err(created.stderr.trim().to_owned())
        }
    }

    async fn restore(&self, conn: &PgConn, archive: &Path, clean: bool) -> Result<String, String> {
        let restored = crate::run(crate::restore_command(conn, archive, clean))
            .await
            .map_err(|error| error.to_string())?;
        if restored.succeeded() {
            Ok(restored.stderr.trim().to_owned())
        } else {
            Err(restored.stderr.trim().to_owned())
        }
    }

    async fn drop(&self, maintenance: &PgConn, name: &str) -> Result<(), String> {
        let drop = crate::drop_database_command(maintenance, name).map_err(|e| e.to_string())?;
        let dropped = crate::run(drop).await.map_err(|error| error.to_string())?;
        if dropped.succeeded() {
            Ok(())
        } else {
            Err(dropped.stderr.trim().to_owned())
        }
    }

    async fn rebuild(&self, conn: &PgConn, only_empty: bool) -> Result<Vec<Rebuilt>, String> {
        let psql = Path::new("psql");
        let schema = tviews::usable_schema(psql, conn)
            .await?
            .ok_or_else(|| format!("pg_tviews is not installed in {}", conn.database()))?;
        let identity = self.identity(conn).await?;
        if identity.recovery {
            return Err(
                "the database is in recovery (a standby, or still starting); TVIEWs can \
                 only be rebuilt on the primary"
                    .to_owned(),
            );
        }
        let privilege = tviews::query(psql, conn, &tviews::rebuild_privilege_sql(&schema)).await?;
        if let Some(problem) = tviews::rebuild_privilege_problem(&schema, &privilege)? {
            return Err(problem);
        }
        tviews::parse_rebuild(
            &tviews::query(psql, conn, &tviews::rebuild_sql(&schema, only_empty)).await?,
        )
    }

    async fn still_empty(&self, conn: &PgConn) -> Result<Vec<String>, String> {
        let psql = Path::new("psql");
        let Some(schema) = tviews::usable_schema(psql, conn).await? else {
            return Ok(Vec::new());
        };
        let found = tviews::find_empty(psql, conn, &schema).await?;
        Ok(found.empty.iter().map(tviews::Tview::qualified).collect())
    }
}

/// A unique, valid throwaway database name derived from `base` (≤ 63 chars,
/// `[A-Za-z0-9_]`, leading letter). Process id + a high-resolution timestamp keep
/// concurrent runs from colliding.
#[must_use]
pub fn throwaway_name(base: &str) -> String {
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
    format!("{head}{THROWAWAY_MARKER}{pid}_{nanos:x}")
        .chars()
        .take(63)
        .collect()
}

/// A temp file path for a fresh-dump archive.
fn temp_archive_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let pid = std::process::id();
    std::env::temp_dir().join(format!("fraisier-rehearsal-{pid}-{nanos:x}.dump"))
}

#[cfg(test)]
mod tests;
