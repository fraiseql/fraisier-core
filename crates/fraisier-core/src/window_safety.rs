//! Blue-green **window-safety** gate — the headline failure path of phase-07.
//!
//! A blue-green swap runs version N-1 and N against **one shared Postgres** for
//! the hold window, so the swap is only allowed when the pending migration is
//! certified *forward-compatible for a two-version window*. fraisier **consumes
//! confiture's verdict** — it never authors, validates, or reinvents
//! expand/contract logic, and it knows nothing of confiture's classifier
//! internals (DDL codes, SQL-vs-`.py`).
//!
//! ## The contract: one typed verdict
//!
//! The migration adapter returns a first-class
//! [`PreflightReport::window_safe`](crate::adapter_axes::PreflightReport::window_safe):
//! `Some(true)` iff **every** pending migration is forward-compatible for a
//! two-version window — confiture folds the relevant concerns into it
//! (replica-unsafe ops + migrations it cannot classify). It is purely about
//! forward-compatibility: transactionality / reversibility are **not** consulted,
//! because blue-green does no DB rollback (rollback is a traffic swap-back to
//! still-hot blue), so a non-transactional-but-forward-compatible op like
//! `CREATE INDEX CONCURRENTLY` is window-safe. The gate is a single boolean read:
//!
//! - `Some(true)` ⇒ **Safe**;
//! - `Some(false)` ⇒ **Refused** (hard block before any instance or traffic change);
//! - `None` ⇒ **Refused** — the adapter offers no window-safety verdict, so
//!   nothing can certify the window (mirrors the `MethodNotSupported`-never-a-pass
//!   design). An adapter without the `preflight` capability is likewise refused.
//!
//! There is **no force-equivalent** (a silent override would re-introduce the
//! exact shared-DB-corruption footgun this gate exists to prevent) and **no
//! fallback to pattern-matching issue codes** — the typed verdict is the contract.
//!
//! ## Cross-repo contract (tracked in fraiseql/confiture#154)
//!
//! confiture emits `window_safe` on the `migrate preflight` JSON report and pins
//! it in its contract test. `window_safe == false` for **any** migration confiture
//! cannot certify — including ones its replica classifier cannot read (non-SQL /
//! `.py`) — so "can't see" can never masquerade as "safe". fraisier requires a
//! confiture release that emits the field; an older one returns `None` and is
//! refused (fail safe).
//!
//! [fraiseql/confiture#154]: https://github.com/fraiseql/confiture/issues/154

use crate::adapter_axes::{AdapterCtx, MigrationAdapter, PreflightReport};

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

/// Apply the window-safety policy to already-gathered inputs (pure — the heart of
/// the gate).
///
/// `has_preflight` is whether the adapter advertises the capability; `report` is
/// the preflight result (`None` when preflight wasn't run). The decision is the
/// adapter's first-class `window_safe` verdict; there is no fallback.
#[must_use]
pub fn evaluate(has_preflight: bool, report: Option<&PreflightReport>) -> WindowSafety {
    if !has_preflight {
        return WindowSafety::Refused(
            "the migration adapter does not advertise `preflight`; nothing can certify the \
             migration window-safe for a two-version blue-green window"
                .to_owned(),
        );
    }
    let Some(report) = report else {
        return WindowSafety::Refused(
            "no preflight report was produced; cannot certify the window".to_owned(),
        );
    };
    // `window_safe` is the SOLE window-safety verdict — purely forward-compatibility
    // for the two-version window. Transactionality / reversibility are deliberately
    // NOT consulted: blue-green does no DB rollback (rollback is a traffic swap-back
    // to still-hot blue on the expanded schema), so a non-transactional but
    // forward-compatible op like `CREATE INDEX CONCURRENTLY` is window-safe. A
    // genuinely broken migration still fails at the `migrate` step (before any
    // traffic moves), not here.
    match report.window_safe {
        Some(true) => WindowSafety::Safe,
        Some(false) => WindowSafety::Refused(
            "the migration is NOT forward-compatible for a two-version window \
             (confiture window_safe = false)"
                .to_owned(),
        ),
        None => WindowSafety::Refused(
            "the migration adapter returned no window-safety verdict (window_safe); blue-green \
             needs a confiture release that emits it — see fraiseql/confiture#154"
                .to_owned(),
        ),
    }
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
            issues: Vec::new(),
            window_safe,
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
