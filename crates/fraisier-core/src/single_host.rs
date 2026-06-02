//! Single-host deploy composition (PRD §5.2) — the deploy layer over the saga.
//!
//! [`SingleHostDeploy`] wraps the four single-host adapter axes (artifact,
//! migration, service, health — load-balancing is multi-host, Phase 4) into
//! [`fraisier_saga`] [`Step`]s and drives them through the saga state machine:
//!
//! ```text
//! Idle → preflight → fetch → migrate → release → health → verify → Committed
//! ```
//!
//! On any step failure the saga compensates the completed steps in reverse,
//! restoring the prior state. Two steps mutate durable state and therefore
//! compensate:
//!
//! - **migrate** rolls back with `down_to(previous_revision)`, where the previous
//!   revision is captured live (via `current_revision`) immediately before `up`.
//! - **release** (activate the staged artifact + restart the service) rolls back
//!   by re-activating the **previously-active artifact** and restarting.
//!
//! # The release ledger (why rollback is durable, not reconstructed)
//!
//! The previously-active artifact was staged by an *earlier deploy process*, so
//! its [`StagedArtifact`] (crucially, its on-disk path) cannot be reconstructed
//! from a live `current()` query — that returns only an [`ArtifactRef`]. Instead,
//! every committed deploy records its active [`StagedArtifact`] in a durable
//! **release ledger** ([`DeployRecord`]) via the state store's snapshot slot
//! ([`StateStore::record_snapshot`]). A later deploy reads the prior record and
//! rolls back by handing that exact [`StagedArtifact`] back to
//! [`ArtifactAdapter::activate`]. This is the standard release-history pattern
//! (Capistrano's releases log, Nix generations, Kubernetes `ReplicaSet` history):
//! artifacts are immutable and retained, activation is a reversible pointer flip,
//! and the orchestrator owns the rollback target — the adapter stays a stateless
//! executor.
//!
//! A failed **first-ever** deploy has no prior record, so its rollback of the
//! mutating steps reports [`SagaOutcome::PartialRollback`]: there is genuinely no
//! committed state to return to. This is deliberate, not a gap — every deploy
//! tool treats first-deploy rollback as a special, operator-visible case.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fraisier_saga::saga::{Saga, SagaError, SagaOutcome, Step, StepContext};
use fraisier_saga::state_store::{FraiseKey, StateStore, StateStoreError};
use serde::{Deserialize, Serialize};

use crate::adapter_axes::{
    AdapterCtx, AdapterError, ArtifactAdapter, HealthAdapter, HostId, MigrationAdapter, Revision,
    ServiceAdapter, Severity, StagedArtifact,
};

/// The durable record of what a committed deploy made live — the rollback target
/// a *later* deploy reads back from the state store's snapshot slot.
///
/// # Example
/// ```
/// # use fraisier_core::single_host::DeployRecord;
/// let record = DeployRecord::default();
/// assert!(record.active.is_none() && record.revision.is_none());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployRecord {
    /// The artifact that was activated, kept whole (with its path) so a future
    /// deploy can re-activate it on rollback.
    pub active: Option<StagedArtifact>,
    /// The migration revision that was live after the deploy committed.
    pub revision: Option<Revision>,
}

/// Errors from running a [`SingleHostDeploy`].
///
/// A *business* failure that rolls back cleanly is **not** an error — it is a
/// successful [`SagaOutcome::RolledBack`]. This type is only for infrastructure
/// failures: the saga/engine, the state store, or ledger (de)serialization.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DeployError {
    /// The saga engine reported an infrastructure failure (locking/persistence).
    #[error(transparent)]
    Saga(#[from] SagaError),
    /// The state store failed while reading or writing the release ledger.
    #[error(transparent)]
    Store(#[from] StateStoreError),
    /// The release ledger could not be (de)serialized.
    #[error("deploy ledger (de)serialization failed: {0}")]
    Ledger(#[from] serde_json::Error),
}

/// A configured single-host deploy. Build one with [`SingleHostDeploy::builder`]
/// and execute it with [`SingleHostDeploy::run`].
pub struct SingleHostDeploy {
    fraise: String,
    environment: String,
    host: HostId,
    ctx: AdapterCtx,
    target: Option<Revision>,
    artifact: Arc<dyn ArtifactAdapter>,
    migration: Arc<dyn MigrationAdapter>,
    service: Arc<dyn ServiceAdapter>,
    health: Arc<dyn HealthAdapter>,
}

impl std::fmt::Debug for SingleHostDeploy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The adapters are trait objects (not `Debug`); show the identity instead.
        f.debug_struct("SingleHostDeploy")
            .field("fraise", &self.fraise)
            .field("environment", &self.environment)
            .field("host", &self.host)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl SingleHostDeploy {
    /// Start building a deploy for `fraise`/`environment` targeting `host`.
    #[must_use]
    pub fn builder(
        fraise: impl Into<String>,
        environment: impl Into<String>,
        host: HostId,
    ) -> SingleHostDeployBuilder {
        SingleHostDeployBuilder::new(fraise, environment, host)
    }

    /// Run the deploy over `store`, returning how the saga ended.
    ///
    /// Reads the prior [`DeployRecord`] (the rollback target) before running and,
    /// on [`SagaOutcome::Committed`], records the new one. The store is cloned
    /// into the saga, so it must be `Clone` (both shipped backends are).
    ///
    /// # Errors
    /// [`DeployError`] for infrastructure failures (engine, state store, or ledger
    /// (de)serialization). A clean rollback is `Ok(SagaOutcome::RolledBack)`, and
    /// an unrecoverable one is `Ok(SagaOutcome::PartialRollback)`.
    pub async fn run<S: StateStore + Clone>(&self, store: S) -> Result<SagaOutcome, DeployError> {
        let key = FraiseKey::new(self.fraise.clone(), self.environment.clone());

        let prior = match store.current_snapshot(&key).await? {
            Some(value) => serde_json::from_value(value)?,
            None => DeployRecord::default(),
        };

        let shared = Arc::new(DeployShared::new(self, prior));
        let saga = Saga::new(store.clone(), self.fraise.clone(), self.environment.clone())
            .with_step(shared.step(Phase::Preflight))
            .with_step(shared.step(Phase::Fetch))
            .with_step(shared.step(Phase::Migrate))
            .with_step(shared.step(Phase::Release))
            .with_step(shared.step(Phase::Health))
            .with_step(shared.step(Phase::Verify));

        let outcome = saga.run().await?;

        if matches!(outcome, SagaOutcome::Committed) {
            let record = shared.committed_record();
            store
                .record_snapshot(&key, &serde_json::to_value(&record)?)
                .await?;
        }

        Ok(outcome)
    }
}

/// Builder for [`SingleHostDeploy`]. All four adapters are required; the
/// [`AdapterCtx`] and migration `target` are optional.
pub struct SingleHostDeployBuilder {
    fraise: String,
    environment: String,
    host: HostId,
    ctx: Option<AdapterCtx>,
    target: Option<Revision>,
    artifact: Option<Arc<dyn ArtifactAdapter>>,
    migration: Option<Arc<dyn MigrationAdapter>>,
    service: Option<Arc<dyn ServiceAdapter>>,
    health: Option<Arc<dyn HealthAdapter>>,
}

impl SingleHostDeployBuilder {
    fn new(fraise: impl Into<String>, environment: impl Into<String>, host: HostId) -> Self {
        Self {
            fraise: fraise.into(),
            environment: environment.into(),
            host,
            ctx: None,
            target: None,
            artifact: None,
            migration: None,
            service: None,
            health: None,
        }
    }

    /// Provide the [`AdapterCtx`] passed to every adapter call. When omitted, a
    /// default context for the `(fraise, environment)` pair is used.
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

    /// Finish building.
    ///
    /// # Errors
    /// [`DeployBuildError`] naming the first required adapter that was not set.
    pub fn build(self) -> Result<SingleHostDeploy, DeployBuildError> {
        let ctx = self
            .ctx
            .unwrap_or_else(|| AdapterCtx::new(self.fraise.clone(), self.environment.clone()));
        Ok(SingleHostDeploy {
            artifact: self
                .artifact
                .ok_or(DeployBuildError::MissingAdapter("artifact"))?,
            migration: self
                .migration
                .ok_or(DeployBuildError::MissingAdapter("migration"))?,
            service: self
                .service
                .ok_or(DeployBuildError::MissingAdapter("service"))?,
            health: self
                .health
                .ok_or(DeployBuildError::MissingAdapter("health"))?,
            fraise: self.fraise,
            environment: self.environment,
            host: self.host,
            ctx,
            target: self.target,
        })
    }
}

/// A [`SingleHostDeployBuilder`] was missing a required adapter.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DeployBuildError {
    /// The named adapter axis was not set on the builder.
    #[error("single-host deploy requires a(n) {0} adapter")]
    MissingAdapter(&'static str),
}

/// The deploy state shared by every step: the adapters, the call context, the
/// durable rollback target (`prior`), and the in-run captures (`runtime`).
struct DeployShared {
    ctx: AdapterCtx,
    host: HostId,
    target: Option<Revision>,
    artifact: Arc<dyn ArtifactAdapter>,
    migration: Arc<dyn MigrationAdapter>,
    service: Arc<dyn ServiceAdapter>,
    health: Arc<dyn HealthAdapter>,
    /// The previously-committed release — the rollback target. Immutable for the
    /// run.
    prior: DeployRecord,
    /// State captured during this run, read back during compensation and commit.
    runtime: Mutex<DeployRuntime>,
}

/// State captured while a deploy runs forward.
#[derive(Default)]
struct DeployRuntime {
    /// The artifact staged by `fetch`, activated by `release`, and recorded into
    /// the new ledger on commit.
    staged: Option<StagedArtifact>,
    /// The revision live immediately before `migrate` ran `up` — the target
    /// `down_to` returns the database to on rollback.
    previous_revision: Option<Revision>,
    /// The revision live after `up` — recorded into the new ledger on commit.
    new_revision: Option<Revision>,
}

/// Which deploy step a [`DeployStep`] represents.
#[derive(Clone, Copy)]
enum Phase {
    Preflight,
    Fetch,
    Migrate,
    Release,
    Health,
    Verify,
}

impl Phase {
    /// The stable step name used in saga state, events, and spans.
    const fn step_name(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::Fetch => "fetch",
            Self::Migrate => "migrate",
            Self::Release => "release",
            Self::Health => "health",
            Self::Verify => "verify",
        }
    }
}

impl DeployShared {
    fn new(deploy: &SingleHostDeploy, prior: DeployRecord) -> Self {
        let mut ctx = deploy.ctx.clone();
        ctx.host = Some(deploy.host.clone());
        Self {
            ctx,
            host: deploy.host.clone(),
            target: deploy.target.clone(),
            artifact: Arc::clone(&deploy.artifact),
            migration: Arc::clone(&deploy.migration),
            service: Arc::clone(&deploy.service),
            health: Arc::clone(&deploy.health),
            prior,
            runtime: Mutex::new(DeployRuntime::default()),
        }
    }

    fn step(self: &Arc<Self>, phase: Phase) -> Box<dyn Step> {
        Box::new(DeployStep {
            shared: Arc::clone(self),
            phase,
        })
    }

    /// The ledger entry to persist on a successful commit.
    fn committed_record(&self) -> DeployRecord {
        let runtime = self.runtime.lock().expect("deploy runtime mutex poisoned");
        DeployRecord {
            active: runtime.staged.clone(),
            revision: runtime.new_revision.clone(),
        }
    }

    /// Map an adapter error into a saga step failure, preserving its rendered detail.
    fn failed(step: &str, error: &AdapterError) -> SagaError {
        SagaError::StepFailed {
            step: step.to_owned(),
            message: error.to_string(),
        }
    }

    async fn run_preflight(&self) -> Result<(), SagaError> {
        let described = self
            .migration
            .describe()
            .await
            .map_err(|e| Self::failed("preflight", &e))?;
        // Decision 4 gate: only call preflight if the adapter advertises it;
        // otherwise proceed at the operator's risk rather than fail.
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

    async fn run_fetch(&self) -> Result<(), SagaError> {
        let staged = self
            .artifact
            .stage(&self.ctx, &self.host)
            .await
            .map_err(|e| Self::failed("fetch", &e))?;
        self.runtime.lock().expect("runtime").staged = Some(staged);
        Ok(())
    }

    async fn run_migrate(&self) -> Result<(), SagaError> {
        // Capture the live pre-migration revision first, so compensation has an
        // exact target even if the durable ledger has drifted.
        let previous = self
            .migration
            .current_revision(&self.ctx)
            .await
            .map_err(|e| Self::failed("migrate", &e))?;
        self.runtime.lock().expect("runtime").previous_revision = previous;

        let outcome = self
            .migration
            .up(&self.ctx, self.target.clone())
            .await
            .map_err(|e| Self::failed("migrate", &e))?;

        // Scope the guard so it drops before returning (no lock held across the
        // function tail).
        {
            let mut runtime = self.runtime.lock().expect("runtime");
            runtime.new_revision = outcome.to.or_else(|| runtime.previous_revision.clone());
        }
        Ok(())
    }

    async fn undo_migrate(&self) -> Result<(), SagaError> {
        let previous = self
            .runtime
            .lock()
            .expect("runtime")
            .previous_revision
            .clone();
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

    async fn run_release(&self) -> Result<(), SagaError> {
        let staged = self
            .runtime
            .lock()
            .expect("runtime")
            .staged
            .clone()
            .ok_or_else(|| SagaError::StepFailed {
                step: "release".to_owned(),
                message: "no staged artifact to activate (fetch did not run)".to_owned(),
            })?;
        self.artifact
            .activate(&self.ctx, &self.host, &staged)
            .await
            .map_err(|e| Self::failed("release", &e))?;
        self.service
            .restart(&self.ctx, &self.host)
            .await
            .map_err(|e| Self::failed("release", &e))?;
        Ok(())
    }

    async fn undo_release(&self) -> Result<(), SagaError> {
        match &self.prior.active {
            Some(previous) => {
                // Re-activate the prior release, then restart, so the old version
                // is what comes back up (PRD §5.4: restore artifact, then restart).
                self.artifact
                    .activate(&self.ctx, &self.host, previous)
                    .await
                    .map_err(|e| Self::failed("release", &e))?;
                self.service
                    .restart(&self.ctx, &self.host)
                    .await
                    .map_err(|e| Self::failed("release", &e))?;
                Ok(())
            }
            None => Err(SagaError::StepFailed {
                step: "release".to_owned(),
                message: "cannot roll back artifact activation: no previously-active artifact is \
                          recorded (a failed first-ever deploy has no prior release to restore)"
                    .to_owned(),
            }),
        }
    }

    async fn run_health(&self) -> Result<(), SagaError> {
        let status = self
            .health
            .check(&self.ctx, &self.host)
            .await
            .map_err(|e| Self::failed("health", &e))?;
        if status.healthy {
            return Ok(());
        }
        let detail = status.detail.map(|d| format!(": {d}")).unwrap_or_default();
        Err(SagaError::StepFailed {
            step: "health".to_owned(),
            message: format!("health check reported the host unhealthy{detail}"),
        })
    }

    async fn run_verify(&self) -> Result<(), SagaError> {
        let report = self
            .migration
            .verify(&self.ctx)
            .await
            .map_err(|e| Self::failed("verify", &e))?;
        if report.ok {
            return Ok(());
        }
        let failed = report.checks.iter().filter(|check| !check.ok).count();
        Err(SagaError::StepFailed {
            step: "verify".to_owned(),
            message: format!("post-migration verify failed {failed} check(s)"),
        })
    }
}

/// One deploy step: a thin adapter from a [`Phase`] to the shared deploy logic.
struct DeployStep {
    shared: Arc<DeployShared>,
    phase: Phase,
}

#[async_trait]
impl Step for DeployStep {
    fn name(&self) -> &str {
        self.phase.step_name()
    }

    async fn forward(&self, _ctx: &StepContext) -> Result<(), SagaError> {
        match self.phase {
            Phase::Preflight => self.shared.run_preflight().await,
            Phase::Fetch => self.shared.run_fetch().await,
            Phase::Migrate => self.shared.run_migrate().await,
            Phase::Release => self.shared.run_release().await,
            Phase::Health => self.shared.run_health().await,
            Phase::Verify => self.shared.run_verify().await,
        }
    }

    async fn compensate(&self, _ctx: &StepContext) -> Result<(), SagaError> {
        match self.phase {
            Phase::Migrate => self.shared.undo_migrate().await,
            Phase::Release => self.shared.undo_release().await,
            // Inert steps have nothing to undo.
            Phase::Preflight | Phase::Fetch | Phase::Health | Phase::Verify => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DeployBuildError, DeployRecord, SingleHostDeploy};
    use crate::adapter_axes::{
        AdapterCtx, AdapterDescription, AdapterError, AdapterErrorKind, ArtifactAdapter,
        ArtifactRef, HealthAdapter, HealthStatus, HostId, MigrationAdapter, MigrationOutcome,
        PreflightIssue, PreflightReport, Revision, ServiceAdapter, ServiceStatus, Severity,
        StagedArtifact, VerifyReport,
    };
    use async_trait::async_trait;
    use fraisier_saga::saga::SagaOutcome;
    use fraisier_saga::state_store::{FilesystemStateStore, FraiseKey, StateStore};
    use std::sync::{Arc, Mutex};

    /// An ordered log of every adapter call, shared across the fakes.
    type Trail = Arc<Mutex<Vec<String>>>;

    fn log(trail: &Trail, entry: impl Into<String>) {
        trail.lock().expect("trail").push(entry.into());
    }

    fn exec_error(message: &str) -> AdapterError {
        AdapterError::new(AdapterErrorKind::Execution, message)
    }

    struct FakeArtifact {
        trail: Trail,
        fail_stage: bool,
    }

    #[async_trait]
    impl ArtifactAdapter for FakeArtifact {
        async fn stage(
            &self,
            _ctx: &AdapterCtx,
            _host: &HostId,
        ) -> Result<StagedArtifact, AdapterError> {
            log(&self.trail, "stage");
            if self.fail_stage {
                return Err(exec_error("stage failed"));
            }
            Ok(StagedArtifact {
                artifact: ArtifactRef {
                    id: "v-new".to_owned(),
                    checksum: None,
                },
                path: "/staging/v-new".into(),
            })
        }

        async fn activate(
            &self,
            _ctx: &AdapterCtx,
            _host: &HostId,
            staged: &StagedArtifact,
        ) -> Result<(), AdapterError> {
            log(&self.trail, format!("activate:{}", staged.artifact.id));
            Ok(())
        }

        async fn current(
            &self,
            _ctx: &AdapterCtx,
            _host: &HostId,
        ) -> Result<Option<ArtifactRef>, AdapterError> {
            // The composition never calls this for rollback (it uses the durable
            // ledger); present only to satisfy the trait.
            Ok(None)
        }
    }

    // Reason: each bool is an independent failure-injection toggle for a distinct
    // test scenario; a two-variant enum per flag (the lint's suggestion) would be
    // more ceremony than signal for a test fake.
    #[allow(clippy::struct_excessive_bools)]
    struct FakeMigration {
        trail: Trail,
        capabilities: Vec<String>,
        current: Option<Revision>,
        preflight_blocking: bool,
        fail_up: bool,
        verify_ok: bool,
        /// Mirror the real sqlx adapter: a forward deploy must call `up(None)`, so
        /// a non-null target reaching `up` is an error (the adapter has no
        /// `run_to`). Guards the [`run_migrate`] contract that `self.target` is
        /// `None` unless an operator explicitly pinned one.
        decline_targeted_up: bool,
    }

    impl FakeMigration {
        fn healthy(trail: &Trail) -> Self {
            Self {
                trail: trail.clone(),
                capabilities: vec!["preflight".to_owned(), "up".to_owned()],
                current: Some(Revision::new("rev-prev")),
                preflight_blocking: false,
                fail_up: false,
                verify_ok: true,
                decline_targeted_up: false,
            }
        }
    }

    #[async_trait]
    impl MigrationAdapter for FakeMigration {
        async fn describe(&self) -> Result<AdapterDescription, AdapterError> {
            log(&self.trail, "describe");
            Ok(AdapterDescription {
                name: "fake".to_owned(),
                version: "0".to_owned(),
                protocol_version: 1,
                capabilities: self.capabilities.clone(),
            })
        }

        async fn current_revision(
            &self,
            _ctx: &AdapterCtx,
        ) -> Result<Option<Revision>, AdapterError> {
            log(&self.trail, "current_revision");
            Ok(self.current.clone())
        }

        async fn up(
            &self,
            _ctx: &AdapterCtx,
            target: Option<Revision>,
        ) -> Result<MigrationOutcome, AdapterError> {
            log(&self.trail, "up");
            if self.decline_targeted_up && target.is_some() {
                return Err(exec_error(
                    "this adapter applies all pending migrations; a targeted up is unsupported",
                ));
            }
            if self.fail_up {
                return Err(exec_error("up failed"));
            }
            Ok(MigrationOutcome {
                from: self.current.clone(),
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
                ok: self.verify_ok,
                checks: Vec::new(),
            })
        }

        async fn preflight(&self, _ctx: &AdapterCtx) -> Result<PreflightReport, AdapterError> {
            log(&self.trail, "preflight");
            let issues = if self.preflight_blocking {
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
                ok: !self.preflight_blocking,
                issues,
            })
        }
    }

    struct FakeService {
        trail: Trail,
    }

    #[async_trait]
    impl ServiceAdapter for FakeService {
        async fn restart(&self, _ctx: &AdapterCtx, _host: &HostId) -> Result<(), AdapterError> {
            log(&self.trail, "restart");
            Ok(())
        }

        async fn status(
            &self,
            _ctx: &AdapterCtx,
            _host: &HostId,
        ) -> Result<ServiceStatus, AdapterError> {
            Ok(ServiceStatus {
                running: true,
                detail: None,
            })
        }
    }

    struct FakeHealth {
        trail: Trail,
        healthy: bool,
    }

    #[async_trait]
    impl HealthAdapter for FakeHealth {
        async fn check(
            &self,
            _ctx: &AdapterCtx,
            _host: &HostId,
        ) -> Result<HealthStatus, AdapterError> {
            log(&self.trail, "check");
            Ok(HealthStatus {
                healthy: self.healthy,
                detail: None,
            })
        }
    }

    /// Assemble a deploy from the four fakes.
    fn deploy(
        trail: &Trail,
        artifact: FakeArtifact,
        migration: FakeMigration,
        health: FakeHealth,
    ) -> SingleHostDeploy {
        SingleHostDeploy::builder("checkout", "production", HostId::new("localhost"))
            .artifact(Arc::new(artifact))
            .migration(Arc::new(migration))
            .service(Arc::new(FakeService {
                trail: trail.clone(),
            }))
            .health(Arc::new(health))
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

    /// Seed a prior committed release so rollback has a target to restore to.
    async fn seed_prior(store: &FilesystemStateStore) {
        let prior = DeployRecord {
            active: Some(StagedArtifact {
                artifact: ArtifactRef {
                    id: "v-old".to_owned(),
                    checksum: None,
                },
                path: "/staging/v-old".into(),
            }),
            revision: Some(Revision::new("rev-old")),
        };
        store
            .record_snapshot(&key(), &serde_json::to_value(&prior).expect("encode"))
            .await
            .expect("seed prior ledger");
    }

    fn drain(trail: &Trail) -> Vec<String> {
        trail.lock().expect("trail").clone()
    }

    #[tokio::test]
    async fn happy_path_commits_and_records_the_release_ledger() {
        let (_dir, store) = store();
        let trail = Trail::default();
        let plan = deploy(
            &trail,
            FakeArtifact {
                trail: trail.clone(),
                fail_stage: false,
            },
            FakeMigration::healthy(&trail),
            FakeHealth {
                trail: trail.clone(),
                healthy: true,
            },
        );

        let outcome = plan.run(store.clone()).await.expect("run");
        assert!(matches!(outcome, SagaOutcome::Committed), "got {outcome:?}");

        assert_eq!(
            drain(&trail),
            vec![
                "describe",
                "preflight",
                "stage",
                "current_revision",
                "up",
                "activate:v-new",
                "restart",
                "check",
                "verify",
            ]
        );

        // The committed deploy recorded its active artifact + revision for the
        // next deploy to roll back to.
        let snapshot = store
            .current_snapshot(&key())
            .await
            .expect("query")
            .expect("ledger recorded");
        let record: DeployRecord = serde_json::from_value(snapshot).expect("decode");
        assert_eq!(record.active.expect("active").artifact.id, "v-new");
        assert_eq!(record.revision, Some(Revision::new("rev-new")));
    }

    #[tokio::test]
    async fn forward_deploy_applies_all_pending_migrations_with_no_target() {
        // A forward deploy must call `up(None)` — apply everything pending. The
        // real sqlx reference adapter has no `run_to` and declines any targeted
        // `up` (-32013); `run_migrate` forwards `self.target`, which the CLI
        // deploy path never sets. This locks that contract: against an adapter
        // that errors on a non-null target, a deploy with no target still commits.
        let (_dir, store) = store();
        let trail = Trail::default();
        let mut migration = FakeMigration::healthy(&trail);
        migration.decline_targeted_up = true;
        let plan = deploy(
            &trail,
            FakeArtifact {
                trail: trail.clone(),
                fail_stage: false,
            },
            migration,
            FakeHealth {
                trail: trail.clone(),
                healthy: true,
            },
        );

        let outcome = plan.run(store).await.expect("run");
        assert!(
            matches!(outcome, SagaOutcome::Committed),
            "a forward deploy must call up(None) and commit; got {outcome:?}",
        );
        assert!(drain(&trail).iter().any(|e| e == "up"), "up was reached");
    }

    #[tokio::test]
    async fn health_failure_rolls_back_release_then_migration() {
        let (_dir, store) = store();
        seed_prior(&store).await;
        let trail = Trail::default();
        let plan = deploy(
            &trail,
            FakeArtifact {
                trail: trail.clone(),
                fail_stage: false,
            },
            FakeMigration::healthy(&trail),
            FakeHealth {
                trail: trail.clone(),
                healthy: false,
            },
        );

        let outcome = plan.run(store).await.expect("run completes with rollback");
        assert!(
            matches!(&outcome, SagaOutcome::RolledBack { failed_step, .. } if failed_step == "health"),
            "got {outcome:?}"
        );

        // Forward through health, then compensate release (re-activate the prior
        // artifact + restart) and migrate (down_to the captured prior revision),
        // in that reverse order.
        assert_eq!(
            drain(&trail),
            vec![
                "describe",
                "preflight",
                "stage",
                "current_revision",
                "up",
                "activate:v-new",
                "restart",
                "check",
                "activate:v-old",
                "restart",
                "down_to:rev-prev",
            ]
        );
    }

    #[tokio::test]
    async fn migration_failure_compensates_nothing_destructive() {
        let (_dir, store) = store();
        seed_prior(&store).await;
        let trail = Trail::default();
        let mut migration = FakeMigration::healthy(&trail);
        migration.fail_up = true;
        let plan = deploy(
            &trail,
            FakeArtifact {
                trail: trail.clone(),
                fail_stage: false,
            },
            migration,
            FakeHealth {
                trail: trail.clone(),
                healthy: true,
            },
        );

        let outcome = plan.run(store).await.expect("run completes with rollback");
        assert!(
            matches!(&outcome, SagaOutcome::RolledBack { failed_step, .. } if failed_step == "migrate"),
            "got {outcome:?}"
        );

        // `up` failed, so migrate never completed and is not compensated; the
        // artifact is never activated and the database is never rolled back.
        let trail = drain(&trail);
        assert_eq!(
            trail,
            vec!["describe", "preflight", "stage", "current_revision", "up"]
        );
        assert!(!trail.iter().any(|e| e.starts_with("activate")));
        assert!(!trail.iter().any(|e| e.starts_with("down_to")));
    }

    #[tokio::test]
    async fn preflight_block_fails_before_any_mutation() {
        let (_dir, store) = store();
        let trail = Trail::default();
        let mut migration = FakeMigration::healthy(&trail);
        migration.preflight_blocking = true;
        let plan = deploy(
            &trail,
            FakeArtifact {
                trail: trail.clone(),
                fail_stage: false,
            },
            migration,
            FakeHealth {
                trail: trail.clone(),
                healthy: true,
            },
        );

        let outcome = plan.run(store).await.expect("run");
        assert!(
            matches!(&outcome, SagaOutcome::RolledBack { failed_step, .. } if failed_step == "preflight"),
            "got {outcome:?}"
        );
        assert_eq!(drain(&trail), vec!["describe", "preflight"]);
    }

    #[tokio::test]
    async fn first_deploy_failure_yields_partial_rollback() {
        // No prior ledger: a failed first-ever deploy has no committed state to
        // restore, so re-activation is impossible and the saga must say so.
        let (_dir, store) = store();
        let trail = Trail::default();
        let plan = deploy(
            &trail,
            FakeArtifact {
                trail: trail.clone(),
                fail_stage: false,
            },
            FakeMigration::healthy(&trail),
            FakeHealth {
                trail: trail.clone(),
                healthy: false,
            },
        );

        let outcome = plan.run(store).await.expect("run completes");
        assert!(
            matches!(outcome, SagaOutcome::PartialRollback { .. }),
            "a first-deploy rollback with no prior release is a PartialRollback, got {outcome:?}"
        );
    }

    #[tokio::test]
    async fn build_requires_every_adapter() {
        let trail = Trail::default();
        let err = SingleHostDeploy::builder("checkout", "production", HostId::new("localhost"))
            .artifact(Arc::new(FakeArtifact {
                trail,
                fail_stage: false,
            }))
            .build()
            .expect_err("missing migration/service/health");
        assert!(matches!(err, DeployBuildError::MissingAdapter("migration")));
    }
}
