//! # fraisier-adapter-support
//!
//! Shared building blocks for fraisier's in-process adapters:
//!
//! - [`run_command`] — spawn a subprocess, capture its output, and turn a
//!   *spawn* failure into a tagged [`AdapterError`] (the caller interprets the
//!   exit code). Used by the shell-out adapters (`command`, `systemd`).
//! - [`retry_on_err`] — retry an async fallible operation with a fixed delay.
//!   Used by the network adapters (`http` health, `release` artifact download).
//! - [`Transport`] — run a shell-out command locally or on a remote host over
//!   `ssh`. The single-host adapters dispatch through it so a multi-host rollout
//!   can run the same commands on each host (see the [`transport`] module).
//!
//! These deliberately do *not* encode any axis-specific policy: error-kind
//! selection and result interpretation stay in each adapter.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::time::Duration;

use fraisier_core::adapter_axes::{AdapterError, AdapterErrorKind};

pub mod transport;

pub use transport::{SshTransport, Transport};

/// The captured result of a finished subprocess.
///
/// # Example
/// ```
/// # use fraisier_adapter_support::Captured;
/// let captured = Captured { code: Some(0), stdout: "ok".into(), stderr: String::new() };
/// assert!(captured.succeeded());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    /// The process exit code, or `None` if it was killed by a signal.
    pub code: Option<i32>,
    /// Captured standard output (lossy UTF-8).
    pub stdout: String,
    /// Captured standard error (lossy UTF-8).
    pub stderr: String,
}

impl Captured {
    /// Whether the process exited with code 0.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.code == Some(0)
    }

    /// `stderr` if it carries any non-whitespace text, else `None` — the shape
    /// [`AdapterError::stderr`] expects.
    #[must_use]
    pub fn stderr_opt(&self) -> Option<String> {
        (!self.stderr.trim().is_empty()).then(|| self.stderr.clone())
    }
}

/// Spawn `program` with `args`, `envs`, and optional working directory, then
/// capture its output.
///
/// Only a *spawn* failure (binary missing, permission denied, …) is an error
/// here; a process that runs and exits non-zero returns `Ok(Captured)` so the
/// caller can map the exit code to its own [`AdapterErrorKind`]. The error is
/// tagged with `adapter` and `operation`.
///
/// # Errors
/// [`AdapterError`] of kind [`AdapterErrorKind::Execution`] if the process
/// cannot be spawned.
///
/// # Example
/// ```no_run
/// # use std::ffi::OsString;
/// # async fn run() -> Result<(), fraisier_core::adapter_axes::AdapterError> {
/// let out = fraisier_adapter_support::run_command(
///     "echo".as_ref(),
///     &[OsString::from("hi")],
///     &[],
///     None,
///     "demo",
///     "echo",
/// )
/// .await?;
/// assert!(out.succeeded());
/// # Ok(())
/// # }
/// ```
pub async fn run_command(
    program: &OsStr,
    args: &[OsString],
    envs: &[(OsString, OsString)],
    cwd: Option<&Path>,
    adapter: &str,
    operation: &str,
) -> Result<Captured, AdapterError> {
    let mut command = tokio::process::Command::new(program);
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }

    let output = command.output().await.map_err(|cause| {
        let program = program.to_string_lossy();
        error(
            AdapterErrorKind::Execution,
            adapter,
            operation,
            format!("failed to spawn '{program}': {cause}"),
            None,
        )
    })?;

    Ok(Captured {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Build an [`AdapterError`] of `kind`, tagged with `adapter` and `operation`.
///
/// # Example
/// ```
/// # use fraisier_adapter_support::error;
/// # use fraisier_core::adapter_axes::AdapterErrorKind;
/// let err = error(AdapterErrorKind::Execution, "systemd", "restart", "boom".into(), None);
/// assert_eq!(err.adapter.as_deref(), Some("systemd"));
/// ```
#[must_use]
pub fn error(
    kind: AdapterErrorKind,
    adapter: &str,
    operation: &str,
    message: String,
    stderr: Option<String>,
) -> AdapterError {
    AdapterError {
        adapter: Some(adapter.to_owned()),
        operation: Some(operation.to_owned()),
        stderr,
        ..AdapterError::new(kind, message)
    }
}

/// Retry an async fallible operation up to `attempts` times (minimum one),
/// sleeping `delay` between tries, returning the first `Ok` or the last `Err`.
///
/// # Errors
/// Returns the `Err` from the final attempt if every attempt fails.
///
/// # Example
/// ```no_run
/// # use std::time::Duration;
/// # async fn demo() {
/// let result: Result<u32, ()> = fraisier_adapter_support::retry_on_err(
///     3,
///     Duration::from_millis(50),
///     || async { Ok(7) },
/// )
/// .await;
/// assert_eq!(result, Ok(7));
/// # }
/// ```
pub async fn retry_on_err<T, E, F, Fut>(attempts: u32, delay: Duration, mut op: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let attempts = attempts.max(1);
    let mut result = op().await;
    let mut done = 1;
    while result.is_err() && done < attempts {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        result = op().await;
        done += 1;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{error, retry_on_err, run_command, Captured};
    use fraisier_core::adapter_axes::AdapterErrorKind;
    use std::ffi::OsString;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    #[test]
    fn captured_helpers() {
        let ok = Captured {
            code: Some(0),
            stdout: "out".into(),
            stderr: "  ".into(),
        };
        assert!(ok.succeeded());
        assert_eq!(ok.stderr_opt(), None); // whitespace-only → None

        let failed = Captured {
            code: Some(2),
            stdout: String::new(),
            stderr: "boom".into(),
        };
        assert!(!failed.succeeded());
        assert_eq!(failed.stderr_opt().as_deref(), Some("boom"));
    }

    #[test]
    fn error_is_tagged() {
        let err = error(
            AdapterErrorKind::InvalidConfig,
            "command",
            "up",
            "bad".into(),
            Some("stderr".into()),
        );
        assert_eq!(err.kind, AdapterErrorKind::InvalidConfig);
        assert_eq!(err.adapter.as_deref(), Some("command"));
        assert_eq!(err.operation.as_deref(), Some("up"));
        assert_eq!(err.stderr.as_deref(), Some("stderr"));
    }

    #[tokio::test]
    async fn run_command_captures_output() {
        let out = run_command(
            "printf".as_ref(),
            &[OsString::from("hello")],
            &[],
            None,
            "test",
            "printf",
        )
        .await
        .expect("printf spawns");
        assert!(out.succeeded());
        assert_eq!(out.stdout, "hello");
    }

    #[tokio::test]
    async fn run_command_maps_spawn_failure() {
        let err = run_command(
            "fraisier-no-such-binary-xyz".as_ref(),
            &[],
            &[],
            None,
            "test",
            "missing",
        )
        .await
        .expect_err("a missing binary must fail to spawn");
        assert_eq!(err.kind, AdapterErrorKind::Execution);
        assert_eq!(err.operation.as_deref(), Some("missing"));
    }

    #[tokio::test]
    async fn retry_returns_first_ok() {
        let tries = AtomicU32::new(0);
        let result: Result<u32, ()> = retry_on_err(5, Duration::from_millis(0), || async {
            let n = tries.fetch_add(1, Ordering::SeqCst);
            if n >= 2 {
                Ok(n)
            } else {
                Err(())
            }
        })
        .await;
        assert_eq!(result, Ok(2));
        assert_eq!(tries.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_exhausts_and_returns_last_err() {
        let tries = AtomicU32::new(0);
        let result: Result<(), u32> = retry_on_err(3, Duration::from_millis(0), || async {
            Err(tries.fetch_add(1, Ordering::SeqCst))
        })
        .await;
        assert_eq!(result, Err(2)); // last error from the 3rd (0-indexed) attempt
        assert_eq!(tries.load(Ordering::SeqCst), 3);
    }
}
