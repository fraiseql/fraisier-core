//! [`IpcMigrationAdapter`]: a [`MigrationAdapter`] backed by an external process.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use async_trait::async_trait;
use fraisier_core::adapter_axes::{
    AdapterCtx, AdapterDescription, AdapterError, AdapterErrorKind, MigrationAdapter,
    MigrationOutcome, PreflightReport, Revision, VerifyReport,
};
use serde::de::DeserializeOwned;
use tokio::io::{AsyncReadExt as _, BufReader};
use tokio::process::Command;

use crate::{framing, protocol};

/// One request per spawn, so a fixed id is unambiguous.
const REQUEST_ID: u64 = 1;

/// Default per-call timeout: an adapter that does not answer within this is killed
/// so a hung subprocess can never wedge a deploy.
const DEFAULT_TIMEOUT: Duration = Duration::from_mins(1);

/// Render an exit status for an error message.
fn describe_exit(status: ExitStatus) -> String {
    status.code().map_or_else(
        || "after a signal".to_owned(),
        |code| format!("with status {code}"),
    )
}

/// `Some(buf)` if `buf` has non-whitespace content, else `None`.
fn stderr_opt(buf: String) -> Option<String> {
    (!buf.trim().is_empty()).then_some(buf)
}

/// A [`MigrationAdapter`] that speaks the JSON-RPC adapter protocol to an external
/// `fraisier-adapter-<name>` process over stdio.
///
/// Each trait call spawns the configured program, writes one framed JSON-RPC
/// request to its stdin, reads one framed response from its stdout, and lets the
/// child exit — giving per-call crash isolation. Secrets are injected as child
/// environment variables via [`IpcMigrationAdapter::with_env`] (the core sets
/// `env[logical] = value` here; PRD review Decision 5), never in the JSON params.
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
    program: OsString,
    args: Vec<OsString>,
    envs: BTreeMap<OsString, OsString>,
    name: String,
    timeout: Duration,
}

impl IpcMigrationAdapter {
    /// Create an adapter that spawns `program`, labelled `name` in errors and traces.
    #[must_use]
    pub fn new(program: impl Into<OsString>, name: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            envs: BTreeMap::new(),
            name: name.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Override the per-call timeout (builder style). An adapter that does not
    /// respond within it is killed and the call fails.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the arguments passed to the spawned program (builder style).
    #[must_use]
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Set an environment variable on the spawned process (builder style). This
    /// is how the core injects a resolved secret value under its logical name.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.envs.insert(key.into(), value.into());
        self
    }

    /// Build an error tagged with this adapter and operation.
    fn fail(
        &self,
        kind: AdapterErrorKind,
        operation: &str,
        message: String,
        stderr: Option<String>,
    ) -> AdapterError {
        AdapterError {
            adapter: Some(self.name.clone()),
            operation: Some(operation.to_owned()),
            stderr,
            ..AdapterError::new(kind, message)
        }
    }

    /// Classify a spawn failure: a missing binary ("not found on PATH") reads very
    /// differently from any other spawn error, and operators hit the former most.
    fn spawn_error(&self, method: &str, error: &std::io::Error) -> AdapterError {
        let program = self.program.to_string_lossy();
        let message = if error.kind() == std::io::ErrorKind::NotFound {
            format!(
                "adapter '{program}' (name '{}') was not found on PATH",
                self.name
            )
        } else {
            format!("failed to spawn adapter '{program}': {error}")
        };
        self.fail(AdapterErrorKind::Execution, method, message, None)
    }

    /// Spawn the adapter, send one `method` request with `params`, and decode the
    /// result as `R`.
    async fn call<R: DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<R, AdapterError> {
        let (body, stderr) = self.transact(method, &params).await?;
        self.decode_response(method, &body, stderr)
    }

    /// Spawn the child, send the framed request, and return the framed response
    /// body together with any captured stderr.
    async fn transact(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<(Vec<u8>, Option<String>), AdapterError> {
        let request = serde_json::json!({ "jsonrpc": "2.0", "id": REQUEST_ID, "method": method, "params": params });
        let request = serde_json::to_vec(&request).map_err(|e| {
            self.fail(
                AdapterErrorKind::Protocol,
                method,
                format!("failed to encode request: {e}"),
                None,
            )
        })?;

        let mut child = Command::new(&self.program)
            .args(&self.args)
            .envs(&self.envs)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| self.spawn_error(method, &e))?;

        // Send the request, then signal EOF by dropping stdin. The request is
        // small (well under a pipe buffer), so writing before reading is safe.
        {
            let mut stdin = child.stdin.take().expect("stdin is piped");
            framing::write_message(&mut stdin, &request)
                .await
                .map_err(|e| {
                    self.fail(
                        AdapterErrorKind::Protocol,
                        method,
                        format!("failed to send request: {e}"),
                        None,
                    )
                })?;
        }

        let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
        let mut stderr = child.stderr.take().expect("stderr is piped");

        // Read the response and drain stderr concurrently (avoids a pipe-fill
        // deadlock), bounded by the call timeout so a hung adapter cannot wedge a
        // deploy.
        let mut stderr_buf = String::new();
        let exchange = async {
            tokio::join!(
                framing::read_message(&mut stdout),
                stderr.read_to_string(&mut stderr_buf),
            )
        };
        let message = match tokio::time::timeout(self.timeout, exchange).await {
            Ok((message, _)) => message,
            Err(_elapsed) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                let program = self.program.to_string_lossy();
                return Err(self.fail(
                    AdapterErrorKind::Execution,
                    method,
                    format!(
                        "adapter '{program}' did not respond within {:?} and was killed",
                        self.timeout
                    ),
                    stderr_opt(stderr_buf),
                ));
            }
        };
        let stderr = stderr_opt(stderr_buf);

        let status = child.wait().await.map_err(|e| {
            self.fail(
                AdapterErrorKind::Execution,
                method,
                format!("failed to await adapter exit: {e}"),
                stderr.clone(),
            )
        })?;

        let body = message
            .map_err(|e| {
                self.fail(
                    AdapterErrorKind::Protocol,
                    method,
                    format!("failed to read response: {e}"),
                    stderr.clone(),
                )
            })?
            .ok_or_else(|| {
                let program = self.program.to_string_lossy();
                self.fail(
                    AdapterErrorKind::Execution,
                    method,
                    format!(
                        "adapter '{program}' exited {} without sending a response",
                        describe_exit(status)
                    ),
                    stderr.clone(),
                )
            })?;
        Ok((body, stderr))
    }

    /// Parse a JSON-RPC response body into `R`, mapping a JSON-RPC error (or a
    /// missing/ill-formed result) to an [`AdapterError`].
    fn decode_response<R: DeserializeOwned>(
        &self,
        method: &str,
        body: &[u8],
        stderr: Option<String>,
    ) -> Result<R, AdapterError> {
        let response: protocol::Response = serde_json::from_slice(body).map_err(|e| {
            self.fail(
                AdapterErrorKind::Protocol,
                method,
                format!("invalid JSON-RPC response: {e}"),
                stderr.clone(),
            )
        })?;

        if let Some(id) = response.id {
            if id != REQUEST_ID {
                return Err(self.fail(
                    AdapterErrorKind::Protocol,
                    method,
                    format!("response id {id} did not match request id {REQUEST_ID}"),
                    stderr,
                ));
            }
        }

        if let Some(err) = response.error {
            let message = err.data.map_or_else(
                || err.message.clone(),
                |data| format!("{} (data: {data})", err.message),
            );
            return Err(AdapterError {
                adapter: Some(self.name.clone()),
                operation: Some(method.to_owned()),
                stderr,
                ..AdapterError::remote(err.code, message)
            });
        }

        let result = response.result.ok_or_else(|| {
            self.fail(
                AdapterErrorKind::Protocol,
                method,
                "response had neither result nor error".to_owned(),
                stderr.clone(),
            )
        })?;
        serde_json::from_value(result).map_err(|e| {
            self.fail(
                AdapterErrorKind::Protocol,
                method,
                format!("failed to decode result: {e}"),
                stderr,
            )
        })
    }
}

#[async_trait]
impl MigrationAdapter for IpcMigrationAdapter {
    async fn describe(&self) -> Result<AdapterDescription, AdapterError> {
        self.call("describe", serde_json::json!({})).await
    }

    async fn current_revision(&self, ctx: &AdapterCtx) -> Result<Option<Revision>, AdapterError> {
        self.call("current_revision", serde_json::json!({ "ctx": ctx }))
            .await
    }

    async fn up(
        &self,
        ctx: &AdapterCtx,
        target: Option<Revision>,
    ) -> Result<MigrationOutcome, AdapterError> {
        self.call("up", serde_json::json!({ "ctx": ctx, "target": target }))
            .await
    }

    async fn down_to(
        &self,
        ctx: &AdapterCtx,
        target: Revision,
    ) -> Result<MigrationOutcome, AdapterError> {
        self.call(
            "down_to",
            serde_json::json!({ "ctx": ctx, "target": target }),
        )
        .await
    }

    async fn verify(&self, ctx: &AdapterCtx) -> Result<VerifyReport, AdapterError> {
        self.call("verify", serde_json::json!({ "ctx": ctx })).await
    }

    async fn preflight(&self, ctx: &AdapterCtx) -> Result<PreflightReport, AdapterError> {
        self.call("preflight", serde_json::json!({ "ctx": ctx }))
            .await
    }

    async fn post_migrate(&self, ctx: &AdapterCtx) -> Result<(), AdapterError> {
        self.call("post_migrate", serde_json::json!({ "ctx": ctx }))
            .await
    }
}
