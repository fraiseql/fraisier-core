//! Pluggable state persistence for the saga engine.
//!
//! [`StateStore`] is the seam that lets the engine run against a filesystem
//! today and Postgres tomorrow without touching the saga logic. The trait is
//! **deliberately designed against the hardest backend** — Postgres with many
//! concurrent writers and advisory locks (PRD risk row) — even though only the
//! filesystem (this cycle) and SQLite (the `sqlite` feature) backends ship in
//! v1.0.0-beta.1.
//!
//! ## Concurrency contract
//!
//! - [`StateStore::acquire_lock`] is a *non-blocking try-lock*: it either takes
//!   the per-`(fraise, environment)` lock immediately or returns
//!   [`StateStoreError::Locked`]. It never waits. This maps cleanly onto a
//!   filesystem `flock(LOCK_EX | LOCK_NB)`, a SQLite row insert under a unique
//!   constraint, and a Postgres `pg_try_advisory_lock` (see
//!   [`FraiseKey::advisory_key`]).
//! - The lock provides cross-deploy serialization (PRD §9.4): at most one
//!   in-flight saga per pair.
//! - [`StateStore::release_lock`] must be called to release a lock. Filesystem
//!   locks also release if the [`LockGuard`] is dropped (the OS closes the fd);
//!   database-backed leases do not, so dropping a guard without releasing leaks
//!   the lock until a future TTL reaper recovers it. Callers should always
//!   release explicitly.
//! - `record_state` is append-only; `current_state` returns the most recently
//!   recorded state for the pair (ordering is by append order, not wall clock).

use std::collections::{HashMap, HashSet};
use std::hash::{Hash as _, Hasher as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::events::{SagaEvent, SagaState};

/// Identifies one deployable in one environment — the unit of locking and state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FraiseKey {
    fraise: String,
    environment: String,
}

impl FraiseKey {
    /// Construct a key from a fraise (deployable) name and an environment.
    #[must_use]
    pub fn new(fraise: impl Into<String>, environment: impl Into<String>) -> Self {
        Self {
            fraise: fraise.into(),
            environment: environment.into(),
        }
    }

    /// The fraise (deployable) name.
    #[must_use]
    pub fn fraise(&self) -> &str {
        &self.fraise
    }

    /// The target environment.
    #[must_use]
    pub fn environment(&self) -> &str {
        &self.environment
    }

    /// A stable 64-bit identity for the pair, suitable as a Postgres advisory-lock
    /// key (`pg_try_advisory_lock($1)`).
    ///
    /// It is a deterministic hash — never a per-process random one — precisely so
    /// that every writer across every host derives the *same* lock key for the
    /// same pair, which is what makes the Postgres backend correct.
    #[must_use]
    pub fn advisory_key(&self) -> i64 {
        i64::from_ne_bytes(self.stable_hash().to_ne_bytes())
    }

    /// A stable, filesystem-safe `<sanitized>-<hash>` slug for file names. The
    /// hash suffix keeps distinct pairs from colliding after sanitization.
    fn slug(&self) -> String {
        let sanitized: String = format!("{}__{}", self.fraise, self.environment)
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        format!("{sanitized}-{:016x}", self.stable_hash())
    }

    fn stable_hash(&self) -> u64 {
        // `DefaultHasher::new` uses fixed keys, so this is stable across processes.
        let mut hasher = std::hash::DefaultHasher::new();
        self.fraise.hash(&mut hasher);
        0u8.hash(&mut hasher); // domain separator: ("a","bc") must differ from ("ab","c")
        self.environment.hash(&mut hasher);
        hasher.finish()
    }
}

impl std::fmt::Display for FraiseKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.fraise, self.environment)
    }
}

/// A persisted snapshot of where a deploy is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentState {
    /// The saga lifecycle state at the time of recording.
    pub state: SagaState,
    /// The deploy's revision identifier, once one has been established.
    pub revision: Option<String>,
    /// When this snapshot was recorded (informational; ordering uses append order).
    pub recorded_at: chrono::DateTime<chrono::Utc>,
}

impl DeploymentState {
    /// Snapshot `state` / `revision`, stamping `recorded_at` with the current time.
    #[must_use]
    pub fn new(state: SagaState, revision: Option<String>) -> Self {
        Self {
            state,
            revision,
            recorded_at: chrono::Utc::now(),
        }
    }
}

/// Errors returned by a [`StateStore`].
#[derive(Debug, thiserror::Error)]
pub enum StateStoreError {
    /// Another writer already holds the lock for this pair.
    #[error("the deploy {key} is already locked by another writer")]
    Locked {
        /// The contended `(fraise, environment)` pair, rendered for humans.
        key: String,
    },
    /// An underlying I/O operation failed.
    #[error("state store I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A record could not be (de)serialized.
    #[error("state store (de)serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    /// A backend-specific failure that does not fit the other variants.
    #[error("state store backend error: {0}")]
    Backend(String),
    /// The SQLite backend reported a database error.
    #[cfg(feature = "sqlite")]
    #[error("state store database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// Proof that the per-`(fraise, environment)` lock is held.
///
/// Return it to [`StateStore::release_lock`] to release the lock. For the
/// filesystem backend the OS also releases on drop; for database-backed leases
/// it does not (see the module-level concurrency contract).
pub struct LockGuard {
    key: FraiseKey,
    /// Held only by the filesystem backend; the live `flock` is released when
    /// this is dropped or explicitly unlocked. `None` for lease-based backends.
    #[cfg(unix)]
    flock: Option<nix::fcntl::Flock<std::fs::File>>,
}

impl LockGuard {
    /// The `(fraise, environment)` pair this guard protects.
    #[must_use]
    pub const fn key(&self) -> &FraiseKey {
        &self.key
    }

    #[cfg(unix)]
    const fn from_flock(key: FraiseKey, flock: nix::fcntl::Flock<std::fs::File>) -> Self {
        Self {
            key,
            flock: Some(flock),
        }
    }

    /// A guard for a backend whose lock is a logical lease — an in-memory set
    /// entry or a database row — rather than an OS `flock`. `release_lock` drops
    /// that lease (removes the entry / deletes the row).
    const fn lease(key: FraiseKey) -> Self {
        Self {
            key,
            #[cfg(unix)]
            flock: None,
        }
    }
}

impl std::fmt::Debug for LockGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockGuard")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// Pluggable persistence and locking for saga runs.
///
/// Implementations must be safe to share across tasks (`Send + Sync`). See the
/// module docs for the concurrency contract every backend upholds.
#[async_trait]
pub trait StateStore: Send + Sync {
    /// Try to take the lock for `key` without blocking.
    ///
    /// # Errors
    /// Returns [`StateStoreError::Locked`] if another writer holds the lock, or
    /// a backend error if the attempt itself fails.
    async fn acquire_lock(&self, key: &FraiseKey) -> Result<LockGuard, StateStoreError>;

    /// Release a previously acquired lock.
    ///
    /// # Errors
    /// Returns a backend error if the release operation fails.
    async fn release_lock(&self, guard: LockGuard) -> Result<(), StateStoreError>;

    /// Append a new state snapshot for `key`.
    ///
    /// # Errors
    /// Returns a backend or serialization error if the snapshot cannot be stored.
    async fn record_state(
        &self,
        key: &FraiseKey,
        state: &DeploymentState,
    ) -> Result<(), StateStoreError>;

    /// Return the most recently recorded state for `key`, or `None` if there is none.
    ///
    /// # Errors
    /// Returns a backend or deserialization error if stored state cannot be read.
    async fn current_state(
        &self,
        key: &FraiseKey,
    ) -> Result<Option<DeploymentState>, StateStoreError>;

    /// Append an event to `key`'s event log.
    ///
    /// # Errors
    /// Returns a backend or serialization error if the event cannot be stored.
    async fn record_event(&self, key: &FraiseKey, event: &SagaEvent)
        -> Result<(), StateStoreError>;

    /// Return `key`'s events in insertion order.
    ///
    /// # Errors
    /// Returns a backend or deserialization error if the log cannot be read.
    async fn events(&self, key: &FraiseKey) -> Result<Vec<SagaEvent>, StateStoreError>;

    /// Store an opaque, last-writer-wins snapshot for `key`, replacing any prior
    /// one.
    ///
    /// Unlike [`record_state`](StateStore::record_state) (an append-only
    /// lifecycle log), this is a single mutable cell whose contents the **engine
    /// never interprets** — it is a durable slot the *caller* defines. The deploy
    /// layer uses it to persist its release ledger (which artifact and revision
    /// are live), so a later process can find the rollback target; future
    /// multi-host coordination can reuse it for per-host progress.
    ///
    /// # Errors
    /// Returns a backend or serialization error if the snapshot cannot be stored.
    async fn record_snapshot(
        &self,
        key: &FraiseKey,
        snapshot: &serde_json::Value,
    ) -> Result<(), StateStoreError>;

    /// Return the latest snapshot stored for `key` via
    /// [`record_snapshot`](StateStore::record_snapshot), or `None` if none was.
    ///
    /// # Errors
    /// Returns a backend or deserialization error if the snapshot cannot be read.
    async fn current_snapshot(
        &self,
        key: &FraiseKey,
    ) -> Result<Option<serde_json::Value>, StateStoreError>;
}

/// A [`StateStore`] backed by a directory of JSON-lines files — one set of files
/// per `(fraise, environment)` pair — with `flock`-based locking.
///
/// This is the default backend for single-host deploys. Locking requires a Unix
/// `flock(2)`; the backend is therefore Unix-only.
#[derive(Debug, Clone)]
pub struct FilesystemStateStore {
    root: PathBuf,
}

impl FilesystemStateStore {
    /// Open (creating if necessary) a filesystem state store rooted at `root`.
    ///
    /// # Errors
    /// Returns [`StateStoreError::Io`] if the root directory cannot be created.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, StateStoreError> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn lock_path(&self, key: &FraiseKey) -> PathBuf {
        self.root.join(format!("{}.lock", key.slug()))
    }

    fn state_path(&self, key: &FraiseKey) -> PathBuf {
        self.root.join(format!("{}.state.jsonl", key.slug()))
    }

    fn events_path(&self, key: &FraiseKey) -> PathBuf {
        self.root.join(format!("{}.events.jsonl", key.slug()))
    }

    fn snapshot_path(&self, key: &FraiseKey) -> PathBuf {
        self.root.join(format!("{}.snapshot.json", key.slug()))
    }
}

/// Write `value` to `path` atomically: serialize to a sibling temp file, then
/// rename over the target so a reader never observes a half-written snapshot.
fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), StateStoreError> {
    let bytes = serde_json::to_vec(value)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Read and deserialize every non-empty line of a JSON-lines file. A missing
/// file is treated as empty.
fn read_jsonl<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>, StateStoreError> {
    match std::fs::read_to_string(path) {
        Ok(content) => content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).map_err(StateStoreError::from))
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

/// Append one JSON-serialized record as a line to a JSON-lines file.
fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> Result<(), StateStoreError> {
    use std::io::Write as _;
    let line = serde_json::to_string(value)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

#[async_trait]
impl StateStore for FilesystemStateStore {
    async fn acquire_lock(&self, key: &FraiseKey) -> Result<LockGuard, StateStoreError> {
        let path = self.lock_path(key);
        #[cfg(unix)]
        {
            use nix::errno::Errno;
            use nix::fcntl::{Flock, FlockArg};
            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false) // the lock file is a pure mutex; never clobber it
                .open(path)?;
            match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(flock) => Ok(LockGuard::from_flock(key.clone(), flock)),
                Err((_file, Errno::EWOULDBLOCK)) => Err(StateStoreError::Locked {
                    key: key.to_string(),
                }),
                Err((_file, errno)) => {
                    Err(StateStoreError::Backend(format!("flock failed: {errno}")))
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(StateStoreError::Backend(
                "the filesystem state store requires a Unix flock".to_owned(),
            ))
        }
    }

    async fn release_lock(&self, guard: LockGuard) -> Result<(), StateStoreError> {
        #[cfg(unix)]
        if let Some(flock) = guard.flock {
            flock.unlock().map_err(|(_file, errno)| {
                StateStoreError::Backend(format!("flock unlock failed: {errno}"))
            })?;
        }
        #[cfg(not(unix))]
        drop(guard);
        Ok(())
    }

    async fn record_state(
        &self,
        key: &FraiseKey,
        state: &DeploymentState,
    ) -> Result<(), StateStoreError> {
        append_jsonl(&self.state_path(key), state)
    }

    async fn current_state(
        &self,
        key: &FraiseKey,
    ) -> Result<Option<DeploymentState>, StateStoreError> {
        let mut states: Vec<DeploymentState> = read_jsonl(&self.state_path(key))?;
        Ok(states.pop())
    }

    async fn record_event(
        &self,
        key: &FraiseKey,
        event: &SagaEvent,
    ) -> Result<(), StateStoreError> {
        append_jsonl(&self.events_path(key), event)
    }

    async fn events(&self, key: &FraiseKey) -> Result<Vec<SagaEvent>, StateStoreError> {
        read_jsonl(&self.events_path(key))
    }

    async fn record_snapshot(
        &self,
        key: &FraiseKey,
        snapshot: &serde_json::Value,
    ) -> Result<(), StateStoreError> {
        write_json_atomic(&self.snapshot_path(key), snapshot)
    }

    async fn current_snapshot(
        &self,
        key: &FraiseKey,
    ) -> Result<Option<serde_json::Value>, StateStoreError> {
        match std::fs::read(self.snapshot_path(key)) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
}

/// A [`StateStore`] kept entirely in process memory — no filesystem, no
/// database, and no extra dependencies (just `std` collections).
///
/// Always available (no Cargo feature). It is the zero-setup backend for
/// **embedder unit tests** and single-process callers that need no durability:
/// state lives in `std` maps behind a `Mutex`, and the per-pair lock is an entry
/// in an in-memory set — the same non-blocking try-lock contract as the other
/// backends, so a contended pair returns [`StateStoreError::Locked`].
///
/// Cloning shares one backing store (it is `Arc`-backed), so a clone handed to a
/// [`Saga`](crate::saga::Saga) and the original observe the same state. Unlike
/// [`FilesystemStateStore`], state does **not** survive the process or a fresh
/// [`MemoryStateStore::new`] — two independently-constructed stores share
/// nothing. A request/response server that must read deploy state across
/// separate calls wants the filesystem (or `sqlite`) backend instead.
#[derive(Debug, Clone, Default)]
pub struct MemoryStateStore {
    inner: Arc<MemoryInner>,
}

/// The shared, mutex-guarded backing maps for a [`MemoryStateStore`]. Keyed by
/// [`FraiseKey`]'s slug so distinct pairs never collide, mirroring the on-disk
/// backend's per-pair file naming.
#[derive(Debug, Default)]
struct MemoryInner {
    /// Slugs of the pairs whose lock is currently held.
    locks: Mutex<HashSet<String>>,
    /// Append-only lifecycle snapshots per pair (latest is last).
    states: Mutex<HashMap<String, Vec<DeploymentState>>>,
    /// Append-only event log per pair, in insertion order.
    events: Mutex<HashMap<String, Vec<SagaEvent>>>,
    /// Single last-writer-wins opaque snapshot per pair.
    snapshots: Mutex<HashMap<String, serde_json::Value>>,
}

impl MemoryStateStore {
    /// Create an empty in-memory state store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl StateStore for MemoryStateStore {
    async fn acquire_lock(&self, key: &FraiseKey) -> Result<LockGuard, StateStoreError> {
        // Bind the boolean so the guard is a statement-scoped temporary, dropped
        // before the branches — the lock is never held across the return.
        let acquired = self
            .inner
            .locks
            .lock()
            .expect("memory state store locks poisoned")
            .insert(key.slug());
        if acquired {
            Ok(LockGuard::lease(key.clone()))
        } else {
            Err(StateStoreError::Locked {
                key: key.to_string(),
            })
        }
    }

    async fn release_lock(&self, guard: LockGuard) -> Result<(), StateStoreError> {
        self.inner
            .locks
            .lock()
            .expect("memory state store locks poisoned")
            .remove(&guard.key().slug());
        Ok(())
    }

    async fn record_state(
        &self,
        key: &FraiseKey,
        state: &DeploymentState,
    ) -> Result<(), StateStoreError> {
        self.inner
            .states
            .lock()
            .expect("memory state store states poisoned")
            .entry(key.slug())
            .or_default()
            .push(state.clone());
        Ok(())
    }

    async fn current_state(
        &self,
        key: &FraiseKey,
    ) -> Result<Option<DeploymentState>, StateStoreError> {
        let latest = self
            .inner
            .states
            .lock()
            .expect("memory state store states poisoned")
            .get(&key.slug())
            .and_then(|states| states.last().cloned());
        Ok(latest)
    }

    async fn record_event(
        &self,
        key: &FraiseKey,
        event: &SagaEvent,
    ) -> Result<(), StateStoreError> {
        self.inner
            .events
            .lock()
            .expect("memory state store events poisoned")
            .entry(key.slug())
            .or_default()
            .push(event.clone());
        Ok(())
    }

    async fn events(&self, key: &FraiseKey) -> Result<Vec<SagaEvent>, StateStoreError> {
        let events = self
            .inner
            .events
            .lock()
            .expect("memory state store events poisoned")
            .get(&key.slug())
            .cloned()
            .unwrap_or_default();
        Ok(events)
    }

    async fn record_snapshot(
        &self,
        key: &FraiseKey,
        snapshot: &serde_json::Value,
    ) -> Result<(), StateStoreError> {
        self.inner
            .snapshots
            .lock()
            .expect("memory state store snapshots poisoned")
            .insert(key.slug(), snapshot.clone());
        Ok(())
    }

    async fn current_snapshot(
        &self,
        key: &FraiseKey,
    ) -> Result<Option<serde_json::Value>, StateStoreError> {
        let snapshot = self
            .inner
            .snapshots
            .lock()
            .expect("memory state store snapshots poisoned")
            .get(&key.slug())
            .cloned();
        Ok(snapshot)
    }
}

/// The embedded SQLite schema, applied idempotently on connect. One table per
/// concept: `locks` (atomic per-pair mutual exclusion via a `PRIMARY KEY`),
/// `deployment_state` (append-only snapshots), and `events` (append-only log).
#[cfg(feature = "sqlite")]
const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS locks (
    key         TEXT PRIMARY KEY,
    fraise      TEXT NOT NULL,
    environment TEXT NOT NULL,
    acquired_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS deployment_state (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    key         TEXT NOT NULL,
    payload     TEXT NOT NULL,
    recorded_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_deployment_state_key ON deployment_state(key);
CREATE TABLE IF NOT EXISTS events (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    key     TEXT NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_key ON events(key);
CREATE TABLE IF NOT EXISTS snapshots (
    key     TEXT PRIMARY KEY,
    payload TEXT NOT NULL
);
";

/// A [`StateStore`] backed by SQLite via `sqlx` (runtime queries, no compile-time
/// query macros, per PRD §9.2).
///
/// Recommended for multi-host deploys: the lock is an atomic row insert under a
/// `PRIMARY KEY`, so two writers contending for the same pair cannot both win.
/// The same try-lock contract as the filesystem backend holds — acquisition
/// never blocks; a contended lock returns [`StateStoreError::Locked`].
#[cfg(feature = "sqlite")]
#[derive(Debug, Clone)]
pub struct SqliteStateStore {
    pool: sqlx::SqlitePool,
}

#[cfg(feature = "sqlite")]
impl SqliteStateStore {
    /// Open (creating if necessary) a SQLite store at `path` and apply the schema.
    ///
    /// # Errors
    /// Returns [`StateStoreError::Database`] if the database cannot be opened or
    /// the schema cannot be applied.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, StateStoreError> {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new().connect_with(options).await?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await?;
        Ok(Self { pool })
    }
}

#[cfg(feature = "sqlite")]
#[async_trait]
impl StateStore for SqliteStateStore {
    async fn acquire_lock(&self, key: &FraiseKey) -> Result<LockGuard, StateStoreError> {
        let result = sqlx::query(
            "INSERT INTO locks (key, fraise, environment, acquired_at) VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(key.slug())
        .bind(key.fraise())
        .bind(key.environment())
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await;

        match result {
            Ok(_) => Ok(LockGuard::lease(key.clone())),
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => {
                Err(StateStoreError::Locked {
                    key: key.to_string(),
                })
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn release_lock(&self, guard: LockGuard) -> Result<(), StateStoreError> {
        sqlx::query("DELETE FROM locks WHERE key = ?1")
            .bind(guard.key().slug())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn record_state(
        &self,
        key: &FraiseKey,
        state: &DeploymentState,
    ) -> Result<(), StateStoreError> {
        let payload = serde_json::to_string(state)?;
        sqlx::query("INSERT INTO deployment_state (key, payload, recorded_at) VALUES (?1, ?2, ?3)")
            .bind(key.slug())
            .bind(payload)
            .bind(state.recorded_at.to_rfc3339())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn current_state(
        &self,
        key: &FraiseKey,
    ) -> Result<Option<DeploymentState>, StateStoreError> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT payload FROM deployment_state WHERE key = ?1 ORDER BY id DESC LIMIT 1",
        )
        .bind(key.slug())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(payload,)| serde_json::from_str(&payload).map_err(StateStoreError::from))
            .transpose()
    }

    async fn record_event(
        &self,
        key: &FraiseKey,
        event: &SagaEvent,
    ) -> Result<(), StateStoreError> {
        let payload = serde_json::to_string(event)?;
        sqlx::query("INSERT INTO events (key, payload) VALUES (?1, ?2)")
            .bind(key.slug())
            .bind(payload)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn events(&self, key: &FraiseKey) -> Result<Vec<SagaEvent>, StateStoreError> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT payload FROM events WHERE key = ?1 ORDER BY id ASC")
                .bind(key.slug())
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter()
            .map(|(payload,)| serde_json::from_str(&payload).map_err(StateStoreError::from))
            .collect()
    }

    async fn record_snapshot(
        &self,
        key: &FraiseKey,
        snapshot: &serde_json::Value,
    ) -> Result<(), StateStoreError> {
        let payload = serde_json::to_string(snapshot)?;
        sqlx::query(
            "INSERT INTO snapshots (key, payload) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET payload = excluded.payload",
        )
        .bind(key.slug())
        .bind(payload)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn current_snapshot(
        &self,
        key: &FraiseKey,
    ) -> Result<Option<serde_json::Value>, StateStoreError> {
        let row: Option<(String,)> = sqlx::query_as("SELECT payload FROM snapshots WHERE key = ?1")
            .bind(key.slug())
            .fetch_optional(&self.pool)
            .await?;
        row.map(|(payload,)| serde_json::from_str(&payload).map_err(StateStoreError::from))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::{DeploymentState, FilesystemStateStore, FraiseKey, StateStore, StateStoreError};
    use crate::events::{SagaEvent, SagaState};

    // --- backend-contract harness: the same assertions run against every backend ---

    pub(super) async fn persists_and_returns_latest_state(store: &dyn StateStore) {
        let key = FraiseKey::new("checkout", "production");
        assert!(
            store
                .current_state(&key)
                .await
                .expect("query empty")
                .is_none(),
            "a fresh key has no state"
        );

        store
            .record_state(
                &key,
                &DeploymentState::new(
                    SagaState::Running("migrate".to_owned()),
                    Some("rev-1".to_owned()),
                ),
            )
            .await
            .expect("record first");
        store
            .record_state(
                &key,
                &DeploymentState::new(SagaState::Committed, Some("rev-2".to_owned())),
            )
            .await
            .expect("record second");

        let latest = store
            .current_state(&key)
            .await
            .expect("query")
            .expect("some state");
        assert_eq!(latest.state, SagaState::Committed, "latest wins");
        assert_eq!(latest.revision.as_deref(), Some("rev-2"));

        // Distinct keys do not bleed into one another.
        let other = FraiseKey::new("checkout", "staging");
        assert!(store
            .current_state(&other)
            .await
            .expect("query other")
            .is_none());
    }

    pub(super) async fn lock_excludes_concurrent_acquisition(store: &dyn StateStore) {
        let key = FraiseKey::new("checkout", "production");
        let held = store
            .acquire_lock(&key)
            .await
            .expect("first acquire succeeds");

        let contended = store.acquire_lock(&key).await;
        assert!(
            matches!(contended, Err(StateStoreError::Locked { .. })),
            "a second acquisition of a held lock is rejected, got {contended:?}"
        );

        // A different pair locks independently.
        let other = FraiseKey::new("checkout", "staging");
        let other_held = store.acquire_lock(&other).await.expect("independent pair");
        store.release_lock(other_held).await.expect("release other");

        store.release_lock(held).await.expect("release");
        let reacquired = store
            .acquire_lock(&key)
            .await
            .expect("re-acquire after release");
        store.release_lock(reacquired).await.expect("final release");
    }

    pub(super) async fn records_and_reads_events_in_order(store: &dyn StateStore) {
        let key = FraiseKey::new("checkout", "production");
        let first = SagaEvent::StateTransition {
            from: SagaState::Idle,
            to: SagaState::Running("preflight".to_owned()),
        };
        let second = SagaEvent::StateTransition {
            from: SagaState::Running("preflight".to_owned()),
            to: SagaState::Committed,
        };
        store
            .record_event(&key, &first)
            .await
            .expect("record first event");
        store
            .record_event(&key, &second)
            .await
            .expect("record second event");

        let events = store.events(&key).await.expect("read events");
        assert_eq!(
            events,
            vec![first, second],
            "events return in insertion order"
        );
    }

    pub(super) async fn snapshot_is_last_writer_wins(store: &dyn StateStore) {
        let key = FraiseKey::new("checkout", "production");
        assert!(
            store
                .current_snapshot(&key)
                .await
                .expect("query empty")
                .is_none(),
            "a fresh key has no snapshot"
        );

        store
            .record_snapshot(&key, &serde_json::json!({ "active": "rev-1" }))
            .await
            .expect("record first snapshot");
        store
            .record_snapshot(&key, &serde_json::json!({ "active": "rev-2" }))
            .await
            .expect("overwrite snapshot");

        let latest = store
            .current_snapshot(&key)
            .await
            .expect("query")
            .expect("some snapshot");
        assert_eq!(
            latest,
            serde_json::json!({ "active": "rev-2" }),
            "the latest write wins; the slot is not append-only"
        );

        // Distinct keys keep distinct snapshots.
        let other = FraiseKey::new("checkout", "staging");
        assert!(store
            .current_snapshot(&other)
            .await
            .expect("query other")
            .is_none());
    }

    // --- filesystem backend instantiation (Cycle 1.3) ---

    fn filesystem_store() -> (tempfile::TempDir, FilesystemStateStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FilesystemStateStore::new(dir.path()).expect("open filesystem store");
        (dir, store)
    }

    #[tokio::test]
    async fn filesystem_persists_and_returns_latest_state() {
        let (_dir, store) = filesystem_store();
        persists_and_returns_latest_state(&store).await;
    }

    #[tokio::test]
    async fn filesystem_lock_excludes_concurrent_acquisition() {
        let (_dir, store) = filesystem_store();
        lock_excludes_concurrent_acquisition(&store).await;
    }

    #[tokio::test]
    async fn filesystem_records_and_reads_events_in_order() {
        let (_dir, store) = filesystem_store();
        records_and_reads_events_in_order(&store).await;
    }

    #[tokio::test]
    async fn filesystem_snapshot_is_last_writer_wins() {
        let (_dir, store) = filesystem_store();
        snapshot_is_last_writer_wins(&store).await;
    }

    // --- in-memory backend (default-on): same harness, different backend ---

    #[tokio::test]
    async fn memory_persists_and_returns_latest_state() {
        persists_and_returns_latest_state(&super::MemoryStateStore::new()).await;
    }

    #[tokio::test]
    async fn memory_lock_excludes_concurrent_acquisition() {
        lock_excludes_concurrent_acquisition(&super::MemoryStateStore::new()).await;
    }

    #[tokio::test]
    async fn memory_records_and_reads_events_in_order() {
        records_and_reads_events_in_order(&super::MemoryStateStore::new()).await;
    }

    #[tokio::test]
    async fn memory_snapshot_is_last_writer_wins() {
        snapshot_is_last_writer_wins(&super::MemoryStateStore::new()).await;
    }

    /// Clones of a [`MemoryStateStore`] share one backing store — the property
    /// embedders rely on to hand the same store to a saga and still read its
    /// state afterwards (the in-memory analogue of two `FilesystemStateStore`
    /// handles over the same root).
    #[tokio::test]
    async fn memory_clones_share_one_backing_store() {
        let store = super::MemoryStateStore::new();
        let clone = store.clone();
        let key = FraiseKey::new("checkout", "production");

        store
            .record_state(&key, &DeploymentState::new(SagaState::Committed, None))
            .await
            .expect("record via the original handle");

        let latest = clone
            .current_state(&key)
            .await
            .expect("query via the clone")
            .expect("the clone observes the original's write");
        assert_eq!(latest.state, SagaState::Committed);
    }

    // --- sqlite backend instantiation (Cycle 1.4): same harness, different backend ---

    #[cfg(feature = "sqlite")]
    async fn sqlite_store() -> (tempfile::TempDir, super::SqliteStateStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = super::SqliteStateStore::connect(dir.path().join("state.db"))
            .await
            .expect("open sqlite store");
        (dir, store)
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn sqlite_persists_and_returns_latest_state() {
        let (_dir, store) = sqlite_store().await;
        persists_and_returns_latest_state(&store).await;
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn sqlite_lock_excludes_concurrent_acquisition() {
        let (_dir, store) = sqlite_store().await;
        lock_excludes_concurrent_acquisition(&store).await;
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn sqlite_records_and_reads_events_in_order() {
        let (_dir, store) = sqlite_store().await;
        records_and_reads_events_in_order(&store).await;
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn sqlite_snapshot_is_last_writer_wins() {
        let (_dir, store) = sqlite_store().await;
        snapshot_is_last_writer_wins(&store).await;
    }
}
