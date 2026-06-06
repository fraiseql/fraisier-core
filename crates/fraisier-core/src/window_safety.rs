//! Blue-green **window-safety** gate — the headline failure path of phase-07.
//!
//! A blue-green swap runs version N-1 and N against **one shared Postgres** for
//! the hold window, so the swap is only allowed when the pending migration is
//! certified *forward-compatible for a two-version window*. fraisier consumes
//! confiture's verdict; it never authors or validates expand/contract logic.
//!
//! ## Two paths to the verdict
//!
//! When the adapter supplies a **first-class** [`PreflightReport::window_safe`]
//! boolean (confiture's `window_safe` field), it is **authoritative**:
//! `Some(false)` blocks, `Some(true)` passes. When it is `None` (older confiture,
//! no typed verdict) the gate falls back to the `PFLIGHT_REPLICA_*` **issue-code
//! presence** rule below. Both fail safe; reversibility (`report.ok`) and the
//! can't-see (`.py`) refusal apply on either path.
//!
//! The presence-rule policy (a **hard block before any instance or traffic
//! change**), grounded in reading confiture 0.22's replica lint, not in assumptions:
//!
//! 1. **Can't-certify ⇒ refuse.** If the adapter does not advertise `preflight`,
//!    nothing can certify the window — refuse (mirrors the
//!    `MethodNotSupported`-never-a-pass design).
//! 2. **Can't-*see* ⇒ refuse.** confiture's replica classifier globs `*.up.sql`
//!    only; a `DROP COLUMN` in a `.py` migration emits **no** finding (the file
//!    is never opened). So "no replica issue" can mean "window-safe" *or* "never
//!    inspected" — refuse if the set contains any file the classifier can't read.
//! 3. **Any forward-compat finding ⇒ refuse.** The replica lint is
//!    warn-by-default (error only when the project declares replicas), so
//!    `report.ok` is `true` for a `DROP COLUMN` on a project with no declared
//!    replicas — `ok` alone cannot certify. Treat **any** `PFLIGHT_REPLICA_*`
//!    issue, warning *or* error, as a hard blocker.
//! 4. **`!report.ok` ⇒ refuse.** Covers reversibility/transactionality errors —
//!    blue-green's down path needs the reverse migration too.
//!
//! There is **no force-equivalent** (a silent override would re-introduce the
//! exact shared-DB-corruption footgun this gate exists to prevent).
//!
//! ## Cross-repo precondition (S2 — tracked in confiture#154)
//!
//! The fallback path keys on the `PFLIGHT_REPLICA_*` code **prefix**, which
//! confiture 0.22 emits but whose *values* its contract test originally did
//! **not** pin — a rename would silently make this gate match nothing. Tracked in
//! [fraiseql/confiture#154], resolved two ways (either suffices):
//! 1. confiture **pins** the `PFLIGHT_REPLICA_*` namespace in its contract test
//!    (Phase 1) — makes the prefix fallback above a guaranteed contract;
//! 2. the **first-class `window_safe` verdict** (Phase 3): consumed here via
//!    [`PreflightReport::window_safe`] — when present it is authoritative and the
//!    prefix matching is bypassed entirely, decoupling fraisier from the codes.
//!
//! [fraiseql/confiture#154]: https://github.com/fraiseql/confiture/issues/154

use crate::adapter_axes::{AdapterCtx, MigrationAdapter, PreflightReport};

/// The stable code **prefix** confiture uses for replica-aware forward-compat
/// findings (`PFLIGHT_REPLICA_DROP_COLUMN`, `…_RENAME_COLUMN`, …). Safe ops emit
/// no finding, so *any* issue with this prefix means a non-window-safe op.
const REPLICA_CODE_PREFIX: &str = "PFLIGHT_REPLICA_";

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
/// `has_preflight` is whether the adapter advertises the capability;
/// `migration_files` are the pending migration basenames (for the can't-see
/// check); `report` is the preflight result (`None` when preflight wasn't run).
#[must_use]
pub fn evaluate(
    has_preflight: bool,
    migration_files: &[String],
    report: Option<&PreflightReport>,
) -> WindowSafety {
    // (1) can't-certify.
    if !has_preflight {
        return WindowSafety::Refused(
            "the migration adapter does not advertise `preflight`; nothing can certify the \
             migration window-safe for a two-version blue-green window"
                .to_owned(),
        );
    }
    // (2) can't-see: the SQL-only replica classifier never opens a .py migration,
    // so a clean report over a set containing one is not a certificate.
    if let Some(file) = migration_files.iter().find(|f| is_unclassifiable(f)) {
        return WindowSafety::Refused(format!(
            "the migration set contains '{file}', which confiture's replica \
             forward-compat classifier cannot read (SQL-only); a clean report over it is not a \
             window-safety certificate"
        ));
    }
    let Some(report) = report else {
        return WindowSafety::Refused(
            "no preflight report was produced; cannot certify the window".to_owned(),
        );
    };
    // (3) reversibility/transactionality errors (the down path), always checked.
    if !report.ok {
        return WindowSafety::Refused(
            "preflight reported blocking issues (report.ok == false): the down path / \
             transactionality is not safe for a two-version window"
                .to_owned(),
        );
    }
    // (4) the forward-compat verdict. Prefer the adapter's **first-class**
    // `window_safe` boolean when it provides one; otherwise fall back to the
    // `PFLIGHT_REPLICA_*` issue-code presence rule (older confiture, no typed
    // verdict). Both fail safe.
    match report.window_safe {
        Some(true) => WindowSafety::Safe,
        Some(false) => WindowSafety::Refused(
            "the migration adapter reports the migration is NOT forward-compatible for a \
             two-version window (window_safe = false)"
                .to_owned(),
        ),
        None => report
            .issues
            .iter()
            .find(|issue| issue.code.starts_with(REPLICA_CODE_PREFIX))
            .map_or(WindowSafety::Safe, |issue| {
                WindowSafety::Refused(format!(
                    "migration is not forward-compatible for a two-version window: {} ({:?}) — {}",
                    issue.code, issue.severity, issue.message
                ))
            }),
    }
}

/// Run the adapter's `preflight` (if advertised) and apply the [`evaluate`] policy.
///
/// This is the call the blue-green flow makes **before** any instance or traffic
/// change; on [`WindowSafety::Refused`] the deploy is hard-blocked.
pub async fn check(
    migration: &dyn MigrationAdapter,
    ctx: &AdapterCtx,
    migration_files: &[String],
) -> WindowSafety {
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
        return evaluate(false, migration_files, None);
    }
    let report = match migration.preflight(ctx).await {
        Ok(report) => report,
        Err(error) => {
            return WindowSafety::Refused(format!(
                "preflight failed; cannot certify the window: {error}"
            ))
        }
    };
    evaluate(true, migration_files, Some(&report))
}

/// Whether `file` is a migration the SQL-only replica classifier cannot read —
/// concretely, a Python (`.py`) migration confiture's resolver runs but the
/// replica lint never opens.
fn is_unclassifiable(file: &str) -> bool {
    std::path::Path::new(file)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("py"))
}

#[cfg(test)]
mod tests {
    use super::{check, evaluate, WindowSafety};
    use crate::adapter_axes::{
        AdapterCtx, AdapterDescription, AdapterError, MigrationAdapter, MigrationOutcome,
        PreflightIssue, PreflightReport, Revision, Severity, VerifyReport,
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

    fn sql_set() -> Vec<String> {
        vec![
            "001_init.up.sql".to_owned(),
            "002_add_col.up.sql".to_owned(),
        ]
    }

    fn replica_warning() -> PreflightReport {
        // ok == true (no declared replicas → warn-by-default), but a forward-compat
        // finding is present: the trap `ok` alone cannot catch.
        PreflightReport {
            ok: true,
            issues: vec![PreflightIssue {
                severity: Severity::Warning,
                code: "PFLIGHT_REPLICA_DROP_COLUMN".to_owned(),
                message: "DROP COLUMN is not forward-compatible".to_owned(),
                migration: Some("003".to_owned()),
            }],
            window_safe: None,
        }
    }

    #[tokio::test]
    async fn a_replica_warning_refuses_even_though_ok_is_true() {
        let adapter = FakeMigration::with_preflight(replica_warning());
        let verdict = check(&adapter, &ctx(), &sql_set()).await;
        assert!(matches!(verdict, WindowSafety::Refused(_)), "{verdict:?}");
        assert!(verdict
            .reason()
            .unwrap()
            .contains("PFLIGHT_REPLICA_DROP_COLUMN"));
    }

    #[tokio::test]
    async fn an_adapter_without_preflight_is_refused() {
        let adapter = FakeMigration::without_preflight();
        let verdict = check(&adapter, &ctx(), &sql_set()).await;
        assert!(matches!(verdict, WindowSafety::Refused(_)), "{verdict:?}");
        assert!(verdict
            .reason()
            .unwrap()
            .contains("does not advertise `preflight`"));
    }

    #[tokio::test]
    async fn a_py_migration_in_the_set_is_refused_even_with_a_clean_report() {
        // A perfectly clean report — but the set has a .py the classifier never read.
        let clean = PreflightReport {
            ok: true,
            issues: Vec::new(),
            window_safe: None,
        };
        let adapter = FakeMigration::with_preflight(clean);
        let mut files = sql_set();
        files.push("003_backfill.py".to_owned());
        let verdict = check(&adapter, &ctx(), &files).await;
        assert!(matches!(verdict, WindowSafety::Refused(_)), "{verdict:?}");
        assert!(verdict.reason().unwrap().contains("003_backfill.py"));
    }

    #[tokio::test]
    async fn a_clean_all_sql_expand_report_is_allowed() {
        let clean = PreflightReport {
            ok: true,
            issues: Vec::new(),
            window_safe: None,
        };
        let adapter = FakeMigration::with_preflight(clean);
        let verdict = check(&adapter, &ctx(), &sql_set()).await;
        assert_eq!(
            verdict,
            WindowSafety::Safe,
            "clean all-SQL expand is window-safe"
        );
    }

    #[test]
    fn evaluate_refuses_a_non_ok_report() {
        // Reversibility/transactionality error → !ok → refused.
        let report = PreflightReport {
            ok: false,
            window_safe: None,
            issues: vec![PreflightIssue {
                severity: Severity::Error,
                code: "missing_down".to_owned(),
                message: "no reverse migration".to_owned(),
                migration: Some("004".to_owned()),
            }],
        };
        let verdict = evaluate(true, &sql_set(), Some(&report));
        assert!(matches!(verdict, WindowSafety::Refused(_)), "{verdict:?}");
    }

    #[tokio::test]
    async fn a_first_class_window_safe_false_refuses_with_no_issue_codes() {
        // The typed verdict blocks on its own — no PFLIGHT_REPLICA_* needed.
        let report = PreflightReport {
            ok: true,
            issues: Vec::new(),
            window_safe: Some(false),
        };
        let adapter = FakeMigration::with_preflight(report);
        let verdict = check(&adapter, &ctx(), &sql_set()).await;
        assert!(matches!(verdict, WindowSafety::Refused(_)), "{verdict:?}");
        assert!(verdict.reason().unwrap().contains("window_safe = false"));
    }

    #[tokio::test]
    async fn a_first_class_window_safe_true_is_allowed() {
        let report = PreflightReport {
            ok: true,
            issues: Vec::new(),
            window_safe: Some(true),
        };
        let adapter = FakeMigration::with_preflight(report);
        assert_eq!(
            check(&adapter, &ctx(), &sql_set()).await,
            WindowSafety::Safe
        );
    }

    #[test]
    fn the_typed_verdict_is_authoritative_over_the_prefix_rule() {
        // `window_safe = Some(true)` is trusted even if a PFLIGHT_REPLICA_* code is
        // present (confiture folded everything into the verdict); the prefix rule is
        // only the `None` fallback.
        let report = PreflightReport {
            ok: true,
            window_safe: Some(true),
            issues: vec![PreflightIssue {
                severity: Severity::Warning,
                code: "PFLIGHT_REPLICA_ADD_COLUMN".to_owned(),
                message: String::new(),
                migration: None,
            }],
        };
        assert_eq!(
            evaluate(true, &sql_set(), Some(&report)),
            WindowSafety::Safe
        );
    }
}
