//! HTTP-tier **blue-green** deploy flow.
//!
//! Composes the traffic-swap primitive ([`TrafficDirector`]) and the schema
//! policy gate ([`crate::policy`]) into a saga, reusing the frozen saga engine +
//! its compensation.
//!
//! Flow (each transition a saga step):
//! `Preflight (window-safety hard gate)` → `ProvisionGreen` (start green; takes
//! NO traffic) → `Migrate` (shared DB, expand-only, already certified) →
//! `HealthGateGreen` (`/healthz` on green, *before* any traffic moves) →
//! `SwapTraffic` (blue→green) → `Hold` (blue kept HOT, watch green) → `Reap`
//! (decommission blue) → Committed.
//!
//! **Rollback before Reap = swap traffic back to still-hot blue** (instant). It
//! falls straight out of saga compensation: a failed step compensates the
//! completed steps in reverse, and `SwapTraffic`'s compensation is the swap-back.
//!
//! Two properties make this structurally **safer** than `self-upgrade apply`'s
//! rollback (which re-execs a binary that can boot-then-die):
//! - green is health-gated *before* any traffic moves, so the swap never points at
//!   a broken fleet;
//! - the migration is certified forward-compatible, so **there is no DB
//!   rollback** — `Migrate`'s compensation is a deliberate no-op; blue keeps
//!   serving correctly on the expanded schema (the contract migration that drops
//!   the now-unused columns is a separate later deploy).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fraisier_saga::saga::{Saga, SagaError, SagaOutcome, Step, StepContext};
use fraisier_saga::state_store::StateStore;

use crate::adapter_axes::{AdapterCtx, MigrationAdapter, TrafficDirector, TrafficTarget};
use crate::connection_budget::{self, BudgetVerdict, ConnectionBudget};
use crate::policy::{self, Baseline, PolicyGate};

/// Operations on the blue/green fleets the flow drives.
///
/// Behind a trait so the orchestration + failure gates are testable without real
/// instances; a real impl composes the artifact + service axes per fleet.
#[async_trait]
pub trait FleetOps: Send + Sync {
    /// Provision the green fleet (stage the artifact + start the service). Green
    /// takes **no** traffic until the swap.
    ///
    /// # Errors
    /// A human-readable message if green could not be provisioned.
    async fn provision_green(&self, ctx: &AdapterCtx) -> Result<(), String>;

    /// Whether the green fleet answers its health probe (`/healthz`) — the
    /// pre-swap gate.
    async fn green_healthy(&self, ctx: &AdapterCtx) -> bool;

    /// Hold blue hot for `hold` while watching green; return `Err` if green
    /// degrades within the window. The cadence (and the real-time wait) lives here
    /// because `fraisier-core` deliberately pulls in no async runtime — the CLI /
    /// embedder owns it.
    ///
    /// # Errors
    /// A human-readable message if green degraded during the hold window.
    async fn watch_green(&self, ctx: &AdapterCtx, hold: Duration) -> Result<(), String>;

    /// Decommission the green fleet (the compensation for [`Self::provision_green`]).
    ///
    /// # Errors
    /// A human-readable message if green could not be decommissioned.
    async fn reap_green(&self, ctx: &AdapterCtx) -> Result<(), String>;

    /// Decommission the now-idle blue fleet, after a healthy hold.
    ///
    /// # Errors
    /// A human-readable message if blue could not be decommissioned.
    async fn reap_blue(&self, ctx: &AdapterCtx) -> Result<(), String>;
}

/// The immutable per-run state shared by every step.
struct BgShared {
    ctx: AdapterCtx,
    green: TrafficTarget,
    hold: Duration,
    /// The green fleet's connection-pool size + the warn margin, for the pre-swap
    /// connection-budget check.
    green_pool: u32,
    budget_margin: u32,
    migration: Arc<dyn MigrationAdapter>,
    traffic: Arc<dyn TrafficDirector>,
    fleet: Arc<dyn FleetOps>,
    /// Optional pre-swap connection-budget probe (None skips the check).
    budget: Option<Arc<dyn ConnectionBudget>>,
    /// The schema policy gate. Its [`Baseline::WindowSafety`] rule applies with
    /// or without a `[policy]` section — blue-green's shared hold window is not
    /// something a config can switch off.
    policy: PolicyGate,
}

/// The mutable run state threaded to every step by the saga engine.
#[derive(Default)]
struct BgRuntime {
    /// The traffic target live *before* the swap — captured by `SwapTraffic` so
    /// its compensation can swap back to still-hot blue.
    prior_target: Option<TrafficTarget>,
    /// Whether green was provisioned (so compensation only reaps what it started).
    green_provisioned: bool,
}

/// Which blue-green step a [`BgStep`] represents.
#[derive(Clone, Copy)]
enum Phase {
    Preflight,
    ProvisionGreen,
    Migrate,
    HealthGateGreen,
    SwapTraffic,
    Hold,
    Reap,
}

impl Phase {
    const fn step_name(self) -> &'static str {
        match self {
            Self::Preflight => "preflight",
            Self::ProvisionGreen => "provision-green",
            Self::Migrate => "migrate",
            Self::HealthGateGreen => "health-gate-green",
            Self::SwapTraffic => "swap-traffic",
            Self::Hold => "hold",
            Self::Reap => "reap",
        }
    }
}

impl BgShared {
    fn step(self: &Arc<Self>, phase: Phase) -> Box<dyn Step<BgRuntime>> {
        Box::new(BgStep {
            shared: Arc::clone(self),
            phase,
        })
    }

    fn failed(step: &str, message: impl Into<String>) -> SagaError {
        SagaError::StepFailed {
            step: step.to_owned(),
            message: message.into(),
        }
    }

    /// The headline gate: refuse the whole deploy if confiture cannot certify the
    /// migration window-safe, or if the schema policy will not allow the pending
    /// changes — **before** any instance or traffic change — and, if a probe is
    /// configured, if doubling connections would exhaust the shared DB.
    ///
    /// One decision function answers both questions from one preflight report
    /// (D3). The window rule is the [`Baseline::WindowSafety`] half and applies
    /// unconditionally; the tier policy is the opt-in half.
    async fn run_preflight(&self) -> Result<(), SagaError> {
        let inspection = policy::inspect(&*self.migration, &self.ctx)
            .await
            .map_err(|e| {
                Self::failed(
                    "preflight",
                    format!("cannot certify the migration for the hold window: {e}"),
                )
            })?;
        self.policy
            .admit(
                Baseline::WindowSafety,
                inspection.capabilities,
                inspection.report.as_ref(),
                &self.ctx,
            )
            .await
            .map_err(|reason| Self::failed("preflight", reason))?;
        // The connection-budget edge: both fleets are live during the window.
        if let Some(budget) = &self.budget {
            let snapshot = budget
                .probe(&self.ctx)
                .await
                .map_err(|e| Self::failed("preflight", format!("connection-budget probe: {e}")))?;
            match connection_budget::evaluate(snapshot, self.green_pool, self.budget_margin) {
                BudgetVerdict::Ok => {}
                BudgetVerdict::Warn(message) => tracing::warn!("connection-budget: {message}"),
                BudgetVerdict::Refuse(reason) => {
                    return Err(Self::failed("preflight", reason));
                }
            }
        }
        Ok(())
    }

    async fn run_provision_green(&self, runtime: &mut BgRuntime) -> Result<(), SagaError> {
        self.fleet
            .provision_green(&self.ctx)
            .await
            .map_err(|e| Self::failed("provision-green", e))?;
        runtime.green_provisioned = true;
        Ok(())
    }

    async fn run_migrate(&self) -> Result<(), SagaError> {
        self.migration
            .up(&self.ctx, None)
            .await
            .map(|_| ())
            .map_err(|e| Self::failed("migrate", e.to_string()))
    }

    /// The **pre-swap health gate**: if green is unhealthy, fail here — traffic
    /// never moves (the swap step is never reached), blue keeps serving.
    async fn run_health_gate(&self) -> Result<(), SagaError> {
        if self.fleet.green_healthy(&self.ctx).await {
            Ok(())
        } else {
            Err(Self::failed(
                "health-gate-green",
                "green failed its pre-swap health gate; traffic was not moved",
            ))
        }
    }

    async fn run_swap(&self, runtime: &mut BgRuntime) -> Result<(), SagaError> {
        let prior = self
            .traffic
            .current_target(&self.ctx)
            .await
            .map_err(|e| Self::failed("swap-traffic", e.to_string()))?;
        runtime.prior_target = Some(prior);
        self.traffic
            .switch_to(&self.ctx, &self.green)
            .await
            .map(|_| ())
            .map_err(|e| Self::failed("swap-traffic", e.to_string()))
    }

    /// Swap traffic **back to still-hot blue** — the instant rollback. No DB
    /// rollback: the migration is forward-compatible, so blue serves correctly on
    /// the expanded schema.
    async fn undo_swap(&self, runtime: &BgRuntime) -> Result<(), SagaError> {
        let Some(prior) = runtime.prior_target.clone() else {
            return Ok(());
        };
        self.traffic
            .switch_to(&self.ctx, &prior)
            .await
            .map(|_| ())
            .map_err(|e| Self::failed("swap-traffic", e.to_string()))
    }

    /// The **post-swap degradation gate**: hold blue hot and watch green; if green
    /// degrades within the window, fail — compensation swaps back to blue.
    async fn run_hold(&self) -> Result<(), SagaError> {
        self.fleet
            .watch_green(&self.ctx, self.hold)
            .await
            .map_err(|e| Self::failed("hold", e))
    }

    async fn run_reap(&self) -> Result<(), SagaError> {
        self.fleet
            .reap_blue(&self.ctx)
            .await
            .map_err(|e| Self::failed("reap", e))
    }

    async fn undo_provision_green(&self, runtime: &BgRuntime) -> Result<(), SagaError> {
        if runtime.green_provisioned {
            self.fleet
                .reap_green(&self.ctx)
                .await
                .map_err(|e| Self::failed("provision-green", e))?;
        }
        Ok(())
    }
}

/// One compensable blue-green step.
struct BgStep {
    shared: Arc<BgShared>,
    phase: Phase,
}

#[async_trait]
impl Step<BgRuntime> for BgStep {
    fn name(&self) -> &str {
        self.phase.step_name()
    }

    async fn forward(&self, _ctx: &StepContext, runtime: &mut BgRuntime) -> Result<(), SagaError> {
        match self.phase {
            Phase::Preflight => self.shared.run_preflight().await,
            Phase::ProvisionGreen => self.shared.run_provision_green(runtime).await,
            Phase::Migrate => self.shared.run_migrate().await,
            Phase::HealthGateGreen => self.shared.run_health_gate().await,
            Phase::SwapTraffic => self.shared.run_swap(runtime).await,
            Phase::Hold => self.shared.run_hold().await,
            Phase::Reap => self.shared.run_reap().await,
        }
    }

    async fn compensate(
        &self,
        _ctx: &StepContext,
        runtime: &mut BgRuntime,
    ) -> Result<(), SagaError> {
        match self.phase {
            // The instant rollback: swap traffic back to still-hot blue.
            Phase::SwapTraffic => self.shared.undo_swap(runtime).await,
            // Decommission green if we started it.
            Phase::ProvisionGreen => self.shared.undo_provision_green(runtime).await,
            // Migrate is forward-compatible — NO DB rollback (blue serves on the
            // expanded schema). Inert steps have nothing to undo.
            Phase::Preflight
            | Phase::Migrate
            | Phase::HealthGateGreen
            | Phase::Hold
            | Phase::Reap => Ok(()),
        }
    }
}

/// A blue-green deploy, ready to [`run`](BlueGreenDeploy::run).
pub struct BlueGreenDeploy {
    fraise: String,
    environment: String,
    shared: Arc<BgShared>,
}

/// The scalar parameters of a blue-green deploy (the non-handle inputs).
pub struct BlueGreenParams {
    /// The deploy's fraise.
    pub fraise: String,
    /// The deploy's environment.
    pub environment: String,
    /// The adapter call context (carries `[migration]`/secrets/settings).
    pub ctx: AdapterCtx,
    /// The green fleet's traffic target id.
    pub green: TrafficTarget,
    /// How long blue is kept hot while green is watched.
    pub hold: Duration,
    /// The green fleet's connection-pool size (for the pre-swap budget check).
    pub green_pool: u32,
    /// The connection-budget warn margin below `max_connections`.
    pub budget_margin: u32,
    /// The schema policy gate resolved from `[policy]`. `Default` runs no tier
    /// policy — blue-green's window-safety baseline applies either way.
    pub policy: PolicyGate,
}

impl BlueGreenDeploy {
    /// Assemble a blue-green deploy from its parameters and adapter handles.
    #[must_use]
    pub fn new(
        params: BlueGreenParams,
        migration: Arc<dyn MigrationAdapter>,
        traffic: Arc<dyn TrafficDirector>,
        fleet: Arc<dyn FleetOps>,
        budget: Option<Arc<dyn ConnectionBudget>>,
    ) -> Self {
        Self {
            fraise: params.fraise,
            environment: params.environment,
            shared: Arc::new(BgShared {
                ctx: params.ctx,
                green: params.green,
                hold: params.hold,
                green_pool: params.green_pool,
                budget_margin: params.budget_margin,
                migration,
                traffic,
                fleet,
                budget,
                policy: params.policy,
            }),
        }
    }

    /// Run the blue-green flow over `store`. A clean rollback (e.g. a failed
    /// pre-swap health gate, or a green degradation that swaps back to blue) is a
    /// successful `Ok(SagaOutcome::RolledBack)`, not an `Err`.
    ///
    /// # Errors
    /// [`SagaError`] for infrastructure failures (locking / state persistence).
    pub async fn run<S: StateStore>(&self, store: S) -> Result<SagaOutcome, SagaError> {
        let saga = Saga::new(store, self.fraise.clone(), self.environment.clone())
            .with_step(self.shared.step(Phase::Preflight))
            .with_step(self.shared.step(Phase::ProvisionGreen))
            .with_step(self.shared.step(Phase::Migrate))
            .with_step(self.shared.step(Phase::HealthGateGreen))
            .with_step(self.shared.step(Phase::SwapTraffic))
            .with_step(self.shared.step(Phase::Hold))
            .with_step(self.shared.step(Phase::Reap));
        let mut runtime = BgRuntime::default();
        saga.run_with_state(&mut runtime).await
    }
}

#[cfg(test)]
mod tests {
    use super::{BlueGreenDeploy, BlueGreenParams, FleetOps};
    use crate::adapter_axes::{
        AdapterCtx, AdapterDescription, AdapterError, ChangeSet, MigrationAdapter,
        MigrationOutcome, PreflightReport, Revision, RiskTier, SchemaChange, SwapToken,
        TrafficDirector, TrafficTarget, VerifyReport,
    };
    use crate::policy::{testing::FixedApproval, Policy, PolicyGate, REFUSED};
    use async_trait::async_trait;
    use fraisier_saga::saga::SagaOutcome;
    use fraisier_saga::state_store::MemoryStateStore;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// A migration adapter that advertises `preflight` and returns `report`,
    /// recording every call so a test can prove the adapter is asked once.
    struct FakeMigration {
        report: PreflightReport,
        classifies: bool,
        calls: Mutex<Vec<String>>,
    }
    impl FakeMigration {
        fn reporting(report: PreflightReport, classifies: bool) -> Arc<Self> {
            Arc::new(Self {
                report,
                classifies,
                calls: Mutex::new(Vec::new()),
            })
        }
        fn window_safe() -> Arc<Self> {
            Self::reporting(
                PreflightReport {
                    ok: true,
                    window_safe: Some(true),
                    ..Default::default()
                },
                false,
            )
        }
        fn unsafe_drop_column() -> Arc<Self> {
            // confiture's typed verdict: NOT forward-compatible for a two-version window.
            Self::reporting(
                PreflightReport {
                    ok: true,
                    window_safe: Some(false),
                    ..Default::default()
                },
                false,
            )
        }
        /// A window-safe migration whose changes carry risk tiers.
        fn classified(changes: Vec<SchemaChange>) -> Arc<Self> {
            Self::reporting(
                PreflightReport::new(true)
                    .with_window_safe(true)
                    .with_change_set(ChangeSet::new(changes)),
                true,
            )
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls").clone()
        }
        fn record(&self, call: &str) {
            self.calls.lock().expect("calls").push(call.to_owned());
        }
    }
    #[async_trait]
    impl MigrationAdapter for FakeMigration {
        async fn describe(&self) -> Result<AdapterDescription, AdapterError> {
            self.record("describe");
            let mut capabilities = vec!["up".to_owned(), "preflight".to_owned()];
            if self.classifies {
                capabilities.push("risk_tier".to_owned());
            }
            Ok(AdapterDescription {
                name: "fake".to_owned(),
                version: "0".to_owned(),
                protocol_version: 1,
                capabilities,
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
            self.record("preflight");
            Ok(self.report.clone())
        }
    }

    /// A traffic director that records every swap and the current live target.
    #[derive(Default)]
    struct FakeTraffic {
        current: Mutex<String>,
        swaps: Mutex<Vec<String>>,
    }
    impl FakeTraffic {
        fn blue() -> Arc<Self> {
            Arc::new(Self {
                current: Mutex::new("blue".to_owned()),
                swaps: Mutex::new(Vec::new()),
            })
        }
        fn swaps(&self) -> Vec<String> {
            self.swaps.lock().unwrap().clone()
        }
        fn live(&self) -> String {
            self.current.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl TrafficDirector for FakeTraffic {
        async fn describe(&self) -> Result<AdapterDescription, AdapterError> {
            Ok(AdapterDescription {
                name: "fake".to_owned(),
                version: "0".to_owned(),
                protocol_version: 1,
                capabilities: vec!["traffic_swap".to_owned()],
            })
        }
        async fn current_target(&self, _ctx: &AdapterCtx) -> Result<TrafficTarget, AdapterError> {
            Ok(TrafficTarget::new(self.live()))
        }
        async fn switch_to(
            &self,
            _ctx: &AdapterCtx,
            target: &TrafficTarget,
        ) -> Result<SwapToken, AdapterError> {
            self.swaps.lock().unwrap().push(target.as_str().to_owned());
            *self.current.lock().unwrap() = target.as_str().to_owned();
            Ok(SwapToken {
                target: target.clone(),
            })
        }
    }

    /// A fleet whose green is healthy for the first `healthy_checks` probes, then
    /// degrades — and which records provision/reap calls.
    struct FakeFleet {
        gate_healthy: bool,
        hold_ok: bool,
        log: Mutex<Vec<String>>,
    }
    impl FakeFleet {
        /// `gate_healthy` controls the pre-swap health gate; `hold_ok` controls the
        /// post-swap hold-window watch.
        fn new(gate_healthy: bool, hold_ok: bool) -> Arc<Self> {
            Arc::new(Self {
                gate_healthy,
                hold_ok,
                log: Mutex::new(Vec::new()),
            })
        }
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl FleetOps for FakeFleet {
        async fn provision_green(&self, _ctx: &AdapterCtx) -> Result<(), String> {
            self.log.lock().unwrap().push("provision_green".to_owned());
            Ok(())
        }
        async fn green_healthy(&self, _ctx: &AdapterCtx) -> bool {
            self.gate_healthy
        }
        async fn watch_green(&self, _ctx: &AdapterCtx, _hold: Duration) -> Result<(), String> {
            if self.hold_ok {
                Ok(())
            } else {
                Err("green degraded during the hold window".to_owned())
            }
        }
        async fn reap_green(&self, _ctx: &AdapterCtx) -> Result<(), String> {
            self.log.lock().unwrap().push("reap_green".to_owned());
            Ok(())
        }
        async fn reap_blue(&self, _ctx: &AdapterCtx) -> Result<(), String> {
            self.log.lock().unwrap().push("reap_blue".to_owned());
            Ok(())
        }
    }

    fn params() -> BlueGreenParams {
        policed(PolicyGate::default())
    }

    /// The same parameters with a schema policy gate applied.
    fn policed(policy: PolicyGate) -> BlueGreenParams {
        BlueGreenParams {
            fraise: "checkout".to_owned(),
            environment: "production".to_owned(),
            ctx: AdapterCtx::new("checkout", "production"),
            green: TrafficTarget::new("green"),
            hold: Duration::from_millis(20),
            green_pool: 20,
            budget_margin: 10,
            policy,
        }
    }

    /// The reason a run was rolled back at `step`, or a panic naming what
    /// happened instead.
    fn refusal(outcome: &SagaOutcome, step: &str) -> String {
        match outcome {
            SagaOutcome::RolledBack {
                failed_step,
                reason,
            } if failed_step == step => reason.clone(),
            other => panic!("expected a rollback at {step}, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_healthy_blue_green_commits_swaps_to_green_and_reaps_blue() {
        let traffic = FakeTraffic::blue();
        let fleet = FakeFleet::new(true, true); // healthy gate + hold
        let bg = BlueGreenDeploy::new(
            params(),
            FakeMigration::window_safe(),
            Arc::clone(&traffic) as Arc<dyn TrafficDirector>,
            Arc::clone(&fleet) as Arc<dyn FleetOps>,
            None,
        );
        let outcome = bg.run(MemoryStateStore::new()).await.expect("run");
        assert!(matches!(outcome, SagaOutcome::Committed), "{outcome:?}");
        assert_eq!(traffic.live(), "green", "traffic is on green");
        assert_eq!(
            traffic.swaps(),
            vec!["green".to_owned()],
            "one swap, to green"
        );
        assert!(fleet.log().contains(&"reap_blue".to_owned()), "blue reaped");
    }

    #[tokio::test]
    async fn the_pre_swap_health_gate_never_moves_traffic() {
        // Green is unhealthy from the start: the health gate fails BEFORE the swap.
        let traffic = FakeTraffic::blue();
        let fleet = FakeFleet::new(false, true);
        let bg = BlueGreenDeploy::new(
            params(),
            FakeMigration::window_safe(),
            Arc::clone(&traffic) as Arc<dyn TrafficDirector>,
            Arc::clone(&fleet) as Arc<dyn FleetOps>,
            None,
        );
        let outcome = bg.run(MemoryStateStore::new()).await.expect("run");
        assert!(
            matches!(outcome, SagaOutcome::RolledBack { ref failed_step, .. } if failed_step == "health-gate-green"),
            "{outcome:?}"
        );
        assert!(
            traffic.swaps().is_empty(),
            "traffic NEVER moved: {:?}",
            traffic.swaps()
        );
        assert_eq!(traffic.live(), "blue", "blue still serves");
        assert!(
            fleet.log().contains(&"reap_green".to_owned()),
            "green decommissioned"
        );
        assert!(
            !fleet.log().contains(&"reap_blue".to_owned()),
            "blue NOT reaped"
        );
    }

    #[tokio::test]
    async fn a_post_swap_degradation_swaps_back_to_still_hot_blue() {
        // Green passes the pre-swap gate, then degrades during the hold window.
        let traffic = FakeTraffic::blue();
        let fleet = FakeFleet::new(true, false);
        let bg = BlueGreenDeploy::new(
            params(),
            FakeMigration::window_safe(),
            Arc::clone(&traffic) as Arc<dyn TrafficDirector>,
            Arc::clone(&fleet) as Arc<dyn FleetOps>,
            None,
        );
        let outcome = bg.run(MemoryStateStore::new()).await.expect("run");
        assert!(
            matches!(outcome, SagaOutcome::RolledBack { ref failed_step, .. } if failed_step == "hold"),
            "{outcome:?}"
        );
        // Swapped to green, then back to blue (the instant rollback).
        assert_eq!(traffic.swaps(), vec!["green".to_owned(), "blue".to_owned()]);
        assert_eq!(traffic.live(), "blue", "service restored to still-hot blue");
        assert!(
            !fleet.log().contains(&"reap_blue".to_owned()),
            "blue was never reaped"
        );
    }

    #[tokio::test]
    async fn an_unsafe_migration_is_refused_before_any_instance_or_traffic_change() {
        let traffic = FakeTraffic::blue();
        let fleet = FakeFleet::new(true, true);
        let bg = BlueGreenDeploy::new(
            params(),
            FakeMigration::unsafe_drop_column(),
            Arc::clone(&traffic) as Arc<dyn TrafficDirector>,
            Arc::clone(&fleet) as Arc<dyn FleetOps>,
            None,
        );
        let outcome = bg.run(MemoryStateStore::new()).await.expect("run");
        assert!(
            matches!(outcome, SagaOutcome::RolledBack { ref failed_step, .. } if failed_step == "preflight"),
            "{outcome:?}"
        );
        assert!(traffic.swaps().is_empty(), "no traffic change");
        assert!(
            fleet.log().is_empty(),
            "no instance change (green never provisioned)"
        );
    }

    /// A connection-budget probe reporting a fixed snapshot.
    struct FakeBudget {
        snapshot: crate::connection_budget::BudgetSnapshot,
    }
    #[async_trait]
    impl crate::connection_budget::ConnectionBudget for FakeBudget {
        async fn probe(
            &self,
            _ctx: &AdapterCtx,
        ) -> Result<crate::connection_budget::BudgetSnapshot, String> {
            Ok(self.snapshot)
        }
    }

    #[tokio::test]
    async fn an_exhausted_connection_budget_refuses_before_the_swap() {
        // 90 in use + a 20-connection green pool = 110 > max_connections 100.
        let traffic = FakeTraffic::blue();
        let fleet = FakeFleet::new(true, true);
        let budget = Arc::new(FakeBudget {
            snapshot: crate::connection_budget::BudgetSnapshot {
                max_connections: 100,
                current: 90,
            },
        });
        let bg = BlueGreenDeploy::new(
            params(), // green_pool = 20, budget_margin = 10
            FakeMigration::window_safe(),
            Arc::clone(&traffic) as Arc<dyn TrafficDirector>,
            Arc::clone(&fleet) as Arc<dyn FleetOps>,
            Some(budget as Arc<dyn crate::connection_budget::ConnectionBudget>),
        );
        let outcome = bg.run(MemoryStateStore::new()).await.expect("run");
        assert!(
            matches!(outcome, SagaOutcome::RolledBack { ref failed_step, .. } if failed_step == "preflight"),
            "{outcome:?}"
        );
        assert!(traffic.swaps().is_empty(), "refused before any swap");
        assert!(fleet.log().is_empty(), "refused before any instance change");
    }

    // -----------------------------------------------------------------------
    // The schema policy gate (#45, D3). Blue-green is the strategy whose
    // window-safety rule the policy gate replaced: one decision function now
    // answers both "can N-1 and N share this database?" and "is this change
    // destructive enough to need a human?".
    // -----------------------------------------------------------------------

    /// Run a blue-green deploy with `policy` and `migration`, returning the
    /// outcome and the traffic director (to prove nothing moved).
    async fn run_gated(
        policy: PolicyGate,
        migration: Arc<FakeMigration>,
    ) -> (SagaOutcome, Arc<FakeTraffic>) {
        let traffic = FakeTraffic::blue();
        let fleet = FakeFleet::new(true, true);
        let bg = BlueGreenDeploy::new(
            policed(policy),
            migration,
            Arc::clone(&traffic) as Arc<dyn TrafficDirector>,
            Arc::clone(&fleet) as Arc<dyn FleetOps>,
            None,
        );
        let outcome = bg.run(MemoryStateStore::new()).await.expect("run");
        assert!(fleet.log().is_empty() || matches!(outcome, SagaOutcome::Committed));
        (outcome, traffic)
    }

    #[tokio::test]
    async fn one_gate_refuses_for_either_cause_with_distinguishable_reasons() {
        // The test that pins D3's resolved reading. `window_safe` is not a second
        // gate, it is an input to the one gate — and a refusal has to say which
        // rule fired, because the two have completely different fixes: one is
        // "rewrite the migration expand/contract", the other is "get sign-off".
        let (window, traffic) = run_gated(
            PolicyGate::new(Policy::default().with_approval_hook(true))
                .with_hook(Some(Arc::new(FixedApproval::approving("oncall")))),
            FakeMigration::unsafe_drop_column(),
        )
        .await;
        let window = refusal(&window, "preflight");
        assert!(traffic.swaps().is_empty(), "no traffic moved");

        let (tier, traffic) = run_gated(
            PolicyGate::new(Policy::default()),
            FakeMigration::classified(vec![SchemaChange::new(
                "drop_column",
                "public.tb_user.legacy_flag",
            )
            .with_tier(RiskTier::Irreversible)]),
        )
        .await;
        let tier = refusal(&tier, "preflight");
        assert!(traffic.swaps().is_empty(), "no traffic moved");

        // Both are the one gate refusing...
        assert!(window.contains(REFUSED), "{window}");
        assert!(tier.contains(REFUSED), "{tier}");
        // ...and an operator can tell them apart at a glance.
        assert!(window.contains("two-version window"), "{window}");
        assert!(!window.contains("public.tb_user.legacy_flag"), "{window}");
        assert!(tier.contains("public.tb_user.legacy_flag"), "{tier}");
        assert!(!tier.contains("two-version window"), "{tier}");
    }

    #[tokio::test]
    async fn blue_green_is_still_gated_with_no_policy_section() {
        // The D6 carve-out at the saga level: the tier policy is opt-in, the
        // window-safety baseline is not. If this ever passes, replacing the
        // window-safety gate has silently deleted it for every existing user —
        // which is all of them, since nobody has a `[policy]` section today.
        let (outcome, traffic) =
            run_gated(PolicyGate::default(), FakeMigration::unsafe_drop_column()).await;
        let reason = refusal(&outcome, "preflight");
        assert!(reason.contains("window_safe = false"), "{reason}");
        assert!(traffic.swaps().is_empty(), "no traffic moved");
    }

    #[tokio::test]
    async fn an_approved_irreversible_change_swaps_traffic() {
        // The other half of the gate: sign-off unblocks what it holds, and the
        // deploy runs to completion.
        let (outcome, traffic) = run_gated(
            PolicyGate::new(Policy::default().with_approval_hook(true))
                .with_hook(Some(Arc::new(FixedApproval::approving("oncall")))),
            FakeMigration::classified(vec![SchemaChange::new(
                "drop_column",
                "public.tb_user.legacy_flag",
            )
            .with_tier(RiskTier::Irreversible)]),
        )
        .await;
        assert!(matches!(outcome, SagaOutcome::Committed), "{outcome:?}");
        assert_eq!(traffic.live(), "green");
    }

    #[tokio::test]
    async fn a_blue_green_deploy_inspects_the_migration_adapter_once() {
        // The window rule and the tier policy read one report. Two subprocesses
        // and two round-trips for the same answer is what the one decision
        // function exists to avoid.
        let migration = FakeMigration::classified(vec![SchemaChange::new(
            "add_column",
            "public.tb_user.nickname",
        )
        .with_tier(RiskTier::Additive)]);
        let (outcome, _traffic) =
            run_gated(PolicyGate::new(Policy::default()), Arc::clone(&migration)).await;
        assert!(matches!(outcome, SagaOutcome::Committed), "{outcome:?}");
        assert_eq!(migration.calls(), vec!["describe", "preflight"]);
    }
}
