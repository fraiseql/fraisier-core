# fraisier

**The deploy tool that gets atomic migrations right — across one host or many.**

fraisier is a deploy-orchestration engine. Every deploy is a *saga*: an ordered
list of compensable steps that either commits as a whole or rolls back in
reverse. When a health check fails after you've activated a new release, fraisier
re-activates the previous release from a durable ledger and migrates the database
back down — automatically, in the right order, on every host. Rollback is the
headline feature, and the one that actually works across N hosts.

It ships three ways:

- a single static-binary **CLI** (`fraisier`),
- an embeddable **Rust library** (`fraisier-saga` — drive your own compensating
  workflows over the same engine),
- an **adapter protocol** you can implement in any language over JSON-RPC.

> **Status: pre-release (`1.0.0-beta.1`).** The engine and adapter contracts are
> frozen for v1.0 and exercised end-to-end against real infrastructure (real
> Postgres + confiture, real systemd, real nginx, real multi-host over SSH).

---

## Why it exists

A deploy that runs a schema migration and *then* fails its health check leaves
you in the worst place: new schema, old (or no) running code. Most tools stop at
"restart the service." fraisier treats the migration, the artifact swap, the
service restart, the health gate, and the load-balancer drain as one transaction
with a defined inverse for each step. If step N fails, steps N-1 … 1 are
compensated in reverse. Across a fleet, a failed host rolls the *whole* fleet
back to the prior release and reverts the migration.

## The model: five axes, one saga

A deploy is composed from five **adapter axes**, each a small trait:

| Axis | Responsibility | Built-in adapters |
|---|---|---|
| `artifact` | fetch + verify + atomically activate a release | release tarball, git, local path, host-pull, IPC |
| `migration` | apply / roll back / preflight schema changes | confiture, command, IPC (e.g. sqlx) |
| `service`   | start / stop / restart the app | systemd, rc, docker-compose |
| `health`    | probe that the new release is live | http, command |
| `lb`        | drain / reattach / swap traffic | nginx |

Each adapter runs either **in-process** (a Rust trait impl) or **out-of-process**
over a JSON-RPC-over-stdio protocol — the same trait, just a different transport,
so an adapter can be written in any language. Adapters never receive secret
*values*: they get the logical name of a secret and resolve it from the
environment themselves.

The single-host deploy runs:

```
preflight → fetch → migrate → activate → restart → health → verify
```

with each step's compensation registered as it completes.

## Quickstart (single host)

```sh
# 0. install (binary: `fraisier`)
cargo install fraisier

# 1. write a starter config
fraisier init

# 2. edit fraisier.toml, then deploy
CHECKOUT_DATABASE_URL=postgres://… fraisier deploy --app-version 1.4.2
```

A minimal `fraisier.toml`:

```toml
[deploy]
name = "checkout"
environment = "production"

[artifact]
source = "release"
release_url  = "https://releases.example.com/checkout-{version}.tar.gz"
checksum_url = "https://releases.example.com/checkout-{version}.tar.gz.sha256"

[migration]
adapter = "confiture"
database_url_env = "CHECKOUT_DATABASE_URL"   # the name, never the value

[service]
adapter = "systemd"
unit = "checkout.service"

[health]
adapter = "http"
url = "http://127.0.0.1:8080/healthz"
```

`fraisier deploy --dry-run` resolves and prints the plan without touching
anything. Every deploy is recorded in a pluggable state store (filesystem by
default; sqlite and in-memory backends ship too), and `fraisier status` /
`fraisier list` / `fraisier rollback` operate on that ledger.

## Multi-host rolling deploy

Add a `[hosts]` inventory and an `[ssh]` block. The migration runs **once** on
the orchestrator; the artifact, service, and health steps run **per host**, and
the load balancer drains each host before and reattaches it after. A failed host
rolls the whole fleet back in reverse order.

```toml
[hosts]
strategy = "rolling"        # or "all-at-once"
rolling_batch_size = 1
inventory = [
  { name = "web-1", address = "web1.internal" },
  { name = "web-2", address = "web2.internal" },
  { name = "web-3", address = "web3.internal" },
]

[ssh]
user = "deploy"
# Host-key checking is left to your ssh config; fraisier runs with
# BatchMode=yes and never disables StrictHostKeyChecking.
options = ["ConnectTimeout=10"]
```

```sh
fraisier deploy --app-version 1.4.2     # rolls across the fleet
fraisier status --per-host              # the live active release on each host
```

Hosts can be reached by shelling out to `ssh`/`scp` or by launching an IPC
adapter over SSH (OpenSSH `ControlMaster` connection reuse) — the migration's DSN
never crosses the wire.

## Blue-green

Set the strategy to `blue-green` and give fraisier an `lb` adapter. fraisier
brings up a green fleet alongside blue, **refuses to proceed unless the pending
migration is certified forward-compatible for a two-version window** (it consumes
confiture's first-class `window_safe` verdict — confiture ≥ 0.23.0), checks the
connection budget against the shared database, swaps traffic at the load
balancer, holds, and — if green degrades during the hold — swaps **back** to the
still-hot blue fleet. Rollback is a traffic swap-back, not a database rollback.

```toml
[deploy]
name = "checkout"
environment = "production"
strategy = "blue-green"

[lb]
adapter = "nginx"
include_dir = "/etc/nginx/fraisier"
```

## Embedding the engine

`fraisier-saga` is the engine as a library. Author your own steps over the public
`Saga` / `Step` / `StepContext` API and get the same atomic-rollback semantics:

```rust
use fraisier_saga::saga::Saga;

let outcome = Saga::new(store, fraise, env)
    .with_step(Box::new(my_step))
    .run()
    .await?;
```

The runnable embedding guide lives at
[`crates/fraisier-saga/docs/embedding-fraisier-saga.md`](crates/fraisier-saga/docs/embedding-fraisier-saga.md)
and is compiled as a doctest so it cannot rot.

## Command reference

```
init              Write a starter fraisier.toml into the current project
validate-config   Parse, expand the SpecQL preset, and validate a fraisier.toml
deploy            Run a deploy (single-host or fleet; --dry-run resolves the plan)
list              List every deploy recorded in the state store
status            Show the recorded saga state and release ledger (--per-host)
health            Probe the configured health endpoint on every host
rollback          Roll back to a prior revision (migrates the database down)
bootstrap         Prepare each host's deploy directories over SSH (or locally)
webhook-server    Run the signed-webhook deploy trigger (socket-activated/standalone)
scheduled         Manage scheduled fraisier runs via systemd timers
self-upgrade      Coordinated management of fraisier's own service
sync              Share the deploy ledger across operators over git refs (experimental)
backup / db       Database lifecycle: backup, migrate, restore, reset
providers         List the adapters available to this binary, per axis
provider-test     Probe one provider (IPC handshake or built-in presence)
check             Run the project's [[checks]] with cross-check parallelism (-j)
version / ship    Show/bump the version; ship = checks → bump → commit → push → deploy
scaffold[-install] Generate/install the systemd/socket/nginx/CI deploy files
```

### Project checks

A project can declare `[[checks]]` in its `fraisier.toml` — named shell commands
(lint / test / typecheck) that `fraisier check` runs with cross-check parallelism
(`-j`, default auto). The same list gates a release: `fraisier ship` runs the
checks first and refuses to bump the version if any fails (`--no-check` skips the
gate). Intra-check parallelism (e.g. `pytest -n auto`) lives in the command
string, so the runner needs no per-framework knowledge.

```toml
[[checks]]
name = "lint"
command = "cargo clippy --all-targets -- -D warnings"

[[checks]]
name = "test"
command = "cargo test --workspace"
```

### Perf-regression rollback gate

The `command` health adapter turns the saga's post-deploy `health` step into an
arbitrary gate: it runs a configured shell command, passes iff the command exits
`0`, and folds the command's output into the rollback reason. Its headline use is
FraiseQL's `fraiseql perf regression-scan --fail-on-regression --json` — a deploy
that makes a database mutation slower rolls back automatically, naming the
regressed operation (`perf regression: order/UPDATE p50 +42% (12ms→17ms)`) in the
rollback reason and the `[schedule].notify` webhook. The DSN reaches the scan by
environment, never argv. See [`docs/perf-regression-gate.md`](docs/perf-regression-gate.md).

## Observability

Every saga step is both persisted (via the state store) and exported as an
OpenTelemetry span, so a tracing backend reconstructs exactly what happened —
including which step failed and which compensations ran.

## Security model

- Webhook triggers are HMAC-SHA256 signed over `"<timestamp>.<body>"`, verified
  in constant time with a replay window.
- Secrets reach adapters by **name**, resolved from the environment; they never
  appear in argv or in the saga ledger.
- Subprocesses are spawned with explicit argv (no shell interpolation); the only
  shell hooks are ones you configure, and they receive their payload on stdin.
- `unsafe` code is forbidden workspace-wide; dependencies are gated by
  `cargo deny`.

## Building & the CI gate

The full gate is defined once, in Rust, as a task runner (`crates/xtask`). The
command you run locally is the command CI runs — there is no second list to drift:

```sh
cargo xtask ci      # fmt + clippy -D warnings + test + release build + deny + shellcheck
```

Individual checks are available too (`cargo xtask fmt | lint | test | deny |
shellcheck`), and `cargo xtask dist` cross-builds the static musl binary via
`cargo-zigbuild`. The underlying commands, if you prefer to run them directly:

```sh
cargo build --release
cargo test --workspace --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo deny check
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
