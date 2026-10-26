# §10.3 criterion 1 — fraiseql v2 production deploy proof

Reproducible artifacts for **PRD §10.3 criterion 1**: *fraiseql v2 deploys
successfully three consecutive times in production (single-host).*

**Re-proved 2026-10-26 on FraiseQL 2.18.0**, both locally and on a throwaway
Hetzner debian-13 pid-1 host, against PostgreSQL 18. 2.18 refuses to boot on an
older PostgreSQL, so the host script's container is `postgres:18`. The version is
pinned in
[`tools/fraiseql-version`](../../tools/fraiseql-version). Every script reads that
file, and refuses a `fraiseql-server` whose `--version` does not match it.

**First passed 2026-06-05 on v2.4.0**, locally (`systemctl --user`, zero spend) and on a real
**Hetzner debian-13 pid-1 systemd** host over the network (auto-deleted, no leak).
fraisier's own deploy saga (`preflight → fetch → migrate → activate → restart →
health → verify`) deployed the **real `fraiseql-server` v2.4.0** against the
**confiture-migrated `ecommerce_api` schema**, 3× consecutively — each `outcome:
committed` + `/health` 200.

The artifacts in this directory (the migration set, configs, schema.compiled.json,
and harness scripts) reproduce that run end to end — the subject is `ecommerce_api`
with a "rich DB + minimal boot" server schema.

## What's here

| File | Role |
|---|---|
| `ecom-confiture/001_initial_schema.{up,down}.sql` | The reversible Confiture migration set — ecommerce_api's clean 15-table production schema (from `~/code/fraiseql/examples/ecommerce_api/db/migrations/001_initial_schema.sql`) wrapped as a paired-SQL Confiture migration with a real `down`. Validated against confiture 0.22 (up→`001`, verify/preflight exit 0, down rolls back, idempotent re-up). |
| `server.config.toml` | Minimal flat `ServerConfig` for the server (`cors_enabled = false`). It passes the production boot checks; every unit sets `FRAISEQL_ENV=production` explicitly. |
| `schema.compiled.json` | A minimal valid compiled boot schema, compiled by the **pinned** release's `fraiseql-cli compile examples/basic/schema.json`. A schema compiled by another release may not load (2.18.0 refuses the old 2.4.0 one). The server doesn't re-validate it against the DB at boot; the served GraphQL is minimal (a fraiseql concern, not fraisier's). |
| `deploy-local.sh` | The local proof (`systemctl --user`, zero spend). It recreates a scratch database whose name must start with `fraisier_criterion1_` (default `fraisier_criterion1_ecom`). It reaches Postgres through `C1_PGHOST`/`C1_PGPORT`/`C1_PGUSER`/`C1_PGPASSWORD`, passed to `psql` as `PG*` variables and never on argv. |
| `criterion1-host.sh` | Runs on a pid-1 host (system systemd): throwaway Postgres container, unit, artifact server, 3 deploys. |
| `hetzner-criterion1.sh` | Orchestrator: provisions a throwaway Hetzner debian-13 host (glibc ≥ 2.39 for the prebuilt binaries), installs docker + confiture ≥ 0.22, ships the binaries + assets, runs `criterion1-host.sh`, deletes the host on exit. |

## The binaries

The two binaries are not committed. Both scripts take them from the environment:

```sh
# fraiseql-server at the pinned tag (needs the fraiseql workspace at ~/code/fraiseql)
git -C ~/code/fraiseql checkout "$(cat tools/fraiseql-version)"
cargo build --release -p fraiseql-server --features observers   # in ~/code/fraiseql
export FRAISEQL_SERVER=~/code/fraiseql/target/release/fraiseql-server
# fraisier itself (in this repo); FRAISIER_BIN defaults to this path
cargo build --bin fraisier

bash scripts/criterion1/deploy-local.sh         # local proof
bash scripts/criterion1/hetzner-criterion1.sh   # pid-1 proof on a throwaway Hetzner host
```

`hetzner-criterion1.sh` strips both binaries into a temporary directory and ships
them along with this directory's schema, config, migrations and the version pin.

To move the pin: edit `tools/fraiseql-version`, rebuild the server at that tag,
recompile the boot schema with that tag's CLI, and run both proofs:

```sh
~/code/fraiseql/target/release/fraiseql-cli compile \
  ~/code/fraiseql/examples/basic/schema.json -o scripts/criterion1/schema.compiled.json
```

fraiseql also publishes release binaries (`fraiseql-<target>.tar.gz`). As of
v2.18.0, though, the release carries no checksum for them, only for the SBOM, so
the proof builds from the tag instead of downloading an unverifiable binary.

## Key mechanics (so a re-run doesn't re-discover them)

- **Confiture format:** confiture 0.22 ignores a bare `NNN_name.sql`; it needs paired
  `.up.sql/.down.sql` (or Python). DSN via `CONFITURE_DATABASE_URL` + `--no-config`.
- **Artifact: the server is set up by hand, and the "release" is a dummy.** The
  `release` adapter stages the downloaded file + symlink-swaps `active_path`; it does
  **not** extract. So the real `fraiseql-server` is fixed infra (installed by the
  script's unit), and the artifact each deploy fetches is an opaque per-version
  tarball (activation exercised mechanically) — same as the matrix/training field.
  Criterion-1 realness = the real pinned binary + the real ecom schema.
- **`/health` is 200 even when degraded (2.18):** a configured capability that failed
  to start still answers 200, so the http health check proves the server is up, not
  that its capabilities run. See [docs/fraiseql.md](../../docs/fraiseql.md).
- **Restart→health race:** the http health adapter defaults to 3×500 ms and `[health]`
  is `deny_unknown_fields`, so a slow (debug) server start would race. The systemd unit
  carries an `ExecStartPost` that blocks until `/health` 200, so `systemctl restart`
  returns only once the server is serving.
- **glibc:** prebuilt binaries need `GLIBC_2.39`; debian-12 (2.36) is too old — the
  Hetzner host uses **debian-13** (glibc 2.41).

## NOT a tag

Per the locked owner decision, passing criterion 1 takes **no tag**: §10.3 criteria 2
(multi-host, Phase 4) and 3 (specql-platform embedding) are not yet built, and the gate
reads "if all five hold, rename + publish." The tag/rename scope decision returns to the
owner.
