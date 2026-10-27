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

> **Status: pre-release (the `1.0.0` betas).** The engine and adapter contracts are
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

### Prebuilt binaries

Each `fraisier-v*` release carries a binary per target
(`fraisier-<target>.tar.gz`: Linux `x86_64`/`aarch64` gnu with a glibc 2.28 floor,
static `x86_64` musl, and macOS), a CycloneDX SBOM per target
(`fraisier-<target>.cdx.json`), and a `SHA256SUMS` file covering all of them.
Check a download against it before installing:

```sh
sha256sum --check --ignore-missing SHA256SUMS
```

That proves the files are the ones the release lists. It does not prove who
published the release, because the checksums come from the same place as the
binaries. [docs/security.md](docs/security.md#offline-install) covers installing
on a host with no network access.

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

`fraisier deploy --dry-run` resolves and prints the plan — including the
classified schema change-set and what the [risk
policy](docs/schema-risk-policy.md) would decide about it — without executing
anything. It reads the database to do that, so a CI job using it as an offline
config check wants `--skip-preflight`, which restores the pure plan. Every
deploy is recorded in a pluggable state store (filesystem by
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
fraisier health                         # health + the live deployed version
```

`fraisier health` reports each host's health *and* the live deployed version,
read from the committed release ledger (`version` / `revision` in `--json`). The
version comes only from the post-commit ledger, so a deploy that fails before it
commits keeps reporting the previous version — never an in-flight one.

Hosts can be reached by shelling out to `ssh`/`scp` or by launching an IPC
adapter over SSH (OpenSSH `ControlMaster` connection reuse) — the migration's DSN
never crosses the wire.

## Blue-green

Set the strategy to `blue-green` and give fraisier an `lb` adapter. fraisier
brings up a green fleet alongside blue, **refuses to proceed unless the pending
migration is certified forward-compatible for a two-version window** (it consumes
confiture's first-class `window_safe` verdict — confiture ≥ 0.44.0 — as the
always-on half of the [schema risk policy gate](docs/schema-risk-policy.md)),
checks the
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
health            Probe the configured health endpoint on every host; also
                  reports the live deployed version from the release ledger
rollback          Roll back to a prior revision (migrates the database down)
bootstrap         Prepare each host's deploy directories over SSH (or locally)
webhook-server    Run the signed-webhook deploy trigger (socket-activated/standalone)
scheduled         Manage scheduled fraisier runs via systemd timers
self-upgrade      Coordinated management of fraisier's own service
sync              Share the deploy ledger across operators over git refs (experimental)
backup            Back up the database; `backup prune` applies retention,
                  `backup drill` proves the newest archive restores
db                Database lifecycle: migrate, restore, reset, `tviews rebuild`
stack             Deploy or report a stack: subgraphs, router and databases
notify            Send a failed scheduled run's record through [notify].command
doctor            Diagnose the install: config, secrets, toolchain, database
env-check         Report which env vars a subcommand would read, and which are unset
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

A failing check's captured output is shown inline. When it runs long (more than
30 lines) only the **tail** — where pytest/ruff/mypy print their verdict — is
printed, and the full stdout+stderr is written to a `0o600` log under
`$XDG_DATA_HOME/fraisier/logs/` (falling back to `~/.local/share/...`) whose path
is named in the report, so nothing is lost. Small failures print whole. The
`--json` report always carries the full output regardless.

### Deploying FraiseQL

fraisier is proven against FraiseQL **2.18.0**, pinned in
[`tools/fraiseql-version`](tools/fraiseql-version). On 2.18, `/health` answers 200
even when a configured capability failed to start. See
[`docs/fraiseql.md`](docs/fraiseql.md) for the supported versions, the
migrate-then-restart order, and what the health check does and does not prove.

### Perf-regression rollback gate

The `command` health adapter turns the saga's post-deploy `health` step into an
arbitrary gate: it runs a configured shell command, passes iff the command exits
`0`, and folds the command's output into the rollback reason. Its headline use is
FraiseQL's `fraiseql perf regression-scan --fail-on-regression --json` — a deploy
that makes a database mutation slower rolls back automatically, naming the
regressed operation (`perf regression: order/UPDATE p50 +42% (12ms→17ms)`) in the
rollback reason and the `[notify].command` webhook. The DSN reaches the scan by
environment, never argv. See [`docs/perf-regression-gate.md`](docs/perf-regression-gate.md).

### Schema risk policy gate

`[policy]` turns the migration adapter's per-change **risk tier** into a deploy
decision: auto-apply the tiers you trust, send the rest to an approval hook, and
**refuse anything nobody classified**.

```toml
[policy]
auto_apply       = ["additive", "reversible"]
require_approval = ["lock_risky", "destructive", "irreversible"]
unclassified     = "deny"                          # never "auto_apply"
approval_command = "scripts/deploy/approve.sh"     # exit 0 approves
```

A deploy that drops a column stops at `preflight`, names the object, and waits
for the hook to say yes — before anything is staged, migrated or restarted.
Configuring the section is the opt-in: with no `[policy]`, deploys behave exactly
as they did. There is no `--force`; approval arrives only through the hook, and a
hook that fails, times out, or cannot be spawned is a **refusal**. Blue-green's
window-safety rule applies with or without the section.

With the confiture adapter this needs **confiture ≥ 0.44.0** — the same floor as
the `window_safe` verdict above, because both rest on the same classifier — and
**confiture ≥ 1.35.0** for the gate to judge only the migrations a deploy will
apply. Below 1.35.0 it judges applied ones too, so one approved `DROP COLUMN`
refuses every later deploy.
Below it fraisier withholds the capability, so a `[policy]` section classifies
nothing and refuses everything; `fraisier doctor` reports the installed version
against the floor. One case survives the upgrade by design: an
`ALTER COLUMN … TYPE` is unclassified — confiture will not guess widening from
narrowing without schema facts fraisier does not ask it for — so a type change
is refused until it is approved or split.

`fraisier deploy --dry-run` previews all of it — the change-set, worst-first,
and the verdict the gate would reach — without ever calling the hook. Add
`--fail-on-block` to turn a would-be block into a nonzero exit for CI. A plan
that cannot see the schema says so (*"Risk is unknown, not zero"*) instead of
printing as though it were clean. See
[`docs/schema-risk-policy.md`](docs/schema-risk-policy.md).

## Backups, retention and drills

With `[backup].dir` set, `fraisier backup` writes a new timestamped archive on
every run and renames it into place only after `pg_restore --list` reads it
back. `[[backup.retain]]` policies decide what `fraisier backup prune` removes:
a dry run unless `--apply`, and never below the policy's floor.
`fraisier backup drill` restores the newest archive into a throwaway database,
migrates it forward, checks it, and drops it, because an archive that has never
been restored is not yet a backup.

`fraisier scheduled install` turns backups, prunes and drills into systemd
timers. Each failed run notifies once, through `[notify].command`, with
credentials stripped from the reason. See [`docs/backups.md`](docs/backups.md).

## Database readiness: PostgreSQL 18 and pg_tviews

A deployable can declare what its database server must offer:

```toml
[database.requires]
postgres_major = 18   # PostgreSQL 18.x or later
pg_tviews = true      # preloaded, installed, and on read contract 2
```

Every deploy checks this first in preflight, before any mutation, and
`fraisier doctor` reports it. With `pg_tviews = true`, a TVIEW that is empty
while its backing view has rows refuses the deploy (`[database.tviews]
on_empty`), and `fraisier db tviews rebuild` repopulates it. An optional
`[database].admin_url_env` names a superuser identity used only to restore a
pg_tviews archive (the restore rehearsal, drills, `db restore`) and to rebuild
TVIEWs, never by a webhook-triggered deploy.
fraisier never installs PostgreSQL or an extension;
[`scripts/provision-pg18-tviews.sh`](scripts/provision-pg18-tviews.sh) gets a
Debian host ready. See
[`docs/database-requirements.md`](docs/database-requirements.md).

## Stacks

A stack is one environment's federated application: the subgraphs, the router
in front of them, and their databases. A `stack.<env>.toml` references each
member's own `fraisier.toml`. `fraisier stack deploy` deploys the members in
dependency order and stops at the first failure; `fraisier stack status`
reports them as one. With `[stack].router`, each subgraph's live SDL is checked
against the router's candidate release before the router deploys. See
[`docs/stacks.md`](docs/stacks.md).

## Observability

Every saga step is both persisted (via the state store) and exported as an
OpenTelemetry span, so a tracing backend reconstructs exactly what happened —
including which step failed and which compensations ran.

With `[metrics].textfile_dir` set, deploy, backup and backup drill each write a
Prometheus textfile of gauges describing their last run, for node_exporter's
textfile collector. See [`docs/metrics.md`](docs/metrics.md).

A failed deploy, and a failed scheduled backup, prune or drill, runs
`[notify].command` once, with a structured payload whose reason has its
credentials stripped.

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
