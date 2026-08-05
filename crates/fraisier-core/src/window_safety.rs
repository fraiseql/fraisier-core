//! Blue-green **window-safety** gate — superseded by [`crate::policy`].
//!
//! The rule itself now lives in `policy`, as the
//! [`Baseline::WindowSafety`](crate::policy::Baseline::WindowSafety) verdict of
//! the one decision function; the *why* — the shared-DB hold window, the typed
//! verdict, the confiture#154 contract — is documented there. This module is a
//! thin adapter over that rule, kept only until the blue-green flow calls the
//! policy gate directly, and it delegates rather than keeping a second copy.

use crate::adapter_axes::{AdapterCtx, MigrationAdapter, PreflightReport};
use crate::policy::{baseline_verdict, Baseline, Capabilities};

/// The verdict of the window-safety gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowSafety {
    /// The migration is certified forward-compatible for a two-version window;
    /// blue-green may proceed.
    Safe,
    /// Blue-green is **refused** — with the reason. No traffic or instance change
    /// must occur.
    Refused(String),
}

impl WindowSafety {
    /// Whether the window is certified safe.
    #[must_use]
    pub const fn is_safe(&self) -> bool {
        matches!(self, Self::Safe)
    }

    /// The refusal reason, if refused.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Refused(reason) => Some(reason),
            Self::Safe => None,
        }
    }
}

/// Apply the window-safety policy to already-gathered inputs (pure).
///
/// `has_preflight` is whether the adapter advertises the capability; `report` is
/// the preflight result (`None` when preflight wasn't run).
///
/// Delegates to [`policy::baseline_verdict`](crate::policy::baseline_verdict) —
/// the rule has exactly one implementation, and it is the one the policy gate
/// applies.
#[must_use]
pub fn evaluate(has_preflight: bool, report: Option<&PreflightReport>) -> WindowSafety {
    // The window rule reads only `preflight`; `risk_tier` is the tier policy's
    // input and has no bearing on certifying the window.
    let capabilities = Capabilities::new(has_preflight, false);
    baseline_verdict(Baseline::WindowSafety, capabilities, report)
        .map_or(WindowSafety::Safe, WindowSafety::Refused)
}

/// Run the adapter's `preflight` (if advertised) and apply the [`evaluate`] policy.
///
/// This is the call the blue-green flow makes **before** any instance or traffic
/// change; on [`WindowSafety::Refused`] the deploy is hard-blocked.
pub async fn check(migration: &dyn MigrationAdapter, ctx: &AdapterCtx) -> WindowSafety {
    let described = match migration.describe().await {
        Ok(described) => described,
        Err(error) => {
            return WindowSafety::Refused(format!(
                "could not describe the migration adapter to certify the window: {error}"
            ))
        }
    };
    let has_preflight = described.capabilities.iter().any(|c| c == "preflight");
    if !has_preflight {
        return evaluate(false, None);
    }
    let report = match migration.preflight(ctx).await {
        Ok(report) => report,
        Err(error) => {
            return WindowSafety::Refused(format!(
                "preflight failed; cannot certify the window: {error}"
            ))
        }
    };
    evaluate(true, Some(&report))
}

#[cfg(test)]
mod tests {
    use super::{check, evaluate, WindowSafety};
    use crate::adapter_axes::{
        AdapterCtx, AdapterDescription, AdapterError, MigrationAdapter, MigrationOutcome,
        PreflightReport, Revision, VerifyReport,
    };

    /// A migration adapter with a configurable capability set + preflight report.
    struct FakeMigration {
        capabilities: Vec<String>,
        report: Option<PreflightReport>,
    }

    impl FakeMigration {
        fn with_preflight(report: PreflightReport) -> Self {
            Self {
                capabilities: vec!["up".to_owned(), "preflight".to_owned()],
                report: Some(report),
            }
        }
        fn without_preflight() -> Self {
            Self {
                capabilities: vec!["up".to_owned()],
                report: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl MigrationAdapter for FakeMigration {
        async fn describe(&self) -> Result<AdapterDescription, AdapterError> {
            Ok(AdapterDescription {
                name: "fake".to_owned(),
                version: "0.0.0".to_owned(),
                protocol_version: 1,
                capabilities: self.capabilities.clone(),
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
        async fn preflight(&self, _ctx: &AdapterCtx) -> Result<PreflightReport, AdapterError> {
            self.report
                .clone()
                .ok_or_else(|| AdapterError::method_not_supported("preflight"))
        }
    }

    fn ctx() -> AdapterCtx {
        AdapterCtx::new("checkout", "production")
    }

    fn report(window_safe: Option<bool>, ok: bool) -> PreflightReport {
        PreflightReport {
            ok,
            window_safe,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn window_safe_true_is_allowed() {
        let adapter = FakeMigration::with_preflight(report(Some(true), true));
        assert_eq!(check(&adapter, &ctx()).await, WindowSafety::Safe);
    }

    #[tokio::test]
    async fn window_safe_false_is_refused() {
        let adapter = FakeMigration::with_preflight(report(Some(false), true));
        let verdict = check(&adapter, &ctx()).await;
        assert!(matches!(verdict, WindowSafety::Refused(_)), "{verdict:?}");
        assert!(verdict.reason().unwrap().contains("window_safe = false"));
    }

    #[tokio::test]
    async fn no_window_safe_verdict_is_refused() {
        // An older confiture that doesn't emit window_safe → can't certify → refuse.
        let adapter = FakeMigration::with_preflight(report(None, true));
        let verdict = check(&adapter, &ctx()).await;
        assert!(matches!(verdict, WindowSafety::Refused(_)), "{verdict:?}");
        assert!(verdict.reason().unwrap().contains("window_safe"));
    }

    #[tokio::test]
    async fn an_adapter_without_preflight_is_refused() {
        let adapter = FakeMigration::without_preflight();
        let verdict = check(&adapter, &ctx()).await;
        assert!(matches!(verdict, WindowSafety::Refused(_)), "{verdict:?}");
        assert!(verdict
            .reason()
            .unwrap()
            .contains("does not advertise `preflight`"));
    }

    #[test]
    fn window_safe_is_authoritative_over_report_ok() {
        // A non-transactional but forward-compatible migration (e.g. CREATE INDEX
        // CONCURRENTLY) may carry `ok == false` yet `window_safe == true`. The gate
        // trusts the verdict: transactionality is not a window-safety concern
        // (blue-green does no DB rollback), so it is allowed. The converse —
        // `window_safe == false` — is refused regardless of `ok`.
        assert_eq!(
            evaluate(true, Some(&report(Some(true), false))),
            WindowSafety::Safe
        );
        assert!(matches!(
            evaluate(true, Some(&report(Some(false), true))),
            WindowSafety::Refused(_)
        ));
    }
}
