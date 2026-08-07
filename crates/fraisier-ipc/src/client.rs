//! [`IpcClient`]: the axis-agnostic JSON-RPC-over-stdio client.
//!
//! Every fraisier IPC adapter — migration, artifact, … — is the same client with
//! a different set of trait methods mapped onto JSON-RPC calls. [`IpcClient`] owns
//! the transport mechanics (spawn the adapter via a [`Launcher`], write one framed
//! request, read one framed response, decode it) so each axis adapter is a thin
//! map of `trait method → client.call(method, params)`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use fraisier_core::adapter_axes::{AdapterCtx, AdapterError, AdapterErrorKind};
use serde::de::DeserializeOwned;
use tokio::io::{AsyncReadExt as _, BufReader};

use crate::launcher::Launcher;
use crate::{framing, protocol};

/// One request per spawn, so a fixed id is unambiguous.
const REQUEST_ID: u64 = 1;

/// Default per-call timeout: an adapter that does not answer within this is killed
/// so a hung subprocess can never wedge a deploy.
const DEFAULT_TIMEOUT: Duration = Duration::from_mins(1);

/// How many extra times to retry a spawn that fails with `ETXTBSY`.
///
/// Deliberately the same budget as `fraisier-adapter-support`'s shell-out spawn:
/// same race, same shape of answer. The two are not shared code — that crate
/// retries a run-to-completion `output()`, this one a streaming `spawn()`, and
/// `fraisier-ipc` keeps its dependency list to the protocol essentials.
const ETXTBSY_RETRIES: u32 = 5;

/// How long to wait between `ETXTBSY` spawn retries.
const ETXTBSY_BACKOFF: Duration = Duration::from_millis(20);

/// Spawn `command`, retrying a few times on `ETXTBSY` ("text file busy").
///
/// Linux refuses to `exec` a file any process still holds open for writing, so a
/// spawn can lose a race against a writer that is about to close: a concurrent
/// install of the adapter binary, or — the common case in this repo's own test
/// suite — a sibling thread that forked while a writer fd to a just-written
/// executable was still open. It is the one spawn failure that is *always*
/// transient, so a short bounded retry resolves it without masking a genuine
/// one: a missing binary is `NotFound` and an unexecutable one is
/// `PermissionDenied`, neither of which is retried.
async fn spawn_with_etxtbsy_retry(
    command: &mut tokio::process::Command,
) -> std::io::Result<tokio::process::Child> {
    for _ in 0..ETXTBSY_RETRIES {
        match command.spawn() {
            Err(cause) if cause.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                tokio::time::sleep(ETXTBSY_BACKOFF).await;
            }
            other => return other,
        }
    }
    command.spawn()
}

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

/// A JSON-RPC-over-stdio client for a `fraisier-adapter-<name>` process.
///
/// Each call launches the adapter via its [`Launcher`] (locally, or on a remote
/// host over `ssh`), writes one framed JSON-RPC request to its stdin, reads one
/// framed response from its stdout, and lets the child exit — per-call crash
/// isolation regardless of where it ran.
pub struct IpcClient {
    program: OsString,
    args: Vec<OsString>,
    envs: BTreeMap<OsString, OsString>,
    name: String,
    timeout: Duration,
    launcher: Launcher,
}

impl IpcClient {
    /// A client that launches `program` (labelled `name` in errors), locally.
    pub fn new(program: impl Into<OsString>, name: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            envs: BTreeMap::new(),
            name: name.into(),
            timeout: DEFAULT_TIMEOUT,
            launcher: Launcher::Local,
        }
    }

    /// Override the per-call timeout (builder style).
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the arguments passed to the spawned program (builder style).
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Set an environment variable on the spawned process (builder style). Applied
    /// only on the [`Launcher::Local`] path; the Ssh launcher carries no env onto a
    /// remote argv.
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.envs.insert(key.into(), value.into());
        self
    }

    /// Run the adapter via `launcher` instead of locally (builder style).
    pub fn with_launcher(mut self, launcher: Launcher) -> Self {
        self.launcher = launcher;
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
        let spawned = self.launcher.spawn_program(&self.program);
        let program = spawned.to_string_lossy();
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

    /// Launch the adapter, send one `method` request with `params`, and decode the
    /// result as `R`. `ctx` supplies the target host for the Ssh launcher (the
    /// Local launcher ignores it; pass `None` for context-free calls like
    /// `describe`).
    pub async fn call<R: DeserializeOwned>(
        &self,
        ctx: Option<&AdapterCtx>,
        method: &str,
        params: serde_json::Value,
    ) -> Result<R, AdapterError> {
        let (body, stderr) = self.transact(ctx, method, &params).await?;
        self.decode_response(method, &body, stderr)
    }

    /// Launch the child, send the framed request, and return the framed response
    /// body together with any captured stderr.
    async fn transact(
        &self,
        ctx: Option<&AdapterCtx>,
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

        let mut command =
            self.launcher
                .command(&self.program, &self.args, &self.envs, ctx, method)?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = spawn_with_etxtbsy_retry(&mut command)
            .await
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
                let spawned = self.launcher.spawn_program(&self.program);
                let program = spawned.to_string_lossy();
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
                let spawned = self.launcher.spawn_program(&self.program);
                let program = spawned.to_string_lossy();
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
