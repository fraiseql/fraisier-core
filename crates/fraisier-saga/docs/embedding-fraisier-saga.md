# Embedding `fraisier-saga`

`fraisier-saga` is the **generic engine** behind fraisier: an atomic saga state
machine that runs an ordered list of compensable steps, persists progress through
a pluggable `StateStore`, and rolls back in reverse on the first failure. It ships
**no deploy semantics** — no artifact/migration/service/health steps. Those live
one layer up in `fraisier-core` and are deliberately kept out of the engine's
public surface (the crate-graph rule: `fraisier-core → fraisier-saga`, never the
reverse).

That makes the engine **embeddable**: a third-party application can depend on
`fraisier-saga` directly, author its **own** compensating steps wrapping its own
side effects, and get **atomic rollback** for a multi-step operation it would
otherwise have to unwind by hand.

> This is not hypothetical: the SpecQL hosting platform embeds the engine exactly
> this way to make its container provisioning atomic. See that write-up and the
> public-API audit in the SpecQL repo's `docs/EMBEDDING-API-REPORT.md`.

## 1. Add the dependency

```toml
[dependencies]
# Once published, a crates.io version; for a local checkout, a path dep.
fraisier-saga = "1"
async-trait = "0.1"   # to implement the `Step` trait
```

Leave **both** engine features off unless you need them:

- `otel` — OTLP span export. Off by default so embedders pay no OpenTelemetry
  weight. The fraisier CLI turns it on; a library embedder rarely wants it.
- `sqlite` — a SQLite `StateStore` backend (atomic per-fraise locks across many
  workers). Off by default; the filesystem store is enough for a single process,
  and `MemoryStateStore` (always available) is enough for tests.
  Enable it if a server runs concurrent deploys of the same `(fraise, env)` pair.

## 2. Write compensating steps over a run state

A `Step<R>` is a forward action plus its undo, both handed a `&mut R` — the
**run state** the engine threads through the whole run. Because a saga runs its
steps strictly one at a time, each step gets *exclusive* access in turn: an early
step **produces** a value (a container handle, a staged artifact) by writing it
into `R`, and a later step — or an earlier step's compensation — **consumes** it
by reading it back. No `Arc<Mutex>`, no interior mutability, no downcasting.

`R` defaults to `()`, so a saga whose steps share nothing is just `Step` (i.e.
`Step<()>`) and runs via `Saga::run`. Implement the trait (it is
`#[async_trait]`) for each effect. Make every `compensate` **idempotent** — it may
run after a partial forward. The engine never inspects your steps; it only calls
`forward` in order and, on the first failure, `compensate` on every **completed**
step in reverse (the step that failed never completed, so it is not compensated).

## 3. Build a store, run, match the outcome

The example below is a **compiled doctest** (run by `cargo test --doc`), so it
cannot drift from the real API. It runs two sagas: one that commits (the `start`
step's effect, recorded in the run state, survives) and one whose `health` step
fails (the engine rolls `start` back, which clears the handle).

```rust
use async_trait::async_trait;
use fraisier_saga::saga::{Saga, SagaError, SagaOutcome, Step, StepContext};
use fraisier_saga::state_store::MemoryStateStore;

/// The run state shared across steps: the handle the `start` step produces and
/// the `health` step (and `start`'s compensation) consume.
#[derive(Default)]
struct Deploy {
    handle: Option<String>,
}

/// Forward "starts" something and records its handle in the run state; compensate
/// stops it. Idempotent — it may run after a partial forward.
struct Start;

#[async_trait]
impl Step<Deploy> for Start {
    fn name(&self) -> &str {
        "start"
    }

    async fn forward(&self, _ctx: &StepContext, state: &mut Deploy) -> Result<(), SagaError> {
        state.handle = Some("container-123".to_owned());
        Ok(())
    }

    async fn compensate(&self, _ctx: &StepContext, state: &mut Deploy) -> Result<(), SagaError> {
        state.handle = None; // stop whatever `forward` started, if anything
        Ok(())
    }
}

/// Forward health-checks the started handle; `healthy = false` models a sick
/// release whose check never comes up, forcing a rollback. A probe has no effect,
/// so compensate is a no-op.
struct Health {
    healthy: bool,
}

#[async_trait]
impl Step<Deploy> for Health {
    fn name(&self) -> &str {
        "health"
    }

    async fn forward(&self, _ctx: &StepContext, state: &mut Deploy) -> Result<(), SagaError> {
        if state.handle.is_none() {
            return Err(SagaError::StepFailed {
                step: "health".to_owned(),
                message: "nothing was started".to_owned(),
            });
        }
        if self.healthy {
            Ok(())
        } else {
            Err(SagaError::StepFailed {
                step: "health".to_owned(),
                message: "the release never became healthy".to_owned(),
            })
        }
    }

    async fn compensate(&self, _ctx: &StepContext, _state: &mut Deploy) -> Result<(), SagaError> {
        Ok(())
    }
}

let rt = tokio::runtime::Runtime::new().unwrap();
rt.block_on(async {
    // (1) A committing run: `start` records the handle, `health` reads it and
    // passes. The caller owns the run state and reads it back afterwards.
    let mut state = Deploy::default();
    let outcome = Saga::new(MemoryStateStore::new(), "demo", "production")
        .with_step(Box::new(Start))
        .with_step(Box::new(Health { healthy: true }))
        .run_with_state(&mut state)
        .await
        .unwrap();
    assert!(matches!(outcome, SagaOutcome::Committed));
    assert_eq!(state.handle.as_deref(), Some("container-123"));

    // (2) A failing run: `health` fails, so `start`'s compensation runs and
    // clears the handle — the engine rolled the prior step's effect back.
    let mut state = Deploy::default();
    let outcome = Saga::new(MemoryStateStore::new(), "demo", "staging")
        .with_step(Box::new(Start))
        .with_step(Box::new(Health { healthy: false }))
        .run_with_state(&mut state)
        .await
        .unwrap();
    assert!(matches!(outcome, SagaOutcome::RolledBack { .. }));
    assert_eq!(state.handle, None, "rollback undid start's effect");
});
```

## 4. Outcomes

`Saga::run_with_state` returns `Result<SagaOutcome, SagaError>`. A clean business
failure is `Ok(SagaOutcome::RolledBack { .. })`, **not** an `Err` — `Err` is
reserved for engine/infrastructure failures (locking, state persistence). Match
all three outcomes (the enum is `#[non_exhaustive]`, so keep a catch-all arm):

- `Committed` — every step ran; the operation is durable.
- `RolledBack { failed_step, reason }` — a step failed and every completed step
  was compensated; you are back at the pre-run baseline.
- `PartialRollback { reason }` — a *compensation itself* failed. The engine never
  pretends the undo succeeded; treat this as an **operator-intervention incident**
  (a distinct error state, alert, no auto-retry).

## 5. Run state, statelessness, and dynamic step lists

- **Run state (`R`)** is threaded as `&mut R` into every `forward`/`compensate`,
  as in the example above. It is the typed, compile-time-checked way to hand a
  value from one step to another — no shared `Arc<Mutex>` to thread into each
  step, and no runtime downcast. The caller owns the `R` and can read it after
  the run (the committed handle, the recorded revision, …).
- **Stateless sagas** (`R = ()`) skip all of that: implement plain `Step`, ignore
  the `&mut ()` argument, and run with `Saga::run` instead of `run_with_state`.
- **Dynamic step lists** — when the steps are built conditionally (a migration
  step only when needed, one rollout step per host batch), assemble a
  `Vec<Box<dyn Step<R>>>` and add them in one call with `Saga::with_steps`
  instead of folding over `with_step`.
- **Choosing a store** — `MemoryStateStore` (always available, no feature) is the
  zero-setup backend for unit tests and single-process callers; it shares one
  backing store across clones but does **not** survive the process.
  `FilesystemStateStore` persists across calls (so a request/response server can
  report deploy state); enable the `sqlite` feature for atomic per-fraise locks
  across many workers.

## 6. What to embed — and what not to

Embed the **engine** (`fraisier-saga`) and author steps over **your own**
effects. Do **not** try to pull `fraisier-core`'s deploy composition
(artifact/migration/service steps) into your application: it is not on the public
surface, and the crate-graph rule keeps it that way. If the public engine API
cannot express something you need, that is a finding worth surfacing upstream —
the engine grows by deliberate, reviewed change.
