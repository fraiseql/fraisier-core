//! [`IpcArtifactAdapter`]: an [`ArtifactAdapter`] backed by an external process,
//! optionally run **on the target host** over `ssh`.
//!
//! This is the artifact half of the IPC-over-SSH model: the rich in-process
//! artifact adapter (e.g. `fraisier-adapter-release`, which downloads + verifies +
//! symlink-swaps a release) is packaged as a JSON-RPC binary and run *where the
//! files live*. With a [`Launcher::Local`] it runs on the orchestrator; with a
//! [`Launcher::Ssh`] the orchestrator launches it on each host (`ssh host --
//! fraisier-adapter-release`) and the adapter does its filesystem/HTTP work there,
//! unchanged. The same binary serves both — the convergence rule.

use std::ffi::OsString;
use std::time::Duration;

use async_trait::async_trait;
use fraisier_core::adapter_axes::{
    AdapterCtx, AdapterError, ArtifactAdapter, ArtifactRef, HostId, StagedArtifact,
};

use crate::client::IpcClient;
use crate::launcher::Launcher;

/// An [`ArtifactAdapter`] that speaks the JSON-RPC adapter protocol to an external
/// `fraisier-adapter-<name>` process.
///
/// # Example
/// ```no_run
/// # use fraisier_core::adapter_axes::{ArtifactAdapter, AdapterCtx, HostId};
/// # use fraisier_ipc::{IpcArtifactAdapter, Launcher, SshLauncher};
/// # async fn run() -> Result<(), fraisier_core::adapter_axes::AdapterError> {
/// // Run the release adapter on each host over ssh (address from the ctx):
/// let adapter = IpcArtifactAdapter::new("fraisier-adapter-release", "release")
///     .with_launcher(Launcher::ssh(SshLauncher::new().with_user("deploy")));
/// let mut ctx = AdapterCtx::new("app", "prod");
/// ctx.settings.insert("address".into(), "web1.internal".into());
/// let staged = adapter.stage(&ctx, &HostId::new("web1")).await?;
/// adapter.activate(&ctx, &HostId::new("web1"), &staged).await?;
/// # Ok(())
/// # }
/// ```
pub struct IpcArtifactAdapter {
    client: IpcClient,
}

impl IpcArtifactAdapter {
    /// Create an adapter that launches `program`, labelled `name` in errors,
    /// locally (use [`with_launcher`](Self::with_launcher) for ssh).
    #[must_use]
    pub fn new(program: impl Into<OsString>, name: impl Into<String>) -> Self {
        Self {
            client: IpcClient::new(program, name),
        }
    }

    /// Run the adapter via `launcher` instead of locally (builder style) — the
    /// multi-host path passes a [`Launcher::Ssh`] so each host runs its own copy.
    #[must_use]
    pub fn with_launcher(mut self, launcher: Launcher) -> Self {
        self.client = self.client.with_launcher(launcher);
        self
    }

    /// Override the per-call timeout (builder style).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.client = self.client.with_timeout(timeout);
        self
    }
}

#[async_trait]
impl ArtifactAdapter for IpcArtifactAdapter {
    async fn stage(&self, ctx: &AdapterCtx, host: &HostId) -> Result<StagedArtifact, AdapterError> {
        self.client
            .call(
                Some(ctx),
                "stage",
                serde_json::json!({ "ctx": ctx, "host": host }),
            )
            .await
    }

    async fn activate(
        &self,
        ctx: &AdapterCtx,
        host: &HostId,
        staged: &StagedArtifact,
    ) -> Result<(), AdapterError> {
        self.client
            .call(
                Some(ctx),
                "activate",
                serde_json::json!({ "ctx": ctx, "host": host, "staged": staged }),
            )
            .await
    }

    async fn current(
        &self,
        ctx: &AdapterCtx,
        host: &HostId,
    ) -> Result<Option<ArtifactRef>, AdapterError> {
        self.client
            .call(
                Some(ctx),
                "current",
                serde_json::json!({ "ctx": ctx, "host": host }),
            )
            .await
    }
}
