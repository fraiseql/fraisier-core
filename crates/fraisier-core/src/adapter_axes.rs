//! The five adapter axis traits (PRD §6.1) and their shared vocabulary types.
//!
//! # Frozen
//!
//! These traits are **frozen** as of the Phase 1 owner review. Every argument and
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
/// This is `params.ctx` on the JSON-RPC wire; its field set is frozen.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::AdapterCtx;
/// let ctx = AdapterCtx::new("checkout", "production");
/// assert_eq!(ctx.fraise, "checkout");
/// ```
// No `PartialEq`: `settings` holds `serde_json::Value`, which is not `Eq`, and
// nothing compares whole contexts.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
            previous_revision: None,
            artifact_ref: None,
            settings: BTreeMap::new(),
        }
    }

    /// Resolve a secret by its logical name.
    ///
    /// Behaves identically in-process and over IPC: it looks up `logical` in
    /// [`AdapterCtx::env_secrets`] to find the source env var name, then reads
    /// that variable from the process environment. The value never travels in
    /// JSON params or argv.
    ///
    /// # Errors
    /// [`AdapterErrorKind::MissingSecret`] if `logical` is not declared, or
    /// [`AdapterErrorKind::SecretReadFailed`] if the mapped env var is unset or
    /// not valid UTF-8.
    pub fn secret(&self, logical: &str) -> Result<String, AdapterError> {
        let source = self
            .env_secrets
            .get(logical)
            .ok_or_else(|| AdapterError::missing_secret(logical))?;
        std::env::var(source)
            .map_err(|cause| AdapterError::secret_read_failed(logical, source, &cause))
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

/// The result of a `preflight` forward-compatibility lint (PRD review Decision 4).
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::PreflightReport;
/// let report = PreflightReport { ok: true, ..Default::default() };
/// assert!(report.ok);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// The methods the adapter actually implements (gates optional calls like
    /// `preflight`).
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

    /// Forward-compatibility lint (PRD review Decision 4).
    ///
    /// Defaults to [`AdapterErrorKind::MethodNotSupported`] — never a passing
    /// report — so that "I can't lint" can never masquerade as "lint passed".
    /// The deploy layer gates this call on [`AdapterDescription::capabilities`].
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
        AdapterCtx, AdapterDescription, AdapterError, AdapterErrorKind, MigrationAdapter,
        MigrationOutcome, Revision, VerifyReport,
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
            previous_revision: None,
            artifact_ref: None,
            settings: BTreeMap::new(),
        }
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
    fn adapter_ctx_round_trips_through_serde() {
        // AdapterCtx crosses the JSON-RPC boundary as params.ctx.
        let json = serde_json::to_string(&ctx()).expect("serialize");
        let back: AdapterCtx = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.fraise, "checkout");
        assert_eq!(back.environment, "production");
    }
}
