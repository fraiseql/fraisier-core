//! Multi-host deploy composition (PRD §5.3) — the rolling rollout over a host
//! inventory.
//!
//! # Two types
//!
//! [`MultiHostPlan`] is the *value object* (an ordered [`HostInventory`] plus a
//! [`RolloutStrategy`]) resolved from `[hosts]` in `fraisier.toml`.
//! [`MultiHostDeploy`] is the *executable*: a plan plus the five adapter axes
//! (artifact, migration, service, health, **and** load-balancer — the LB axis is
//! what makes a rollout multi-host). Build one with [`MultiHostDeploy::builder`]
//! and run it with [`MultiHostDeploy::run`].
//!
//! # Composition, not a second engine
//!
//! The multi-host flow is composed from the same [`fraisier_saga`] primitives as
//! the single-host one — there is no separate state machine. Each phase is a saga
//! [`Step`] with a compensating undo; the saga's reverse-order rollback gives the
//! PRD §5.4 contract (reverse the rollout, then roll the migration back once) for
//! free, and a failed *compensation* surfaces as [`SagaOutcome::PartialRollback`].
//!
//! ```text
//! Idle → preflight → fetch → migrate → rollout(batch…) → verify → Committed
//! ```
//!
//! Migration runs **once** against the shared database (not per host). The
//! per-host work (drain → activate → restart → health → reattach) happens in the
//! rollout phase, in strategy order. A *batch* is one saga step: its hosts advance
//! concurrently, and batches run in order so the rest of the fleet stays live
//! (PRD §5.5). Modelling each batch as a step lets the saga reverse the rollout in
//! batch order on failure, exactly as the contract requires.
//!
//! # The release ledger
//!
//! Like the single-host composition, every committed deploy records what it made
//! live — here a [`MultiHostRecord`] mapping each host to its active
//! [`StagedArtifact`] plus the migration revision. A later deploy reads that
//! record as its rollback target: restoring a host means re-activating *its* prior
//! artifact. A failed first-ever deploy has no such record, so restoring a host it
//! had already activated is impossible and surfaces as
//! [`SagaOutcome::PartialRollback`].
//!
//! # Partial rollback — operator escalation
//!
//! A clean rollback returns [`SagaOutcome::RolledBack`]: every host is back on its
//! prior artifact and the database is back on its prior revision. When a *rollback*
//! step itself fails — a host that cannot be re-activated or restarted, or a
//! first-ever deploy with no prior release to fall back to — the run returns
//! [`SagaOutcome::PartialRollback`] with a human-readable reason naming the host
//! and the operation that failed. fraisier never reports this as success. The
//! fleet is then in a mixed state and needs an operator: read the reason, inspect
//! the named host's active artifact and service status (`deployment-status
//! --per-host`), and either finish restoring it by hand or re-run the deploy once
//! the underlying fault (unreachable host, wedged unit) is cleared. The migration
//! is rolled back only after every host is restored, so a `PartialRollback`
//! surfaced during the host phase means the database may still be on the new
//! revision — check it before retrying.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use fraisier_saga::saga::{Saga, SagaError, SagaOutcome, Step, StepContext};
use fraisier_saga::state_store::{FraiseKey, StateStore, StateStoreError};
use serde_json::Value;

use crate::adapter_axes::{
    AdapterCtx, AdapterError, ArtifactAdapter, HealthAdapter, HostId, LbAdapter, LbMembership,
    MigrationAdapter, Revision, ServiceAdapter, Severity, StagedArtifact,
};

/// One host in a multi-host deploy's inventory.
///
/// `overrides` reserves per-host adapter-axis configuration (a canary host on a
/// different artifact source, an LB segment with different drain semantics, …).
/// Phase 1 only reserves the field; Phase 4 interprets it.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::HostId;
/// # use fraisier_core::multi_host::HostEntry;
/// let entry = HostEntry::new(HostId::new("web-1"), "web1.internal");
/// assert!(entry.overrides.is_empty());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HostEntry {
    /// The host's inventory name.
    pub host: HostId,
    /// The address fraisier reaches it at (hostname or IP).
    pub address: String,
    /// Per-host partial adapter config, merged over the deploy-wide config in
    /// Phase 4. Keyed by axis name (`"artifact"`, `"lb"`, …).
    #[serde(default)]
    pub overrides: BTreeMap<String, serde_json::Value>,
}

impl HostEntry {
    /// Create an inventory entry with no overrides.
    #[must_use]
    pub fn new(host: HostId, address: impl Into<String>) -> Self {
        Self {
            host,
            address: address.into(),
            overrides: BTreeMap::new(),
        }
    }
}

/// The ordered set of hosts a deploy targets.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::HostId;
/// # use fraisier_core::multi_host::{HostEntry, HostInventory};
/// let inv = HostInventory::new()
///     .with_host(HostEntry::new(HostId::new("web-1"), "web1.internal"));
/// assert_eq!(inv.hosts().len(), 1);
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HostInventory {
    hosts: Vec<HostEntry>,
}

impl HostInventory {
    /// An empty inventory.
    #[must_use]
    pub const fn new() -> Self {
        Self { hosts: Vec::new() }
    }

    /// Append a host (builder style).
    #[must_use]
    pub fn with_host(mut self, host: HostEntry) -> Self {
        self.hosts.push(host);
        self
    }

    /// The hosts, in rollout order.
    #[must_use]
    pub fn hosts(&self) -> &[HostEntry] {
        &self.hosts
    }

    /// Whether the inventory is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }
}

/// How hosts are advanced through a rollout (PRD §5.5).
///
/// `#[non_exhaustive]`: `BlueGreen` (v1.0.0 GA) and `Canary` (later) will be
/// added without it being a breaking change.
///
/// # Example
/// ```
/// # use fraisier_core::multi_host::RolloutStrategy;
/// assert!(matches!(RolloutStrategy::Rolling(2), RolloutStrategy::Rolling(2)));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RolloutStrategy {
    /// Every host updated in parallel; brief full downtime tolerated.
    AllAtOnce,
    /// Process this many hosts at a time; the rest stay live.
    Rolling(usize),
}

/// A multi-host deploy plan: an inventory plus a rollout strategy.
///
/// This is the value object resolved from `[hosts]`. To *execute* it, hand it to
/// [`MultiHostDeploy::builder`] together with the adapter axes.
///
/// # Example
/// ```
/// # use fraisier_core::adapter_axes::HostId;
/// # use fraisier_core::multi_host::{HostEntry, HostInventory, MultiHostPlan, RolloutStrategy};
/// let inv = HostInventory::new().with_host(HostEntry::new(HostId::new("web-1"), "web1.internal"));
/// let plan = MultiHostPlan::new(inv, RolloutStrategy::Rolling(1));
/// assert_eq!(plan.inventory().hosts().len(), 1);
/// ```
#[derive(Debug, Clone)]
pub struct MultiHostPlan {
    inventory: HostInventory,
    strategy: RolloutStrategy,
}

impl MultiHostPlan {
    /// Build a plan from an inventory and a strategy.
    #[must_use]
    pub const fn new(inventory: HostInventory, strategy: RolloutStrategy) -> Self {
        Self {
            inventory,
            strategy,
        }
    }

    /// The host inventory.
    #[must_use]
    pub const fn inventory(&self) -> &HostInventory {
        &self.inventory
    }

    /// The rollout strategy.
    #[must_use]
    pub const fn strategy(&self) -> RolloutStrategy {
        self.strategy
    }
}

/// The durable record of what a committed multi-host deploy made live.
///
/// This is the rollback target a *later* deploy reads back from the state store's
/// snapshot slot — the multi-host analogue of the single-host `DeployRecord`, but
/// keyed per host because each host carries its own active artifact.
///
/// # Example
/// ```
/// # use fraisier_core::multi_host::MultiHostRecord;
/// let record = MultiHostRecord::default();
/// assert!(record.active.is_empty() && record.revision.is_none());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MultiHostRecord {
    /// The artifact left active on each host, kept whole (with its path) so a
    /// future deploy can re-activate it on rollback.
    pub active: BTreeMap<HostId, StagedArtifact>,
    /// The migration revision live after the deploy committed.
    pub revision: Option<Revision>,
}

/// Errors from running a [`MultiHostDeploy`].
///
/// As with the single-host deploy, a *business* failure that rolls back cleanly
/// is **not** an error — it is a successful [`SagaOutcome::RolledBack`]. This type
/// is only for infrastructure failures (the saga/engine or the state store).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MultiHostError {
    /// The saga engine reported an infrastructure failure (locking/persistence).
    #[error(transparent)]
    Saga(#[from] SagaError),
    /// The state store failed while reading or writing the release ledger.
    #[error(transparent)]
    Store(#[from] StateStoreError),
    /// The release ledger could not be (de)serialized.
    #[error("multi-host deploy ledger (de)serialization failed: {0}")]
    Ledger(#[from] serde_json::Error),
}

/// A [`MultiHostDeployBuilder`] was missing a required input.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MultiHostBuildError {
    /// The named adapter axis was not set on the builder.
    #[error("multi-host deploy requires a(n) {0} adapter")]
    MissingAdapter(&'static str),
    /// The plan's inventory had no hosts.
    #[error("multi-host deploy requires a non-empty [hosts] inventory")]
    EmptyInventory,
}

/// A configured multi-host deploy. Build one with [`MultiHostDeploy::builder`]
/// and execute it with [`MultiHostDeploy::run`].
pub struct MultiHostDeploy {
    fraise: String,
    environment: String,
    plan: MultiHostPlan,
    ctx: AdapterCtx,
    target: Option<Revision>,
    forward_compatible_lint: bool,
    artifact: Arc<dyn ArtifactAdapter>,
    migration: Arc<dyn MigrationAdapter>,
    service: Arc<dyn ServiceAdapter>,
    health: Arc<dyn HealthAdapter>,
    lb: Arc<dyn LbAdapter>,
}

impl std::fmt::Debug for MultiHostDeploy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The adapters are trait objects (not `Debug`); show the identity instead.
        f.debug_struct("MultiHostDeploy")
            .field("fraise", &self.fraise)
            .field("environment", &self.environment)
            .field("plan", &self.plan)
            .field("target", &self.target)
            .field("forward_compatible_lint", &self.forward_compatible_lint)
            .finish_non_exhaustive()
    }
}

impl MultiHostDeploy {
    /// Start building a multi-host deploy for `fraise`/`environment` over `plan`.
    #[must_use]
    pub fn builder(
        fraise: impl Into<String>,
        environment: impl Into<String>,
        plan: MultiHostPlan,
    ) -> MultiHostDeployBuilder {
        MultiHostDeployBuilder::new(fraise, environment, plan)
    }

    /// Run the multi-host deploy over `store`, returning how the saga ended.
    ///
    /// Reads the prior [`MultiHostRecord`] (the rollback target) before running
    /// and, on [`SagaOutcome::Committed`], records the new one. The store is cloned
    /// into the saga, so it must be `Clone` (both shipped backends are).
    ///
    /// # Errors
    /// [`MultiHostError`] for infrastructure failures (engine, state store, or
    /// ledger (de)serialization). A clean rollback is `Ok(SagaOutcome::RolledBack)`,
    /// and an unrecoverable one is `Ok(SagaOutcome::PartialRollback)`.
    pub async fn run<S: StateStore + Clone>(
        &self,
        store: S,
    ) -> Result<SagaOutcome, MultiHostError> {
        let key = FraiseKey::new(self.fraise.clone(), self.environment.clone());

        let prior = match store.current_snapshot(&key).await? {
            Some(value) => serde_json::from_value(value)?,
            None => MultiHostRecord::default(),
        };

        let shared = Arc::new(RolloutShared::new(self, prior));
        // Compose the fixed phases plus one rollout step per batch as a single
        // dynamically built list (Saga::with_steps).
        let mut steps: Vec<Box<dyn Step<RolloutRuntime>>> = vec![
            shared.step(StepKind::Preflight),
            shared.step(StepKind::Fetch),
            shared.step(StepKind::Migrate),
        ];
        for (index, batch) in plan_batches(self.plan.strategy(), self.plan.inventory().hosts())
            .into_iter()
            .enumerate()
        {
            steps.push(shared.rollout_step(index + 1, batch));
        }
        steps.push(shared.step(StepKind::Verify));
        let saga = Saga::new(store.clone(), self.fraise.clone(), self.environment.clone())
            .with_steps(steps);

        let mut runtime = RolloutRuntime::default();
        let saga_outcome = saga.run_with_state(&mut runtime).await?;
        let outcome = runtime.finalize(saga_outcome);

        if matches!(outcome, SagaOutcome::Committed) {
            let record = runtime.committed_record();
            store
                .record_snapshot(&key, &serde_json::to_value(&record)?)
                .await?;
        }
        Ok(outcome)
    }
}

/// Split the inventory into the batches a strategy advances together: one batch of
/// every host for [`RolloutStrategy::AllAtOnce`]; consecutive chunks of `n`
/// (at least one) for [`RolloutStrategy::Rolling`].
fn plan_batches(strategy: RolloutStrategy, inventory: &[HostEntry]) -> Vec<Vec<HostEntry>> {
    match strategy {
        RolloutStrategy::AllAtOnce => vec![inventory.to_vec()],
        RolloutStrategy::Rolling(size) => inventory
            .chunks(size.max(1))
            .map(<[HostEntry]>::to_vec)
            .collect(),
    }
}

/// Builder for [`MultiHostDeploy`]. All five adapters are required; the
/// [`AdapterCtx`] and migration `target` are optional.
pub struct MultiHostDeployBuilder {
    fraise: String,
    environment: String,
    plan: MultiHostPlan,
    ctx: Option<AdapterCtx>,
    target: Option<Revision>,
    forward_compatible_lint: bool,
    artifact: Option<Arc<dyn ArtifactAdapter>>,
    migration: Option<Arc<dyn MigrationAdapter>>,
    service: Option<Arc<dyn ServiceAdapter>>,
    health: Option<Arc<dyn HealthAdapter>>,
    lb: Option<Arc<dyn LbAdapter>>,
}

impl MultiHostDeployBuilder {
    fn new(fraise: impl Into<String>, environment: impl Into<String>, plan: MultiHostPlan) -> Self {
        Self {
            fraise: fraise.into(),
            environment: environment.into(),
            plan,
            ctx: None,
            target: None,
            // Default on: the forward-compat preflight runs whenever the adapter
            // advertises it, unless the operator opts out (PRD G11 / Decision 4).
            forward_compatible_lint: true,
            artifact: None,
            migration: None,
            service: None,
            health: None,
            lb: None,
        }
    }

    /// Provide the [`AdapterCtx`] passed to every adapter call. When omitted, a
    /// default context for the `(fraise, environment)` pair is used. The per-host
    /// fields (`host`, the `address` setting) are filled in per call.
    #[must_use]
    pub fn context(mut self, ctx: AdapterCtx) -> Self {
        self.ctx = Some(ctx);
        self
    }

    /// Migrate only up to `target` instead of applying every pending migration.
    #[must_use]
    pub fn target(mut self, target: Revision) -> Self {
        self.target = Some(target);
        self
    }

    /// Whether to run the migration adapter's forward-compatibility `preflight`
    /// lint before deploying (default `true`).
    #[must_use]
    pub const fn forward_compatible_lint(mut self, enabled: bool) -> Self {
        self.forward_compatible_lint = enabled;
        self
    }

    /// Set the artifact adapter (required).
    #[must_use]
    pub fn artifact(mut self, artifact: Arc<dyn ArtifactAdapter>) -> Self {
        self.artifact = Some(artifact);
        self
    }

    /// Set the migration adapter (required).
    #[must_use]
    pub fn migration(mut self, migration: Arc<dyn MigrationAdapter>) -> Self {
        self.migration = Some(migration);
        self
    }

    /// Set the service adapter (required).
    #[must_use]
    pub fn service(mut self, service: Arc<dyn ServiceAdapter>) -> Self {
        self.service = Some(service);
        self
    }

    /// Set the health adapter (required).
    #[must_use]
    pub fn health(mut self, health: Arc<dyn HealthAdapter>) -> Self {
        self.health = Some(health);
        self
    }

    /// Set the load-balancer adapter (required for multi-host).
    #[must_use]
    pub fn lb(mut self, lb: Arc<dyn LbAdapter>) -> Self {
        self.lb = Some(lb);
        self
    }

    /// Finish building.
    ///
    /// # Errors
    /// [`MultiHostBuildError`] if the inventory is empty or a required adapter was
    /// not set.
    pub fn build(self) -> Result<MultiHostDeploy, MultiHostBuildError> {
        if self.plan.inventory().is_empty() {
            return Err(MultiHostBuildError::EmptyInventory);
        }
        let ctx = self
            .ctx
            .unwrap_or_else(|| AdapterCtx::new(self.fraise.clone(), self.environment.clone()));
        Ok(MultiHostDeploy {
            artifact: self
                .artifact
                .ok_or(MultiHostBuildError::MissingAdapter("artifact"))?,
            migration: self
                .migration
                .ok_or(MultiHostBuildError::MissingAdapter("migration"))?,
            service: self
                .service
                .ok_or(MultiHostBuildError::MissingAdapter("service"))?,
            health: self
                .health
                .ok_or(MultiHostBuildError::MissingAdapter("health"))?,
            lb: self.lb.ok_or(MultiHostBuildError::MissingAdapter("lb"))?,
            fraise: self.fraise,
            environment: self.environment,
            plan: self.plan,
            ctx,
            target: self.target,
            forward_compatible_lint: self.forward_compatible_lint,
        })
    }
}

/// The rollout state shared by every step: the adapters, the base call context,
/// the inventory, and the durable rollback target (`prior`). All fields are
/// immutable for the run — the mutable in-run captures live in [`RolloutRuntime`],
/// which the saga engine threads to each step as `&mut`.
struct RolloutShared {
    ctx: AdapterCtx,
    inventory: Vec<HostEntry>,
    target: Option<Revision>,
    forward_compatible_lint: bool,
    artifact: Arc<dyn ArtifactAdapter>,
    migration: Arc<dyn MigrationAdapter>,
    service: Arc<dyn ServiceAdapter>,
    health: Arc<dyn HealthAdapter>,
    lb: Arc<dyn LbAdapter>,
    /// The previously-committed release — the per-host rollback target. Immutable
    /// for the run.
    prior: MultiHostRecord,
}

/// The mutable run state captured while a multi-host deploy runs forward, threaded
/// to every step by the saga engine as `&mut RolloutRuntime`. Within the rollout
/// step, hosts advance concurrently into **disjoint** `progress` slots (no lock):
/// each task fills its own host's [`HostProgress`], merged into this map after the
/// batch completes.
#[derive(Default)]
struct RolloutRuntime {
    /// The artifact staged on each host by `fetch` and activated by the rollout.
    staged: BTreeMap<HostId, StagedArtifact>,
    /// The revision live immediately before `migrate` ran `up` — the target
    /// `down_to` returns the database to on rollback.
    previous_revision: Option<Revision>,
    /// The revision live after `up` — recorded into the new ledger on commit.
    new_revision: Option<Revision>,
    /// How far each host advanced through the rollout, so a restore touches only
    /// the steps that actually happened.
    progress: BTreeMap<HostId, HostProgress>,
    /// Set when a *rollback* step could not fully restore a host: the run then
    /// reports [`SagaOutcome::PartialRollback`] rather than a clean rollback.
    unrecoverable: Option<String>,
}

impl RolloutRuntime {
    /// Map the saga outcome onto the run's own findings: a clean rollback that left
    /// at least one host unrecoverable is reported as a [`SagaOutcome::PartialRollback`].
    fn finalize(&mut self, outcome: SagaOutcome) -> SagaOutcome {
        match (outcome, self.unrecoverable.take()) {
            (
                SagaOutcome::RolledBack {
                    failed_step,
                    reason,
                },
                Some(detail),
            ) => SagaOutcome::PartialRollback {
                reason: format!(
                    "rollback after '{failed_step}' failed left a host unrecoverable: {detail} \
                         (original failure: {reason})"
                ),
            },
            (other, _) => other,
        }
    }

    /// The ledger entry to persist on a successful commit: the artifact now active
    /// on each host (the staged one), plus the new migration revision.
    fn committed_record(&self) -> MultiHostRecord {
        MultiHostRecord {
            active: self.staged.clone(),
            revision: self.new_revision.clone(),
        }
    }
}

/// How far one host got through `drain → activate → restart → health → reattach`.
#[derive(Default, Clone)]
struct HostProgress {
    /// The LB membership captured when the host was drained (`Some` once drained).
    drained: Option<LbMembership>,
    /// Whether the new artifact was activated on this host.
    activated: bool,
    /// Whether the host was reattached (fully advanced, back in the pool).
    reattached: bool,
}

/// Which deploy phase a [`RolloutStep`] represents.
enum StepKind {
    Preflight,
    Fetch,
    Migrate,
    /// A rollout batch: a stable step name plus the hosts it advances together.
    Rollout {
        name: String,
        batch: Vec<HostEntry>,
    },
    Verify,
}

impl StepKind {
    /// The stable step name used in saga state, events, and spans.
    fn step_name(&self) -> &str {
        match self {
            Self::Preflight => "preflight",
            Self::Fetch => "fetch",
            Self::Migrate => "migrate",
            Self::Rollout { name, .. } => name,
            Self::Verify => "verify",
        }
    }
}

impl RolloutShared {
    fn new(deploy: &MultiHostDeploy, prior: MultiHostRecord) -> Self {
        Self {
            ctx: deploy.ctx.clone(),
            inventory: deploy.plan.inventory().hosts().to_vec(),
            target: deploy.target.clone(),
            forward_compatible_lint: deploy.forward_compatible_lint,
            artifact: Arc::clone(&deploy.artifact),
            migration: Arc::clone(&deploy.migration),
            service: Arc::clone(&deploy.service),
            health: Arc::clone(&deploy.health),
            lb: Arc::clone(&deploy.lb),
            prior,
        }
    }

    fn step(self: &Arc<Self>, kind: StepKind) -> Box<dyn Step<RolloutRuntime>> {
        Box::new(RolloutStep {
            shared: Arc::clone(self),
            kind,
        })
    }

    fn rollout_step(
        self: &Arc<Self>,
        index: usize,
        batch: Vec<HostEntry>,
    ) -> Box<dyn Step<RolloutRuntime>> {
        self.step(StepKind::Rollout {
            name: format!("rollout-{index}"),
            batch,
        })
    }

    /// Map an adapter error into a saga step failure, preserving its detail.
    fn failed(step: &str, error: &AdapterError) -> SagaError {
        SagaError::StepFailed {
            step: step.to_owned(),
            message: error.to_string(),
        }
    }

    /// A rollout-phase failure tagged with the host and the operation that failed,
    /// so an aggregated batch error names exactly which host broke and where.
    fn host_failed(host: &HostId, operation: &str, error: &AdapterError) -> SagaError {
        SagaError::StepFailed {
            step: "rollout".to_owned(),
            message: format!("host {host} {operation}: {error}"),
        }
    }

    /// The per-host call context: the base context with `host` set and the host's
    /// `address` exposed as a setting, so address-keyed adapters (the LB, and the
    /// future SSH transport) can resolve which host this call targets.
    fn host_ctx(&self, entry: &HostEntry) -> AdapterCtx {
        let mut ctx = self.ctx.clone();
        ctx.host = Some(entry.host.clone());
        ctx.settings
            .insert("address".to_owned(), Value::String(entry.address.clone()));
        ctx
    }

    /// The migration forward-compatibility lint, gated exactly like the single-host
    /// composition: skipped when opted out, and only invoked when the adapter
    /// advertises the `preflight` capability.
    async fn run_forward_compat_lint(&self) -> Result<(), SagaError> {
        if !self.forward_compatible_lint {
            return Ok(());
        }
        let described = self
            .migration
            .describe()
            .await
            .map_err(|e| Self::failed("preflight", &e))?;
        if !described.capabilities.iter().any(|c| c == "preflight") {
            return Ok(());
        }
        let report = self
            .migration
            .preflight(&self.ctx)
            .await
            .map_err(|e| Self::failed("preflight", &e))?;
        let blocking = report
            .issues
            .iter()
            .filter(|issue| issue.severity == Severity::Error)
            .count();
        if blocking > 0 {
            return Err(SagaError::StepFailed {
                step: "preflight".to_owned(),
                message: format!(
                    "forward-compatibility preflight found {blocking} blocking issue(s)"
                ),
            });
        }
        Ok(())
    }

    /// Preflight: the shared forward-compat lint, then a reachability probe of
    /// **every** host in parallel. Reports all unreachable hosts at once.
    async fn run_preflight(&self) -> Result<(), SagaError> {
        self.run_forward_compat_lint().await?;

        let probes = self.inventory.iter().map(|entry| {
            let ctx = self.host_ctx(entry);
            async move {
                self.service
                    .status(&ctx, &entry.host)
                    .await
                    .map(|_| ())
                    .map_err(|e| format!("{}: {e}", entry.host))
            }
        });
        let unreachable: Vec<String> = futures::future::join_all(probes)
            .await
            .into_iter()
            .filter_map(Result::err)
            .collect();

        if unreachable.is_empty() {
            return Ok(());
        }
        Err(SagaError::StepFailed {
            step: "preflight".to_owned(),
            message: format!(
                "{} host(s) unreachable: {}",
                unreachable.len(),
                unreachable.join("; ")
            ),
        })
    }

    /// Fetch: stage the artifact on every host in parallel, recording each staged
    /// artifact for the rollout phase. Reports all hosts that failed to stage.
    async fn run_fetch(&self, runtime: &mut RolloutRuntime) -> Result<(), SagaError> {
        let pending = self.inventory.iter().map(|entry| {
            let ctx = self.host_ctx(entry);
            async move {
                self.artifact
                    .stage(&ctx, &entry.host)
                    .await
                    .map(|artifact| (entry.host.clone(), artifact))
                    .map_err(|e| format!("{}: {e}", entry.host))
            }
        });
        let results = futures::future::join_all(pending).await;

        let mut staged = BTreeMap::new();
        let mut failures = Vec::new();
        for result in results {
            match result {
                Ok((host, artifact)) => {
                    staged.insert(host, artifact);
                }
                Err(message) => failures.push(message),
            }
        }
        if !failures.is_empty() {
            return Err(SagaError::StepFailed {
                step: "fetch".to_owned(),
                message: format!(
                    "{} host(s) failed to stage: {}",
                    failures.len(),
                    failures.join("; ")
                ),
            });
        }
        runtime.staged = staged;
        Ok(())
    }

    /// Migrate **once** against the shared database. Captures the live revision
    /// before `up` so a rollback has an exact `down_to` target.
    async fn run_migrate(&self, runtime: &mut RolloutRuntime) -> Result<(), SagaError> {
        let previous = self
            .migration
            .current_revision(&self.ctx)
            .await
            .map_err(|e| Self::failed("migrate", &e))?;
        runtime.previous_revision = previous;

        let outcome = self
            .migration
            .up(&self.ctx, self.target.clone())
            .await
            .map_err(|e| Self::failed("migrate", &e))?;

        runtime.new_revision = outcome.to.or_else(|| runtime.previous_revision.clone());
        Ok(())
    }

    /// Roll the single shared-database migration back to the pre-deploy revision.
    async fn undo_migrate(&self, runtime: &RolloutRuntime) -> Result<(), SagaError> {
        match runtime.previous_revision.clone() {
            Some(target) => self
                .migration
                .down_to(&self.ctx, target)
                .await
                .map(|_| ())
                .map_err(|e| Self::failed("migrate", &e)),
            None => Err(SagaError::StepFailed {
                step: "migrate".to_owned(),
                message: "cannot roll back the initial migration: there is no prior revision to \
                          return the database to (a failed first-ever deploy has no committed \
                          state to restore)"
                    .to_owned(),
            }),
        }
    }

    /// `VerifyGlobal` (PRD §5.3): once every host has advanced and passed its local
    /// health check, confirm the deploy as a whole. Two gates, in order:
    ///
    /// 1. the migration adapter's post-apply `verify` (its SQL hooks), run **once**
    ///    against the shared database; and
    /// 2. a fleet smoke test — re-probe every host's health in parallel, now that
    ///    the *entire* fleet is on the new release. This catches a regression that
    ///    only shows once all hosts have advanced (which the per-host rollout checks
    ///    cannot see). Point the health adapter's URL at the load balancer to make
    ///    this an across-LB check.
    ///
    /// A failure in either gate fails the step, so the saga rolls the whole deploy
    /// back exactly as a host health failure would.
    async fn run_verify(&self) -> Result<(), SagaError> {
        let report = self
            .migration
            .verify(&self.ctx)
            .await
            .map_err(|e| Self::failed("verify", &e))?;
        if !report.ok {
            let failed = report.checks.iter().filter(|check| !check.ok).count();
            return Err(SagaError::StepFailed {
                step: "verify".to_owned(),
                message: format!("post-migration verify failed {failed} check(s)"),
            });
        }

        let probes = self.inventory.iter().map(|entry| {
            let ctx = self.host_ctx(entry);
            async move {
                match self.health.check(&ctx, &entry.host).await {
                    Ok(status) if status.healthy => Ok(()),
                    Ok(status) => {
                        let detail = status.detail.map(|d| format!(" ({d})")).unwrap_or_default();
                        Err(format!("{} unhealthy{detail}", entry.host))
                    }
                    Err(e) => Err(format!("{}: {e}", entry.host)),
                }
            }
        });
        let unhealthy: Vec<String> = futures::future::join_all(probes)
            .await
            .into_iter()
            .filter_map(Result::err)
            .collect();
        if unhealthy.is_empty() {
            return Ok(());
        }
        Err(SagaError::StepFailed {
            step: "verify".to_owned(),
            message: format!(
                "global verify found {} unhealthy host(s): {}",
                unhealthy.len(),
                unhealthy.join("; ")
            ),
        })
    }

    /// Advance one batch: every host through `drain → activate → restart → health
    /// → reattach`, concurrently. If any host fails, this batch's own hosts are
    /// restored before the failure is surfaced (the saga then rolls back the
    /// earlier batches and the migration); a restore that itself fails is recorded
    /// as unrecoverable.
    async fn run_rollout(
        &self,
        batch: &[HostEntry],
        runtime: &mut RolloutRuntime,
    ) -> Result<(), SagaError> {
        // Each host advances concurrently, but into its **own** progress slot
        // (disjoint `&mut`, so no lock); the shared `staged` map is read-only here.
        let mut progresses: Vec<(HostId, HostProgress)> = batch
            .iter()
            .map(|entry| (entry.host.clone(), HostProgress::default()))
            .collect();
        let advances = batch
            .iter()
            .zip(progresses.iter_mut())
            .map(|(entry, (_, progress))| self.advance_host(entry, &runtime.staged, progress));
        let errors: Vec<String> = futures::future::join_all(advances)
            .await
            .into_iter()
            .filter_map(Result::err)
            .map(|e| e.to_string())
            .collect();

        // Merge how far each host got into the run state, so a restore touches
        // exactly the steps that actually happened (this runs after the concurrent
        // advances, so the `&mut runtime` borrow no longer overlaps them).
        for (host, progress) in progresses {
            runtime.progress.insert(host, progress);
        }

        if errors.is_empty() {
            return Ok(());
        }
        let restore = self.restore_batch(batch, runtime).await;
        if let Err(restore_err) = restore {
            runtime
                .unrecoverable
                .get_or_insert_with(|| restore_err.to_string());
        }
        Err(SagaError::StepFailed {
            step: "rollout".to_owned(),
            message: format!(
                "{} host(s) failed to advance: {}",
                errors.len(),
                errors.join("; ")
            ),
        })
    }

    /// Advance a single host through the rollout sequence, recording how far it got
    /// into its own `progress` slot so a later restore can undo exactly those steps.
    async fn advance_host(
        &self,
        entry: &HostEntry,
        staged: &BTreeMap<HostId, StagedArtifact>,
        progress: &mut HostProgress,
    ) -> Result<(), SagaError> {
        let ctx = self.host_ctx(entry);
        let host = &entry.host;

        let membership = self
            .lb
            .drain(&ctx, host)
            .await
            .map_err(|e| Self::host_failed(host, "drain", &e))?;
        progress.drained = Some(membership.clone());

        let staged_artifact = staged
            .get(host)
            .cloned()
            .ok_or_else(|| SagaError::StepFailed {
                step: "rollout".to_owned(),
                message: format!("no staged artifact recorded for host {host}"),
            })?;
        self.artifact
            .activate(&ctx, host, &staged_artifact)
            .await
            .map_err(|e| Self::host_failed(host, "activate", &e))?;
        progress.activated = true;

        self.service
            .restart(&ctx, host)
            .await
            .map_err(|e| Self::host_failed(host, "restart", &e))?;

        let status = self
            .health
            .check(&ctx, host)
            .await
            .map_err(|e| Self::host_failed(host, "health", &e))?;
        if !status.healthy {
            let detail = status.detail.map(|d| format!(": {d}")).unwrap_or_default();
            return Err(SagaError::StepFailed {
                step: "rollout".to_owned(),
                message: format!("host {host} reported unhealthy after restart{detail}"),
            });
        }

        self.lb
            .reattach(&ctx, host, &membership)
            .await
            .map_err(|e| Self::host_failed(host, "reattach", &e))?;
        progress.reattached = true;
        Ok(())
    }

    /// Restore every host in a batch concurrently, reporting all that could not be
    /// fully restored.
    async fn restore_batch(
        &self,
        batch: &[HostEntry],
        runtime: &RolloutRuntime,
    ) -> Result<(), SagaError> {
        let restores = batch.iter().map(|entry| self.restore_host(entry, runtime));
        let errors: Vec<String> = futures::future::join_all(restores)
            .await
            .into_iter()
            .filter_map(Result::err)
            .map(|e| e.to_string())
            .collect();
        if errors.is_empty() {
            return Ok(());
        }
        Err(SagaError::StepFailed {
            step: "rollout".to_owned(),
            message: format!(
                "{} host(s) could not be restored: {}",
                errors.len(),
                errors.join("; ")
            ),
        })
    }

    /// Restore one host to its pre-deploy state: take it back out of the pool if it
    /// had been reattached, re-activate the prior artifact + restart if the new one
    /// had been activated, then restore the LB membership captured at drain. A host
    /// that was never drained was never touched, so nothing is undone.
    async fn restore_host(
        &self,
        entry: &HostEntry,
        runtime: &RolloutRuntime,
    ) -> Result<(), SagaError> {
        let ctx = self.host_ctx(entry);
        let host = &entry.host;
        let progress = runtime.progress.get(host).cloned().unwrap_or_default();
        let Some(prior_membership) = progress.drained else {
            return Ok(());
        };

        if progress.reattached {
            self.lb
                .drain(&ctx, host)
                .await
                .map_err(|e| Self::host_failed(host, "rollback drain", &e))?;
        }

        if progress.activated {
            let prior_artifact =
                self.prior
                    .active
                    .get(host)
                    .cloned()
                    .ok_or_else(|| SagaError::StepFailed {
                        step: "rollout".to_owned(),
                        message: format!(
                            "cannot restore host {host}: no prior artifact is recorded (a failed \
                         first-ever deploy has no prior release to re-activate)"
                        ),
                    })?;
            self.artifact
                .activate(&ctx, host, &prior_artifact)
                .await
                .map_err(|e| Self::host_failed(host, "rollback activate", &e))?;
            self.service
                .restart(&ctx, host)
                .await
                .map_err(|e| Self::host_failed(host, "rollback restart", &e))?;
        }

        self.lb
            .reattach(&ctx, host, &prior_membership)
            .await
            .map_err(|e| Self::host_failed(host, "rollback reattach", &e))
    }
}

/// One deploy phase: a thin adapter from a [`StepKind`] to the shared rollout
/// logic.
struct RolloutStep {
    shared: Arc<RolloutShared>,
    kind: StepKind,
}

#[async_trait]
impl Step<RolloutRuntime> for RolloutStep {
    fn name(&self) -> &str {
        self.kind.step_name()
    }

    async fn forward(
        &self,
        _ctx: &StepContext,
        runtime: &mut RolloutRuntime,
    ) -> Result<(), SagaError> {
        match &self.kind {
            StepKind::Preflight => self.shared.run_preflight().await,
            StepKind::Fetch => self.shared.run_fetch(runtime).await,
            StepKind::Migrate => self.shared.run_migrate(runtime).await,
            StepKind::Rollout { batch, .. } => self.shared.run_rollout(batch, runtime).await,
            StepKind::Verify => self.shared.run_verify().await,
        }
    }

    async fn compensate(
        &self,
        _ctx: &StepContext,
        runtime: &mut RolloutRuntime,
    ) -> Result<(), SagaError> {
        match &self.kind {
            // Preflight, fetch, and verify are non-mutating (a reachability probe,
            // an artifact staged but never activated, a read-only smoke test), so
            // they have nothing to undo.
            StepKind::Preflight | StepKind::Fetch | StepKind::Verify => Ok(()),
            StepKind::Migrate => self.shared.undo_migrate(runtime).await,
            StepKind::Rollout { batch, .. } => self.shared.restore_batch(batch, runtime).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        plan_batches, HostEntry, HostInventory, MultiHostBuildError, MultiHostDeploy,
        MultiHostPlan, MultiHostRecord, RolloutStrategy,
    };
    use crate::adapter_axes::{
        AdapterCtx, AdapterDescription, AdapterError, AdapterErrorKind, ArtifactAdapter,
        ArtifactRef, HealthAdapter, HealthStatus, HostId, LbAdapter, LbMembership, LbState,
        MigrationAdapter, MigrationOutcome, PreflightIssue, PreflightReport, Revision,
        ServiceAdapter, ServiceStatus, Severity, StagedArtifact, VerifyCheck, VerifyReport,
    };
    use async_trait::async_trait;
    use fraisier_saga::saga::SagaOutcome;
    use fraisier_saga::state_store::{FilesystemStateStore, FraiseKey, StateStore};
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, Mutex};

    /// An ordered log of every adapter call (`op:host`), shared across the fakes.
    type Trail = Arc<Mutex<Vec<String>>>;

    fn log(trail: &Trail, entry: impl Into<String>) {
        trail.lock().expect("trail").push(entry.into());
    }

    fn drain_trail(trail: &Trail) -> Vec<String> {
        trail.lock().expect("trail").clone()
    }

    fn exec_error(message: &str) -> AdapterError {
        AdapterError::new(AdapterErrorKind::Execution, message)
    }

    /// The hostname an adapter call targets, read from `ctx.host`.
    fn host_of(ctx: &AdapterCtx) -> String {
        ctx.host
            .as_ref()
            .map_or_else(|| "<none>".to_owned(), |h| h.as_str().to_owned())
    }

    /// Failure-injection knobs, shared by reference across the fakes.
    #[derive(Clone, Default)]
    struct Faults {
        /// Hosts whose `service.status` errors (i.e. unreachable at preflight).
        status_fail: BTreeSet<String>,
        /// Hosts whose `artifact.stage` errors.
        stage_fail: BTreeSet<String>,
        /// Whether `preflight` reports a blocking issue.
        preflight_blocking: bool,
        /// Whether the single `migrate` `up` errors.
        migrate_fail: bool,
        /// Hosts whose `lb.drain` errors.
        drain_fail: BTreeSet<String>,
        /// Hosts whose activation of the *new* artifact errors.
        activate_fail: BTreeSet<String>,
        /// Hosts that report unhealthy after restart.
        health_unhealthy: BTreeSet<String>,
        /// How many `restart` calls to fail per host before succeeding. `1` fails
        /// the new release's restart but lets a rollback restart succeed; a larger
        /// value also fails the rollback restart (forcing `PartialRollback`).
        restart_fail: BTreeMap<String, usize>,
        /// Whether the post-migration `verify` reports a failing check.
        verify_fail: bool,
    }

    impl Faults {
        fn hits(set: &BTreeSet<String>, host: &HostId) -> bool {
            set.contains(host.as_str())
        }
    }

    struct FakeArtifact {
        trail: Trail,
        faults: Arc<Faults>,
    }

    #[async_trait]
    impl ArtifactAdapter for FakeArtifact {
        async fn stage(
            &self,
            ctx: &AdapterCtx,
            host: &HostId,
        ) -> Result<StagedArtifact, AdapterError> {
            log(&self.trail, format!("stage:{host}"));
            assert_eq!(host_of(ctx), host.as_str(), "stage ctx targets the host");
            if Faults::hits(&self.faults.stage_fail, host) {
                return Err(exec_error("stage failed"));
            }
            Ok(StagedArtifact {
                artifact: ArtifactRef {
                    id: "v-new".to_owned(),
                    checksum: None,
                },
                path: format!("/staging/{host}/v-new").into(),
            })
        }

        async fn activate(
            &self,
            _ctx: &AdapterCtx,
            host: &HostId,
            staged: &StagedArtifact,
        ) -> Result<(), AdapterError> {
            log(
                &self.trail,
                format!("activate:{host}:{}", staged.artifact.id),
            );
            // Only the forward activation of the *new* artifact can be faulted;
            // re-activating the prior artifact on rollback always succeeds.
            if staged.artifact.id == "v-new" && Faults::hits(&self.faults.activate_fail, host) {
                return Err(exec_error("activate failed"));
            }
            Ok(())
        }

        async fn current(
            &self,
            _ctx: &AdapterCtx,
            _host: &HostId,
        ) -> Result<Option<ArtifactRef>, AdapterError> {
            Ok(None)
        }
    }

    struct FakeMigration {
        trail: Trail,
        faults: Arc<Faults>,
    }

    #[async_trait]
    impl MigrationAdapter for FakeMigration {
        async fn describe(&self) -> Result<AdapterDescription, AdapterError> {
            log(&self.trail, "describe");
            Ok(AdapterDescription {
                name: "fake".to_owned(),
                version: "0".to_owned(),
                protocol_version: 1,
                capabilities: vec!["preflight".to_owned(), "up".to_owned()],
            })
        }

        async fn current_revision(
            &self,
            _ctx: &AdapterCtx,
        ) -> Result<Option<Revision>, AdapterError> {
            log(&self.trail, "current_revision");
            Ok(Some(Revision::new("rev-prev")))
        }

        async fn up(
            &self,
            ctx: &AdapterCtx,
            _target: Option<Revision>,
        ) -> Result<MigrationOutcome, AdapterError> {
            log(&self.trail, "up");
            assert!(
                ctx.host.is_none(),
                "migrate runs against the shared DB, host-agnostic"
            );
            if self.faults.migrate_fail {
                return Err(exec_error("migration up failed"));
            }
            Ok(MigrationOutcome {
                from: Some(Revision::new("rev-prev")),
                to: Some(Revision::new("rev-new")),
                applied: vec![Revision::new("rev-new")],
                log: String::new(),
            })
        }

        async fn down_to(
            &self,
            _ctx: &AdapterCtx,
            target: Revision,
        ) -> Result<MigrationOutcome, AdapterError> {
            log(&self.trail, format!("down_to:{target}"));
            Ok(MigrationOutcome::default())
        }

        async fn verify(&self, ctx: &AdapterCtx) -> Result<VerifyReport, AdapterError> {
            log(&self.trail, "verify");
            assert!(
                ctx.host.is_none(),
                "global verify runs against the shared DB"
            );
            let checks = if self.faults.verify_fail {
                vec![VerifyCheck {
                    name: "row count".to_owned(),
                    ok: false,
                    detail: Some("expected 3 rows, found 0".to_owned()),
                }]
            } else {
                Vec::new()
            };
            Ok(VerifyReport {
                ok: !self.faults.verify_fail,
                checks,
            })
        }

        async fn preflight(&self, _ctx: &AdapterCtx) -> Result<PreflightReport, AdapterError> {
            log(&self.trail, "preflight");
            let issues = if self.faults.preflight_blocking {
                vec![PreflightIssue {
                    severity: Severity::Error,
                    code: "non_reversible".to_owned(),
                    message: "migration 003 has no down".to_owned(),
                    migration: Some("003".to_owned()),
                }]
            } else {
                Vec::new()
            };
            Ok(PreflightReport {
                ok: !self.faults.preflight_blocking,
                issues,
            })
        }
    }

    struct FakeService {
        trail: Trail,
        faults: Arc<Faults>,
        /// Remaining `restart` failures per host (decremented per call).
        restart_remaining: Mutex<BTreeMap<String, usize>>,
    }

    #[async_trait]
    impl ServiceAdapter for FakeService {
        async fn restart(&self, _ctx: &AdapterCtx, host: &HostId) -> Result<(), AdapterError> {
            log(&self.trail, format!("restart:{host}"));
            let left = {
                let remaining = self.restart_remaining.lock().expect("restart_remaining");
                remaining.get(host.as_str()).copied().unwrap_or(0)
            };
            if left > 0 {
                self.restart_remaining
                    .lock()
                    .expect("restart_remaining")
                    .insert(host.as_str().to_owned(), left - 1);
                return Err(exec_error("restart failed"));
            }
            Ok(())
        }

        async fn status(
            &self,
            _ctx: &AdapterCtx,
            host: &HostId,
        ) -> Result<ServiceStatus, AdapterError> {
            log(&self.trail, format!("status:{host}"));
            if Faults::hits(&self.faults.status_fail, host) {
                return Err(exec_error("host unreachable"));
            }
            Ok(ServiceStatus {
                running: true,
                detail: None,
            })
        }
    }

    struct FakeHealth {
        trail: Trail,
        faults: Arc<Faults>,
    }

    #[async_trait]
    impl HealthAdapter for FakeHealth {
        async fn check(
            &self,
            _ctx: &AdapterCtx,
            host: &HostId,
        ) -> Result<HealthStatus, AdapterError> {
            log(&self.trail, format!("check:{host}"));
            Ok(HealthStatus {
                healthy: !Faults::hits(&self.faults.health_unhealthy, host),
                detail: None,
            })
        }
    }

    struct FakeLb {
        trail: Trail,
        faults: Arc<Faults>,
    }

    #[async_trait]
    impl LbAdapter for FakeLb {
        async fn drain(
            &self,
            _ctx: &AdapterCtx,
            host: &HostId,
        ) -> Result<LbMembership, AdapterError> {
            log(&self.trail, format!("drain:{host}"));
            if Faults::hits(&self.faults.drain_fail, host) {
                return Err(exec_error("drain failed"));
            }
            Ok(LbMembership {
                state: LbState::InPool,
                weight: Some(100),
            })
        }

        async fn reattach(
            &self,
            _ctx: &AdapterCtx,
            host: &HostId,
            _prior: &LbMembership,
        ) -> Result<(), AdapterError> {
            log(&self.trail, format!("reattach:{host}"));
            Ok(())
        }
    }

    fn inventory() -> HostInventory {
        HostInventory::new()
            .with_host(HostEntry::new(HostId::new("web-1"), "web1.internal"))
            .with_host(HostEntry::new(HostId::new("web-2"), "web2.internal"))
            .with_host(HostEntry::new(HostId::new("web-3"), "web3.internal"))
    }

    /// Assemble a multi-host deploy from the fakes, sharing one `Faults`/`Trail`.
    fn deploy(trail: &Trail, faults: &Arc<Faults>, strategy: RolloutStrategy) -> MultiHostDeploy {
        MultiHostDeploy::builder(
            "checkout",
            "production",
            MultiHostPlan::new(inventory(), strategy),
        )
        .artifact(Arc::new(FakeArtifact {
            trail: trail.clone(),
            faults: Arc::clone(faults),
        }))
        .migration(Arc::new(FakeMigration {
            trail: trail.clone(),
            faults: Arc::clone(faults),
        }))
        .service(Arc::new(FakeService {
            trail: trail.clone(),
            faults: Arc::clone(faults),
            restart_remaining: Mutex::new(faults.restart_fail.clone()),
        }))
        .health(Arc::new(FakeHealth {
            trail: trail.clone(),
            faults: Arc::clone(faults),
        }))
        .lb(Arc::new(FakeLb {
            trail: trail.clone(),
            faults: Arc::clone(faults),
        }))
        .build()
        .expect("all adapters provided")
    }

    fn store() -> (tempfile::TempDir, FilesystemStateStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FilesystemStateStore::new(dir.path()).expect("store");
        (dir, store)
    }

    fn key() -> FraiseKey {
        FraiseKey::new("checkout", "production")
    }

    /// Seed a prior committed multi-host release so rollback has per-host targets.
    async fn seed_prior(store: &FilesystemStateStore) {
        let mut active = BTreeMap::new();
        for name in ["web-1", "web-2", "web-3"] {
            active.insert(
                HostId::new(name),
                StagedArtifact {
                    artifact: ArtifactRef {
                        id: "v-old".to_owned(),
                        checksum: None,
                    },
                    path: format!("/staging/{name}/v-old").into(),
                },
            );
        }
        let prior = MultiHostRecord {
            active,
            revision: Some(Revision::new("rev-old")),
        };
        store
            .record_snapshot(&key(), &serde_json::to_value(&prior).expect("encode"))
            .await
            .expect("seed prior ledger");
    }

    /// The position of the first trail entry equal to `needle`.
    fn pos(trail: &[String], needle: &str) -> usize {
        trail
            .iter()
            .position(|e| e == needle)
            .unwrap_or_else(|| panic!("missing {needle:?} in {trail:?}"))
    }

    /// Whether `trail` contains every entry in `expected`, regardless of order.
    fn contains_all(trail: &[String], expected: &[&str]) -> bool {
        expected.iter().all(|e| trail.iter().any(|t| t == e))
    }

    fn count(trail: &[String], needle: &str) -> usize {
        trail.iter().filter(|e| e.as_str() == needle).count()
    }

    #[test]
    fn batches_follow_the_strategy() {
        let hosts = inventory().hosts().to_vec();

        let all = plan_batches(RolloutStrategy::AllAtOnce, &hosts);
        assert_eq!(all.len(), 1, "all-at-once is a single batch");
        assert_eq!(all[0].len(), 3);

        let one = plan_batches(RolloutStrategy::Rolling(1), &hosts);
        assert_eq!(one.len(), 3, "rolling(1) is one host per batch");

        let two = plan_batches(RolloutStrategy::Rolling(2), &hosts);
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].len(), 2);
        assert_eq!(two[1].len(), 1, "the last batch holds the remainder");

        // A zero batch size is clamped to one rather than looping forever.
        assert_eq!(plan_batches(RolloutStrategy::Rolling(0), &hosts).len(), 3);
    }

    #[tokio::test]
    async fn rolling_one_migrates_once_then_cycles_each_host_in_order() {
        let (_dir, store) = store();
        let trail = Trail::default();
        let faults = Arc::new(Faults::default());
        let plan = deploy(&trail, &faults, RolloutStrategy::Rolling(1));

        let outcome = plan.run(store.clone()).await.expect("run");
        assert!(matches!(outcome, SagaOutcome::Committed), "got {outcome:?}");

        let trail = drain_trail(&trail);
        // Migration runs exactly once against the shared DB, not per host.
        assert_eq!(count(&trail, "up"), 1, "migrate-once: {trail:?}");
        assert_eq!(count(&trail, "current_revision"), 1);

        // Each host cycles drain → activate → restart → health → reattach in order.
        for name in ["web-1", "web-2", "web-3"] {
            let drain = pos(&trail, &format!("drain:{name}"));
            let activate = pos(&trail, &format!("activate:{name}:v-new"));
            let restart = pos(&trail, &format!("restart:{name}"));
            let check = pos(&trail, &format!("check:{name}"));
            let reattach = pos(&trail, &format!("reattach:{name}"));
            assert!(
                drain < activate && activate < restart && restart < check && check < reattach,
                "host {name} advances in order: {trail:?}"
            );
        }
        // Batches run strictly in inventory order: each host fully reattaches
        // before the next host is drained (rolling(1) keeps the rest live).
        assert!(pos(&trail, "reattach:web-1") < pos(&trail, "drain:web-2"));
        assert!(pos(&trail, "reattach:web-2") < pos(&trail, "drain:web-3"));

        // The committed ledger records each host's now-active artifact + revision.
        let snapshot = store
            .current_snapshot(&key())
            .await
            .expect("query")
            .expect("ledger recorded");
        let record: MultiHostRecord = serde_json::from_value(snapshot).expect("decode");
        assert_eq!(record.revision, Some(Revision::new("rev-new")));
        assert_eq!(record.active.len(), 3);
        assert!(record
            .active
            .values()
            .all(|staged| staged.artifact.id == "v-new"));
    }

    #[tokio::test]
    async fn all_at_once_migrates_once_and_advances_every_host() {
        let (_dir, store) = store();
        let trail = Trail::default();
        let faults = Arc::new(Faults::default());
        let plan = deploy(&trail, &faults, RolloutStrategy::AllAtOnce);

        let outcome = plan.run(store).await.expect("run");
        assert!(matches!(outcome, SagaOutcome::Committed), "got {outcome:?}");

        let trail = drain_trail(&trail);
        assert_eq!(count(&trail, "up"), 1, "migrate-once: {trail:?}");
        assert!(
            contains_all(
                &trail,
                &[
                    "drain:web-1",
                    "drain:web-2",
                    "drain:web-3",
                    "activate:web-1:v-new",
                    "activate:web-2:v-new",
                    "activate:web-3:v-new",
                    "reattach:web-1",
                    "reattach:web-2",
                    "reattach:web-3",
                ]
            ),
            "every host advanced: {trail:?}"
        );
    }

    #[tokio::test]
    async fn a_host_failure_rolls_back_advanced_hosts_then_the_migration_once() {
        // web-2's new release fails to restart (once); the rollback restart of the
        // prior release succeeds. A prior ledger is seeded so the restore has
        // per-host targets, so the whole deploy rolls back cleanly.
        let (_dir, store) = store();
        seed_prior(&store).await;
        let trail = Trail::default();
        let faults = Arc::new(Faults {
            restart_fail: BTreeMap::from([("web-2".to_owned(), 1)]),
            ..Faults::default()
        });
        let plan = deploy(&trail, &faults, RolloutStrategy::Rolling(1));

        let outcome = plan.run(store).await.expect("run completes with rollback");
        let SagaOutcome::RolledBack { failed_step, .. } = &outcome else {
            panic!("expected RolledBack, got {outcome:?}");
        };
        assert_eq!(failed_step, "rollout-2", "the second batch (web-2) failed");

        let trail = drain_trail(&trail);
        // The migration rolled back exactly once, to the pre-deploy revision.
        assert_eq!(count(&trail, "down_to:rev-prev"), 1, "{trail:?}");
        // web-2 self-healed: re-activate the prior artifact + restart it.
        assert!(
            contains_all(&trail, &["activate:web-2:v-old", "reattach:web-2"]),
            "web-2 restored to prior: {trail:?}"
        );
        // web-1 (the earlier, fully-advanced batch) was restored in reverse: it is
        // drained back out, re-activated to the prior artifact, and reattached.
        assert!(
            contains_all(&trail, &["activate:web-1:v-old", "reattach:web-1"]),
            "web-1 restored to prior: {trail:?}"
        );
        // The restored migration is the very last mutation (reverse order: hosts
        // first, then the single shared DB).
        let down = pos(&trail, "down_to:rev-prev");
        assert!(
            pos(&trail, "activate:web-1:v-old") < down,
            "hosts restore before the DB: {trail:?}"
        );
    }

    #[tokio::test]
    async fn preflight_reports_all_unreachable_hosts_and_skips_fetch() {
        let (_dir, store) = store();
        let trail = Trail::default();
        let faults = Arc::new(Faults {
            status_fail: BTreeSet::from(["web-2".to_owned(), "web-3".to_owned()]),
            ..Faults::default()
        });
        let plan = deploy(&trail, &faults, RolloutStrategy::Rolling(1));

        let outcome = plan.run(store).await.expect("run completes with rollback");
        let SagaOutcome::RolledBack {
            failed_step,
            reason,
        } = &outcome
        else {
            panic!("expected RolledBack, got {outcome:?}");
        };
        assert_eq!(failed_step, "preflight");
        assert!(
            reason.contains("web-2") && reason.contains("web-3"),
            "names hosts: {reason}"
        );

        let trail = drain_trail(&trail);
        assert!(
            !trail.iter().any(|e| e.starts_with("stage:")),
            "fetch never ran after preflight failed: {trail:?}"
        );
    }

    #[tokio::test]
    async fn blocking_preflight_fails_before_any_host_is_probed() {
        let (_dir, store) = store();
        let trail = Trail::default();
        let faults = Arc::new(Faults {
            preflight_blocking: true,
            ..Faults::default()
        });
        let plan = deploy(&trail, &faults, RolloutStrategy::AllAtOnce);

        let outcome = plan.run(store).await.expect("run");
        assert!(
            matches!(&outcome, SagaOutcome::RolledBack { failed_step, .. } if failed_step == "preflight"),
            "got {outcome:?}"
        );
        let trail = drain_trail(&trail);
        assert_eq!(
            trail,
            vec!["describe", "preflight"],
            "no host probed: {trail:?}"
        );
    }

    #[tokio::test]
    async fn build_requires_every_adapter_and_a_non_empty_inventory() {
        let empty = MultiHostDeploy::builder(
            "checkout",
            "production",
            MultiHostPlan::new(HostInventory::new(), RolloutStrategy::AllAtOnce),
        )
        .build()
        .expect_err("empty inventory");
        assert!(matches!(empty, MultiHostBuildError::EmptyInventory));

        let trail = Trail::default();
        let faults = Arc::new(Faults::default());
        let missing = MultiHostDeploy::builder(
            "checkout",
            "production",
            MultiHostPlan::new(inventory(), RolloutStrategy::AllAtOnce),
        )
        .artifact(Arc::new(FakeArtifact { trail, faults }))
        .build()
        .expect_err("missing migration/service/health/lb");
        assert!(matches!(
            missing,
            MultiHostBuildError::MissingAdapter("migration")
        ));
    }

    #[test]
    fn plan_exposes_inventory_and_strategy() {
        let plan = MultiHostPlan::new(inventory(), RolloutStrategy::Rolling(2));
        assert_eq!(plan.inventory().hosts().len(), 3);
        assert!(matches!(plan.strategy(), RolloutStrategy::Rolling(2)));
    }

    #[test]
    fn host_entry_reserves_per_host_adapter_overrides() {
        let mut entry = HostEntry::new(HostId::new("canary"), "canary.internal");
        entry.overrides.insert(
            "artifact".to_owned(),
            serde_json::json!({ "source": "local" }),
        );
        assert!(entry.overrides.contains_key("artifact"));
    }

    #[test]
    fn strategy_round_trips_through_serde() {
        let strategy = RolloutStrategy::Rolling(2);
        let json = serde_json::to_string(&strategy).expect("serialize");
        let back: RolloutStrategy = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(back, RolloutStrategy::Rolling(2)));
    }

    // ----- Cycle 4.3: forced-failure rollback at each phase + PartialRollback ----

    #[tokio::test]
    async fn fetch_failure_rolls_back_without_touching_hosts_or_db() {
        let (_dir, store) = store();
        let trail = Trail::default();
        let faults = Arc::new(Faults {
            stage_fail: BTreeSet::from(["web-2".to_owned()]),
            ..Faults::default()
        });
        let plan = deploy(&trail, &faults, RolloutStrategy::Rolling(1));

        let outcome = plan.run(store).await.expect("run completes with rollback");
        assert!(
            matches!(&outcome, SagaOutcome::RolledBack { failed_step, .. } if failed_step == "fetch"),
            "got {outcome:?}"
        );
        let trail = drain_trail(&trail);
        // Fetch failed before the migration and any host op: nothing to undo.
        assert_eq!(count(&trail, "up"), 0, "DB untouched: {trail:?}");
        assert!(
            !trail
                .iter()
                .any(|e| e.starts_with("drain:") || e.starts_with("down_to:")),
            "no host or DB mutation: {trail:?}"
        );
    }

    #[tokio::test]
    async fn migrate_failure_compensates_nothing_destructive() {
        // `up` fails, so migrate never completed: no host is touched, and the DB is
        // never rolled back (there is nothing applied to undo).
        let (_dir, store) = store();
        seed_prior(&store).await;
        let trail = Trail::default();
        let faults = Arc::new(Faults {
            migrate_fail: true,
            ..Faults::default()
        });
        let plan = deploy(&trail, &faults, RolloutStrategy::AllAtOnce);

        let outcome = plan.run(store).await.expect("run completes with rollback");
        assert!(
            matches!(&outcome, SagaOutcome::RolledBack { failed_step, .. } if failed_step == "migrate"),
            "got {outcome:?}"
        );
        let trail = drain_trail(&trail);
        assert_eq!(count(&trail, "up"), 1, "up was attempted: {trail:?}");
        assert!(
            !trail
                .iter()
                .any(|e| e.starts_with("drain:") || e.starts_with("down_to:")),
            "no host advanced, no DB rollback: {trail:?}"
        );
    }

    #[tokio::test]
    async fn mid_rollout_failure_restores_earlier_batches_in_reverse_then_db_once() {
        // rolling(1), three hosts; the LAST host (web-3) is unhealthy. web-1 and
        // web-2 fully advanced, so they are restored in REVERSE order (web-2 before
        // web-1), then the single migration is rolled back once — last of all.
        let (_dir, store) = store();
        seed_prior(&store).await;
        let trail = Trail::default();
        let faults = Arc::new(Faults {
            health_unhealthy: BTreeSet::from(["web-3".to_owned()]),
            ..Faults::default()
        });
        let plan = deploy(&trail, &faults, RolloutStrategy::Rolling(1));

        let outcome = plan.run(store).await.expect("run completes with rollback");
        assert!(
            matches!(&outcome, SagaOutcome::RolledBack { failed_step, .. } if failed_step == "rollout-3"),
            "got {outcome:?}"
        );
        let trail = drain_trail(&trail);
        assert_eq!(
            count(&trail, "down_to:rev-prev"),
            1,
            "DB rolled back once: {trail:?}"
        );
        // Every host ends back on the prior artifact.
        assert!(contains_all(
            &trail,
            &[
                "activate:web-1:v-old",
                "activate:web-2:v-old",
                "activate:web-3:v-old"
            ]
        ));
        // Reverse host order, then the DB last.
        assert!(
            pos(&trail, "activate:web-2:v-old") < pos(&trail, "activate:web-1:v-old"),
            "later host restores first: {trail:?}"
        );
        assert!(
            pos(&trail, "activate:web-1:v-old") < pos(&trail, "down_to:rev-prev"),
            "all hosts restore before the DB: {trail:?}"
        );
    }

    #[tokio::test]
    async fn a_host_after_the_failing_batch_is_never_touched() {
        // web-2 fails to activate the new artifact; web-3 (a later batch) must never
        // be drained — a rollout stops advancing once a batch fails.
        let (_dir, store) = store();
        seed_prior(&store).await;
        let trail = Trail::default();
        let faults = Arc::new(Faults {
            activate_fail: BTreeSet::from(["web-2".to_owned()]),
            ..Faults::default()
        });
        let plan = deploy(&trail, &faults, RolloutStrategy::Rolling(1));

        let outcome = plan.run(store).await.expect("run completes with rollback");
        assert!(
            matches!(&outcome, SagaOutcome::RolledBack { failed_step, .. } if failed_step == "rollout-2"),
            "got {outcome:?}"
        );
        let trail = drain_trail(&trail);
        assert!(
            !trail.iter().any(|e| e == "drain:web-3"),
            "the batch after the failure never started: {trail:?}"
        );
        // web-2 only drained (activate failed before it could mutate the release),
        // so its restore is just a reattach — never a prior re-activation.
        assert!(
            !trail.iter().any(|e| e == "activate:web-2:v-old"),
            "a host that never activated is not re-activated on restore: {trail:?}"
        );
        assert!(trail.iter().any(|e| e == "reattach:web-2"));
    }

    #[tokio::test]
    async fn an_unrecoverable_host_restore_reports_partial_rollback() {
        // web-2's restart fails forever, so both the forward restart AND the
        // rollback restart fail: the host cannot be returned to its prior release.
        // The run must surface PartialRollback (not a clean RolledBack) and name it.
        let (_dir, store) = store();
        seed_prior(&store).await;
        let trail = Trail::default();
        let faults = Arc::new(Faults {
            restart_fail: BTreeMap::from([("web-2".to_owned(), 5)]),
            ..Faults::default()
        });
        let plan = deploy(&trail, &faults, RolloutStrategy::Rolling(1));

        let outcome = plan.run(store).await.expect("run completes");
        let SagaOutcome::PartialRollback { reason } = &outcome else {
            panic!("expected PartialRollback, got {outcome:?}");
        };
        assert!(
            reason.contains("web-2"),
            "the operator-facing reason names the stuck host: {reason}"
        );
    }

    #[tokio::test]
    async fn a_first_ever_deploy_failure_reports_partial_rollback() {
        // No prior ledger: a host that activated the new release cannot be rolled
        // back (there is no prior artifact to restore), so a first-deploy failure is
        // a PartialRollback, exactly like the single-host first-deploy case.
        let (_dir, store) = store();
        let trail = Trail::default();
        let faults = Arc::new(Faults {
            health_unhealthy: BTreeSet::from(["web-1".to_owned()]),
            ..Faults::default()
        });
        let plan = deploy(&trail, &faults, RolloutStrategy::Rolling(1));

        let outcome = plan.run(store).await.expect("run completes");
        assert!(
            matches!(&outcome, SagaOutcome::PartialRollback { .. }),
            "a first-deploy rollback with no prior release is a PartialRollback, got {outcome:?}"
        );
        let trail = drain_trail(&trail);
        assert!(
            !trail.iter().any(|e| e == "drain:web-2"),
            "later hosts never started: {trail:?}"
        );
    }

    // ----- Cycle 4.4: VerifyGlobal -----------------------------------------------

    #[tokio::test]
    async fn global_verify_runs_once_after_the_rollout_and_reprobes_the_fleet() {
        let (_dir, store) = store();
        let trail = Trail::default();
        let faults = Arc::new(Faults::default());
        let plan = deploy(&trail, &faults, RolloutStrategy::Rolling(1));

        let outcome = plan.run(store).await.expect("run");
        assert!(matches!(outcome, SagaOutcome::Committed), "got {outcome:?}");

        let trail = drain_trail(&trail);
        // The post-migration verify runs exactly once, after the whole rollout.
        assert_eq!(count(&trail, "verify"), 1, "{trail:?}");
        assert!(
            pos(&trail, "verify") > pos(&trail, "reattach:web-3"),
            "verify runs after every host advanced: {trail:?}"
        );
        // The fleet smoke test re-probes every host once more (so each host is
        // health-checked twice: once on reattach, once in the global verify).
        for name in ["web-1", "web-2", "web-3"] {
            assert_eq!(
                count(&trail, &format!("check:{name}")),
                2,
                "host {name} probed in rollout and again in verify: {trail:?}"
            );
        }
    }

    #[tokio::test]
    async fn verify_failure_triggers_a_full_rollback() {
        // Every host advances and passes its local health check, but the
        // post-migration verify fails: the whole deploy rolls back — all hosts
        // restored in reverse, then the migration once.
        let (_dir, store) = store();
        seed_prior(&store).await;
        let trail = Trail::default();
        let faults = Arc::new(Faults {
            verify_fail: true,
            ..Faults::default()
        });
        let plan = deploy(&trail, &faults, RolloutStrategy::Rolling(1));

        let outcome = plan.run(store).await.expect("run completes with rollback");
        assert!(
            matches!(&outcome, SagaOutcome::RolledBack { failed_step, .. } if failed_step == "verify"),
            "got {outcome:?}"
        );
        let trail = drain_trail(&trail);
        assert_eq!(
            count(&trail, "down_to:rev-prev"),
            1,
            "DB rolled back once: {trail:?}"
        );
        assert!(contains_all(
            &trail,
            &[
                "activate:web-1:v-old",
                "activate:web-2:v-old",
                "activate:web-3:v-old"
            ]
        ));
        // Reverse host order (the last-advanced host restores first), DB last.
        assert!(pos(&trail, "activate:web-3:v-old") < pos(&trail, "activate:web-1:v-old"));
        assert!(pos(&trail, "activate:web-1:v-old") < pos(&trail, "down_to:rev-prev"));
    }
}
