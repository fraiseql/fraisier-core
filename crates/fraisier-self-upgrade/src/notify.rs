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
