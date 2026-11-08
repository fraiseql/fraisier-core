//! What a migration adapter's preflight report means for a forward migrate.
//!
//! Every forward migrate — the deploy strategies and `db migrate` — reads a
//! report the same two ways:
//!
//! - [`lint_refusal`]: an `Error`-severity finding blocks it. Findings are
//!   grouped by the migration they concern, so one broken migration that two
//!   checks flag counts once.
//! - [`reversibility_refusal`]: a pending migration with no `down` that undoes
//!   it blocks it unless the operator set `[migration].allow_irreversible`. A
//!   rollback past such a migration rewinds the ledger and leaves the schema.
//!
//! A deliberate rollback reads neither: the report describes the pending
//! forward changes, which a rollback does not apply.

use crate::adapter_axes::{PreflightIssue, PreflightReport, Reversibility, Severity};

/// The `Error`-severity findings about one migration, or one finding that
/// names none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockingMigration {
    /// The migration, when the findings name one.
    pub migration: Option<String>,
    /// Each finding as `CODE: message`, in report order.
    pub issues: Vec<String>,
}

/// The report's `Error`-severity findings, grouped by migration in the order
/// each migration first appears. A finding that names no migration is a group
/// of its own.
#[must_use]
pub fn blocking(report: &PreflightReport) -> Vec<BlockingMigration> {
    let mut groups: Vec<BlockingMigration> = Vec::new();
    for issue in report
        .issues
        .iter()
        .filter(|issue| issue.severity == Severity::Error)
    {
        let line = describe(issue);
        let existing = issue.migration.as_ref().and_then(|migration| {
            groups
                .iter_mut()
                .find(|group| group.migration.as_ref() == Some(migration))
        });
        match existing {
            Some(group) => group.issues.push(line),
            None => groups.push(BlockingMigration {
                migration: issue.migration.clone(),
                issues: vec![line],
            }),
        }
    }
    groups
}

fn describe(issue: &PreflightIssue) -> String {
    format!("{}: {}", issue.code, issue.message)
}

impl BlockingMigration {
    /// The migration, then each finding: `003: CODE: message; CODE: message`.
    #[must_use]
    pub fn line(&self) -> String {
        let issues = self.issues.join("; ");
        self.migration.as_ref().map_or_else(
            || issues.clone(),
            |migration| format!("{migration}: {issues}"),
        )
    }
}

/// How many migrations fail and how many findings name none, for a headline:
/// `2 pending migration(s) would fail, 1 other blocking issue(s)`.
#[must_use]
pub fn counts(groups: &[BlockingMigration]) -> String {
    let migrations = groups.iter().filter(|g| g.migration.is_some()).count();
    let others = groups.len() - migrations;
    let mut counts = Vec::new();
    if migrations > 0 {
        counts.push(format!("{migrations} pending migration(s) would fail"));
    }
    if others > 0 {
        counts.push(format!("{others} other blocking issue(s)"));
    }
    counts.join(", ")
}

/// Why the forward-compatibility lint refuses this report, or `None` when no
/// finding blocks it.
#[must_use]
pub fn lint_refusal(report: &PreflightReport) -> Option<String> {
    let groups = blocking(report);
    if groups.is_empty() {
        return None;
    }
    let detail: Vec<String> = groups.iter().map(BlockingMigration::line).collect();
    Some(format!(
        "forward-compatibility preflight: {} — {}",
        counts(&groups),
        detail.join(" | ")
    ))
}

/// Why this report's pending migrations may not be applied, or `None` when they
/// may.
///
/// `None` from the adapter (it offers no reversibility check, like the command
/// and IPC adapters) passes: fraisier refuses only on what an adapter said.
#[must_use]
pub fn reversibility_refusal(
    report: Option<&PreflightReport>,
    allow_irreversible: bool,
) -> Option<String> {
    if allow_irreversible {
        return None;
    }
    match report?.reversibility.as_ref()? {
        Reversibility::Checked { irreversible } if irreversible.is_empty() => None,
        Reversibility::Checked { irreversible } => Some(format!(
            "pending migration(s) {} have no down that undoes them, so a rollback past them \
             would rewind the ledger and leave the schema; write a down, or set \
             [migration].allow_irreversible = true to apply them anyway",
            irreversible.join(", ")
        )),
        Reversibility::Unchecked { reason } => Some(format!(
            "cannot tell whether the pending migrations can be undone: {reason}; upgrade it, or \
             set [migration].allow_irreversible = true to apply them anyway"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{blocking, lint_refusal, reversibility_refusal};
    use crate::adapter_axes::{PreflightIssue, PreflightReport, Reversibility, Severity};

    fn issue(severity: Severity, code: &str, migration: Option<&str>) -> PreflightIssue {
        PreflightIssue {
            severity,
            code: code.to_owned(),
            message: format!("{code} happened"),
            migration: migration.map(str::to_owned),
        }
    }

    fn report(issues: Vec<PreflightIssue>) -> PreflightReport {
        PreflightReport::new(false).with_issues(issues)
    }

    #[test]
    fn two_findings_on_one_migration_count_as_one() {
        let report = report(vec![
            issue(Severity::Error, "PFLIGHT_MISSING_DOWN", Some("003")),
            issue(
                Severity::Warning,
                "PFLIGHT_REPLICA_CHANGE_TYPE",
                Some("003"),
            ),
            issue(Severity::Error, "PFLIGHT_REPLAY_FAILED", Some("003")),
        ]);
        let groups = blocking(&report);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].issues.len(), 2);
        let message = lint_refusal(&report).expect("refused");
        assert!(
            message.contains("1 pending migration(s) would fail"),
            "{message}"
        );
        assert!(
            message.contains(
                "003: PFLIGHT_MISSING_DOWN: PFLIGHT_MISSING_DOWN happened; PFLIGHT_REPLAY_FAILED"
            ),
            "{message}"
        );
    }

    #[test]
    fn a_finding_without_a_migration_stays_its_own() {
        let report = report(vec![
            issue(Severity::Error, "A", None),
            issue(Severity::Error, "B", None),
            issue(Severity::Error, "C", Some("004")),
        ]);
        assert_eq!(blocking(&report).len(), 3);
        let message = lint_refusal(&report).expect("refused");
        assert!(
            message.contains("1 pending migration(s) would fail, 2 other blocking issue(s)"),
            "{message}"
        );
    }

    #[test]
    fn warnings_alone_do_not_refuse() {
        assert_eq!(
            lint_refusal(&report(vec![issue(Severity::Warning, "W", Some("1"))])),
            None
        );
    }

    fn reversibility(r: Reversibility) -> PreflightReport {
        PreflightReport::new(true).with_reversibility(r)
    }

    #[test]
    fn an_irreversible_migration_is_refused_by_name_with_the_setting() {
        let report = reversibility(Reversibility::Checked {
            irreversible: vec!["005_add_index".to_owned()],
        });
        let message = reversibility_refusal(Some(&report), false).expect("refused");
        assert!(message.contains("005_add_index"), "{message}");
        assert!(
            message.contains("[migration].allow_irreversible = true"),
            "{message}"
        );
        assert_eq!(reversibility_refusal(Some(&report), true), None);
    }

    #[test]
    fn an_unchecked_report_is_refused_with_its_reason_and_the_setting() {
        let report = reversibility(Reversibility::Unchecked {
            reason: "confiture 1.36.0 is older than 1.40.0".to_owned(),
        });
        let message = reversibility_refusal(Some(&report), false).expect("refused");
        assert!(message.contains("1.36.0 is older than 1.40.0"), "{message}");
        assert!(
            message.contains("[migration].allow_irreversible = true"),
            "{message}"
        );
        assert_eq!(reversibility_refusal(Some(&report), true), None);
    }

    #[test]
    fn no_verdict_and_an_empty_list_pass() {
        assert_eq!(reversibility_refusal(None, false), None);
        assert_eq!(
            reversibility_refusal(Some(&PreflightReport::new(true)), false),
            None
        );
        let clean = reversibility(Reversibility::Checked {
            irreversible: vec![],
        });
        assert_eq!(reversibility_refusal(Some(&clean), false), None);
    }
}
