# Proposal: Adapter axis traits + IPC convergence (Cycle 1.6)

**Status:** PROPOSAL — not implemented, nothing frozen. The adapter trait shape
is the owner's decision (per the Phase 1 directive). This document exists to make
that decision concrete; no Rust in the tree commits to any of it.

**Date:** 2026-05-31
**Scope:** PRD §6 (adapter ecosystem), Phase 1 Cycles 1.6 (trait freeze) + 1.7
(IPC protocol). Informed by a hands-on audit of the installed Confiture CLI.

---

## 1. The one constraint that drives everything: IPC ⇄ in-process convergence

The Phase 1 risk note is explicit: *"the in-process trait and the IPC protocol
describe the same shape, so they must converge."* The cleanest way to guarantee
that is to make them **literally the same trait**:

- `MigrationAdapter` is a normal `#[async_trait]` trait.
- The in-process `ConfitureMigration` / `CommandMigration` implement it directly.
- `IpcMigrationAdapter` *also* implements it — its body just serializes each call
  to JSON-RPC over a child process's stdio and deserializes the reply.

For that to work, **every argument and return type on the migration trait must be
`Serialize + Deserialize`**. That single rule is what keeps the trait and the wire
protocol from drifting: a method you can't express as `{params} → {result}` JSON
is a method an external adapter can't implement, so it can't exist on the trait.

```
                         ┌─────────────────────────────┐
   deploy layer  ──────► │  trait MigrationAdapter      │
   (saga Steps)          └─────────────────────────────┘
                            ▲              ▲
              in-process ───┘              └─── IpcMigrationAdapter
         (ConfitureMigration,                   (spawns `fraisier-adapter-<name>`,
          CommandMigration)                      one JSON-RPC call per trait method)
```

The JSON-RPC method set in PRD §6.2 — `describe`, `current_revision`, `up`,
`down_to`, `verify`, `post_migrate` — becomes, one-to-one, the trait method set.

---

## 2. Shared vocabulary types

All five axes share these. The migration ones must be serde types (rule above);
the others are in-process-only in v1.0 but should follow the same discipline so
an axis can be promoted to IPC later without reshaping.

| Type | Sketch | Notes |
|---|---|---|
| `AdapterCtx` | `{ fraise, environment, host: Option<HostId>, database_url: Option<String>, migrations_path: Option<PathBuf>, workdir: PathBuf, settings: Map<String,Value> }` | Passed to every call. Maps to the IPC `params.ctx`. **Must be serde.** |
| `AdapterError` | `{ adapter: String, operation: String, code: i32, message: String, stderr: Option<String>, source }` | `code` is the JSON-RPC error code for IPC; `stderr` captures subprocess output (PRD §9.3). |
| `Revision` | `struct Revision(String)` | Opaque, adapter-defined format (e.g. `"20260531_abc123"`). |
| `MigrationOutcome` | `{ from: Option<Revision>, to: Option<Revision>, applied: Vec<Revision>, log: String }` | Result of `up`/`down_to`. |
| `VerifyReport` | `{ ok: bool, checks: Vec<{name, ok, detail}> }` | Result of `verify`. |
| `AdapterDescription` | `{ name, version, protocol_version: u32, capabilities: Vec<String> }` | Result of `describe`; drives `fraisier adapter describe`. |
| `HostId` | `struct HostId(String)` | A host name from the inventory. |
| `LbMembership` | `{ state: InPool \| Draining \| Removed, weight: Option<u32> }` | Captured before drain so reattach can restore it exactly. |

`protocol_version` in `describe` is the version handshake (Cycle 1.7 REFACTOR):
the core refuses adapters whose major protocol version it doesn't speak.

---

## 3. The five axis traits (proposed signatures)

### 3.1 `MigrationAdapter` — the one that becomes the IPC contract

```rust
#[async_trait]
pub trait MigrationAdapter: Send + Sync {
    async fn describe(&self) -> Result<AdapterDescription, AdapterError>;
    async fn current_revision(&self, ctx: &AdapterCtx)
        -> Result<Option<Revision>, AdapterError>;
    async fn up(&self, ctx: &AdapterCtx, target: Option<Revision>)
        -> Result<MigrationOutcome, AdapterError>;
    async fn down_to(&self, ctx: &AdapterCtx, target: Revision)
        -> Result<MigrationOutcome, AdapterError>;
    async fn verify(&self, ctx: &AdapterCtx) -> Result<VerifyReport, AdapterError>;
    async fn post_migrate(&self, ctx: &AdapterCtx) -> Result<(), AdapterError>;
}
```

JSON-RPC mapping (`method` = trait method, snake_case):

```
→ {"jsonrpc":"2.0","id":1,"method":"up","params":{"ctx":{…},"target":null}}
← {"jsonrpc":"2.0","id":1,"result":{"from":"…","to":"…","applied":[…],"log":"…"}}
✗ {"jsonrpc":"2.0","id":1,"error":{"code":-32010,"message":"…","data":{"stderr":"…"}}}
```

### 3.2 `ArtifactAdapter`, `ServiceAdapter`, `HealthAdapter`, `LbAdapter` (in-process)

```rust
#[async_trait] pub trait ArtifactAdapter: Send + Sync {
    async fn stage(&self, ctx: &AdapterCtx, host: &HostId) -> Result<StagedArtifact, AdapterError>;
    async fn activate(&self, ctx: &AdapterCtx, host: &HostId, staged: &StagedArtifact) -> Result<(), AdapterError>;
    async fn current(&self, ctx: &AdapterCtx, host: &HostId) -> Result<Option<ArtifactRef>, AdapterError>; // for rollback
}
#[async_trait] pub trait ServiceAdapter: Send + Sync {
    async fn restart(&self, ctx: &AdapterCtx, host: &HostId) -> Result<(), AdapterError>;
    async fn status(&self, ctx: &AdapterCtx, host: &HostId) -> Result<ServiceStatus, AdapterError>;
}
#[async_trait] pub trait HealthAdapter: Send + Sync {
    async fn check(&self, ctx: &AdapterCtx, host: &HostId) -> Result<HealthStatus, AdapterError>; // retry inside
}
#[async_trait] pub trait LbAdapter: Send + Sync {
    async fn drain(&self, ctx: &AdapterCtx, host: &HostId) -> Result<LbMembership, AdapterError>;
    async fn reattach(&self, ctx: &AdapterCtx, host: &HostId, prior: &LbMembership) -> Result<(), AdapterError>;
}
```

### 3.3 How this lands on the Cycle 1.5 saga skeleton

The engine never sees an adapter; the deploy layer wraps each adapter call as a
`Step` (forward + compensate). The compensations *are* the rollback contract:

| Saga `Step` | `forward` | `compensate` |
|---|---|---|
| Migrate | `migration.up(ctx, target)` | `migration.down_to(ctx, prior_revision)` |
| LbDrain | `lb.drain(host)` | `lb.reattach(host, prior)` |
| Activate | `artifact.activate(host, new)` | `artifact.activate(host, prior)` |
| Restart | `service.restart(host)` | `service.restart(host)` after artifact restored |
| HealthCheck | `health.check(host)` | (none — read-only) |

`current_revision`/`artifact.current`/`lb.membership` exist precisely so each
`compensate` can capture the prior state before the forward action mutates it.

---

## 4. Confiture CLI audit — where reality contradicts the PRD's assumptions

I exercised the installed `confiture` (`~/.local/bin/confiture`). The PRD/Phase
assume the JSON-RPC methods map onto existing Confiture commands. **Three of the
six do not map cleanly.** Each is an implementation note for Cycle 1.10 *and* a
shaping input the owner should weigh now.

| Trait method | Assumed Confiture command | Reality | Resolution |
|---|---|---|---|
| `up(target)` | `confiture migrate up [--target V]` | ✅ Exact match (`-t/--target`). | Direct. |
| `current_revision` | (a "current revision" command) | ⚠️ **No such command.** Closest: `migrate status --format json` (exit 0/1/2/3) or `migrate introspect --format json`. | Parse highest applied version from `migrate status --format json`. Map exit code 2 ("no tracking table") → `Ok(None)`. |
| `down_to(target)` | `confiture migrate down_to <rev>` | ❌ **Does not exist.** Confiture only has `migrate down --steps N` (roll back the last N). | Adapter computes N = (#applied newer than `target`) from `migrate status --format json`, then calls `migrate down --steps N`. |
| `verify` | `confiture verify` | ⚠️ **Ambiguous — two commands.** Top-level `confiture verify` = checksum integrity (files vs stored). `confiture migrate verify` = runtime correctness via `.verify.sql` sidecars. | Recommend `migrate verify --format json` (post-apply correctness). Owner must define `verify` semantics (see §5). |
| `post_migrate` | (a post-migrate command) | ❌ **No Confiture command.** It was a fraisier-level hook (Python fraisier: `post_migrate` hooks + `smoke_tests`). | Implement as configured shell hooks, not a Confiture call — or move it off the migration trait entirely (see §5). |
| `describe` | (n/a) | n/a — fraisier-defined capability handshake. | Adapter reports `{name:"confiture", capabilities:["up","down_to","verify","preflight"], …}`. |

**Two further integration findings:**

1. **Connection model mismatch.** `migrate up/down/status` take `--config
   db/environments/{env}.yaml` (a YAML), **not** a raw DSN. Only `preflight`
   takes `--against <url>`. The PRD config (§7.1) supplies `database_url_env`.
   → The `ConfitureMigration` adapter must **synthesize a Confiture config YAML**
   (or set the env Confiture reads) from the resolved `database_url`. This is the
   single biggest practical question for Cycle 1.10 and should be spiked early.

2. **Double locking.** `migrate up` does its *own* distributed locking
   (`--lock-timeout`, `--no-lock`, `--force`). fraisier's saga also locks via the
   `StateStore`. Two lock layers is fine, but decide deliberately: likely let
   Confiture keep its DB-level lock (protects the DB) while the StateStore lock
   serializes the *deploy* (protects the host rollout). Document so they don't
   fight (e.g. avoid `--no-lock` unless intentional).

**Where reality is *better* than the PRD:** `confiture migrate preflight` already
checks reversibility (`.down.sql` present), non-transactional statements
(`CREATE INDEX CONCURRENTLY`), duplicate versions, and checksums — exactly the
forward-compatibility / blue-green-safety lint that is fraisier's stated moat
(PRD G11, §7.1 `forward_compatible_lint`). The §6.2 method list omits it. See §5.

---

## 5. Open questions for the owner (these shape the freeze)

1. **`down_to(target)` vs `down(steps)`?** Recommend **`down_to(target: Revision)`** —
   it is the portable cross-framework abstraction (Alembic `downgrade <rev>`,
   Django `migrate app <name>`, sqlx step-wise). Each adapter translates;
   Confiture computes steps from status. `down(steps)` would leak one framework's
   model into the protocol. *Trade-off:* `down_to` makes adapters do more work,
   but keeps the wire contract framework-neutral.

2. **`verify` semantics — pin them.** Proposed: `verify` = *post-apply
   correctness* (Confiture `migrate verify`, sidecar `.verify.sql`). The checksum
   check (`confiture verify`) is a different concern — fold it into a `preflight`
   capability, not `verify`.

3. **Does `post_migrate` belong on the migration trait at all?** It is the only
   method with no DB-migration meaning. Two options: (a) keep it (lets a Django
   IPC adapter fire Django's `post_migrate` signal); (b) drop it from the trait
   and make post-migrate hooks a **deploy-layer `Step`** driven by config. I lean
   (a) for protocol completeness with a no-op default, but it is genuinely the
   owner's call.

4. **Add `preflight`/`lint` to the migration method set?** Reality supports it and
   it is the competitive moat. Recommend adding
   `async fn preflight(&self, ctx) -> Result<PreflightReport, AdapterError>` to
   the trait and the JSON-RPC set, with `describe.capabilities` advertising
   whether an adapter implements it (the `command` adapter won't; Confiture will).

5. **`AdapterCtx` is the load-bearing serde type.** Everything an adapter needs to
   act must live in it and be JSON-serializable. Confirm the field set (esp. how
   secrets like `database_url` are passed to a *subprocess* adapter — env var vs
   stdin params — without leaking into `ps`/argv).

---

## 6. Recommendation

Freeze the **migration trait = JSON-RPC method set** with the serde-only rule,
adopting `down_to(target)`, `verify` = post-apply correctness, and an added
`preflight` capability. Resolve the Confiture connection-model spike (§4.1) before
Cycle 1.10, because it changes how `AdapterCtx.database_url` is consumed and is
the likeliest source of a late trait change.
