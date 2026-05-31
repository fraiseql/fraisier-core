//! Atomic-rollback semantics: forced failures at every stage, plus the
//! partial-rollback path (a compensation that itself fails).
//!
//! The saga engine is generic over [`Step`]s, so this drives a pipeline named
//! after the single-host deploy stages (PRD §5.2) with a scripted harness that
//! can fail any step's forward action and/or its compensation. The deploy layer
//! will later wrap real adapters into these steps; the rollback machinery proven
//! here is what it relies on.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fraisier_saga::events::SagaState;
use fraisier_saga::saga::{Saga, SagaError, SagaOutcome, Step, StepContext};
use fraisier_saga::state_store::{FilesystemStateStore, FraiseKey, StateStore};

type Trail = Arc<Mutex<Vec<String>>>;

/// The single-host forward sequence (PRD §5.2). "Commit" is the terminal
/// transition after the last step, so it is success rather than a step.
const STAGES: [&str; 6] = [
    "preflight",
    "fetch",
    "migrate",
    "restart",
    "healthcheck",
    "verify",
];

/// A step that records every call on a shared trail and can be scripted to fail
/// its forward action and/or its compensation.
struct ScriptedStep {
    name: String,
    trail: Trail,
    fail_forward: bool,
    fail_compensate: bool,
}

#[async_trait]
impl Step for ScriptedStep {
    fn name(&self) -> &str {
        &self.name
    }

    async fn forward(&self, _ctx: &StepContext) -> Result<(), SagaError> {
        self.trail
            .lock()
            .expect("trail")
            .push(format!("forward:{}", self.name));
        if self.fail_forward {
            return Err(SagaError::StepFailed {
                step: self.name.clone(),
                message: "forced forward failure".to_owned(),
            });
        }
        Ok(())
    }

    async fn compensate(&self, _ctx: &StepContext) -> Result<(), SagaError> {
        self.trail
            .lock()
            .expect("trail")
            .push(format!("compensate:{}", self.name));
        if self.fail_compensate {
            return Err(SagaError::StepFailed {
                step: self.name.clone(),
                message: "forced compensation failure".to_owned(),
            });
        }
        Ok(())
    }
}

fn step(name: &str, trail: &Trail, fail_forward: bool, fail_compensate: bool) -> Box<dyn Step> {
    Box::new(ScriptedStep {
        name: name.to_owned(),
        trail: trail.clone(),
        fail_forward,
        fail_compensate,
    })
}

async fn current_state(store: &FilesystemStateStore) -> SagaState {
    store
        .current_state(&FraiseKey::new("checkout", "production"))
        .await
        .expect("query")
        .expect("a state was recorded")
        .state
}

#[tokio::test]
async fn forced_failure_at_each_stage_rolls_back_completed_steps_in_reverse() {
    for fail_at in 0..STAGES.len() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FilesystemStateStore::new(dir.path()).expect("store");
        let trail = Trail::default();

        let mut saga = Saga::new(store.clone(), "checkout", "production");
        for (index, name) in STAGES.iter().enumerate() {
            saga = saga.with_step(step(name, &trail, index == fail_at, false));
        }

        let outcome = saga
            .run()
            .await
            .expect("run completes with a clean rollback");
        match outcome {
            SagaOutcome::RolledBack { failed_step, .. } => {
                assert_eq!(failed_step, STAGES[fail_at]);
            }
            other => panic!("expected RolledBack at {}, got {other:?}", STAGES[fail_at]),
        }

        // Forward through the failing stage inclusive, then compensate every
        // *completed* (earlier) stage in reverse. The failing stage never
        // completed, so it is not compensated.
        let mut expected: Vec<String> = STAGES[..=fail_at]
            .iter()
            .map(|s| format!("forward:{s}"))
            .collect();
        expected.extend(
            STAGES[..fail_at]
                .iter()
                .rev()
                .map(|s| format!("compensate:{s}")),
        );
        assert_eq!(
            *trail.lock().expect("trail"),
            expected,
            "stage {}",
            STAGES[fail_at]
        );

        assert_eq!(
            current_state(&store).await,
            SagaState::RolledBack,
            "pre-deploy state restored after failure at {}",
            STAGES[fail_at]
        );
    }
}

#[tokio::test]
async fn all_stages_passing_commits_without_compensating() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = FilesystemStateStore::new(dir.path()).expect("store");
    let trail = Trail::default();

    let mut saga = Saga::new(store.clone(), "checkout", "production");
    for name in STAGES {
        saga = saga.with_step(step(name, &trail, false, false));
    }

    assert!(matches!(
        saga.run().await.expect("run"),
        SagaOutcome::Committed
    ));
    let expected: Vec<String> = STAGES.iter().map(|s| format!("forward:{s}")).collect();
    assert_eq!(
        *trail.lock().expect("trail"),
        expected,
        "nothing is compensated on success"
    );
    assert_eq!(current_state(&store).await, SagaState::Committed);
}

#[tokio::test]
async fn failed_compensation_yields_partial_rollback_with_diagnostics() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = FilesystemStateStore::new(dir.path()).expect("store");
    let trail = Trail::default();

    // `migrate` completes but its compensation fails; `restart` then fails
    // forward, triggering rollback. Rolling back hits `migrate` and cannot undo it.
    let saga = Saga::new(store.clone(), "checkout", "production")
        .with_step(step("preflight", &trail, false, false))
        .with_step(step("migrate", &trail, false, true)) // compensation fails
        .with_step(step("restart", &trail, true, false)); // forward fails

    match saga.run().await.expect("run completes") {
        SagaOutcome::PartialRollback { reason } => {
            assert!(
                reason.contains("migrate"),
                "names the unrecoverable step: {reason}"
            );
            assert!(
                reason.contains("compensation"),
                "operator-readable: {reason}"
            );
        }
        other => panic!("expected PartialRollback, got {other:?}"),
    }

    assert_eq!(
        *trail.lock().expect("trail"),
        vec![
            "forward:preflight",
            "forward:migrate",
            "forward:restart",
            "compensate:migrate"
        ],
        "rollback stops at the failed compensation; preflight is never reached"
    );
    assert!(
        matches!(current_state(&store).await, SagaState::PartialRollback(_)),
        "the engine surfaces PartialRollback rather than pretending success"
    );
}
