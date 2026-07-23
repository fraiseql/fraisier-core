//! Single-host deploy composition (PRD §5.2) — the deploy layer over the saga.
//!
//! [`SingleHostDeploy`] wraps the four single-host adapter axes (artifact,
//! migration, service, health — load-balancing is a multi-host concern) into
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

use std::sync::Arc;

use async_trait::async_trait;
use fraisier_saga::saga::{Saga, SagaError, SagaOutcome, Step, StepContext};
use fraisier_saga::state_store::{FraiseKey, StateStore, StateStoreError};
use serde::{Deserialize, Serialize};

use crate::adapter_axes::{
    AdapterCtx, AdapterError, ArtifactAdapter, HealthAdapter, HostId, MigrationAdapter, Revision,
    ServiceAdapter, Severity, StagedArtifact,
};
use crate::restore_rehearsal::{run_restore_rehearsal, BackupSource, RehearsalDb};

/// Which preflight a deploy runs before touching the live database.
///
/// Selected by `[migration].preflight_mode` in `fraisier.toml`; the legacy
/// `forward_compatible_lint` boolean maps onto [`Live`](Self::Live) /
/// [`Off`](Self::Off) for back-compatibility.
///
/// # Example
/// ```
/// # use fraisier_core::single_host::PreflightMode;
/// assert_eq!(PreflightMode::default(), PreflightMode::Live);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreflightMode {
    /// Run the migration adapter's forward-compatibility lint against the **live**
    /// database (the default; equivalent to legacy `forward_compatible_lint = true`).
    #[default]
    Live,
    /// In addition to the live lint, rehearse a real restore + migrate on a
    /// throwaway copy of the database *before* touching the live one
    /// (a DR-grade preflight). See [`crate::restore_rehearsal`].
    RestoreRehearsal,
    /// Skip every preflight (equivalent to legacy `forward_compatible_lint = false`).
    Off,
}

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
    /// When set, this run is a deliberate rollback: the migrate step runs
    /// `down_to(this)` instead of `up`, and its compensation goes back `up`.
    rollback_to: Option<Revision>,
    forward_compatible_lint: bool,
    /// When set, the preflight step also rehearses a restore + migrate on a
    /// throwaway DB (`[migration].preflight_mode = "restore_rehearsal"`).
    restore_rehearsal: Option<Arc<dyn RehearsalDb>>,
    backup_source: BackupSource,
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
            .field("forward_compatible_lint", &self.forward_compatible_lint)
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
            .with_step(shared.step(Phase::Activate))
            .with_step(shared.step(Phase::Restart))
            .with_step(shared.step(Phase::Health))
            .with_step(shared.step(Phase::Verify));

        let mut runtime = DeployRuntime::default();
        let outcome = saga.run_with_state(&mut runtime).await?;

        if matches!(outcome, SagaOutcome::Committed) {
            let record = runtime.committed_record();
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
    rollback_to: Option<Revision>,
    forward_compatible_lint: bool,
    restore_rehearsal: Option<Arc<dyn RehearsalDb>>,
    backup_source: BackupSource,
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
            rollback_to: None,
            // Default on: forward-compat preflight runs whenever the adapter
            // advertises it, unless the operator opts out (PRD G11 / Decision 4).
            forward_compatible_lint: true,
            restore_rehearsal: None,
            backup_source: BackupSource::FreshDump,
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

    /// Make this run a deliberate rollback to `revision`: the migrate step runs
    /// `down_to(revision)` instead of `up`, its compensation goes back `up` to the
    /// pre-rollback revision, and (as always) the artifact for the rolled-back-to
    /// version is staged + activated. Mutually exclusive with [`target`] in intent.
    #[must_use]
    pub fn rollback_to(mut self, revision: Revision) -> Self {
        self.rollback_to = Some(revision);
        self
    }

    /// Whether to run the migration adapter's forward-compatibility `preflight`
    /// lint before deploying (default `true`). When `false`, the preflight step is
    /// skipped even if the adapter advertises the capability — the operator's
    /// opt-out from `[migration].forward_compatible_lint`.
    #[must_use]
    pub const fn forward_compatible_lint(mut self, enabled: bool) -> Self {
        self.forward_compatible_lint = enabled;
        self
    }

    /// Enable the restore-rehearsal preflight (`[migration].preflight_mode =
    /// "restore_rehearsal"`): before migrating the live DB, `db` provisions a
    /// throwaway copy seeded from `backup` and the pending migrations are rehearsed
    /// there. Complementary to [`Self::forward_compatible_lint`]; both can run.
    #[must_use]
    pub fn restore_rehearsal(mut self, db: Arc<dyn RehearsalDb>, backup: BackupSource) -> Self {
        self.restore_rehearsal = Some(db);
        self.backup_source = backup;
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
            rollback_to: self.rollback_to,
            forward_compatible_lint: self.forward_compatible_lint,
            restore_rehearsal: self.restore_rehearsal,
            backup_source: self.backup_source,
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

/// The deploy state shared by every step: the adapters, the call context, and
/// the durable rollback target (`prior`). All fields are immutable for the run —
/// the mutable in-run captures live in [`DeployRuntime`], which the saga engine
/// threads to each step as `&mut`.
struct DeployShared {
    ctx: AdapterCtx,
    host: HostId,
    target: Option<Revision>,
    rollback_to: Option<Revision>,
    forward_compatible_lint: bool,
    restore_rehearsal: Option<Arc<dyn RehearsalDb>>,
    backup_source: BackupSource,
    artifact: Arc<dyn ArtifactAdapter>,
    migration: Arc<dyn MigrationAdapter>,
    service: Arc<dyn ServiceAdapter>,
    health: Arc<dyn HealthAdapter>,
    /// The previously-committed release — the rollback target. Immutable for the
    /// run.
    prior: DeployRecord,
}

/// The mutable run state captured while a deploy runs forward, threaded to every
/// step by the saga engine as `&mut DeployRuntime` (no lock, no `Arc`).
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

impl DeployRuntime {
    /// The ledger entry to persist on a successful commit.
    fn committed_record(&self) -> DeployRecord {
        DeployRecord {
            active: self.staged.clone(),
            revision: self.new_revision.clone(),
        }
    }
}

/// Which deploy step a [`DeployStep`] represents.
#[derive(Clone, Copy)]
enum Phase {
    Preflight,
    Fetch,
    Migrate,
    // `Release` is split into `Activate` then `Restart` so each step is a single
    // compensable mutation: if `Restart` fails, the saga compensates the completed
    // `Activate` step (re-activating + restarting the prior release) rather than
    // leaving `current` stranded on a release that never came up.
    Activate,
    Restart,
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
            Self::Activate => "activate",
            Self::Restart => "restart",
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
            rollback_to: deploy.rollback_to.clone(),
            forward_compatible_lint: deploy.forward_compatible_lint,
            restore_rehearsal: deploy.restore_rehearsal.clone(),
            backup_source: deploy.backup_source.clone(),
            artifact: Arc::clone(&deploy.artifact),
            migration: Arc::clone(&deploy.migration),
            service: Arc::clone(&deploy.service),
            health: Arc::clone(&deploy.health),
            prior,
        }
    }

    fn step(self: &Arc<Self>, phase: Phase) -> Box<dyn Step<DeployRuntime>> {
        Box::new(DeployStep {
            shared: Arc::clone(self),
            phase,
        })
    }

    /// Map an adapter error into a saga step failure, preserving its rendered detail.
    fn failed(step: &str, error: &AdapterError) -> SagaError {
        SagaError::StepFailed {
            step: step.to_owned(),
            message: error.to_string(),
        }
    }

    /// The preflight step, dispatched by mode (`[migration].preflight_mode`): the
    /// forward-compatibility lint runs against the live DB (`live`/`restore_rehearsal`)
    /// and the DR-grade restore rehearsal runs additionally (`restore_rehearsal`).
    /// `off` sets both off, so the step is an inert no-op.
    async fn run_preflight(&self) -> Result<(), SagaError> {
        if self.forward_compatible_lint {
            self.run_forward_compat_lint().await?;
        }
        // The restore rehearsal proves a *forward* migrate applies; it is irrelevant
        // to a deliberate rollback (which migrates `down`), so skip it there.
        if let Some(db) = &self.restore_rehearsal {
            if self.rollback_to.is_none() {
                self.run_restore_rehearsal_step(db.as_ref()).await?;
            }
        }
        Ok(())
    }

    /// Run the migration adapter's forward-compatibility lint against the **live**
    /// database. The adapter must advertise the `preflight` capability or the lint
    /// is skipped (proceed at the operator's risk rather than fail — Decision 4).
    async fn run_forward_compat_lint(&self) -> Result<(), SagaError> {
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

    /// Rehearse a restore + migrate on a throwaway copy of the database before the
    /// live deploy touches anything. A failure aborts the deploy (it is the first
    /// step, so nothing has changed yet) with a message naming the escape hatch.
    async fn run_restore_rehearsal_step(&self, db: &dyn RehearsalDb) -> Result<(), SagaError> {
        run_restore_rehearsal(db, self.migration.as_ref(), &self.ctx, &self.backup_source)
            .await
            .map(|_report| ())
            .map_err(|error| SagaError::StepFailed {
                step: "preflight".to_owned(),
                message: format!(
                    "restore-rehearsal preflight failed: {error}. The pending migrations did not \
                     apply cleanly on a fresh restore. Fix the migrations, or bypass this check \
                     with `--skip-preflight` (or `[migration].preflight_mode = \"off\"`)."
                ),
            })
    }

    async fn run_fetch(&self, runtime: &mut DeployRuntime) -> Result<(), SagaError> {
        let staged = self
            .artifact
            .stage(&self.ctx, &self.host)
            .await
            .map_err(|e| Self::failed("fetch", &e))?;
        runtime.staged = Some(staged);
        Ok(())
    }

    /// The migration context for this run, with `workdir` pointed at the staged
    /// release directory when the artifact was staged **locally**.
    ///
    /// This is what lets a source-run migration run *from the release it was cut
    /// from* rather than the operator's invocation directory: a `command`-adapter
    /// `up = "bash scripts/deploy/prepare.sh"` resolves the script against the
    /// release, and a confiture `--migrations-dir` given relatively resolves the
    /// same way. Scoped to the migrate step (Preflight, which runs before `fetch`,
    /// keeps the base workdir), so the release is always present when it applies.
    ///
    /// The override is gated on the staged path existing as a local directory, so
    /// a deploy with no artifact axis — or a single-host stage whose path lives on
    /// a remote host — keeps the base `workdir` unchanged.
    fn migration_ctx(&self, runtime: &DeployRuntime) -> AdapterCtx {
        let mut ctx = self.ctx.clone();
        if let Some(staged) = &runtime.staged {
            if staged.path.is_dir() {
                ctx.workdir.clone_from(&staged.path);
            }
        }
        ctx
    }

    async fn run_migrate(&self, runtime: &mut DeployRuntime) -> Result<(), SagaError> {
        let ctx = self.migration_ctx(runtime);
        // Capture the live pre-migration revision first, so compensation has an
        // exact target even if the durable ledger has drifted.
        let previous = self
            .migration
            .current_revision(&ctx)
            .await
            .map_err(|e| Self::failed("migrate", &e))?;
        runtime.previous_revision = previous;

        if let Some(target) = &self.rollback_to {
            // Deliberate rollback: take the database *down* to the target revision.
            self.migration
                .down_to(&ctx, target.clone())
                .await
                .map_err(|e| Self::failed("migrate", &e))?;
            runtime.new_revision = Some(target.clone());
        } else {
            let outcome = self
                .migration
                .up(&ctx, self.target.clone())
                .await
                .map_err(|e| Self::failed("migrate", &e))?;
            runtime.new_revision = outcome.to.or_else(|| runtime.previous_revision.clone());
        }
        Ok(())
    }

    async fn undo_migrate(&self, runtime: &DeployRuntime) -> Result<(), SagaError> {
        let Some(previous) = runtime.previous_revision.clone() else {
            return Err(SagaError::StepFailed {
                step: "migrate".to_owned(),
                message: "cannot roll back the initial migration: there is no prior revision to \
                          return the database to (a failed first-ever deploy has no committed \
                          state to restore)"
                    .to_owned(),
            });
        };
        // Compensation undoes whatever `run_migrate` did: a forward deploy went
        // `up`, so it comes back `down_to(previous)`; a rollback went `down_to`, so
        // it goes back `up` to the pre-rollback revision. Runs from the same
        // release-relative workdir the forward migration used.
        let ctx = self.migration_ctx(runtime);
        let result = if self.rollback_to.is_some() {
            self.migration.up(&ctx, Some(previous)).await.map(|_| ())
        } else {
            self.migration.down_to(&ctx, previous).await.map(|_| ())
        };
        result.map_err(|e| Self::failed("migrate", &e))
    }

    async fn run_activate(&self, runtime: &DeployRuntime) -> Result<(), SagaError> {
        let staged = runtime
            .staged
            .clone()
            .ok_or_else(|| SagaError::StepFailed {
                step: "activate".to_owned(),
                message: "no staged artifact to activate (fetch did not run)".to_owned(),
            })?;
        self.artifact
            .activate(&self.ctx, &self.host, &staged)
            .await
            .map_err(|e| Self::failed("activate", &e))
    }

    async fn run_restart(&self) -> Result<(), SagaError> {
        self.service
            .restart(&self.ctx, &self.host)
            .await
            .map_err(|e| Self::failed("restart", &e))
    }

    /// Compensation for the `Activate` step. Reached whenever a step at or after
    /// `Activate` fails (a failed `Restart`, an unhealthy release, a failed
    /// verify), so a release that activated but never came up healthy is undone
    /// rather than left live: re-activate the prior release, then restart it
    /// (PRD §5.4: restore artifact, then restart). A first-ever deploy has no
    /// prior to restore — the saga surfaces that as `PartialRollback`.
    async fn undo_activate(&self) -> Result<(), SagaError> {
        match &self.prior.active {
            Some(previous) => {
                self.artifact
                    .activate(&self.ctx, &self.host, previous)
                    .await
                    .map_err(|e| Self::failed("activate", &e))?;
                self.service
                    .restart(&self.ctx, &self.host)
                    .await
                    .map_err(|e| Self::failed("activate", &e))?;
                Ok(())
            }
            None => Err(SagaError::StepFailed {
                step: "activate".to_owned(),
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
impl Step<DeployRuntime> for DeployStep {
    fn name(&self) -> &str {
        self.phase.step_name()
    }

    async fn forward(
        &self,
        _ctx: &StepContext,
        runtime: &mut DeployRuntime,
    ) -> Result<(), SagaError> {
        match self.phase {
            Phase::Preflight => self.shared.run_preflight().await,
            Phase::Fetch => self.shared.run_fetch(runtime).await,
            Phase::Migrate => self.shared.run_migrate(runtime).await,
            Phase::Activate => self.shared.run_activate(runtime).await,
            Phase::Restart => self.shared.run_restart().await,
            Phase::Health => self.shared.run_health().await,
            Phase::Verify => self.shared.run_verify().await,
        }
    }

    async fn compensate(
        &self,
        _ctx: &StepContext,
        runtime: &mut DeployRuntime,
    ) -> Result<(), SagaError> {
        match self.phase {
            Phase::Migrate => self.shared.undo_migrate(runtime).await,
            // Undoing the activation re-points + restarts the prior release; the
            // restart step itself has nothing to undo (its compensation is the
            // Activate step's, run next in reverse order).
            Phase::Activate => self.shared.undo_activate().await,
            // Inert steps have nothing to undo.
            Phase::Preflight | Phase::Fetch | Phase::Restart | Phase::Health | Phase::Verify => {
                Ok(())
            }
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
    use crate::restore_rehearsal::{BackupSource, RehearsalDb, RehearsalError, ThrowawayDb};
    use async_trait::async_trait;
    use fraisier_saga::saga::SagaOutcome;
    use fraisier_saga::state_store::{FilesystemStateStore, FraiseKey, StateStore};
    use std::path::PathBuf;
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

    /// An artifact fake whose `stage` reports a caller-provided (real) path, so a
    /// test can assert the migrate step runs from that staged release directory.
    struct StagedPathArtifact {
        trail: Trail,
        path: PathBuf,
    }

    #[async_trait]
    impl ArtifactAdapter for StagedPathArtifact {
        async fn stage(
            &self,
            _ctx: &AdapterCtx,
            _host: &HostId,
        ) -> Result<StagedArtifact, AdapterError> {
            log(&self.trail, "stage");
            Ok(StagedArtifact {
                artifact: ArtifactRef {
                    id: "v-new".to_owned(),
                    checksum: None,
                },
                path: self.path.clone(),
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
        /// The `ctx.workdir` seen by each migration call, for asserting the
        /// migrate step runs from the staged release directory.
        seen_workdirs: Arc<Mutex<Vec<PathBuf>>>,
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
                seen_workdirs: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn record_workdir(&self, ctx: &AdapterCtx) {
            self.seen_workdirs
                .lock()
                .expect("workdirs")
                .push(ctx.workdir.clone());
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
            ctx: &AdapterCtx,
        ) -> Result<Option<Revision>, AdapterError> {
            self.record_workdir(ctx);
            log(&self.trail, "current_revision");
            Ok(self.current.clone())
        }

        async fn up(
            &self,
            ctx: &AdapterCtx,
            target: Option<Revision>,
        ) -> Result<MigrationOutcome, AdapterError> {
            self.record_workdir(ctx);
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
            ctx: &AdapterCtx,
            target: Revision,
        ) -> Result<MigrationOutcome, AdapterError> {
            self.record_workdir(ctx);
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
                window_safe: None,
            })
        }
    }

    struct FakeService {
        trail: Trail,
        /// Number of upcoming `restart` calls to fail before succeeding. `1` fails
        /// the new release's restart while letting the rollback's restart of the
        /// prior release succeed.
        fail_restarts: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl ServiceAdapter for FakeService {
        async fn restart(&self, _ctx: &AdapterCtx, _host: &HostId) -> Result<(), AdapterError> {
            log(&self.trail, "restart");
            let fail = {
                let mut remaining = self.fail_restarts.lock().expect("fail_restarts");
                let fail = *remaining > 0;
                *remaining = remaining.saturating_sub(1);
                fail
            };
            if fail {
                return Err(exec_error("restart failed"));
            }
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
                fail_restarts: Arc::new(Mutex::new(0)),
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
    async fn migrate_runs_from_the_staged_release_directory() {
        // A source-run deploy stages the release, then its command/confiture
        // migration must run *from* that release (not the operator's invocation
        // cwd). The migrate ctx.workdir must be the staged path, captured after
        // fetch — proving the fix for the escape-hatch adapter's release coupling.
        let (_dir, store) = store();
        let release = tempfile::tempdir().expect("release dir");
        let trail = Trail::default();
        let migration = FakeMigration::healthy(&trail);
        let seen = migration.seen_workdirs.clone();

        let plan = SingleHostDeploy::builder("checkout", "production", HostId::new("localhost"))
            .artifact(Arc::new(StagedPathArtifact {
                trail: trail.clone(),
                path: release.path().to_path_buf(),
            }))
            .migration(Arc::new(migration))
            .service(Arc::new(FakeService {
                trail: trail.clone(),
                fail_restarts: Arc::new(Mutex::new(0)),
            }))
            .health(Arc::new(FakeHealth {
                trail: trail.clone(),
                healthy: true,
            }))
            .build()
            .expect("build");

        let outcome = plan.run(store).await.expect("run");
        assert!(matches!(outcome, SagaOutcome::Committed), "got {outcome:?}");

        let seen = seen.lock().expect("workdirs").clone();
        assert!(!seen.is_empty(), "the migration axis ran");
        assert!(
            seen.iter().all(|dir| dir == release.path()),
            "every migration call ran from the staged release {:?}, saw {seen:?}",
            release.path()
        );
    }

    #[tokio::test]
    async fn migrate_keeps_base_workdir_when_no_local_release_is_staged() {
        // With the default staged path (`/staging/v-new`, absent on this host —
        // as for a remote single-host stage), the override must not apply: the
        // migration keeps the base workdir rather than cd-ing into a missing dir.
        let (_dir, store) = store();
        let trail = Trail::default();
        let migration = FakeMigration::healthy(&trail);
        let seen = migration.seen_workdirs.clone();

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
        assert!(matches!(outcome, SagaOutcome::Committed), "got {outcome:?}");

        let seen = seen.lock().expect("workdirs").clone();
        assert!(
            seen.iter().all(|dir| dir == &PathBuf::from(".")),
            "a non-local stage keeps the base workdir, saw {seen:?}"
        );
    }

    #[tokio::test]
    async fn rollback_migrates_down_to_the_target_and_commits() {
        let (_dir, store) = store();
        seed_prior(&store).await;
        let trail = Trail::default();
        let plan = SingleHostDeploy::builder("checkout", "production", HostId::new("localhost"))
            .artifact(Arc::new(FakeArtifact {
                trail: trail.clone(),
                fail_stage: false,
            }))
            .migration(Arc::new(FakeMigration::healthy(&trail)))
            .service(Arc::new(FakeService {
                trail: trail.clone(),
                fail_restarts: Arc::new(Mutex::new(0)),
            }))
            .health(Arc::new(FakeHealth {
                trail: trail.clone(),
                healthy: true,
            }))
            .rollback_to(Revision::new("rev-old"))
            .build()
            .expect("build");

        let outcome = plan.run(store.clone()).await.expect("run");
        assert!(matches!(outcome, SagaOutcome::Committed), "got {outcome:?}");

        let trail = drain(&trail);
        assert!(
            trail.contains(&"down_to:rev-old".to_owned()),
            "a rollback migrates down to the target: {trail:?}"
        );
        assert!(
            !trail.contains(&"up".to_owned()),
            "a rollback must not migrate up on the forward path: {trail:?}"
        );
        // The ledger now reflects the rolled-back-to revision.
        let record: DeployRecord = serde_json::from_value(
            store
                .current_snapshot(&key())
                .await
                .expect("query")
                .expect("ledger"),
        )
        .expect("decode");
        assert_eq!(record.revision, Some(Revision::new("rev-old")));
    }

    #[tokio::test]
    async fn rollback_compensation_migrates_back_up() {
        // A rollback whose health check fails must compensate by going back *up* to
        // the pre-rollback revision — the inverse of the rollback's `down_to`.
        let (_dir, store) = store();
        seed_prior(&store).await;
        let trail = Trail::default();
        let plan = SingleHostDeploy::builder("checkout", "production", HostId::new("localhost"))
            .artifact(Arc::new(FakeArtifact {
                trail: trail.clone(),
                fail_stage: false,
            }))
            .migration(Arc::new(FakeMigration::healthy(&trail)))
            .service(Arc::new(FakeService {
                trail: trail.clone(),
                fail_restarts: Arc::new(Mutex::new(0)),
            }))
            .health(Arc::new(FakeHealth {
                trail: trail.clone(),
                healthy: false,
            }))
            .rollback_to(Revision::new("rev-old"))
            .build()
            .expect("build");

        let outcome = plan.run(store).await.expect("run completes with rollback");
        assert!(
            matches!(outcome, SagaOutcome::RolledBack { .. }),
            "got {outcome:?}"
        );
        let trail = drain(&trail);
        assert!(
            trail.contains(&"down_to:rev-old".to_owned()),
            "forward rollback went down: {trail:?}"
        );
        assert!(
            trail.contains(&"up".to_owned()),
            "compensation goes back up to the pre-rollback revision: {trail:?}"
        );
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
    async fn forward_compatible_lint_disabled_skips_the_preflight_step() {
        // Opting out (`forward_compatible_lint(false)`) skips the preflight step
        // entirely — no `describe`, no `preflight` — even when the adapter
        // advertises preflight and *would* block. The deploy proceeds to commit.
        let (_dir, store) = store();
        let trail = Trail::default();
        let mut migration = FakeMigration::healthy(&trail);
        migration.preflight_blocking = true; // would roll back at preflight if it ran
        let plan = SingleHostDeploy::builder("checkout", "production", HostId::new("localhost"))
            .artifact(Arc::new(FakeArtifact {
                trail: trail.clone(),
                fail_stage: false,
            }))
            .migration(Arc::new(migration))
            .service(Arc::new(FakeService {
                trail: trail.clone(),
                fail_restarts: Arc::new(Mutex::new(0)),
            }))
            .health(Arc::new(FakeHealth {
                trail: trail.clone(),
                healthy: true,
            }))
            .forward_compatible_lint(false)
            .build()
            .expect("all adapters provided");

        let outcome = plan.run(store).await.expect("run");
        assert!(
            matches!(outcome, SagaOutcome::Committed),
            "the lint opt-out lets the deploy commit despite a blocking preflight; got {outcome:?}",
        );
        let trail = drain(&trail);
        assert!(
            !trail.iter().any(|e| e == "describe" || e == "preflight"),
            "the preflight step made no adapter call: {trail:?}",
        );
    }

    /// A fake throwaway-DB lifecycle for the restore-rehearsal saga tests.
    struct FakeRehearsalDb {
        trail: Trail,
        fail: bool,
    }

    #[async_trait]
    impl RehearsalDb for FakeRehearsalDb {
        async fn provision(
            &self,
            live_dsn: &str,
            _backup: &BackupSource,
        ) -> Result<ThrowawayDb, RehearsalError> {
            log(&self.trail, "rehearsal:provision");
            if self.fail {
                return Err(RehearsalError::Provision("seed restore failed".to_owned()));
            }
            Ok(ThrowawayDb {
                dsn: format!("{live_dsn}_rehearsal"),
                name: "throwaway".to_owned(),
            })
        }

        async fn teardown(&self, _db: &ThrowawayDb) -> Result<(), String> {
            log(&self.trail, "rehearsal:teardown");
            Ok(())
        }
    }

    /// A context whose `DATABASE_URL` resolves in-process (so the rehearsal can
    /// clone it and redirect at the throwaway DB).
    fn ctx_with_dsn() -> AdapterCtx {
        AdapterCtx::new("checkout", "production")
            .with_resolved_secret("DATABASE_URL", "postgres://u:pw@h/app")
    }

    #[tokio::test]
    async fn restore_rehearsal_runs_before_migrate_on_the_live_db() {
        // `preflight_mode = "restore_rehearsal"` provisions + migrates a throwaway
        // copy *before* the live fetch/migrate, then tears it down — and the deploy
        // still commits.
        let (_dir, store) = store();
        let trail = Trail::default();
        let plan = SingleHostDeploy::builder("checkout", "production", HostId::new("localhost"))
            .context(ctx_with_dsn())
            .artifact(Arc::new(FakeArtifact {
                trail: trail.clone(),
                fail_stage: false,
            }))
            .migration(Arc::new(FakeMigration::healthy(&trail)))
            .service(Arc::new(FakeService {
                trail: trail.clone(),
                fail_restarts: Arc::new(Mutex::new(0)),
            }))
            .health(Arc::new(FakeHealth {
                trail: trail.clone(),
                healthy: true,
            }))
            .restore_rehearsal(
                Arc::new(FakeRehearsalDb {
                    trail: trail.clone(),
                    fail: false,
                }),
                BackupSource::FreshDump,
            )
            .build()
            .expect("all adapters provided");

        let outcome = plan.run(store).await.expect("run");
        assert!(matches!(outcome, SagaOutcome::Committed), "got {outcome:?}");

        let trail = drain(&trail);
        let provision = trail
            .iter()
            .position(|e| e == "rehearsal:provision")
            .expect("rehearsal provisioned a throwaway db");
        let stage = trail
            .iter()
            .position(|e| e == "stage")
            .expect("the live fetch ran");
        assert!(
            provision < stage,
            "the rehearsal must run before the live fetch/migrate: {trail:?}"
        );
        assert!(
            trail.iter().any(|e| e == "rehearsal:teardown"),
            "the throwaway db is torn down: {trail:?}"
        );
        // One rehearsal `up` (throwaway) + one live `up`.
        assert_eq!(
            trail.iter().filter(|e| *e == "up").count(),
            2,
            "the pending set is rehearsed and then applied live: {trail:?}"
        );
    }

    #[tokio::test]
    async fn restore_rehearsal_failure_aborts_before_touching_the_live_db() {
        // A failed rehearsal aborts the deploy at the preflight step — the live
        // fetch/migrate never run.
        let (_dir, store) = store();
        let trail = Trail::default();
        let plan = SingleHostDeploy::builder("checkout", "production", HostId::new("localhost"))
            .context(ctx_with_dsn())
            .artifact(Arc::new(FakeArtifact {
                trail: trail.clone(),
                fail_stage: false,
            }))
            .migration(Arc::new(FakeMigration::healthy(&trail)))
            .service(Arc::new(FakeService {
                trail: trail.clone(),
                fail_restarts: Arc::new(Mutex::new(0)),
            }))
            .health(Arc::new(FakeHealth {
                trail: trail.clone(),
                healthy: true,
            }))
            .restore_rehearsal(
                Arc::new(FakeRehearsalDb {
                    trail: trail.clone(),
                    fail: true,
                }),
                BackupSource::FreshDump,
            )
            .build()
            .expect("all adapters provided");

        let outcome = plan.run(store).await.expect("run completes");
        assert!(
            !matches!(outcome, SagaOutcome::Committed),
            "a failed rehearsal must not commit; got {outcome:?}"
        );
        let trail = drain(&trail);
        assert!(
            !trail.iter().any(|e| e == "stage"),
            "the live deploy never started after a failed rehearsal: {trail:?}"
        );
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
    async fn failed_migrate_leaves_the_prior_version_in_the_ledger() {
        // The release ledger is the only durable "what version is live" signal,
        // and it is written *only* post-commit. A deploy that fails before commit
        // (here at `migrate`) must therefore leave the ledger naming the prior
        // version — never the in-flight one. This is the property #21's
        // health-reported version relies on: it can never surface a version that
        // was not actually deployed.
        let (_dir, store) = store();
        seed_prior(&store).await; // ledger starts at v-old / rev-old
        let trail = Trail::default();
        let mut migration = FakeMigration::healthy(&trail);
        migration.fail_up = true; // the in-flight deploy would have been v-new
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

        let outcome = plan.run(store.clone()).await.expect("run");
        assert!(
            matches!(&outcome, SagaOutcome::RolledBack { failed_step, .. } if failed_step == "migrate"),
            "got {outcome:?}"
        );

        // The committed ledger still names v-old / rev-old — the in-flight v-new
        // was never recorded because commit never happened.
        let snapshot = store
            .current_snapshot(&key())
            .await
            .expect("query")
            .expect("prior ledger intact");
        let record: DeployRecord = serde_json::from_value(snapshot).expect("decode");
        assert_eq!(record.active.expect("active").artifact.id, "v-old");
        assert_eq!(record.revision, Some(Revision::new("rev-old")));
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
    async fn restart_failure_reactivates_the_prior_release() {
        // Regression for the run_release atomicity gap: `activate` had already
        // swapped the symlink before `restart` ran, so a failed restart must not
        // strand `current` on the bad release. Splitting activate/restart into
        // separate saga steps means a restart failure compensates the completed
        // Activate step — re-activating + restarting the PRIOR release — then
        // reverts the migration. Availability is restored, outcome RolledBack.
        let (_dir, store) = store();
        seed_prior(&store).await;
        let trail = Trail::default();
        let plan = SingleHostDeploy::builder("checkout", "production", HostId::new("localhost"))
            .artifact(Arc::new(FakeArtifact {
                trail: trail.clone(),
                fail_stage: false,
            }))
            .migration(Arc::new(FakeMigration::healthy(&trail)))
            .service(Arc::new(FakeService {
                trail: trail.clone(),
                fail_restarts: Arc::new(Mutex::new(1)), // the new release fails to start
            }))
            .health(Arc::new(FakeHealth {
                trail: trail.clone(),
                healthy: true,
            }))
            .build()
            .expect("all adapters provided");

        let outcome = plan.run(store).await.expect("run completes with rollback");
        assert!(
            matches!(&outcome, SagaOutcome::RolledBack { failed_step, .. } if failed_step == "restart"),
            "got {outcome:?}"
        );

        // Forward: activate the new release, restart (fails). Compensate the
        // completed Activate step: re-activate + restart the PRIOR release, then
        // down_to the prior revision. `current` lands back on the prior release.
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
                "activate:v-old",
                "restart",
                "down_to:rev-prev",
            ]
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
