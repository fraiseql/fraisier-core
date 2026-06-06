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

use crate::events::{instrument_deploy_run, instrument_state_transition, SagaEvent, SagaState};
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
///
/// # Run-state (`R`)
///
/// Steps share mutable per-run state through the typed `&mut R` parameter the
/// engine threads into every `forward`/`compensate` call — a value the *caller*
/// owns and passes to [`Saga::run_with_state`]. Because a saga runs its steps
/// strictly one at a time, each step gets **exclusive** (`&mut`) access in turn:
/// a step *produces* a value (a staged artifact, a container handle) by writing
/// it into the state, and a later step — or an earlier step's compensation —
/// *consumes* it by reading it back. No `Arc<Mutex>`, no interior mutability, and
/// no downcasting. `R` defaults to `()` for stateless sagas, whose steps simply
/// ignore the `&mut ()` argument and run via [`Saga::run`].
#[async_trait]
pub trait Step<R: Send = ()>: Send + Sync {
    /// A short, stable name used in state, events, and spans (e.g. `"migrate"`).
    fn name(&self) -> &str;

    /// Perform the step's forward action, reading and writing the shared run
    /// state `state` as needed.
    ///
    /// # Errors
    /// Returns a [`SagaError`] if the action fails; the saga then compensates
    /// every previously completed step in reverse.
    async fn forward(&self, ctx: &StepContext, state: &mut R) -> Result<(), SagaError>;

    /// Undo the step's forward action, using `state` to recover what `forward`
    /// produced. Only called for steps whose `forward` completed successfully.
    ///
    /// # Errors
    /// Returns a [`SagaError`] if compensation fails; the saga then reports
    /// [`SagaOutcome::PartialRollback`] rather than pretending it succeeded.
    async fn compensate(&self, ctx: &StepContext, state: &mut R) -> Result<(), SagaError>;
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

/// Drives a sequence of [`Step`]s atomically over a [`StateStore`], threading a
/// caller-owned run state `R` (default `()`) through every step.
///
/// Construct with [`Saga::new`], append steps with [`Saga::with_step`] /
/// [`Saga::with_steps`], and execute with [`Saga::run_with_state`] — or with the
/// [`Saga::run`] shortcut when `R = ()` (a stateless saga).
pub struct Saga<S: StateStore, R: Send = ()> {
    store: S,
    key: FraiseKey,
    steps: Vec<Box<dyn Step<R>>>,
}

impl<S: StateStore, R: Send> Saga<S, R> {
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
    pub fn with_step(mut self, step: Box<dyn Step<R>>) -> Self {
        self.steps.push(step);
        self
    }

    /// Append every step yielded by `steps` to the forward sequence, in order
    /// (builder style).
    ///
    /// A convenience for composing a *dynamically built* list of steps — e.g. a
    /// `Vec<Box<dyn Step<R>>>` assembled with conditional steps — without folding
    /// over [`Saga::with_step`]. Equivalent to calling `with_step` once per item.
    #[must_use]
    pub fn with_steps<I>(mut self, steps: I) -> Self
    where
        I: IntoIterator<Item = Box<dyn Step<R>>>,
    {
        self.steps.extend(steps);
        self
    }

    /// Acquire the per-pair lock, run every step forward threading `state`, and
    /// commit — or roll back in reverse on the first failure. The lock is always
    /// released; `state` reflects whatever the steps left in it.
    ///
    /// # Errors
    /// Returns a [`SagaError`] for infrastructure failures (locking or state
    /// persistence). A *business* failure that rolls back cleanly is reported as
    /// a successful `Ok(SagaOutcome::RolledBack)`, not an `Err`.
    pub async fn run_with_state(&self, state: &mut R) -> Result<SagaOutcome, SagaError> {
        use tracing::Instrument as _;
        // One root span per run, held across the whole run, so every transition
        // span nests beneath it and the deploy exports as a single trace.
        let span = instrument_deploy_run(self.key.fraise(), self.key.environment());
        self.run_inner(state).instrument(span).await
    }

    async fn run_inner(&self, state: &mut R) -> Result<SagaOutcome, SagaError> {
        let guard = self.store.acquire_lock(&self.key).await?;
        let outcome = self.execute(state).await;
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

    async fn execute(&self, state: &mut R) -> Result<SagaOutcome, SagaError> {
        let ctx = StepContext::new(self.key.clone());
        let mut from = SagaState::Idle;
        let mut completed: Vec<&dyn Step<R>> = Vec::new();

        for step in &self.steps {
            let to = SagaState::Running(step.name().to_owned());
            self.transition(&from, &to).await?;
            if let Err(error) = step.forward(&ctx, state).await {
                return self
                    .rollback(&ctx, &completed, step.name(), &error, state)
                    .await;
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
        completed: &[&dyn Step<R>],
        failed_step: &str,
        error: &SagaError,
        state: &mut R,
    ) -> Result<SagaOutcome, SagaError> {
        let mut from = SagaState::Running(failed_step.to_owned());
        for step in completed.iter().rev() {
            let to = SagaState::Compensating(step.name().to_owned());
            self.transition(&from, &to).await?;
            if let Err(comp_err) = step.compensate(ctx, state).await {
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

impl<S: StateStore> Saga<S, ()> {
    /// Run a **stateless** saga (run state `()`), the common case where steps
    /// share nothing across the run. A convenience for
    /// `run_with_state(&mut ())`; see [`Saga::run_with_state`] for the threaded
    /// run-state form and its error contract.
    ///
    /// # Errors
    /// Returns a [`SagaError`] for infrastructure failures; a clean business
    /// rollback is `Ok(SagaOutcome::RolledBack)`.
    pub async fn run(&self) -> Result<SagaOutcome, SagaError> {
        self.run_with_state(&mut ()).await
    }
}

#[cfg(test)]
mod tests {
    use super::{Saga, SagaError, SagaOutcome, Step, StepContext};
    use crate::events::SagaState;
    use crate::state_store::{FilesystemStateStore, FraiseKey, StateStore};
    use std::sync::{Arc, Mutex};

    type Trail = Arc<Mutex<Vec<String>>>;

    /// Install a process-global default subscriber once, so the `saga.*` span
    /// callsites are always evaluated as *interested*.
    ///
    /// `tracing`'s callsite interest cache is process-global. Without a global
    /// default, a saga test that runs with no subscriber can cache
    /// `Interest::never` for the `saga.deploy` / `saga.state_transition`
    /// callsites; the span-capture test below then sees zero (or unparented)
    /// spans depending on which test hit the callsite first — a
    /// concurrency-dependent flake. A no-op global default keeps the callsites
    /// interested; capture tests still layer their own subscriber via the
    /// thread-local `set_default`, which takes precedence on their thread.
    fn init_global_subscriber() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            let _ = tracing::subscriber::set_global_default(
                tracing_subscriber::registry::Registry::default(),
            );
        });
    }

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
        async fn forward(&self, _ctx: &StepContext, _state: &mut ()) -> Result<(), SagaError> {
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
        async fn compensate(&self, _ctx: &StepContext, _state: &mut ()) -> Result<(), SagaError> {
            self.trail
                .lock()
                .expect("trail")
                .push(format!("compensate:{}", self.name));
            Ok(())
        }
    }

    #[tokio::test]
    async fn noop_steps_progress_idle_to_committed() {
        init_global_subscriber();
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

    #[tokio::test(flavor = "current_thread")]
    async fn deploy_run_is_one_trace_with_transitions_nested() {
        use std::collections::HashMap;
        use tracing::span::{Attributes, Id};
        use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};
        use tracing_subscriber::registry::LookupSpan;
        use tracing_subscriber::Registry;

        // Capture each span's (name, parent-name) so we can assert the tree shape.
        type SpanNames = Arc<Mutex<HashMap<u64, String>>>;
        type SpanEdges = Arc<Mutex<Vec<(String, Option<String>)>>>;
        #[derive(Clone, Default)]
        struct SpanTree {
            names: SpanNames,
            edges: SpanEdges,
        }
        impl<S> Layer<S> for SpanTree
        where
            S: tracing::Subscriber + for<'a> LookupSpan<'a>,
        {
            fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
                let name = attrs.metadata().name().to_owned();
                let parent = attrs
                    .parent()
                    .cloned()
                    .or_else(|| ctx.current_span().id().cloned())
                    .and_then(|pid| {
                        self.names
                            .lock()
                            .expect("lock")
                            .get(&pid.into_u64())
                            .cloned()
                    });
                self.names
                    .lock()
                    .expect("lock")
                    .insert(id.into_u64(), name.clone());
                self.edges.lock().expect("lock").push((name, parent));
            }
        }

        init_global_subscriber();
        let tree = SpanTree::default();
        let edges = tree.edges.clone();
        let _guard = tracing::subscriber::set_default(Registry::default().with(tree));

        let dir = tempfile::tempdir().expect("tempdir");
        let store = FilesystemStateStore::new(dir.path()).expect("store");
        let trail: Trail = Trail::default();
        Saga::new(store, "checkout", "production")
            .with_step(RecordingStep::ok("preflight", &trail))
            .with_step(RecordingStep::ok("migrate", &trail))
            .run()
            .await
            .expect("run");

        let edges = edges.lock().expect("lock").clone();
        let deploy_roots = edges.iter().filter(|(n, _)| n == "saga.deploy").count();
        assert_eq!(deploy_roots, 1, "exactly one root deploy span: {edges:?}");
        let transitions = || edges.iter().filter(|(n, _)| n == "saga.state_transition");
        assert!(
            transitions().next().is_some(),
            "transitions were emitted: {edges:?}"
        );
        assert!(
            transitions().all(|(_, parent)| parent.as_deref() == Some("saga.deploy")),
            "every transition nests under the one saga.deploy root: {edges:?}",
        );
    }

    #[tokio::test]
    async fn with_steps_appends_an_iterator_of_steps() {
        init_global_subscriber();
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FilesystemStateStore::new(dir.path()).expect("store");
        let trail: Trail = Trail::default();

        // A dynamically built list of steps composed in a single call, rather
        // than a fixed `.with_step(...).with_step(...)` chain.
        let steps: Vec<Box<dyn Step>> = vec![
            RecordingStep::ok("preflight", &trail),
            RecordingStep::ok("migrate", &trail),
            RecordingStep::ok("activate", &trail),
        ];
        let saga = Saga::new(store, "checkout", "production").with_steps(steps);

        let outcome = saga.run().await.expect("run succeeds");
        assert!(matches!(outcome, SagaOutcome::Committed), "got {outcome:?}");
        assert_eq!(
            *trail.lock().expect("trail"),
            vec!["forward:preflight", "forward:migrate", "forward:activate"],
            "every step from the iterator ran forward, in iterator order"
        );
    }

    #[tokio::test]
    async fn with_step_and_with_steps_compose_in_append_order() {
        init_global_subscriber();
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FilesystemStateStore::new(dir.path()).expect("store");
        let trail: Trail = Trail::default();

        // Mixing the singular and plural builders preserves overall order: a
        // leading step, then a batch, then a trailing step.
        let batch: Vec<Box<dyn Step>> = vec![
            RecordingStep::ok("b", &trail),
            RecordingStep::ok("c", &trail),
        ];
        let saga = Saga::new(store, "checkout", "production")
            .with_step(RecordingStep::ok("a", &trail))
            .with_steps(batch)
            .with_step(RecordingStep::ok("d", &trail));

        saga.run().await.expect("run succeeds");
        assert_eq!(
            *trail.lock().expect("trail"),
            vec!["forward:a", "forward:b", "forward:c", "forward:d"],
            "with_step and with_steps preserve overall append order"
        );
    }

    /// Writes a value into the typed run-state; a no-op compensation. Models a
    /// step that *produces* cross-step data (e.g. a container handle).
    struct Producer;

    #[async_trait::async_trait]
    impl Step<Option<u32>> for Producer {
        fn name(&self) -> &'static str {
            "produce"
        }
        async fn forward(
            &self,
            _ctx: &StepContext,
            state: &mut Option<u32>,
        ) -> Result<(), SagaError> {
            *state = Some(42);
            Ok(())
        }
        async fn compensate(
            &self,
            _ctx: &StepContext,
            state: &mut Option<u32>,
        ) -> Result<(), SagaError> {
            *state = None; // undo the production so rollback leaves clean state
            Ok(())
        }
    }

    /// Reads the value the producer left in the run-state; optionally fails to
    /// force a rollback. Models a step that *consumes* cross-step data.
    struct Consumer {
        fail: bool,
    }

    #[async_trait::async_trait]
    impl Step<Option<u32>> for Consumer {
        fn name(&self) -> &'static str {
            "consume"
        }
        async fn forward(
            &self,
            _ctx: &StepContext,
            state: &mut Option<u32>,
        ) -> Result<(), SagaError> {
            if *state != Some(42) {
                return Err(SagaError::StepFailed {
                    step: "consume".to_owned(),
                    message: format!("expected the producer's 42, got {state:?}"),
                });
            }
            if self.fail {
                return Err(SagaError::StepFailed {
                    step: "consume".to_owned(),
                    message: "forced failure after reading state".to_owned(),
                });
            }
            Ok(())
        }
        async fn compensate(
            &self,
            _ctx: &StepContext,
            _state: &mut Option<u32>,
        ) -> Result<(), SagaError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn run_with_state_passes_typed_values_between_steps() {
        init_global_subscriber();
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FilesystemStateStore::new(dir.path()).expect("store");

        // The caller owns the run-state; steps mutate it through `&mut R` with no
        // Arc<Mutex> and no downcast. The consumer reads what the producer wrote.
        let mut state: Option<u32> = None;
        let outcome = Saga::new(store, "checkout", "production")
            .with_step(Box::new(Producer))
            .with_step(Box::new(Consumer { fail: false }))
            .run_with_state(&mut state)
            .await
            .expect("run succeeds");

        assert!(matches!(outcome, SagaOutcome::Committed), "got {outcome:?}");
        assert_eq!(
            state,
            Some(42),
            "the consumer saw the producer's value and the caller reads it back"
        );
    }

    #[tokio::test]
    async fn run_with_state_threads_state_into_compensation_on_rollback() {
        init_global_subscriber();
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FilesystemStateStore::new(dir.path()).expect("store");

        // The consumer fails after the producer wrote state; the producer's
        // compensation runs with `&mut R` and clears it.
        let mut state: Option<u32> = None;
        let outcome = Saga::new(store, "checkout", "production")
            .with_step(Box::new(Producer))
            .with_step(Box::new(Consumer { fail: true }))
            .run_with_state(&mut state)
            .await
            .expect("run completes (with rollback)");

        assert!(
            matches!(&outcome, SagaOutcome::RolledBack { failed_step, .. } if failed_step == "consume"),
            "got {outcome:?}"
        );
        assert_eq!(
            state, None,
            "the producer's compensation saw the run-state and cleared it"
        );
    }

    #[tokio::test]
    async fn forward_failure_rolls_back_completed_steps() {
        init_global_subscriber();
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
