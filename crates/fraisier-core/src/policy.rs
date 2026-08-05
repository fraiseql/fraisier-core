//! The **policy gate** — one decision function over one preflight report.
//!
//! [`evaluate`] is where a preflight report becomes a verdict. It is pure: it
//! takes the facts already gathered (what the adapter says it can do, what its
//! preflight reported) and returns a [`PolicyDecision`]. All I/O — running
//! preflight, running an approval hook — belongs above it, which is what lets the
//! whole decision table be covered by unit tests with no fixtures and no
//! database.
//!
//! Two orthogonal questions are answered in one pass, so that a refusal has
//! exactly one origin and names which rule fired:
//!
//! 1. *Can N-1 and N share this database for the blue-green hold window?* — the
//!    [`Baseline`], always on for the strategy that needs it.
//! 2. *Is this change destructive enough to need a human?* — the **tier
//!    policy**, opt-in through the `[policy]` config section.
//!
//! They do not substitute for one another: a `lock_risky` index build is
//! window-safe and policy-relevant, while a migration confiture cannot read is
//! window-unsafe with no tier at all.
//!
//! ## The baseline: window safety
//!
//! A blue-green swap runs version N-1 and N against **one shared Postgres** for
//! the hold window, so the swap is only allowed when the pending migration is
//! certified *forward-compatible for a two-version window*. fraisier **consumes
//! confiture's verdict** — it never authors, validates, or reinvents
//! expand/contract logic, and it knows nothing of confiture's classifier
//! internals (DDL codes, SQL-vs-`.py`).
//!
//! The verdict is first-class:
//! [`PreflightReport::window_safe`](crate::adapter_axes::PreflightReport::window_safe)
//! is `Some(true)` iff **every** pending migration is forward-compatible for a
//! two-version window — confiture folds the relevant concerns into it
//! (replica-unsafe ops + migrations it cannot classify). It is purely about
//! forward-compatibility: transactionality / reversibility are **not** consulted,
//! because blue-green does no DB rollback (rollback is a traffic swap-back to
//! still-hot blue), so a non-transactional-but-forward-compatible op like
//! `CREATE INDEX CONCURRENTLY` is window-safe. The rule is a single boolean read:
//!
//! - `Some(true)` ⇒ fall through to the tier policy;
//! - `Some(false)` ⇒ **refused** (hard block before any instance or traffic change);
//! - `None` ⇒ **refused** — the adapter offers no window-safety verdict, so
//!   nothing can certify the window (mirrors the `MethodNotSupported`-never-a-pass
//!   design). An adapter without the `preflight` capability is likewise refused.
//!
//! There is **no force-equivalent** (a silent override would re-introduce the
//! exact shared-DB-corruption footgun the rule exists to prevent) and **no
//! fallback to pattern-matching issue codes** — the typed verdict is the contract.
//!
//! The baseline is deliberately **not** opt-in. Only the tier policy is: a
//! deploy with no `[policy]` section behaves exactly as it does today, and that
//! includes still being blocked by the window rule.
//!
//! ### Cross-repo contract (tracked in fraiseql/confiture#154)
//!
//! confiture emits `window_safe` on the `migrate preflight` JSON report and pins
//! it in its contract test. `window_safe == false` for **any** migration confiture
//! cannot certify — including ones its replica classifier cannot read (non-SQL /
//! `.py`) — so "can't see" can never masquerade as "safe". fraisier requires a
//! confiture release that emits the field; an older one returns `None` and is
//! refused (fail safe).
//!
//! ## The tier policy
//!
//! Each planned change carries a [`RiskTier`] the adapter assigned, read through
//! [`usable_change_set`](crate::adapter_axes::PreflightReport::usable_change_set).
//! The [`Policy`] maps each tier to a [`PolicyAction`] independently — the tier
//! ordering is for reporting, never for deciding — and the change-set takes the
//! worst action any single change maps to.
//!
//! ## Absence is never safety
//!
//! Every way of not knowing is a refusal: no `preflight` report, no `risk_tier`
//! capability, a capability advertised with no change-set behind it, a change-set
//! written to a contract version this build cannot read, or one entry whose tier
//! this build does not recognise. None of them mean "proceed", and a refusal says
//! which one it was — see `docs/proposals/migration-risk-contract.md` §6.
//!
//! [fraiseql/confiture#154]: https://github.com/fraiseql/confiture/issues/154

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::adapter_axes::{ChangeSetUnavailable, PreflightReport, RiskTier, SchemaChange};

/// The tiers a policy auto-applies when its config does not say otherwise.
pub const DEFAULT_AUTO_APPLY: [RiskTier; 2] = [RiskTier::Additive, RiskTier::Reversible];

/// The tiers a policy sends to the approval hook when its config does not say
/// otherwise.
pub const DEFAULT_REQUIRE_APPROVAL: [RiskTier; 3] = [
    RiskTier::LockRisky,
    RiskTier::Destructive,
    RiskTier::Irreversible,
];

/// What the policy says to do about one planned change.
///
/// The variants are ordered least → most restrictive, and **that order is
/// load-bearing**: a change-set's outcome is the worst action any single change
/// maps to, computed as a `max` over this ordering. A variant added later must be
/// inserted at its restrictiveness position, not appended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PolicyAction {
    /// Apply without asking anyone.
    AutoApply,
    /// Apply only once the approval hook says yes.
    RequireApproval,
    /// Refuse. No hook can unblock this.
    Deny,
}

/// What the policy does with a change nobody classified.
///
/// Deliberately **not** a [`PolicyAction`]: there is no legitimate configuration
/// in which an unclassified change auto-applies, so the type refuses to express
/// one rather than leaving a validation rule to be remembered. Absence is never
/// safety — see `docs/proposals/migration-risk-contract.md` §6.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum UnclassifiedAction {
    /// Refuse anything the adapter did not classify. The default.
    #[default]
    Deny,
    /// Send it to the approval hook instead of refusing outright.
    RequireApproval,
}

impl From<UnclassifiedAction> for PolicyAction {
    fn from(action: UnclassifiedAction) -> Self {
        match action {
            UnclassifiedAction::Deny => Self::Deny,
            UnclassifiedAction::RequireApproval => Self::RequireApproval,
        }
    }
}

/// The resolved policy — already validated, so the engine itself cannot fail.
///
/// Built from the `[policy]` config section by `PolicySection::resolve`; this
/// struct is `#[non_exhaustive]`, so other crates go through [`Policy::new`] and
/// the `with_*` methods rather than a struct literal.
///
/// # Example
/// ```
/// # use fraisier_core::policy::{Policy, PolicyAction, UnclassifiedAction};
/// # use fraisier_core::adapter_axes::RiskTier;
/// let policy = Policy::default();
/// assert_eq!(policy.actions[&RiskTier::Additive], PolicyAction::AutoApply);
/// assert_eq!(policy.actions[&RiskTier::Irreversible], PolicyAction::RequireApproval);
/// assert_eq!(policy.unclassified, UnclassifiedAction::Deny);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Policy {
    /// What to do about each tier. **A tier absent from this map is denied** —
    /// that is what keeps a tier added to the taxonomy later from silently
    /// auto-applying on configs written today.
    pub actions: BTreeMap<RiskTier, PolicyAction>,
    /// What to do about a change the adapter did not classify.
    pub unclassified: UnclassifiedAction,
    /// Whether an approval hook is configured at all. Without one, a decision
    /// that would need approval is a [`PolicyDecision::Deny`] — never a silent
    /// pass.
    pub has_approval_hook: bool,
}

impl Default for Policy {
    /// The documented default policy: [`DEFAULT_AUTO_APPLY`] applies,
    /// [`DEFAULT_REQUIRE_APPROVAL`] needs sign-off, unclassified denies, and no
    /// approval hook is configured.
    ///
    /// Not `#[derive(Default)]`: an empty action map would deny every tier, and a
    /// policy that denies everything is not the neutral starting point a derive
    /// implies.
    fn default() -> Self {
        Self::new(
            DEFAULT_AUTO_APPLY
                .into_iter()
                .map(|tier| (tier, PolicyAction::AutoApply))
                .chain(
                    DEFAULT_REQUIRE_APPROVAL
                        .into_iter()
                        .map(|tier| (tier, PolicyAction::RequireApproval)),
                )
                .collect(),
            UnclassifiedAction::Deny,
        )
    }
}

impl Policy {
    /// A policy mapping `actions` over the tiers, with no approval hook.
    #[must_use]
    pub const fn new(
        actions: BTreeMap<RiskTier, PolicyAction>,
        unclassified: UnclassifiedAction,
    ) -> Self {
        Self {
            actions,
            unclassified,
            has_approval_hook: false,
        }
    }

    /// State whether an approval hook is configured.
    #[must_use]
    pub const fn with_approval_hook(mut self, present: bool) -> Self {
        self.has_approval_hook = present;
        self
    }

    /// What this policy says about one change.
    ///
    /// An unclassified change takes [`Self::unclassified`]; a classified one
    /// takes its tier's action, and a tier this policy does not mention is
    /// denied.
    fn action_for(&self, change: &SchemaChange) -> PolicyAction {
        change.tier.map_or_else(
            || self.unclassified.into(),
            |tier| {
                self.actions
                    .get(&tier)
                    .copied()
                    .unwrap_or(PolicyAction::Deny)
            },
        )
    }
}

/// Why the gate decided what it decided — carried into the refusal message, the
/// audit record, and the dry-run render.
///
/// One per change that triggered a non-auto-apply outcome, or one for the
/// change-set as a whole when no single change is at fault (nothing classified
/// it, nothing emitted it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct PolicyReason {
    /// The tier that triggered this, or `None` when the change is unclassified.
    pub tier: Option<RiskTier>,
    /// The object the change touches, or a stand-in when the reason is about the
    /// whole change-set.
    pub object: String,
    /// The change's `kind`, or a machine code for a set-level cause.
    pub kind: String,
    /// The migration the change belongs to, when the adapter attributed it.
    pub migration: Option<String>,
}

impl PolicyReason {
    /// The reason attached to one classified-or-not change.
    fn for_change(change: &SchemaChange) -> Self {
        Self {
            tier: change.tier,
            object: change.object.clone(),
            kind: change.kind.clone(),
            migration: change.migration.clone(),
        }
    }

    /// A reason about the change-set as a whole — no single change is at fault.
    fn for_change_set(kind: impl Into<String>) -> Self {
        Self {
            tier: None,
            object: "all pending migrations".to_owned(),
            kind: kind.into(),
            migration: None,
        }
    }
}

/// What the policy gate decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub enum PolicyDecision {
    /// Every change is in an auto-apply tier (or there are no changes).
    Allow {
        /// How many changes were classified as auto-applicable.
        changes: usize,
    },
    /// At least one change needs sign-off. Carries every triggering change, not
    /// just the worst one — an operator asked to approve needs the whole picture.
    NeedsApproval {
        /// Every change that is not auto-apply, in the order the adapter listed
        /// them.
        reasons: Vec<PolicyReason>,
    },
    /// Refused outright — no hook can unblock this.
    Deny {
        /// Which rule fired, in one line an operator can act on at 3am.
        reason: String,
        /// Every change that is not auto-apply, in the order the adapter listed
        /// them. Empty when the refusal is not about any particular change.
        reasons: Vec<PolicyReason>,
    },
}

/// The worst of a set of actions — the outcome a whole change-set takes.
///
/// Expressed as a `max` over [`PolicyAction`]'s derived ordering rather than a
/// match ladder, so a variant added later orders itself.
fn worst(actions: impl IntoIterator<Item = PolicyAction>) -> PolicyAction {
    actions.into_iter().max().unwrap_or(PolicyAction::AutoApply)
}

/// The set-level `kind` codes, for a reason no single change is responsible for.
/// Rendered verbatim like [`SchemaChange::kind`], never parsed for meaning.
mod kinds {
    /// Preflight produced no report at all.
    pub const PREFLIGHT_NOT_RUN: &str = "preflight_not_run";
    /// The adapter does not advertise `risk_tier`.
    pub const NO_RISK_TIER_CAPABILITY: &str = "no_risk_tier_capability";
    /// The adapter advertises `risk_tier` and emitted no change-set.
    pub const NO_CHANGE_SET: &str = "no_change_set";
    /// The change-set is written to a contract this build cannot read.
    pub const UNREADABLE_CHANGE_SET: &str = "unreadable_change_set";
}

/// What the adapter says it can do — the two capability strings this gate reads.
///
/// A struct rather than loose booleans because the two are read by different
/// halves of the decision and a positional `bool, bool` at the call site is a
/// silent-swap waiting to happen.
///
/// # Example
/// ```
/// # use fraisier_core::policy::Capabilities;
/// let advertised = vec!["up".to_owned(), "preflight".to_owned()];
/// let capabilities = Capabilities::from_advertised(&advertised);
/// assert!(capabilities.preflight);
/// assert!(!capabilities.risk_tier);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Capabilities {
    /// `preflight` — the adapter implements the forward-compatibility lint.
    pub preflight: bool,
    /// `risk_tier` — its [`PreflightReport`] carries a classified change-set.
    pub risk_tier: bool,
}

impl Capabilities {
    /// State both capabilities directly.
    #[must_use]
    pub const fn new(preflight: bool, risk_tier: bool) -> Self {
        Self {
            preflight,
            risk_tier,
        }
    }

    /// Read them off an [`AdapterDescription::capabilities`] list.
    ///
    /// [`AdapterDescription::capabilities`]: crate::adapter_axes::AdapterDescription::capabilities
    #[must_use]
    pub fn from_advertised(capabilities: &[String]) -> Self {
        Self::new(
            capabilities.iter().any(|c| c == "preflight"),
            capabilities.iter().any(|c| c == "risk_tier"),
        )
    }
}

/// Which always-on rule applies, independent of whether `[policy]` is
/// configured.
///
/// This is how the window-safety gate survives being folded into the policy
/// gate: the *tier* policy is opt-in by config presence, but the baseline is
/// not. Making it opt-in would silently delete today's blue-green block for
/// every existing user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Baseline {
    /// No always-on rule. Single-host and multi-host deploys run one version at
    /// a time against the database, so there is no shared hold window to
    /// certify.
    None,
    /// Blue-green: N-1 and N serve against **one** database for the hold window,
    /// so the migration must be certified forward-compatible for a two-version
    /// window before any instance or traffic changes.
    WindowSafety,
}

/// The always-on rule's verdict: `Some(reason)` refuses, `None` falls through to
/// the tier policy.
///
/// The single implementation of the window-safety rule. [`crate::window_safety`]
/// delegates here rather than keeping a second copy.
pub(crate) fn baseline_verdict(
    baseline: Baseline,
    capabilities: Capabilities,
    report: Option<&PreflightReport>,
) -> Option<String> {
    match baseline {
        Baseline::None => None,
        Baseline::WindowSafety => {
            if !capabilities.preflight {
                return Some(
                    "the migration adapter does not advertise `preflight`; nothing can certify \
                     the migration window-safe for a two-version blue-green window"
                        .to_owned(),
                );
            }
            let Some(report) = report else {
                return Some(
                    "no preflight report was produced; cannot certify the window".to_owned(),
                );
            };
            // `window_safe` is the SOLE window-safety verdict — purely
            // forward-compatibility for the two-version window. Transactionality
            // / reversibility are deliberately NOT consulted: blue-green does no
            // DB rollback (rollback is a traffic swap-back to still-hot blue on
            // the expanded schema), so a non-transactional but forward-compatible
            // op like `CREATE INDEX CONCURRENTLY` is window-safe. A genuinely
            // broken migration still fails at the `migrate` step, before any
            // traffic moves, not here.
            match report.window_safe {
                Some(true) => None,
                Some(false) => Some(
                    "the migration is NOT forward-compatible for a two-version window \
                     (confiture window_safe = false)"
                        .to_owned(),
                ),
                None => Some(
                    "the migration adapter returned no window-safety verdict (window_safe); \
                     blue-green needs a confiture release that emits it — see \
                     fraiseql/confiture#154"
                        .to_owned(),
                ),
            }
        }
    }
}

/// Apply `policy` to an already-gathered preflight result — the single decision
/// function.
///
/// `baseline` selects the always-on rule for the deploy strategy and is applied
/// **first**, with or without a `[policy]` section. `policy` is `None` when no
/// section is configured, which leaves the tier gate switched off entirely (D6:
/// configuring the section is the opt-in). `capabilities` is what the adapter
/// advertises; `report` is the preflight result, `None` when preflight did not
/// run.
///
/// Every way of not knowing resolves to a refusal, because absence is never
/// safety — see `docs/proposals/migration-risk-contract.md` §6.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::PreflightReport;
/// # use fraisier_core::policy::{evaluate, Baseline, Capabilities, Policy, PolicyDecision};
/// // An adapter that lints but does not classify, under a policy that expects
/// // a classification: refused, because nothing looked at the schema changes.
/// let report = PreflightReport::new(true);
/// let lints_only = Capabilities::new(true, false);
/// let decision = evaluate(
///     Some(&Policy::default()),
///     Baseline::None,
///     lints_only,
///     Some(&report),
/// );
/// assert!(matches!(decision, PolicyDecision::Deny { .. }));
///
/// // The same deploy with no `[policy]` section is not tier-gated at all.
/// assert_eq!(
///     evaluate(None, Baseline::None, lints_only, Some(&report)),
///     PolicyDecision::Allow { changes: 0 },
/// );
/// ```
#[must_use]
pub fn evaluate(
    policy: Option<&Policy>,
    baseline: Baseline,
    capabilities: Capabilities,
    report: Option<&PreflightReport>,
) -> PolicyDecision {
    // The baseline is not opt-in, so it is read before the `[policy]` switch.
    // Its refusal carries no per-change reasons: no tier is at fault, the shared
    // database is.
    if let Some(reason) = baseline_verdict(baseline, capabilities, report) {
        return PolicyDecision::Deny {
            reason,
            reasons: Vec::new(),
        };
    }
    // No `[policy]` section ⇒ no tier gate at all (D6). This is the switch that
    // keeps every existing deploy behaving exactly as it does today.
    let Some(policy) = policy else {
        return PolicyDecision::Allow { changes: 0 };
    };
    // The guards run most-fundamental first: "nobody looked" outranks "the one
    // who looked cannot classify", which outranks "the classification is
    // unreadable". Each names its own cause — a generic "policy denied" is
    // useless at 3am.
    let Some(report) = report else {
        return PolicyDecision::Deny {
            reason: "no preflight report was produced, so nothing inspected the pending schema \
                     changes; the policy cannot approve what nobody has looked at"
                .to_owned(),
            reasons: vec![PolicyReason::for_change_set(kinds::PREFLIGHT_NOT_RUN)],
        };
    };
    if !capabilities.risk_tier {
        return unclassified_change_set(
            policy,
            "the migration adapter does not advertise `risk_tier`, so no pending schema change \
             was classified",
            kinds::NO_RISK_TIER_CAPABILITY,
        );
    }
    let change_set = match report.usable_change_set() {
        Ok(change_set) => change_set,
        Err(ChangeSetUnavailable::NotEmitted) => {
            return unclassified_change_set(
                policy,
                "the migration adapter advertises `risk_tier` but emitted no change-set; it \
                 claimed to classify and then did not, which is an adapter bug",
                kinds::NO_CHANGE_SET,
            )
        }
        // Every other way of being unreadable — a contract from the future
        // today, whatever is added later — is refused by its own message and is
        // never approvable: an approver would be signing off on a payload
        // nobody in this process can read.
        Err(error) => {
            return PolicyDecision::Deny {
                reason: error.to_string(),
                reasons: vec![PolicyReason::for_change_set(kinds::UNREADABLE_CHANGE_SET)],
            }
        }
    };
    let triggers = change_set
        .changes
        .iter()
        .map(|change| (policy.action_for(change), PolicyReason::for_change(change)))
        .filter(|(action, _)| *action != PolicyAction::AutoApply)
        .collect();
    assemble(policy, triggers, change_set.changes.len())
}

/// The decision when the change-set as a whole is unclassified — nobody is at
/// fault change-by-change, so the reason is about the set.
fn unclassified_change_set(policy: &Policy, cause: &str, kind: &str) -> PolicyDecision {
    let reason = PolicyReason::for_change_set(kind);
    match policy.unclassified {
        UnclassifiedAction::Deny => PolicyDecision::Deny {
            reason: cause.to_owned(),
            reasons: vec![reason],
        },
        UnclassifiedAction::RequireApproval => {
            assemble(policy, vec![(PolicyAction::RequireApproval, reason)], 0)
        }
    }
}

/// Fold the per-change actions into one decision.
///
/// `triggers` holds every change that is **not** auto-apply, in the order the
/// adapter listed them; `changes` is the size of the whole set. The decision
/// takes the worst action, and carries all of the triggers with it — an operator
/// asked to approve needs the whole picture, not only the worst line of it.
fn assemble(
    policy: &Policy,
    triggers: Vec<(PolicyAction, PolicyReason)>,
    changes: usize,
) -> PolicyDecision {
    let outcome = worst(triggers.iter().map(|(action, _)| *action));
    // The message names only what is responsible for the outcome — an operator
    // reading a refusal should not have to work out which of the listed changes
    // is the one that cannot be unblocked.
    let responsible: Vec<&PolicyReason> = triggers
        .iter()
        .filter(|(action, _)| *action == outcome)
        .map(|(_, reason)| reason)
        .collect();
    let (count, named) = (responsible.len(), summarise(&responsible));
    let reasons: Vec<PolicyReason> = triggers.into_iter().map(|(_, reason)| reason).collect();
    match outcome {
        PolicyAction::AutoApply => PolicyDecision::Allow { changes },
        // A policy that asks for sign-off with nothing configured to give it is
        // a refusal, not an approval. The alternative is a `NeedsApproval` the
        // caller has to resolve into a silent pass — the gate as theatre.
        PolicyAction::RequireApproval if !policy.has_approval_hook => PolicyDecision::Deny {
            reason: format!(
                "approval is required for {named}, but no approval hook is configured \
                 (`[policy].approval_command`)"
            ),
            reasons,
        },
        PolicyAction::RequireApproval => PolicyDecision::NeedsApproval { reasons },
        PolicyAction::Deny => PolicyDecision::Deny {
            reason: format!(
                "the policy refuses {count} of {changes} planned schema change(s): {named}"
            ),
            reasons,
        },
    }
}

/// Name the changes a refusal is about, in one line.
///
/// Capped: a 400-change migration must not print 400 objects into a refusal
/// message. The full list always survives in
/// [`PolicyDecision::Deny::reasons`], which is what the audit record and the
/// dry-run render read.
fn summarise(reasons: &[&PolicyReason]) -> String {
    /// How many changes a refusal names before it starts counting.
    const NAMED: usize = 3;

    let mut rendered: Vec<String> = reasons
        .iter()
        .take(NAMED)
        .map(|reason| {
            let tier = reason.tier.map_or("unclassified", RiskTier::as_str);
            format!("{} ({}, {tier})", reason.object, reason.kind)
        })
        .collect();
    if let Some(rest) = reasons.len().checked_sub(NAMED).filter(|rest| *rest > 0) {
        rendered.push(format!("and {rest} more"));
    }
    rendered.join(", ")
}

#[cfg(test)]
mod tests {
    use super::{
        evaluate, worst, Baseline, Capabilities, Policy, PolicyAction, PolicyDecision,
        PolicyReason, UnclassifiedAction,
    };
    use crate::adapter_axes::{ChangeSet, PreflightReport, RiskTier, SchemaChange};

    /// The tier half of the gate on its own — no baseline rule in the way.
    fn tier_gate(
        policy: Option<&Policy>,
        capabilities: Capabilities,
        report: Option<&PreflightReport>,
    ) -> PolicyDecision {
        evaluate(policy, Baseline::None, capabilities, report)
    }

    /// Both capabilities advertised — the adapter classifies.
    const CLASSIFIES: Capabilities = Capabilities::new(true, true);
    /// `preflight` only — an adapter that lints but does not classify.
    const NO_TIERS: Capabilities = Capabilities::new(true, false);

    /// The default policy, with an approval hook configured.
    fn policy() -> Policy {
        Policy::default().with_approval_hook(true)
    }

    /// The default policy, sending unclassified changes to the hook instead of
    /// refusing them outright.
    fn lenient() -> Policy {
        Policy::new(
            Policy::default().actions,
            UnclassifiedAction::RequireApproval,
        )
        .with_approval_hook(true)
    }

    /// A passing preflight report carrying `changes`.
    fn classified(changes: Vec<SchemaChange>) -> PreflightReport {
        PreflightReport::new(true).with_change_set(ChangeSet::new(changes))
    }

    /// A passing preflight report that classified nothing.
    fn unclassified_report() -> PreflightReport {
        PreflightReport::new(true)
    }

    fn change(kind: &str, object: &str) -> SchemaChange {
        SchemaChange::new(kind, object)
    }

    /// A policy that maps exactly `actions` and nothing else, with a hook.
    fn policy_of(actions: &[(RiskTier, PolicyAction)]) -> Policy {
        Policy::new(actions.iter().copied().collect(), UnclassifiedAction::Deny)
            .with_approval_hook(true)
    }

    /// The changes an approval request is about, or a panic naming what was
    /// decided instead.
    fn approval(decision: &PolicyDecision) -> &[PolicyReason] {
        match decision {
            PolicyDecision::NeedsApproval { reasons } => reasons,
            other => panic!("expected an approval request, got {other:?}"),
        }
    }

    /// The refusal reason, or a panic naming what was decided instead.
    fn denial(decision: &PolicyDecision) -> &str {
        match decision {
            PolicyDecision::Deny { reason, .. } => reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn without_a_policy_section_the_tier_gate_does_not_run() {
        // D6: configuring `[policy]` is the opt-in. With no section, an adapter
        // that classifies nothing is exactly as deployable as it is today.
        assert_eq!(
            tier_gate(None, NO_TIERS, Some(&unclassified_report())),
            PolicyDecision::Allow { changes: 0 }
        );
    }

    #[test]
    fn a_missing_preflight_report_is_denied() {
        // Nothing ran, so nothing looked. This outranks every other cause: an
        // approver would be signing off on a set nobody has seen.
        let decision = tier_gate(Some(&policy()), CLASSIFIES, None);
        assert!(denial(&decision).contains("no preflight report"));
    }

    #[test]
    fn a_missing_preflight_report_is_never_approvable() {
        let decision = tier_gate(Some(&lenient()), CLASSIFIES, None);
        assert!(denial(&decision).contains("no preflight report"));
    }

    #[test]
    fn an_adapter_that_does_not_classify_is_denied() {
        let decision = tier_gate(Some(&policy()), NO_TIERS, Some(&unclassified_report()));
        assert!(denial(&decision).contains("does not advertise `risk_tier`"));
    }

    #[test]
    fn an_adapter_that_does_not_classify_can_require_approval_instead() {
        let decision = tier_gate(Some(&lenient()), NO_TIERS, Some(&unclassified_report()));
        let PolicyDecision::NeedsApproval { reasons } = &decision else {
            panic!("expected an approval request, got {decision:?}");
        };
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0].tier, None);
    }

    #[test]
    fn an_advertised_capability_that_emits_no_change_set_is_denied() {
        // The adapter claimed it classifies and then did not. That is a producer
        // bug, and the refusal says so rather than blaming the operator.
        let decision = tier_gate(Some(&policy()), CLASSIFIES, Some(&unclassified_report()));
        let reason = denial(&decision);
        assert!(reason.contains("advertises `risk_tier`"), "{reason}");
        assert!(reason.contains("no change-set"), "{reason}");
    }

    #[test]
    fn a_change_set_from_a_newer_contract_is_denied_naming_both_versions() {
        let future = ChangeSet::new(Vec::new()).with_contract_version(2);
        let report = PreflightReport::new(true).with_change_set(future);
        let decision = tier_gate(Some(&policy()), CLASSIFIES, Some(&report));
        let reason = denial(&decision);
        assert!(reason.contains('2'), "{reason}");
        assert!(reason.contains('1'), "{reason}");
    }

    #[test]
    fn a_change_set_from_a_newer_contract_is_never_approvable() {
        // We cannot read the payload, so an approver would be signing off on
        // nothing. No `unclassified` setting can turn this into a question.
        let future = ChangeSet::new(Vec::new()).with_contract_version(2);
        let report = PreflightReport::new(true).with_change_set(future);
        let decision = tier_gate(Some(&lenient()), CLASSIFIES, Some(&report));
        assert!(denial(&decision).contains("risk-contract version"));
    }

    #[test]
    fn an_untiered_change_is_denied() {
        let report = classified(vec![
            change("add_column", "public.tb_user.nickname").with_tier(RiskTier::Additive),
            change("entangle_column", "public.tb_user.spin"),
        ]);
        let decision = tier_gate(Some(&policy()), CLASSIFIES, Some(&report));
        let reason = denial(&decision);
        assert!(reason.contains("unclassified"), "{reason}");
        assert!(reason.contains("public.tb_user.spin"), "{reason}");
    }

    #[test]
    fn an_untiered_change_can_require_approval_instead() {
        let report = classified(vec![change("entangle_column", "public.tb_user.spin")]);
        let decision = tier_gate(Some(&lenient()), CLASSIFIES, Some(&report));
        let PolicyDecision::NeedsApproval { reasons } = &decision else {
            panic!("expected an approval request, got {decision:?}");
        };
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0].object, "public.tb_user.spin");
        assert_eq!(reasons[0].tier, None);
    }

    #[test]
    fn an_empty_change_set_is_allowed() {
        // The adapter looked and there is nothing to change. Contrast
        // `an_advertised_capability_that_emits_no_change_set_is_denied`: an empty
        // set and an absent one are different answers, and only one is safe.
        let decision = tier_gate(Some(&policy()), CLASSIFIES, Some(&classified(Vec::new())));
        assert_eq!(decision, PolicyDecision::Allow { changes: 0 });
    }

    #[test]
    fn an_all_auto_apply_change_set_is_allowed() {
        let report = classified(vec![
            change("add_column", "public.tb_user.nickname").with_tier(RiskTier::Additive),
            change("set_default", "public.tb_user.locale").with_tier(RiskTier::Reversible),
        ]);
        let decision = tier_gate(Some(&policy()), CLASSIFIES, Some(&report));
        assert_eq!(decision, PolicyDecision::Allow { changes: 2 });
    }

    #[test]
    fn one_change_in_a_require_approval_tier_needs_approval() {
        let report = classified(vec![
            change("add_column", "public.tb_user.nickname").with_tier(RiskTier::Additive),
            change("drop_column", "public.tb_user.legacy_flag")
                .with_migration("20260804120100")
                .with_tier(RiskTier::Irreversible),
        ]);
        let decision = tier_gate(Some(&policy()), CLASSIFIES, Some(&report));
        let reasons = approval(&decision);
        assert_eq!(reasons.len(), 1, "{reasons:?}");
        assert_eq!(reasons[0].object, "public.tb_user.legacy_flag");
        assert_eq!(reasons[0].tier, Some(RiskTier::Irreversible));
        assert_eq!(reasons[0].migration.as_deref(), Some("20260804120100"));
    }

    #[test]
    fn a_mixed_change_set_carries_every_triggering_change() {
        // Not just the worst one: an operator asked to approve needs the whole
        // picture. The auto-applied change is not a trigger and stays out.
        let report = classified(vec![
            change("add_column", "public.tb_user.nickname").with_tier(RiskTier::Additive),
            change("create_index", "public.ix_user_email").with_tier(RiskTier::LockRisky),
            change("drop_table", "public.tb_legacy").with_tier(RiskTier::Irreversible),
        ]);
        let decision = tier_gate(Some(&policy()), CLASSIFIES, Some(&report));
        let objects: Vec<&str> = approval(&decision)
            .iter()
            .map(|reason| reason.object.as_str())
            .collect();
        assert_eq!(objects, ["public.ix_user_email", "public.tb_legacy"]);
    }

    #[test]
    fn a_tier_absent_from_both_lists_is_denied() {
        // A sixth tier added to the taxonomy later must not silently auto-apply
        // on a config written today.
        let policy = policy_of(&[(RiskTier::Additive, PolicyAction::AutoApply)]);
        let report = classified(vec![
            change("set_default", "public.tb_user.locale").with_tier(RiskTier::Reversible)
        ]);
        let decision = tier_gate(Some(&policy), CLASSIFIES, Some(&report));
        let reason = denial(&decision);
        assert!(reason.contains("public.tb_user.locale"), "{reason}");
        assert!(reason.contains("reversible"), "{reason}");
    }

    #[test]
    fn deny_wins_over_needs_approval_and_still_names_both() {
        let policy = policy_of(&[
            (RiskTier::Additive, PolicyAction::AutoApply),
            (RiskTier::LockRisky, PolicyAction::RequireApproval),
            (RiskTier::Irreversible, PolicyAction::Deny),
        ]);
        let report = classified(vec![
            change("add_column", "public.tb_user.nickname").with_tier(RiskTier::Additive),
            change("create_index", "public.ix_user_email").with_tier(RiskTier::LockRisky),
            change("drop_table", "public.tb_legacy").with_tier(RiskTier::Irreversible),
        ]);
        let decision = tier_gate(Some(&policy), CLASSIFIES, Some(&report));
        let PolicyDecision::Deny { reason, reasons } = &decision else {
            panic!("expected a refusal, got {decision:?}");
        };
        // The message is about what cannot be unblocked...
        assert!(reason.contains("public.tb_legacy"), "{reason}");
        assert!(!reason.contains("public.ix_user_email"), "{reason}");
        // ...and the record still carries everything that was not auto-applied.
        let objects: Vec<&str> = reasons.iter().map(|r| r.object.as_str()).collect();
        assert_eq!(objects, ["public.ix_user_email", "public.tb_legacy"]);
    }

    #[test]
    fn a_refusal_names_at_most_three_changes_and_counts_the_rest() {
        let policy = policy_of(&[]);
        let report = classified(vec![
            change("drop_table", "public.tb_a").with_tier(RiskTier::Irreversible),
            change("drop_table", "public.tb_b").with_tier(RiskTier::Irreversible),
            change("drop_table", "public.tb_c").with_tier(RiskTier::Irreversible),
            change("drop_table", "public.tb_d").with_tier(RiskTier::Irreversible),
        ]);
        let decision = tier_gate(Some(&policy), CLASSIFIES, Some(&report));
        let reason = denial(&decision);
        assert!(reason.contains("and 1 more"), "{reason}");
        assert!(!reason.contains("public.tb_d"), "{reason}");
        // The full list survives where the audit and the plan render read it.
        let PolicyDecision::Deny { reasons, .. } = &decision else {
            unreachable!()
        };
        assert_eq!(reasons.len(), 4);
    }

    #[test]
    fn require_approval_without_a_hook_is_denied_not_approved() {
        // The failure this rule exists to prevent: a policy that asks for
        // sign-off, nothing configured to give it, and the deploy resolving the
        // question by proceeding. `Policy::default()` has no hook.
        let report = classified(vec![
            change("drop_table", "public.tb_legacy").with_tier(RiskTier::Irreversible)
        ]);
        let decision = tier_gate(Some(&Policy::default()), CLASSIFIES, Some(&report));
        let reason = denial(&decision);
        assert!(reason.contains("no approval hook"), "{reason}");
        // The change that needed sign-off is still named.
        let PolicyDecision::Deny { reasons, .. } = &decision else {
            unreachable!()
        };
        assert_eq!(reasons.len(), 1);
        assert_eq!(reasons[0].object, "public.tb_legacy");
    }

    #[test]
    fn an_unclassified_set_needing_approval_without_a_hook_is_denied() {
        // Same rule where no single change is at fault.
        let lenient = Policy::new(
            Policy::default().actions,
            UnclassifiedAction::RequireApproval,
        );
        let decision = tier_gate(Some(&lenient), NO_TIERS, Some(&unclassified_report()));
        assert!(denial(&decision).contains("no approval hook"));
    }

    #[test]
    fn deny_only_policies_do_not_need_a_hook() {
        // A refusal no hook could unblock must not be re-labelled a hook problem.
        let policy =
            policy_of(&[(RiskTier::Irreversible, PolicyAction::Deny)]).with_approval_hook(false);
        let report = classified(vec![
            change("drop_table", "public.tb_legacy").with_tier(RiskTier::Irreversible)
        ]);
        let reason = denial(&tier_gate(Some(&policy), CLASSIFIES, Some(&report))).to_owned();
        assert!(reason.contains("public.tb_legacy"), "{reason}");
        assert!(!reason.contains("hook"), "{reason}");
    }

    #[test]
    fn an_auto_apply_only_deploy_needs_no_hook() {
        let report = classified(vec![
            change("add_column", "public.tb_user.nickname").with_tier(RiskTier::Additive)
        ]);
        let decision = tier_gate(Some(&Policy::default()), CLASSIFIES, Some(&report));
        assert_eq!(decision, PolicyDecision::Allow { changes: 1 });
    }

    // ---------------------------------------------------------------------
    // The baseline: the window-safety rule, folded in here from the gate it
    // replaces. These five rows must reproduce `window_safety::evaluate`'s
    // verdict in meaning, or the replacement has quietly lost coverage.
    // ---------------------------------------------------------------------

    /// A blue-green deploy whose adapter lints but does not classify.
    fn blue_green(window_safe: Option<bool>) -> PreflightReport {
        window_safe.map_or_else(
            || PreflightReport::new(true),
            |safe| PreflightReport::new(true).with_window_safe(safe),
        )
    }

    #[test]
    fn a_window_unsafe_migration_is_denied_with_no_policy_section() {
        // The single most important test in this phase. Replacing the
        // window-safety gate must not delete it for the users who never
        // configure `[policy]` — which is all of them today.
        let decision = evaluate(
            None,
            Baseline::WindowSafety,
            NO_TIERS,
            Some(&blue_green(Some(false))),
        );
        assert!(denial(&decision).contains("window_safe = false"));
    }

    #[test]
    fn a_window_safe_migration_falls_through_to_the_tier_policy() {
        let decision = evaluate(
            None,
            Baseline::WindowSafety,
            NO_TIERS,
            Some(&blue_green(Some(true))),
        );
        assert_eq!(decision, PolicyDecision::Allow { changes: 0 });
    }

    #[test]
    fn no_window_safety_verdict_is_denied() {
        // An older confiture that does not emit `window_safe` cannot certify the
        // window, and "cannot certify" is not "safe".
        let decision = evaluate(
            None,
            Baseline::WindowSafety,
            NO_TIERS,
            Some(&blue_green(None)),
        );
        assert!(denial(&decision).contains("window_safe"));
    }

    #[test]
    fn an_adapter_without_preflight_cannot_certify_the_window() {
        let decision = evaluate(
            None,
            Baseline::WindowSafety,
            Capabilities::new(false, false),
            None,
        );
        assert!(denial(&decision).contains("does not advertise `preflight`"));
    }

    #[test]
    fn no_preflight_report_cannot_certify_the_window() {
        let decision = evaluate(None, Baseline::WindowSafety, NO_TIERS, None);
        assert!(denial(&decision).contains("cannot certify the window"));
    }

    #[test]
    fn window_safety_is_authoritative_over_report_ok() {
        // A non-transactional but forward-compatible migration (CREATE INDEX
        // CONCURRENTLY) can carry `ok == false` and `window_safe == true`. The
        // rule trusts the verdict: transactionality is not a window-safety
        // concern, because blue-green does no DB rollback.
        let tolerated = PreflightReport::new(false).with_window_safe(true);
        assert_eq!(
            evaluate(None, Baseline::WindowSafety, NO_TIERS, Some(&tolerated)),
            PolicyDecision::Allow { changes: 0 }
        );
    }

    #[test]
    fn the_baseline_is_skipped_for_single_host() {
        // Single-host and multi-host have no shared-DB hold window, so they must
        // not inherit blue-green's rule.
        let decision = tier_gate(None, NO_TIERS, Some(&blue_green(Some(false))));
        assert_eq!(decision, PolicyDecision::Allow { changes: 0 });
    }

    #[test]
    fn a_baseline_denial_names_the_window_not_a_tier() {
        // Distinguishable from a tier refusal at a glance: no change is at
        // fault, so no change is named.
        let report = PreflightReport::new(true)
            .with_window_safe(false)
            .with_change_set(ChangeSet::new(vec![change(
                "drop_table",
                "public.tb_legacy",
            )
            .with_tier(RiskTier::Irreversible)]));
        let decision = evaluate(
            Some(&policy()),
            Baseline::WindowSafety,
            CLASSIFIES,
            Some(&report),
        );
        let PolicyDecision::Deny { reason, reasons } = &decision else {
            panic!("expected a refusal, got {decision:?}");
        };
        assert!(reason.contains("two-version window"), "{reason}");
        assert!(!reason.contains("public.tb_legacy"), "{reason}");
        assert!(reasons.is_empty(), "{reasons:?}");
    }

    #[test]
    fn the_baseline_runs_before_the_tier_policy() {
        // A window-unsafe migration whose every change auto-applies is still
        // refused: the two rules answer different questions and the baseline is
        // not something a policy can configure away.
        let report = PreflightReport::new(true)
            .with_window_safe(false)
            .with_change_set(ChangeSet::new(vec![change(
                "add_column",
                "public.tb_user.nickname",
            )
            .with_tier(RiskTier::Additive)]));
        let decision = evaluate(
            Some(&policy()),
            Baseline::WindowSafety,
            CLASSIFIES,
            Some(&report),
        );
        assert!(denial(&decision).contains("window_safe = false"));
    }

    #[test]
    fn deny_beats_needs_approval_beats_allow() {
        // A mixed change-set takes the worst outcome, whatever order it arrives in.
        assert_eq!(
            worst([
                PolicyAction::AutoApply,
                PolicyAction::Deny,
                PolicyAction::RequireApproval,
            ]),
            PolicyAction::Deny,
        );
        assert_eq!(
            worst([PolicyAction::RequireApproval, PolicyAction::AutoApply]),
            PolicyAction::RequireApproval,
        );
        // Nothing to decide on is not a refusal.
        assert_eq!(worst([]), PolicyAction::AutoApply);
    }

    #[test]
    fn the_action_ordering_is_least_to_most_restrictive() {
        // Load-bearing: `worst` is a max over this ordering.
        assert!(PolicyAction::AutoApply < PolicyAction::RequireApproval);
        assert!(PolicyAction::RequireApproval < PolicyAction::Deny);
    }

    #[test]
    fn an_unclassified_change_never_auto_applies() {
        // The type cannot express it; this pins the conversion the engine uses.
        assert_eq!(
            PolicyAction::from(super::UnclassifiedAction::Deny),
            PolicyAction::Deny
        );
        assert_eq!(
            PolicyAction::from(super::UnclassifiedAction::RequireApproval),
            PolicyAction::RequireApproval
        );
    }
}
