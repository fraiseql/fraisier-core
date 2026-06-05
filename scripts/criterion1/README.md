# §10.3 criterion 1 — fraiseql v2 production deploy proof

Reproducible artifacts for **PRD §10.3 criterion 1**: *fraiseql v2 deploys
successfully three consecutive times in production (single-host).*

**PASSED 2026-06-05**, locally (`systemctl --user`, zero spend) and on a real
**Hetzner debian-13 pid-1 systemd** host over the network (auto-deleted, no leak).
fraisier's own deploy saga (`preflight → fetch → migrate → activate → restart →
health → verify`) deployed the **real `fraiseql-server` v2.4.0** against the
**confiture-migrated `ecommerce_api` schema**, 3× consecutively — each `outcome:
committed` + `/health` 200.

See `.phases/part-b-fraiseql-canonical-migrations.md` for the full investigation
(why the subject is ecommerce_api, why the server schema is "rich DB + minimal boot",
and the framework/app schema findings).

## What's here

| File | Role |
|---|---|
| `ecom-confiture/001_initial_schema.{up,down}.sql` | The reversible Confiture migration set — ecommerce_api's clean 15-table production schema (from `~/code/fraiseql/examples/ecommerce_api/db/migrations/001_initial_schema.sql`) wrapped as a paired-SQL Confiture migration with a real `down`. Validated against confiture 0.22 (up→`001`, verify/preflight exit 0, down rolls back, idempotent re-up). |
| `server.config.toml` | Minimal flat `ServerConfig` for the server (`cors_enabled = false`, production-safe). |
| `schema.compiled.json` | A minimal valid compiled boot schema (from `fraiseql-cli compile examples/basic/schema.json`). The server doesn't re-validate it against the DB at boot; the served GraphQL is minimal (a fraiseql concern, not fraisier's). |
| `deploy-local.sh` | The local proof (`systemctl --user`, zero spend). |
| `criterion1-host.sh` | Runs on a pid-1 host (system systemd): throwaway Postgres container, unit, artifact server, 3 deploys. |
| `hetzner-criterion1.sh` | Orchestrator: provisions a throwaway Hetzner debian-13 host (glibc ≥ 2.39 for the prebuilt binaries), installs docker + confiture ≥ 0.22, ships the binaries + assets, runs `criterion1-host.sh`, deletes the host on exit. |

## Regenerating the two non-committed inputs

The binaries and the compiled schema are environment-specific / large and are **not**
committed; regenerate them into `~/code/partb-materialize-scratch/` before running:

```sh
# server binary (needs the fraiseql workspace at ~/code/fraiseql, tag v2.4.0)
cargo build -p fraiseql-server --bin fraiseql-server   # debug is fine
cp ~/code/fraiseql/target/debug/fraiseql-server ~/code/partb-materialize-scratch/fraiseql-server.bin
# fraisier engine
cargo build --features otel --bin fraisier             # in this repo
cp target/debug/fraisier ~/code/partb-materialize-scratch/fraisier.bin
strip ~/code/partb-materialize-scratch/*.bin           # shrink transfer
# compiled boot schema (already committed here; regenerate if needed)
~/code/fraiseql/target/debug/fraiseql-cli compile \
  ~/code/fraiseql/examples/basic/schema.json -o ~/code/partb-materialize-scratch/schema.compiled.json
```

## Key mechanics (so a re-run doesn't re-discover them)

- **Confiture format:** confiture 0.22 ignores a bare `NNN_name.sql`; it needs paired
  `.up.sql/.down.sql` (or Python). DSN via `CONFITURE_DATABASE_URL` + `--no-config`.
- **Artifact:** the `release` adapter stages the downloaded file + symlink-swaps
  `active_path`; it does **not** extract. So the real binary is fixed infra and the
  artifact is opaque per-version (activation exercised mechanically) — same as the
  matrix/training field. Criterion-1 realness = real v2.4.0 binary + real ecom schema.
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
