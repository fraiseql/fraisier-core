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
  workers). Off by default; the filesystem store is enough for a single process.
  Enable it if a server runs concurrent deploys of the same `(fraise, env)` pair.

## 2. Write compensating steps

A `Step` is a forward action plus its undo. Implement the trait (it is
`#[async_trait]`) for each effect you want the saga to manage. Make every
`compensate` **idempotent** — it may run after a partial forward.

The engine never inspects your steps; it only calls `forward` in order and, on
the first failure, `compensate` on every **completed** step in reverse (the step
that failed never completed, so it is not compensated).

## 3. Build a store, run, match the outcome

The example below is a **compiled doctest** (run by `cargo test --doc`), so it
cannot drift from the real API. It runs two sagas over a temp directory: one that
commits (its effect persists) and one whose second step fails (the engine rolls
the first step's effect back).

```rust
use std::path::PathBuf;

use async_trait::async_trait;
use fraisier_saga::saga::{Saga, SagaError, SagaOutcome, Step, StepContext};
use fraisier_saga::state_store::FilesystemStateStore;

/// A compensating step: forward writes a file; compensate removes it.
struct WriteFile {
    path: PathBuf,
}

#[async_trait]
impl Step for WriteFile {
    fn name(&self) -> &str {
        "write-file"
    }

    async fn forward(&self, _ctx: &StepContext) -> Result<(), SagaError> {
        std::fs::write(&self.path, b"live").map_err(|e| SagaError::StepFailed {
            step: "write-file".to_owned(),
            message: e.to_string(),
        })
    }

    async fn compensate(&self, _ctx: &StepContext) -> Result<(), SagaError> {
        // Idempotent: "already gone" is success.
        let _ = std::fs::remove_file(&self.path);
        Ok(())
    }
}

/// A step whose forward always fails — used here to force a rollback.
struct AlwaysFails;

#[async_trait]
impl Step for AlwaysFails {
    fn name(&self) -> &str {
        "always-fails"
    }

    async fn forward(&self, _ctx: &StepContext) -> Result<(), SagaError> {
        Err(SagaError::StepFailed {
            step: "always-fails".to_owned(),
            message: "forced failure".to_owned(),
        })
    }

    async fn compensate(&self, _ctx: &StepContext) -> Result<(), SagaError> {
        Ok(())
    }
}

let rt = tokio::runtime::Runtime::new().unwrap();
rt.block_on(async {
    let dir = tempfile::tempdir().unwrap();

    // (1) A committing run: the file is written and the saga commits.
    let store = FilesystemStateStore::new(dir.path()).unwrap();
    let committed = dir.path().join("service.live");
    let outcome = Saga::new(store, "demo", "production")
        .with_step(Box::new(WriteFile { path: committed.clone() }))
        .run()
        .await
        .unwrap();
    assert!(matches!(outcome, SagaOutcome::Committed));
    assert!(committed.exists(), "a committed run keeps its effects");

    // (2) A failing run: the engine rolls back, so WriteFile's effect is undone.
    let store = FilesystemStateStore::new(dir.path()).unwrap();
    let rolled_back = dir.path().join("other.live");
    let outcome = Saga::new(store, "demo", "staging")
        .with_step(Box::new(WriteFile { path: rolled_back.clone() }))
        .with_step(Box::new(AlwaysFails))
        .run()
        .await
        .unwrap();
    assert!(matches!(outcome, SagaOutcome::RolledBack { .. }));
    assert!(!rolled_back.exists(), "a rolled-back run leaves nothing behind");
});
```

## 4. Outcomes

`Saga::run` returns `Result<SagaOutcome, SagaError>`. A clean business failure is
`Ok(SagaOutcome::RolledBack { .. })`, **not** an `Err` — `Err` is reserved for
engine/infrastructure failures (locking, state persistence). Match all three
outcomes (the enum is `#[non_exhaustive]`, so keep a catch-all arm):

- `Committed` — every step ran; the operation is durable.
- `RolledBack { failed_step, reason }` — a step failed and every completed step
  was compensated; you are back at the pre-run baseline.
- `PartialRollback { reason }` — a *compensation itself* failed. The engine never
  pretends the undo succeeded; treat this as an **operator-intervention incident**
  (a distinct error state, alert, no auto-retry).

## 5. Threading per-run state between steps

`StepContext` carries the `(fraise, environment)` key only. When a later step (or
a compensation) needs a value an earlier step produced — a container handle, a
spawned pid — share it through your own `Arc<Mutex<…>>`, cloned into each step.
The engine intentionally does not own that state. (A typed run-state slot on
`StepContext` is a tracked future ergonomics improvement; see the API report.)

## 6. What to embed — and what not to

Embed the **engine** (`fraisier-saga`) and author steps over **your own**
effects. Do **not** try to pull `fraisier-core`'s deploy composition
(artifact/migration/service steps) into your application: it is not on the public
surface, and the crate-graph rule keeps it that way. If the public engine API
cannot express something you need, that is a finding worth surfacing upstream —
the engine's frozen shape grows only by additive, non-breaking change.
