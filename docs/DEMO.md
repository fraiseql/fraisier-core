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

## Observability (OpenTelemetry → Jaeger)

Every saga state transition is a span. OTLP export is behind the `otel` feature on
`fraisier-saga` (off by default for the library, so embedders pay nothing). To see
a deploy as a trace:

```sh
docker run -d --name jaeger -p 16686:16686 -p 4317:4317 jaegertracing/all-in-one
cargo build -p fraisier-saga --features otel        # verified to compile
# point OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317 and run the deploy
# open http://localhost:16686 and find the `fraiseql/production` trace
```

## Owner-run validation checkpoint (real host)

The reproducible tests above cover the engineering. The PRD §10.3 checkpoint —
three consecutive production deploys of fraiseql v2 on a Hetzner-class host, a
forced-failure rollback at each phase, and the trace rendered in Jaeger — is a
manual, infra-bound validation to run before promoting `fraisier-core` →
`fraisier v1.0.0-beta.1`. Steps:

1. Provision the host; install the `fraisier` binary and the `confiture` CLI.
2. Drop the sample `fraisier.toml` (above) with the host's real `active_path`,
   `unit`, health `url`, and `FRAISEQL_DATABASE_URL`.
3. `fraisier deploy … --app-version <v>` three times; confirm `status` after each.
4. Force a failure (bad health endpoint, or a migration with a missing `.down`);
   confirm the rollback restores the prior release and revision.
5. Confirm the trace renders in Jaeger.
