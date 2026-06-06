//! The failure-notification primitive shared by the unattended flows.
//!
//! When an unattended action fails in a way an operator would want to know about
//! — `self-upgrade apply` reverting or hitting a manual-intervention state, and
//! (Phase 6.2) a scheduled deploy rolling back — fraisier fires a [`Notifier`].
//! The contract is deliberately generic: a structured [`FailurePayload`], not an
//! SMTP/email client (out of scope). The concrete sink is an exec-hook plus an
//! OpenTelemetry span event.

use async_trait::async_trait;
use serde::Serialize;

/// A structured description of an unattended failure, handed to a [`Notifier`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FailurePayload {
    /// A stable machine event name (e.g. `self-upgrade-reverted`).
    pub event: String,
    /// The artifact/binary id that failed, when applicable.
    pub failed: Option<String>,
    /// The artifact/binary id service was restored to, when a revert succeeded.
    pub restored: Option<String>,
    /// A human-readable reason.
    pub reason: String,
}

/// A sink for [`FailurePayload`]s. Implementations must not panic; a failed
/// notification must never mask the failure being reported.
#[async_trait]
pub trait Notifier: Send + Sync {
    /// Deliver a failure notification (best-effort).
    async fn notify(&self, payload: &FailurePayload);
}

/// A [`Notifier`] that does nothing — the default when no sink is configured.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopNotifier;

#[async_trait]
impl Notifier for NoopNotifier {
    async fn notify(&self, _payload: &FailurePayload) {}
}

/// The concrete failure sink: an operator-supplied exec hook plus an OTel event.
///
/// The hook is run via `sh -c <command>` with the payload as JSON on **stdin**
/// and each field exported as an **argv-safe environment variable**
/// (`FRAISIER_NOTIFY_EVENT`, `_FAILED`, `_RESTORED`, `_REASON`) — never on argv,
/// so a payload value can't be injected as an argument. Delivery is best-effort:
/// a hook that fails to spawn or exits non-zero is logged, never propagated, so a
/// broken notifier never masks the failure it is reporting.
#[derive(Debug, Clone)]
pub struct ExecHookNotifier {
    command: String,
    context: Vec<(String, String)>,
}

impl ExecHookNotifier {
    /// A notifier that runs `command` (via `sh -c`) on each failure.
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            context: Vec::new(),
        }
    }

    /// Add a fixed environment variable exported to every hook invocation (e.g.
    /// `FRAISIER_NOTIFY_UNIT`, the deploy's fraise/environment).
    #[must_use]
    pub fn with_context(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.context.push((key.into(), value.into()));
        self
    }
}

#[async_trait]
impl Notifier for ExecHookNotifier {
    async fn notify(&self, payload: &FailurePayload) {
        // The OTel span event (also visible in logs without a collector).
        tracing::error!(
            event = %payload.event,
            failed = ?payload.failed,
            restored = ?payload.restored,
            reason = %payload.reason,
            "fraisier unattended-failure notification"
        );

        let json = serde_json::to_string(payload).unwrap_or_default();
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(&self.command)
            .env("FRAISIER_NOTIFY_EVENT", &payload.event)
            .env("FRAISIER_NOTIFY_REASON", &payload.reason)
            .stdin(std::process::Stdio::piped());
        if let Some(failed) = &payload.failed {
            command.env("FRAISIER_NOTIFY_FAILED", failed);
        }
        if let Some(restored) = &payload.restored {
            command.env("FRAISIER_NOTIFY_RESTORED", restored);
        }
        for (key, value) in &self.context {
            command.env(key, value);
        }

        match command.spawn() {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    use tokio::io::AsyncWriteExt as _;
                    let _ = stdin.write_all(json.as_bytes()).await;
                    drop(stdin); // close stdin so the hook sees EOF
                }
                match child.wait().await {
                    Ok(status) if status.success() => {}
                    Ok(status) => tracing::warn!("notify hook exited {status}"),
                    Err(error) => tracing::warn!("notify hook wait failed: {error}"),
                }
            }
            Err(error) => tracing::warn!("notify hook failed to spawn: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ExecHookNotifier, FailurePayload, Notifier};

    fn payload() -> FailurePayload {
        FailurePayload {
            event: "self-upgrade-reverted".to_owned(),
            failed: Some("2.0.0".to_owned()),
            restored: Some("1.0.0".to_owned()),
            reason: "boots-then-dies".to_owned(),
        }
    }

    #[tokio::test]
    async fn the_hook_receives_the_event_via_env_and_the_payload_via_stdin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("captured");
        // Write the env var, then append everything on stdin (the JSON payload).
        let command = format!(
            "printf 'EVENT=%s\\n' \"$FRAISIER_NOTIFY_EVENT\" > {out}; cat >> {out}",
            out = out.display()
        );
        ExecHookNotifier::new(command).notify(&payload()).await;

        let captured = std::fs::read_to_string(&out).expect("hook wrote output");
        assert!(
            captured.contains("EVENT=self-upgrade-reverted"),
            "{captured}"
        );
        assert!(
            captured.contains("\"failed\":\"2.0.0\""),
            "stdin json: {captured}"
        );
        assert!(
            captured.contains("\"reason\":\"boots-then-dies\""),
            "{captured}"
        );
    }

    #[tokio::test]
    async fn context_env_is_exported_to_the_hook() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("captured");
        let command = format!(
            "printf '%s' \"$FRAISIER_NOTIFY_UNIT\" > {out}",
            out = out.display()
        );
        ExecHookNotifier::new(command)
            .with_context("FRAISIER_NOTIFY_UNIT", "fraisier-webhook.service")
            .notify(&payload())
            .await;
        assert_eq!(
            std::fs::read_to_string(&out).expect("output"),
            "fraisier-webhook.service"
        );
    }

    #[tokio::test]
    async fn a_failing_hook_is_best_effort_and_never_panics() {
        // Exits non-zero; notify must return normally.
        ExecHookNotifier::new("exit 3").notify(&payload()).await;
    }
}
