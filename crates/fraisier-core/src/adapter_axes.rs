//! The five adapter axis traits (PRD §6.1) and their shared vocabulary types.
//!
//! # Frozen
//!
//! These traits are the **frozen** v1.0 adapter contract. Every argument and
//! return type is `Serialize + Deserialize` so the in-process adapters and the
//! IPC (JSON-RPC over stdio) adapters implement the *same* trait — the IPC
//! adapter is just a transport (the convergence rule, see the crate docs).
//!
//! # Secret handling
//!
//! Adapters never receive secret *values* in [`AdapterCtx`]; they receive a
//! mapping of logical name → source env var in [`AdapterCtx::env_secrets`] and
//! resolve the value through [`AdapterCtx::secret`]. The same helper runs on the
//! in-process and IPC paths, so they behave identically (PRD review Decision 5).

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// The kind of an [`AdapterError`]. Each maps to a JSON-RPC error code on the
/// wire (see [`AdapterErrorKind::code`]).
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::AdapterErrorKind;
/// assert_eq!(AdapterErrorKind::MethodNotSupported.code(), -32601);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AdapterErrorKind {
    /// The adapter does not implement the requested method (e.g. `preflight`).
    MethodNotSupported,
    /// A secret was requested that the context does not declare.
    MissingSecret,
    /// A declared secret's source env var could not be read.
    SecretReadFailed,
    /// The adapter's configuration is invalid.
    InvalidConfig,
    /// A required precondition was not met — e.g. the target database has no
    /// migration ledger yet (confiture `PRECON_1001`, exit 2). Distinct from
    /// [`InvalidConfig`](Self::InvalidConfig): the configuration is fine, the
    /// database is simply uninitialised, so the operator's fix is to migrate,
    /// not to edit config. Added additively to this `#[non_exhaustive]` enum.
    PreconditionFailed,
    /// The target database could not be reached — host/auth/network failure
    /// (confiture exit 3). Distinct from a config error: the DSN may be perfect
    /// and the server simply down.
    DbUnreachable,
    /// A schema / DDL / build operation failed (confiture exit 4).
    SchemaError,
    /// A migration lock or connection-pool is held by another writer (confiture
    /// exit 6). **Retriable** — waiting and retrying unchanged may succeed.
    LockContention,
    /// A git / pgGit / grant-accompaniment step failed (confiture exit 7).
    GitError,
    /// A rollback was irreversible, or left inconsistent state (confiture exit
    /// 8) — the most dangerous class; manual intervention is usually required.
    IrreversibleRollback,
    /// An unexpected internal failure with no more specific class (confiture
    /// exit 1 / `INTERNAL_ERROR`). Distinct from [`Execution`](Self::Execution),
    /// which is the *generic* underlying-operation failure other adapters raise.
    InternalError,
    /// The adapter ran but the underlying operation failed.
    Execution,
    /// The IPC framing or JSON-RPC envelope was malformed.
    Protocol,
    /// A remote (subprocess) adapter returned an error of its own.
    Remote,
}

impl AdapterErrorKind {
    /// The stable wire string for this kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MethodNotSupported => "method_not_supported",
            Self::MissingSecret => "missing_secret",
            Self::SecretReadFailed => "secret_read_failed",
            Self::InvalidConfig => "invalid_config",
            Self::PreconditionFailed => "precondition_failed",
            Self::DbUnreachable => "db_unreachable",
            Self::SchemaError => "schema_error",
            Self::LockContention => "lock_contention",
            Self::GitError => "git_error",
            Self::IrreversibleRollback => "irreversible_rollback",
            Self::InternalError => "internal_error",
            Self::Execution => "execution",
            Self::Protocol => "protocol",
            Self::Remote => "remote",
        }
    }

    /// The JSON-RPC error code carried for this kind when crossing the IPC boundary.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            Self::MethodNotSupported => -32601, // JSON-RPC "method not found"
            Self::InvalidConfig => -32602,      // JSON-RPC "invalid params"
            Self::Protocol => -32600,           // JSON-RPC "invalid request"
            Self::MissingSecret => -32001,
            Self::SecretReadFailed => -32002,
            Self::Execution => -32000,
            Self::Remote => -32003,
            // fraisier server-error space (-320xx): the confiture exit-code
            // taxonomy, one code per class (see `fraisier-adapter-confiture`).
            Self::PreconditionFailed => -32004,
            Self::DbUnreachable => -32005,
            Self::SchemaError => -32006,
            Self::LockContention => -32007,
            Self::GitError => -32008,
            Self::IrreversibleRollback => -32009,
            Self::InternalError => -32010,
        }
    }
}

impl fmt::Display for AdapterErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error type returned by every adapter method.
///
/// It is a plain serializable struct (no boxed source) so it survives the
/// JSON-RPC boundary intact: `kind`/`code` map to the error code, `message` to
/// the message, and `stderr` to captured subprocess output (PRD §9.3).
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::{AdapterError, AdapterErrorKind};
/// let err = AdapterError::method_not_supported("preflight");
/// assert_eq!(err.kind, AdapterErrorKind::MethodNotSupported);
/// assert_eq!(err.code(), -32601);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("[{kind}] {message}")]
pub struct AdapterError {
    /// The error kind.
    pub kind: AdapterErrorKind,
    /// The JSON-RPC error code. Defaults to the kind's code, but a remote
    /// (IPC) adapter's own code is preserved here so it survives the boundary.
    pub code: i32,
    /// A human-readable description.
    pub message: String,
    /// The adapter that produced the error, when known.
    pub adapter: Option<String>,
    /// The operation in progress when it failed, when known.
    pub operation: Option<String>,
    /// Captured subprocess stderr, for IPC adapters.
    pub stderr: Option<String>,
}

impl AdapterError {
    /// Construct an error of `kind` with `message`.
    #[must_use]
    pub fn new(kind: AdapterErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            code: kind.code(),
            message: message.into(),
            adapter: None,
            operation: None,
            stderr: None,
        }
    }

    /// An error reported by a remote (IPC) adapter, preserving its JSON-RPC code.
    #[must_use]
    pub fn remote(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            ..Self::new(AdapterErrorKind::Remote, message)
        }
    }

    /// The method/axis is not implemented by this adapter.
    #[must_use]
    pub fn method_not_supported(method: impl Into<String>) -> Self {
        let method = method.into();
        Self {
            operation: Some(method.clone()),
            ..Self::new(
                AdapterErrorKind::MethodNotSupported,
                format!("method '{method}' is not supported by this adapter"),
            )
        }
    }

    /// A secret was requested that the context does not declare.
    #[must_use]
    pub fn missing_secret(logical: &str) -> Self {
        Self::new(
            AdapterErrorKind::MissingSecret,
            format!("secret '{logical}' is not declared in env_secrets"),
        )
    }

    /// A declared secret's source env var could not be read.
    #[must_use]
    pub fn secret_read_failed(logical: &str, source: &str, cause: &std::env::VarError) -> Self {
        Self::new(
            AdapterErrorKind::SecretReadFailed,
            format!(
                "secret '{logical}' maps to env var '{source}' which could not be read: {cause}"
            ),
        )
    }

    /// Attach the producing adapter's name (builder style).
    #[must_use]
    pub fn with_adapter(mut self, adapter: impl Into<String>) -> Self {
        self.adapter = Some(adapter.into());
        self
    }

    /// The JSON-RPC error code for this error.
    #[must_use]
    pub const fn code(&self) -> i32 {
        self.code
    }
}

// ---------------------------------------------------------------------------
// Shared context
// ---------------------------------------------------------------------------

/// Everything an adapter needs to act, minus secret *values* (see the module docs).
///
/// This is `params.ctx` on the JSON-RPC wire; its **serialized** field set is
/// frozen. [`resolved_secrets`](Self::resolved_secrets) is `#[serde(skip)]` and
/// so never appears on the wire — it is an in-process-only escape hatch.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::AdapterCtx;
/// let ctx = AdapterCtx::new("checkout", "production");
/// assert_eq!(ctx.fraise, "checkout");
/// ```
// No `PartialEq`: `settings` holds `serde_json::Value`, which is not `Eq`, and
// nothing compares whole contexts. `Debug` is hand-written so the in-process
// resolved secret *values* are never rendered.
#[derive(Clone, Serialize, Deserialize)]
pub struct AdapterCtx {
    /// The fraise (deployable) name.
    pub fraise: String,
    /// The target environment.
    pub environment: String,
    /// The host this call targets, for per-host operations in multi-host plans.
    pub host: Option<HostId>,
    /// The working directory the adapter should assume.
    pub workdir: PathBuf,
    /// The migrations directory, for migration adapters.
    pub migrations_path: Option<PathBuf>,
    /// Logical secret name → name of the env var to read on this process.
    /// Never carries secret *values*. Resolve via [`AdapterCtx::secret`].
    pub env_secrets: BTreeMap<String, String>,
    /// In-process resolved secret *values*, checked by [`AdapterCtx::secret`]
    /// **before** the [`env_secrets`](Self::env_secrets) indirection.
    ///
    /// `#[serde(skip)]`, so values never cross the IPC boundary (only in-process
    /// adapters such as confiture observe them) and the redacting [`fmt::Debug`]
    /// impl never renders them. Set via [`AdapterCtx::with_resolved_secret`]; used
    /// by the restore-rehearsal preflight to point an adapter at a throwaway DB.
    #[serde(skip)]
    pub resolved_secrets: BTreeMap<String, String>,
    /// The previously deployed revision, for rollback diagnostics.
    pub previous_revision: Option<Revision>,
    /// The artifact currently staged or active, when known.
    pub artifact_ref: Option<ArtifactRef>,
    /// Adapter-specific configuration from `fraisier.toml`.
    pub settings: BTreeMap<String, serde_json::Value>,
}

impl AdapterCtx {
    /// A context for `fraise`/`environment` with all optional fields empty and
    /// `workdir` set to the current directory (`"."`).
    #[must_use]
    pub fn new(fraise: impl Into<String>, environment: impl Into<String>) -> Self {
        Self {
            fraise: fraise.into(),
            environment: environment.into(),
            host: None,
            workdir: PathBuf::from("."),
            migrations_path: None,
            env_secrets: BTreeMap::new(),
            resolved_secrets: BTreeMap::new(),
            previous_revision: None,
            artifact_ref: None,
            settings: BTreeMap::new(),
        }
    }

    /// Return a clone of this context with `logical` resolved directly to `value`
    /// in-process (bypassing the env-var indirection).
    ///
    /// The value is stored only in [`resolved_secrets`](Self::resolved_secrets),
    /// which is never serialized — so it stays in-process and is observed only by
    /// in-process adapters. Used by the restore-rehearsal preflight to redirect a
    /// migration adapter at a throwaway database.
    #[must_use]
    pub fn with_resolved_secret(
        mut self,
        logical: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.resolved_secrets.insert(logical.into(), value.into());
        self
    }

    /// Resolve a secret by its logical name.
    ///
    /// Checks the in-process [`resolved_secrets`](Self::resolved_secrets) override
    /// first; otherwise behaves identically in-process and over IPC, looking up
    /// `logical` in [`AdapterCtx::env_secrets`] to find the source env var name and
    /// reading that variable from the process environment. The value never travels
    /// in JSON params or argv.
    ///
    /// # Errors
    /// [`AdapterErrorKind::MissingSecret`] if `logical` is not declared, or
    /// [`AdapterErrorKind::SecretReadFailed`] if the mapped env var is unset or
    /// not valid UTF-8.
    pub fn secret(&self, logical: &str) -> Result<String, AdapterError> {
        if let Some(value) = self.resolved_secrets.get(logical) {
            return Ok(value.clone());
        }
        let source = self
            .env_secrets
            .get(logical)
            .ok_or_else(|| AdapterError::missing_secret(logical))?;
        std::env::var(source)
            .map_err(|cause| AdapterError::secret_read_failed(logical, source, &cause))
    }
}

impl fmt::Debug for AdapterCtx {
    /// Renders every field except the in-process resolved secret *values*, which
    /// are shown only as their logical key names so a debug print can never leak a
    /// resolved DSN/password.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted: Vec<&String> = self.resolved_secrets.keys().collect();
        f.debug_struct("AdapterCtx")
            .field("fraise", &self.fraise)
            .field("environment", &self.environment)
            .field("host", &self.host)
            .field("workdir", &self.workdir)
            .field("migrations_path", &self.migrations_path)
            .field("env_secrets", &self.env_secrets)
            .field("resolved_secrets", &redacted)
            .field("previous_revision", &self.previous_revision)
            .field("artifact_ref", &self.artifact_ref)
            .field("settings", &self.settings)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Migration vocabulary
// ---------------------------------------------------------------------------

/// An opaque, adapter-defined migration revision identifier (e.g. `"20260531_abc"`).
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::Revision;
/// let rev = Revision::new("20260531_abc123");
/// assert_eq!(rev.as_str(), "20260531_abc123");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Revision(pub String);

impl Revision {
    /// Wrap a revision string.
    #[must_use]
    pub fn new(revision: impl Into<String>) -> Self {
        Self(revision.into())
    }

    /// Borrow the revision as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The result of an `up`/`down_to` migration call.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::MigrationOutcome;
/// let outcome = MigrationOutcome::default();
/// assert!(outcome.applied.is_empty());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationOutcome {
    /// The revision before the call, if any.
    pub from: Option<Revision>,
    /// The revision after the call, if any.
    pub to: Option<Revision>,
    /// The revisions applied (or reverted) by this call, in order.
    pub applied: Vec<Revision>,
    /// Captured adapter log output.
    pub log: String,
}

/// A single check performed by `verify`.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::VerifyCheck;
/// let check = VerifyCheck { name: "tb_user exists".into(), ok: true, detail: None };
/// assert!(check.ok);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyCheck {
    /// What was checked.
    pub name: String,
    /// Whether it passed.
    pub ok: bool,
    /// Optional detail (e.g. the failing query result).
    pub detail: Option<String>,
}

/// The result of a post-apply correctness `verify` (PRD review Decision 2).
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::VerifyReport;
/// let report = VerifyReport { ok: true, checks: Vec::new() };
/// assert!(report.ok);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyReport {
    /// Whether every check passed.
    pub ok: bool,
    /// The individual checks performed.
    pub checks: Vec<VerifyCheck>,
}

/// Severity of a [`PreflightIssue`].
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::Severity;
/// assert_ne!(Severity::Error, Severity::Warning);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Blocks the deploy.
    Error,
    /// Surfaced but does not block.
    Warning,
    /// Informational only.
    Info,
}

/// One finding from a `preflight` lint.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::{PreflightIssue, Severity};
/// let issue = PreflightIssue {
///     severity: Severity::Error,
///     code: "missing_down".into(),
///     message: "no .down.sql for 003".into(),
///     migration: Some("003".into()),
/// };
/// assert_eq!(issue.severity, Severity::Error);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightIssue {
    /// How serious the finding is.
    pub severity: Severity,
    /// A stable machine-readable code (e.g. `"non_transactional"`).
    pub code: String,
    /// A human-readable description.
    pub message: String,
    /// The migration the issue concerns, if specific to one.
    pub migration: Option<String>,
}

// ---------------------------------------------------------------------------
// The migration risk contract
// ---------------------------------------------------------------------------

/// The revision of the migration risk contract this build of fraisier
/// understands.
///
/// It is a **major**, carried inside the change-set payload as
/// [`ChangeSet::contract_version`] — deliberately *not* the IPC
/// [`AdapterDescription::protocol_version`], which a purely additive payload
/// field must not invalidate for every external adapter. Adding a field to a
/// change entry does not bump it; removing or renaming one, or changing what a
/// tier *means*, does.
///
/// A change-set stamped with a **greater** version is treated as absent, not
/// best-effort parsed — see [`PreflightReport::usable_change_set`].
///
/// The contract is specified in `docs/proposals/migration-risk-contract.md`.
pub const RISK_CONTRACT_VERSION: u32 = 1;

/// How risky one planned schema change is, as classified by the migration
/// adapter.
///
/// The variants are ordered least → most severe, and that order exists for
/// exactly two purposes: computing the worst tier in a [`ChangeSet`], and
/// sorting a plan render worst-first. **It is not how policy decisions are
/// made** — policy maps each tier to an action independently, so an operator who
/// considers a lock-risky index build more dangerous than a `DROP INDEX` on
/// their workload expresses that in configuration, not by arguing about this
/// ordering.
///
/// A change qualifying for two tiers takes the **more severe** one.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::RiskTier;
/// assert!(RiskTier::Additive < RiskTier::Irreversible);
/// // The wire name is the cross-repo pact with confiture.
/// assert_eq!(serde_json::to_string(&RiskTier::LockRisky).unwrap(), "\"lock_risky\"");
/// // A tier this build does not know is never a nearest match — it is no tier.
/// assert!(serde_json::from_str::<RiskTier>("\"quantum\"").is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RiskTier {
    /// Adds a new object. No existing reader or writer can break.
    ///
    /// `CREATE TABLE`, `ADD COLUMN … NULL`, `CREATE INDEX CONCURRENTLY`.
    Additive,
    /// Changes existing state, with a proven `down` path that restores it.
    ///
    /// `ALTER … SET DEFAULT`, widening a `varchar(n)`.
    Reversible,
    /// Semantically safe, but takes a lock that can stall a hot table.
    ///
    /// `ADD COLUMN … NOT NULL DEFAULT` on older PostgreSQL, a non-concurrent
    /// `CREATE INDEX`, a table rewrite.
    LockRisky,
    /// Destroys data or an object, but the loss is bounded and recoverable from
    /// backup.
    ///
    /// `DELETE`, `TRUNCATE`, and — the ruling that is not re-litigated per pull
    /// request — **`DROP INDEX`**: the index is rebuildable from the data it
    /// indexes, so the cost is time and load, not information.
    Destructive,
    /// Destroys data with no `down` path that can restore it.
    ///
    /// `DROP TABLE`, narrowing a type, and **`DROP COLUMN` even when a
    /// `down.sql` exists** — the down path restores the *schema*, not the
    /// *data*. Reversibility here means the state is recoverable, not that a
    /// script exists.
    Irreversible,
}

impl RiskTier {
    /// Every tier, least → most severe.
    ///
    /// For rendering the taxonomy to an operator (a config error listing what
    /// was expected, a plan legend). Adding a tier means adding it here;
    /// [`as_str`](Self::as_str)'s exhaustive match is the compiler-enforced half
    /// of that pair, and `snake_case_wire_names_are_pinned` is the test half.
    pub const ALL: [Self; 5] = [
        Self::Additive,
        Self::Reversible,
        Self::LockRisky,
        Self::Destructive,
        Self::Irreversible,
    ];

    /// The tier's name on the wire — the cross-repo pact with confiture.
    ///
    /// Kept in step with the `snake_case` serde representation by
    /// `snake_case_wire_names_are_pinned`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Additive => "additive",
            Self::Reversible => "reversible",
            Self::LockRisky => "lock_risky",
            Self::Destructive => "destructive",
            Self::Irreversible => "irreversible",
        }
    }
}

impl std::fmt::Display for RiskTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RiskTier {
    type Err = UnknownRiskTier;

    /// Parse a wire name. **Never a nearest match** — an unrecognised string is
    /// no tier at all, which the policy gate denies (contract §5).
    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|tier| tier.as_str() == name)
            .ok_or_else(|| UnknownRiskTier {
                name: name.to_owned(),
            })
    }
}

/// A tier name that is not in the taxonomy.
///
/// Its message lists the valid names, so a config typo is one line away from
/// being fixed rather than one grep through the docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownRiskTier {
    /// The name that did not parse.
    pub name: String,
}

impl std::fmt::Display for UnknownRiskTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let expected: Vec<&str> = RiskTier::ALL.iter().map(|tier| tier.as_str()).collect();
        write!(
            f,
            "unknown risk tier '{}'; expected one of: {}",
            self.name,
            expected.join(", ")
        )
    }
}

impl std::error::Error for UnknownRiskTier {}

/// Deserialize a [`SchemaChange::tier`] leniently: a tier string this build does
/// not recognise — or a `null`, or any other shape — becomes `None`
/// (*unclassified*) instead of failing the enclosing entry.
///
/// The failure mode this exists to prevent: confiture adds a sixth tier, every
/// `SchemaChange` carrying it fails to parse, the whole `change_set` envelope
/// goes with it, and a deploy that should have been **denied for one
/// unclassified change** instead reports "no change-set at all" — a state the
/// gate cannot distinguish from an adapter that never classified. One producer
/// release would become a fleet-wide outage or, worse, a wrong verdict.
///
/// The leniency is scoped to this one field. `kind` and `object` stay required,
/// and [`RiskTier`] itself stays strict (an unknown tier is never rounded to a
/// nearest match) — see `docs/proposals/migration-risk-contract.md` §6.
fn deserialize_lenient_tier<'de, D>(deserializer: D) -> Result<Option<RiskTier>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    /// Matches a known tier first; anything else is swallowed as unclassified.
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Lenient {
        Known(RiskTier),
        Unrecognised(serde::de::IgnoredAny),
    }

    Ok(match Option::<Lenient>::deserialize(deserializer)? {
        Some(Lenient::Known(tier)) => Some(tier),
        Some(Lenient::Unrecognised(_)) | None => None,
    })
}

/// One planned schema change, as classified by the migration adapter.
///
/// fraisier **consumes** this classification; it never re-derives one. In
/// particular [`tier`](Self::tier) is never inferred from [`kind`](Self::kind):
/// inference from string codes is how a producer-side rename becomes a silent
/// consumer-side misclassification, which is the whole reason the tier travels
/// as typed data.
///
/// This struct is `#[non_exhaustive]`, so adapters in other crates build it
/// through [`SchemaChange::new`] and the `with_*` methods rather than a struct
/// literal — that is what keeps a later field addition additive for them.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::{RiskTier, SchemaChange};
/// let change = SchemaChange::new("drop_column", "public.tb_user.legacy_flag")
///     .with_migration("20260804120100")
///     .with_tier(RiskTier::Irreversible)
///     .with_detail("DROP COLUMN legacy_flag");
/// assert_eq!(change.tier, Some(RiskTier::Irreversible));
///
/// // A tier this build does not know is unclassified, and the entry survives.
/// let unknown: SchemaChange = serde_json::from_str(
///     r#"{"kind": "entangle_column", "object": "public.tb_user.spin", "tier": "quantum"}"#,
/// ).unwrap();
/// assert_eq!(unknown.tier, None);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SchemaChange {
    /// A stable machine code for the operation, e.g. `"drop_column"`.
    ///
    /// Rendered verbatim to the operator; never parsed for meaning.
    pub kind: String,
    /// The object touched, fully qualified: `schema.table`,
    /// `schema.table.column`, `schema.index`.
    ///
    /// It is shown to the operator, so it must identify the object
    /// unambiguously without a further lookup.
    pub object: String,
    /// The migration this change belongs to, when the adapter attributes it —
    /// the **version prefix** (`"20260804120100"`), not the filename, matching
    /// [`PreflightIssue::migration`].
    #[serde(default)]
    pub migration: Option<String>,
    /// The adapter's classification.
    ///
    /// `None` ⇒ *unclassified* ⇒ the policy gate denies. Absent, `null`, and
    /// unrecognised all arrive here as `None`; none of them mean "safe".
    #[serde(default, deserialize_with = "deserialize_lenient_tier")]
    pub tier: Option<RiskTier>,
    /// One human-readable line for the plan render. Never parsed.
    ///
    /// Must not contain a DSN or any other credential — it is printed and
    /// logged.
    #[serde(default)]
    pub detail: Option<String>,
}

impl SchemaChange {
    /// An **unclassified** change to `object` of kind `kind`.
    ///
    /// The tier starts absent deliberately: an adapter states a classification
    /// by calling [`with_tier`](Self::with_tier), and saying nothing is never
    /// mistaken for saying "safe".
    #[must_use]
    pub fn new(kind: impl Into<String>, object: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            object: object.into(),
            migration: None,
            tier: None,
            detail: None,
        }
    }

    /// Attribute the change to a migration, by version prefix.
    #[must_use]
    pub fn with_migration(mut self, migration: impl Into<String>) -> Self {
        self.migration = Some(migration.into());
        self
    }

    /// Classify the change.
    #[must_use]
    pub const fn with_tier(mut self, tier: RiskTier) -> Self {
        self.tier = Some(tier);
        self
    }

    /// Attach the one-line description shown in the plan render.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// The migration adapter's classified plan — every change it intends to apply,
/// each with the tier it assigned.
///
/// **Its presence is the signal.** `Some(ChangeSet)` with an empty
/// [`changes`](Self::changes) means *the adapter looked and there is nothing to
/// change*; a `None` [`PreflightReport::change_set`] means *nobody classified
/// this*, which is not safe. That is why the wire form is an object and never a
/// bare array: an array conflates the two states, and the conflation resolves in
/// the dangerous direction — an unclassified migration presenting as a clean,
/// empty plan and applying itself.
///
/// This struct is `#[non_exhaustive]`; build it with [`ChangeSet::new`], which
/// stamps [`RISK_CONTRACT_VERSION`].
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::{ChangeSet, RiskTier, SchemaChange};
/// let set = ChangeSet::new(vec![
///     SchemaChange::new("add_column", "public.tb_user.nickname").with_tier(RiskTier::Additive),
///     SchemaChange::new("drop_table", "public.tb_legacy").with_tier(RiskTier::Irreversible),
///     SchemaChange::new("entangle_column", "public.tb_user.spin"), // unclassified
/// ]);
/// assert_eq!(set.worst_tier(), Some(RiskTier::Irreversible));
/// assert_eq!(set.unclassified().count(), 1);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ChangeSet {
    /// The revision of the risk contract this payload was written to.
    ///
    /// Required: an unversioned payload is an unreadable one, not an empty one.
    /// A version **greater** than [`RISK_CONTRACT_VERSION`] makes the whole set
    /// unusable — see [`PreflightReport::usable_change_set`].
    pub contract_version: u32,
    /// The planned changes, in the order the adapter listed them (migration
    /// order, which is deliberately *not* severity order — the render sorts, the
    /// contract does not).
    ///
    /// Absent on the wire is equivalent to empty: the producer classified and
    /// found nothing.
    #[serde(default)]
    pub changes: Vec<SchemaChange>,
}

impl Default for ChangeSet {
    /// An empty change-set stamped with **this build's** contract version.
    ///
    /// Not `#[derive(Default)]`: a derived `contract_version` of `0` is outside
    /// the contract's domain (majors start at 1) and would sail through the
    /// version check as though a producer had stamped it.
    fn default() -> Self {
        Self {
            contract_version: RISK_CONTRACT_VERSION,
            changes: Vec::new(),
        }
    }
}

impl ChangeSet {
    /// A change-set of `changes`, stamped with [`RISK_CONTRACT_VERSION`].
    ///
    /// This is the constructor for an adapter *producing* a classification in
    /// Rust; a change-set arriving over the wire carries the producer's own
    /// version instead.
    #[must_use]
    pub fn new(changes: Vec<SchemaChange>) -> Self {
        Self {
            changes,
            ..Self::default()
        }
    }

    /// Stamp the change-set with the contract version a **producer** wrote.
    ///
    /// [`new`](Self::new) stamps *this build's* [`RISK_CONTRACT_VERSION`],
    /// which is correct for a change-set produced in Rust and wrong for one
    /// reconstructed from the wire: a payload written to a later contract has
    /// to keep its own version, or [`PreflightReport::usable_change_set`]
    /// cannot refuse it *by name* — and a refusal that cannot say which side to
    /// upgrade is not actionable. Restamping a payload we cannot read with a
    /// version we can is the best-effort parse the contract forbids.
    ///
    /// # Example
    /// ```
    /// # use fraisier_core::adapter_axes::{ChangeSet, PreflightReport};
    /// // An adapter parsing a report from a producer newer than this build.
    /// let from_the_wire = ChangeSet::new(Vec::new()).with_contract_version(2);
    /// let report = PreflightReport::new(true).with_change_set(from_the_wire);
    /// assert!(report.usable_change_set().is_err());
    /// ```
    #[must_use]
    pub const fn with_contract_version(mut self, contract_version: u32) -> Self {
        self.contract_version = contract_version;
        self
    }

    /// The most severe tier present, or `None` when no change carries one.
    ///
    /// Unclassified changes are **not** folded in — they are not "tier zero".
    /// A set whose worst *known* tier is `additive` reads as approvable, so the
    /// unclassified ones are surfaced separately by
    /// [`unclassified`](Self::unclassified) and denied by the policy gate.
    #[must_use]
    pub fn worst_tier(&self) -> Option<RiskTier> {
        self.changes.iter().filter_map(|change| change.tier).max()
    }

    /// The changes the adapter did not classify, in the order it listed them.
    ///
    /// Each one is a reason to refuse; naming them is what makes the refusal
    /// actionable.
    pub fn unclassified(&self) -> impl Iterator<Item = &SchemaChange> {
        self.changes.iter().filter(|change| change.tier.is_none())
    }
}

/// Deserialize [`PreflightReport::change_set`] leniently: an envelope this build
/// cannot read becomes `None` (*not classified*) instead of failing the whole
/// report.
///
/// A broken envelope invalidates the **change-set**, not the report. The report
/// also carries `ok`, `issues` and `window_safe`, all of which predate this
/// contract and all of which the deploy blocks on today; a hard parse error here
/// would turn a producer's typo in a purely additive field into a failed deploy
/// for operators who never asked for risk tiers. The change-set is still
/// unusable — which is the part that has to fail safe — and the policy gate
/// refuses on it.
///
/// A `contract_version` of `0` is rejected here too: majors start at `1`, so a
/// zero is an unstamped payload rather than a payload from an older contract,
/// and it must not slip past the version check in
/// [`PreflightReport::usable_change_set`].
fn deserialize_lenient_change_set<'de, D>(deserializer: D) -> Result<Option<ChangeSet>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    /// Matches a well-formed envelope first; anything else is not a change-set.
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Lenient {
        Envelope(ChangeSet),
        Broken(serde::de::IgnoredAny),
    }

    Ok(match Option::<Lenient>::deserialize(deserializer)? {
        Some(Lenient::Envelope(change_set)) if change_set.contract_version != 0 => Some(change_set),
        Some(Lenient::Envelope(_) | Lenient::Broken(_)) | None => None,
    })
}

/// The result of a `preflight` forward-compatibility lint (PRD review Decision 4).
///
/// This struct is `#[non_exhaustive]`, so adapters in other crates build it
/// through [`PreflightReport::new`] and the `with_*` methods; every future field
/// is then additive for them.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::PreflightReport;
/// let report = PreflightReport::new(true).with_window_safe(true);
/// assert!(report.ok);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PreflightReport {
    /// Whether the migrations are safe to deploy (no `Error`-severity issues).
    pub ok: bool,
    /// The findings.
    pub issues: Vec<PreflightIssue>,
    /// The migration adapter's **first-class window-safety verdict**, when it
    /// provides one: `Some(true)` iff every pending migration is forward-compatible
    /// for a two-version window (both N-1 and N serving against the shared DB).
    /// `Some(false)` is a hard block; `None` means the adapter offers no typed
    /// verdict and the consumer must fall back to inspecting [`Self::issues`]
    /// (e.g. confiture's `PFLIGHT_REPLICA_*` codes). Additive / Option-typed so an
    /// adapter that doesn't emit it stays compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_safe: Option<bool>,
    /// The adapter's classified change-set, when it advertises the `risk_tier`
    /// capability.
    ///
    /// `None` ⇒ *nobody classified this*, which is not safe. Read it through
    /// [`usable_change_set`](Self::usable_change_set) rather than directly, so
    /// the contract-version check cannot be skipped.
    ///
    /// `skip_serializing_if` keeps the serialized form byte-identical for an
    /// adapter that does not classify, so no downstream consumer sees a new
    /// `null` key appear.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_lenient_change_set"
    )]
    pub change_set: Option<ChangeSet>,
}

impl PreflightReport {
    /// A report carrying `ok`, with no issues, no window-safety verdict and no
    /// change-set.
    ///
    /// Every optional verdict starts absent: an adapter states what it knows,
    /// and silence is never read as a pass.
    #[must_use]
    pub fn new(ok: bool) -> Self {
        Self {
            ok,
            ..Self::default()
        }
    }

    /// Attach the lint findings.
    #[must_use]
    pub fn with_issues(mut self, issues: Vec<PreflightIssue>) -> Self {
        self.issues = issues;
        self
    }

    /// State the two-version window-safety verdict (see [`crate::window_safety`]).
    #[must_use]
    pub const fn with_window_safe(mut self, window_safe: bool) -> Self {
        self.window_safe = Some(window_safe);
        self
    }

    /// Attach the classified change-set — only for an adapter that advertises
    /// the `risk_tier` capability.
    #[must_use]
    pub fn with_change_set(mut self, change_set: ChangeSet) -> Self {
        self.change_set = Some(change_set);
        self
    }

    /// The change-set, if there is one **and** this build can read it.
    ///
    /// This is the only supported way to reach [`Self::change_set`]: it
    /// centralises the [`RISK_CONTRACT_VERSION`] check so no call site can
    /// forget it, and every way of failing resolves to *unclassified*, which the
    /// policy gate denies. Absence is never safety.
    ///
    /// # Errors
    /// [`ChangeSetUnavailable::NotEmitted`] when the adapter emitted no readable
    /// change-set, or [`ChangeSetUnavailable::VersionTooNew`] when it emitted one
    /// written to a later contract — named, not best-effort parsed.
    pub const fn usable_change_set(&self) -> Result<&ChangeSet, ChangeSetUnavailable> {
        let Some(change_set) = &self.change_set else {
            return Err(ChangeSetUnavailable::NotEmitted);
        };
        if change_set.contract_version > RISK_CONTRACT_VERSION {
            return Err(ChangeSetUnavailable::VersionTooNew {
                found: change_set.contract_version,
                understood: RISK_CONTRACT_VERSION,
            });
        }
        Ok(change_set)
    }
}

/// Why a [`PreflightReport`] yielded no usable change-set.
///
/// Both variants mean the same thing to a policy decision — *unclassified*,
/// therefore denied. They differ only in what the operator is told to do about
/// it, which is the whole reason they are distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ChangeSetUnavailable {
    /// No readable change-set was emitted.
    ///
    /// Either the adapter does not classify at all — it should not be
    /// advertising `risk_tier` — or it advertised the capability and then sent
    /// nothing usable, which is a producer bug worth naming out loud.
    #[error(
        "the migration adapter emitted no usable change-set; nothing classified the pending \
         schema changes"
    )]
    NotEmitted,
    /// The change-set was written to a later revision of the risk contract.
    ///
    /// Naming both versions is the actionable part: it says which side to
    /// upgrade. No best-effort parse is attempted — a payload written to a
    /// contract we cannot read is not one we may approve on the operator's
    /// behalf.
    #[error(
        "the migration adapter emitted a change-set at risk-contract version {found}, but this \
         build of fraisier understands version {understood}; upgrade fraisier or pin the adapter"
    )]
    VersionTooNew {
        /// The version the adapter stamped on the payload.
        found: u32,
        /// The version this build understands ([`RISK_CONTRACT_VERSION`]).
        understood: u32,
    },
}

/// An adapter's self-description, returned by `describe` — the capability and
/// protocol-version handshake.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::AdapterDescription;
/// let desc = AdapterDescription {
///     name: "confiture".into(),
///     version: "0.6.0".into(),
///     protocol_version: 1,
///     capabilities: vec!["up".into(), "down_to".into(), "preflight".into()],
/// };
/// assert!(desc.capabilities.iter().any(|c| c == "preflight"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterDescription {
    /// The adapter's name (matches the `fraisier-adapter-<name>` discovery key).
    pub name: String,
    /// The adapter's own version.
    pub version: String,
    /// The IPC protocol major version the adapter speaks.
    pub protocol_version: u32,
    /// The methods and optional behaviours the adapter actually implements —
    /// this is fraisier's feature-negotiation idiom, in preference to bumping
    /// [`protocol_version`](Self::protocol_version) for additive work.
    ///
    /// Gates optional calls (`preflight`, `traffic_swap`) and optional *payload*
    /// content:
    ///
    /// | Capability | Means |
    /// |---|---|
    /// | `preflight` | The adapter implements the forward-compatibility lint. |
    /// | `window_safe` | Its [`PreflightReport`] carries a typed window-safety verdict. |
    /// | `risk_tier` | Its [`PreflightReport`] carries a classified [`ChangeSet`]. |
    ///
    /// An adapter advertises a capability only when the **installed** producer
    /// can actually fulfil it — for a CLI-backed adapter that means gating on the
    /// detected tool version, not hard-coding the string. Advertising one that
    /// cannot be fulfilled turns every deploy into a denial: safe, but useless.
    /// Not advertising it is the honest signal *"I do not do this"*, which
    /// callers handle deliberately.
    pub capabilities: Vec<String>,
}

// ---------------------------------------------------------------------------
// Host / artifact / service / health / load-balancer vocabulary
// ---------------------------------------------------------------------------

/// A host name drawn from the deploy's inventory.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::HostId;
/// let host = HostId::new("web-1");
/// assert_eq!(host.as_str(), "web-1");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostId(pub String);

impl HostId {
    /// Wrap a host name.
    #[must_use]
    pub fn new(host: impl Into<String>) -> Self {
        Self(host.into())
    }

    /// Borrow the host name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HostId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A reference to a deployable artifact (a release archive, git ref, or local path).
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::ArtifactRef;
/// let artifact = ArtifactRef { id: "v1.2.3".into(), checksum: None };
/// assert_eq!(artifact.id, "v1.2.3");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// An adapter-defined identifier (version, sha, path, …).
    pub id: String,
    /// The expected content checksum, when the source provides one.
    pub checksum: Option<String>,
}

/// An artifact staged on a host but not yet activated.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::{ArtifactRef, StagedArtifact};
/// let staged = StagedArtifact {
///     artifact: ArtifactRef { id: "v1.2.3".into(), checksum: None },
///     path: "/var/lib/app/staging/v1.2.3".into(),
/// };
/// assert_eq!(staged.artifact.id, "v1.2.3");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedArtifact {
    /// Which artifact was staged.
    pub artifact: ArtifactRef,
    /// Where it was staged on the host.
    pub path: PathBuf,
}

/// A service's run state.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::ServiceStatus;
/// let status = ServiceStatus { running: true, detail: None };
/// assert!(status.running);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceStatus {
    /// Whether the service is currently running.
    pub running: bool,
    /// Optional detail (e.g. the systemd `ActiveState`).
    pub detail: Option<String>,
}

/// The result of a health probe.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::HealthStatus;
/// let health = HealthStatus { healthy: true, detail: None };
/// assert!(health.healthy);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthStatus {
    /// Whether the host is serving correctly.
    pub healthy: bool,
    /// Optional detail (e.g. the HTTP status or body excerpt).
    pub detail: Option<String>,
}

/// A host's membership state at the load balancer.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::LbState;
/// assert_ne!(LbState::InPool, LbState::Draining);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LbState {
    /// Receiving traffic.
    InPool,
    /// Being removed; finishing in-flight requests.
    Draining,
    /// Removed from the pool.
    Removed,
}

/// A snapshot of a host's load-balancer membership, captured before draining so
/// that reattach can restore it exactly.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::{LbMembership, LbState};
/// let m = LbMembership { state: LbState::InPool, weight: Some(100) };
/// assert_eq!(m.state, LbState::InPool);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LbMembership {
    /// The membership state.
    pub state: LbState,
    /// The host's weight in the pool, if the LB supports weighting.
    pub weight: Option<u32>,
}

// ---------------------------------------------------------------------------
// The five axis traits
// ---------------------------------------------------------------------------

/// **Migration axis** — run database migrations.
///
/// This is the trait the IPC JSON-RPC method set maps onto one-to-one (PRD §6.2):
/// each method name is a JSON-RPC method, `params` carry the (serializable)
/// arguments, and the result is the (serializable) return value.
///
/// `preflight` and `post_migrate` have defaults; everything else is required.
///
/// # The risk contract
///
/// An adapter that can classify the schema changes it is about to apply
/// advertises the **`risk_tier`** capability and attaches a [`ChangeSet`] to its
/// [`PreflightReport`]. No extra trait method: the change-set rides on the
/// existing `preflight` return, which is why a purely additive payload field
/// needs no IPC protocol bump. The wire shape, the tier taxonomy and the
/// boundary rulings are specified in
/// `docs/proposals/migration-risk-contract.md`.
///
/// # Example
/// ```no_run
/// # use fraisier_core::adapter_axes::{MigrationAdapter, AdapterCtx, AdapterError};
/// async fn migrate(adapter: &dyn MigrationAdapter, ctx: &AdapterCtx) -> Result<(), AdapterError> {
///     let desc = adapter.describe().await?;
///     if desc.capabilities.iter().any(|c| c == "preflight") {
///         adapter.preflight(ctx).await?; // only call what the adapter advertises
///     }
///     adapter.up(ctx, None).await?;
///     adapter.post_migrate(ctx).await?;
///     Ok(())
/// }
/// ```
#[async_trait]
pub trait MigrationAdapter: Send + Sync {
    /// Report the adapter's name, version, protocol version, and capabilities.
    ///
    /// # Errors
    /// [`AdapterError`] if the adapter cannot describe itself.
    async fn describe(&self) -> Result<AdapterDescription, AdapterError>;

    /// Return the currently applied revision, or `None` if none is applied.
    ///
    /// # Errors
    /// [`AdapterError`] if the revision cannot be determined.
    async fn current_revision(&self, ctx: &AdapterCtx) -> Result<Option<Revision>, AdapterError>;

    /// Apply migrations up to `target` (or all pending when `None`).
    ///
    /// # Errors
    /// [`AdapterError`] if migration fails.
    async fn up(
        &self,
        ctx: &AdapterCtx,
        target: Option<Revision>,
    ) -> Result<MigrationOutcome, AdapterError>;

    /// Roll the database back so `target` is the latest applied revision.
    ///
    /// # Errors
    /// [`AdapterError`] if rollback fails.
    async fn down_to(
        &self,
        ctx: &AdapterCtx,
        target: Revision,
    ) -> Result<MigrationOutcome, AdapterError>;

    /// Verify post-apply correctness (PRD review Decision 2).
    ///
    /// # Errors
    /// [`AdapterError`] if verification cannot run.
    async fn verify(&self, ctx: &AdapterCtx) -> Result<VerifyReport, AdapterError>;

    /// Forward-compatibility lint (PRD review Decision 4), and — for an adapter
    /// advertising `risk_tier` — the classified [`ChangeSet`].
    ///
    /// Defaults to [`AdapterErrorKind::MethodNotSupported`] — never a passing
    /// report — so that "I can't lint" can never masquerade as "lint passed".
    /// The deploy layer gates this call on [`AdapterDescription::capabilities`].
    ///
    /// The same fail-safe rule governs the change-set: a report with no usable
    /// change-set is *unclassified*, and unclassified is a refusal, not a pass.
    /// A consumer gates on the capability and then reads through
    /// [`PreflightReport::usable_change_set`], never the field directly:
    ///
    /// ```
    /// # use fraisier_core::adapter_axes::{AdapterDescription, PreflightReport, RiskTier};
    /// /// The worst tier in the plan, or why there is no answer.
    /// fn worst_risk(
    ///     desc: &AdapterDescription,
    ///     report: &PreflightReport,
    /// ) -> Result<Option<RiskTier>, String> {
    ///     // 1. The capability gates the read. Not advertising it is the honest
    ///     //    signal "I do not classify" — which denies, it does not proceed.
    ///     if !desc.capabilities.iter().any(|c| c == "risk_tier") {
    ///         return Err("the migration adapter does not classify schema changes".to_owned());
    ///     }
    ///     // 2. The accessor carries the contract-version check, so no call site
    ///     //    can forget it; its error already names what to do about it.
    ///     let change_set = report.usable_change_set().map_err(|e| e.to_string())?;
    ///     // 3. An unclassified change is a refusal even beside classified ones.
    ///     let unclassified: Vec<&str> =
    ///         change_set.unclassified().map(|c| c.object.as_str()).collect();
    ///     if !unclassified.is_empty() {
    ///         return Err(format!("unclassified schema changes: {}", unclassified.join(", ")));
    ///     }
    ///     Ok(change_set.worst_tier())
    /// }
    ///
    /// // An adapter that lints but does not classify: no capability, no answer.
    /// let desc = AdapterDescription {
    ///     name: "confiture".into(),
    ///     version: "0.38.1".into(),
    ///     protocol_version: 1,
    ///     capabilities: vec!["preflight".into(), "window_safe".into()],
    /// };
    /// assert!(worst_risk(&desc, &PreflightReport::new(true)).is_err());
    /// ```
    ///
    /// # Errors
    /// [`AdapterError`] of kind [`AdapterErrorKind::MethodNotSupported`] by
    /// default; a real implementation errors if the lint cannot run.
    async fn preflight(&self, _ctx: &AdapterCtx) -> Result<PreflightReport, AdapterError> {
        Err(AdapterError::method_not_supported("preflight"))
    }

    /// Run post-migration hooks (PRD review Decision 3).
    ///
    /// Defaults to a safe no-op (`Ok(())`): "do nothing" *is* the correct
    /// semantic for an adapter with no post-migrate concept.
    ///
    /// # Errors
    /// [`AdapterError`] if a real implementation's hooks fail.
    async fn post_migrate(&self, _ctx: &AdapterCtx) -> Result<(), AdapterError> {
        Ok(())
    }
}

/// **Artifact axis** — get code/binary onto a host and activate it.
///
/// # Example
/// ```no_run
/// # use fraisier_core::adapter_axes::{ArtifactAdapter, AdapterCtx, AdapterError, HostId};
/// async fn deploy(a: &dyn ArtifactAdapter, ctx: &AdapterCtx, host: &HostId) -> Result<(), AdapterError> {
///     let staged = a.stage(ctx, host).await?;
///     a.activate(ctx, host, &staged).await
/// }
/// ```
#[async_trait]
pub trait ArtifactAdapter: Send + Sync {
    /// Fetch and stage the artifact on `host` without activating it.
    ///
    /// # Errors
    /// [`AdapterError`] if staging fails (download, checksum mismatch, …).
    async fn stage(&self, ctx: &AdapterCtx, host: &HostId) -> Result<StagedArtifact, AdapterError>;

    /// Activate a previously staged artifact (the swap-in step).
    ///
    /// # Errors
    /// [`AdapterError`] if activation fails.
    async fn activate(
        &self,
        ctx: &AdapterCtx,
        host: &HostId,
        staged: &StagedArtifact,
    ) -> Result<(), AdapterError>;

    /// Return the artifact currently active on `host`, for rollback capture.
    ///
    /// # Errors
    /// [`AdapterError`] if the current artifact cannot be determined.
    async fn current(
        &self,
        ctx: &AdapterCtx,
        host: &HostId,
    ) -> Result<Option<ArtifactRef>, AdapterError>;
}

/// **Service axis** — start, stop, and restart the service on a host.
///
/// # Example
/// ```no_run
/// # use fraisier_core::adapter_axes::{ServiceAdapter, AdapterCtx, AdapterError, HostId};
/// async fn bounce(s: &dyn ServiceAdapter, ctx: &AdapterCtx, host: &HostId) -> Result<(), AdapterError> {
///     s.restart(ctx, host).await?;
///     let status = s.status(ctx, host).await?;
///     assert!(status.running);
///     Ok(())
/// }
/// ```
#[async_trait]
pub trait ServiceAdapter: Send + Sync {
    /// Restart the service on `host`.
    ///
    /// # Errors
    /// [`AdapterError`] if the restart fails.
    async fn restart(&self, ctx: &AdapterCtx, host: &HostId) -> Result<(), AdapterError>;

    /// Report the service's current status on `host`.
    ///
    /// # Errors
    /// [`AdapterError`] if status cannot be read.
    async fn status(&self, ctx: &AdapterCtx, host: &HostId) -> Result<ServiceStatus, AdapterError>;
}

/// **Health axis** — verify a host is serving correctly. Implementations own
/// their own retry/backoff.
///
/// # Example
/// ```no_run
/// # use fraisier_core::adapter_axes::{HealthAdapter, AdapterCtx, AdapterError, HostId};
/// async fn gate(h: &dyn HealthAdapter, ctx: &AdapterCtx, host: &HostId) -> Result<bool, AdapterError> {
///     Ok(h.check(ctx, host).await?.healthy)
/// }
/// ```
#[async_trait]
pub trait HealthAdapter: Send + Sync {
    /// Probe `host` and report whether it is healthy.
    ///
    /// # Errors
    /// [`AdapterError`] if the probe cannot be performed (distinct from an
    /// unhealthy result, which is `Ok(HealthStatus { healthy: false, .. })`).
    async fn check(&self, ctx: &AdapterCtx, host: &HostId) -> Result<HealthStatus, AdapterError>;
}

/// **Load-balancer axis** — drain a host from the pool and reattach it.
///
/// # Example
/// ```no_run
/// # use fraisier_core::adapter_axes::{LbAdapter, AdapterCtx, AdapterError, HostId};
/// async fn cycle(lb: &dyn LbAdapter, ctx: &AdapterCtx, host: &HostId) -> Result<(), AdapterError> {
///     let prior = lb.drain(ctx, host).await?; // capture membership for exact restore
///     // ... update the host ...
///     lb.reattach(ctx, host, &prior).await
/// }
/// ```
#[async_trait]
pub trait LbAdapter: Send + Sync {
    /// Drain `host` from the pool, returning its prior membership so it can be
    /// restored exactly on reattach.
    ///
    /// # Errors
    /// [`AdapterError`] if draining fails.
    async fn drain(&self, ctx: &AdapterCtx, host: &HostId) -> Result<LbMembership, AdapterError>;

    /// Reattach `host`, restoring the `prior` membership captured by `drain`.
    ///
    /// # Errors
    /// [`AdapterError`] if reattaching fails.
    async fn reattach(
        &self,
        ctx: &AdapterCtx,
        host: &HostId,
        prior: &LbMembership,
    ) -> Result<(), AdapterError>;
}

/// Which fleet the load balancer's active upstream points at during a blue-green
/// window. The name is opaque to fraisier — an operator-meaningful upstream/fleet
/// id (e.g. `"blue"` / `"green"`).
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::TrafficTarget;
/// let t = TrafficTarget::new("green");
/// assert_eq!(t.as_str(), "green");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrafficTarget(pub String);

impl TrafficTarget {
    /// A target named `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The target's name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TrafficTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The receipt of a completed traffic swap — confirms which target is now live.
///
/// The caller captures the *prior* [`TrafficTarget`] (via
/// [`TrafficDirector::current_target`]) before swapping, so a rollback is just a
/// swap back to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapToken {
    /// The target traffic now points at.
    pub target: TrafficTarget,
}

/// **Traffic-direction capability** — an *additive* extension to the `lb` axis.
///
/// The frozen [`LbAdapter`] expresses per-host pool `drain`/`reattach` (rolling
/// deploys); it **cannot** express "point the active upstream at the green fleet,"
/// which is what a blue-green swap needs. Rather than reopen the Cycle-1.6 freeze,
/// this is a separate, **capability-gated** trait: the deploy layer composes it
/// only when the adapter advertises `traffic_swap` in
/// [`AdapterDescription::capabilities`] (exactly as `preflight` gates the
/// migration axis), and refuses blue-green otherwise. The frozen `LbAdapter` is
/// untouched — this is surfaced, not silently changed.
///
/// # Example
/// ```no_run
/// # use fraisier_core::adapter_axes::{TrafficDirector, TrafficTarget, AdapterCtx, AdapterError};
/// async fn swap(d: &dyn TrafficDirector, ctx: &AdapterCtx) -> Result<(), AdapterError> {
///     let prior = d.current_target(ctx).await?;     // capture, for swap-back
///     d.switch_to(ctx, &TrafficTarget::new("green")).await?;
///     // ... on failure within the hold window: swap back ...
///     d.switch_to(ctx, &prior).await?;
///     Ok(())
/// }
/// ```
#[async_trait]
pub trait TrafficDirector: Send + Sync {
    /// Report identity + capabilities (must include `traffic_swap`).
    ///
    /// # Errors
    /// [`AdapterError`] if the adapter cannot describe itself.
    async fn describe(&self) -> Result<AdapterDescription, AdapterError>;

    /// Which fleet is currently live — captured *before* a swap so a rollback can
    /// swap back to it.
    ///
    /// # Errors
    /// [`AdapterError`] if the current target cannot be determined.
    async fn current_target(&self, ctx: &AdapterCtx) -> Result<TrafficTarget, AdapterError>;

    /// Atomically point the active upstream at `target` and make it live.
    ///
    /// # Errors
    /// [`AdapterError`] if the swap or reload fails.
    async fn switch_to(
        &self,
        ctx: &AdapterCtx,
        target: &TrafficTarget,
    ) -> Result<SwapToken, AdapterError>;
}

#[cfg(test)]
mod tests {
    use super::{
        AdapterCtx, AdapterDescription, AdapterError, AdapterErrorKind, ChangeSet,
        ChangeSetUnavailable, MigrationAdapter, MigrationOutcome, PreflightIssue, PreflightReport,
        Revision, RiskTier, SchemaChange, Severity, VerifyReport, RISK_CONTRACT_VERSION,
    };
    use std::collections::BTreeMap;

    /// Serializes env-mutating tests so `set_var`/`var` don't race other tests.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A migration adapter that implements only the required methods, leaving
    /// `preflight` and `post_migrate` on their trait defaults.
    struct MinimalMigration;

    #[async_trait::async_trait]
    impl MigrationAdapter for MinimalMigration {
        async fn describe(&self) -> Result<AdapterDescription, AdapterError> {
            Ok(AdapterDescription {
                name: "minimal".to_owned(),
                version: "0.0.0".to_owned(),
                protocol_version: 1,
                capabilities: vec!["up".to_owned()],
            })
        }
        async fn current_revision(
            &self,
            _ctx: &AdapterCtx,
        ) -> Result<Option<Revision>, AdapterError> {
            Ok(None)
        }
        async fn up(
            &self,
            _ctx: &AdapterCtx,
            _target: Option<Revision>,
        ) -> Result<MigrationOutcome, AdapterError> {
            Ok(MigrationOutcome::default())
        }
        async fn down_to(
            &self,
            _ctx: &AdapterCtx,
            _target: Revision,
        ) -> Result<MigrationOutcome, AdapterError> {
            Ok(MigrationOutcome::default())
        }
        async fn verify(&self, _ctx: &AdapterCtx) -> Result<VerifyReport, AdapterError> {
            Ok(VerifyReport {
                ok: true,
                checks: Vec::new(),
            })
        }
    }

    fn ctx() -> AdapterCtx {
        AdapterCtx {
            fraise: "checkout".to_owned(),
            environment: "production".to_owned(),
            host: None,
            workdir: ".".into(),
            migrations_path: None,
            env_secrets: BTreeMap::new(),
            resolved_secrets: BTreeMap::new(),
            previous_revision: None,
            artifact_ref: None,
            settings: BTreeMap::new(),
        }
    }

    #[test]
    fn resolved_secret_override_wins_over_env_and_is_redacted_in_debug() {
        // An in-process resolved value short-circuits the env-var indirection…
        let ctx = AdapterCtx::new("checkout", "production")
            .with_resolved_secret("DATABASE_URL", "postgres://u:pw@h/throwaway");
        assert_eq!(
            ctx.secret("DATABASE_URL").expect("resolved"),
            "postgres://u:pw@h/throwaway"
        );
        // …and the value never appears in a debug print (only its logical key does).
        let rendered = format!("{ctx:?}");
        assert!(
            !rendered.contains("throwaway") && !rendered.contains("pw"),
            "resolved secret value leaked into Debug: {rendered}"
        );
        assert!(
            rendered.contains("DATABASE_URL"),
            "the logical key should still be visible: {rendered}"
        );
    }

    #[tokio::test]
    async fn preflight_defaults_to_method_not_supported() {
        let err = MinimalMigration
            .preflight(&ctx())
            .await
            .expect_err("default preflight must error, never silently pass");
        assert_eq!(err.kind, AdapterErrorKind::MethodNotSupported);
    }

    #[tokio::test]
    async fn post_migrate_defaults_to_ok() {
        MinimalMigration
            .post_migrate(&ctx())
            .await
            .expect("default post_migrate is a safe no-op");
    }

    #[test]
    fn secret_is_missing_when_not_declared() {
        let err = ctx()
            .secret("DATABASE_URL")
            .expect_err("an undeclared secret must error");
        assert_eq!(err.kind, AdapterErrorKind::MissingSecret);
    }

    #[test]
    fn secret_resolves_through_the_env_mapping() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let source = "FRAISIER_TEST_SECRET_SRC";
        std::env::set_var(source, "postgres://example/db");
        let mut ctx = ctx();
        ctx.env_secrets
            .insert("DATABASE_URL".to_owned(), source.to_owned());
        assert_eq!(
            ctx.secret("DATABASE_URL").expect("resolve"),
            "postgres://example/db"
        );
        std::env::remove_var(source);
    }

    #[test]
    fn adapter_error_round_trips_through_serde() {
        // Convergence rule: the Err half of every adapter return must survive JSON-RPC.
        let err = AdapterError::method_not_supported("preflight");
        let json = serde_json::to_string(&err).expect("serialize");
        let back: AdapterError = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.kind, AdapterErrorKind::MethodNotSupported);
    }

    #[test]
    fn confiture_failure_kinds_have_stable_wire_strings_and_distinct_codes() {
        // The confiture exit-code taxonomy projected 1:1 onto the wire enum. Each
        // wire string is the cross-repo contract vocabulary shared with confiture
        // and the Python adapter; each code must be distinct (a collision would,
        // e.g., send a no-ledger precondition to InvalidConfig's -32602 and tell
        // the operator to fix a healthy config file).
        let taxonomy = [
            (
                AdapterErrorKind::PreconditionFailed,
                "precondition_failed",
                -32004,
            ),
            (AdapterErrorKind::DbUnreachable, "db_unreachable", -32005),
            (AdapterErrorKind::SchemaError, "schema_error", -32006),
            (AdapterErrorKind::LockContention, "lock_contention", -32007),
            (AdapterErrorKind::GitError, "git_error", -32008),
            (
                AdapterErrorKind::IrreversibleRollback,
                "irreversible_rollback",
                -32009,
            ),
            (AdapterErrorKind::InternalError, "internal_error", -32010),
        ];
        let mut seen_codes = std::collections::BTreeSet::new();
        for (kind, wire, code) in taxonomy {
            assert_eq!(kind.as_str(), wire);
            assert_eq!(kind.code(), code);
            assert!(seen_codes.insert(code), "duplicate JSON-RPC code {code}");
            // Serde uses the snake_case variant name — it must equal `as_str`.
            let json = serde_json::to_string(&kind).expect("serialize");
            assert_eq!(json, format!("\"{wire}\""));
            let back: AdapterErrorKind = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, kind);
        }
        // The taxonomy must not collide with the pre-existing generic kinds.
        for generic in [
            AdapterErrorKind::InvalidConfig,
            AdapterErrorKind::Execution,
            AdapterErrorKind::Protocol,
            AdapterErrorKind::Remote,
        ] {
            assert!(
                !seen_codes.contains(&generic.code()),
                "{} collides with a taxonomy code",
                generic.as_str()
            );
        }
    }

    #[test]
    fn adapter_ctx_round_trips_through_serde() {
        // AdapterCtx crosses the JSON-RPC boundary as params.ctx.
        let json = serde_json::to_string(&ctx()).expect("serialize");
        let back: AdapterCtx = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.fraise, "checkout");
        assert_eq!(back.environment, "production");
    }

    // -----------------------------------------------------------------------
    // The migration risk contract (docs/proposals/migration-risk-contract.md)
    // -----------------------------------------------------------------------

    #[test]
    fn unknown_tier_strings_do_not_deserialize() {
        // A tier this build does not know must never round down to a nearest
        // match: `"quantum"` is not `destructive` because the words rhyme, and
        // `"DESTRUCTIVE"` is not `destructive` because serde is case-sensitive
        // by design here. Both are *unclassified*, which the gate denies.
        for wire in ["\"quantum\"", "\"DESTRUCTIVE\"", "\"lockRisky\"", "\"\""] {
            serde_json::from_str::<RiskTier>(wire)
                .expect_err(&format!("{wire} must not parse as a tier"));
        }
    }

    #[test]
    fn a_non_string_tier_does_not_deserialize() {
        // The enum itself is strict about shape as well as spelling; the one
        // deliberate leniency lives on `SchemaChange::tier`, nowhere else.
        for wire in ["1", "null", "{}", "[]", "true"] {
            serde_json::from_str::<RiskTier>(wire)
                .expect_err(&format!("{wire} must not parse as a tier"));
        }
    }

    #[test]
    fn severity_ordering_is_additive_to_irreversible() {
        // The full chain, asserted as a chain rather than pairwise, so a
        // reordered variant cannot hide behind a passing neighbour comparison.
        let ascending = [
            RiskTier::Additive,
            RiskTier::Reversible,
            RiskTier::LockRisky,
            RiskTier::Destructive,
            RiskTier::Irreversible,
        ];
        for pair in ascending.windows(2) {
            assert!(
                pair[0] < pair[1],
                "{:?} must rank below {:?}",
                pair[0],
                pair[1]
            );
        }
        assert_eq!(
            ascending.iter().copied().max(),
            Some(RiskTier::Irreversible),
            "the derived Ord is what `worst_tier` relies on"
        );
    }

    #[test]
    fn snake_case_wire_names_are_pinned() {
        // These strings are the cross-repo pact with confiture. A serde rename
        // slip here does not fail loudly — it silently reclassifies every change
        // of that tier as unclassified on one side of the seam.
        let pact = [
            (RiskTier::Additive, "additive"),
            (RiskTier::Reversible, "reversible"),
            (RiskTier::LockRisky, "lock_risky"),
            (RiskTier::Destructive, "destructive"),
            (RiskTier::Irreversible, "irreversible"),
        ];
        for (tier, wire) in pact {
            let json = serde_json::to_string(&tier).expect("serialize");
            assert_eq!(json, format!("\"{wire}\""));
            let back: RiskTier = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, tier);
            // `as_str` / `FromStr` are the same pact spelled for config files
            // and messages; a drift between them and serde would classify one
            // side of the seam differently from the other.
            assert_eq!(tier.as_str(), wire);
            assert_eq!(wire.parse::<RiskTier>().expect("parses"), tier);
        }
        assert_eq!(pact.map(|(tier, _)| tier), RiskTier::ALL, "ALL is complete");
    }

    #[test]
    fn an_unknown_tier_name_does_not_parse_to_a_nearest_match() {
        let error = "destructve"
            .parse::<RiskTier>()
            .expect_err("a typo must not resolve to a tier");
        let message = error.to_string();
        assert!(message.contains("destructve"), "{message}");
        // The message lists the taxonomy, so the fix does not need a doc lookup.
        for tier in RiskTier::ALL {
            assert!(message.contains(tier.as_str()), "{message}");
        }
    }

    #[test]
    fn a_change_without_a_tier_is_unclassified() {
        // No `tier` key at all. The absent tier must surface as `None` — never
        // as a default tier, which would be fraisier inventing a classification
        // the adapter declined to make.
        let change: SchemaChange = serde_json::from_str(
            r#"{"kind": "alter_column_type", "object": "public.tb_order.total_cents"}"#,
        )
        .expect("an entry with no tier is still a well-formed change");
        assert_eq!(change.tier, None);
        assert_eq!(change.kind, "alter_column_type");
        assert_eq!(change.migration, None);
        assert_eq!(change.detail, None);
    }

    #[test]
    fn an_unknown_tier_is_unclassified_not_nearest_match() {
        // A tier from a future confiture. The entry must survive with
        // `tier: None`: rejecting it would let one producer-side addition
        // discard the whole change-set, turning a confiture release into a
        // fraisier outage.
        let change: SchemaChange = serde_json::from_str(
            r#"{"kind": "entangle_column", "object": "public.tb_user.spin_state",
                "tier": "quantum", "detail": "a tier this build does not recognise"}"#,
        )
        .expect("an unrecognised tier must not fail the entry");
        assert_eq!(change.tier, None);
        assert_eq!(change.object, "public.tb_user.spin_state");
        assert_eq!(
            change.detail.as_deref(),
            Some("a tier this build does not recognise"),
            "the rest of the entry is still rendered to the operator"
        );
    }

    #[test]
    fn a_null_or_non_string_tier_is_unclassified_not_a_parse_failure() {
        // Every shape confusion resolves the same way: unclassified. `null` is
        // the documented "adapter looked and could not say"; the others are
        // producer bugs that must still leave the change visible in the plan.
        for raw in ["null", "3", "{}", "[]", "true", "\"DESTRUCTIVE\""] {
            let json = format!(
                r#"{{"kind": "drop_column", "object": "public.tb_user.x", "tier": {raw}}}"#
            );
            let change: SchemaChange = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("tier {raw} must not fail the entry: {e}"));
            assert_eq!(change.tier, None, "tier {raw} must be unclassified");
        }
    }

    #[test]
    fn a_newly_built_change_is_unclassified_until_a_tier_is_stated() {
        // The constructor must not seed a tier. An adapter that builds a change
        // and forgets to classify it produces an unclassified change — which the
        // gate denies — not an accidentally `additive` one.
        let change = SchemaChange::new("drop_column", "public.tb_user.legacy_flag");
        assert_eq!(change.tier, None);
        assert_eq!(
            change.with_tier(RiskTier::Irreversible).tier,
            Some(RiskTier::Irreversible),
            "stating a tier is an explicit act"
        );
    }

    #[test]
    fn a_change_missing_kind_or_object_does_not_parse() {
        // `kind` and `object` are what the plan render and the audit trail are
        // written from; an entry without them identifies nothing. Leniency is
        // scoped to `tier` alone.
        for json in [
            r#"{"object": "public.tb_user.x", "tier": "additive"}"#,
            r#"{"kind": "drop_column", "tier": "additive"}"#,
        ] {
            serde_json::from_str::<SchemaChange>(json)
                .expect_err("kind and object are required by the contract");
        }
    }

    #[test]
    fn an_empty_change_set_is_not_the_same_as_no_change_set() {
        // The distinction the whole design rests on, asserted at the type level:
        // `Some(empty)` is "the adapter looked and there is nothing to change";
        // `None` is "nobody looked". A bare `changes: []` would conflate them,
        // and the conflation resolves in the dangerous direction — an
        // unclassified migration presenting as a clean, empty plan.
        let empty = ChangeSet::new(Vec::new());
        let looked: Option<ChangeSet> = Some(empty.clone());
        let nobody_looked: Option<ChangeSet> = None;
        assert_ne!(looked, nobody_looked);
        assert!(empty.changes.is_empty());
        assert_eq!(empty.worst_tier(), None);
    }

    #[test]
    fn worst_tier_picks_the_most_severe() {
        // Independently of the order the adapter listed them in: the set below is
        // in migration order, which is not severity order.
        let set = ChangeSet::new(vec![
            SchemaChange::new("add_column", "public.tb_user.nickname")
                .with_tier(RiskTier::Additive),
            SchemaChange::new("create_index", "public.tb_order.idx_placed_at")
                .with_tier(RiskTier::LockRisky),
            SchemaChange::new("drop_column", "public.tb_user.legacy_flag")
                .with_tier(RiskTier::Irreversible),
        ]);
        assert_eq!(set.worst_tier(), Some(RiskTier::Irreversible));
        assert_eq!(
            set.changes.first().expect("first").kind,
            "add_column",
            "the adapter's order is preserved for the plan render"
        );
    }

    #[test]
    fn worst_tier_is_none_when_empty() {
        assert_eq!(ChangeSet::new(Vec::new()).worst_tier(), None);
    }

    #[test]
    fn worst_tier_ignores_unclassified_changes_which_unclassified_reports() {
        // `worst_tier` answers "how bad is the worst *known* change" — it must
        // not silently absorb an unclassified one, because a set whose worst
        // known tier is `additive` reads as approvable. The unclassified changes
        // are surfaced separately, and the gate is what refuses on them.
        let set = ChangeSet::new(vec![
            SchemaChange::new("add_column", "public.tb_user.nickname")
                .with_tier(RiskTier::Additive),
            SchemaChange::new("alter_column_type", "public.tb_order.total_cents"),
            SchemaChange::new("entangle_column", "public.tb_user.spin_state"),
        ]);
        assert_eq!(set.worst_tier(), Some(RiskTier::Additive));
        let unclassified: Vec<&str> = set.unclassified().map(|c| c.object.as_str()).collect();
        assert_eq!(
            unclassified,
            ["public.tb_order.total_cents", "public.tb_user.spin_state"],
            "every untiered change must be nameable in the refusal reason"
        );
    }

    #[test]
    fn a_change_set_built_here_carries_this_builds_contract_version() {
        // An in-process adapter that emits a change-set is emitting *this*
        // contract; a zero-valued default version would sail past the version
        // check as if it had been stamped.
        assert_eq!(
            ChangeSet::new(Vec::new()).contract_version,
            RISK_CONTRACT_VERSION
        );
        assert_eq!(ChangeSet::default().contract_version, RISK_CONTRACT_VERSION);
        assert_ne!(
            ChangeSet::default().contract_version,
            0,
            "majors start at 1; a zero would sail past the version check unstamped"
        );
    }

    /// The counterpart, for an adapter that *parses* a change-set rather than
    /// producing one: the producer's version has to survive reconstruction, or
    /// the too-new refusal cannot name it — and a payload from a contract we
    /// cannot read would present as one we can.
    #[test]
    fn a_reconstructed_change_set_keeps_the_producers_contract_version() {
        let from_the_wire = ChangeSet::new(Vec::new()).with_contract_version(2);
        assert_eq!(from_the_wire.contract_version, 2);
        assert_eq!(
            PreflightReport::new(true)
                .with_change_set(from_the_wire)
                .usable_change_set(),
            Err(ChangeSetUnavailable::VersionTooNew {
                found: 2,
                understood: RISK_CONTRACT_VERSION,
            })
        );
    }

    #[test]
    fn absent_changes_is_an_empty_change_set_but_absent_version_is_not_a_change_set() {
        // `changes` absent means the producer classified and found nothing…
        let classified: ChangeSet =
            serde_json::from_str(r#"{"contract_version": 1}"#).expect("changes defaults to empty");
        assert!(classified.changes.is_empty());
        // …while `contract_version` is what makes the envelope an envelope. An
        // unversioned payload is unreadable, not empty.
        serde_json::from_str::<ChangeSet>(r#"{"changes": []}"#)
            .expect_err("an unversioned change-set is not a change-set");
    }

    #[test]
    fn a_report_without_a_change_set_round_trips() {
        // A payload shaped like confiture 0.38's — the back-compat baseline. An
        // adapter that predates this contract must keep working unchanged, and
        // must land in the "did not classify" state, not a default one.
        let report: PreflightReport =
            serde_json::from_str(r#"{"ok": true, "issues": [], "window_safe": true}"#)
                .expect("a pre-contract report still deserializes");
        assert!(report.ok);
        assert_eq!(report.window_safe, Some(true));
        assert_eq!(report.change_set, None);
        assert_eq!(
            report.usable_change_set(),
            Err(ChangeSetUnavailable::NotEmitted),
            "absent is unclassified, never safe"
        );
    }

    #[test]
    fn change_set_is_omitted_when_absent() {
        // Serialization stays byte-identical for adapters that do not classify,
        // so no existing downstream consumer sees a new `null` key appear.
        let report = PreflightReport {
            ok: true,
            window_safe: Some(true),
            ..Default::default()
        };
        let json = serde_json::to_string(&report).expect("serialize");
        assert!(
            !json.contains("change_set"),
            "an unclassified report must not grow a key: {json}"
        );
    }

    #[test]
    fn a_future_contract_version_is_treated_as_absent() {
        // We cannot read a payload written to a contract we do not know, so we
        // do not guess at it. Naming both versions is the actionable part of the
        // refusal — it tells the operator which side to upgrade.
        let report: PreflightReport = serde_json::from_str(
            r#"{"ok": true, "issues": [],
                "change_set": {"contract_version": 2, "changes": [
                    {"kind": "drop_column", "object": "public.tb_user.legacy_flag",
                     "tier": "irreversible"}
                ]}}"#,
        )
        .expect("a future change-set still deserializes; it is the *use* that is refused");
        assert!(
            report.change_set.is_some(),
            "the payload is retained for diagnostics"
        );
        assert_eq!(
            report.usable_change_set(),
            Err(ChangeSetUnavailable::VersionTooNew {
                found: 2,
                understood: RISK_CONTRACT_VERSION,
            })
        );
        let reason = report.usable_change_set().expect_err("too new").to_string();
        assert!(
            reason.contains('2') && reason.contains(&RISK_CONTRACT_VERSION.to_string()),
            "both versions must be named: {reason}"
        );
    }

    #[test]
    fn a_malformed_change_set_envelope_does_not_break_the_report() {
        // A broken *envelope* invalidates the change-set, not the whole report:
        // preflight's lint and `window_safe` predate this contract and must keep
        // working. A hard parse error here would turn a producer's typo into a
        // failed deploy for operators who never asked for risk tiers — while
        // still leaving the change-set unusable, which is the safe part.
        for broken in [
            r#""additive""#,                // a string where the object belongs
            "[]",                           // a bare array — the shape the ADR forbids
            r#"{"changes": []}"#,           // no contract_version
            r#"{"contract_version": "1"}"#, // a version that is not an integer
            r#"{"contract_version": 0}"#,   // a version outside the contract's domain
        ] {
            let json = format!(r#"{{"ok": true, "issues": [], "change_set": {broken}}}"#);
            let report: PreflightReport = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("a broken envelope must not fail the report: {e}"));
            assert!(report.ok);
            assert_eq!(
                report.usable_change_set(),
                Err(ChangeSetUnavailable::NotEmitted),
                "broken envelope {broken} must be unusable"
            );
        }
    }

    #[test]
    fn a_current_version_change_set_is_usable_and_round_trips() {
        let report = PreflightReport {
            ok: true,
            issues: vec![PreflightIssue {
                severity: Severity::Warning,
                code: "PFLIGHT_NON_TRANSACTIONAL".to_owned(),
                message: "non-transactional statement".to_owned(),
                migration: Some("20260804120050".to_owned()),
            }],
            window_safe: Some(true),
            change_set: Some(ChangeSet::new(vec![SchemaChange::new(
                "drop_column",
                "public.tb_user.legacy_flag",
            )
            .with_tier(RiskTier::Irreversible)])),
        };
        let json = serde_json::to_string(&report).expect("serialize");
        let back: PreflightReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            back, report,
            "the change-set survives the JSON-RPC boundary"
        );
        let set = back.usable_change_set().expect("current version is usable");
        assert_eq!(set.worst_tier(), Some(RiskTier::Irreversible));
    }

    #[test]
    fn a_passing_report_still_carries_no_classification() {
        // `ok: true` answers the lint's question, not the risk question. An
        // adapter that lints clean and does not classify must not read as
        // "classified, and nothing risky" — that is the exact conflation the
        // change-set's presence semantics exist to prevent.
        for report in [PreflightReport::default(), PreflightReport::new(true)] {
            assert_eq!(
                report.usable_change_set(),
                Err(ChangeSetUnavailable::NotEmitted)
            );
        }
    }

    #[tokio::test]
    async fn an_adapter_that_does_not_advertise_risk_tier_has_no_change_set() {
        // The capability is the honest signal "I do not classify". An adapter
        // that lints but does not classify advertises `preflight` and not
        // `risk_tier`, and its report carries no change-set — consistently, so a
        // consumer that checks either one reaches the same verdict.
        let described = MinimalMigration
            .describe()
            .await
            .expect("describe succeeds");
        assert!(!described.capabilities.iter().any(|c| c == "risk_tier"));

        let report = PreflightReport::new(true).with_window_safe(true);
        assert_eq!(
            report.usable_change_set(),
            Err(ChangeSetUnavailable::NotEmitted),
            "a window-safe migration is still an unclassified one"
        );
    }
}
