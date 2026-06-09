//! # fraisier-adapter-rc
//!
//! The [`RcService`] adapter: a [`ServiceAdapter`] that drives a FreeBSD rc.d
//! service through the `service(8)` CLI (PRD §6.3 — shell out in v1.0).
//!
//! ## Configuration
//!
//! Read per call from [`AdapterCtx::settings`] (the `[service]` table):
//!
//! ```toml
//! [service]
//! adapter = "rc"
//! name = "fraiseql"     # the rc.d service name (the `service <name> …` argument)
//! ```
//!
//! ## `service(8)` argument order
//!
//! Unlike `systemctl <verb> <unit>`, FreeBSD's `service` takes the name *before*
//! the command: `service <name> restart`, `service <name> status`. The status
//! sub-command reports `"<name> is running as pid N."` (exit 0) or
//! `"<name> is not running."` (exit 1); [`RcService::status`] reads that text and
//! falls back to the exit code, so a stopped service is a normal `running: false`
//! result rather than an error.
//!
//! ## Locality
//!
//! By default `service` runs on the **local** host; build with
//! [`RcService::with_transport`] and a [`Transport::Ssh`] to run it on a remote
//! host (the multi-host rollout does this per host). The adapter never assumes
//! privilege — escalation (sudo) is the operator's concern.

use std::ffi::OsString;

use async_trait::async_trait;
use fraisier_adapter_support::{error, Captured, Transport};
use fraisier_core::adapter_axes::{
    AdapterCtx, AdapterError, AdapterErrorKind, HostId, ServiceAdapter, ServiceStatus,
};
use serde_json::Value;

/// The adapter's identity name.
const ADAPTER_NAME: &str = "rc";

/// The env var that overrides which `service` binary the adapter spawns.
const PROGRAM_ENV: &str = "FRAISIER_SERVICE_BIN";

/// A [`ServiceAdapter`] backed by FreeBSD's `service(8)`.
///
/// # Example
/// ```
/// use fraisier_adapter_rc::RcService;
///
/// let adapter = RcService::new();
/// let pinned = RcService::with_program("/usr/sbin/service");
/// let _ = (adapter, pinned);
/// ```
pub struct RcService {
    program: OsString,
    transport: Transport,
}

impl Default for RcService {
    fn default() -> Self {
        Self::new()
    }
}

impl RcService {
    /// Create an adapter that spawns `service` (honouring the
    /// `FRAISIER_SERVICE_BIN` override) on the **local** host.
    #[must_use]
    pub fn new() -> Self {
        let program = std::env::var_os(PROGRAM_ENV)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| OsString::from("service"));
        Self {
            program,
            transport: Transport::Local,
        }
    }

    /// Create an adapter that spawns the binary at `program`.
    #[must_use]
    pub fn with_program(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            transport: Transport::Local,
        }
    }

    /// Run `service` over `transport` instead of locally (the multi-host path
    /// passes a [`Transport::Ssh`] to manage the service on each remote host).
    #[must_use]
    pub fn with_transport(mut self, transport: Transport) -> Self {
        self.transport = transport;
        self
    }

    /// Build the argv for a `service` `verb` against the configured service:
    /// `<name> <verb>` (the name precedes the command, unlike `systemctl`).
    fn args_for(
        ctx: &AdapterCtx,
        verb: &str,
        operation: &str,
    ) -> Result<Vec<OsString>, AdapterError> {
        let name = ctx
            .settings
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                error(
                    AdapterErrorKind::InvalidConfig,
                    ADAPTER_NAME,
                    operation,
                    "no 'name' configured in [service] settings".to_owned(),
                    None,
                )
            })?;
        Ok(vec![OsString::from(name), OsString::from(verb)])
    }

    /// Run a `service` `verb`, returning the captured output.
    async fn service(
        &self,
        ctx: &AdapterCtx,
        verb: &str,
        operation: &str,
    ) -> Result<Captured, AdapterError> {
        let args = Self::args_for(ctx, verb, operation)?;
        self.transport
            .run(
                ctx,
                &self.program,
                &args,
                &[],
                None,
                ADAPTER_NAME,
                operation,
            )
            .await
    }
}

/// Interpret a `service <name> status` result.
///
/// rc.d status text is the source of truth (`"is not running"` is checked before
/// `"is running"` so it can't be masked); when the script prints neither phrase,
/// the exit code decides.
fn parse_status(captured: &Captured) -> ServiceStatus {
    let text = captured.stdout.trim();
    let running = if text.contains("is not running") {
        false
    } else if text.contains("is running") {
        true
    } else {
        captured.succeeded()
    };
    ServiceStatus {
        running,
        detail: text.lines().next().map(str::to_owned),
    }
}

#[async_trait]
impl ServiceAdapter for RcService {
    async fn restart(&self, ctx: &AdapterCtx, _host: &HostId) -> Result<(), AdapterError> {
        let captured = self.service(ctx, "restart", "restart").await?;
        if captured.succeeded() {
            return Ok(());
        }
        let code = captured
            .code
            .map_or_else(|| "signal".to_owned(), |code| code.to_string());
        Err(error(
            AdapterErrorKind::Execution,
            ADAPTER_NAME,
            "restart",
            format!("`service <name> restart` exited with {code}"),
            captured.stderr_opt(),
        ))
    }

    async fn status(
        &self,
        ctx: &AdapterCtx,
        _host: &HostId,
    ) -> Result<ServiceStatus, AdapterError> {
        // A stopped service exits non-zero; that is informational here, not a
        // spawn error, so only a failure to spawn `service` propagates.
        let captured = self.service(ctx, "status", "status").await?;
        Ok(parse_status(&captured))
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_status, RcService};
    use fraisier_adapter_support::Captured;
    use fraisier_core::adapter_axes::AdapterCtx;
    use serde_json::json;

    fn captured(code: i32, stdout: &str) -> Captured {
        Captured {
            code: Some(code),
            stdout: stdout.to_owned(),
            stderr: String::new(),
        }
    }

    #[test]
    fn args_require_a_name() {
        let ctx = AdapterCtx::new("checkout", "production");
        let err =
            RcService::args_for(&ctx, "restart", "restart").expect_err("missing name must fail");
        assert_eq!(err.adapter.as_deref(), Some("rc"));
    }

    #[test]
    fn args_put_the_name_before_the_verb() {
        let mut ctx = AdapterCtx::new("checkout", "production");
        ctx.settings.insert("name".to_owned(), json!("fraiseql"));
        let args = RcService::args_for(&ctx, "status", "status").expect("args");
        assert_eq!(args, vec!["fraiseql", "status"]);
    }

    #[test]
    fn status_reads_the_running_phrase() {
        let status = parse_status(&captured(0, "fraiseql is running as pid 4321."));
        assert!(status.running);
        assert_eq!(
            status.detail.as_deref(),
            Some("fraiseql is running as pid 4321.")
        );
    }

    #[test]
    fn status_reads_the_not_running_phrase_despite_exit_zero() {
        // The phrase wins over a misleading exit code.
        let status = parse_status(&captured(0, "fraiseql is not running."));
        assert!(!status.running);
    }

    #[test]
    fn status_falls_back_to_the_exit_code_without_a_phrase() {
        assert!(parse_status(&captured(0, "")).running);
        assert!(!parse_status(&captured(1, "")).running);
    }
}
