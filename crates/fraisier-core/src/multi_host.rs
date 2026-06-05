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

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

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
        let mut saga = Saga::new(store.clone(), self.fraise.clone(), self.environment.clone())
            .with_step(shared.step(StepKind::Preflight))
            .with_step(shared.step(StepKind::Fetch))
            .with_step(shared.step(StepKind::Migrate));
        for (index, batch) in plan_batches(self.plan.strategy(), self.plan.inventory().hosts())
            .into_iter()
            .enumerate()
        {
            saga = saga.with_step(shared.rollout_step(index + 1, batch));
        }

        let outcome = shared.finalize(saga.run().await?);

        if matches!(outcome, SagaOutcome::Committed) {
            let record = shared.committed_record();
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
/// the inventory, the durable rollback target (`prior`), and the in-run captures.
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
    /// State captured while the deploy runs forward, read back during compensation
    /// and commit.
    runtime: Mutex<RolloutRuntime>,
}

/// State captured while a multi-host deploy runs forward.
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
}

impl StepKind {
    /// The stable step name used in saga state, events, and spans.
    fn step_name(&self) -> &str {
        match self {
            Self::Preflight => "preflight",
            Self::Fetch => "fetch",
            Self::Migrate => "migrate",
            Self::Rollout { name, .. } => name,
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
            runtime: Mutex::new(RolloutRuntime::default()),
        }
    }

    fn step(self: &Arc<Self>, kind: StepKind) -> Box<dyn Step> {
        Box::new(RolloutStep {
            shared: Arc::clone(self),
            kind,
        })
    }

    fn rollout_step(self: &Arc<Self>, index: usize, batch: Vec<HostEntry>) -> Box<dyn Step> {
        self.step(StepKind::Rollout {
            name: format!("rollout-{index}"),
            batch,
        })
    }

    fn runtime(&self) -> MutexGuard<'_, RolloutRuntime> {
        self.runtime.lock().expect("rollout runtime mutex poisoned")
    }

    /// Map an adapter error into a saga step failure, preserving its detail.
    fn failed(step: &str, error: &AdapterError) -> SagaError {
        SagaError::StepFailed {
            step: step.to_owned(),
            message: error.to_string(),
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

    /// Map the saga outcome onto the run's own findings: a clean rollback that left
    /// at least one host unrecoverable is reported as a [`SagaOutcome::PartialRollback`].
    fn finalize(&self, outcome: SagaOutcome) -> SagaOutcome {
        let unrecoverable = self.runtime().unrecoverable.take();
        match (outcome, unrecoverable) {
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
        let runtime = self.runtime();
        MultiHostRecord {
            active: runtime.staged.clone(),
            revision: runtime.new_revision.clone(),
        }
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
    async fn run_fetch(&self) -> Result<(), SagaError> {
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
        self.runtime().staged = staged;
        Ok(())
    }

    /// Migrate **once** against the shared database. Captures the live revision
    /// before `up` so a rollback has an exact `down_to` target.
    async fn run_migrate(&self) -> Result<(), SagaError> {
        let previous = self
            .migration
            .current_revision(&self.ctx)
            .await
            .map_err(|e| Self::failed("migrate", &e))?;
        self.runtime().previous_revision = previous;

        let outcome = self
            .migration
            .up(&self.ctx, self.target.clone())
            .await
            .map_err(|e| Self::failed("migrate", &e))?;

        // Scope the guard so it drops before the function tail (no lock held across
        // the return).
        {
            let mut runtime = self.runtime();
            runtime.new_revision = outcome.to.or_else(|| runtime.previous_revision.clone());
        }
        Ok(())
    }

    /// Roll the single shared-database migration back to the pre-deploy revision.
    async fn undo_migrate(&self) -> Result<(), SagaError> {
        let previous = self.runtime().previous_revision.clone();
        match previous {
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

    /// Advance one batch: every host through `drain → activate → restart → health
    /// → reattach`, concurrently. If any host fails, this batch's own hosts are
    /// restored before the failure is surfaced (the saga then rolls back the
    /// earlier batches and the migration); a restore that itself fails is recorded
    /// as unrecoverable.
    async fn run_rollout(&self, batch: &[HostEntry]) -> Result<(), SagaError> {
        let advances = batch.iter().map(|entry| self.advance_host(entry));
        let errors: Vec<String> = futures::future::join_all(advances)
            .await
            .into_iter()
            .filter_map(Result::err)
            .map(|e| e.to_string())
            .collect();
        if errors.is_empty() {
            return Ok(());
        }
        if let Err(restore_err) = self.restore_batch(batch).await {
            self.runtime()
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
    /// so a later restore can undo exactly those steps.
    async fn advance_host(&self, entry: &HostEntry) -> Result<(), SagaError> {
        let ctx = self.host_ctx(entry);
        let host = &entry.host;

        let membership = self
            .lb
            .drain(&ctx, host)
            .await
            .map_err(|e| Self::failed("rollout", &e))?;
        self.runtime()
            .progress
            .entry(host.clone())
            .or_default()
            .drained = Some(membership.clone());

        let staged =
            self.runtime()
                .staged
                .get(host)
                .cloned()
                .ok_or_else(|| SagaError::StepFailed {
                    step: "rollout".to_owned(),
                    message: format!("no staged artifact recorded for host {host}"),
                })?;
        self.artifact
            .activate(&ctx, host, &staged)
            .await
            .map_err(|e| Self::failed("rollout", &e))?;
        self.runtime()
            .progress
            .entry(host.clone())
            .or_default()
            .activated = true;

        self.service
            .restart(&ctx, host)
            .await
            .map_err(|e| Self::failed("rollout", &e))?;

        let status = self
            .health
            .check(&ctx, host)
            .await
            .map_err(|e| Self::failed("rollout", &e))?;
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
            .map_err(|e| Self::failed("rollout", &e))?;
        self.runtime()
            .progress
            .entry(host.clone())
            .or_default()
            .reattached = true;
        Ok(())
    }

    /// Restore every host in a batch concurrently, reporting all that could not be
    /// fully restored.
    async fn restore_batch(&self, batch: &[HostEntry]) -> Result<(), SagaError> {
        let restores = batch.iter().map(|entry| self.restore_host(entry));
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
    async fn restore_host(&self, entry: &HostEntry) -> Result<(), SagaError> {
        let ctx = self.host_ctx(entry);
        let host = &entry.host;
        let progress = self
            .runtime()
            .progress
            .get(host)
            .cloned()
            .unwrap_or_default();
        let Some(prior_membership) = progress.drained else {
            return Ok(());
        };

        if progress.reattached {
            self.lb
                .drain(&ctx, host)
                .await
                .map_err(|e| Self::failed("rollout", &e))?;
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
                .map_err(|e| Self::failed("rollout", &e))?;
            self.service
                .restart(&ctx, host)
                .await
                .map_err(|e| Self::failed("rollout", &e))?;
        }

        self.lb
            .reattach(&ctx, host, &prior_membership)
            .await
            .map_err(|e| Self::failed("rollout", &e))
    }
}

/// One deploy phase: a thin adapter from a [`StepKind`] to the shared rollout
/// logic.
struct RolloutStep {
    shared: Arc<RolloutShared>,
    kind: StepKind,
}

#[async_trait]
impl Step for RolloutStep {
    fn name(&self) -> &str {
        self.kind.step_name()
    }

    async fn forward(&self, _ctx: &StepContext) -> Result<(), SagaError> {
        match &self.kind {
            StepKind::Preflight => self.shared.run_preflight().await,
            StepKind::Fetch => self.shared.run_fetch().await,
            StepKind::Migrate => self.shared.run_migrate().await,
            StepKind::Rollout { batch, .. } => self.shared.run_rollout(batch).await,
        }
    }

    async fn compensate(&self, _ctx: &StepContext) -> Result<(), SagaError> {
        match &self.kind {
            // Preflight and fetch are non-mutating (a reachability probe, an
            // artifact staged but never activated), so they have nothing to undo.
            StepKind::Preflight | StepKind::Fetch => Ok(()),
            StepKind::Migrate => self.shared.undo_migrate().await,
            StepKind::Rollout { batch, .. } => self.shared.restore_batch(batch).await,
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
        ServiceAdapter, ServiceStatus, Severity, StagedArtifact, VerifyReport,
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

        async fn verify(&self, _ctx: &AdapterCtx) -> Result<VerifyReport, AdapterError> {
            log(&self.trail, "verify");
            Ok(VerifyReport {
                ok: true,
                checks: Vec::new(),
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
}
