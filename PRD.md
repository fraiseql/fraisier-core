# Fraisier v1.x — Product Requirements Document

**Status**: Draft v0.3 (architectural reshape + multi-host)
**Author**: Lionel Hamayon
**Date**: 2026-05-31

---

## 1. Summary

This project is the **Rust v1.x line** of fraisier. It is a clean-room reimplementation that **reshapes** fraisier from a Python-multi-framework deploy CLI into a **deploy orchestration engine with first-class atomic multi-host migration safety**, shipped as `fraisier v1.0.0-beta.1` on crates.io.

Fraisier is also the **deploy engine of the FraiseQL stack** — the open-source Supabase alternative the author is building (SpecQL + fraiseql + Confiture + fraisier, plus Auth/Storage/Realtime/Functions components already integrated into fraiseql-server and adjacent crates). Most end users of the FraiseQL stack will invoke fraisier indirectly via SpecQL's `deploy` command rather than directly; power users on the data-intensive bare-metal path will use it directly. The PRD scope below is the same in both cases — the umbrella positioning does not change the engineering scope, only the way the value is exposed to end users. See `~/code/fraise-stack/ROADMAP.md` for the umbrella roadmap.

The reshape is deliberate. The conversation that produced this PRD established that none of fraisier's value props are Python-specific, and that the architectural pivots available right now (pluggable state, IPC adapter protocol, engine-as-crate, multi-host) are cheap to commit to before week 1 of code and prohibitively expensive to retrofit later. The pivots also expand the addressable audience meaningfully without commensurate effort increase, because dropping the in-process Python adapters from the v1.0 core offsets the architectural work.

### 1.1 Positioning

> **The only deploy tool that gets atomic migrations right across multiple hosts.**
>
> Single static binary CLI. Embeddable Rust library. Adapter ecosystem you can extend in any language via the IPC protocol. Pluggable state backend ready for multi-host coordination. Atomic rollback as the marketing story — and the only one that actually rolls back across N hosts.

### 1.2 Version line

| Line | Versions | Language | Status |
|---|---|---|---|
| `fraisier` v0.x (PyPI) | v0.1 → v0.32 → security fixes only | Python | Legacy, feature-frozen |
| `fraisier` v1.x (crates.io) | v1.0.0-alpha.1 → v1.0.0-beta.1 → v1.0.0 GA | Rust | The future |

The Rust workspace is developed under the working name `fraisier-core` for weeks 1–4. At the end of week 4, if the validation checkpoint passes, the crate is renamed to `fraisier` and published as `v1.0.0-beta.1`.

### 1.3 Primary consumers

1. **fraiseql v2** — deploys via the Rust binary on bare-metal hosts with no Python runtime required.
2. **specql-platform** — embeds the orchestration engine crate; atomic-migration deploy becomes a platform-native feature.
3. **Data-intensive multi-host bare-metal shops** (the genuinely new audience) — Hetzner / OVH / self-hosted teams running 2–10 app hosts behind a load balancer, against a shared PostgreSQL primary. Currently served badly by Kamal (no migration atomicity) or Ansible (no atomicity at all).
4. **Existing Python fraisier users** — `fraises.yaml` parses unchanged; Python framework adapters available as external IPC adapter packages, not in the v1.0 core binary.
5. **Rust ecosystem deploy users** — addressable via reference adapters (sqlx, refinery) and the IPC protocol.

---

## 2. Motivation

Three threads from the conversation that drove this PRD:

1. **None of fraisier's value props are Python-specific.** Atomic orchestration, socket activation, migration safety, systemd integration — all language-agnostic. The Python-multi-framework adapter list was incidental, not essential.
2. **The user's stack is Rust-native.** Fraiseql v2 (Rust), specql (Rust), specql-platform (Rust, becoming a hosted product). A Python deploy tool deploying Rust binaries is a coherence tax.
3. **Several architectural pivots are free now, expensive later.** Pluggable state (unlocks multi-host), IPC adapter protocol (unlocks any-language ecosystem), engine-as-crate (unlocks specql-platform embedding), drop-Python-adapters-from-core (unlocks scope reduction).

Combined with the data-intensive bare-metal positioning, this is the right moment to take the reshape rather than do a literal port. The cost is a slightly different week-1 architecture; the gain is a product with a meaningfully larger addressable market and a clean foundation that will not need a v2 rewrite in three years.

The honestly-disclosed trade: more architectural work in week 1, less adapter-porting work in week 2, multi-host work in week 4. Net effort is roughly neutral; net product surface is qualitatively different.

---

## 3. Goals

### 3.1 Primary goals

- **G1**: Ship a Rust workspace organized around three crates: `fraisier-saga` (atomic orchestration engine), `fraisier-core` (deploy-specific composition), `fraisier-cli` (binary).
- **G2**: Pluggable state backend with filesystem and SQLite implementations in v1.0; Postgres backend designed-in but deferred to v1.1.
- **G3**: IPC adapter protocol (JSON-RPC over stdio) for migration and load-balancer adapters; adapters are external processes discovered via PATH.
- **G4**: Multi-host deploy plan with `all-at-once` and `rolling(n)` strategies, marked `experimental` in v1.0.0-beta.1, promoted to stable at v1.0.0 GA after production validation.
- **G5**: Atomic rollback across multiple hosts: migration runs once against shared DB, host-level fetch/restart/health is per-host, failure at any point rolls back DB once + all hosts.
- **G6** *(withdrawn 2026-06-02)*: `fraises.yaml` runtime compatibility. Withdrawn before implementation — the project has a single Python fraisier user, who hand-converts to `fraisier.toml`. New and migrating configs use the native schema only. See the Decision log.
- **G7**: OpenTelemetry-native observability; every state transition is a span, every adapter call a child span.
- **G8**: specql-platform embeds the engine crate and deploys a fixture app via library call.

### 3.2 Secondary goals

- **G9**: Three-axis adapter contract (migration / service / health) plus an artifact axis and a load-balancer axis for multi-host.
- **G10**: Webhook server (socket-activated on systemd, standalone HTTP elsewhere).
- **G11**: Forward-compatibility lint, surfaced via the migration adapter's `preflight` capability. Confiture implements it natively (`migrate preflight`: reversibility, non-transactional statements, duplicate versions, checksum integrity); the `command` adapter declines (no `preflight` capability advertised); other framework adapters implement it as their ecosystems allow. The moat is making it a first-class, advertisable contract every migration adapter can opt into — not a Confiture-unique trick. Enforced at config-time so blue-green safety is checked before deploy.

### 3.3 Non-goals (this release)

- **Not** a Kubernetes deployer. Out of scope, as always.
- **Not** multi-region. Out of scope.
- **Not** multi-service cross-app coordination. Single app, multiple hosts only.
- **Not** blue-green deploys in v1.0.0-beta.1 — designed-in but deferred to v1.0.0 GA.
- **Not** Python adapter reimplementations in the core binary. They live as external IPC adapter packages.
- **Not** a Postgres state backend in v1.0.0-beta.1. Filesystem + SQLite ship; Postgres is v1.1.
- **Not** D-Bus systemd integration. Shell out to `systemctl` in v1.0; D-Bus is v1.1+.
- **Not** PyPI republishing or PyO3 bindings.
- **Not** a `fraises.yaml` parser or configuration migrator. Python fraisier's format is not read at runtime; the sole Python user hand-converts to `fraisier.toml` (decision 2026-06-02).

### 3.4 Feature scope matrix

| Subsystem | Python v0.32 equivalent | v1.0.0-beta.1 target |
|---|---|---|
| Single-host orchestration | `trigger-deploy` | ✅ Required, via `fraisier-saga` |
| Multi-host orchestration | (none) | ✅ Required, `experimental` flag |
| Rolling strategy | (none) | ✅ `all-at-once`, `rolling(n)` |
| Blue-green | (none) | ⏳ v1.0.0 GA |
| State backend | implicit file state | ✅ Pluggable; filesystem + SQLite ship |
| Adapter protocol | in-process per-framework | ✅ IPC over stdio (JSON-RPC) |
| Migration adapters in core | Django, Alembic, Flask-Migrate, Peewee, Confiture | ✅ Confiture + `command` + reference sqlx adapter only |
| Migration adapters as external IPC packages | (n/a) | ✅ Django, Alembic, Flask-Migrate, Peewee — separate repos, separate release cadence |
| Service managers | systemd, rc.d, docker-compose | ✅ All three |
| Health checks | HTTP probe | ✅ Required |
| Load balancer adapter | (none) | ✅ Reference: nginx; trait open for HAProxy/cloud |
| Artifact source | git pull | ✅ `release` (URL+sha256), `git`, `local` |
| Config | `fraises.yaml` + `!envvar` | ✅ `fraisier.toml` native format only (`fraises.yaml` runtime compat withdrawn 2026-06-02) |
| Post-migration verification | `post_migrate` hooks, `smoke_tests` | ✅ Required |
| Webhook server | socket-activated | ✅ Required + standalone HTTP fallback |
| Bootstrap | SSH-based | ✅ Required (Python-subprocess fallback acceptable for beta) |
| Scaffold | `scaffold`, `scaffold-install` | ✅ Required, with `--prune` |
| Release workflow | `ship` | ✅ For `Cargo.toml` and `pyproject.toml` |
| Versioning | `version show`, `version bump` | ✅ Required |
| DB operations | `db migrate`, `db restore`, `db reset`, `backup` | ✅ Required |
| Status / introspection | `deployment-status`, `list`, `health` | ✅ Required |
| Rollback (manual) | `rollback` | ✅ Required |
| Sync | `fraisier/sync/*` namespace + orphan reclaim | ✅ Required |
| Providers | `providers`, `provider-test` | ✅ Required |
| Init | `fraisier init` | ✅ Required |
| Self-upgrade restart | v0.31 coordinated restart | ✅ Required |
| Scheduled install | v0.30/v0.32 features | ✅ Required |
| Observability | logs only | ✅ OpenTelemetry traces + structured logs |
| Forward-compat migration lint | (none) | ✅ Via the migration adapter's `preflight` capability (Confiture native; `command` declines) |

Rows marked ✅ ship in v1.0.0-beta.1. ⏳ indicates designed-in but deferred. Anything not in this matrix is out of scope.

---

## 4. Users and use cases

### 4.1 Data-intensive multi-host bare-metal operator (new primary audience)

A team running 3 app hosts behind nginx against a 2TB PostgreSQL primary on Hetzner.

**Story**: "We need to deploy a new version with a migration. Migration must run once against the shared DB. App restart must be rolling so we maintain availability. If anything fails — migration error, any host fails health — everything rolls back to the prior state, on every host, atomically."

**Flow**: `fraisier trigger-deploy myapp production` → preflight all hosts → migrate once → for each host in rolling order: drain from LB, fetch artifact, restart service, health-check, re-add to LB → verify → committed. Any failure → DB rolls back + all hosts roll back + LB membership restored.

**Why this matters**: nobody else does this correctly. Kamal punts on migration atomicity; Ansible has no atomicity guarantees. This audience currently writes their own scripts and lives with the gaps.

### 4.2 fraiseql v2 operator

A developer deploying fraiseql v2 to a single bare-metal Linux host. See PRD v0.1 §4.1 — unchanged. Multi-host configuration optional.

### 4.3 specql-platform

The platform crate embeds `fraisier-saga` (the engine) directly. Atomic deploy is a platform feature, not a subprocess call. See PRD v0.1 §4.2.

### 4.4 Existing Python fraisier user

There is exactly one, on the FraiseQL/Confiture stack. They hand-convert their `fraises.yaml` to `fraisier.toml` once and switch to the Rust binary (decision 2026-06-02 — `fraises.yaml` is not parsed at runtime). The external Python framework adapters (`fraisier-adapter-django`, etc.) remain available on PATH for anyone who wants them, but are no longer load-bearing for this audience.

### 4.5 Rust ecosystem adopter

A Rust developer using sqlx or refinery on bare metal. Installs `fraisier` from crates.io, uses the reference sqlx adapter or wires their own via the `command` adapter. Never touches Python.

### 4.6 Any-language ecosystem contributor

A Drizzle or Prisma user writes a JSON-RPC adapter as a Node.js binary, publishes `fraisier-adapter-prisma` to npm. Users install it on PATH; fraisier discovers it; no change to fraisier core. This is the audience expansion the IPC protocol enables.

---

## 5. Scope: orchestration model

### 5.1 Two-layer state machine

**Layer 1 — Saga (the engine, `fraisier-saga` crate):** a generic atomic state machine with typed events, rollback semantics, and a `StateStore` trait. Not deploy-specific. Reusable for any multi-step operation with rollback. Public crate that fraisier and specql-platform both depend on.

**Layer 2 — Deploy (`fraisier-core` crate):** composes the saga primitives into deploy-specific flows. Single-host and multi-host both live here.

### 5.2 Single-host flow

Per the original PRD §5.1: Idle → Preflight → Fetch → Migrate → Restart → HealthCheck → Verify → Committed. Rollback semantics unchanged.

### 5.3 Multi-host flow

```
Idle
  └─> Preflight (all hosts reachable, DB reachable, current revision)
        └─> FetchAll (parallel artifact fetch to all hosts, staged)
              └─> Migrate (ONCE against shared DB; not per-host)
                    └─> RolloutLoop (per rolling strategy):
                          For each host (or batch) in strategy order:
                            ├─> LbDrain (remove from LB)
                            ├─> Activate (swap staged artifact in)
                            ├─> Restart (service adapter)
                            ├─> HealthCheck (host-local)
                            └─> LbReattach
                          ├─ any host fails ──> Rollback.MultiHost
                          └─ all hosts ok ──> VerifyGlobal
                                                ├─ fail ──> Rollback.MultiHost
                                                └─ ok ──> Committed
```

### 5.4 Multi-host rollback contract

If failure occurs at any state, rollback proceeds:

1. **Reverse the rollout** in reverse order: each host restored to prior artifact, restarted, re-added to LB. Hosts that never advanced are skipped.
2. **Migration rolls back once** against the shared DB.
3. If any host rollback step fails, the state moves to `PartialRollback` and the operator is notified. The core does not pretend a failed rollback succeeded.

### 5.5 Rolling strategies

- `all-at-once`: every host updated in parallel; brief full downtime tolerated. Simplest. Default for single-host configs.
- `rolling(n)`: process N hosts at a time; rest stay live. Default for multi-host configs.
- `blue-green` (designed-in, v1.0.0 GA): provision new host set, swap traffic atomically. Requires forward-compatible migrations (Confiture lint enforces this).

### 5.6 Event stream and OpenTelemetry

Every state transition is a typed event AND an OTel span. The deploy itself is a trace. Adapter calls are child spans. Operators get a "what happened during this deploy" view in their OTel backend (Grafana Tempo, Jaeger, etc.) by default.

---

## 6. Scope: adapter ecosystem

### 6.1 Adapter axes

Five distinct axes, each with its own trait:

1. **Artifact** (`ArtifactAdapter`): how to get code/binary to a host. `release` (URL), `git`, `local`.
2. **Migration** (`MigrationAdapter`): how to run database migrations. Confiture, command, sqlx, plus external IPC adapters.
3. **Service** (`ServiceAdapter`): how to start/stop/restart. systemd, rc.d, docker-compose.
4. **Health** (`HealthAdapter`): how to verify. HTTP probe; trait open for gRPC, TCP, custom command.
5. **Load Balancer** (`LbAdapter`): how to drain/reattach. nginx reference impl; trait open for HAProxy, Caddy, cloud APIs.

### 6.2 IPC adapter protocol (the architectural pivot)

In-process trait adapters live in the core for axes where the surface is stable and the ecosystem is small (artifact, service, health, LB). For **migration** — where the long tail is largest — adapters are external processes discovered via PATH, speaking **JSON-RPC over stdio**.

**The convergence rule (Phase 1 review):** the in-process `MigrationAdapter` trait and the IPC protocol are *the same shape* — the IPC adapter is a transport that implements the same trait by serializing each call. This is enforced mechanically: **every argument and return type on every adapter trait method is `Serialize + Deserialize`.** A method that would require a non-serializable type is a wrong method, not a missing capability. This is the only constraint that prevents the in-process and IPC paths from drifting over the project's lifetime.

Protocol shape:

```
Request:   {"jsonrpc":"2.0","id":1,"method":"current_revision","params":{"ctx":...}}
Response:  {"jsonrpc":"2.0","id":1,"result":{"revision":"20260531_abc123"}}
Error:     {"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"...","data":{...}}}
```

Methods: `describe` (capability + protocol-version handshake), `current_revision`, `up`, `down_to`, `verify`, `preflight` (forward-compat lint — Phase 1 review Decision 4), `post_migrate`. `preflight` and `post_migrate` are optional: an adapter advertises support via `describe.capabilities`. Secrets are passed to subprocess adapters via environment variables (the core sets `env[logical] = value` at spawn), never in JSON params or argv (Phase 1 review Decision 5).

Discovery: binaries on PATH matching `fraisier-adapter-<name>`. Selected by `migration.adapter = "<name>"` in config. The core spawns the binary as a child process, communicates over stdin/stdout, terminates on completion.

Benefits:
- Adapters in any language (Python adapter for Django is a tiny pip package; Node.js adapter for Prisma is an npm package; Rust adapter for sqlx is a cargo binary).
- Adapters version independently of fraisier.
- Crash isolation: an adapter crash does not kill the orchestrator.
- Community-extensible without touching the core repo.

### 6.3 v1.0.0-beta.1 adapter inventory

**In-process (in the `fraisier` binary):**

| Axis | Adapter | Notes |
|---|---|---|
| Artifact | `ReleaseArtifact`, `GitArtifact`, `LocalArtifact` | Three sources |
| Migration | `ConfitureMigration` | Native, not via IPC. Wraps the `confiture migrate <subcommand>` CLI. Note: Confiture exposes **no** `current_revision`/`down_to`/`post_migrate` subcommands — the adapter derives `current_revision` from `migrate status --format json`, implements `down_to(target)` by computing `--steps N` from that status, and treats `post_migrate` as configured hooks. Connection-model translation (Confiture takes `--config <yaml>`, not a DSN) per Phase 1 review §3. Forward-compat lint via `migrate preflight`; blue-green prep. |
| Migration | `CommandMigration` | Universal escape hatch |
| Service | `SystemdService`, `RcService`, `DockerComposeService` | All three |
| Health | `HttpHealth` | |
| LB | `NginxLb` | Reference; trait public for others |

**As external IPC adapter packages (released separately, separate repos):**

| Adapter | Source language | Why external |
|---|---|---|
| `fraisier-adapter-sqlx` | Rust | Reference IPC implementation; validates the protocol for Rust authors |
| `fraisier-adapter-django` | Python | Maintained alongside Python fraisier sunset; users `pip install` it |
| `fraisier-adapter-alembic` | Python | Same |
| `fraisier-adapter-flask-migrate` | Python | Same |
| `fraisier-adapter-peewee` | Python | Same |

Python adapter packages are part of the migration path from Python fraisier v0.x: existing users keep their `fraises.yaml`, install the adapter packages, and switch the `fraisier` binary.

---

## 7. Scope: configuration

### 7.1 Native format: `fraisier.toml`

Schema-driven, TOML-serialized. Schema lives in the `fraisier-core` crate and is publishable as a JSON Schema for IDE integration. Single deployable per file or multi-fraise workspace via `[fraises.<name>]` tables.

```toml
[deploy]
name = "fraiseql"
environment = "production"

[hosts]
strategy = "rolling"
rolling_batch_size = 1
inventory = [
  { name = "web-1", address = "web1.internal" },
  { name = "web-2", address = "web2.internal" },
  { name = "web-3", address = "web3.internal" },
]

[artifact]
source = "release"
release_url = "https://github.com/.../fraiseql-{version}-musl.tar.gz"
checksum_url = "https://github.com/.../fraiseql-{version}-musl.tar.gz.sha256"

[migration]
adapter = "confiture"
database_url_env = "FRAISEQL_DATABASE_URL"
migrations_path = "./migrations"
forward_compatible_lint = true
# `database_url_env` is the *source* env var name. The core exposes it to the
# adapter as `AdapterCtx.env_secrets["DATABASE_URL"] = "FRAISEQL_DATABASE_URL"`;
# the adapter resolves the value via `ctx.secret("DATABASE_URL")`. For IPC
# adapters the core reads the source var and re-exposes the value under the
# logical name `DATABASE_URL` on the spawned child (Phase 1 review Decision 5).

[service]
adapter = "systemd"
unit = "fraiseql.service"

[health]
adapter = "http"
url = "http://{host.address}:8080/health"
expected_status = 200

[lb]
adapter = "nginx"
config_path = "/etc/nginx/sites-available/fraiseql"
upstream = "fraiseql_upstream"
```

### 7.1a SpecQL-deployed app preset

For applications deployed via SpecQL (the FraiseQL stack's umbrella product), a one-line preset replaces most of the explicit configuration. The preset fills in Confiture + fraiseql defaults automatically.

```toml
[specql]
schema = "./schema.toml"           # path to the SpecQL schema
environment = "production"
hosts = ["web1.internal", "web2.internal"]
```

The preset expands at config-load time into the full `[deploy]` / `[artifact]` / `[migration]` / `[service]` / `[health]` / `[lb]` set with Fraise-stack-conventional defaults. Users who need to override any field can still write the explicit blocks alongside — explicit blocks win. The preset is implemented in `fraisier-config` as a config-time macro, not a runtime layer.

### 7.2 `fraises.yaml` compatibility — withdrawn (2026-06-02)

The runtime `fraises.yaml` compatibility layer was dropped before implementation: the project has a single Python fraisier user, who hand-converts to `fraisier.toml`. The mapping the compat layer would have performed — Python's monolithic `framework: django` → IPC adapter discovery (`migration.adapter = "django"` → spawn `fraisier-adapter-django` from PATH) — is instead expressed directly in the native config. `fraisier.toml` is the only configuration format the binary reads.

### 7.3 State backend configuration

```toml
[state]
backend = "sqlite"          # or "filesystem"
path = "/var/lib/fraisier/state.db"
```

Filesystem is the default for single-host; SQLite recommended for multi-host (atomic per-fraise locks and progress tracking). Postgres backend is a v1.1 addition without core changes.

---

## 8. Scope: the CLI

Mirrors the Python v0.32 surface with multi-host additions:

```
fraisier init
fraisier list [--flat]
fraisier health [--json] [--host <name>]
fraisier deployment-status <fraise> [--per-host]

fraisier trigger-deploy <fraise> <env> [--dry-run] [--force] [--strategy <name>] [--hosts <list>]
fraisier rollback <fraise> <env> [--to <revision>] [--hosts <list>]

fraisier ship patch|minor|major [--dry-run] [--no-deploy]
fraisier version show
fraisier version bump patch|minor|major

fraisier db migrate <fraise> -e <env>
fraisier db restore <fraise> -e <env>
fraisier db reset <fraise> -e <env>
fraisier backup <fraise> -e <env>

fraisier bootstrap -e <env> [--dry-run] [--host <name>]
fraisier scaffold [--dry-run]
fraisier scaffold-install [--dry-run] [--yes] [--prune]
fraisier providers
fraisier provider-test <type>

fraisier sync
fraisier webhook-server
fraisier validate-config

fraisier adapter list           # discovered IPC adapters on PATH
fraisier adapter describe <name>
```

---

## 9. Architecture

### 9.1 Workspace layout

```
fraisier-core/                            (renamed to fraisier/ at v1.0.0-beta.1)
├── Cargo.toml                            # workspace manifest
├── PRD.md
├── .phases/                              # phase plans
├── crates/
│   ├── fraisier-saga/                    # generic atomic state machine engine
│   │   ├── src/state_machine.rs
│   │   ├── src/state_store.rs            # StateStore trait + filesystem + sqlite impls
│   │   ├── src/events.rs
│   │   └── src/otel.rs
│   ├── fraisier-core/                    # deploy-specific composition over saga
│   │   ├── src/single_host.rs
│   │   ├── src/multi_host.rs
│   │   ├── src/rollout.rs                # rolling strategies
│   │   └── src/adapter_axes.rs           # 5 traits
│   ├── fraisier-cli/                     # binary
│   ├── fraisier-config/                  # fraisier.toml + fraises.yaml compat
│   ├── fraisier-webhook/                 # socket-activated server
│   ├── fraisier-bootstrap/               # SSH-based provisioning
│   ├── fraisier-scaffold/                # systemd/nginx/CI file generation
│   ├── fraisier-sync/                    # branch namespace + orphan reclaim
│   ├── fraisier-ship/                    # version bump + release workflow
│   ├── fraisier-db/                      # db migrate/restore/reset/backup
│   ├── fraisier-ipc/                     # JSON-RPC adapter protocol client
│   ├── fraisier-adapter-confiture/       # in-process Confiture adapter (intimate integration)
│   ├── fraisier-adapter-command/         # in-process command adapter
│   ├── fraisier-adapter-systemd/
│   ├── fraisier-adapter-rc/
│   ├── fraisier-adapter-docker-compose/
│   ├── fraisier-adapter-http/
│   ├── fraisier-adapter-nginx/
│   ├── fraisier-artifact-release/
│   ├── fraisier-artifact-git/
│   └── fraisier-artifact-local/
└── tests/
    ├── integration/
    ├── compat/                           # fraises.yaml corpus
    └── multi_host/                       # fixture multi-host suite

External repos (not in this workspace):
├── fraisier-adapter-sqlx/                # reference Rust IPC adapter
├── fraisier-adapter-django/              # Python IPC adapter
├── fraisier-adapter-alembic/             # Python IPC adapter
├── fraisier-adapter-flask-migrate/       # Python IPC adapter
└── fraisier-adapter-peewee/              # Python IPC adapter
```

### 9.2 Dependencies

- `tokio`, `async-trait`, `serde`, `toml`, `serde_yaml`, `tracing`, `tracing-subscriber`
- `opentelemetry`, `opentelemetry-otlp` — first-class observability
- `reqwest` (rustls)
- `russh` — SSH for bootstrap and multi-host dispatch
- `clap` v4
- `git2`
- `sqlx` (postgres + sqlite, no compile-time queries) — db ops + sqlite state backend
- `jsonrpsee` or hand-rolled JSON-RPC over stdio for IPC adapters
- `nix` for socket activation

### 9.3 Error model

Typed errors per adapter axis. `SagaError` is the engine-level type; `DeployError` wraps it with deploy-specific context. IPC adapter errors carry the adapter name, JSON-RPC error code, and stderr capture. Errors emitted as both events and OTel span errors.

### 9.4 Concurrency

Single deploy execution is sequential by design at the saga level. Multi-host fan-out happens at the `RolloutLoop` state; within a batch, host operations are parallel (`futures::join_all`). Cross-deploy serialization via the StateStore's per-fraise lock.

---

## 10. Success criteria

### 10.1 Functional acceptance (v1.0.0-beta.1)

- [ ] Every row of §3.4 marked ✅ holds.
- [x] Single-host deploy of fraiseql v2 through the Rust binary works three consecutive times. *(Met 2026-06-05 — real `fraiseql-server` v2.4.0 + `ecommerce_api` Confiture schema, 3× on a real Hetzner pid-1 host; see §10.3.)*
- [ ] Multi-host deploy against a 3-host fixture cluster (rolling strategy) succeeds, including forced-failure rollback at each phase.
- [ ] ~~At least one Python `fraises.yaml` from Python fraisier's test corpus deploys identically through the Rust binary~~ — withdrawn with the `fraises.yaml` compat layer (2026-06-02). The IPC-adapter coverage it implied is met by the reference adapters below.
- [ ] specql-platform embeds `fraisier-saga` and deploys a fixture app via library call.
- [x] IPC adapter protocol exercised against an external adapter over a real subprocess (the `fraisier-adapter-sqlx` reference, Cycle 2.2). *(Revised 2026-06-02 from "at least three (sqlx + Python + community)" — the Python adapters were withdrawn; a non-Rust adapter remains the open way to prove the protocol is genuinely language-agnostic, deferred until a real non-Rust consumer exists.)*
- [ ] OTel traces visible in a Jaeger or Tempo instance for every deploy.

### 10.2 Quality acceptance

- [ ] `cargo clippy --all-targets --all-features -- -D warnings` passes with `pedantic` denied per global standards.
- [ ] `unsafe_code = "forbid"` at workspace level.
- [ ] Integration test suite runs in <8 minutes locally (allowance bumped for multi-host fixtures).
- [ ] Public API of `fraisier-saga` and `fraisier-core` documented; all public items have rustdoc.
- [ ] JSON-RPC adapter protocol documented with a reference implementation guide.

### 10.3 Validation checkpoint (end of week 4)

Gate for promoting `fraisier-core` → `fraisier v1.0.0-beta.1`:

- [x] Fraiseql v2 deploys successfully three consecutive times in production (single-host). **Met 2026-06-05:** fraisier's deploy saga deployed the real `fraiseql-server` v2.4.0 against the confiture-migrated `ecommerce_api` schema 3× consecutively, each committed + `/health` 200, on a real Hetzner debian-13 **pid-1 systemd** host over the network (and locally). See `.phases/part-b-fraiseql-canonical-migrations.md`.
- [ ] Multi-host fixture cluster deploys successfully three consecutive times (experimental flag set).
- [ ] specql-platform library embedding works.
- [ ] ~~At least one Python `fraises.yaml` deploys identically via external IPC adapter~~ — withdrawn (2026-06-02); the IPC protocol is validated by the reference sqlx adapter instead.
- [ ] Adapter trait shape (in-process + IPC contract) unchanged since week 1 freeze.

If all five hold, rename + publish. If any fail, name the gap, defer the rename, continue under `fraisier-core` until the bar is met.

---

## 11. Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| 4-week timeline + reshape is unrealistic | **High** | **High** | Scope reduction (Python adapters → external packages) offsets the architectural work. Phase-by-phase milestones. Explicit deferral list (blue-green, Postgres state, D-Bus) protects the timeline. |
| IPC adapter protocol design wrong on first try | Medium | High | Protocol designed in Phase 1 with two reference adapters (one Rust, one Python) by end of week 2. The contract that supports both is more likely to support a third. |
| Multi-host rollback edge cases | High | Medium | Forced-failure tests at every state. `PartialRollback` state surfaces unrecoverable cases explicitly rather than pretending. |
| Load balancer integration is messy per-environment | Medium | Medium | Ship only the nginx reference adapter; trait public so users add HAProxy/Caddy/cloud. Don't try to be exhaustive. |
| SSH multi-host dispatch reuses Phase 3 bootstrap logic — coupling | Low | Medium | Extract `RemoteHost` abstraction in Phase 3 explicitly so multi-host in Phase 4 inherits it cleanly. |
| State backend protocol (filesystem + sqlite) doesn't generalize to Postgres later | Low | High | Design the trait against the hardest case (Postgres + N writers) on day 1, even though only filesystem + sqlite ship. |
| OTel adds dependency weight to specql-platform | Low | Low | Feature-gate OTel exports behind `otel` feature flag; default off for the library, on for the CLI. |
| Forward-compat migration lint requires Confiture changes | Medium | Medium | Spike on day 1 against current Confiture; if Confiture needs changes, scope them as a separate PR in the Confiture repo, not this project. |
| Validation checkpoint fails | Medium | Medium | Deferred-rename strategy contains the cost. |
| Scope creep | High | High | §3.4 matrix is the contract. New ideas → `FOLLOW_UPS.md`. |

---

## 12. Timeline (4 weeks)

| Week | Phase | Deliverable |
|---|---|---|
| Week 1 | Phase 1 — Foundation | Workspace with `fraisier-saga` + `fraisier-core` + state store trait + single-host state machine + adapter axis traits + IPC protocol design + reference Confiture & command adapters + OTel wiring. **Demo**: single-host deploy of fraiseql v2. |
| Week 2 | Phase 2 — IPC ecosystem + compat | Reference Rust IPC adapter (`fraisier-adapter-sqlx`) + one reference Python IPC adapter scaffold (`fraisier-adapter-django`) + `fraises.yaml` compat parser. Service/health/LB adapters. **Demo**: Django via fraises.yaml + external IPC adapter package. |
| Week 3 | Phase 3 — Infrastructure | Webhook server (socket-activated + standalone) + SSH bootstrap + scaffold + sync + providers. SSH abstraction shared with Phase 4 multi-host dispatch. **Demo**: webhook-triggered deploy on freshly-bootstrapped VM. |
| Week 4 | Phase 4 — Multi-host + ops + polish | Multi-host plan (rolling strategy) + nginx LB adapter + ship + db ops + status + specql-platform embedding + validation checkpoint + release. **Demo**: rolling deploy across 3 hosts via webhook, end-to-end. |
| Parallel (days 18–20) | Phase 5 — Finalize | Archaeology removal, security audit, docs, crates.io publication. |

---

## 13. Open questions

1. **Naming** — closed. `fraisier-core` during dev, renamed to `fraisier` at v1.0.0-beta.1.
2. **Artifact sources** — `release`, `git`, `local` all ship in v1.0.0-beta.1.
3. **Systemd mechanism** — shell out in v1.0; D-Bus is v1.1.
4. **YAML compat surface** — bounded by Python fraisier's test corpus.
5. **Python fraisier sunset** — deferred to post-v1.0.0 GA.
6. **Bootstrap fallback** — if week 3 incomplete, Python-subprocess wrapper acceptable for beta.
7. **Webhook transport** — socket activation + standalone HTTP both ship.
8. **State backend protocol** — designed against Postgres on day 1 even though only filesystem + sqlite ship in v1.0.0-beta.1.
9. **IPC adapter packaging** — Rust adapters published to crates.io as binaries (`cargo install`); Python adapters published to PyPI; Node.js adapters to npm. No central registry; PATH-based discovery is the contract.
10. **Blue-green forward-compat lint** — requires close coordination with Confiture. If Confiture needs new lint hooks, scope that as a separate work item before week 4.

---

## 14. Decision log

| Date | Decision | Rationale |
|---|---|---|
| 2026-05-31 | Build as new Rust crate, not literal port. | Clean adapter contract; avoids legacy bugs. |
| 2026-05-31 | Three-axis adapter model + 5 axes total (artifact, migration, service, health, LB). | Decouples concerns that Python fraisier conflated. |
| 2026-05-31 | Adopt v1.x version line; Python v0.x frozen on PyPI. | Major version bump correctly signals the break. |
| 2026-05-31 | Defer `fraisier-core` → `fraisier` rename to v1.0.0-beta.1 release. | Protects v1.x semver from failing checkpoint. |
| 2026-05-31 | **Two-crate architecture: `fraisier-saga` (engine) + `fraisier-core` (deploy).** | Engine reusable beyond deploy; specql-platform embeds the engine specifically. Audience expansion at near-zero cost. |
| 2026-05-31 | **Pluggable state backend (`StateStore` trait); filesystem + sqlite ship.** | Unlocks multi-host without retrofitting. |
| 2026-05-31 | **IPC adapter protocol (JSON-RPC over stdio) for migration adapters.** | Any-language ecosystem extensibility; community-contributable; process crash isolation. |
| 2026-05-31 | **Drop Python framework adapters from v1.0 core; ship them as external IPC packages.** | Scope reduction offsets the architectural work; cleanly separates fraisier from its legacy adapters. |
| 2026-05-31 | **Multi-host as first-class v1.0 feature, marked `experimental`.** | Positioning moat (atomic multi-host migrations) at moderate effort given the other pivots are committed. |
| 2026-05-31 | **OpenTelemetry as default observability surface.** | Free if added day 1; meaningful UX uplift in ops-mature environments. |
| 2026-05-31 | Confiture migration adapter stays in-process, not IPC. | Intimate integration enables forward-compat lint and blue-green prep — unique competitive moat. |
| 2026-05-31 | Blue-green deferred to v1.0.0 GA, designed-in via the multi-host plan. | LB integration surface is real work; ship `rolling` first, prove it, then blue-green. |
| 2026-05-31 | Postgres state backend deferred to v1.1. | Trait shape supports it; concrete implementation can wait. |
| 2026-06-02 | **Drop `fraises.yaml` runtime compatibility (G6 / §7.2 / §10.1).** | The project has one Python fraisier user, who will hand-convert to `fraisier.toml`. A runtime parser plus a synthesized fixture corpus would prove only that it parses configs we invented — the real Python corpus is not in this repo. The native format is the single source of truth; the `framework → adapter` mapping survives as direct native config. Withdraws Cycles 2.8–2.11. |
