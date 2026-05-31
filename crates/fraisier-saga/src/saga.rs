//! The saga driver: runs an ordered list of compensable [`Step`]s, persisting
//! progress and rolling back in reverse on failure.
//!
//! # Stability
//!
//! Frozen as of the Phase 1 owner review: [`Saga`], [`Step`], [`StepContext`],
//! [`SagaError`], and [`SagaOutcome`] are the load-bearing engine API. The types
//! expected to grow as the deploy layer matures ([`StepContext`], [`SagaError`],
//! [`SagaOutcome`]) are `#[non_exhaustive]`, so that growth stays additive
//! rather than breaking.

use async_trait::async_trait;

use crate::events::{instrument_state_transition, SagaEvent, SagaState};
use crate::state_store::{DeploymentState, FraiseKey, StateStore, StateStoreError};

/// Engine-level errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SagaError {
    /// A forward step reported failure; triggers rollback.
    #[error("step '{step}' failed: {message}")]
    StepFailed {
        /// The step that failed.
        step: String,
        /// A human-readable reason.
        message: String,
    },
    /// A state-store operation failed.
    #[error(transparent)]
    Store(#[from] StateStoreError),
}

/// Context passed to each [`Step`].
///
/// `#[non_exhaustive]` because the deploy layer will add shared state, the
/// resolved revision, and adapter handles; construct via [`StepContext::new`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StepContext {
    /// The `(fraise, environment)` pair this run targets.
    pub key: FraiseKey,
}

impl StepContext {
    /// Create a context for one `(fraise, environment)` pair.
    #[must_use]
    pub const fn new(key: FraiseKey) -> Self {
        Self { key }
    }
}

/// One compensable unit of work in a saga: a forward action and its undo.
///
/// This is the engine's *generic* step abstraction — deliberately not an adapter
/// axis. The deploy layer will wrap adapters (artifact/migration/…) into `Step`s;
/// the engine never sees an adapter directly.
#[async_trait]
pub trait Step: Send + Sync {
    /// A short, stable name used in state, events, and spans (e.g. `"migrate"`).
    fn name(&self) -> &str;

    /// Perform the step's forward action.
    ///
    /// # Errors
    /// Returns a [`SagaError`] if the action fails; the saga then compensates
    /// every previously completed step in reverse.
    async fn forward(&self, ctx: &StepContext) -> Result<(), SagaError>;

    /// Undo the step's forward action. Only called for steps whose `forward`
    /// completed successfully.
    ///
    /// # Errors
    /// Returns a [`SagaError`] if compensation fails; the saga then reports
    /// [`SagaOutcome::PartialRollback`] rather than pretending it succeeded.
    async fn compensate(&self, ctx: &StepContext) -> Result<(), SagaError>;
}

/// How a saga run ended.
#[derive(Debug)]
#[non_exhaustive]
pub enum SagaOutcome {
    /// Every step ran forward and the run committed.
    Committed,
    /// A step failed and all completed steps were compensated cleanly.
    RolledBack {
        /// The step whose forward action failed.
        failed_step: String,
        /// Why it failed.
        reason: String,
    },
    /// A step failed and a *compensation* then failed; operator intervention is
    /// required (PRD §5.4).
    PartialRollback {
        /// What went wrong during rollback.
        reason: String,
    },
}

/// Drives a sequence of [`Step`]s atomically over a [`StateStore`].
///
/// Construct with [`Saga::new`], append steps with [`Saga::with_step`], and
/// execute with [`Saga::run`].
pub struct Saga<S: StateStore> {
    store: S,
    key: FraiseKey,
    steps: Vec<Box<dyn Step>>,
}

impl<S: StateStore> Saga<S> {
    /// Create an empty saga for one `(fraise, environment)` pair.
    #[must_use]
    pub fn new(store: S, fraise: impl Into<String>, environment: impl Into<String>) -> Self {
        Self {
            store,
            key: FraiseKey::new(fraise, environment),
            steps: Vec::new(),
        }
    }

    /// Append a step to the forward sequence (builder style).
    #[must_use]
    pub fn with_step(mut self, step: Box<dyn Step>) -> Self {
        self.steps.push(step);
        self
    }

    /// Acquire the per-pair lock, run every step forward, and commit — or roll
    /// back in reverse on the first failure. The lock is always released.
    ///
    /// # Errors
    /// Returns a [`SagaError`] for infrastructure failures (locking or state
    /// persistence). A *business* failure that rolls back cleanly is reported as
    /// a successful `Ok(SagaOutcome::RolledBack)`, not an `Err`.
    pub async fn run(&self) -> Result<SagaOutcome, SagaError> {
        let guard = self.store.acquire_lock(&self.key).await?;
        let outcome = self.execute().await;
        match self.store.release_lock(guard).await {
            Ok(()) => outcome,
            // Prefer the run's own error; otherwise surface the release failure.
            Err(release_err) => {
                if outcome.is_err() {
                    outcome
                } else {
                    Err(release_err.into())
                }
            }
        }
    }

    async fn execute(&self) -> Result<SagaOutcome, SagaError> {
        let ctx = StepContext::new(self.key.clone());
        let mut from = SagaState::Idle;
        let mut completed: Vec<&dyn Step> = Vec::new();

        for step in &self.steps {
            let to = SagaState::Running(step.name().to_owned());
            self.transition(&from, &to).await?;
            if let Err(error) = step.forward(&ctx).await {
                return self.rollback(&ctx, &completed, step.name(), &error).await;
            }
            completed.push(step.as_ref());
            from = to;
        }

        self.transition(&from, &SagaState::Committed).await?;
        Ok(SagaOutcome::Committed)
    }

    /// Compensate every completed step in reverse, honouring the "what state did
    /// we reach" invariant: the failed step never ran to completion, so it is not
    /// compensated.
    async fn rollback(
        &self,
        ctx: &StepContext,
        completed: &[&dyn Step],
        failed_step: &str,
        error: &SagaError,
    ) -> Result<SagaOutcome, SagaError> {
        let mut from = SagaState::Running(failed_step.to_owned());
        for step in completed.iter().rev() {
            let to = SagaState::Compensating(step.name().to_owned());
            self.transition(&from, &to).await?;
            if let Err(comp_err) = step.compensate(ctx).await {
                let reason = format!("compensation for '{}' failed: {comp_err}", step.name());
                self.transition(&to, &SagaState::PartialRollback(reason.clone()))
                    .await?;
                return Ok(SagaOutcome::PartialRollback { reason });
            }
            from = to;
        }
        self.transition(&from, &SagaState::RolledBack).await?;
        Ok(SagaOutcome::RolledBack {
            failed_step: failed_step.to_owned(),
            reason: error.to_string(),
        })
    }

    /// Record one state transition: open its OTel span, append the event, and
    /// persist the new state.
    async fn transition(&self, from: &SagaState, to: &SagaState) -> Result<(), SagaError> {
        let _span =
            instrument_state_transition(self.key.fraise(), self.key.environment(), from, to);
        self.store
            .record_event(
                &self.key,
                &SagaEvent::StateTransition {
                    from: from.clone(),
                    to: to.clone(),
                },
            )
            .await?;
        self.store
            .record_state(&self.key, &DeploymentState::new(to.clone(), None))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Saga, SagaError, SagaOutcome, Step, StepContext};
    use crate::events::SagaState;
    use crate::state_store::{FilesystemStateStore, FraiseKey, StateStore};
    use std::sync::{Arc, Mutex};

    type Trail = Arc<Mutex<Vec<String>>>;

    /// A step that records each call on a shared trail; forward optionally fails.
    struct RecordingStep {
        name: String,
        trail: Trail,
        fail_forward: bool,
    }

    impl RecordingStep {
        fn ok(name: &str, trail: &Trail) -> Box<dyn Step> {
            Box::new(Self {
                name: name.to_owned(),
                trail: trail.clone(),
                fail_forward: false,
            })
        }
        fn failing(name: &str, trail: &Trail) -> Box<dyn Step> {
            Box::new(Self {
                name: name.to_owned(),
                trail: trail.clone(),
                fail_forward: true,
            })
        }
    }

    #[async_trait::async_trait]
    impl Step for RecordingStep {
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
                    message: "synthetic forward failure".to_owned(),
                });
            }
            Ok(())
        }
        async fn compensate(&self, _ctx: &StepContext) -> Result<(), SagaError> {
            self.trail
                .lock()
                .expect("trail")
                .push(format!("compensate:{}", self.name));
            Ok(())
        }
    }

    #[tokio::test]
    async fn noop_steps_progress_idle_to_committed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FilesystemStateStore::new(dir.path()).expect("store");
        let trail: Trail = Trail::default();

        let saga = Saga::new(store.clone(), "checkout", "production")
            .with_step(RecordingStep::ok("preflight", &trail))
            .with_step(RecordingStep::ok("migrate", &trail));

        let outcome = saga.run().await.expect("run succeeds");
        assert!(matches!(outcome, SagaOutcome::Committed), "got {outcome:?}");

        assert_eq!(
            *trail.lock().expect("trail"),
            vec!["forward:preflight", "forward:migrate"],
            "steps run forward in order, nothing compensated"
        );

        let key = FraiseKey::new("checkout", "production");
        let latest = store
            .current_state(&key)
            .await
            .expect("query")
            .expect("state");
        assert_eq!(latest.state, SagaState::Committed);
    }

    #[tokio::test]
    async fn forward_failure_rolls_back_completed_steps() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FilesystemStateStore::new(dir.path()).expect("store");
        let trail: Trail = Trail::default();

        let saga = Saga::new(store.clone(), "checkout", "production")
            .with_step(RecordingStep::ok("preflight", &trail))
            .with_step(RecordingStep::failing("migrate", &trail));

        let outcome = saga.run().await.expect("run completes (with rollback)");
        assert!(
            matches!(&outcome, SagaOutcome::RolledBack { failed_step, .. } if failed_step == "migrate"),
            "got {outcome:?}"
        );

        // preflight ran forward then was compensated; migrate failed forward and is
        // not compensated (it never completed).
        assert_eq!(
            *trail.lock().expect("trail"),
            vec![
                "forward:preflight",
                "forward:migrate",
                "compensate:preflight"
            ]
        );

        let key = FraiseKey::new("checkout", "production");
        let latest = store
            .current_state(&key)
            .await
            .expect("query")
            .expect("state");
        assert_eq!(latest.state, SagaState::RolledBack);
    }
}
