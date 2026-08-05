//! What `deploy --dry-run` says about the pending schema change.
//!
//! A plan that cannot see the schema is the gap issue #46 names, so `--dry-run`
//! inspects the migration adapter and renders two things the saga step list
//! never showed: **what would change**, and **what the policy gate would decide
//! about it**. The verdict comes from the same
//! [`policy::evaluate`](fraisier_core::policy::evaluate) the live gate calls —
//! never a parallel implementation — and the approval hook is structurally out
//! of reach, because only
//! [`PolicyGate::admit`](fraisier_core::policy::PolicyGate::admit) runs it.
//!
//! ## Unknown is not zero
//!
//! Everything here is built around one distinction: *the adapter looked and
//! there is nothing to change* is [`SchemaPreview::change_set`] `Some` with an
//! empty `changes`; *nobody classified this* is `None` with a
//! [`SchemaPreview::change_set_unavailable`] beside it. They serialize
//! differently, they render differently, and neither key is ever omitted — an
//! absent key and a `null` read the same to a sloppy consumer, and the reason
//! has to travel with the null.
//!
//! ## The cost of seeing
//!
//! Reaching a change-set means spawning the migration adapter and reading the
//! database, so a dry-run now does I/O where it did none (D8). Every way that
//! can fail — no DSN, unreachable database, an adapter that does not lint —
//! **degrades**: the plan still prints and still exits 0, carrying an explicit
//! unavailability. `--dry-run --skip-preflight` restores the pure offline plan
//! byte-for-byte.

use fraisier_core::adapter_axes::{
    AdapterCtx, AdapterDescription, ChangeSetUnavailable, MigrationAdapter, PreflightReport,
    RiskTier, SchemaChange,
};
use fraisier_core::policy::{
    self, Baseline, Capabilities, Inspection, PolicyDecision, PolicyGate, PolicyReason,
};
use serde::Serialize;

/// The schema half of a `--dry-run` plan.
///
/// Flattened into the strategies' plan summaries, so its keys sit beside the
/// resolved axis names rather than under a nested object — strictly additive for
/// an agent already parsing dry-run JSON.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SchemaPreview {
    /// The classified change-set. `Some` with an empty `changes` means the
    /// adapter looked and there is nothing to change.
    pub change_set: Option<ChangeSetPreview>,
    /// Why there is no change-set — set exactly when [`Self::change_set`] is
    /// `None`. Never omitted from the serialized form.
    pub change_set_unavailable: Option<Unavailable>,
    /// What the gate would decide. `None` when nothing would decide anything: no
    /// `[policy]` section and a strategy with no always-on baseline (D6).
    pub policy: Option<PolicyPreview>,
    /// The adapter's window-safety verdict. `None` when the strategy never asks
    /// — only blue-green holds two versions against one database.
    pub window_safe: Option<WindowSafety>,
}

/// The change-set as a plan renders it: which adapter classified it, at which
/// contract, and every change it listed.
///
/// The changes keep the adapter's own order (migration order). Sorting is the
/// renderer's job, not the payload's — an agent diffing two plans wants the
/// order the migrations will actually apply in.
#[derive(Debug, Clone, Serialize)]
pub struct ChangeSetPreview {
    /// The risk-contract revision the adapter wrote this payload to.
    pub contract_version: u32,
    /// The migration adapter's name, as it describes itself.
    pub adapter: String,
    /// Its version — the actionable half of *"which side do I upgrade"*.
    pub adapter_version: String,
    /// Every planned change, in migration order.
    pub changes: Vec<SchemaChange>,
}

/// Why a dry-run has no change-set to show.
///
/// [`code`](Self::code) is the stable machine name a pipeline branches on;
/// [`detail`](Self::detail) is the line an operator reads. The pair is what
/// keeps rule 3 honest — *unknown* is distinguishable from *empty* without
/// parsing prose.
#[derive(Debug, Clone, Serialize)]
pub struct Unavailable {
    /// A stable machine code. Rendered verbatim, never parsed for meaning.
    pub code: &'static str,
    /// One line for the operator, always ending in the reason it matters.
    pub detail: String,
}

impl Unavailable {
    /// The operator turned preflight off for this run (`--skip-preflight`).
    ///
    /// The one state the human render stays silent about: telling an operator
    /// the plan cannot see the schema is telling them what they just asked for,
    /// and rule 1 makes `--dry-run --skip-preflight` print today's plan
    /// byte-for-byte. The machine form still says it.
    pub const SKIPPED: &'static str = "preflight_skipped";
    /// The config turned every preflight off (`[migration].preflight_mode`).
    ///
    /// Distinct from [`SKIPPED`](Self::SKIPPED): nobody asked for it *on this
    /// run*, so the plan says so out loud.
    pub const PREFLIGHT_OFF: &'static str = "preflight_off";
    /// The migration adapter could not be built at all — an unset DSN env var,
    /// an adapter name this build does not know.
    pub const ADAPTER_UNAVAILABLE: &'static str = "adapter_unavailable";
    /// The adapter does not advertise `preflight`, so it has no lint to run.
    pub const NO_PREFLIGHT_CAPABILITY: &'static str = "no_preflight_capability";
    /// `describe` or `preflight` failed — an unreachable database, a crash.
    pub const PREFLIGHT_FAILED: &'static str = "preflight_failed";
    /// The adapter lints but does not classify.
    pub const NO_RISK_TIER_CAPABILITY: &'static str = "no_risk_tier_capability";
    /// It advertises `risk_tier` and emitted no change-set — an adapter bug.
    pub const NO_CHANGE_SET: &'static str = "no_change_set";
    /// The change-set is written to a contract this build cannot read.
    pub const UNREADABLE_CHANGE_SET: &'static str = "unreadable_change_set";

    /// State the code and the operator-facing detail.
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

/// What the policy gate **would** decide, computed without asking anyone.
#[derive(Debug, Clone, Serialize)]
pub struct PolicyPreview {
    /// The verdict.
    pub decision: Verdict,
    /// The most severe tier among the reasons, or `None` when nothing that
    /// triggered was classified.
    pub worst_tier: Option<RiskTier>,
    /// The refusal message, for a [`Verdict::Deny`] only.
    pub reason: Option<String>,
    /// Every change that is not auto-apply, in the order the adapter listed
    /// them.
    pub reasons: Vec<PolicyReason>,
    /// The configured `[policy].approval_command`, when one is configured —
    /// what an operator would have to satisfy to unblock this.
    pub approval_command: Option<String>,
}

/// A [`PolicyDecision`] flattened for the wire.
///
/// The gate's own enum is data-carrying, so it serializes externally tagged;
/// a plan wants one stable string an agent can switch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every planned change auto-applies.
    Allow,
    /// At least one change needs sign-off. A dry-run reports it and stops
    /// there — it never asks.
    NeedsApproval,
    /// Refused. No hook can unblock this.
    Deny,
}

/// The adapter's window-safety verdict, for the one strategy that needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowSafety {
    /// Every pending migration is forward-compatible for a two-version window.
    Safe,
    /// At least one is not — a hard block before any instance or traffic change.
    Unsafe,
    /// The adapter offered no verdict, so nothing can certify the window.
    Unknown,
}

impl WindowSafety {
    /// Read the adapter's verdict. No verdict is [`Unknown`](Self::Unknown), and
    /// unknown is not safe.
    const fn of(report: Option<&PreflightReport>) -> Self {
        match report {
            Some(PreflightReport {
                window_safe: Some(true),
                ..
            }) => Self::Safe,
            Some(PreflightReport {
                window_safe: Some(false),
                ..
            }) => Self::Unsafe,
            _ => Self::Unknown,
        }
    }
}

/// Inspect the migration adapter and work out what the deploy would decide.
///
/// One `describe`, one `preflight`, no saga, and — structurally — no approval
/// hook: this calls [`policy::evaluate`], and only
/// [`PolicyGate::admit`](fraisier_core::policy::PolicyGate::admit) can run a
/// hook. `baseline` is fixed by the deploy strategy exactly as it is on the live
/// path, so the preview and the deploy answer the same question.
///
/// Never fails. An adapter that cannot be inspected yields a plan with an
/// explicit unavailability, because a dry-run that cannot reach the database
/// still owes the operator the plan (rule 2, D8).
pub async fn gather(
    migration: &dyn MigrationAdapter,
    ctx: &AdapterCtx,
    gate: &PolicyGate,
    approval_command: Option<&str>,
    baseline: Baseline,
) -> SchemaPreview {
    let inspection = match policy::inspect(migration, ctx).await {
        Ok(inspection) => inspection,
        // "The check could not run" is never "the check passed". The verdict is
        // computed as though nothing had been inspected, which is exactly what
        // the deploy itself would conclude.
        Err(error) => {
            return assemble(
                Err(Unavailable::new(
                    Unavailable::PREFLIGHT_FAILED,
                    redact_credentials(&format!(
                        "the migration adapter could not be inspected: {error}"
                    )),
                )),
                gate,
                approval_command,
                baseline,
                Capabilities::default(),
                None,
            )
        }
    };
    let change_set = classify(&inspection);
    assemble(
        change_set,
        gate,
        approval_command,
        baseline,
        inspection.capabilities,
        inspection.report.as_ref(),
    )
}

/// The plan for a run that inspected nothing — preflight is off, or the adapter
/// could not be built at all.
///
/// The verdict is still previewed, from the same inputs the deploy itself would
/// reach the gate with: no capabilities and no report. A configured `[policy]`
/// refuses on exactly that basis, and a preview that stayed quiet about it would
/// print a plan that reads as clean over a deploy that will not run.
pub fn not_inspected(
    reason: Unavailable,
    gate: &PolicyGate,
    approval_command: Option<&str>,
    baseline: Baseline,
) -> SchemaPreview {
    assemble(
        Err(reason),
        gate,
        approval_command,
        baseline,
        Capabilities::default(),
        None,
    )
}

/// The change-set an inspection yielded, or why there is none.
///
/// Every way of not knowing gets its own code and names the adapter, because
/// *"upgrade confiture"* and *"your adapter has a bug"* are different jobs.
fn classify(inspection: &Inspection) -> Result<ChangeSetPreview, Unavailable> {
    let (name, version) = inspection.adapter.as_ref().map_or_else(
        || ("the migration adapter".to_owned(), String::new()),
        |adapter: &AdapterDescription| (adapter.name.clone(), adapter.version.clone()),
    );
    let named = format!("{name} {version}");
    let named = named.trim_end();
    let Some(report) = inspection.report.as_ref() else {
        return Err(Unavailable::new(
            Unavailable::NO_PREFLIGHT_CAPABILITY,
            format!(
                "{named} does not advertise `preflight`, so nothing inspected the pending schema \
                 changes"
            ),
        ));
    };
    if !inspection.capabilities.risk_tier {
        return Err(Unavailable::new(
            Unavailable::NO_RISK_TIER_CAPABILITY,
            format!("{named} does not advertise `risk_tier`, so no pending schema change was classified"),
        ));
    }
    match report.usable_change_set() {
        Ok(change_set) => Ok(ChangeSetPreview {
            contract_version: change_set.contract_version,
            adapter: name,
            adapter_version: version,
            changes: change_set.changes.clone(),
        }),
        Err(ChangeSetUnavailable::NotEmitted) => Err(Unavailable::new(
            Unavailable::NO_CHANGE_SET,
            format!(
                "{named} advertises `risk_tier` and emitted no change-set; it claimed to classify \
                 and then did not, which is an adapter bug"
            ),
        )),
        Err(error) => Err(Unavailable::new(
            Unavailable::UNREADABLE_CHANGE_SET,
            error.to_string(),
        )),
    }
}

/// Fold what was learned into the plan, applying the gate to it exactly once.
fn assemble(
    change_set: Result<ChangeSetPreview, Unavailable>,
    gate: &PolicyGate,
    approval_command: Option<&str>,
    baseline: Baseline,
    capabilities: Capabilities,
    report: Option<&PreflightReport>,
) -> SchemaPreview {
    let (change_set, change_set_unavailable) = match change_set {
        Ok(change_set) => (Some(change_set), None),
        Err(reason) => (None, Some(reason)),
    };
    // With the tier gate switched off (D6) *and* no always-on baseline, nothing
    // decides anything — which is not the same as deciding to allow, and must
    // not render as a verdict the operator never configured.
    let decides = gate.policy().is_some() || baseline != Baseline::None;
    SchemaPreview {
        change_set,
        change_set_unavailable,
        policy: decides.then(|| {
            PolicyPreview::of(
                &policy::evaluate(gate.policy(), baseline, capabilities, report),
                approval_command.filter(|_| gate.has_hook()),
            )
        }),
        window_safe: match baseline {
            Baseline::WindowSafety => Some(WindowSafety::of(report)),
            // Single-host and multi-host run one version at a time, so they
            // never ask — reported as `null`, distinct from "asked, no answer".
            _ => None,
        },
    }
}

impl PolicyPreview {
    /// Flatten a decision for the plan.
    fn of(decision: &PolicyDecision, approval_command: Option<&str>) -> Self {
        let (verdict, reason, reasons) = match decision {
            PolicyDecision::Allow { .. } => (Verdict::Allow, None, Vec::new()),
            PolicyDecision::NeedsApproval { reasons } => {
                (Verdict::NeedsApproval, None, reasons.clone())
            }
            PolicyDecision::Deny { reason, reasons } => {
                (Verdict::Deny, Some(reason.clone()), reasons.clone())
            }
            // A variant added to the gate after this build: not one this plan
            // can characterise, so it reports the safe reading rather than the
            // convenient one.
            other => (
                Verdict::Deny,
                Some(format!(
                    "this build cannot render the policy decision it computed ({other:?}); \
                     upgrade fraisier"
                )),
                Vec::new(),
            ),
        };
        Self {
            decision: verdict,
            worst_tier: reasons.iter().filter_map(|reason| reason.tier).max(),
            reason,
            // A hook cannot unblock a refusal, so naming one beside a `deny`
            // would send an operator to a script that will not help.
            approval_command: approval_command
                .filter(|_| verdict == Verdict::NeedsApproval)
                .map(str::to_owned),
            reasons,
        }
    }
}

/// Whether `--fail-on-block` should fail this plan.
///
/// A plan was produced either way, so a dry-run exits 0 by default — terraform
/// semantics. `--fail-on-block` is for the pipeline that wants the plan *and* a
/// nonzero exit when the deploy it previews would not go through: both a refusal
/// and a decision waiting on a human qualify, because neither one deploys
/// unattended.
///
/// A plan with no verdict at all (D6: no `[policy]`, no baseline) never blocks —
/// a pipeline must not start failing on a gate its config never opted in to.
#[must_use]
pub fn would_block(preview: &SchemaPreview) -> bool {
    matches!(
        preview.policy.as_ref().map(|policy| policy.decision),
        Some(Verdict::Deny | Verdict::NeedsApproval)
    )
}

/// The phrase every degraded line ends on.
///
/// A plan is read fast, and a missing change-set at a glance looks exactly like
/// an empty one. Saying so in the string — not only in the documentation — is
/// what keeps the two apart at 3am.
const UNKNOWN_NOT_ZERO: &str = "Risk is unknown, not zero.";

/// How wide a plan line may run before it wraps.
const WIDTH: usize = 78;

/// Render the schema half of a plan: what changes, and what the policy would
/// say about it.
///
/// Pure, so the whole render is covered without building a config or touching a
/// database — the same shape as the perf gate's `format_perf_detail`. Returns
/// the empty string when there is nothing to add, which is what keeps
/// `--dry-run --skip-preflight` byte-identical to the plan it printed before
/// this phase.
#[must_use]
pub fn render(preview: &SchemaPreview) -> String {
    let mut lines: Vec<String> = Vec::new();
    match (&preview.change_set, &preview.change_set_unavailable) {
        (Some(change_set), _) => lines.extend(render_change_set(change_set)),
        // The operator turned preflight off on this run, so telling them the
        // plan cannot see the schema is telling them what they just asked for
        // (rule 1). Every other cause is said out loud.
        (None, Some(reason)) if reason.code != Unavailable::SKIPPED => {
            lines.extend(wrap(&format!(
                "schema change-set: UNAVAILABLE — {}. {UNKNOWN_NOT_ZERO}",
                reason.detail
            )));
        }
        (None, _) => {}
    }
    if let Some(window) = preview.window_safe {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.extend(wrap(match window {
            WindowSafety::Safe => "window safety: certified for the two-version blue-green window",
            WindowSafety::Unsafe => {
                "window safety: NOT CERTIFIED — the migration is not forward-compatible for a \
                 two-version window, and blue-green runs N-1 and N against one database"
            }
            WindowSafety::Unknown => {
                "window safety: UNKNOWN — the migration adapter emitted no window-safety verdict, \
                 so nothing can certify the hold window. Risk is unknown, not zero."
            }
        }));
    }
    if let Some(policy) = &preview.policy {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.extend(render_policy(policy, preview.change_set.as_ref()));
    }
    lines
        .iter()
        .map(|line| format!("  {line}").trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The change-set table, worst-first.
fn render_change_set(change_set: &ChangeSetPreview) -> Vec<String> {
    let header = format!(
        "schema change-set ({} {}, contract v{})",
        change_set.adapter, change_set.adapter_version, change_set.contract_version
    );
    if change_set.changes.is_empty() {
        // Deliberately not the UNAVAILABLE wording: the adapter looked, and
        // there is nothing to change. Only one of those two is safe.
        return vec![format!("{header} — no schema changes")];
    }
    let mut sorted: Vec<&SchemaChange> = change_set.changes.iter().collect();
    // Unclassified first (nothing vouched for it), then most severe down to
    // least. Stable, so equal severities keep the order they will apply in.
    sorted.sort_by_key(|change| (change.tier.is_some(), std::cmp::Reverse(change.tier)));

    let rows: Vec<[String; 4]> = sorted
        .iter()
        .map(|change| {
            [
                format!("[{}]", change.tier.map_or("unclassified", RiskTier::as_str)),
                change.kind.clone(),
                change.object.clone(),
                change.migration.clone().unwrap_or_default(),
            ]
        })
        .collect();
    // Widths come from the data: a hard-coded guess is one long object name
    // away from a ragged table.
    let widths: Vec<usize> = (0..3)
        .map(|column| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();

    let mut lines = vec![format!(
        "{header} — {} change{}:",
        change_set.changes.len(),
        if change_set.changes.len() == 1 {
            ""
        } else {
            "s"
        }
    )];
    lines.extend(sorted.iter().zip(&rows).map(|(change, row)| {
        let mut line = format!(
            "  {:tag$}  {:kind$}  {:object$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            tag = widths[0],
            kind = widths[1],
            object = widths[2],
        );
        if let Some(detail) = &change.detail {
            line = format!("{}  {detail}", line.trim_end());
        }
        line
    }));
    lines
}

/// The verdict, and — when a hook could lift it — what to satisfy.
fn render_policy(policy: &PolicyPreview, change_set: Option<&ChangeSetPreview>) -> Vec<String> {
    match policy.decision {
        Verdict::Allow => {
            let changes = change_set.map_or(0, |set| set.changes.len());
            vec![if changes == 0 {
                "policy: would allow — nothing to decide".to_owned()
            } else {
                format!("policy: would allow — all {changes} planned change(s) auto-apply")
            }]
        }
        // A refusal already names what is responsible, up to three objects and
        // a count of the rest, so itemising it again would only repeat itself.
        Verdict::Deny => wrap(&format!(
            "policy: WOULD BLOCK — {}",
            policy.reason.as_deref().unwrap_or("refused")
        )),
        Verdict::NeedsApproval => {
            let worst = policy
                .worst_tier
                .map_or_else(String::new, |tier| format!(" (worst tier: {tier})"));
            let mut lines = wrap(&format!(
                "policy: WOULD BLOCK — {} change(s) require approval{worst}",
                policy.reasons.len()
            ));
            // Every triggering change, not just the worst: an operator deciding
            // whether to go and get sign-off needs the whole picture.
            lines.extend(policy.reasons.iter().map(|reason| {
                let migration = reason
                    .migration
                    .as_deref()
                    .map_or_else(String::new, |migration| format!(" ({migration})"));
                format!("  · {} {}{migration}", reason.kind, reason.object)
            }));
            if let Some(command) = &policy.approval_command {
                lines.push(format!("  approval hook: {command}"));
            }
            lines
        }
    }
}

/// Wrap `text` to [`WIDTH`], indenting continuations under the first line.
fn wrap(text: &str) -> Vec<String> {
    /// How far a continuation line is indented under the line it continues.
    const CONTINUATION: &str = "  ";

    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            // `+ 1` for the space this word would need. The indent counts
            // against the budget, so a wrapped block stays inside the width.
            Some(line) if line.chars().count() + 1 + word.chars().count() <= WIDTH => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(format!(
                "{}{word}",
                if lines.is_empty() { "" } else { CONTINUATION }
            )),
        }
    }
    lines
}

/// Strip `user:password@` out of every URL in `text`.
///
/// A plan is printed and logged, and the confiture adapter folds the first line
/// of stderr into its error message — where a failed connection prints the DSN
/// in full. The host survives, because *which* database could not be reached is
/// the actionable half; the credentials do not.
pub fn redact_credentials(text: &str) -> String {
    /// What ends a URL's authority section.
    const AUTHORITY_END: [char; 8] = ['/', '?', '#', ' ', '\t', '"', '\'', ','];
    /// What separates a scheme from the authority that may carry credentials.
    const MARK: &str = "://";

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(mark) = rest.find(MARK) {
        let (head, authority) = rest.split_at(mark + MARK.len());
        out.push_str(head);
        let end = authority.find(AUTHORITY_END).unwrap_or(authority.len());
        if let Some(at) = authority[..end].rfind('@') {
            out.push_str("***");
            rest = &authority[at..];
        } else {
            out.push_str(&authority[..end]);
            rest = &authority[end..];
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::{
        gather, redact_credentials, ChangeSetPreview, PolicyPreview, SchemaPreview, Unavailable,
        Verdict,
    };
    use async_trait::async_trait;
    use fraisier_core::adapter_axes::{
        AdapterCtx, AdapterDescription, AdapterError, ChangeSet, MigrationAdapter,
        MigrationOutcome, PreflightReport, Revision, RiskTier, SchemaChange, VerifyReport,
    };
    use fraisier_core::policy::{
        ApprovalHook, ApprovalRequest, ApprovalVerdict, Baseline, Policy, PolicyGate, PolicyReason,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// A migration adapter that answers `describe`/`preflight` from a script and
    /// records every call, so a test can pin *how many times* the dry-run asked.
    struct FakeMigration {
        capabilities: Vec<String>,
        version: String,
        report: Result<PreflightReport, AdapterError>,
        describe_fails: Option<AdapterError>,
        trail: Mutex<Vec<&'static str>>,
    }

    impl FakeMigration {
        /// An adapter advertising `capabilities` that answers `preflight` with
        /// `report`.
        fn new(capabilities: &[&str], report: Result<PreflightReport, AdapterError>) -> Self {
            Self {
                capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
                version: "0.40.0".to_owned(),
                report,
                describe_fails: None,
                trail: Mutex::new(Vec::new()),
            }
        }

        /// One that classifies, and reports `changes`.
        fn classifying(changes: Vec<SchemaChange>) -> Self {
            Self::new(
                &["up", "preflight", "risk_tier"],
                Ok(PreflightReport::new(true)
                    .with_window_safe(true)
                    .with_change_set(ChangeSet::new(changes))),
            )
        }

        fn calls(&self) -> Vec<&'static str> {
            self.trail.lock().expect("trail").clone()
        }

        fn record(&self, call: &'static str) {
            self.trail.lock().expect("trail").push(call);
        }
    }

    #[async_trait]
    impl MigrationAdapter for FakeMigration {
        async fn describe(&self) -> Result<AdapterDescription, AdapterError> {
            self.record("describe");
            if let Some(error) = &self.describe_fails {
                return Err(error.clone());
            }
            Ok(AdapterDescription {
                name: "confiture".to_owned(),
                version: self.version.clone(),
                protocol_version: 1,
                capabilities: self.capabilities.clone(),
            })
        }

        async fn current_revision(
            &self,
            _ctx: &AdapterCtx,
        ) -> Result<Option<Revision>, AdapterError> {
            unreachable!("a dry-run never asks for the revision")
        }

        async fn up(
            &self,
            _ctx: &AdapterCtx,
            _target: Option<Revision>,
        ) -> Result<MigrationOutcome, AdapterError> {
            unreachable!("a dry-run never migrates")
        }

        async fn down_to(
            &self,
            _ctx: &AdapterCtx,
            _target: Revision,
        ) -> Result<MigrationOutcome, AdapterError> {
            unreachable!("a dry-run never migrates")
        }

        async fn verify(&self, _ctx: &AdapterCtx) -> Result<VerifyReport, AdapterError> {
            unreachable!("a dry-run never verifies")
        }

        async fn preflight(&self, _ctx: &AdapterCtx) -> Result<PreflightReport, AdapterError> {
            self.record("preflight");
            self.report.clone()
        }
    }

    /// An approval hook that counts the times it was asked. A dry-run must never
    /// reach it.
    struct CountingHook(AtomicUsize);

    #[async_trait]
    impl ApprovalHook for CountingHook {
        async fn request(&self, _request: &ApprovalRequest) -> ApprovalVerdict {
            self.0.fetch_add(1, Ordering::SeqCst);
            ApprovalVerdict::approved("should-never-be-asked")
        }
    }

    fn ctx() -> AdapterCtx {
        AdapterCtx::new("checkout", "production")
    }

    /// The default policy with a hook declared — the shape that sends an
    /// irreversible change for sign-off instead of refusing it outright.
    fn gate_with_hook(hook: Arc<dyn ApprovalHook>) -> PolicyGate {
        PolicyGate::new(Policy::default().with_approval_hook(true)).with_hook(Some(hook))
    }

    fn change(kind: &str, object: &str) -> SchemaChange {
        SchemaChange::new(kind, object)
    }

    /// The unavailability code, or a panic naming what was previewed instead.
    fn unavailable(preview: &SchemaPreview) -> &Unavailable {
        preview
            .change_set_unavailable
            .as_ref()
            .unwrap_or_else(|| panic!("expected an unavailable change-set, got {preview:?}"))
    }

    #[tokio::test]
    async fn an_adapter_without_preflight_degrades_with_a_reason() {
        // No lint at all: there is no report to classify, and the plan says so
        // rather than printing as though the schema were clean.
        let adapter = FakeMigration::new(&["up"], Ok(PreflightReport::new(true)));
        let preview = gather(
            &adapter,
            &ctx(),
            &PolicyGate::default(),
            None,
            Baseline::None,
        )
        .await;
        assert!(preview.change_set.is_none(), "{preview:?}");
        assert_eq!(
            unavailable(&preview).code,
            Unavailable::NO_PREFLIGHT_CAPABILITY
        );
        assert!(
            unavailable(&preview).detail.contains("preflight"),
            "{preview:?}"
        );
    }

    #[tokio::test]
    async fn a_preflight_error_degrades_and_names_the_error() {
        // An unreachable database is the common case. The plan still exists;
        // the change-set does not, and the reason carries the adapter's own
        // words so the operator can act on them.
        let adapter = FakeMigration::new(
            &["up", "preflight", "risk_tier"],
            Err(AdapterError::new(
                fraisier_core::adapter_axes::AdapterErrorKind::Execution,
                "could not connect to server: Connection refused",
            )),
        );
        let preview = gather(
            &adapter,
            &ctx(),
            &PolicyGate::default(),
            None,
            Baseline::None,
        )
        .await;
        assert_eq!(unavailable(&preview).code, Unavailable::PREFLIGHT_FAILED);
        assert!(
            unavailable(&preview).detail.contains("Connection refused"),
            "{preview:?}"
        );
    }

    #[tokio::test]
    async fn a_degradation_reason_never_leaks_the_dsn() {
        // confiture folds the first stderr line into its error message, and a
        // psycopg connection failure prints the whole DSN. The plan is printed
        // and logged, so the credentials must not survive into it.
        let adapter = FakeMigration::new(
            &["up", "preflight"],
            Err(AdapterError::new(
                fraisier_core::adapter_axes::AdapterErrorKind::Execution,
                "connection to postgresql://checkout:hunter2@db.internal:5432/checkout failed",
            )),
        );
        let preview = gather(
            &adapter,
            &ctx(),
            &PolicyGate::default(),
            None,
            Baseline::None,
        )
        .await;
        let detail = &unavailable(&preview).detail;
        assert!(!detail.contains("hunter2"), "password leaked: {detail}");
        assert!(!detail.contains("checkout:"), "userinfo leaked: {detail}");
        // The actionable part survives: which host, and that it failed.
        assert!(detail.contains("db.internal"), "{detail}");
    }

    #[test]
    fn redaction_keeps_a_credential_free_url_intact() {
        assert_eq!(
            redact_credentials("connection to postgresql://db.internal:5432/checkout failed"),
            "connection to postgresql://db.internal:5432/checkout failed"
        );
        assert_eq!(
            redact_credentials("postgres://u:p@h/db and postgresql://a:b@c/d"),
            "postgres://***@h/db and postgresql://***@c/d"
        );
    }

    #[tokio::test]
    async fn a_dry_run_never_calls_the_approval_hook() {
        // Structural — only `PolicyGate::admit` runs a hook, and a preview calls
        // `evaluate`. Asserted anyway: this is the rule a later "just reuse
        // admit" refactor would silently break.
        let hook = Arc::new(CountingHook(AtomicUsize::new(0)));
        let adapter = FakeMigration::classifying(vec![
            change("drop_table", "public.tb_legacy").with_tier(RiskTier::Irreversible)
        ]);
        let preview = gather(
            &adapter,
            &ctx(),
            &gate_with_hook(hook.clone()),
            Some("scripts/deploy/approve.sh"),
            Baseline::None,
        )
        .await;
        assert_eq!(
            preview.policy.as_ref().map(|p| p.decision),
            Some(Verdict::NeedsApproval),
            "{preview:?}"
        );
        assert_eq!(hook.0.load(Ordering::SeqCst), 0, "the hook was asked");
    }

    #[tokio::test]
    async fn the_dry_run_asks_the_adapter_once() {
        // One `describe`, one `preflight` — the same one-call discipline the
        // saga preflight step holds itself to.
        let adapter = FakeMigration::classifying(Vec::new());
        let _ = gather(
            &adapter,
            &ctx(),
            &PolicyGate::default(),
            None,
            Baseline::None,
        )
        .await;
        assert_eq!(adapter.calls(), ["describe", "preflight"]);
    }

    #[tokio::test]
    async fn an_adapter_that_does_not_classify_degrades_with_its_version_named() {
        // "Which side do I upgrade" is the actionable half, so the adapter's own
        // version has to reach the reason.
        let adapter = FakeMigration::new(&["up", "preflight"], Ok(PreflightReport::new(true)));
        let preview = gather(
            &adapter,
            &ctx(),
            &PolicyGate::default(),
            None,
            Baseline::None,
        )
        .await;
        let detail = &unavailable(&preview).detail;
        assert_eq!(
            unavailable(&preview).code,
            Unavailable::NO_RISK_TIER_CAPABILITY
        );
        assert!(detail.contains("confiture 0.40.0"), "{detail}");
        assert!(detail.contains("risk_tier"), "{detail}");
    }

    #[tokio::test]
    async fn a_change_set_from_a_newer_contract_is_unavailable_not_rendered() {
        let future =
            ChangeSet::new(vec![change("drop_table", "public.tb_legacy")]).with_contract_version(2);
        let adapter = FakeMigration::new(
            &["up", "preflight", "risk_tier"],
            Ok(PreflightReport::new(true).with_change_set(future)),
        );
        let preview = gather(
            &adapter,
            &ctx(),
            &PolicyGate::default(),
            None,
            Baseline::None,
        )
        .await;
        assert_eq!(
            unavailable(&preview).code,
            Unavailable::UNREADABLE_CHANGE_SET
        );
        assert!(preview.change_set.is_none(), "{preview:?}");
    }

    #[tokio::test]
    async fn the_preview_verdict_is_the_gates_own_evaluate() {
        // Not a parallel implementation: the same call, on the same inputs.
        let report = PreflightReport::new(true).with_change_set(ChangeSet::new(vec![change(
            "drop_table",
            "public.tb_legacy",
        )
        .with_tier(RiskTier::Irreversible)]));
        let adapter = FakeMigration::new(&["up", "preflight", "risk_tier"], Ok(report.clone()));
        let gate = PolicyGate::new(Policy::default());
        let preview = gather(&adapter, &ctx(), &gate, None, Baseline::None).await;
        let direct = fraisier_core::policy::evaluate(
            gate.policy(),
            Baseline::None,
            fraisier_core::policy::Capabilities::new(true, true),
            Some(&report),
        );
        let fraisier_core::policy::PolicyDecision::Deny { reason, .. } = &direct else {
            panic!("expected a refusal, got {direct:?}");
        };
        let previewed = preview.policy.as_ref().expect("a verdict");
        assert_eq!(previewed.decision, Verdict::Deny);
        assert_eq!(previewed.reason.as_deref(), Some(reason.as_str()));
    }

    #[tokio::test]
    async fn no_policy_section_and_no_baseline_previews_no_verdict() {
        // D6: an operator who has not opted in sees the change-set and nothing
        // else — there is no decision to report.
        let adapter = FakeMigration::classifying(vec![
            change("drop_table", "public.tb_legacy").with_tier(RiskTier::Irreversible)
        ]);
        let preview = gather(
            &adapter,
            &ctx(),
            &PolicyGate::default(),
            None,
            Baseline::None,
        )
        .await;
        assert!(preview.change_set.is_some(), "{preview:?}");
        assert!(preview.policy.is_none(), "{preview:?}");
    }

    #[test]
    fn every_unavailability_code_is_documented() {
        // The codes are published as a stable contract a pipeline branches on,
        // so a new one that never reaches the guide is a contract an operator
        // cannot discover. `docs/schema-risk-policy.md` is the list.
        let guide = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .expect("workspace root")
                .join("docs/schema-risk-policy.md"),
        )
        .expect("read the operator guide");
        for code in [
            Unavailable::SKIPPED,
            Unavailable::PREFLIGHT_OFF,
            Unavailable::ADAPTER_UNAVAILABLE,
            Unavailable::NO_PREFLIGHT_CAPABILITY,
            Unavailable::PREFLIGHT_FAILED,
            Unavailable::NO_RISK_TIER_CAPABILITY,
            Unavailable::NO_CHANGE_SET,
            Unavailable::UNREADABLE_CHANGE_SET,
        ] {
            assert!(guide.contains(code), "undocumented unavailability: {code}");
        }
    }

    // -----------------------------------------------------------------------
    // `--fail-on-block`
    // -----------------------------------------------------------------------

    /// A preview whose gate reached `decision`.
    fn decided(decision: Verdict) -> SchemaPreview {
        SchemaPreview {
            policy: Some(PolicyPreview {
                decision,
                worst_tier: None,
                reason: Some("because".to_owned()),
                reasons: Vec::new(),
                approval_command: None,
            }),
            ..SchemaPreview::default()
        }
    }

    #[test]
    fn fail_on_block_exits_nonzero_on_deny() {
        assert!(super::would_block(&decided(Verdict::Deny)));
    }

    #[test]
    fn fail_on_block_exits_nonzero_on_needs_approval() {
        // A decision waiting on a human does not deploy unattended either, so a
        // pipeline gating on `--fail-on-block` has to see it.
        assert!(super::would_block(&decided(Verdict::NeedsApproval)));
    }

    #[test]
    fn fail_on_block_exits_zero_on_allow() {
        assert!(!super::would_block(&decided(Verdict::Allow)));
    }

    #[test]
    fn fail_on_block_exits_zero_when_nothing_decided() {
        // D6: no `[policy]` section and no baseline means no verdict, and a
        // pipeline must not start failing on a gate its config never opted in
        // to.
        assert!(!super::would_block(&SchemaPreview::default()));
    }

    // -----------------------------------------------------------------------
    // The human render
    // -----------------------------------------------------------------------

    /// A preview of `changes`, classified by a current confiture.
    fn previewed(changes: Vec<SchemaChange>) -> SchemaPreview {
        SchemaPreview {
            change_set: Some(ChangeSetPreview {
                contract_version: 1,
                adapter: "confiture".to_owned(),
                adapter_version: "0.40.0".to_owned(),
                changes,
            }),
            ..SchemaPreview::default()
        }
    }

    #[test]
    fn render_shows_tier_kind_object_and_migration_per_change() {
        let rendered = super::render(&previewed(vec![change(
            "drop_column",
            "public.tb_user.legacy_flag",
        )
        .with_migration("20260804120100")
        .with_tier(RiskTier::Irreversible)]));
        assert!(rendered.contains("[irreversible]"), "{rendered}");
        assert!(rendered.contains("drop_column"), "{rendered}");
        assert!(
            rendered.contains("public.tb_user.legacy_flag"),
            "{rendered}"
        );
        assert!(rendered.contains("20260804120100"), "{rendered}");
        // The header names who classified it, and at which contract.
        assert!(
            rendered.contains("confiture 0.40.0, contract v1"),
            "{rendered}"
        );
    }

    #[test]
    fn render_sorts_by_severity_worst_first() {
        // An operator scans the top of the list. The dangerous change has to be
        // there, not wherever migration order happened to put it.
        let rendered = super::render(&previewed(vec![
            change("add_column", "public.tb_user.nickname").with_tier(RiskTier::Additive),
            change("drop_table", "public.tb_legacy").with_tier(RiskTier::Irreversible),
            change("entangle_column", "public.tb_user.spin"),
            change("create_index", "public.ix_user_email").with_tier(RiskTier::LockRisky),
        ]));
        let order: Vec<&str> = rendered
            .lines()
            .filter_map(|line| line.trim().strip_prefix('['))
            .map(|line| line.split(']').next().unwrap_or_default())
            .collect();
        assert_eq!(
            order,
            ["unclassified", "irreversible", "lock_risky", "additive"],
            "{rendered}"
        );
    }

    #[test]
    fn render_keeps_migration_order_within_a_tier() {
        // Worst-first is a *stable* re-ordering: two changes of equal severity
        // still apply in the order the adapter listed them, and the plan has to
        // show that order or it is not a plan.
        let rendered = super::render(&previewed(vec![
            change("drop_table", "public.tb_b").with_tier(RiskTier::Irreversible),
            change("drop_table", "public.tb_a").with_tier(RiskTier::Irreversible),
        ]));
        let b = rendered.find("public.tb_b").expect("b");
        let a = rendered.find("public.tb_a").expect("a");
        assert!(b < a, "{rendered}");
    }

    #[test]
    fn render_aligns_columns_from_the_data() {
        // Widths come from the rows, not from a hard-coded guess that a long
        // object name would blow through.
        let rendered = super::render(&previewed(vec![
            change(
                "add_column",
                "public.tb_user.a_very_long_column_name_indeed",
            )
            .with_tier(RiskTier::Additive),
            change("drop_table", "public.b").with_tier(RiskTier::Irreversible),
        ]));
        let columns: Vec<usize> = rendered
            .lines()
            .filter(|line| line.trim_start().starts_with('['))
            .map(|line| line.find("public.").expect("an object column"))
            .collect();
        assert_eq!(columns.len(), 2, "{rendered}");
        assert_eq!(
            columns[0], columns[1],
            "objects are not aligned:\n{rendered}"
        );
    }

    #[test]
    fn render_says_no_schema_changes_when_the_set_is_empty() {
        // Distinct wording from the degraded case, and this is the test that
        // pins the distinction: the adapter looked, and there is nothing.
        let rendered = super::render(&previewed(Vec::new()));
        assert!(rendered.contains("no schema changes"), "{rendered}");
        assert!(!rendered.contains("UNAVAILABLE"), "{rendered}");
        assert!(!rendered.contains("unknown, not zero"), "{rendered}");
    }

    #[test]
    fn render_says_risk_is_unknown_not_zero_when_degraded() {
        let rendered = super::render(&SchemaPreview {
            change_set_unavailable: Some(Unavailable::new(
                Unavailable::NO_RISK_TIER_CAPABILITY,
                "confiture 0.38.1 does not advertise `risk_tier`",
            )),
            ..SchemaPreview::default()
        });
        assert!(rendered.contains("UNAVAILABLE"), "{rendered}");
        assert!(rendered.contains("confiture 0.38.1"), "{rendered}");
        assert!(
            rendered.contains("Risk is unknown, not zero."),
            "{rendered}"
        );
    }

    #[test]
    fn render_is_silent_when_preflight_was_skipped() {
        // Rule 1's other half: the operator who turned preflight off is not told
        // that preflight is off. This is what keeps `--dry-run --skip-preflight`
        // byte-identical to the plan it printed before this phase.
        let rendered = super::render(&SchemaPreview {
            change_set_unavailable: Some(Unavailable::new(
                Unavailable::SKIPPED,
                "preflight was skipped for this run",
            )),
            ..SchemaPreview::default()
        });
        assert_eq!(rendered, "");
    }

    #[test]
    fn render_names_the_approval_hook_when_one_is_configured() {
        let rendered = super::render(&SchemaPreview {
            policy: Some(PolicyPreview {
                decision: Verdict::NeedsApproval,
                worst_tier: Some(RiskTier::Irreversible),
                reason: None,
                reasons: vec![PolicyReason::new(
                    Some(RiskTier::Irreversible),
                    "public.tb_user.legacy_flag",
                    "drop_column",
                    Some("20260804120100".to_owned()),
                )],
                approval_command: Some("scripts/deploy/approve.sh".to_owned()),
            }),
            ..SchemaPreview::default()
        });
        assert!(rendered.contains("WOULD BLOCK"), "{rendered}");
        assert!(
            rendered.contains("approval hook: scripts/deploy/approve.sh"),
            "{rendered}"
        );
        // The changes that need sign-off are itemised, not just counted.
        assert!(
            rendered.contains("· drop_column public.tb_user.legacy_flag (20260804120100)"),
            "{rendered}"
        );
    }

    #[test]
    fn render_names_no_hook_beside_a_refusal_no_hook_could_lift() {
        // Pointing an operator at a script that cannot help them is worse than
        // saying nothing.
        let rendered = super::render(&SchemaPreview {
            policy: Some(PolicyPreview {
                decision: Verdict::Deny,
                worst_tier: None,
                reason: Some("the policy refuses 1 of 1 planned schema change(s)".to_owned()),
                reasons: Vec::new(),
                approval_command: Some("scripts/deploy/approve.sh".to_owned()),
            }),
            ..SchemaPreview::default()
        });
        assert!(rendered.contains("WOULD BLOCK"), "{rendered}");
        assert!(!rendered.contains("approve.sh"), "{rendered}");
    }

    #[test]
    fn render_shows_the_window_safety_verdict() {
        let unknown = super::render(&SchemaPreview {
            window_safe: Some(super::WindowSafety::Unknown),
            ..SchemaPreview::default()
        });
        assert!(unknown.contains("window safety: UNKNOWN"), "{unknown}");
        assert!(unknown.contains("Risk is unknown, not zero."), "{unknown}");

        let unsafe_ = super::render(&SchemaPreview {
            window_safe: Some(super::WindowSafety::Unsafe),
            ..SchemaPreview::default()
        });
        assert!(
            unsafe_.contains("window safety: NOT CERTIFIED"),
            "{unsafe_}"
        );

        let safe = super::render(&SchemaPreview {
            window_safe: Some(super::WindowSafety::Safe),
            ..SchemaPreview::default()
        });
        assert!(safe.contains("window safety: certified"), "{safe}");
    }

    #[tokio::test]
    async fn blue_green_previews_a_verdict_with_no_policy_section() {
        // The D6 carve-out: the window-safety baseline is not opt-in, so
        // blue-green always has something to report.
        let adapter = FakeMigration::new(
            &["up", "preflight"],
            Ok(PreflightReport::new(true).with_window_safe(false)),
        );
        let preview = gather(
            &adapter,
            &ctx(),
            &PolicyGate::default(),
            None,
            Baseline::WindowSafety,
        )
        .await;
        assert_eq!(preview.window_safe, Some(super::WindowSafety::Unsafe));
        let previewed = preview.policy.as_ref().expect("a verdict");
        assert_eq!(previewed.decision, Verdict::Deny);
        assert!(
            previewed
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("window_safe = false")),
            "{previewed:?}"
        );
    }

    #[test]
    fn plan_summary_serializes_change_set_and_policy() {
        let preview = SchemaPreview {
            change_set: Some(ChangeSetPreview {
                contract_version: 1,
                adapter: "confiture".to_owned(),
                adapter_version: "0.40.0".to_owned(),
                changes: vec![SchemaChange::new("add_column", "public.tb_user.nickname")
                    .with_migration("20260804120000")
                    .with_tier(RiskTier::Additive)
                    .with_detail("ADD COLUMN nickname text NULL")],
            }),
            policy: Some(PolicyPreview {
                decision: Verdict::NeedsApproval,
                worst_tier: Some(RiskTier::Irreversible),
                reason: None,
                reasons: vec![PolicyReason::new(
                    Some(RiskTier::Irreversible),
                    "public.tb_user.legacy_flag",
                    "drop_column",
                    Some("20260804120100".to_owned()),
                )],
                approval_command: Some("scripts/deploy/approve.sh".to_owned()),
            }),
            ..SchemaPreview::default()
        };
        let json = serde_json::to_value(&preview).expect("serialize");
        assert_eq!(json["change_set"]["contract_version"], 1);
        assert_eq!(json["change_set"]["adapter_version"], "0.40.0");
        assert_eq!(json["change_set"]["changes"][0]["tier"], "additive");
        assert_eq!(json["change_set_unavailable"], serde_json::Value::Null);
        assert_eq!(json["policy"]["decision"], "needs_approval");
        assert_eq!(json["policy"]["worst_tier"], "irreversible");
        assert_eq!(json["policy"]["reasons"][0]["kind"], "drop_column");
    }

    #[test]
    fn plan_summary_without_a_change_set_emits_null_and_a_reason() {
        // Never an omitted key: "absent" and "null" read the same to a sloppy
        // consumer, and the *reason* has to travel beside the null.
        let preview = SchemaPreview {
            change_set_unavailable: Some(Unavailable::new(
                Unavailable::NO_RISK_TIER_CAPABILITY,
                "the migration adapter does not advertise `risk_tier`",
            )),
            ..SchemaPreview::default()
        };
        let json = serde_json::to_value(&preview).expect("serialize");
        let object = json.as_object().expect("an object");
        assert!(object.contains_key("change_set"), "{json}");
        assert!(object.contains_key("policy"), "{json}");
        assert_eq!(json["change_set"], serde_json::Value::Null);
        assert_eq!(
            json["change_set_unavailable"]["code"],
            "no_risk_tier_capability"
        );
    }

    #[test]
    fn an_empty_change_set_is_not_an_unavailable_one() {
        // The presence distinction the type system carries in Phase 01, out on
        // the wire: "the adapter looked and found nothing" must not serialize
        // the same as "nobody looked".
        let looked = SchemaPreview {
            change_set: Some(ChangeSetPreview {
                contract_version: 1,
                adapter: "confiture".to_owned(),
                adapter_version: "0.40.0".to_owned(),
                changes: Vec::new(),
            }),
            ..SchemaPreview::default()
        };
        let json = serde_json::to_value(&looked).expect("serialize");
        assert_eq!(json["change_set"]["changes"], serde_json::json!([]));
        assert_eq!(json["change_set_unavailable"], serde_json::Value::Null);
    }
}
