# Phase 1 demo — single-host deploy with atomic rollback

This is the end-of-week-1 gut-check: the adapter axes, the saga engine, the config
parser, and the CLI compose to deploy something real, and a failure rolls the
whole thing back atomically.

## What is verified automatically (reproducible / CI-able)

- **End-to-end deploy + rollback** — `crates/fraisier-cli/src/e2e.rs` runs a full
  single-host deploy against the **real** adapters with local fixtures and no
  external services or root:
  - `ReleaseArtifact` downloads + sha256-verifies + symlink-activates from a local
    HTTP server,
  - `CommandMigration` runs real `sh -c` migration commands,
  - `HttpHealth` probes the local endpoint,
  - only `systemctl` is a fake script (managing real units needs root).

  Two deploys share one state store: the first commits (v1 live, ledger recorded);
  the second is forced to fail its health check after activation, and the saga
  rolls back by re-activating the **prior release from the durable ledger**
  (symlink back to v1) and `down_to`-ing the migration to v1.

- **Real Confiture + Postgres migration** — `crates/fraisier-adapter-confiture`'s
  opt-in round-trip (`roundtrip_up_current_down_against_postgres`) runs `up →
  current → down-to` against a real Postgres through the real `confiture` CLI.
  Verified on the dev box with Confiture 0.20.0:

  ```sh
  createdb fraisier_demo
  FRAISIER_TEST_DATABASE_URL="postgresql:///fraisier_demo?host=/run/postgresql" \
    cargo test -p fraisier-adapter-confiture
  dropdb fraisier_demo
  ```

- **External IPC adapter (sqlx) end-to-end** — `crates/fraisier-cli/tests/e2e_ipc_sqlx.rs`
  drives the **real `fraisier` binary** against a `fraisier.toml` whose
  `[migration].adapter = "sqlx"`, discovering the external
  [`fraisier-adapter-sqlx`](../../fraisier-adapter-sqlx) reference adapter on
  `PATH` and running it over the JSON-RPC IPC protocol — the migration axis lives
  in a *separate process*, the rest of the deploy (artifact, health, service) runs
  in-process exactly as above. Two deploys: v1 commits (migration `0001` applied
  over IPC); v2 applies `0002`, fails its health check, and the saga rolls back —
  re-activating v1's artifact from the ledger and reverting `0002` by driving the
  adapter's `down_to` over the wire. This proves three things the in-process tests
  cannot: a forward deploy calls `up(None)` (the adapter has no `run_to` and
  declines a targeted `up`), the DSN reaches the adapter only as the injected
  `DATABASE_URL` env var resolved from a differently-named source var, and rollback
  is real IPC compensation.

  The sqlx adapter is a separate repo, so build its binary first; the test then
  finds it (or skips with a diagnostic if it is genuinely absent):

  ```sh
  ( cd ../fraisier-adapter-sqlx && cargo build )   # produces the adapter binary
  cargo test -p fraisier-cli --test e2e_ipc_sqlx   # or set FRAISIER_SQLX_ADAPTER_BIN
  ```

  To drive it by hand, put the adapter on `PATH` and deploy normally:

  ```sh
  PATH="$PWD/../fraisier-adapter-sqlx/target/debug:$PATH" \
    fraisier adapter list                          # → sqlx
  PATH="…:$PATH" SQLX_DATABASE_URL="sqlite:///srv/app.db?mode=rwc" \
    fraisier deploy --config fraisier.toml --app-version v1 --state-dir ./state
  ```

## Driving it from the CLI

```sh
fraisier validate-config --config fraisier.toml          # parse + validate
fraisier deploy --config fraisier.toml --app-version 2.0.0 --dry-run   # resolve the plan
fraisier deploy --config fraisier.toml --app-version 2.0.0 \
    --state-dir /var/lib/fraisier/state                  # execute
fraisier status --config fraisier.toml --state-dir /var/lib/fraisier/state
```

`--json` works on every command.

### Sample `fraisier.toml` (single-host fraiseql v2)

```toml
[deploy]
name = "fraiseql"
environment = "production"

[artifact]
source = "release"
release_url = "https://github.com/.../fraiseql-{version}-musl.tar.gz"
checksum_url = "https://github.com/.../fraiseql-{version}-musl.tar.gz.sha256"
active_path = "/var/lib/fraiseql/current"     # the symlink swapped on activate
staging_dir = "/var/lib/fraiseql/staging"

[migration]
adapter = "confiture"
database_url_env = "FRAISEQL_DATABASE_URL"    # the *source* env var; never the DSN itself
migrations_path = "./migrations"
forward_compatible_lint = true

[service]
adapter = "systemd"
unit = "fraiseql.service"

[health]
adapter = "http"
url = "http://127.0.0.1:8080/health"
expected_status = 200
```

The deploy reads the DSN from the env var named by `database_url_env` — set
`FRAISEQL_DATABASE_URL=postgres://…` in the deploy environment; it is never
written to config, argv, or JSON (Decision 5).

To run migrations through the external sqlx IPC adapter instead of in-process
Confiture, swap the `[migration]` block — everything else is unchanged, and the
adapter is discovered on `PATH` as `fraisier-adapter-sqlx`:

```toml
[migration]
adapter = "sqlx"                              # spawned over IPC, found on PATH
database_url_env = "FRAISEQL_DATABASE_URL"    # resolved → injected as DATABASE_URL
migrations_path = "./migrations"
```

## Observability (OpenTelemetry → Jaeger)

Every saga state transition is a span. OTLP export is behind the `otel` feature
(off by default, so the stock binary and library embedders pay nothing). Build the
CLI with it, then point the standard `OTEL_EXPORTER_OTLP_ENDPOINT` at a collector;
the CLI installs the exporter when that variable (or `FRAISIER_OTEL`) is set.
Transport is OTLP over **HTTP/protobuf**, so the endpoint is the collector's
**4318** port (not the 4317 gRPC port), and the `/v1/traces` path is appended for
you:

```sh
docker run -d --name jaeger -e COLLECTOR_OTLP_ENABLED=true \
  -p 16686:16686 -p 4318:4318 jaegertracing/all-in-one
cargo build --features otel --bin fraisier
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 \
  fraisier deploy --config fraisier.toml --app-version 2.0.0 --state-dir ./state
```

Then inspect the trace — open `http://localhost:16686`, or on a **headless host**
render it as text straight from Jaeger's API (no browser):

```sh
scripts/show-trace.sh                          # one tree per deploy, rollback marked
scripts/show-trace.sh --jaeger http://host:16686 --service fraisier
```

## §10.3 validation checkpoint (real systemd / real host)

The reproducible tests above cover the in-process engineering. The PRD §10.3 gate
validates the parts the tests fake — the real `systemctl`, real symlink
activation, and real OTLP export — and is what stands between here and promoting
`fraisier-core` → `fraisier v1.0.0-beta.1` (and the `v1.0.0-alpha.1-week2` tag). It
is split into two runnable scripts so cost stops blocking it:

### (a) Local — `scripts/checkpoint-local.sh` (zero spend, no root)

Runs a real deploy against your own user systemd manager: real
`systemctl --user restart`/`is-active`, real filesystem symlink activation, the
sqlx adapter over real IPC, a forced-failure rollback, and real OTLP→Jaeger export
(local container). v1 commits; v2 ships an unhealthy release and rolls back, with
the app's health a function of the *currently symlinked + restarted* release, so a
pass proves the symlink swap and the restart are both load-bearing.

```sh
scripts/checkpoint-local.sh           # builds both binaries, runs Jaeger, tears down
KEEP_JAEGER=1 scripts/checkpoint-local.sh   # leave the trace up to inspect
```

It validates the adapter logic and the observability pipeline with no spend. It
does **not** cover a genuinely remote host, the network path, or a system (pid-1)
systemd manager — that is (b).

### (b) Remote — `scripts/checkpoint-hetzner.sh` (a few cents, auto-teardown)

Provisions a throwaway Hetzner host, installs the toolchain, and runs a checkpoint
in **system** systemd mode (real pid-1 manager, as root) over the network. The host
is always deleted on exit. Requires the `hcloud` CLI and a registered SSH key; it
asks for confirmation before provisioning (`--yes` to skip).

```sh
scripts/checkpoint-hetzner.sh --ssh-key <your-hcloud-key>          # the two-deploy scenario
scripts/checkpoint-hetzner.sh --ssh-key <key> --matrix             # the production matrix, Part A
scripts/checkpoint-hetzner.sh --ssh-key <key> --training           # the Confiture-on-Postgres training field
scripts/checkpoint-hetzner.sh --ssh-key <key> --keep               # leave the host up
```

Default scenario is the two-deploy `checkpoint-local.sh` (committed + rolled_back).
`--matrix` instead runs the full production matrix **Part A** (per-phase forced-
failure rollback + three consecutive deploys) on the remote pid-1 manager — this is
what exercises the `activate`/`restart` split and the `reset-failed`-before-restart
fix under a *real* rate-limited systemd. The remote matrix store is the reference
sqlx adapter (SQLite); criterion 1 against real Postgres is Part B below (`--keep`,
then drive your real config).

### Multi-host IPC — `scripts/checkpoint-hetzner-multihost.sh` (§6.4 GA gate)

Runs the proven 2a/2b/2c multi-host rollout across **real Hetzner VMs over the
real network** with the **IPC-over-SSH artifact** — the environmental delta the
podman fixture (`checkpoint-multihost.sh`) cannot cover. Lean 2-VM topology: a
**local** orchestrator + N (default 2) **rocky-9** app hosts (Rocky 9's glibc
matches the ubi9 builder, so the locally-built IPC adapter runs unchanged) + a
real nginx LB in a local podman container routing real HTTP to the hosts' public
IPs. Migrates once on the orchestrator; each host runs the
`fraisier-adapter-release` IPC adapter over SSH. Hosts are always deleted on exit.

```sh
scripts/checkpoint-hetzner-multihost.sh --ssh-key <key> --ssh-identity <file>
```

It asserts: (2a) three consecutive deploys commit (migrate once, all hosts on the
version, LB routes to the fleet); (2b) a sick build rolls the **whole fleet** back
+ reverts the migration; (2c) a crash build rolls the fleet back. This is one of
the **two GA-blocking gates** (the other is the blue-green checkpoint above).

### Final production sign-off — `scripts/checkpoint-matrix.sh` (operator judgement)

The production matrix is scripted and self-asserting. It runs against a real
engine (real systemd, real symlink activation, the migration adapter over IPC,
real OTLP→Jaeger) in two parts:

- **Part A** (always; zero-spend locally) — three consecutive successful deploys
  and a forced, deterministic failure at each *forceable* saga phase, each
  asserted to roll back cleanly to the healthy baseline:
  - `migrate` — a deploy carrying invalid SQL → `down_to(previous)`;
  - `release` — a deploy whose app won't start (surfaces at the `restart` step;
    the saga compensates the completed `activate` step, re-activating the prior);
  - `health` — a deploy that starts but reports unhealthy.
  `verify` is asserted to *pass*; a verify-phase **failure** is not inducible by
  natural config (it is a post-migration success report — sqlx reads
  `_sqlx_migrations.success`, confiture reflects its `failed_count`), so its
  rollback is left to the unit tests (`fraisier-saga/tests/rollback.rs`,
  `single_host.rs`) rather than faked on the host.
- **Part B** (`--real-config`) — PRD §10.3 criterion 1: N consecutive deploys of
  **your** real `fraisier.toml` (real fraiseql v2 artifact + real Postgres via your
  configured migration adapter — Confiture on the fraiseql production path; the
  reference sqlx adapter is SQLite-only), each asserted to commit. Export the DSN
  env var your `[migration].database_url_env` names; the script drives your config
  as-is.

```sh
# locally (user systemd, sqlx/SQLite, no spend): exercises Part A end-to-end
scripts/checkpoint-matrix.sh

# on the --keep'd Hetzner host (pid-1 systemd) + your real criterion-1 deploys
export FRAISEQL_DATABASE_URL=postgres://…          # whatever your config names
scripts/checkpoint-matrix.sh --systemd system \
  --real-config /path/to/fraiseql.toml \
  --real-version v2.0.0 --real-version v2.0.0 --real-version v2.0.0
```

The deploy phases are `preflight → fetch → migrate → activate → restart → health
→ verify` (`release` is split into `activate` + `restart` so a failed restart
re-activates the prior release rather than stranding `current` — a gap this matrix
surfaced). Render the trace headless with `scripts/show-trace.sh` on the host (or
tunnel the API: `ssh -L 16686:localhost:16686 root@<ip>` and run it locally).

### Training field — `scripts/checkpoint-training.sh` (Confiture-on-Postgres proof)

The matrix above drives the **sqlx** adapter over IPC against **SQLite**. The
training field drives the **in-process Confiture adapter against a real Postgres**,
inside the real deploy saga — the path no other checkpoint covers. It deploys the
tiny DB-backed `examples/training-field` app (whose `/health` is 200 only when the
Confiture-migrated `notes` table is queryable) and asserts three consecutive
commits plus a forced failure at each forceable phase (migrate / restart / health),
each rolling back to the healthy baseline. Postgres and Jaeger run as throwaway
containers, so it is self-contained in either systemd mode.

```sh
# locally (user systemd + containerised Postgres, zero spend)
scripts/checkpoint-training.sh --systemd user

# genuinely-remote, pid-1 systemd (provisions a host, installs Confiture, runs it)
scripts/checkpoint-hetzner.sh --training --ssh-key <key>
```

This proves the Confiture-on-Postgres pipeline and de-risks Part B; it is **not**
§10.3 criterion 1 (which names fraiseql v2). See `examples/training-field/README.md`.

### Blue-green — `scripts/checkpoint-blue-green.sh` (window-safety + budget gates)

Drives the real `fraisier deploy --strategy blue-green` through its **preflight
gates** against **real confiture** and a **real Postgres**. The gate consumes
confiture's first-class **`window_safe`** verdict (confiture#154 Phase 3) — one
typed boolean, no code pattern-matching. The script probes for it:

- **with `window_safe` (Phase 3):** a `DROP COLUMN` migration → `window_safe =
  false` → **refused** before any instance/traffic change; an `ADD COLUMN`
  (nullable) **expand** → `window_safe = true` → clears the gate; and with green's
  pool larger than the shared DB's headroom the pre-swap **connection-budget**
  probe (a real `psql` query) refuses before the swap;
- **without it (older confiture):** any migration → *no verdict* → **refused**
  (the fail-safe — an un-upgraded confiture can't certify the window). The script
  asserts this and notes the full gates need Phase 3.

```sh
scripts/checkpoint-blue-green.sh                 # throwaway Postgres (cached *-alpine), zero spend
scripts/checkpoint-blue-green.sh --db-dsn <url>  # query an existing Postgres for the budget gate
```

These are three of the four phase-07 §4 gates (window-safety refuse/allow +
connection budget). The two **traffic-tier** gates run against a **real nginx** in
the companion fixture below.

### Blue-green traffic — `scripts/checkpoint-blue-green-traffic.sh` (real nginx)

The traffic-tier half of §7.5: drives the real `fraisier deploy --strategy
blue-green` through the full flow against a **real nginx** routing real HTTP
between a blue and a green fleet (two `python3` HTTP servers as user systemd
units), with a throwaway Postgres + confiture for the migrate/window-safety step.
It asserts what nginx actually serves:

- **healthy green → traffic swaps blue→green** (nginx serves "green"), the deploy
  commits, blue is reaped;
- **sick green → the pre-swap health gate holds**: traffic never moves (nginx
  still serves "blue"), green is decommissioned;
- **green degrades during the hold window → traffic swaps back** to still-hot blue
  (nginx serves "blue" again).

```sh
scripts/checkpoint-blue-green-traffic.sh          # rootless podman + user systemd, zero spend
```

The include dir is bind-mounted into the nginx container at its *same absolute
path*, so fraisier's absolute swap symlink resolves identically inside. Together
with `checkpoint-blue-green.sh`, all four phase-07 §4 gates are proven end-to-end
against real infra. (This fixture needs a confiture that emits `window_safe`
(confiture#154 Phase 3) — the swap requires the window-safety gate to *pass*; it
dies early with a clear message otherwise. The traffic-tier gates are also proven
hermetically in `fraisier-core::blue_green`.)
