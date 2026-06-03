//! # fraisier-adapter-systemd
//!
//! The [`SystemdService`] adapter: a [`ServiceAdapter`] that drives a systemd
//! unit through the `systemctl` CLI (PRD §3.3 — shell out in v1.0; D-Bus is
//! v1.1+).
//!
//! ## Configuration
//!
//! Read per call from [`AdapterCtx::settings`] (the `[service]` table):
//!
//! ```toml
//! [service]
//! adapter = "systemd"
//! unit = "fraiseql.service"
//! user = false              # optional: systemctl --user
//! ```
//!
//! ## Locality
//!
//! Phase 1 runs `systemctl` on the **local** host; the `host` argument is
//! reserved for the Phase 3+ SSH dispatch layer, which will run the same
//! commands on a remote host. The adapter never assumes privilege — escalation
//! (sudo, polkit) is the operator's concern.

use std::ffi::OsString;

use async_trait::async_trait;
use fraisier_adapter_support::{error, run_command, Captured};
use fraisier_core::adapter_axes::{
    AdapterCtx, AdapterError, AdapterErrorKind, HostId, ServiceAdapter, ServiceStatus,
};
use serde_json::Value;

/// The adapter's identity name.
const ADAPTER_NAME: &str = "systemd";

/// The env var that overrides which `systemctl` binary the adapter spawns.
const PROGRAM_ENV: &str = "FRAISIER_SYSTEMCTL_BIN";

/// The `systemctl is-active` token that means the unit is running.
const ACTIVE: &str = "active";

/// A [`ServiceAdapter`] backed by `systemctl`.
///
/// # Example
/// ```
/// use fraisier_adapter_systemd::SystemdService;
///
/// let adapter = SystemdService::new();
/// let pinned = SystemdService::with_program("/usr/bin/systemctl");
/// let _ = (adapter, pinned);
/// ```
pub struct SystemdService {
    program: OsString,
}

impl Default for SystemdService {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemdService {
    /// Create an adapter that spawns `systemctl` (honouring the
    /// `FRAISIER_SYSTEMCTL_BIN` override).
    #[must_use]
    pub fn new() -> Self {
        let program = std::env::var_os(PROGRAM_ENV)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| OsString::from("systemctl"));
        Self { program }
    }

    /// Create an adapter that spawns the binary at `program`.
    #[must_use]
    pub fn with_program(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// Build the argv for a `systemctl` `verb` against the configured unit:
    /// `[--user] <verb> <unit>`.
    fn args_for(
        ctx: &AdapterCtx,
        verb: &str,
        operation: &str,
    ) -> Result<Vec<OsString>, AdapterError> {
        let unit = ctx
            .settings
            .get("unit")
            .and_then(Value::as_str)
            .filter(|unit| !unit.is_empty())
            .ok_or_else(|| {
                error(
                    AdapterErrorKind::InvalidConfig,
                    ADAPTER_NAME,
                    operation,
                    "no 'unit' configured in [service] settings".to_owned(),
                    None,
                )
            })?;
        let mut args = Vec::new();
        if ctx.settings.get("user").and_then(Value::as_bool) == Some(true) {
            args.push(OsString::from("--user"));
        }
        args.push(OsString::from(verb));
        args.push(OsString::from(unit));
        Ok(args)
    }

    /// Run a `systemctl` `verb`, returning the captured output.
    async fn systemctl(
        &self,
        ctx: &AdapterCtx,
        verb: &str,
        operation: &str,
    ) -> Result<Captured, AdapterError> {
        let args = Self::args_for(ctx, verb, operation)?;
        run_command(&self.program, &args, &[], None, ADAPTER_NAME, operation).await
    }
}

#[async_trait]
impl ServiceAdapter for SystemdService {
    async fn restart(&self, ctx: &AdapterCtx, _host: &HostId) -> Result<(), AdapterError> {
        // Clear any leftover failed state and the start rate-limit counter first,
        // so the restart is not refused as "start request repeated too quickly".
        // This matters most for a rollback restart that follows a just-failed
        // start (systemd's default StartLimitBurst is per-unit). Best-effort: a
        // reset-failed error (e.g. unit not loaded) must not block the restart.
        let _ = self.systemctl(ctx, "reset-failed", "restart").await;
        let captured = self.systemctl(ctx, "restart", "restart").await?;
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
            format!("`systemctl restart` exited with {code}"),
            captured.stderr_opt(),
        ))
    }

    async fn status(
        &self,
        ctx: &AdapterCtx,
        _host: &HostId,
    ) -> Result<ServiceStatus, AdapterError> {
        // `systemctl is-active` exits non-zero for an inactive unit, so the exit
        // code is informational, not an error — only a spawn failure errors.
        let captured = self.systemctl(ctx, "is-active", "status").await?;
        let state = captured.stdout.trim();
        Ok(ServiceStatus {
            running: state == ACTIVE,
            detail: (!state.is_empty()).then(|| state.to_owned()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::SystemdService;
    use fraisier_core::adapter_axes::AdapterCtx;
    use serde_json::json;

    #[test]
    fn args_require_a_unit() {
        let ctx = AdapterCtx::new("checkout", "production");
        let err = SystemdService::args_for(&ctx, "restart", "restart")
            .expect_err("missing unit must fail");
        assert_eq!(err.adapter.as_deref(), Some("systemd"));
    }

    #[test]
    fn args_include_unit_and_optional_user() {
        let mut ctx = AdapterCtx::new("checkout", "production");
        ctx.settings
            .insert("unit".to_owned(), json!("fraiseql.service"));
        let args = SystemdService::args_for(&ctx, "is-active", "status").expect("args");
        assert_eq!(args, vec!["is-active", "fraiseql.service"]);

        ctx.settings.insert("user".to_owned(), json!(true));
        let args = SystemdService::args_for(&ctx, "restart", "restart").expect("args");
        assert_eq!(args, vec!["--user", "restart", "fraiseql.service"]);
    }
}
