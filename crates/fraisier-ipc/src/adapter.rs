//! [`IpcMigrationAdapter`]: a [`MigrationAdapter`] backed by an external process.

use std::ffi::OsString;
use std::time::Duration;

use async_trait::async_trait;
use fraisier_core::adapter_axes::{
    AdapterCtx, AdapterDescription, AdapterError, MigrationAdapter, MigrationOutcome,
    PreflightReport, Revision, VerifyReport,
};

use crate::client::IpcClient;

/// A [`MigrationAdapter`] that speaks the JSON-RPC adapter protocol to an external
/// `fraisier-adapter-<name>` process over stdio.
///
/// Migrations run **on the orchestrator** (Decision 5: the DSN secret never leaves
/// the box), so this adapter always launches its subprocess locally. Each trait
/// call spawns the configured program, writes one framed JSON-RPC request to its
/// stdin, reads one framed response from its stdout, and lets the child exit —
/// giving per-call crash isolation. Secrets are injected as child environment
/// variables via [`IpcMigrationAdapter::with_env`] (the core sets `env[logical] =
/// value` here; PRD review Decision 5), never in the JSON params.
///
/// # Example
/// ```no_run
/// # use fraisier_core::adapter_axes::MigrationAdapter;
/// # use fraisier_ipc::IpcMigrationAdapter;
/// # async fn run() -> Result<(), fraisier_core::adapter_axes::AdapterError> {
/// let adapter = IpcMigrationAdapter::new("fraisier-adapter-sqlx", "sqlx")
///     .with_env("DATABASE_URL", "postgres://localhost/app");
/// let desc = adapter.describe().await?;
/// println!("{} speaks protocol v{}", desc.name, desc.protocol_version);
/// # Ok(())
/// # }
/// ```
pub struct IpcMigrationAdapter {
    client: IpcClient,
}

impl IpcMigrationAdapter {
    /// Create an adapter that spawns `program`, labelled `name` in errors and traces.
    #[must_use]
    pub fn new(program: impl Into<OsString>, name: impl Into<String>) -> Self {
        Self {
            client: IpcClient::new(program, name),
        }
    }

    /// Override the per-call timeout (builder style). An adapter that does not
    /// respond within it is killed and the call fails.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.client = self.client.with_timeout(timeout);
        self
    }

    /// Set the arguments passed to the spawned program (builder style).
    #[must_use]
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.client = self.client.with_args(args);
        self
    }

    /// Set an environment variable on the spawned process (builder style). This
    /// is how the core injects a resolved secret value under its logical name.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.client = self.client.with_env(key, value);
        self
    }
}

#[async_trait]
impl MigrationAdapter for IpcMigrationAdapter {
    async fn describe(&self) -> Result<AdapterDescription, AdapterError> {
        self.client
            .call(None, "describe", serde_json::json!({}))
            .await
    }

    async fn current_revision(&self, ctx: &AdapterCtx) -> Result<Option<Revision>, AdapterError> {
        self.client
            .call(
                Some(ctx),
                "current_revision",
                serde_json::json!({ "ctx": ctx }),
            )
            .await
    }

    async fn up(
        &self,
        ctx: &AdapterCtx,
        target: Option<Revision>,
    ) -> Result<MigrationOutcome, AdapterError> {
        self.client
            .call(
                Some(ctx),
                "up",
                serde_json::json!({ "ctx": ctx, "target": target }),
            )
            .await
    }

    async fn down_to(
        &self,
        ctx: &AdapterCtx,
        target: Revision,
    ) -> Result<MigrationOutcome, AdapterError> {
        self.client
            .call(
                Some(ctx),
                "down_to",
                serde_json::json!({ "ctx": ctx, "target": target }),
            )
            .await
    }

    async fn verify(&self, ctx: &AdapterCtx) -> Result<VerifyReport, AdapterError> {
        self.client
            .call(Some(ctx), "verify", serde_json::json!({ "ctx": ctx }))
            .await
    }

    async fn preflight(&self, ctx: &AdapterCtx) -> Result<PreflightReport, AdapterError> {
        self.client
            .call(Some(ctx), "preflight", serde_json::json!({ "ctx": ctx }))
            .await
    }

    async fn post_migrate(&self, ctx: &AdapterCtx) -> Result<(), AdapterError> {
        self.client
            .call(Some(ctx), "post_migrate", serde_json::json!({ "ctx": ctx }))
            .await
    }
}
