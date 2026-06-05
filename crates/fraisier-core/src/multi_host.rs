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
//! rollout phase, in strategy order. Within a batch, hosts advance concurrently;
//! batches run in order so the rest of the fleet stays live (PRD §5.5).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fraisier_saga::saga::{Saga, SagaError, SagaOutcome, Step, StepContext};
use fraisier_saga::state_store::{StateStore, StateStoreError};
use serde_json::Value;

use crate::adapter_axes::{
    AdapterCtx, AdapterError, ArtifactAdapter, HealthAdapter, HostId, LbAdapter, MigrationAdapter,
    Revision, ServiceAdapter, Severity, StagedArtifact,
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
    /// The store is cloned into the saga, so it must be `Clone` (both shipped
    /// backends are).
    ///
    /// # Errors
    /// [`MultiHostError`] for infrastructure failures (engine or state store). A
    /// clean rollback is `Ok(SagaOutcome::RolledBack)`, and an unrecoverable one is
    /// `Ok(SagaOutcome::PartialRollback)`.
    pub async fn run<S: StateStore + Clone>(
        &self,
        store: S,
    ) -> Result<SagaOutcome, MultiHostError> {
        let shared = Arc::new(RolloutShared::new(self));
        let saga = Saga::new(store, self.fraise.clone(), self.environment.clone())
            .with_step(shared.step(Phase::Preflight))
            .with_step(shared.step(Phase::Fetch));

        Ok(saga.run().await?)
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
/// the inventory/strategy, and the in-run captures (`runtime`).
struct RolloutShared {
    ctx: AdapterCtx,
    inventory: Vec<HostEntry>,
    #[allow(dead_code)] // Reason: consumed by the rollout phase (Cycle 4.2).
    strategy: RolloutStrategy,
    #[allow(dead_code)] // Reason: consumed by the migrate phase (Cycle 4.2).
    target: Option<Revision>,
    forward_compatible_lint: bool,
    artifact: Arc<dyn ArtifactAdapter>,
    migration: Arc<dyn MigrationAdapter>,
    service: Arc<dyn ServiceAdapter>,
    #[allow(dead_code)] // Reason: consumed by the rollout phase (Cycle 4.2).
    health: Arc<dyn HealthAdapter>,
    #[allow(dead_code)] // Reason: consumed by the rollout phase (Cycle 4.2).
    lb: Arc<dyn LbAdapter>,
    /// State captured during this run, read back during later phases.
    runtime: Mutex<RolloutRuntime>,
}

/// State captured while a multi-host deploy runs forward.
#[derive(Default)]
struct RolloutRuntime {
    /// The artifact staged on each host by `fetch`, keyed by host.
    staged: BTreeMap<HostId, StagedArtifact>,
}

/// Which deploy phase a [`RolloutStep`] represents.
#[derive(Clone, Copy)]
enum Phase {
    Preflight,
    Fetch,
}

impl Phase {
    /// The stable step name used in saga state, events, and spans.
    const fn step_name(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::Fetch => "fetch",
        }
    }
}

impl RolloutShared {
    fn new(deploy: &MultiHostDeploy) -> Self {
        Self {
            ctx: deploy.ctx.clone(),
            inventory: deploy.plan.inventory().hosts().to_vec(),
            strategy: deploy.plan.strategy(),
            target: deploy.target.clone(),
            forward_compatible_lint: deploy.forward_compatible_lint,
            artifact: Arc::clone(&deploy.artifact),
            migration: Arc::clone(&deploy.migration),
            service: Arc::clone(&deploy.service),
            health: Arc::clone(&deploy.health),
            lb: Arc::clone(&deploy.lb),
            runtime: Mutex::new(RolloutRuntime::default()),
        }
    }

    fn step(self: &Arc<Self>, phase: Phase) -> Box<dyn Step> {
        Box::new(RolloutStep {
            shared: Arc::clone(self),
            phase,
        })
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
        self.runtime.lock().expect("rollout runtime").staged = staged;
        Ok(())
    }
}

/// One deploy phase: a thin adapter from a [`Phase`] to the shared rollout logic.
struct RolloutStep {
    shared: Arc<RolloutShared>,
    phase: Phase,
}

#[async_trait]
impl Step for RolloutStep {
    fn name(&self) -> &str {
        self.phase.step_name()
    }

    async fn forward(&self, _ctx: &StepContext) -> Result<(), SagaError> {
        match self.phase {
            Phase::Preflight => self.shared.run_preflight().await,
            Phase::Fetch => self.shared.run_fetch().await,
        }
    }

    async fn compensate(&self, _ctx: &StepContext) -> Result<(), SagaError> {
        // Preflight and fetch are non-mutating (a reachability probe, an artifact
        // staged but never activated), so they have nothing to undo.
        match self.phase {
            Phase::Preflight | Phase::Fetch => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HostEntry, HostInventory, MultiHostBuildError, MultiHostDeploy, MultiHostPlan,
        RolloutStrategy,
    };
    use crate::adapter_axes::{
        AdapterCtx, AdapterDescription, AdapterError, AdapterErrorKind, ArtifactAdapter,
        ArtifactRef, HealthAdapter, HealthStatus, HostId, LbAdapter, LbMembership, LbState,
        MigrationAdapter, MigrationOutcome, PreflightIssue, PreflightReport, Revision,
        ServiceAdapter, ServiceStatus, Severity, StagedArtifact, VerifyReport,
    };
    use async_trait::async_trait;
    use fraisier_saga::saga::SagaOutcome;
    use fraisier_saga::state_store::FilesystemStateStore;
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    /// An ordered log of every adapter call (`op:host`), shared across the fakes.
    type Trail = Arc<Mutex<Vec<String>>>;

    fn log(trail: &Trail, entry: impl Into<String>) {
        trail.lock().expect("trail").push(entry.into());
    }

    fn drain(trail: &Trail) -> Vec<String> {
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

    /// Failure-injection knobs, shared by reference across the fakes. Each set
    /// names the hosts whose call of that axis should fail.
    #[derive(Clone, Default)]
    struct Faults {
        /// Hosts whose `service.status` errors (i.e. unreachable at preflight).
        status_fail: BTreeSet<String>,
        /// Hosts whose `artifact.stage` errors.
        stage_fail: BTreeSet<String>,
        /// Whether `preflight` reports a blocking issue.
        preflight_blocking: bool,
    }

    impl Faults {
        fn hits(set: &BTreeSet<String>, host: &str) -> bool {
            set.contains(host)
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
            if Faults::hits(&self.faults.stage_fail, host.as_str()) {
                return Err(exec_error("stage failed"));
            }
            assert_eq!(host_of(ctx), host.as_str(), "stage ctx targets the host");
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
            _ctx: &AdapterCtx,
            _target: Option<Revision>,
        ) -> Result<MigrationOutcome, AdapterError> {
            log(&self.trail, "up");
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
    }

    #[async_trait]
    impl ServiceAdapter for FakeService {
        async fn restart(&self, _ctx: &AdapterCtx, host: &HostId) -> Result<(), AdapterError> {
            log(&self.trail, format!("restart:{host}"));
            Ok(())
        }

        async fn status(
            &self,
            _ctx: &AdapterCtx,
            host: &HostId,
        ) -> Result<ServiceStatus, AdapterError> {
            log(&self.trail, format!("status:{host}"));
            if Faults::hits(&self.faults.status_fail, host.as_str()) {
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
                healthy: true,
                detail: None,
            })
        }
    }

    struct FakeLb {
        trail: Trail,
    }

    #[async_trait]
    impl LbAdapter for FakeLb {
        async fn drain(
            &self,
            _ctx: &AdapterCtx,
            host: &HostId,
        ) -> Result<LbMembership, AdapterError> {
            log(&self.trail, format!("drain:{host}"));
            Ok(LbMembership {
                state: LbState::InPool,
                weight: None,
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
        }))
        .health(Arc::new(FakeHealth {
            trail: trail.clone(),
        }))
        .lb(Arc::new(FakeLb {
            trail: trail.clone(),
        }))
        .build()
        .expect("all adapters provided")
    }

    fn store() -> (tempfile::TempDir, FilesystemStateStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FilesystemStateStore::new(dir.path()).expect("store");
        (dir, store)
    }

    /// Whether `trail` contains every `op:host` in `expected`, regardless of the
    /// order concurrent calls happened to interleave in.
    fn contains_all(trail: &[String], expected: &[&str]) -> bool {
        expected.iter().all(|e| trail.iter().any(|t| t == e))
    }

    #[tokio::test]
    async fn preflight_probes_every_host_then_fetch_stages_every_host() {
        let (_dir, store) = store();
        let trail = Trail::default();
        let faults = Arc::new(Faults::default());
        let plan = deploy(&trail, &faults, RolloutStrategy::Rolling(1));

        let outcome = plan.run(store).await.expect("run");
        assert!(matches!(outcome, SagaOutcome::Committed), "got {outcome:?}");

        let trail = drain(&trail);
        // The forward-compat lint runs once against the shared DB (not per host).
        assert_eq!(trail.iter().filter(|e| *e == "describe").count(), 1);
        assert_eq!(trail.iter().filter(|e| *e == "preflight").count(), 1);
        // Every host is probed for reachability and staged.
        assert!(
            contains_all(
                &trail,
                &[
                    "status:web-1",
                    "status:web-2",
                    "status:web-3",
                    "stage:web-1",
                    "stage:web-2",
                    "stage:web-3",
                ]
            ),
            "every host probed + staged: {trail:?}"
        );
        // Preflight precedes fetch: the last status comes before the first stage.
        let last_status = trail
            .iter()
            .rposition(|e| e.starts_with("status:"))
            .expect("status");
        let first_stage = trail
            .iter()
            .position(|e| e.starts_with("stage:"))
            .expect("stage");
        assert!(
            last_status < first_stage,
            "preflight precedes fetch: {trail:?}"
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
        // The message names every unreachable host, not just the first.
        assert!(
            reason.contains("web-2") && reason.contains("web-3"),
            "names hosts: {reason}"
        );

        let trail = drain(&trail);
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
        let trail = drain(&trail);
        assert_eq!(
            trail,
            vec!["describe", "preflight"],
            "no host was probed: {trail:?}"
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
