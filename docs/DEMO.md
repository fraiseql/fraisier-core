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
# open http://localhost:16686 and find the `fraiseql/checkpoint` trace (service "fraisier")
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

Provisions a throwaway Hetzner host, installs the toolchain, and runs the *same*
scenario in **system** systemd mode (real pid-1 manager, as root) over the network.
The host is always deleted on exit. Requires the `hcloud` CLI and a registered SSH
key; it asks for confirmation before provisioning.

```sh
scripts/checkpoint-hetzner.sh --ssh-key <your-hcloud-key>          # provision → run → delete
scripts/checkpoint-hetzner.sh --ssh-key <key> --keep               # leave the host up
```

### Final production sign-off (operator judgement)

With the host up (`--keep`), run the genuinely production-shaped matrix before
tagging: three consecutive deploys of the **real** fraiseql v2 artifact against a
real Postgres (Confiture, or sqlx/Postgres); a forced failure at **each** saga
phase (migrate / release / health / verify — per-phase rollback is unit-proven in
Cycle 1.8, this is the real-host confirmation); and the `fraiseql/production` trace
rendered in Jaeger (`ssh -L 16686:localhost:16686 root@<ip>`).
